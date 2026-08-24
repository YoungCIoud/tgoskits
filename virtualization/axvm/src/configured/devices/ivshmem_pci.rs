//! Initial configured ivshmem PCI endpoint with one memory BAR.
//!
//! This model intentionally implements only a private, zero-initialized BAR2
//! aperture. Shared backing, control registers, peer notification, and MSI-X
//! remain outside this initial vPCI registration path.

use std::sync::{Arc, Mutex};

use axdevice::*;
use axdevice_base::{DeviceContext, DeviceError, DeviceResult};
use axvmconfig::VirtualDeviceRequest;

use crate::{
    ConfiguredDeviceError, ConfiguredPciEndpoint, ConfiguredPciModelRegistration,
    DeviceInstantiationContext,
};

const IVSHMEM_VENDOR_ID: u16 = 0x1af4;
const IVSHMEM_DEVICE_ID: u16 = 0x1110;
const SHARED_MEMORY_BAR_INDEX: u8 = 2;
const SHARED_MEMORY_SIZE: usize = 0x1_0000;

const REGISTRATION: ConfiguredPciModelRegistration = ConfiguredPciModelRegistration {
    model: "ivshmem-pci",
    create: create_ivshmem_pci,
};

pub(super) fn register(
    catalog: &mut crate::ConfiguredDeviceCatalog,
) -> Result<(), ConfiguredDeviceError> {
    catalog.register_pci_model(module_path!(), REGISTRATION)
}

#[derive(Clone, Copy, Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct IvshmemPciOptions {}

fn create_ivshmem_pci(
    id: DeviceNodeId,
    request: &VirtualDeviceRequest,
    _context: &DeviceInstantiationContext,
) -> Result<ConfiguredPciEndpoint, ConfiguredDeviceError> {
    request
        .deserialize_options::<IvshmemPciOptions>()
        .map_err(|error| ConfiguredDeviceError::InvalidOptions {
            device: request.id.clone(),
            model: request.model.clone(),
            detail: error.to_string(),
        })?;

    let identity = PciEndpointIdentity::new(
        IVSHMEM_VENDOR_ID,
        IVSHMEM_DEVICE_ID,
        PciClass::new(0x05, 0x00, 0x00),
    );
    let bar = PciMemoryBar::new(
        PciBarIndex::new(SHARED_MEMORY_BAR_INDEX)
            .map_err(|error| endpoint_error(request, "create ivshmem BAR2 index", error))?,
        SHARED_MEMORY_SIZE as u64,
        PciMemoryBarWidth::Bits32,
    )
    .map_err(|error| endpoint_error(request, "create ivshmem BAR2", error))?;
    let function = PciFunctionSpec::new(id, identity)
        .with_bar(bar)
        .map_err(|error| endpoint_error(request, "attach ivshmem BAR2", error))?;
    Ok(ConfiguredPciEndpoint::new(
        function,
        Arc::new(IvshmemPciModel),
    ))
}

fn endpoint_error(
    request: &VirtualDeviceRequest,
    operation: &'static str,
    error: impl core::fmt::Display,
) -> ConfiguredDeviceError {
    ConfiguredDeviceError::Instantiation {
        device: request.id.clone(),
        model: request.model.clone(),
        detail: std::format!("{operation}: {error}"),
    }
}

struct IvshmemPciModel;

impl PciEndpointModel for IvshmemPciModel {
    fn requirements(&self) -> DeviceManagerResult<DeviceRequirements> {
        Ok(DeviceRequirements::new())
    }

    fn build(
        &self,
        _context: &mut DeviceBuildContext<'_>,
    ) -> DeviceManagerResult<PciEndpointBundle> {
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(SHARED_MEMORY_SIZE).map_err(|_| {
            DeviceManagerError::InvalidState {
                operation: "allocate initial ivshmem BAR2 backing",
                detail: "64 KiB backing allocation failed".into(),
            }
        })?;
        bytes.resize(SHARED_MEMORY_SIZE, 0);
        Ok(PciEndpointBundle::new(Arc::new(IvshmemPciFunction {
            bytes: Mutex::new(bytes.into_boxed_slice()),
        })))
    }
}

struct IvshmemPciFunction {
    bytes: Mutex<Box<[u8]>>,
}

impl IvshmemPciFunction {
    fn access_range(access: &PciBarAccess) -> DeviceResult<core::ops::Range<usize>> {
        if access.bar().value() != SHARED_MEMORY_BAR_INDEX {
            return Err(DeviceError::OutOfRange {
                addr: access.offset(),
            });
        }
        let start = usize::try_from(access.offset()).map_err(|_| DeviceError::OutOfRange {
            addr: access.offset(),
        })?;
        let end = start
            .checked_add(access.width().size())
            .filter(|end| *end <= SHARED_MEMORY_SIZE)
            .ok_or(DeviceError::OutOfRange {
                addr: access.offset(),
            })?;
        Ok(start..end)
    }
}

impl PciFunction for IvshmemPciFunction {
    fn name(&self) -> &str {
        "ivshmem-pci"
    }

    fn read_bar(
        &self,
        access: &PciBarAccess,
        _context: &mut dyn DeviceContext,
    ) -> DeviceResult<u64> {
        let range = Self::access_range(access)?;
        let bytes = self.bytes.lock().map_err(|_| DeviceError::Internal)?;
        let mut value = [0u8; 8];
        value[..range.len()].copy_from_slice(&bytes[range]);
        Ok(u64::from_le_bytes(value))
    }

    fn write_bar(
        &self,
        access: &PciBarAccess,
        value: u64,
        _context: &mut dyn DeviceContext,
    ) -> DeviceResult {
        let range = Self::access_range(access)?;
        let width = range.len();
        let mut bytes = self.bytes.lock().map_err(|_| DeviceError::Internal)?;
        bytes[range].copy_from_slice(&value.to_le_bytes()[..width]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use axdevice_base::{BusKind, DeviceAccess, DeviceVcpuId};

    use super::*;
    use crate::{ConfiguredDeviceCatalog, configured::ConfiguredDeviceAttachment};

    #[test]
    fn built_in_ivshmem_pci_registers_and_routes_bar2_memory() {
        let request = VirtualDeviceRequest {
            id: "ivshmem0".into(),
            model: "ivshmem-pci".into(),
            options: Default::default(),
        };
        let mut catalog = ConfiguredDeviceCatalog::new();
        register(&mut catalog).unwrap();
        let attachment = catalog
            .instantiate(&request, &DeviceInstantiationContext::new())
            .unwrap();
        let ConfiguredDeviceAttachment::Pci(endpoint) = attachment else {
            panic!("ivshmem-pci must retain its PCI attachment");
        };
        let (function, model) = endpoint.into_parts();
        let endpoint_id = function.id().clone();
        let host_id = DeviceNodeId::new("pci-host").unwrap();
        let host_resources = PciHostResourceRequirements::new(0x0400_0000, 0x0400_0000).unwrap();
        let mut pci = PciBusGraphBuilder::new(host_id.clone(), host_resources);
        let mut graph = DeviceGraphBuilder::new();
        graph.add(pci.host_node()).unwrap();
        graph
            .add(pci.endpoint_node(function, model).unwrap())
            .unwrap();
        let mut pools = ResourcePools::new();
        pools.add_auto_mmio(0x0b00_0000..0x1000_0000).unwrap();
        let graph = graph.declare().unwrap().resolve(pools).unwrap();
        let pci = pci.resolve(&graph).unwrap();
        let mut runtime_builder = DeviceRuntimeBuilder::new(RuntimeAccessPorts::new());
        for node in graph.nodes() {
            runtime_builder
                .build_graph_node(node, graph.resource_plan())
                .unwrap();
        }
        let runtime = runtime_builder.finish(graph.resource_plan()).unwrap();
        let ecam_base = pci.ecam_window().start;
        let function = pci.topology().function(&endpoint_id).unwrap();
        let bdf = function.bdf();
        let bar = function
            .bar(PciBarIndex::new(SHARED_MEMORY_BAR_INDEX).unwrap())
            .unwrap();

        assert_eq!(read_config(&runtime, ecam_base, bdf, 0), 0x1110_1af4);
        write_config(&runtime, ecam_base, bdf, 4, 2);
        let bar_access = mmio_access(bar.address() + 0x20, AccessWidth::Qword);
        assert!(
            runtime
                .try_write(&bar_access, 0x4956_5348_4d45_4d31, None)
                .unwrap()
        );
        assert_eq!(
            runtime.try_read(&bar_access).unwrap().unwrap(),
            0x4956_5348_4d45_4d31,
        );
    }

    fn mmio_access(address: u64, width: AccessWidth) -> DeviceAccess {
        DeviceAccess::new(DeviceVcpuId::new(0), BusKind::Mmio, address, width)
    }

    fn read_config(runtime: &DeviceRuntime, ecam_base: u64, bdf: PciBdf, offset: u16) -> u64 {
        runtime
            .try_read(&mmio_access(
                ecam_base + bdf.ecam_offset() + u64::from(offset),
                AccessWidth::Dword,
            ))
            .unwrap()
            .unwrap()
    }

    fn write_config(runtime: &DeviceRuntime, ecam_base: u64, bdf: PciBdf, offset: u16, value: u64) {
        assert!(
            runtime
                .try_write(
                    &mmio_access(
                        ecam_base + bdf.ecam_offset() + u64::from(offset),
                        AccessWidth::Dword,
                    ),
                    value,
                    None,
                )
                .unwrap()
        );
    }
}
