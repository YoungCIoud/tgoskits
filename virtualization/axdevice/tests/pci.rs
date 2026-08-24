use std::{
    ops::Range,
    sync::{Arc, Mutex},
};

use axdevice::*;
use axdevice_base::*;

const ECAM_BASE: u64 = 0x1000_0000;
const APERTURE_BASE: u64 = 0x2000_0000;
const APERTURE_END: u64 = 0x2010_0000;

fn ecam_window(base: u64) -> Range<u64> {
    base..base + 0x10_0000
}

type RecordedAccess = (PciBdf, PciBarIndex, u64, bool, u64);

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
        "recording-pci-function"
    }

    fn read_bar(
        &self,
        access: &PciBarAccess,
        _context: &mut dyn DeviceContext,
    ) -> DeviceResult<u64> {
        self.accesses
            .lock()
            .unwrap()
            .push((access.bdf(), access.bar(), access.offset(), true, 0));
        Ok(0xa500_0000 | access.offset())
    }

    fn write_bar(
        &self,
        access: &PciBarAccess,
        value: u64,
        _context: &mut dyn DeviceContext,
    ) -> DeviceResult {
        self.accesses.lock().unwrap().push((
            access.bdf(),
            access.bar(),
            access.offset(),
            false,
            value,
        ));
        Ok(())
    }
}

struct StaticEndpointModel {
    function: Arc<RecordingFunction>,
}

impl PciEndpointModel for StaticEndpointModel {
    fn requirements(&self) -> DeviceManagerResult<DeviceRequirements> {
        Ok(DeviceRequirements::new())
    }

    fn build(
        &self,
        _context: &mut DeviceBuildContext<'_>,
    ) -> DeviceManagerResult<PciEndpointBundle> {
        let function: Arc<dyn PciFunction> = self.function.clone();
        Ok(PciEndpointBundle::new(function))
    }
}

#[test]
fn typed_pci_addresses_reject_out_of_range_components() {
    let segment = PciSegment::new(0);

    assert!(PciBdf::new(segment, 0, 32, 0).is_err());
    assert!(PciBdf::new(segment, 0, 0, 8).is_err());
    assert!(PciBarIndex::new(6).is_err());
    assert!(ConfigOffset::new(0x1000).is_err());

    let bdf = PciBdf::new(segment, 0, 31, 7).unwrap();
    assert_eq!(bdf.ecam_offset(), 0x000f_f000);
}

#[test]
fn ecam_exposes_type_zero_identity_capabilities_and_absent_functions() {
    let function = Arc::new(RecordingFunction::default());
    let identity = PciEndpointIdentity::new(0x110a, 0x4106, PciClass::new(0xff, 0x00, 0x01))
        .with_revision(2)
        .with_subsystem(0x110a, 0x0001);
    let capability = PciCapability::new(0x09, [0xaa, 0xbb], [0x0f, 0x00]).unwrap();
    let second_capability = PciCapability::read_only(0x11, [0x34, 0x12]).unwrap();
    let bdf = PciBdf::new(PciSegment::new(0), 0, 5, 0).unwrap();
    let spec = PciFunctionSpec::new(function_id("endpoint"), identity)
        .with_bdf(ResourceRequest::Fixed(bdf))
        .with_capability(capability)
        .unwrap()
        .with_capability(second_capability)
        .unwrap();
    let (pci, runtime) = build_pci([spec], [("endpoint", function)]);

    assert_eq!(
        read_config(&runtime, bdf, 0, AccessWidth::Dword),
        0x4106_110a
    );
    assert_eq!(
        read_config(&runtime, bdf, 8, AccessWidth::Dword),
        0xff00_0102
    );
    assert_eq!(
        read_config(&runtime, bdf, 0x2c, AccessWidth::Dword),
        0x0001_110a
    );
    assert_eq!(read_config(&runtime, bdf, 0x34, AccessWidth::Byte), 0x40);
    assert_eq!(
        read_config(&runtime, bdf, 0x40, AccessWidth::Dword),
        0xbbaa_4409
    );
    assert_eq!(
        read_config(&runtime, bdf, 0x44, AccessWidth::Dword),
        0x1234_0011
    );
    assert_ne!(
        read_config(&runtime, bdf, 4, AccessWidth::Dword) & 0x0010_0000,
        0
    );

    write_config(&runtime, bdf, 0x42, AccessWidth::Byte, 0xff);
    assert_eq!(read_config(&runtime, bdf, 0x42, AccessWidth::Byte), 0xaf);
    write_config(&runtime, bdf, 0x43, AccessWidth::Byte, 0x00);
    assert_eq!(read_config(&runtime, bdf, 0x43, AccessWidth::Byte), 0xbb);

    write_config(&runtime, bdf, 4, AccessWidth::Word, u64::MAX);
    assert_eq!(read_config(&runtime, bdf, 4, AccessWidth::Word), 0x0002);
    assert_eq!(read_config(&runtime, bdf, 0x100, AccessWidth::Dword), 0);

    let absent = PciBdf::new(PciSegment::new(0), 0, 31, 7).unwrap();
    assert_eq!(read_config(&runtime, absent, 0, AccessWidth::Byte), 0xff);
    assert_eq!(read_config(&runtime, absent, 0, AccessWidth::Word), 0xffff);
    assert_eq!(
        read_config(&runtime, absent, 0, AccessWidth::Dword),
        0xffff_ffff
    );
    write_config(&runtime, absent, 0, AccessWidth::Dword, 0);

    assert_eq!(
        pci.topology()
            .function(&function_id("endpoint"))
            .unwrap()
            .bdf(),
        bdf
    );

    runtime.reset_lifecycle_devices().unwrap();
    assert_eq!(read_config(&runtime, bdf, 0x42, AccessWidth::Byte), 0xaa);
    assert_eq!(read_config(&runtime, bdf, 4, AccessWidth::Word), 0);
}

#[test]
fn memory_bar_probe_relocation_and_runtime_routing_share_one_state() {
    let function = Arc::new(RecordingFunction::default());
    let bdf = PciBdf::new(PciSegment::new(0), 0, 1, 0).unwrap();
    let bar = PciMemoryBar::new(
        PciBarIndex::new(0).unwrap(),
        0x1000,
        PciMemoryBarWidth::Bits32,
    )
    .unwrap()
    .with_address(ResourceRequest::Fixed(APERTURE_BASE));
    let spec = PciFunctionSpec::new(function_id("endpoint"), endpoint_identity())
        .with_bdf(ResourceRequest::Fixed(bdf))
        .with_bar(bar)
        .unwrap();
    let (_, runtime) = build_pci([spec], [("endpoint", function.clone())]);

    assert!(read_mmio(&runtime, APERTURE_BASE, AccessWidth::Dword).is_err());
    assert_eq!(
        read_config(&runtime, bdf, 0x10, AccessWidth::Dword),
        APERTURE_BASE
    );

    write_config(&runtime, bdf, 0x10, AccessWidth::Dword, 0xffff_ffff);
    assert_eq!(
        read_config(&runtime, bdf, 0x10, AccessWidth::Dword),
        0xffff_f000
    );
    write_config(&runtime, bdf, 0x10, AccessWidth::Dword, APERTURE_BASE);
    write_config(
        &runtime,
        bdf,
        0x10,
        AccessWidth::Dword,
        APERTURE_BASE + 0x2123,
    );
    assert_eq!(
        read_config(&runtime, bdf, 0x10, AccessWidth::Dword),
        APERTURE_BASE + 0x2000
    );
    write_config(&runtime, bdf, 0x10, AccessWidth::Dword, APERTURE_BASE);

    write_config(&runtime, bdf, 4, AccessWidth::Word, 2);
    assert_eq!(
        read_mmio(&runtime, APERTURE_BASE + 0x24, AccessWidth::Dword).unwrap(),
        0xa500_0024
    );

    let relocated = APERTURE_BASE + 0x4000;
    write_config(&runtime, bdf, 0x10, AccessWidth::Dword, relocated);
    assert_eq!(
        read_config(&runtime, bdf, 0x10, AccessWidth::Dword),
        relocated
    );
    assert!(read_mmio(&runtime, APERTURE_BASE + 0x24, AccessWidth::Dword).is_err());
    assert_eq!(
        read_mmio(&runtime, relocated + 0x38, AccessWidth::Dword).unwrap(),
        0xa500_0038
    );

    write_mmio(&runtime, relocated + 0x40, AccessWidth::Word, 0x55aa).unwrap();
    assert_eq!(
        function.accesses(),
        vec![
            (bdf, PciBarIndex::new(0).unwrap(), 0x24, true, 0),
            (bdf, PciBarIndex::new(0).unwrap(), 0x38, true, 0),
            (bdf, PciBarIndex::new(0).unwrap(), 0x40, false, 0x55aa),
        ]
    );

    runtime.reset_lifecycle_devices().unwrap();
    assert_eq!(
        read_config(&runtime, bdf, 0x10, AccessWidth::Dword),
        APERTURE_BASE
    );
    assert!(read_mmio(&runtime, APERTURE_BASE, AccessWidth::Dword).is_err());
}

#[test]
fn invalid_bar_relocations_preserve_the_previous_config_and_route() {
    let left = Arc::new(RecordingFunction::default());
    let right = Arc::new(RecordingFunction::default());
    let left_bdf = PciBdf::new(PciSegment::new(0), 0, 2, 0).unwrap();
    let right_bdf = PciBdf::new(PciSegment::new(0), 0, 3, 0).unwrap();
    let left_address = APERTURE_BASE + 0x1000;
    let right_address = APERTURE_BASE + 0x3000;
    let specs = [
        fixed_bar_function("left", left_bdf, left_address),
        fixed_bar_function("right", right_bdf, right_address),
    ];
    let (_, runtime) = build_pci(specs, [("left", left), ("right", right)]);
    write_config(&runtime, left_bdf, 4, AccessWidth::Word, 2);
    write_config(&runtime, right_bdf, 4, AccessWidth::Word, 2);

    write_config(&runtime, left_bdf, 0x10, AccessWidth::Dword, right_address);
    assert_eq!(
        read_config(&runtime, left_bdf, 0x10, AccessWidth::Dword),
        left_address
    );

    write_config(&runtime, left_bdf, 0x10, AccessWidth::Dword, APERTURE_END);
    assert_eq!(
        read_config(&runtime, left_bdf, 0x10, AccessWidth::Dword),
        left_address
    );

    assert_eq!(
        read_mmio(&runtime, left_address, AccessWidth::Dword).unwrap(),
        0xa500_0000
    );
    assert_eq!(
        read_mmio(&runtime, right_address, AccessWidth::Dword).unwrap(),
        0xa500_0000
    );
}

#[test]
fn sixty_four_bit_bar_probe_and_relocation_update_the_pair_together() {
    let function0 = Arc::new(RecordingFunction::default());
    let function1 = Arc::new(RecordingFunction::default());
    let bdf0 = PciBdf::new(PciSegment::new(0), 0, 4, 0).unwrap();
    let bdf1 = PciBdf::new(PciSegment::new(0), 0, 4, 1).unwrap();
    let initial = APERTURE_BASE + 0x8000;
    let bar = PciMemoryBar::new(
        PciBarIndex::new(2).unwrap(),
        0x2000,
        PciMemoryBarWidth::Bits64,
    )
    .unwrap()
    .with_address(ResourceRequest::Fixed(initial));
    let specs = [
        PciFunctionSpec::new(function_id("function0"), endpoint_identity())
            .with_bdf(ResourceRequest::Fixed(bdf0)),
        PciFunctionSpec::new(function_id("function1"), endpoint_identity())
            .with_bdf(ResourceRequest::Fixed(bdf1))
            .with_bar(bar)
            .unwrap(),
    ];
    let (_, runtime) = build_pci(specs, [("function0", function0), ("function1", function1)]);

    assert_eq!(read_config(&runtime, bdf0, 0x0e, AccessWidth::Byte), 0x80);
    assert_eq!(
        read_config(&runtime, bdf1, 0x18, AccessWidth::Dword),
        initial | 0x4
    );
    assert_eq!(read_config(&runtime, bdf1, 0x1c, AccessWidth::Dword), 0);

    write_config(&runtime, bdf1, 0x18, AccessWidth::Dword, 0xffff_ffff);
    write_config(&runtime, bdf1, 0x1c, AccessWidth::Dword, 0xffff_ffff);
    assert_eq!(
        read_config(&runtime, bdf1, 0x18, AccessWidth::Dword),
        0xffff_e004
    );
    assert_eq!(
        read_config(&runtime, bdf1, 0x1c, AccessWidth::Dword),
        0xffff_ffff
    );
    write_config(&runtime, bdf1, 0x18, AccessWidth::Dword, initial | 0x4);
    write_config(&runtime, bdf1, 0x1c, AccessWidth::Dword, 0);

    let relocated = APERTURE_BASE + 0xc000;
    write_config(&runtime, bdf1, 0x18, AccessWidth::Dword, relocated | 0x4);
    write_config(&runtime, bdf1, 0x1c, AccessWidth::Dword, 1);
    assert_eq!(read_config(&runtime, bdf1, 0x1c, AccessWidth::Dword), 0);
    assert_eq!(
        read_config(&runtime, bdf1, 0x18, AccessWidth::Dword),
        relocated | 0x4
    );
    write_config(&runtime, bdf1, 0x1c, AccessWidth::Dword, 0);
    write_config(&runtime, bdf1, 4, AccessWidth::Word, 2);
    assert_eq!(
        read_mmio(&runtime, relocated + 0x100, AccessWidth::Dword).unwrap(),
        0xa500_0100
    );
}

#[test]
fn topology_rejects_duplicate_bdfs_bar_slots_and_orphan_functions() {
    let original_bdf = PciBdf::new(PciSegment::new(0), 0, 5, 0).unwrap();
    let replacement_bdf = PciBdf::new(PciSegment::new(0), 0, 5, 1).unwrap();
    let mut duplicate_id = PciTopologyBuilder::new();
    duplicate_id
        .add_function(bare_function("same").with_bdf(ResourceRequest::Fixed(original_bdf)))
        .unwrap();
    assert!(
        duplicate_id
            .add_function(bare_function("same").with_bdf(ResourceRequest::Fixed(replacement_bdf)))
            .is_err()
    );
    let duplicate_id = duplicate_id.resolve(APERTURE_BASE..APERTURE_END).unwrap();
    assert_eq!(
        duplicate_id.function(&function_id("same")).unwrap().bdf(),
        original_bdf
    );

    let bdf = PciBdf::new(PciSegment::new(0), 0, 6, 0).unwrap();
    let duplicate_bdf = [
        bare_function("alpha").with_bdf(ResourceRequest::Fixed(bdf)),
        bare_function("beta").with_bdf(ResourceRequest::Fixed(bdf)),
    ];
    assert!(resolve_topology(duplicate_bdf).is_err());

    let bar0 = PciMemoryBar::new(
        PciBarIndex::new(0).unwrap(),
        0x1000,
        PciMemoryBarWidth::Bits32,
    )
    .unwrap();
    let duplicate_bar = bare_function("duplicate-bar")
        .with_bar(bar0.clone())
        .unwrap()
        .with_bar(bar0);
    assert!(duplicate_bar.is_err());

    let orphan = bare_function("orphan").with_bdf(ResourceRequest::Fixed(
        PciBdf::new(PciSegment::new(0), 0, 7, 1).unwrap(),
    ));
    assert!(resolve_topology([orphan]).is_err());

    let other_segment = bare_function("other-segment").with_bdf(ResourceRequest::Fixed(
        PciBdf::new(PciSegment::new(1), 0, 0, 0).unwrap(),
    ));
    assert!(resolve_topology([other_segment]).is_err());
    let other_bus = bare_function("other-bus").with_bdf(ResourceRequest::Fixed(
        PciBdf::new(PciSegment::new(0), 1, 0, 0).unwrap(),
    ));
    assert!(resolve_topology([other_bus]).is_err());
}

#[test]
fn host_and_config_access_validation_rejects_invalid_ranges_and_widths() {
    assert!(
        validate_host_windows(ecam_window(ECAM_BASE + 0x1000), APERTURE_BASE..APERTURE_END)
            .is_err()
    );
    assert!(validate_host_windows(ecam_window(ECAM_BASE), APERTURE_BASE..APERTURE_BASE).is_err());
    assert!(validate_host_windows(ecam_window(ECAM_BASE), APERTURE_BASE..0x1_0000_1000).is_err());
    // Overlapping windows must also be rejected before any runtime device is
    // published.
    assert!(
        validate_host_windows(ecam_window(APERTURE_BASE), APERTURE_BASE..APERTURE_END).is_err()
    );

    let bdf = PciBdf::new(PciSegment::new(0), 0, 0, 0).unwrap();
    let (_, runtime) = build_pci(
        [bare_function("endpoint")],
        [("endpoint", Arc::new(RecordingFunction::default()))],
    );
    assert!(
        runtime
            .try_read(&device_access(config_address(bdf, 1), AccessWidth::Word))
            .is_err()
    );
    assert!(
        runtime
            .try_read(&device_access(config_address(bdf, 0), AccessWidth::Qword))
            .is_err()
    );
}

#[test]
fn topology_reports_bar_and_capability_aperture_exhaustion() {
    let invalid_identity = PciFunctionSpec::new(
        function_id("absent-identity"),
        PciEndpointIdentity::new(u16::MAX, 1, PciClass::new(0xff, 0, 0)),
    );
    assert!(resolve_topology([invalid_identity]).is_err());

    let tiny_host = APERTURE_BASE..APERTURE_BASE + 0x1000;
    let bar = PciMemoryBar::new(
        PciBarIndex::new(0).unwrap(),
        0x2000,
        PciMemoryBarWidth::Bits32,
    )
    .unwrap();
    let mut builder = PciTopologyBuilder::new();
    builder
        .add_function(bare_function("large").with_bar(bar).unwrap())
        .unwrap();
    assert!(builder.resolve(tiny_host).is_err());

    let body = vec![0u8; 0xbe];
    let capability = PciCapability::read_only(0x09, body).unwrap();
    let function = bare_function("capability-overflow")
        .with_capability(capability)
        .unwrap();
    let second = PciCapability::read_only(0x11, [0]).unwrap();
    assert!(function.with_capability(second).is_err());
}

#[test]
fn fixed_platform_functions_and_reservations_shape_deterministic_auto_allocation() {
    let platform_bdf =
        |device: u8, function: u8| PciBdf::new(PciSegment::new(0), 0, device, function).unwrap();

    let build = |reverse_platform: bool| -> ResolvedPciTopology {
        let mut builder = PciTopologyBuilder::new();
        // The host bridge and LPC are real guest-enumerable platform
        // functions declared below; only contract-kept holes are reserved.
        builder.reserve_bdf(platform_bdf(3, 0)).unwrap();
        builder.reserve_bdf(platform_bdf(3, 0)).unwrap();
        let mut platform = [
            (
                "host-bridge",
                PciEndpointIdentity::new(0x8086, 0x29c0, PciClass::new(0x06, 0x00, 0x00)),
                platform_bdf(0, 0),
            ),
            (
                "lpc",
                PciEndpointIdentity::new(0x8086, 0x2918, PciClass::new(0x06, 0x01, 0x00)),
                platform_bdf(31, 0),
            ),
        ];
        if reverse_platform {
            platform.reverse();
        }
        for (id, identity, position) in platform {
            builder
                .add_function(
                    PciFunctionSpec::new(function_id(id), identity)
                        .with_bdf(ResourceRequest::Fixed(position)),
                )
                .unwrap();
        }
        builder
            .add_function(auto_bar_function("beta", 0x1000))
            .unwrap();
        builder
            .add_function(auto_bar_function("alpha", 0x2000))
            .unwrap();
        builder.resolve(APERTURE_BASE..APERTURE_END).unwrap()
    };

    let forward = build(false);
    let reversed = build(true);

    // Automatic placement skips fixed platform functions and reservations.
    // Allocation follows stable node-id order across whole devices.
    assert_eq!(
        forward.function(&function_id("beta")).unwrap().bdf(),
        PciBdf::new(PciSegment::new(0), 0, 2, 0).unwrap()
    );
    assert_eq!(
        forward.function(&function_id("alpha")).unwrap().bdf(),
        PciBdf::new(PciSegment::new(0), 0, 1, 0).unwrap()
    );
    // Declaration order never changes the resolved placement.
    for id in ["alpha", "beta", "host-bridge", "lpc"] {
        assert_eq!(
            forward.function(&function_id(id)).map(|f| f.bdf()),
            reversed.function(&function_id(id)).map(|f| f.bdf())
        );
    }
}

#[test]
fn reserved_bdfs_reject_fixed_requests() {
    let reserved = PciBdf::new(PciSegment::new(0), 0, 5, 0).unwrap();
    let mut builder = PciTopologyBuilder::new();
    builder.reserve_bdf(reserved).unwrap();
    builder
        .add_function(bare_function("clash").with_bdf(ResourceRequest::Fixed(reserved)))
        .unwrap();

    match builder.resolve(APERTURE_BASE..APERTURE_END) {
        Err(PciError::BdfReserved { bdf, .. }) => assert_eq!(bdf, reserved),
        other => panic!("reserved placement must fail deterministically, got {other:?}"),
    }
}

#[test]
fn automatic_topology_assignment_is_stable_and_reset_restores_power_on_state() {
    let mut first = PciTopologyBuilder::new();
    first
        .add_function(auto_bar_function("beta", 0x2000))
        .unwrap();
    first
        .add_function(auto_bar_function("alpha", 0x1000))
        .unwrap();
    let first = Arc::new(first.resolve(APERTURE_BASE..APERTURE_END).unwrap());

    let mut second = PciTopologyBuilder::new();
    second
        .add_function(auto_bar_function("alpha", 0x1000))
        .unwrap();
    second
        .add_function(auto_bar_function("beta", 0x2000))
        .unwrap();
    let second = second.resolve(APERTURE_BASE..APERTURE_END).unwrap();

    assert_eq!(
        first.function(&function_id("alpha")).unwrap().bdf(),
        second.function(&function_id("alpha")).unwrap().bdf()
    );
    assert_eq!(
        first.function(&function_id("beta")).unwrap().bdf(),
        second.function(&function_id("beta")).unwrap().bdf()
    );
    let bar0 = PciBarIndex::new(0).unwrap();
    assert_eq!(
        first
            .function(&function_id("beta"))
            .unwrap()
            .bar(bar0)
            .unwrap()
            .address(),
        APERTURE_BASE
    );
    assert_eq!(
        first
            .function(&function_id("alpha"))
            .unwrap()
            .bar(bar0)
            .unwrap()
            .address(),
        APERTURE_BASE + 0x2000
    );
    assert_eq!(
        first.function(&function_id("alpha")).unwrap().bar(bar0),
        second.function(&function_id("alpha")).unwrap().bar(bar0)
    );

    let (pci, runtime) = build_pci(
        [auto_bar_function("alpha", 0x1000)],
        [("alpha", Arc::new(RecordingFunction::default()))],
    );
    let alpha = pci
        .topology()
        .function(&function_id("alpha"))
        .unwrap()
        .bdf();
    write_config(&runtime, alpha, 4, AccessWidth::Word, 2);
    runtime.reset_lifecycle_devices().unwrap();
    assert_eq!(read_config(&runtime, alpha, 4, AccessWidth::Word), 0);
}

fn resolve_topology<const N: usize>(specs: [PciFunctionSpec; N]) -> PciResult<ResolvedPciTopology> {
    let mut builder = PciTopologyBuilder::new();
    for spec in specs {
        builder.add_function(spec)?;
    }
    builder.resolve(APERTURE_BASE..APERTURE_END)
}

fn function_id(value: &str) -> DeviceNodeId {
    DeviceNodeId::new(value).unwrap()
}

fn endpoint_identity() -> PciEndpointIdentity {
    PciEndpointIdentity::new(0x1234, 0x5678, PciClass::new(0xff, 0, 0))
}

fn bare_function(id: &str) -> PciFunctionSpec {
    PciFunctionSpec::new(function_id(id), endpoint_identity())
}

fn auto_bar_function(id: &str, size: u64) -> PciFunctionSpec {
    let bar = PciMemoryBar::new(
        PciBarIndex::new(0).unwrap(),
        size,
        PciMemoryBarWidth::Bits32,
    )
    .unwrap();
    bare_function(id).with_bar(bar).unwrap()
}

fn fixed_bar_function(id: &str, bdf: PciBdf, address: u64) -> PciFunctionSpec {
    let bar = PciMemoryBar::new(
        PciBarIndex::new(0).unwrap(),
        0x1000,
        PciMemoryBarWidth::Bits32,
    )
    .unwrap()
    .with_address(ResourceRequest::Fixed(address));
    PciFunctionSpec::new(function_id(id), endpoint_identity())
        .with_bdf(ResourceRequest::Fixed(bdf))
        .with_bar(bar)
        .unwrap()
}

fn build_pci<const N: usize>(
    specs: [PciFunctionSpec; N],
    functions: [(&str, Arc<RecordingFunction>); N],
) -> (ResolvedPciBus, DeviceRuntime) {
    let host_id = function_id("pci-host");
    let host_resources = PciHostResourceRequirements::new(
        APERTURE_END - APERTURE_BASE,
        APERTURE_END - APERTURE_BASE,
    )
    .unwrap();
    let mut pci = PciBusGraphBuilder::new(host_id, host_resources);
    let mut graph = DeviceGraphBuilder::new();
    graph.add(pci.host_node()).unwrap();
    for (spec, (_, function)) in specs.into_iter().zip(functions) {
        let model: Arc<dyn PciEndpointModel> = Arc::new(StaticEndpointModel { function });
        graph.add(pci.endpoint_node(spec, model).unwrap()).unwrap();
    }
    let mut pools = ResourcePools::new();
    pools
        .add_auto_mmio(ECAM_BASE..ECAM_BASE + 0x10_0000)
        .unwrap();
    pools.add_auto_mmio(APERTURE_BASE..APERTURE_END).unwrap();
    let graph = graph.declare().unwrap().resolve(pools).unwrap();
    let pci = pci.resolve(&graph).unwrap();
    let mut builder = DeviceRuntimeBuilder::new(RuntimeAccessPorts::new());
    for node in graph.nodes() {
        builder
            .build_graph_node(node, graph.resource_plan())
            .unwrap();
    }
    let runtime = builder.finish(graph.resource_plan()).unwrap();
    (pci, runtime)
}

fn config_address(bdf: PciBdf, offset: u16) -> GuestPhysAddr {
    GuestPhysAddr::from_usize((ECAM_BASE + bdf.ecam_offset() + u64::from(offset)) as usize)
}

fn device_access(address: GuestPhysAddr, width: AccessWidth) -> DeviceAccess {
    DeviceAccess::new(
        DeviceVcpuId::new(0),
        BusKind::Mmio,
        address.as_usize() as u64,
        width,
    )
}

fn read_config(runtime: &DeviceRuntime, bdf: PciBdf, offset: u16, width: AccessWidth) -> u64 {
    runtime
        .try_read(&device_access(config_address(bdf, offset), width))
        .unwrap()
        .unwrap()
}

fn write_config(runtime: &DeviceRuntime, bdf: PciBdf, offset: u16, width: AccessWidth, value: u64) {
    assert!(
        runtime
            .try_write(
                &device_access(config_address(bdf, offset), width),
                value,
                None,
            )
            .unwrap()
    );
}

fn read_mmio(runtime: &DeviceRuntime, address: u64, width: AccessWidth) -> Result<u64, String> {
    runtime
        .try_read(&device_access(
            GuestPhysAddr::from_usize(address as usize),
            width,
        ))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "MMIO address is not routed".into())
}

fn write_mmio(
    runtime: &DeviceRuntime,
    address: u64,
    width: AccessWidth,
    value: u64,
) -> Result<(), String> {
    runtime
        .try_write(
            &device_access(GuestPhysAddr::from_usize(address as usize), width),
            value,
            None,
        )
        .map_err(|error| error.to_string())?
        .then_some(())
        .ok_or_else(|| "MMIO address is not routed".into())
}
