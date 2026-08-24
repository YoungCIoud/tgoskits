//! Memory BAR descriptors and mutable decode state.
//!
//! Two deliberate differences from designs that store sizing responses in
//! the config image (recorded for the x86 migration):
//!
//! * Attribute bits are re-derived from the resolved plan on every read
//!   instead of being stored from guest writes, so no access width can flip
//!   them.
//! * A rejected relocation write still clears that dword's probe latch, so
//!   the readback returns to the committed address; it does not keep showing
//!   a stale size mask. Likewise, a non-canonical probe that writes
//!   `~(size - 1)` with cleared attribute bits classifies as a relocation
//!   candidate under [`PciBarDecodePolicy::Fixed`] and is rejected —
//!   compliant drivers write all-ones dwords.

use core::ops::Range;

use super::{PciBarIndex, PciError, PciResult};
use crate::ResourceRequest;

/// Width encoded by a non-prefetchable memory BAR.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PciMemoryBarWidth {
    /// One 32-bit BAR register.
    Bits32,
    /// A low/high pair of BAR registers.
    Bits64,
}

/// Runtime decode policy of one memory BAR, independent of its initial
/// placement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PciBarDecodePolicy {
    /// The planner address is permanent: guest writes never move the decode.
    ///
    /// Sizing probes still report the size mask and rewriting the planned
    /// address itself stays accepted, so compliant drivers observe normal
    /// behavior while stray relocations are ignored with a diagnostic.
    Fixed,
    /// The BAR may relocate inside the host memory aperture at runtime.
    RelocatableWithinHostAperture,
}

/// One non-prefetchable memory BAR requested by a virtual PCI function.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PciMemoryBar {
    index: PciBarIndex,
    size: u64,
    width: PciMemoryBarWidth,
    prefetchable: bool,
    decode_policy: PciBarDecodePolicy,
    address: ResourceRequest<u64>,
}

impl PciMemoryBar {
    /// Creates an automatically placed memory BAR.
    ///
    /// The BAR defaults to non-prefetchable with a relocatable decode; use
    /// [`PciMemoryBar::prefetchable`] and [`PciMemoryBar::with_decode_policy`]
    /// to model the other contracts.
    ///
    /// # Errors
    ///
    /// Returns [`PciError::InvalidBar`] if the size is below 16 bytes, is not
    /// a power of two, cannot be represented by the selected width, or a
    /// 64-bit BAR starts at BAR5.
    pub fn new(index: PciBarIndex, size: u64, width: PciMemoryBarWidth) -> PciResult<Self> {
        if size < 16 || !size.is_power_of_two() {
            return Err(invalid_bar(
                index,
                "size must be a power of two of at least 16 bytes",
            ));
        }
        if width == PciMemoryBarWidth::Bits32 && size > (u64::from(u32::MAX) + 1) {
            return Err(invalid_bar(index, "32-bit BAR size exceeds 4 GiB"));
        }
        if width == PciMemoryBarWidth::Bits64 && index.value() == 5 {
            return Err(invalid_bar(
                index,
                "64-bit BAR requires a following BAR slot",
            ));
        }
        Ok(Self {
            index,
            size,
            width,
            prefetchable: false,
            decode_policy: PciBarDecodePolicy::RelocatableWithinHostAperture,
            address: ResourceRequest::Auto,
        })
    }

    /// Marks the BAR prefetchable (config attribute bit 3).
    pub const fn prefetchable(mut self) -> Self {
        self.prefetchable = true;
        self
    }

    /// Selects the runtime decode policy, independent of the initial
    /// placement requested through [`PciMemoryBar::with_address`].
    pub const fn with_decode_policy(mut self, decode_policy: PciBarDecodePolicy) -> Self {
        self.decode_policy = decode_policy;
        self
    }

    /// Selects automatic or fixed initial placement inside the host aperture.
    pub fn with_address(mut self, address: ResourceRequest<u64>) -> Self {
        self.address = address;
        self
    }

    /// Returns the first BAR slot.
    pub const fn index(&self) -> PciBarIndex {
        self.index
    }

    /// Returns the fixed BAR size.
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// Returns the encoded BAR width.
    pub const fn width(&self) -> PciMemoryBarWidth {
        self.width
    }

    /// Returns whether the BAR declares the prefetchable attribute.
    pub const fn is_prefetchable(&self) -> bool {
        self.prefetchable
    }

    /// Returns the runtime decode policy.
    pub const fn decode_policy(&self) -> PciBarDecodePolicy {
        self.decode_policy
    }

    pub(crate) const fn address_request(&self) -> ResourceRequest<u64> {
        self.address
    }

    pub(crate) const fn occupied_slots(&self) -> u8 {
        match self.width {
            PciMemoryBarWidth::Bits32 => 1,
            PciMemoryBarWidth::Bits64 => 2,
        }
    }
}

/// Encodes the immutable attribute bits of one memory BAR dword.
pub(crate) const fn bar_attributes(width: PciMemoryBarWidth, prefetchable: bool) -> u32 {
    let width_bits = match width {
        PciMemoryBarWidth::Bits32 => 0,
        PciMemoryBarWidth::Bits64 => 0x4,
    };
    if prefetchable {
        width_bits | 0x8
    } else {
        width_bits
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResolvedBarPlan {
    pub(crate) index: PciBarIndex,
    pub(crate) size: u64,
    pub(crate) width: PciMemoryBarWidth,
    pub(crate) prefetchable: bool,
    pub(crate) policy: PciBarDecodePolicy,
    pub(crate) address: u64,
}

pub(crate) struct BarState {
    plan: ResolvedBarPlan,
    address: u64,
    probe_low: bool,
    probe_high: bool,
}

impl BarState {
    pub(crate) const fn new(plan: ResolvedBarPlan) -> Self {
        Self {
            plan,
            address: plan.address,
            probe_low: false,
            probe_high: false,
        }
    }

    pub(crate) const fn index(&self) -> PciBarIndex {
        self.plan.index
    }

    pub(crate) const fn size(&self) -> u64 {
        self.plan.size
    }

    pub(crate) const fn width(&self) -> PciMemoryBarWidth {
        self.plan.width
    }

    pub(crate) fn range(&self) -> Option<Range<u64>> {
        Some(self.address..self.address.checked_add(self.plan.size)?)
    }

    pub(crate) const fn raw_dword(&self, high: bool) -> u32 {
        if high && self.probe_high {
            self.mask_high()
        } else if !high && self.probe_low {
            self.mask_low()
        } else {
            self.committed_dword(high)
        }
    }

    pub(crate) const fn committed_dword(&self, high: bool) -> u32 {
        if high {
            (self.address >> 32) as u32
        } else {
            (self.address as u32 & 0xffff_fff0) | self.attributes()
        }
    }

    pub(crate) fn set_probe(&mut self, high: bool) {
        if high {
            self.probe_high = true;
        } else {
            self.probe_low = true;
        }
    }

    pub(crate) fn clear_probe(&mut self, high: bool) {
        if high {
            self.probe_high = false;
        } else {
            self.probe_low = false;
        }
    }

    pub(crate) fn candidate_address(&self, high: bool, dword: u32) -> u64 {
        let candidate = if high {
            (self.address & u64::from(u32::MAX)) | (u64::from(dword) << 32)
        } else {
            (self.address & !u64::from(u32::MAX)) | u64::from(dword & 0xffff_fff0)
        };
        candidate & !(self.plan.size - 1)
    }

    pub(crate) fn commit_address(&mut self, address: u64) {
        self.address = address;
    }

    pub(crate) fn reset(&mut self) {
        self.address = self.plan.address;
        self.probe_low = false;
        self.probe_high = false;
    }

    pub(crate) const fn attributes(&self) -> u32 {
        bar_attributes(self.plan.width, self.plan.prefetchable)
    }

    pub(crate) const fn decode_policy(&self) -> PciBarDecodePolicy {
        self.plan.policy
    }

    /// Returns the planner-owned base this BAR resets and decodes to.
    pub(crate) const fn planned_address(&self) -> u64 {
        self.plan.address
    }

    const fn mask_low(&self) -> u32 {
        (!(self.plan.size - 1) as u32 & 0xffff_fff0) | self.attributes()
    }

    const fn mask_high(&self) -> u32 {
        match self.plan.width {
            PciMemoryBarWidth::Bits32 => 0,
            PciMemoryBarWidth::Bits64 => ((!(self.plan.size - 1)) >> 32) as u32,
        }
    }
}

fn invalid_bar(index: PciBarIndex, detail: &str) -> PciError {
    PciError::InvalidBar {
        bar: index,
        detail: detail.into(),
    }
}
