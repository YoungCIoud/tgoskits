//! Type-0 config-space image, capability chain, and write moderation.

use alloc::{
    boxed::Box,
    string::ToString,
    sync::{Arc, Weak},
    vec::Vec,
};

use super::{
    PciBdf, PciEndpointIdentity, PciError, PciFunction, PciMemoryBarWidth, PciResult,
    address::CONFIG_SPACE_SIZE,
    bar::{BarState, ResolvedBarPlan},
};
use crate::DeviceNodeId;

const LEGACY_CONFIG_END: usize = 0x100;
const CAPABILITY_START: usize = 0x40;
const COMMAND_MEMORY_ENABLE: u8 = 0x02;
const COMMAND_BUS_MASTER_ENABLE: u8 = 0x04;
/// PCI Interrupt Disable is command bit 10: bit 2 of the high byte of the
/// little-endian 16-bit command field at offset 4.
const COMMAND_INTX_DISABLE_HIGH: u8 = 0x04;
const STATUS_CAPABILITY_LIST: u8 = 0x10;

/// Standard PCI command state owned by the root config image.
///
/// Endpoints observe transitions through the root's out-of-lock effect
/// dispatch instead of owning their own header copy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PciCommandState {
    pub(crate) memory_space_enabled: bool,
    pub(crate) bus_master_enabled: bool,
    pub(crate) intx_disabled: bool,
}

impl PciCommandState {
    /// Reads the modeled command bits from one config image.
    pub(crate) fn from_config(config: &[u8; CONFIG_SPACE_SIZE]) -> Self {
        Self {
            memory_space_enabled: config[4] & COMMAND_MEMORY_ENABLE != 0,
            bus_master_enabled: config[4] & COMMAND_BUS_MASTER_ENABLE != 0,
            intx_disabled: config[5] & COMMAND_INTX_DISABLE_HIGH != 0,
        }
    }
}

/// One conventional PCI capability body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PciCapability {
    id: u8,
    body: Vec<u8>,
    write_mask: Vec<u8>,
}

impl PciCapability {
    /// Creates a capability whose selected body bits are guest writable.
    ///
    /// `body` excludes the capability ID and next-pointer bytes. `write_mask`
    /// has one byte per body byte; set bits are writable.
    ///
    /// # Errors
    ///
    /// Returns [`PciError::InvalidCapability`] for a reserved ID, mismatched
    /// mask, or a capability too large for conventional config space.
    pub fn new(
        id: u8,
        body: impl Into<Vec<u8>>,
        write_mask: impl Into<Vec<u8>>,
    ) -> PciResult<Self> {
        let body = body.into();
        let write_mask = write_mask.into();
        validate_capability(id, &body, &write_mask)?;
        Ok(Self {
            id,
            body,
            write_mask,
        })
    }

    /// Creates a read-only capability.
    ///
    /// # Errors
    ///
    /// Returns [`PciError::InvalidCapability`] under the same conditions as
    /// [`PciCapability::new`].
    pub fn read_only(id: u8, body: impl Into<Vec<u8>>) -> PciResult<Self> {
        let body = body.into();
        let write_mask = alloc::vec![0; body.len()];
        Self::new(id, body, write_mask)
    }

    /// Returns the standard capability ID.
    pub const fn id(&self) -> u8 {
        self.id
    }

    pub(crate) fn encoded_len(&self) -> usize {
        self.body.len() + 2
    }
}

/// Computes the DWORD-aligned offset of each capability in the conventional
/// chain that starts at 0x40.
///
/// This single layout rule is shared by declaration-time validation
/// ([`PciFunctionSpec::with_capability`](super::PciFunctionSpec::with_capability))
/// and image building so the two phases cannot disagree about whether a
/// chain fits below 0x100.
pub(crate) fn capability_chain_offsets<'a>(
    capabilities: impl IntoIterator<Item = &'a PciCapability>,
) -> PciResult<Vec<usize>> {
    let mut offsets = Vec::new();
    let mut next = CAPABILITY_START;
    for capability in capabilities {
        offsets.push(next);
        next = align_up(next + capability.encoded_len(), 4);
        if next > LEGACY_CONFIG_END {
            return Err(PciError::InvalidCapability {
                id: capability.id,
                detail: "capability chain exceeds offset 0xff".into(),
            });
        }
    }
    Ok(offsets)
}

#[derive(Clone)]
pub(crate) struct PowerOnConfig {
    bytes: Box<[u8; CONFIG_SPACE_SIZE]>,
    write_mask: Box<[u8; CONFIG_SPACE_SIZE]>,
}

impl PowerOnConfig {
    pub(crate) fn build(
        identity: PciEndpointIdentity,
        bars: &[ResolvedBarPlan],
        capabilities: &[PciCapability],
        multifunction: bool,
    ) -> PciResult<Self> {
        if identity.vendor_id() == u16::MAX {
            return Err(PciError::InvalidEndpointIdentity {
                detail: "vendor ID 0xffff denotes an absent function",
            });
        }
        let mut bytes = Box::new([0; CONFIG_SPACE_SIZE]);
        let mut write_mask = Box::new([0; CONFIG_SPACE_SIZE]);
        bytes[0..2].copy_from_slice(&identity.vendor_id().to_le_bytes());
        bytes[2..4].copy_from_slice(&identity.device_id().to_le_bytes());
        write_mask[4] = COMMAND_MEMORY_ENABLE | COMMAND_BUS_MASTER_ENABLE;
        write_mask[5] = COMMAND_INTX_DISABLE_HIGH;
        let class = identity.class();
        bytes[8] = identity.revision();
        bytes[9] = class.programming_interface();
        bytes[10] = class.subclass();
        bytes[11] = class.base();
        bytes[14] = if multifunction { 0x80 } else { 0 };
        bytes[0x2c..0x2e].copy_from_slice(&identity.subsystem_vendor_id().to_le_bytes());
        bytes[0x2e..0x30].copy_from_slice(&identity.subsystem_id().to_le_bytes());
        write_initial_bars(&mut bytes, bars);
        write_capabilities(&mut bytes, &mut write_mask, capabilities)?;
        Ok(Self { bytes, write_mask })
    }
}

pub(crate) struct FunctionState {
    id: DeviceNodeId,
    bdf: PciBdf,
    binding: Option<FunctionBinding>,
    power_on: PowerOnConfig,
    config: Box<[u8; CONFIG_SPACE_SIZE]>,
    bars: Vec<BarState>,
}

struct FunctionBinding {
    generation: u64,
    handler: Weak<dyn PciFunction>,
}

pub(crate) enum BarWriteAction {
    Probe {
        bar: usize,
        high: bool,
    },
    Relocate {
        bar: usize,
        high: bool,
        candidate: u64,
    },
}

impl FunctionState {
    pub(crate) fn new(
        id: DeviceNodeId,
        bdf: PciBdf,
        power_on: PowerOnConfig,
        bars: &[ResolvedBarPlan],
    ) -> Self {
        Self {
            id,
            bdf,
            binding: None,
            config: power_on.bytes.clone(),
            power_on,
            bars: bars.iter().copied().map(BarState::new).collect(),
        }
    }

    pub(crate) const fn id(&self) -> &DeviceNodeId {
        &self.id
    }

    pub(crate) const fn bdf(&self) -> PciBdf {
        self.bdf
    }

    pub(crate) fn bind_handler(
        &mut self,
        generation: u64,
        handler: &Arc<dyn PciFunction>,
    ) -> PciResult {
        if self.binding.is_some() {
            return Err(PciError::FunctionAlreadyBound {
                function: self.id.to_string(),
            });
        }
        self.binding = Some(FunctionBinding {
            generation,
            handler: Arc::downgrade(handler),
        });
        Ok(())
    }

    pub(crate) fn unbind_handler(&mut self, generation: u64) {
        if self
            .binding
            .as_ref()
            .is_some_and(|binding| binding.generation == generation)
        {
            self.binding = None;
        }
    }

    pub(crate) fn handler(&self) -> Option<Arc<dyn PciFunction>> {
        self.binding.as_ref()?.handler.upgrade()
    }

    pub(crate) fn memory_decode_enabled(&self) -> bool {
        self.config[4] & COMMAND_MEMORY_ENABLE != 0
    }

    /// Returns the standard command state of this function's config image.
    pub(crate) fn command_state(&self) -> PciCommandState {
        PciCommandState::from_config(&self.config)
    }

    pub(crate) fn bars(&self) -> &[BarState] {
        &self.bars
    }

    pub(crate) fn read(&self, offset: usize, size: usize) -> u64 {
        if let Some((bar, high)) = self.bar_dword(offset) {
            let dword = self.bars[bar].raw_dword(high).to_le_bytes();
            return read_bytes(&dword, offset % 4, size);
        }
        read_bytes(self.config.as_slice(), offset, size)
    }

    pub(crate) fn prepare_bar_write(
        &self,
        offset: usize,
        size: usize,
        value: u64,
    ) -> Option<BarWriteAction> {
        let (bar, high) = self.bar_dword(offset)?;
        // Classify only after merging the write into the full BAR dword, so a
        // partial write that completes the all-ones sizing mask is treated
        // exactly like a whole-dword probe.
        let mut dword = self.bars[bar].committed_dword(high).to_le_bytes();
        merge_bytes(&mut dword, offset % 4, size, value, &[u8::MAX; 4]);
        let merged = u32::from_le_bytes(dword);
        if merged == u32::MAX {
            return Some(BarWriteAction::Probe { bar, high });
        }
        Some(BarWriteAction::Relocate {
            bar,
            high,
            candidate: self.bars[bar].candidate_address(high, merged),
        })
    }

    pub(crate) fn write_non_bar(&mut self, offset: usize, size: usize, value: u64) {
        merge_bytes(
            self.config.as_mut_slice(),
            offset,
            size,
            value,
            self.power_on.write_mask.as_slice(),
        );
    }

    pub(crate) fn apply_probe(&mut self, bar: usize, high: bool) {
        self.bars[bar].set_probe(high);
    }

    pub(crate) fn finish_relocation(&mut self, bar: usize, high: bool, accepted: Option<u64>) {
        self.bars[bar].clear_probe(high);
        if let Some(address) = accepted {
            self.bars[bar].commit_address(address);
        }
    }

    pub(crate) fn reset(&mut self) {
        self.config.clone_from(&self.power_on.bytes);
        for bar in &mut self.bars {
            bar.reset();
        }
    }

    fn bar_dword(&self, offset: usize) -> Option<(usize, bool)> {
        if !(0x10..0x28).contains(&offset) {
            return None;
        }
        let slot = ((offset - 0x10) / 4) as u8;
        self.bars.iter().enumerate().find_map(|(index, bar)| {
            if slot == bar.index().value() {
                Some((index, false))
            } else if bar.width() == PciMemoryBarWidth::Bits64 && slot == bar.index().value() + 1 {
                Some((index, true))
            } else {
                None
            }
        })
    }
}

fn validate_capability(id: u8, body: &[u8], write_mask: &[u8]) -> PciResult {
    if id == 0 || id == u8::MAX {
        return Err(PciError::InvalidCapability {
            id,
            detail: "capability ID is reserved".into(),
        });
    }
    if body.len() != write_mask.len() {
        return Err(PciError::InvalidCapability {
            id,
            detail: "body and write mask lengths differ".into(),
        });
    }
    if body.len() + 2 > LEGACY_CONFIG_END - CAPABILITY_START {
        return Err(PciError::InvalidCapability {
            id,
            detail: "capability cannot fit in conventional config space".into(),
        });
    }
    Ok(())
}

fn write_initial_bars(bytes: &mut [u8; CONFIG_SPACE_SIZE], bars: &[ResolvedBarPlan]) {
    for bar in bars {
        let offset = usize::from(bar.index.config_offset());
        let low = (bar.address as u32 & 0xffff_fff0)
            | super::bar::bar_attributes(bar.width, bar.prefetchable);
        bytes[offset..offset + 4].copy_from_slice(&low.to_le_bytes());
        if bar.width == PciMemoryBarWidth::Bits64 {
            bytes[offset + 4..offset + 8]
                .copy_from_slice(&((bar.address >> 32) as u32).to_le_bytes());
        }
    }
}

fn write_capabilities(
    bytes: &mut [u8; CONFIG_SPACE_SIZE],
    write_mask: &mut [u8; CONFIG_SPACE_SIZE],
    capabilities: &[PciCapability],
) -> PciResult {
    if capabilities.is_empty() {
        return Ok(());
    }
    let offsets = capability_chain_offsets(capabilities)?;
    bytes[6] |= STATUS_CAPABILITY_LIST;
    bytes[0x34] = CAPABILITY_START as u8;
    for (index, capability) in capabilities.iter().enumerate() {
        let offset = offsets[index];
        bytes[offset] = capability.id;
        bytes[offset + 1] = offsets.get(index + 1).copied().unwrap_or(0) as u8;
        let body_start = offset + 2;
        let body_end = body_start + capability.body.len();
        bytes[body_start..body_end].copy_from_slice(&capability.body);
        write_mask[body_start..body_end].copy_from_slice(&capability.write_mask);
    }
    Ok(())
}

fn align_up(value: usize, alignment: usize) -> usize {
    (value + alignment - 1) & !(alignment - 1)
}

fn read_bytes(bytes: &[u8], offset: usize, size: usize) -> u64 {
    bytes[offset..offset + size]
        .iter()
        .enumerate()
        .fold(0, |value, (index, byte)| {
            value | (u64::from(*byte) << (index * 8))
        })
}

fn merge_bytes(bytes: &mut [u8], offset: usize, size: usize, value: u64, masks: &[u8]) {
    for index in 0..size {
        let mask = masks[offset + index];
        let update = (value >> (index * 8)) as u8;
        bytes[offset + index] = (bytes[offset + index] & !mask) | (update & mask);
    }
}
