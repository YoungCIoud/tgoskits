//! Conditional AArch64 PCI bus construction and resolved firmware view.

use std::vec::Vec;

use axdevice::{
    DeviceNodeId, DeviceNodeSpec, PciBusGraphBuilder, PciHostResourceRequirements,
    ResolvedDeviceGraph, ResolvedPciBus,
};

use crate::{
    AxVmError, AxVmResult, ConfiguredPciEndpoint, boot::fdt::core::pci::GuestPciHost,
    config::AxVMConfig,
};

const PCI_HOST_ID: &str = "pci-host";
const PCI_MEMORY_APERTURE_SIZE: u64 = 0x0400_0000;

pub(super) struct Aarch64PciPlanBuilder {
    bus: PciBusGraphBuilder,
}

impl Aarch64PciPlanBuilder {
    pub(super) fn declare(
        config: &AxVMConfig,
        controller: &DeviceNodeId,
        endpoints: Vec<ConfiguredPciEndpoint>,
    ) -> AxVmResult<Option<(Self, Vec<DeviceNodeSpec>)>> {
        if endpoints.is_empty() {
            return Ok(None);
        }
        if config.image_config().dtb_load_gpa.is_none() {
            return Err(AxVmError::unsupported(
                "create AArch64 virtual PCI host",
                "configured PCI endpoints require a guest DTB; UEFI/ACPI PCI is not implemented",
            ));
        }

        let requirements =
            PciHostResourceRequirements::new(PCI_MEMORY_APERTURE_SIZE, PCI_MEMORY_APERTURE_SIZE)
                .map_err(|error| AxVmError::device("declare AArch64 PCI host resources", error))?;
        let mut bus = PciBusGraphBuilder::new(DeviceNodeId::new(PCI_HOST_ID)?, requirements);
        let mut nodes = Vec::with_capacity(endpoints.len() + 1);
        nodes.push(bus.host_node().with_dependency(controller.clone()));
        for endpoint in endpoints {
            let (function, model) = endpoint.into_parts();
            nodes.push(
                bus.endpoint_node(function, model)
                    .map_err(|error| AxVmError::device("declare AArch64 PCI endpoint", error))?,
            );
        }
        Ok(Some((Self { bus }, nodes)))
    }

    pub(super) fn resolve(self, graph: &ResolvedDeviceGraph) -> AxVmResult<Aarch64PciPlan> {
        let bus = self.bus.resolve(graph)?;
        let firmware = GuestPciHost::from_windows(bus.ecam_window(), bus.memory_aperture());
        Ok(Aarch64PciPlan {
            _bus: bus,
            firmware,
        })
    }
}

pub(super) struct Aarch64PciPlan {
    // The resolved bus owns the shared topology publication consumed by the
    // host and endpoint runtime models for this VM plan.
    _bus: ResolvedPciBus,
    firmware: GuestPciHost,
}

impl Aarch64PciPlan {
    pub(super) const fn firmware(&self) -> GuestPciHost {
        self.firmware
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axdevice::{
        DeviceBuildContext, DeviceGraphBuilder, DeviceManagerResult, DeviceRequirements,
        PciBarAccess, PciBarIndex, PciClass, PciEndpointBundle, PciEndpointIdentity,
        PciEndpointModel, PciFunction, PciFunctionSpec, PciMemoryBar, PciMemoryBarWidth,
        ResourcePools,
    };
    use axdevice_base::{DeviceContext, DeviceResult, GuestPhysAddr};

    use super::*;
    use crate::config::{AxVMConfigParams, PhysCpuList, VMImageConfig};

    struct TestFunction;

    impl PciFunction for TestFunction {
        fn name(&self) -> &str {
            "aarch64-test-pci"
        }

        fn read_bar(
            &self,
            _access: &PciBarAccess,
            _context: &mut dyn DeviceContext,
        ) -> DeviceResult<u64> {
            Ok(0)
        }

        fn write_bar(
            &self,
            _access: &PciBarAccess,
            _value: u64,
            _context: &mut dyn DeviceContext,
        ) -> DeviceResult {
            Ok(())
        }
    }

    struct TestEndpointModel;

    impl PciEndpointModel for TestEndpointModel {
        fn requirements(&self) -> DeviceManagerResult<DeviceRequirements> {
            Ok(DeviceRequirements::new())
        }

        fn build(
            &self,
            _context: &mut DeviceBuildContext<'_>,
        ) -> DeviceManagerResult<PciEndpointBundle> {
            Ok(PciEndpointBundle::new(Arc::new(TestFunction)))
        }
    }

    fn config(with_dtb: bool) -> AxVMConfig {
        AxVMConfig::new(AxVMConfigParams {
            phys_cpu_ls: PhysCpuList::new(1, None, None),
            image_config: VMImageConfig {
                dtb_load_gpa: with_dtb.then(|| GuestPhysAddr::from_usize(0x4000_0000)),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    fn endpoint(id: &str) -> ConfiguredPciEndpoint {
        let function = PciFunctionSpec::new(
            DeviceNodeId::new(id).unwrap(),
            PciEndpointIdentity::new(0x110a, 0x4106, PciClass::new(0xff, 0, 0)),
        )
        .with_bar(
            PciMemoryBar::new(
                PciBarIndex::new(0).unwrap(),
                0x1000,
                PciMemoryBarWidth::Bits32,
            )
            .unwrap(),
        )
        .unwrap();
        ConfiguredPciEndpoint::new(function, Arc::new(TestEndpointModel))
    }

    #[test]
    fn empty_endpoint_set_creates_no_host_nodes() {
        let controller = DeviceNodeId::new("vgic").unwrap();
        assert!(
            Aarch64PciPlanBuilder::declare(&config(true), &controller, Vec::new())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn endpoint_creates_vgic_host_endpoint_order_and_resolved_firmware() {
        let controller = DeviceNodeId::new("vgic").unwrap();
        let (builder, nodes) =
            Aarch64PciPlanBuilder::declare(&config(true), &controller, vec![endpoint("endpoint0")])
                .unwrap()
                .unwrap();
        let mut graph = DeviceGraphBuilder::new();
        graph
            .add(DeviceNodeSpec::firmware_only(controller.clone()))
            .unwrap();
        for node in nodes {
            graph.add(node).unwrap();
        }
        let mut pools = ResourcePools::new();
        pools.add_auto_mmio(0x0b00_0000..0x1000_0000).unwrap();
        let graph = graph.declare().unwrap().resolve(pools).unwrap();
        let ids = graph
            .nodes()
            .map(|node| node.id().as_str())
            .collect::<Vec<_>>();

        assert_eq!(ids, ["vgic", "pci-host", "endpoint0"]);
        let plan = builder.resolve(&graph).unwrap();
        assert_eq!(plan.firmware().ecam_base(), 0x0b00_0000);
        assert_eq!(plan.firmware().memory_base(), 0x0c00_0000);
        assert_eq!(plan.firmware().memory_size(), 0x0400_0000);
    }

    #[test]
    fn pci_host_fails_when_the_aarch64_mmio_pool_cannot_fit_the_aperture() {
        let controller = DeviceNodeId::new("vgic").unwrap();
        let (builder, nodes) =
            Aarch64PciPlanBuilder::declare(&config(true), &controller, vec![endpoint("endpoint0")])
                .unwrap()
                .unwrap();
        let mut graph = DeviceGraphBuilder::new();
        graph
            .add(DeviceNodeSpec::firmware_only(controller))
            .unwrap();
        for node in nodes {
            graph.add(node).unwrap();
        }
        let mut pools = ResourcePools::new();
        pools.add_auto_mmio(0x0b00_0000..0x0c00_0000).unwrap();

        let error = match graph.declare().unwrap().resolve(pools) {
            Ok(_) => panic!("undersized AArch64 MMIO pool must reject the PCI aperture"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("mmio auto pool is exhausted"));
        drop(builder);
    }

    #[test]
    fn pci_endpoint_without_guest_dtb_is_rejected() {
        let controller = DeviceNodeId::new("vgic").unwrap();
        let error = Aarch64PciPlanBuilder::declare(
            &config(false),
            &controller,
            vec![endpoint("endpoint0")],
        )
        .err()
        .unwrap();

        assert!(matches!(error, AxVmError::Unsupported { .. }));
        assert!(error.to_string().contains("require a guest DTB"));
    }
}
