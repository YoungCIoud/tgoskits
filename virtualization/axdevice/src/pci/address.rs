//! Typed PCI function, BAR, and config-space addresses.

use core::fmt;

use super::{PciError, PciResult};
use crate::AccessWidth;

/// Size of one PCIe function's config space.
pub(crate) const CONFIG_SPACE_SIZE: usize = 0x1000;
const MAX_DEVICE: u8 = 31;
const MAX_FUNCTION: u8 = 7;

/// One PCI segment number.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct PciSegment(u16);

impl PciSegment {
    /// Creates a segment number.
    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    /// Returns the numeric segment.
    pub const fn value(self) -> u16 {
        self.0
    }
}

/// A validated PCI segment:bus:device.function address.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PciBdf {
    segment: PciSegment,
    bus: u8,
    device: u8,
    function: u8,
}

impl PciBdf {
    /// Creates a BDF after validating device and function fields.
    ///
    /// # Errors
    ///
    /// Returns [`PciError::InvalidAddress`] when `device >= 32` or
    /// `function >= 8`.
    pub fn new(segment: PciSegment, bus: u8, device: u8, function: u8) -> PciResult<Self> {
        if device > MAX_DEVICE {
            return Err(PciError::InvalidAddress {
                component: "device",
                value: u64::from(device),
            });
        }
        if function > MAX_FUNCTION {
            return Err(PciError::InvalidAddress {
                component: "function",
                value: u64::from(function),
            });
        }
        Ok(Self {
            segment,
            bus,
            device,
            function,
        })
    }

    /// Returns the segment.
    pub const fn segment(self) -> PciSegment {
        self.segment
    }

    /// Returns the bus number.
    pub const fn bus(self) -> u8 {
        self.bus
    }

    /// Returns the device number.
    pub const fn device(self) -> u8 {
        self.device
    }

    /// Returns the function number.
    pub const fn function(self) -> u8 {
        self.function
    }

    /// Returns this function's offset in a conventional ECAM window.
    pub const fn ecam_offset(self) -> u64 {
        ((self.bus as u64) << 20) | ((self.device as u64) << 15) | ((self.function as u64) << 12)
    }

    pub(crate) const fn bus_zero(devfn: u16) -> Self {
        Self {
            segment: PciSegment::new(0),
            bus: 0,
            device: ((devfn >> 3) & 0x1f) as u8,
            function: (devfn & 0x7) as u8,
        }
    }
}

impl fmt::Display for PciBdf {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:04x}:{:02x}:{:02x}.{}",
            self.segment.value(),
            self.bus,
            self.device,
            self.function
        )
    }
}

/// A validated Type-0 BAR index.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PciBarIndex(u8);

impl PciBarIndex {
    /// Creates a BAR index in `0..=5`.
    ///
    /// # Errors
    ///
    /// Returns [`PciError::InvalidAddress`] for an index above BAR5.
    pub fn new(value: u8) -> PciResult<Self> {
        if value >= 6 {
            return Err(PciError::InvalidAddress {
                component: "BAR index",
                value: u64::from(value),
            });
        }
        Ok(Self(value))
    }

    /// Returns the numeric BAR index.
    pub const fn value(self) -> u8 {
        self.0
    }

    pub(crate) const fn config_offset(self) -> u16 {
        0x10 + self.0 as u16 * 4
    }
}

impl fmt::Display for PciBarIndex {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// A byte offset in one 4 KiB PCIe function config space.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ConfigOffset(u16);

impl ConfigOffset {
    /// Creates a config-space offset below 4 KiB.
    ///
    /// # Errors
    ///
    /// Returns [`PciError::InvalidAddress`] for offsets outside one function.
    pub fn new(value: u16) -> PciResult<Self> {
        if usize::from(value) >= CONFIG_SPACE_SIZE {
            return Err(PciError::InvalidAddress {
                component: "config offset",
                value: u64::from(value),
            });
        }
        Ok(Self(value))
    }

    /// Returns the numeric byte offset.
    pub const fn value(self) -> u16 {
        self.0
    }

    pub(crate) fn validate_access(self, width: AccessWidth) -> PciResult<usize> {
        let size = match width {
            AccessWidth::Byte | AccessWidth::Word | AccessWidth::Dword => width.size(),
            AccessWidth::Qword => {
                return Err(PciError::InvalidConfigAccess {
                    offset: self.0,
                    width,
                    detail: "config accesses are limited to 32 bits",
                });
            }
        };
        if usize::from(self.0) % size != 0 {
            return Err(PciError::InvalidConfigAccess {
                offset: self.0,
                width,
                detail: "access is not naturally aligned",
            });
        }
        let end = usize::from(self.0) + size;
        if end > CONFIG_SPACE_SIZE {
            return Err(PciError::InvalidConfigAccess {
                offset: self.0,
                width,
                detail: "access crosses the function config-space boundary",
            });
        }
        let first_dword = usize::from(self.0) / 4;
        let last_dword = (end - 1) / 4;
        if first_dword != last_dword {
            return Err(PciError::InvalidConfigAccess {
                offset: self.0,
                width,
                detail: "access crosses a config DWORD boundary",
            });
        }
        Ok(size)
    }
}

/// Splits a relative conventional-ECAM window offset into the addressed bus-0
/// function and its in-function config offset.
///
/// This is the inverse of [`PciBdf::ecam_offset`]: the bus/device/function
/// fields live above the 4 KiB per-function config page, so the low 12 bits
/// always form a valid [`ConfigOffset`].
pub(crate) const fn decode_ecam_offset(relative: u64) -> (PciBdf, ConfigOffset) {
    let bdf = PciBdf::bus_zero(((relative >> 12) & 0xff) as u16);
    let offset = ConfigOffset((relative & 0xfff) as u16);
    (bdf, offset)
}
