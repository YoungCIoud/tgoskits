use std::sync::{Arc, Mutex};

use axdevice::*;
use axdevice_base::*;

const APERTURE_BASE: u64 = 0xc000_0000;
const APERTURE_SIZE: u64 = 0x10_0000;

type RootSlot = Arc<Mutex<Option<Arc<PciRootState>>>>;
type BindingSlot = Arc<Mutex<Option<Arc<PciRootBinding>>>>;

fn id(value: &str) -> DeviceNodeId {
    DeviceNodeId::new(value).unwrap()
}
fn slot(value: &str) -> ResourceSlot {
    ResourceSlot::new(value).unwrap()
}
fn host_key() -> PciHostKey {
    PciHostKey::new("pci").unwrap()
}

struct HostModel {
    root: RootSlot,
    binding: BindingSlot,
}

impl DeviceModel for HostModel {
    fn requirements(&self) -> DeviceManagerResult<DeviceRequirements> {
        DeviceRequirements::new().with_mmio(
            slot("pci-memory"),
            APERTURE_SIZE,
            APERTURE_SIZE,
            ResourceRequest::Auto,
        )
    }

    fn firmware(&self) -> DeviceFirmwareSpec {
        DeviceFirmwareSpec::None
    }

    fn build(&self, context: &mut DeviceBuildContext<'_>) -> DeviceManagerResult<DeviceBundle> {
        let _aperture = context.mmio("pci-memory")?;
        let topology = context.pci_host_topology().unwrap().clone();
        let root = Arc::new(PciRootState::new(topology));
        let binding = Arc::new(PciRootBinding::new(id("pci-host"), root.clone()));
        *self.root.lock().unwrap() = Some(root);
        *self.binding.lock().unwrap() = Some(binding.clone());
        DeviceBundle::new().with_service::<PciRootBindingKey>(binding)
    }
}

#[derive(Default)]
struct RecordingEndpoint {
    reads: Mutex<Vec<(DeviceId, PciBarAccess)>>,
}

impl Device for RecordingEndpoint {
    fn name(&self) -> &str {
        "recording-pci-endpoint"
    }
    fn resources(&self) -> &[Resource] {
        &[]
    }
    fn read(&self, _access: &DeviceAccess, _context: &mut dyn DeviceContext) -> DeviceResult<u64> {
        Err(DeviceError::NotFound)
    }
    fn write(
        &self,
        _access: &DeviceAccess,
        _value: u64,
        _context: &mut dyn DeviceContext,
    ) -> DeviceResult {
        Err(DeviceError::NotFound)
    }
}

impl PciFunction for RecordingEndpoint {
    fn read_bar(&self, access: PciBarAccess, context: &mut dyn DeviceContext) -> DeviceResult<u64> {
        self.reads
            .lock()
            .unwrap()
            .push((context.device_id(), access));
        Ok(0xfeed_0000 | access.offset())
    }

    fn write_bar(
        &self,
        _access: PciBarAccess,
        _value: u64,
        _context: &mut dyn DeviceContext,
    ) -> DeviceResult {
        Ok(())
    }
}

struct EndpointModel {
    endpoint: Arc<RecordingEndpoint>,
    fail_registration: bool,
}

impl DeviceModel for EndpointModel {
    fn requirements(&self) -> DeviceManagerResult<DeviceRequirements> {
        let requirement = PciFunctionRequirement::new(
            host_key(),
            PciEndpointIdentity::new(0x1af4, 0x1110, PciClass::new(0xff, 0, 0)),
        )
        .with_bar(PciMemoryBar::new(PciBarIndex::new(2).unwrap(), 0x1_0000)?)?;
        DeviceRequirements::new().with_pci_function(requirement)
    }

    fn firmware(&self) -> DeviceFirmwareSpec {
        DeviceFirmwareSpec::None
    }

    fn build(&self, _context: &mut DeviceBuildContext<'_>) -> DeviceManagerResult<DeviceBundle> {
        let mut bundle = DeviceBundle::new();
        bundle.add_pci_function(self.endpoint.clone())?;
        if self.fail_registration {
            bundle.add_device(Arc::new(ConflictingDevice("first")));
            bundle.add_device(Arc::new(ConflictingDevice("second")));
        }
        Ok(bundle)
    }
}

struct ConflictingDevice(&'static str);

impl Device for ConflictingDevice {
    fn name(&self) -> &str {
        self.0
    }
    fn resources(&self) -> &[Resource] {
        static RESOURCES: [Resource; 1] = [Resource::MmioRange {
            base: 0x5000_0000,
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

fn resolved_graph(
    endpoint: Arc<RecordingEndpoint>,
    fail_registration: bool,
) -> (ResolvedDeviceGraph, RootSlot, BindingSlot) {
    let root = Arc::new(Mutex::new(None));
    let binding = Arc::new(Mutex::new(None));
    let host_model = Arc::new(HostModel {
        root: root.clone(),
        binding: binding.clone(),
    });
    let endpoint_model = Arc::new(EndpointModel {
        endpoint,
        fail_registration,
    });
    let provider = PciHostProvider::new(
        host_key(),
        DeviceNodeSpec::virtual_device(id("pci-host"), host_model),
        slot("pci-memory"),
    );
    let mut graph = DeviceGraphBuilder::new();
    graph.register_pci_host(provider).unwrap();
    graph
        .add(DeviceNodeSpec::virtual_device(
            id("endpoint"),
            endpoint_model,
        ))
        .unwrap();
    let mut pools = ResourcePools::new();
    pools
        .add_auto_mmio(APERTURE_BASE..APERTURE_BASE + APERTURE_SIZE)
        .unwrap();
    (
        graph.declare().unwrap().resolve(pools).unwrap(),
        root,
        binding,
    )
}

#[test]
fn graph_bundles_bind_and_dispatch_with_the_endpoint_device_identity() {
    let endpoint = Arc::new(RecordingEndpoint::default());
    let (graph, root_slot, binding_slot) = resolved_graph(endpoint.clone(), false);
    let mut builder = DeviceRuntimeBuilder::new(RuntimeAccessPorts::new());
    for node in graph.nodes() {
        builder
            .build_graph_node(node, graph.resource_plan())
            .unwrap();
    }
    let runtime = builder.finish(graph.resource_plan()).unwrap();
    let root = root_slot.lock().unwrap().clone().unwrap();
    let binding = binding_slot.lock().unwrap().clone().unwrap();
    let function = graph
        .pci_topology(&host_key())
        .unwrap()
        .function(&id("endpoint"))
        .unwrap();
    let bar = function.bar(PciBarIndex::new(2).unwrap()).unwrap();
    root.write_config(
        function.bdf(),
        ConfigOffset::new(4).unwrap(),
        AccessWidth::Word,
        2,
    )
    .unwrap();

    assert_eq!(
        binding
            .read_bar(bar.address() + 0x20, AccessWidth::Dword)
            .unwrap(),
        0xfeed_0020
    );
    let reads = endpoint.reads.lock().unwrap();
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0].0, DeviceId::new(0));
    assert_eq!(reads[0].1.offset(), 0x20);
    drop(reads);

    drop(runtime);
    assert_eq!(
        binding.read_bar(bar.address(), AccessWidth::Dword),
        Err(DeviceError::NotFound)
    );
}

#[test]
fn failed_bundle_registration_invalidates_the_provisional_route() {
    let endpoint = Arc::new(RecordingEndpoint::default());
    let (graph, root_slot, binding_slot) = resolved_graph(endpoint, true);
    let mut builder = DeviceRuntimeBuilder::new(RuntimeAccessPorts::new());
    let mut nodes = graph.nodes();
    builder
        .build_graph_node(nodes.next().unwrap(), graph.resource_plan())
        .unwrap();
    assert!(
        builder
            .build_graph_node(nodes.next().unwrap(), graph.resource_plan())
            .is_err()
    );
    let root = root_slot.lock().unwrap().clone().unwrap();
    let binding = binding_slot.lock().unwrap().clone().unwrap();
    let function = graph
        .pci_topology(&host_key())
        .unwrap()
        .function(&id("endpoint"))
        .unwrap();
    let bar = function.bar(PciBarIndex::new(2).unwrap()).unwrap();
    root.write_config(
        function.bdf(),
        ConfigOffset::new(4).unwrap(),
        AccessWidth::Word,
        2,
    )
    .unwrap();
    assert_eq!(
        binding.read_bar(bar.address(), AccessWidth::Dword),
        Err(DeviceError::NotFound)
    );
}
