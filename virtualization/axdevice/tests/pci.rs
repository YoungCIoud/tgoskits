use std::sync::Arc;

use axdevice::{
    AccessWidth, ConfigOffset, DeviceNodeId, PciBarIndex, PciBdf, PciClass, PciEndpointIdentity,
    PciError, PciFunctionSpec, PciMemoryBar, PciRootState, PciSegment, PciTopologyBuilder,
    ResourceRequest,
};

const APERTURE_START: u64 = 0x2000_0000;
const APERTURE_END: u64 = 0x2040_0000;
const BAR_SIZE: u64 = 0x1_0000;

#[test]
fn rejects_addresses_outside_conventional_pci_ranges() {
    assert!(matches!(
        PciBdf::new(PciSegment::new(0), 0, 32, 0),
        Err(PciError::InvalidAddress {
            component: "device",
            ..
        })
    ));
    assert!(matches!(
        PciBdf::new(PciSegment::new(0), 0, 0, 8),
        Err(PciError::InvalidAddress {
            component: "function",
            ..
        })
    ));
    assert!(matches!(
        PciBarIndex::new(6),
        Err(PciError::InvalidAddress {
            component: "BAR index",
            ..
        })
    ));
    assert!(matches!(
        ConfigOffset::new(0x100),
        Err(PciError::InvalidAddress {
            component: "config offset",
            ..
        })
    ));
}

#[test]
fn rejects_invalid_memory_bar_sizes() {
    let bar = PciBarIndex::new(2).unwrap();
    assert!(matches!(
        PciMemoryBar::new(bar, 0),
        Err(PciError::InvalidBar { .. })
    ));
    assert!(matches!(
        PciMemoryBar::new(bar, 0x18),
        Err(PciError::InvalidBar { .. })
    ));
    assert!(matches!(
        PciMemoryBar::new(bar, 1_u64 << 33),
        Err(PciError::InvalidBar { .. })
    ));
}

#[test]
fn rejects_absent_vendor_identity_and_invalid_host_apertures() {
    let mut identity = PciTopologyBuilder::new();
    identity
        .add_function(PciFunctionSpec::new(
            node("absent"),
            PciEndpointIdentity::new(u16::MAX, 0x5678, PciClass::new(0x05, 0x00, 0x00)),
        ))
        .unwrap();
    assert!(matches!(
        identity.resolve(APERTURE_START..APERTURE_END),
        Err(PciError::InvalidEndpointIdentity { .. })
    ));

    assert!(matches!(
        PciTopologyBuilder::new().resolve(APERTURE_START..APERTURE_START),
        Err(PciError::InvalidHostAperture { .. })
    ));
    assert!(matches!(
        PciTopologyBuilder::new().resolve(APERTURE_START..(1_u64 << 32) + 1),
        Err(PciError::InvalidHostAperture { .. })
    ));
}

#[test]
fn resolves_auto_bdfs_deterministically_and_skips_reservations() {
    let mut builder = PciTopologyBuilder::new();
    builder.reserve_bdf(bdf(0, 0)).unwrap();
    builder.reserve_bdf(bdf(31, 0)).unwrap();
    builder.add_function(function("beta")).unwrap();
    builder.add_function(function("alpha")).unwrap();

    let topology = builder.resolve(APERTURE_START..APERTURE_END).unwrap();

    assert_eq!(topology.function(&node("alpha")).unwrap().bdf(), bdf(1, 0));
    assert_eq!(topology.function(&node("beta")).unwrap().bdf(), bdf(2, 0));
}

#[test]
fn rejects_fixed_requests_for_nonzero_functions() {
    let mut builder = PciTopologyBuilder::new();
    builder
        .add_function(function("endpoint").with_bdf(ResourceRequest::Fixed(bdf(3, 1))))
        .unwrap();
    assert!(matches!(
        builder.resolve(APERTURE_START..APERTURE_END),
        Err(PciError::UnsupportedFunctionPlacement { .. })
    ));
}

#[test]
fn rejects_fixed_bdf_conflicts_and_reserved_requests() {
    let mut duplicate = PciTopologyBuilder::new();
    duplicate
        .add_function(function("alpha").with_bdf(ResourceRequest::Fixed(bdf(3, 0))))
        .unwrap();
    duplicate
        .add_function(function("beta").with_bdf(ResourceRequest::Fixed(bdf(3, 0))))
        .unwrap();
    assert!(matches!(
        duplicate.resolve(APERTURE_START..APERTURE_END),
        Err(PciError::DuplicateBdf { .. })
    ));

    let mut reserved = PciTopologyBuilder::new();
    reserved.reserve_bdf(bdf(3, 0)).unwrap();
    reserved
        .add_function(function("endpoint").with_bdf(ResourceRequest::Fixed(bdf(3, 0))))
        .unwrap();
    assert!(matches!(
        reserved.resolve(APERTURE_START..APERTURE_END),
        Err(PciError::BdfReserved { .. })
    ));
}

#[test]
fn places_larger_auto_bars_first_and_preserves_function_order_tiebreaks() {
    let bar0 = PciBarIndex::new(0).unwrap();
    let bar2 = PciBarIndex::new(2).unwrap();
    let mut builder = PciTopologyBuilder::new();
    builder
        .add_function(
            function("beta")
                .with_bar(PciMemoryBar::new(bar0, 0x1_0000).unwrap())
                .unwrap(),
        )
        .unwrap();
    builder
        .add_function(
            function("alpha")
                .with_bar(PciMemoryBar::new(bar2, 0x20_0000).unwrap())
                .unwrap(),
        )
        .unwrap();

    let topology = builder.resolve(APERTURE_START..APERTURE_END).unwrap();

    assert_eq!(
        topology
            .function(&node("alpha"))
            .unwrap()
            .bar(bar2)
            .unwrap()
            .address(),
        APERTURE_START
    );
    assert_eq!(
        topology
            .function(&node("beta"))
            .unwrap()
            .bar(bar0)
            .unwrap()
            .address(),
        APERTURE_START + 0x20_0000
    );
}

#[test]
fn rejects_fixed_bars_outside_or_overlapping_the_aperture() {
    let bar0 = PciBarIndex::new(0).unwrap();
    let mut outside = PciTopologyBuilder::new();
    outside
        .add_function(
            function("outside")
                .with_bar(
                    PciMemoryBar::new(bar0, BAR_SIZE)
                        .unwrap()
                        .with_address(ResourceRequest::Fixed(APERTURE_END)),
                )
                .unwrap(),
        )
        .unwrap();
    assert!(matches!(
        outside.resolve(APERTURE_START..APERTURE_END),
        Err(PciError::InvalidBar { .. })
    ));

    let mut overlap = PciTopologyBuilder::new();
    for id in ["alpha", "beta"] {
        overlap
            .add_function(
                function(id)
                    .with_bar(
                        PciMemoryBar::new(bar0, BAR_SIZE)
                            .unwrap()
                            .with_address(ResourceRequest::Fixed(APERTURE_START)),
                    )
                    .unwrap(),
            )
            .unwrap();
    }
    assert!(matches!(
        overlap.resolve(APERTURE_START..APERTURE_END),
        Err(PciError::BarConflict { .. })
    ));
}

#[test]
fn exposes_a_256_byte_type_zero_config_image() {
    let (root, endpoint_bdf, _) = root_with_bar();

    assert_eq!(
        root.read_config(endpoint_bdf, offset(0), AccessWidth::Dword)
            .unwrap(),
        0x5678_1234
    );
    assert_eq!(
        root.read_config(endpoint_bdf, offset(8), AccessWidth::Dword)
            .unwrap(),
        0x0500_0001
    );
    assert_eq!(
        root.read_config(endpoint_bdf, offset(0x0e), AccessWidth::Byte)
            .unwrap(),
        0
    );
    assert!(matches!(
        ConfigOffset::new(0x100),
        Err(PciError::InvalidAddress { .. })
    ));
}

#[test]
fn absent_functions_read_all_ones_and_ignore_writes() {
    let (root, ..) = root_with_bar();
    let absent = bdf(30, 0);

    assert_eq!(
        root.read_config(absent, offset(0), AccessWidth::Word)
            .unwrap(),
        0xffff
    );
    root.write_config(absent, offset(4), AccessWidth::Word, 0xffff)
        .unwrap();
    assert_eq!(
        root.read_config(absent, offset(4), AccessWidth::Word)
            .unwrap(),
        0xffff
    );
}

#[test]
fn rejects_misaligned_and_qword_config_accesses() {
    let (root, endpoint_bdf, _) = root_with_bar();

    assert!(matches!(
        root.read_config(endpoint_bdf, offset(1), AccessWidth::Word),
        Err(PciError::InvalidConfigAccess { .. })
    ));
    assert!(matches!(
        root.read_config(endpoint_bdf, offset(0), AccessWidth::Qword),
        Err(PciError::InvalidConfigAccess { .. })
    ));
}

#[test]
fn platform_config_bytes_cannot_override_core_identity_or_bars() {
    let function = PciFunctionSpec::new(
        node("platform"),
        PciEndpointIdentity::new(0x1234, 0x5678, PciClass::new(0x06, 0, 0)),
    );
    assert!(matches!(
        function
            .clone()
            .with_platform_config_byte(ConfigOffset::new(0).unwrap(), 0, u8::MAX),
        Err(PciError::InvalidConfigPatch { offset: 0, .. })
    ));
    assert!(matches!(
        function.with_platform_config_byte(ConfigOffset::new(0x10).unwrap(), 0, u8::MAX),
        Err(PciError::InvalidConfigPatch { offset: 0x10, .. })
    ));
}

#[test]
fn command_register_only_accepts_memory_space_enable() {
    let (root, endpoint_bdf, bar_base) = root_with_bar();

    assert!(root.resolve_bar(bar_base, AccessWidth::Dword).is_none());
    root.write_config(endpoint_bdf, offset(4), AccessWidth::Word, 0xffff)
        .unwrap();

    assert_eq!(
        root.read_config(endpoint_bdf, offset(4), AccessWidth::Word)
            .unwrap(),
        0x0002
    );
    let route = root.resolve_bar(bar_base + 8, AccessWidth::Dword).unwrap();
    assert_eq!(route.bdf(), endpoint_bdf);
    assert_eq!(route.bar(), PciBarIndex::new(2).unwrap());
    assert_eq!(route.offset(), 8);
    assert_eq!(route.width(), AccessWidth::Dword);
    assert!(
        root.resolve_bar(bar_base + BAR_SIZE - 2, AccessWidth::Dword)
            .is_none()
    );
}

#[test]
fn bar_probe_reports_size_without_changing_the_runtime_route() {
    let (root, endpoint_bdf, bar_base) = enabled_root_with_bar();

    root.write_config(
        endpoint_bdf,
        offset(0x18),
        AccessWidth::Dword,
        u64::from(u32::MAX),
    )
    .unwrap();

    assert_eq!(
        root.read_config(endpoint_bdf, offset(0x18), AccessWidth::Dword)
            .unwrap(),
        0xffff_0000
    );
    assert!(root.resolve_bar(bar_base, AccessWidth::Byte).is_some());
}

#[test]
fn valid_bar_relocation_moves_the_route() {
    let (root, endpoint_bdf, old_base) = enabled_root_with_bar();
    let new_base = APERTURE_START + 0x10_0000;

    root.write_config(endpoint_bdf, offset(0x18), AccessWidth::Dword, new_base)
        .unwrap();

    assert_eq!(
        root.read_config(endpoint_bdf, offset(0x18), AccessWidth::Dword)
            .unwrap(),
        new_base
    );
    assert!(root.resolve_bar(old_base, AccessWidth::Byte).is_none());
    assert!(root.resolve_bar(new_base, AccessWidth::Byte).is_some());
}

#[test]
fn partial_bar_write_uses_the_merged_dword_and_preserves_attributes() {
    let (root, endpoint_bdf, old_base) = enabled_root_with_bar();
    let new_base = APERTURE_START + 0x10_0000;

    root.write_config(
        endpoint_bdf,
        offset(0x1a),
        AccessWidth::Word,
        new_base >> 16,
    )
    .unwrap();

    assert_eq!(
        root.read_config(endpoint_bdf, offset(0x18), AccessWidth::Dword)
            .unwrap(),
        new_base
    );
    assert_eq!(
        root.read_config(endpoint_bdf, offset(0x18), AccessWidth::Byte)
            .unwrap(),
        0
    );
    assert!(root.resolve_bar(old_base, AccessWidth::Byte).is_none());
    assert!(root.resolve_bar(new_base, AccessWidth::Byte).is_some());
}

#[test]
fn invalid_bar_relocation_preserves_config_and_route() {
    let (root, endpoint_bdf, old_base) = enabled_root_with_bar();

    root.write_config(
        endpoint_bdf,
        offset(0x18),
        AccessWidth::Dword,
        APERTURE_START + 0x10,
    )
    .unwrap();

    assert_eq!(
        root.read_config(endpoint_bdf, offset(0x18), AccessWidth::Dword)
            .unwrap(),
        old_base
    );
    assert!(root.resolve_bar(old_base, AccessWidth::Byte).is_some());
}

#[test]
fn overlapping_bar_relocation_preserves_the_previous_route() {
    let bar2 = PciBarIndex::new(2).unwrap();
    let alpha_base = APERTURE_START;
    let beta_base = APERTURE_START + BAR_SIZE;
    let mut builder = PciTopologyBuilder::new();
    for (id, base) in [("alpha", alpha_base), ("beta", beta_base)] {
        builder
            .add_function(
                function(id)
                    .with_bar(
                        PciMemoryBar::new(bar2, BAR_SIZE)
                            .unwrap()
                            .with_address(ResourceRequest::Fixed(base)),
                    )
                    .unwrap(),
            )
            .unwrap();
    }
    let topology = Arc::new(builder.resolve(APERTURE_START..APERTURE_END).unwrap());
    let alpha_bdf = topology.function(&node("alpha")).unwrap().bdf();
    let beta_bdf = topology.function(&node("beta")).unwrap().bdf();
    let root = PciRootState::new(topology);
    for endpoint_bdf in [alpha_bdf, beta_bdf] {
        root.write_config(endpoint_bdf, offset(4), AccessWidth::Word, 0x0002)
            .unwrap();
    }

    root.write_config(beta_bdf, offset(0x18), AccessWidth::Dword, alpha_base)
        .unwrap();

    assert_eq!(
        root.read_config(beta_bdf, offset(0x18), AccessWidth::Dword)
            .unwrap(),
        beta_base
    );
    assert_eq!(
        root.resolve_bar(alpha_base, AccessWidth::Byte)
            .unwrap()
            .bdf(),
        alpha_bdf
    );
    assert_eq!(
        root.resolve_bar(beta_base, AccessWidth::Byte)
            .unwrap()
            .bdf(),
        beta_bdf
    );
}

#[test]
fn reset_restores_root_owned_command_bar_and_route_state() {
    let (root, endpoint_bdf, power_on_base) = enabled_root_with_bar();
    let relocated = APERTURE_START + 0x10_0000;
    root.write_config(endpoint_bdf, offset(0x18), AccessWidth::Dword, relocated)
        .unwrap();
    assert!(root.resolve_bar(relocated, AccessWidth::Byte).is_some());

    root.reset();

    assert_eq!(
        root.read_config(endpoint_bdf, offset(4), AccessWidth::Word)
            .unwrap(),
        0
    );
    assert_eq!(
        root.read_config(endpoint_bdf, offset(0x18), AccessWidth::Dword)
            .unwrap(),
        power_on_base
    );
    assert!(root.resolve_bar(power_on_base, AccessWidth::Byte).is_none());
}

fn enabled_root_with_bar() -> (PciRootState, PciBdf, u64) {
    let (root, endpoint_bdf, bar_base) = root_with_bar();
    root.write_config(endpoint_bdf, offset(4), AccessWidth::Word, 0x0002)
        .unwrap();
    (root, endpoint_bdf, bar_base)
}

fn root_with_bar() -> (PciRootState, PciBdf, u64) {
    let bar2 = PciBarIndex::new(2).unwrap();
    let endpoint = function("endpoint")
        .with_bar(PciMemoryBar::new(bar2, BAR_SIZE).unwrap())
        .unwrap();
    let mut builder = PciTopologyBuilder::new();
    builder.add_function(endpoint).unwrap();
    let topology = Arc::new(builder.resolve(APERTURE_START..APERTURE_END).unwrap());
    let function = topology.function(&node("endpoint")).unwrap();
    let endpoint_bdf = function.bdf();
    let bar_base = function.bar(bar2).unwrap().address();
    (PciRootState::new(topology), endpoint_bdf, bar_base)
}

fn function(id: &str) -> PciFunctionSpec {
    PciFunctionSpec::new(
        node(id),
        PciEndpointIdentity::new(0x1234, 0x5678, PciClass::new(0x05, 0x00, 0x00)).with_revision(1),
    )
}

fn node(id: &str) -> DeviceNodeId {
    DeviceNodeId::new(id).unwrap()
}

fn bdf(device: u8, function: u8) -> PciBdf {
    PciBdf::new(PciSegment::new(0), 0, device, function).unwrap()
}

fn offset(value: u16) -> ConfigOffset {
    ConfigOffset::new(value).unwrap()
}
