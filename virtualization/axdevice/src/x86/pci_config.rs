//! x86 PCI configuration-mechanism #1 and memory-aperture adapters.

use alloc::{boxed::Box, sync::Arc};

use ax_sync::SpinLock;
use axdevice_base::*;

use crate::{
    ConfigOffset, DeviceLifecycle, DeviceManagerResult, PciBdf, PciRootBinding, PciRootState,
    PciSegment,
};

const CONFIG_ADDRESS_ENABLE: u32 = 1 << 31;
const CONFIG_ADDRESS_PORT: u16 = 0xcf8;
const CONFIG_DATA_PORT: u16 = 0xcfc;

/// CF8/CFC frontend that only decodes x86 port accesses.
pub struct X86PciConfigFrontend {
    address: SpinLock<u32>,
    root: Arc<PciRootState>,
    resources: Box<[Resource]>,
}

impl X86PciConfigFrontend {
    /// Base of the PCI configuration address/data port window.
    pub const PORT_BASE: u16 = CONFIG_ADDRESS_PORT;
    /// Size of the combined address/data port window.
    pub const PORT_SIZE: u16 = 8;
    /// Creates a frontend for one generic PCI root.
    pub fn new(root: Arc<PciRootState>) -> Self {
        Self {
            address: SpinLock::new(0),
            root,
            resources: alloc::vec![Resource::PortRange {
                base: Self::PORT_BASE,
                size: Self::PORT_SIZE
            }]
            .into_boxed_slice(),
        }
    }

    fn decode_access(access: &DeviceAccess) -> DeviceResult<(u16, usize)> {
        if access.bus() != BusKind::Port {
            return Err(DeviceError::OutOfRange {
                addr: access.address(),
            });
        }
        if access.width() == AccessWidth::Qword {
            return Err(DeviceError::Unsupported {
                operation: "access x86 PCI configuration port",
                detail: "CF8/CFC supports byte, word, and dword accesses only".into(),
            });
        }
        let port = u16::try_from(access.address()).map_err(|_| DeviceError::OutOfRange {
            addr: access.address(),
        })?;
        Ok((port, access.width().size()))
    }

    fn selection(&self, data_offset: usize, size: usize) -> Option<(PciBdf, ConfigOffset)> {
        let address = *self.address.lock_irqsave();
        if address & CONFIG_ADDRESS_ENABLE == 0 || data_offset.checked_add(size)? > 4 {
            return None;
        }
        let register = (address as usize & 0xfc).checked_add(data_offset)?;
        let bdf = PciBdf::new(
            PciSegment::new(0),
            (address >> 16) as u8,
            ((address >> 11) & 0x1f) as u8,
            ((address >> 8) & 0x7) as u8,
        )
        .ok()?;
        Some((bdf, ConfigOffset::new(u16::try_from(register).ok()?).ok()?))
    }
}

impl Device for X86PciConfigFrontend {
    fn name(&self) -> &str {
        "x86-pci-config"
    }
    fn resources(&self) -> &[Resource] {
        &self.resources
    }
    fn read(&self, access: &DeviceAccess, _context: &mut dyn DeviceContext) -> DeviceResult<u64> {
        let (port, size) = Self::decode_access(access)?;
        if (CONFIG_ADDRESS_PORT..CONFIG_DATA_PORT).contains(&port) {
            let offset = usize::from(port - CONFIG_ADDRESS_PORT);
            if offset + size > 4 {
                return Err(DeviceError::OutOfRange {
                    addr: access.address(),
                });
            }
            return Ok(read_bytes(
                &self.address.lock_irqsave().to_le_bytes(),
                offset,
                size,
            ));
        }
        if (CONFIG_DATA_PORT..CONFIG_DATA_PORT + 4).contains(&port) {
            let offset = usize::from(port - CONFIG_DATA_PORT);
            let Some((bdf, register)) = self.selection(offset, size) else {
                return Ok(all_ones(size));
            };
            return self
                .root
                .read_config(bdf, register, access.width())
                .map_err(pci_access_error);
        }
        Err(DeviceError::OutOfRange {
            addr: access.address(),
        })
    }
    fn write(
        &self,
        access: &DeviceAccess,
        value: u64,
        _context: &mut dyn DeviceContext,
    ) -> DeviceResult {
        let (port, size) = Self::decode_access(access)?;
        if (CONFIG_ADDRESS_PORT..CONFIG_DATA_PORT).contains(&port) {
            let offset = usize::from(port - CONFIG_ADDRESS_PORT);
            if offset + size > 4 {
                return Err(DeviceError::OutOfRange {
                    addr: access.address(),
                });
            }
            let mut address = self.address.lock_irqsave();
            let mut bytes = address.to_le_bytes();
            write_bytes(&mut bytes, offset, size, value);
            *address = u32::from_le_bytes(bytes);
            return Ok(());
        }
        if (CONFIG_DATA_PORT..CONFIG_DATA_PORT + 4).contains(&port) {
            let offset = usize::from(port - CONFIG_DATA_PORT);
            if let Some((bdf, register)) = self.selection(offset, size) {
                self.root
                    .write_config(bdf, register, access.width(), value)
                    .map_err(pci_access_error)?;
            }
            return Ok(());
        }
        Err(DeviceError::OutOfRange {
            addr: access.address(),
        })
    }
}

/// Single top-level MMIO device owning a PCI root's complete memory aperture.
pub struct PciMemoryApertureDevice {
    binding: Arc<PciRootBinding>,
    resources: Box<[Resource]>,
}
impl PciMemoryApertureDevice {
    /// Creates the aperture adapter from the graph-resolved range.
    pub fn new(base: u64, size: u64, binding: Arc<PciRootBinding>) -> Self {
        Self {
            binding,
            resources: alloc::vec![Resource::MmioRange { base, size }].into_boxed_slice(),
        }
    }
}
impl Device for PciMemoryApertureDevice {
    fn name(&self) -> &str {
        "pci-memory-aperture"
    }
    fn resources(&self) -> &[Resource] {
        &self.resources
    }
    fn read(&self, access: &DeviceAccess, _context: &mut dyn DeviceContext) -> DeviceResult<u64> {
        if access.bus() != BusKind::Mmio {
            return Err(DeviceError::OutOfRange {
                addr: access.address(),
            });
        }
        match self.binding.read_bar(access.address(), access.width()) {
            Err(DeviceError::NotFound) => Ok(all_ones(access.width().size())),
            result => result,
        }
    }
    fn write(
        &self,
        access: &DeviceAccess,
        value: u64,
        _context: &mut dyn DeviceContext,
    ) -> DeviceResult {
        if access.bus() != BusKind::Mmio {
            return Err(DeviceError::OutOfRange {
                addr: access.address(),
            });
        }
        match self
            .binding
            .write_bar(access.address(), access.width(), value)
        {
            Err(DeviceError::NotFound) => Ok(()),
            result => result,
        }
    }
}

/// Lifecycle adapter restoring only root-owned PCI config and BAR state.
pub struct PciRootLifecycle(Arc<PciRootState>);
impl PciRootLifecycle {
    /// Creates a lifecycle adapter for one generic PCI root.
    pub const fn new(root: Arc<PciRootState>) -> Self {
        Self(root)
    }
}
impl DeviceLifecycle for PciRootLifecycle {
    fn reset(&self) -> DeviceManagerResult {
        self.0.reset();
        Ok(())
    }
    fn suspend(&self) -> DeviceManagerResult {
        Ok(())
    }
    fn resume(&self) -> DeviceManagerResult {
        Ok(())
    }
}

fn pci_access_error(error: crate::PciError) -> DeviceError {
    DeviceError::InvalidInput {
        operation: "access x86 PCI configuration",
        detail: alloc::format!("{error}"),
    }
}
fn read_bytes(bytes: &[u8], offset: usize, size: usize) -> u64 {
    bytes[offset..offset + size]
        .iter()
        .enumerate()
        .fold(0, |value, (index, byte)| {
            value | (u64::from(*byte) << (index * 8))
        })
}
fn write_bytes(bytes: &mut [u8], offset: usize, size: usize, value: u64) {
    for (index, byte) in bytes[offset..offset + size].iter_mut().enumerate() {
        *byte = (value >> (index * 8)) as u8;
    }
}
fn all_ones(size: usize) -> u64 {
    u64::MAX >> ((8 - size) * 8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        PciClass, PciEndpointIdentity, PciFunctionSpec, PciTopologyBuilder, ResourceRequest,
    };

    fn bdf(device: u8) -> PciBdf {
        PciBdf::new(PciSegment::new(0), 0, device, 0).unwrap()
    }

    fn frontend() -> X86PciConfigFrontend {
        let mut topology = PciTopologyBuilder::new();
        let host = PciFunctionSpec::new(
            crate::DeviceNodeId::new("host").unwrap(),
            PciEndpointIdentity::new(0x8086, 0x29c0, PciClass::new(0x06, 0, 0)),
        )
        .with_bdf(ResourceRequest::Fixed(bdf(0)));
        let lpc = PciFunctionSpec::new(
            crate::DeviceNodeId::new("lpc").unwrap(),
            PciEndpointIdentity::new(0x8086, 0x2918, PciClass::new(0x06, 1, 0)),
        )
        .with_bdf(ResourceRequest::Fixed(bdf(0x1f)))
        .with_platform_config_byte(ConfigOffset::new(0x0e).unwrap(), 0x80, 0)
        .unwrap()
        .with_platform_config_byte(ConfigOffset::new(0x40).unwrap(), 1, 0x80)
        .unwrap()
        .with_platform_config_byte(ConfigOffset::new(0x41).unwrap(), 0, 0xff)
        .unwrap()
        .with_platform_config_byte(ConfigOffset::new(0x44).unwrap(), 0, 0x87)
        .unwrap();
        topology.add_function(host).unwrap();
        topology.add_function(lpc).unwrap();
        X86PciConfigFrontend::new(Arc::new(PciRootState::new(Arc::new(
            topology.resolve(0xc000_0000..0xd000_0000).unwrap(),
        ))))
    }

    fn access(port: u16, width: AccessWidth) -> DeviceAccess {
        DeviceAccess::new(DeviceVcpuId::new(0), BusKind::Port, u64::from(port), width)
    }

    fn write(frontend: &X86PciConfigFrontend, port: u16, width: AccessWidth, value: u64) {
        frontend
            .write(
                &access(port, width),
                value,
                &mut NoopDeviceContext::new(DeviceId::new(0)),
            )
            .unwrap();
    }

    fn read(frontend: &X86PciConfigFrontend, port: u16, width: AccessWidth) -> u64 {
        frontend
            .read(
                &access(port, width),
                &mut NoopDeviceContext::new(DeviceId::new(0)),
            )
            .unwrap()
    }

    #[test]
    fn generic_root_preserves_q35_identity_and_lpc_pm_fields() {
        let frontend = frontend();
        write(
            &frontend,
            CONFIG_ADDRESS_PORT,
            AccessWidth::Dword,
            0x8000_0000,
        );
        assert_eq!(
            read(&frontend, CONFIG_DATA_PORT, AccessWidth::Dword),
            0x29c0_8086
        );

        write(
            &frontend,
            CONFIG_ADDRESS_PORT,
            AccessWidth::Dword,
            0x8000_f840,
        );
        write(&frontend, CONFIG_DATA_PORT, AccessWidth::Dword, 0x601);
        assert_eq!(read(&frontend, CONFIG_DATA_PORT, AccessWidth::Dword), 0x601);

        write(
            &frontend,
            CONFIG_ADDRESS_PORT,
            AccessWidth::Dword,
            0x8000_f844,
        );
        write(&frontend, CONFIG_DATA_PORT, AccessWidth::Byte, 0x80);
        assert_eq!(read(&frontend, CONFIG_DATA_PORT, AccessWidth::Byte), 0x80);

        write(
            &frontend,
            CONFIG_ADDRESS_PORT,
            AccessWidth::Dword,
            0x8000_0800,
        );
        assert_eq!(
            read(&frontend, CONFIG_DATA_PORT, AccessWidth::Dword),
            u32::MAX.into()
        );
    }
}
