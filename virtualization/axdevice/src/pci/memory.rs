//! PCI memory aperture device delegating BAR accesses to the root state.
//!
//! The device owns and reports the whole CPU-visible PCI memory aperture
//! resource. It holds no config state of its own: every access is resolved
//! against the shared [`PciRootState`](super::root::PciRootState), and the
//! endpoint handler callback runs outside the root lock. This device is also
//! the single lifecycle owner, so a runtime reset restores the root exactly
//! once even though the ECAM frontend shares the same state.

use alloc::boxed::Box;
use core::ops::Range;

use axdevice_base::{Device, DeviceAccess, DeviceContext, DeviceError, DeviceResult, Resource};

use super::root::{SharedPciRoot, contains_access, ensure_mmio_access};
use crate::{DeviceLifecycle, DeviceManagerResult};

pub(crate) struct PciMemoryApertureDevice {
    aperture: Range<u64>,
    root: SharedPciRoot,
    resources: Box<[Resource]>,
}

impl PciMemoryApertureDevice {
    /// Creates the aperture frontend for one resolved PCI memory window.
    pub(crate) fn new(aperture: Range<u64>, root: SharedPciRoot) -> Self {
        let resources = alloc::vec![Resource::MmioRange {
            base: aperture.start,
            size: aperture.end - aperture.start,
        }]
        .into_boxed_slice();
        Self {
            aperture,
            root,
            resources,
        }
    }

    fn read_bar(
        &self,
        access: &DeviceAccess,
        context: &mut dyn DeviceContext,
    ) -> DeviceResult<u64> {
        let route = self
            .root
            .lock_irqsave()
            .resolve_bar(access)
            .ok_or(DeviceError::NotFound)?;
        route.handler.read_bar(&route.access, context)
    }

    fn write_bar(
        &self,
        access: &DeviceAccess,
        value: u64,
        context: &mut dyn DeviceContext,
    ) -> DeviceResult {
        let route = self
            .root
            .lock_irqsave()
            .resolve_bar(access)
            .ok_or(DeviceError::NotFound)?;
        route.handler.write_bar(&route.access, value, context)
    }
}

impl core::fmt::Debug for PciMemoryApertureDevice {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("PciMemoryApertureDevice")
            .field("aperture", &self.aperture)
            .finish_non_exhaustive()
    }
}

impl Device for PciMemoryApertureDevice {
    fn name(&self) -> &str {
        "pci-memory-aperture"
    }

    fn resources(&self) -> &[Resource] {
        &self.resources
    }

    fn read(&self, access: &DeviceAccess, context: &mut dyn DeviceContext) -> DeviceResult<u64> {
        ensure_mmio_access(access)?;
        if contains_access(&self.aperture, access) {
            self.read_bar(access, context)
        } else {
            Err(DeviceError::OutOfRange {
                addr: access.address(),
            })
        }
    }

    fn write(
        &self,
        access: &DeviceAccess,
        value: u64,
        context: &mut dyn DeviceContext,
    ) -> DeviceResult {
        ensure_mmio_access(access)?;
        if contains_access(&self.aperture, access) {
            self.write_bar(access, value, context)
        } else {
            Err(DeviceError::OutOfRange {
                addr: access.address(),
            })
        }
    }
}

impl DeviceLifecycle for PciMemoryApertureDevice {
    fn reset(&self) -> DeviceManagerResult {
        self.root.lock_irqsave().reset();
        Ok(())
    }

    fn suspend(&self) -> DeviceManagerResult {
        Ok(())
    }

    fn resume(&self) -> DeviceManagerResult {
        Ok(())
    }
}
