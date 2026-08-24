//! ECAM host frontend: window validation, address decoding, and delegation.
//!
//! The ECAM device owns and reports only its own 1 MiB window. It decodes an
//! access into a bus-0 function plus config offset, applies the ECAM-specific
//! width/alignment rules, and delegates the already-typed access to the
//! shared [`PciRootState`](super::root::PciRootState). Host window invariants
//! are checked once by [`validate_host_windows`] before any runtime device is
//! published.

use alloc::{boxed::Box, format};
use core::{fmt, ops::Range};

use axdevice_base::{Device, DeviceAccess, DeviceContext, DeviceError, DeviceResult, Resource};

use super::{
    FOUR_GIB, PciError, PciResult,
    address::decode_ecam_offset,
    root::{SharedPciRoot, contains_access, ensure_mmio_access},
};

pub(crate) const ECAM_SIZE: u64 = 1 << 20;
const ECAM_ALIGNMENT: u64 = ECAM_SIZE;

/// Validates one resolved ECAM window against one PCI memory aperture.
///
/// # Errors
///
/// Returns [`PciError::InvalidHostAperture`] when the ECAM window is not a
/// 1 MiB-aligned range of exactly 1 MiB, the aperture is empty or extends
/// above 4 GiB, or the two windows overlap.
pub fn validate_host_windows(ecam: Range<u64>, aperture: Range<u64>) -> PciResult {
    if ecam.start & (ECAM_ALIGNMENT - 1) != 0 {
        return Err(invalid_host("ECAM base is not 1 MiB aligned"));
    }
    // The device built from this window reports exactly ECAM_SIZE bytes, so a
    // caller-supplied range that disagrees with its own base would silently
    // describe different facts.
    if ecam.start.checked_add(ECAM_SIZE) != Some(ecam.end) {
        return Err(invalid_host("ECAM window must be exactly 1 MiB"));
    }
    if aperture.start >= aperture.end {
        return Err(invalid_host("memory aperture is empty"));
    }
    if aperture.end > FOUR_GIB {
        return Err(invalid_host("memory aperture extends above 4 GiB"));
    }
    if ecam.start < aperture.end && aperture.start < ecam.end {
        return Err(invalid_host("ECAM and memory aperture overlap"));
    }
    Ok(())
}

/// Segment-0, bus-0 virtual PCI ECAM config frontend registered as one
/// runtime device.
pub(crate) struct PciEcamDevice {
    window: Range<u64>,
    root: SharedPciRoot,
    resources: Box<[Resource]>,
}

impl fmt::Debug for PciEcamDevice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PciEcamDevice")
            .field("window", &self.window)
            .finish_non_exhaustive()
    }
}

impl PciEcamDevice {
    /// Creates the ECAM frontend for one resolved 1 MiB window.
    pub(crate) fn new(window: Range<u64>, root: SharedPciRoot) -> Self {
        let resources = alloc::vec![Resource::MmioRange {
            base: window.start,
            size: ECAM_SIZE,
        }]
        .into_boxed_slice();
        Self {
            window,
            root,
            resources,
        }
    }

    fn relative_offset(&self, access: &DeviceAccess) -> DeviceResult<u64> {
        access
            .address()
            .checked_sub(self.window.start)
            .ok_or(DeviceError::OutOfRange {
                addr: access.address(),
            })
    }

    fn read_config(&self, access: &DeviceAccess) -> DeviceResult<u64> {
        let (bdf, offset) = decode_ecam_offset(self.relative_offset(access)?);
        let size = offset
            .validate_access(access.width())
            .map_err(config_access_error)?;
        self.root
            .lock_irqsave()
            .read_config(bdf, usize::from(offset.value()), size)
            .map_err(config_access_error)
    }

    fn write_config(&self, access: &DeviceAccess, value: u64) -> DeviceResult {
        let (bdf, offset) = decode_ecam_offset(self.relative_offset(access)?);
        let size = offset
            .validate_access(access.width())
            .map_err(config_access_error)?;
        super::root::write_config(&self.root, bdf, usize::from(offset.value()), size, value)
            .map_err(config_access_error)
    }
}

impl Device for PciEcamDevice {
    fn name(&self) -> &str {
        "pci-ecam"
    }

    fn resources(&self) -> &[Resource] {
        &self.resources
    }

    fn read(&self, access: &DeviceAccess, _context: &mut dyn DeviceContext) -> DeviceResult<u64> {
        ensure_mmio_access(access)?;
        if contains_access(&self.window, access) {
            self.read_config(access)
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
        _context: &mut dyn DeviceContext,
    ) -> DeviceResult {
        ensure_mmio_access(access)?;
        if contains_access(&self.window, access) {
            self.write_config(access, value)
        } else {
            Err(DeviceError::OutOfRange {
                addr: access.address(),
            })
        }
    }
}

fn invalid_host(detail: &'static str) -> PciError {
    PciError::InvalidHostAperture { detail }
}

fn config_access_error(error: PciError) -> DeviceError {
    DeviceError::InvalidInput {
        operation: "access PCI configuration space",
        detail: format!("{error}"),
    }
}
