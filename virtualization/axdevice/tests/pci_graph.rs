use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use axdevice::*;
use axdevice_base::*;

const AUTO_MMIO_START: u64 = 0x1000_0000;
const AUTO_MMIO_END: u64 = 0x3000_0000;
const PCI_MEMORY_SIZE: u64 = 0x10_0000;

type RecordedAccess = (PciBdf, PciBarIndex, u64);

#[derive(Default)]
struct RecordingFunction {
    accesses: Mutex<Vec<RecordedAccess>>,
}

impl RecordingFunction {
    fn accesses(&self) -> Vec<RecordedAccess> {
        self.accesses.lock().unwrap().clone()
    }
}

impl PciFunction for RecordingFunction {
    fn name(&self) -> &str {
        "graph-recording-function"
    }

    fn read_bar(
        &self,
        access: &PciBarAccess,
        _context: &mut dyn DeviceContext,
    ) -> DeviceResult<u64> {
        self.accesses
            .lock()
            .unwrap()
            .push((access.bdf(), access.bar(), access.offset()));
        Ok(0xcafe_0000 | access.offset())
    }

    fn write_bar(
        &self,
        access: &PciBarAccess,
        _value: u64,
        _context: &mut dyn DeviceContext,
    ) -> DeviceResult {
        self.accesses
            .lock()
            .unwrap()
            .push((access.bdf(), access.bar(), access.offset()));
        Ok(())
    }
}

struct TestEndpointModel {
    function: Arc<RecordingFunction>,
    fail_registration_once: AtomicBool,
}

struct FinishRetryEndpointModel {
    function: Arc<RecordingFunction>,
    leave_claim_unconsumed_once: AtomicBool,
}

impl TestEndpointModel {
    fn new(function: Arc<RecordingFunction>, fail_registration_once: bool) -> Self {
        Self {
            function,
            fail_registration_once: AtomicBool::new(fail_registration_once),
        }
    }
}

impl PciEndpointModel for TestEndpointModel {
    fn requirements(&self) -> DeviceManagerResult<DeviceRequirements> {
        Ok(DeviceRequirements::new())
    }

    fn build(
        &self,
        _context: &mut DeviceBuildContext<'_>,
    ) -> DeviceManagerResult<PciEndpointBundle> {
        let function: Arc<dyn PciFunction> = self.function.clone();
        if self.fail_registration_once.swap(false, Ordering::AcqRel) {
            let mut bundle = DeviceBundle::new();
            bundle.add_device(Arc::new(ConflictingDevice("first")));
            bundle.add_device(Arc::new(ConflictingDevice("second")));
            Ok(PciEndpointBundle::with_bundle(function, bundle))
        } else {
            Ok(PciEndpointBundle::new(function))
        }
    }
}

impl PciEndpointModel for FinishRetryEndpointModel {
    fn requirements(&self) -> DeviceManagerResult<DeviceRequirements> {
        DeviceRequirements::new().with_mmio(
            ResourceSlot::new("endpoint-private")?,
            0x1000,
            0x1000,
            ResourceRequest::Auto,
        )
    }

    fn build(
        &self,
        context: &mut DeviceBuildContext<'_>,
    ) -> DeviceManagerResult<PciEndpointBundle> {
        if !self
            .leave_claim_unconsumed_once
            .swap(false, Ordering::AcqRel)
        {
            let _private_range = context.mmio("endpoint-private")?;
        }
        let function: Arc<dyn PciFunction> = self.function.clone();
        Ok(PciEndpointBundle::new(function))
    }
}

struct ConflictingDevice(&'static str);

struct OwnedRangeDevice {
    resources: Vec<Resource>,
}

impl Device for ConflictingDevice {
    fn name(&self) -> &str {
        self.0
    }

    fn resources(&self) -> &[Resource] {
        static RESOURCES: [Resource; 1] = [Resource::MmioRange {
            base: 0x4000_0000,
            size: 0x1000,
        }];
        &RESOURCES
    }

    fn read(&self, _access: &DeviceAccess, _context: &mut dyn DeviceContext) -> DeviceResult<u64> {
        Ok(0)
    }

    fn write(
        &self,
        _access: &DeviceAccess,
        _value: u64,
        _context: &mut dyn DeviceContext,
    ) -> DeviceResult {
        Ok(())
    }
}

impl Device for OwnedRangeDevice {
    fn name(&self) -> &str {
        "owned-range-conflict"
    }

    fn resources(&self) -> &[Resource] {
        &self.resources
    }

    fn read(&self, _access: &DeviceAccess, _context: &mut dyn DeviceContext) -> DeviceResult<u64> {
        Ok(0)
    }

    fn write(
        &self,
        _access: &DeviceAccess,
        _value: u64,
        _context: &mut dyn DeviceContext,
    ) -> DeviceResult {
        Ok(())
    }
}

#[test]
fn graph_resources_freeze_one_topology_before_host_and_endpoint_build() {
    let host_id = DeviceNodeId::new("pci-host").unwrap();
    let endpoint_id = DeviceNodeId::new("ivshmem0").unwrap();
    let function = Arc::new(RecordingFunction::default());
    let endpoint_model: Arc<dyn PciEndpointModel> =
        Arc::new(TestEndpointModel::new(function.clone(), false));
    let (mut pci, mut graph) = graph_with_host(host_id.clone());
    graph.add(pci.host_node()).unwrap();
    graph
        .add(
            pci.endpoint_node(endpoint_spec(endpoint_id.clone()), endpoint_model)
                .unwrap(),
        )
        .unwrap();
    let graph = resolve_graph(graph);

    let endpoint_node = graph
        .nodes()
        .find(|node| node.id() == &endpoint_id)
        .unwrap();
    assert_eq!(endpoint_node.dependencies(), std::slice::from_ref(&host_id));

    let mut runtime_builder = DeviceRuntimeBuilder::new(RuntimeAccessPorts::new());
    let host_node = graph.nodes().find(|node| node.id() == &host_id).unwrap();
    assert!(
        runtime_builder
            .build_graph_node(host_node, graph.resource_plan())
            .is_err()
    );

    let pci = pci.resolve(&graph).unwrap();
    let ecam_window = pci.ecam_window();
    let host_resources = graph.resources_for(&host_id).unwrap();
    assert_eq!(
        host_resources
            .mmio(&ResourceSlot::new("ecam").unwrap())
            .unwrap(),
        (ecam_window.start, ecam_window.end - ecam_window.start)
    );
    let aperture = pci.memory_aperture();
    assert_eq!(
        host_resources
            .mmio(&ResourceSlot::new("memory").unwrap())
            .unwrap(),
        (aperture.start, aperture.end - aperture.start)
    );

    for node in graph.nodes() {
        runtime_builder
            .build_graph_node(node, graph.resource_plan())
            .unwrap();
    }
    let runtime = runtime_builder.finish(graph.resource_plan()).unwrap();
    let resolved_function = pci.topology().function(&endpoint_id).unwrap();
    let bdf = resolved_function.bdf();
    let bar = resolved_function.bar(PciBarIndex::new(0).unwrap()).unwrap();

    assert_eq!(
        read_config(&runtime, ecam_window.start, bdf, 0),
        0x4106_110a
    );
    write_config(&runtime, ecam_window.start, bdf, 4, 2);
    assert_eq!(read_mmio(&runtime, bar.address() + 0x20), 0xcafe_0020);
    assert_eq!(
        function.accesses(),
        vec![(bdf, PciBarIndex::new(0).unwrap(), 0x20)]
    );
}

#[test]
fn host_bundle_registration_failure_releases_root_and_resource_claims_for_retry() {
    let host_id = DeviceNodeId::new("pci-host").unwrap();
    let endpoint_id = DeviceNodeId::new("endpoint").unwrap();
    let function = Arc::new(RecordingFunction::default());
    let endpoint_model: Arc<dyn PciEndpointModel> =
        Arc::new(TestEndpointModel::new(function, false));
    let (mut pci, mut graph) = graph_with_host(host_id.clone());
    graph.add(pci.host_node()).unwrap();
    graph
        .add(
            pci.endpoint_node(endpoint_spec(endpoint_id), endpoint_model)
                .unwrap(),
        )
        .unwrap();
    let graph = resolve_graph(graph);
    let pci = pci.resolve(&graph).unwrap();
    let ecam = pci.ecam_window().start;
    let host_node = graph.nodes().find(|node| node.id() == &host_id).unwrap();

    let mut conflicting_builder = DeviceRuntimeBuilder::new(RuntimeAccessPorts::new());
    conflicting_builder
        .register_bundle(DeviceBundle::from_registration(DeviceRegistration::Device(
            Arc::new(OwnedRangeDevice {
                resources: vec![Resource::MmioRange {
                    base: ecam,
                    size: 0x1000,
                }],
            }),
        )))
        .unwrap();
    assert!(
        conflicting_builder
            .build_graph_node(host_node, graph.resource_plan())
            .is_err()
    );
    drop(conflicting_builder);

    let mut retry = DeviceRuntimeBuilder::new(RuntimeAccessPorts::new());
    for node in graph.nodes() {
        retry.build_graph_node(node, graph.resource_plan()).unwrap();
    }
    retry.finish(graph.resource_plan()).unwrap();
}

#[test]
fn pci_resolution_rejects_a_declared_endpoint_node_omitted_from_the_graph() {
    let host_id = DeviceNodeId::new("pci-host").unwrap();
    let endpoint_id = DeviceNodeId::new("omitted-endpoint").unwrap();
    let function = Arc::new(RecordingFunction::default());
    let endpoint_model: Arc<dyn PciEndpointModel> =
        Arc::new(TestEndpointModel::new(function, false));
    let (mut pci, mut graph) = graph_with_host(host_id);
    graph.add(pci.host_node()).unwrap();
    let _omitted = pci
        .endpoint_node(endpoint_spec(endpoint_id), endpoint_model)
        .unwrap();
    let graph = resolve_graph(graph);

    assert!(pci.resolve(&graph).is_err());
}

#[test]
fn unfinished_endpoint_claim_releases_function_binding_for_retry() {
    let host_id = DeviceNodeId::new("pci-host").unwrap();
    let endpoint_id = DeviceNodeId::new("finish-retry-endpoint").unwrap();
    let function = Arc::new(RecordingFunction::default());
    let endpoint_model: Arc<dyn PciEndpointModel> = Arc::new(FinishRetryEndpointModel {
        function,
        leave_claim_unconsumed_once: AtomicBool::new(true),
    });
    let (mut pci, mut graph) = graph_with_host(host_id.clone());
    graph.add(pci.host_node()).unwrap();
    graph
        .add(
            pci.endpoint_node(endpoint_spec(endpoint_id.clone()), endpoint_model)
                .unwrap(),
        )
        .unwrap();
    let graph = resolve_graph(graph);
    let pci = pci.resolve(&graph).unwrap();
    let mut runtime_builder = DeviceRuntimeBuilder::new(RuntimeAccessPorts::new());
    let mut nodes = graph.nodes();
    let host_node = nodes.next().unwrap();
    let endpoint_node = nodes.next().unwrap();
    assert_eq!(host_node.id(), &host_id);
    assert_eq!(endpoint_node.id(), &endpoint_id);

    runtime_builder
        .build_graph_node(host_node, graph.resource_plan())
        .unwrap();
    assert!(
        runtime_builder
            .build_graph_node(endpoint_node, graph.resource_plan())
            .is_err()
    );
    runtime_builder
        .build_graph_node(endpoint_node, graph.resource_plan())
        .unwrap();
    let runtime = runtime_builder.finish(graph.resource_plan()).unwrap();

    let resolved_function = pci.topology().function(&endpoint_id).unwrap();
    let ecam_base = pci.ecam_window().start;
    let bdf = resolved_function.bdf();
    let bar = resolved_function.bar(PciBarIndex::new(0).unwrap()).unwrap();
    write_config(&runtime, ecam_base, bdf, 4, 2);
    assert_eq!(read_mmio(&runtime, bar.address()), 0xcafe_0000);
}

#[test]
fn endpoint_bundle_registration_failure_releases_function_binding_for_retry() {
    let host_id = DeviceNodeId::new("pci-host").unwrap();
    let endpoint_id = DeviceNodeId::new("retry-endpoint").unwrap();
    let function = Arc::new(RecordingFunction::default());
    let endpoint_model: Arc<dyn PciEndpointModel> =
        Arc::new(TestEndpointModel::new(function, true));
    let (mut pci, mut graph) = graph_with_host(host_id.clone());
    graph.add(pci.host_node()).unwrap();
    graph
        .add(
            pci.endpoint_node(endpoint_spec(endpoint_id.clone()), endpoint_model)
                .unwrap(),
        )
        .unwrap();
    let graph = resolve_graph(graph);
    let pci = pci.resolve(&graph).unwrap();
    let mut runtime_builder = DeviceRuntimeBuilder::new(RuntimeAccessPorts::new());
    let mut nodes = graph.nodes();
    let host_node = nodes.next().unwrap();
    let endpoint_node = nodes.next().unwrap();
    assert_eq!(host_node.id(), &host_id);
    assert_eq!(endpoint_node.id(), &endpoint_id);

    runtime_builder
        .build_graph_node(host_node, graph.resource_plan())
        .unwrap();
    assert!(
        runtime_builder
            .build_graph_node(endpoint_node, graph.resource_plan())
            .is_err()
    );
    runtime_builder
        .build_graph_node(endpoint_node, graph.resource_plan())
        .unwrap();
    let runtime = runtime_builder.finish(graph.resource_plan()).unwrap();

    let resolved_function = pci.topology().function(&endpoint_id).unwrap();
    let ecam_base = pci.ecam_window().start;
    let bdf = resolved_function.bdf();
    let bar = resolved_function.bar(PciBarIndex::new(0).unwrap()).unwrap();
    write_config(&runtime, ecam_base, bdf, 4, 2);
    assert_eq!(read_mmio(&runtime, bar.address()), 0xcafe_0000);
}

fn graph_with_host(host_id: DeviceNodeId) -> (PciBusGraphBuilder, DeviceGraphBuilder) {
    let resources = PciHostResourceRequirements::new(PCI_MEMORY_SIZE, PCI_MEMORY_SIZE).unwrap();
    (
        PciBusGraphBuilder::new(host_id, resources),
        DeviceGraphBuilder::new(),
    )
}

fn endpoint_spec(id: DeviceNodeId) -> PciFunctionSpec {
    let identity = PciEndpointIdentity::new(0x110a, 0x4106, PciClass::new(0xff, 0, 1));
    let bar = PciMemoryBar::new(
        PciBarIndex::new(0).unwrap(),
        0x1000,
        PciMemoryBarWidth::Bits32,
    )
    .unwrap();
    PciFunctionSpec::new(id, identity).with_bar(bar).unwrap()
}

fn resolve_graph(graph: DeviceGraphBuilder) -> ResolvedDeviceGraph {
    let mut pools = ResourcePools::new();
    pools.add_auto_mmio(AUTO_MMIO_START..AUTO_MMIO_END).unwrap();
    graph.declare().unwrap().resolve(pools).unwrap()
}

fn mmio_access(address: u64) -> DeviceAccess {
    DeviceAccess::new(
        DeviceVcpuId::new(0),
        BusKind::Mmio,
        address,
        AccessWidth::Dword,
    )
}

fn read_config(runtime: &DeviceRuntime, ecam_base: u64, bdf: PciBdf, offset: u16) -> u64 {
    runtime
        .try_read(&mmio_access(
            ecam_base + bdf.ecam_offset() + u64::from(offset),
        ))
        .unwrap()
        .unwrap()
}

fn write_config(runtime: &DeviceRuntime, ecam_base: u64, bdf: PciBdf, offset: u16, value: u64) {
    assert!(
        runtime
            .try_write(
                &mmio_access(ecam_base + bdf.ecam_offset() + u64::from(offset)),
                value,
                None,
            )
            .unwrap()
    );
}

fn read_mmio(runtime: &DeviceRuntime, address: u64) -> u64 {
    runtime.try_read(&mmio_access(address)).unwrap().unwrap()
}
