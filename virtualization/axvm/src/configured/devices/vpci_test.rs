//! Test-only ordinary model for the generic x86 PCI enumeration path.

use std::{
    sync::{Arc, Mutex},
    vec::Vec,
};

use axdevice::*;
use axdevice_base::{Device, DeviceAccess, DeviceContext, DeviceError, DeviceResult, Resource};
use axvmconfig::VirtualDeviceRequest;

use crate::{ConfiguredDeviceError, ConfiguredModelRegistration, DeviceInstantiationContext};

const MODEL: &str = "vpci-test";
const HOST_KEY: &str = "x86-q35";
const BAR_INDEX: u8 = 2;
const BAR_SIZE: usize = 0x1_0000;
const VENDOR_ID: u16 = 0x1af4;
const DEVICE_ID: u16 = 0x1110;

/// Catalog entry for the opt-in generic PCI enumeration fixture.
pub const REGISTRATION: ConfiguredModelRegistration = ConfiguredModelRegistration {
    model: MODEL,
    create: create_device_node,
};

pub(super) fn register(
    catalog: &mut crate::ConfiguredDeviceCatalog,
) -> Result<(), ConfiguredDeviceError> {
    catalog.register(module_path!(), REGISTRATION)
}

#[derive(Clone, Copy, Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct VpciTestOptions {}

fn create_device_node(
    id: DeviceNodeId,
    request: &VirtualDeviceRequest,
    _context: &DeviceInstantiationContext,
) -> Result<DeviceNodeSpec, ConfiguredDeviceError> {
    request
        .deserialize_options::<VpciTestOptions>()
        .map_err(|error| ConfiguredDeviceError::InvalidOptions {
            device: request.id.clone(),
            model: request.model.clone(),
            detail: error.to_string(),
        })?;
    Ok(DeviceNodeSpec::virtual_device(id, Arc::new(VpciTestModel)))
}

struct VpciTestModel;

impl DeviceModel for VpciTestModel {
    fn requirements(&self) -> DeviceManagerResult<DeviceRequirements> {
        let bar = PciMemoryBar::new(PciBarIndex::new(BAR_INDEX)?, BAR_SIZE as u64)?;
        let requirement = PciFunctionRequirement::new(
            PciHostKey::new(HOST_KEY)?,
            PciEndpointIdentity::new(VENDOR_ID, DEVICE_ID, PciClass::new(0x06, 0x00, 0x00)),
        )
        .with_bar(bar)?;
        DeviceRequirements::new().with_pci_function(requirement)
    }

    fn firmware(&self) -> DeviceFirmwareSpec {
        DeviceFirmwareSpec::None
    }

    fn build(&self, _context: &mut DeviceBuildContext<'_>) -> DeviceManagerResult<DeviceBundle> {
        let function = Arc::new(VpciTestFunction::new()?);
        let mut bundle = DeviceBundle::new();
        bundle.add_pci_function(function)?;
        Ok(bundle)
    }
}

struct VpciTestFunction {
    backing: Mutex<Vec<u8>>,
}

impl VpciTestFunction {
    fn new() -> DeviceManagerResult<Self> {
        let mut backing = Vec::new();
        backing
            .try_reserve_exact(BAR_SIZE)
            .map_err(|_| DeviceManagerError::OutOfMemory {
                operation: "allocate vpci-test BAR backing",
            })?;
        backing.resize(BAR_SIZE, 0);
        Ok(Self {
            backing: Mutex::new(backing),
        })
    }

    fn access_range(&self, access: PciBarAccess) -> DeviceResult<(usize, usize)> {
        let offset = usize::try_from(access.offset()).map_err(|_| DeviceError::OutOfRange {
            addr: access.offset(),
        })?;
        let size = access.width().size();
        let end = offset.checked_add(size).ok_or(DeviceError::OutOfRange {
            addr: access.offset(),
        })?;
        if end > BAR_SIZE {
            return Err(DeviceError::OutOfRange {
                addr: access.offset(),
            });
        }
        Ok((offset, size))
    }
}

impl Device for VpciTestFunction {
    fn name(&self) -> &str {
        MODEL
    }

    fn resources(&self) -> &[Resource] {
        &[]
    }

    fn read(&self, _access: &DeviceAccess, _context: &mut dyn DeviceContext) -> DeviceResult<u64> {
        Err(DeviceError::Unsupported {
            operation: "access vpci-test endpoint device",
            detail: "direct access is routed through the PCI BAR".into(),
        })
    }

    fn write(
        &self,
        _access: &DeviceAccess,
        _value: u64,
        _context: &mut dyn DeviceContext,
    ) -> DeviceResult {
        Err(DeviceError::Unsupported {
            operation: "access vpci-test endpoint device",
            detail: "direct access is routed through the PCI BAR".into(),
        })
    }
}

impl PciFunction for VpciTestFunction {
    fn read_bar(
        &self,
        access: PciBarAccess,
        _context: &mut dyn DeviceContext,
    ) -> DeviceResult<u64> {
        let (offset, size) = self.access_range(access)?;
        let backing = self.backing.lock().map_err(|_| DeviceError::InvalidState {
            operation: "read vpci-test BAR",
            detail: "BAR backing lock is poisoned".into(),
        })?;
        Ok(backing[offset..offset + size]
            .iter()
            .enumerate()
            .fold(0, |value, (index, byte)| {
                value | u64::from(*byte) << (index * 8)
            }))
    }

    fn write_bar(
        &self,
        access: PciBarAccess,
        value: u64,
        _context: &mut dyn DeviceContext,
    ) -> DeviceResult {
        let (offset, size) = self.access_range(access)?;
        let mut backing = self.backing.lock().map_err(|_| DeviceError::InvalidState {
            operation: "write vpci-test BAR",
            detail: "BAR backing lock is poisoned".into(),
        })?;
        let bytes = value.to_le_bytes();
        backing[offset..offset + size].copy_from_slice(&bytes[..size]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axdevice::{DeviceGraphBuilder, DeviceRuntimeBuilder, PciRootBindingKey, ResourcePools};
    use axdevice_base::AccessWidth;

    use super::*;

    const APERTURE_BASE: u64 = 0xc000_0000;
    const APERTURE_SIZE: u64 = 0x10_0000;

    fn id(value: &str) -> DeviceNodeId {
        DeviceNodeId::new(value).unwrap()
    }

    fn slot(value: &str) -> ResourceSlot {
        ResourceSlot::new(value).unwrap()
    }

    #[test]
    fn requirements_declare_only_the_test_pci_function() {
        let requirements = VpciTestModel.requirements().unwrap();
        let function = requirements.pci_function().unwrap();
        assert_eq!(function.host().as_str(), HOST_KEY);
        let expected = PciFunctionRequirement::new(
            PciHostKey::new(HOST_KEY).unwrap(),
            PciEndpointIdentity::new(VENDOR_ID, DEVICE_ID, PciClass::new(0x06, 0, 0)),
        )
        .with_bar(PciMemoryBar::new(PciBarIndex::new(BAR_INDEX).unwrap(), BAR_SIZE as u64).unwrap())
        .unwrap();
        assert_eq!(function, &expected);
    }

    #[test]
    fn backing_is_zeroed_per_endpoint() {
        let first = VpciTestFunction::new().unwrap();
        let second = VpciTestFunction::new().unwrap();
        first.backing.lock().unwrap()[0] = 0xa5;
        assert_eq!(second.backing.lock().unwrap()[0], 0);
    }

    struct HostModel {
        root: Arc<Mutex<Option<Arc<PciRootState>>>>,
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
            let _ = context.mmio("pci-memory")?;
            let topology =
                context
                    .pci_host_topology()
                    .cloned()
                    .ok_or(DeviceManagerError::InvalidState {
                        operation: "build vpci-test host",
                        detail: "test host topology was not resolved".into(),
                    })?;
            let root = Arc::new(PciRootState::new(topology));
            *self.root.lock().unwrap() = Some(root.clone());
            let binding = Arc::new(PciRootBinding::new(id("pci-host"), root));
            DeviceBundle::new().with_service::<PciRootBindingKey>(binding)
        }
    }

    #[test]
    fn graph_build_routes_private_bar_backing() {
        let root_slot = Arc::new(Mutex::new(None));
        let provider = PciHostProvider::new(
            PciHostKey::new(HOST_KEY).unwrap(),
            DeviceNodeSpec::virtual_device(
                id("pci-host"),
                Arc::new(HostModel {
                    root: root_slot.clone(),
                }),
            ),
            slot("pci-memory"),
        );
        let mut builder = DeviceGraphBuilder::new();
        builder.register_pci_host(provider).unwrap();
        builder
            .add(DeviceNodeSpec::virtual_device(
                id("vpci-test0"),
                Arc::new(VpciTestModel),
            ))
            .unwrap();
        let mut pools = ResourcePools::new();
        pools
            .add_auto_mmio(APERTURE_BASE..APERTURE_BASE + APERTURE_SIZE)
            .unwrap();
        let graph = builder.declare().unwrap().resolve(pools).unwrap();
        let mut runtime_builder = DeviceRuntimeBuilder::new(RuntimeAccessPorts::new());
        for node in graph.nodes() {
            runtime_builder
                .build_graph_node(node, graph.resource_plan())
                .unwrap();
        }
        let runtime = runtime_builder.finish(graph.resource_plan()).unwrap();
        let root = root_slot.lock().unwrap().clone().unwrap();
        let binding = runtime
            .services()
            .all::<PciRootBindingKey>()
            .into_iter()
            .next()
            .unwrap();
        let function = graph
            .pci_topology(&PciHostKey::new(HOST_KEY).unwrap())
            .unwrap()
            .function(&id("vpci-test0"))
            .unwrap();
        let bar = function.bar(PciBarIndex::new(BAR_INDEX).unwrap()).unwrap();
        root.write_config(
            function.bdf(),
            ConfigOffset::new(4).unwrap(),
            AccessWidth::Word,
            2,
        )
        .unwrap();
        assert_eq!(
            binding.read_bar(bar.address(), AccessWidth::Dword).unwrap(),
            0
        );
        binding
            .write_bar(bar.address() + 4, AccessWidth::Dword, 0x1122_3344)
            .unwrap();
        assert_eq!(
            binding
                .read_bar(bar.address() + 4, AccessWidth::Dword)
                .unwrap(),
            0x1122_3344
        );
        assert_eq!(
            binding.read_bar(bar.address() + BAR_SIZE as u64, AccessWidth::Byte),
            Err(DeviceError::NotFound)
        );
    }
}
