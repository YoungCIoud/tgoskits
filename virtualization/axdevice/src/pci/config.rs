//! Conventional Type-0 config image and guest-writable root state.

use alloc::vec::Vec;

use super::{
    PciBdf, PciCapabilityEffectRegion, PciCapabilityId, PciCapabilityLayout, PciCapabilitySnapshot,
    PciEndpointIdentity, PciResult,
    address::CONFIG_SPACE_SIZE,
    bar::{BarState, ResolvedBarPlan},
    function::PciConfigByte,
    runtime::PciCommandState,
};

const COMMAND_BUS_MASTER_ENABLE: u8 = 0x04;
const COMMAND_INTERRUPT_DISABLE: u8 = 0x04;
const COMMAND_MEMORY_SPACE_ENABLE: u8 = 0x02;
const STATUS_CAPABILITIES_LIST: u8 = 0x10;

#[derive(Clone)]
pub(crate) struct PowerOnConfig {
    bytes: [u8; CONFIG_SPACE_SIZE],
    write_mask: [u8; CONFIG_SPACE_SIZE],
    capabilities: Vec<PciCapabilityLayout>,
}

impl PowerOnConfig {
    pub(crate) fn build(
        identity: PciEndpointIdentity,
        bars: &[ResolvedBarPlan],
        config_bytes: &[PciConfigByte],
        capabilities: &[PciCapabilityLayout],
    ) -> PciResult<Self> {
        if identity.vendor_id() == u16::MAX {
            return Err(super::PciError::InvalidEndpointIdentity {
                detail: "vendor ID 0xffff denotes an absent function",
            });
        }
        let mut bytes = [0; CONFIG_SPACE_SIZE];
        let mut write_mask = [0; CONFIG_SPACE_SIZE];
        bytes[0..2].copy_from_slice(&identity.vendor_id().to_le_bytes());
        bytes[2..4].copy_from_slice(&identity.device_id().to_le_bytes());
        bytes[0x2c..0x2e].copy_from_slice(&identity.subsystem_vendor_id().to_le_bytes());
        bytes[0x2e..0x30].copy_from_slice(&identity.subsystem_device_id().to_le_bytes());
        write_mask[4] = COMMAND_MEMORY_SPACE_ENABLE | COMMAND_BUS_MASTER_ENABLE;
        write_mask[5] = COMMAND_INTERRUPT_DISABLE;
        let class = identity.class();
        bytes[8] = identity.revision();
        bytes[9] = class.programming_interface();
        bytes[10] = class.subclass();
        bytes[11] = class.base();
        bytes[14] = 0;
        for patch in config_bytes {
            let offset = usize::from(patch.offset.value());
            bytes[offset] = patch.value;
            write_mask[offset] = patch.write_mask;
        }
        for bar in bars {
            let offset = bar.index.config_offset();
            bytes[offset..offset + 4]
                .copy_from_slice(&(bar.address as u32 & 0xffff_fff0).to_le_bytes());
        }
        if let Some(first) = capabilities.first() {
            bytes[0x06] |= STATUS_CAPABILITIES_LIST;
            bytes[0x34] = first.offset().value() as u8;
        }
        for (index, capability) in capabilities.iter().enumerate() {
            let base = usize::from(capability.offset().value());
            bytes[base] = capability.id().value();
            bytes[base + 1] = capabilities
                .get(index + 1)
                .map_or(0, |next| next.offset().value() as u8);
            bytes[base + 2..base + usize::from(capability.length())]
                .copy_from_slice(capability.body());
            write_mask[base + 2..base + usize::from(capability.length())]
                .copy_from_slice(capability.write_mask());
        }
        Ok(Self {
            bytes,
            write_mask,
            capabilities: capabilities.to_vec(),
        })
    }
}

pub(crate) struct FunctionState {
    bdf: PciBdf,
    power_on: PowerOnConfig,
    config: [u8; CONFIG_SPACE_SIZE],
    bars: Vec<BarState>,
}

pub(crate) enum BarWriteAction {
    Probe { bar: usize },
    Relocate { bar: usize, candidate: u64 },
}

impl FunctionState {
    pub(crate) fn new(bdf: PciBdf, power_on: PowerOnConfig, bars: &[ResolvedBarPlan]) -> Self {
        Self {
            bdf,
            config: power_on.bytes,
            power_on,
            bars: bars.iter().copied().map(BarState::new).collect(),
        }
    }

    pub(crate) const fn bdf(&self) -> PciBdf {
        self.bdf
    }

    pub(crate) fn memory_decode_enabled(&self) -> bool {
        self.config[4] & COMMAND_MEMORY_SPACE_ENABLE != 0
    }

    pub(crate) fn command_state(&self) -> PciCommandState {
        PciCommandState::new(
            self.config[4] & COMMAND_MEMORY_SPACE_ENABLE != 0,
            self.config[4] & COMMAND_BUS_MASTER_ENABLE != 0,
            self.config[5] & COMMAND_INTERRUPT_DISABLE != 0,
        )
    }

    pub(crate) fn bars(&self) -> &[BarState] {
        &self.bars
    }

    pub(crate) fn read(&self, offset: usize, size: usize) -> u64 {
        if let Some(bar) = self.bar_dword(offset) {
            let dword = self.bars[bar].raw_dword().to_le_bytes();
            return read_bytes(&dword, offset % 4, size);
        }
        read_bytes(&self.config, offset, size)
    }

    pub(crate) fn config_effect(
        &self,
        offset: usize,
        size: usize,
        width: crate::AccessWidth,
        write: bool,
    ) -> PciResult<
        Option<(
            PciCapabilityId,
            PciCapabilityEffectRegion,
            u8,
            PciCapabilitySnapshot,
        )>,
    > {
        for capability in &self.power_on.capabilities {
            let Some(effect) = capability.effect_for_access(offset, size, write, width)? else {
                continue;
            };
            let relative = offset
                .checked_sub(usize::from(capability.offset().value()))
                .ok_or(super::PciError::InvalidConfigAccess {
                    offset: offset as u16,
                    width,
                    detail: "capability effect offset underflows",
                })?;
            return Ok(Some((
                capability.id(),
                effect,
                relative as u8,
                capability.snapshot(&self.config),
            )));
        }
        Ok(None)
    }

    pub(crate) fn intersects_config_effect(&self, offset: usize, size: usize) -> bool {
        self.power_on
            .capabilities
            .iter()
            .any(|capability| capability.intersects_effect(offset, size))
    }

    /// Classifies one BAR write after merging the guest lanes into a full
    /// dword. The size probe is recognized only when the merged dword equals
    /// all ones in one access; lane-wise accumulation across multiple writes
    /// is intentionally not tracked, matching the design's four-row contract
    /// rather than hardware register latching.
    pub(crate) fn prepare_bar_write(
        &self,
        offset: usize,
        size: usize,
        value: u64,
    ) -> Option<BarWriteAction> {
        let bar = self.bar_dword(offset)?;
        let mut dword = self.bars[bar].committed_dword().to_le_bytes();
        merge_bytes(&mut dword, offset % 4, size, value, &[u8::MAX; 4]);
        let merged = u32::from_le_bytes(dword);
        if merged == u32::MAX {
            return Some(BarWriteAction::Probe { bar });
        }
        Some(BarWriteAction::Relocate {
            bar,
            candidate: BarState::candidate_address(merged),
        })
    }

    pub(crate) fn write_non_bar(&mut self, offset: usize, size: usize, value: u64) {
        merge_bytes(
            &mut self.config,
            offset,
            size,
            value,
            &self.power_on.write_mask,
        );
    }

    pub(crate) fn apply_probe(&mut self, bar: usize) {
        self.bars[bar].set_probe();
    }

    pub(crate) fn finish_relocation(&mut self, bar: usize, accepted: Option<u64>) {
        self.bars[bar].finish_relocation(accepted);
    }

    pub(crate) fn reset(&mut self) {
        self.config = self.power_on.bytes;
        for bar in &mut self.bars {
            bar.reset();
        }
    }

    fn bar_dword(&self, offset: usize) -> Option<usize> {
        if !(0x10..0x28).contains(&offset) {
            return None;
        }
        let slot = ((offset - 0x10) / 4) as u8;
        self.bars.iter().position(|bar| slot == bar.index().value())
    }
}

pub(crate) fn read_bytes(bytes: &[u8], offset: usize, size: usize) -> u64 {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PciBarIndex, PciClass, PciMemoryBar};

    #[test]
    fn function_state_keeps_unimplemented_header_fields_read_only() {
        let identity = PciEndpointIdentity::new(0x1234, 0x5678, PciClass::new(0x05, 0x00, 0x00));
        let bar = PciMemoryBar::new(PciBarIndex::new(2).unwrap(), 0x1_0000).unwrap();
        let plan = ResolvedBarPlan {
            index: bar.index(),
            size: bar.size(),
            address: 0x2000_0000,
        };
        let power_on = PowerOnConfig::build(identity, &[plan], &[], &[]).unwrap();
        let mut state = FunctionState::new(PciBdf::bus_zero(1), power_on, &[plan]);

        state.write_non_bar(0, 4, 0);

        assert_eq!(state.read(0, 4), 0x5678_1234);
    }
}
