//! Architecture-neutral PCI root-owned config and BAR decode state.
//!
//! Frontends pass already-decoded BDF/config accesses into this object. The
//! root owns only conventional config bytes and BAR routes; endpoint objects,
//! runtime identities, and lifecycle callbacks are introduced by later
//! integration layers.

use alloc::{boxed::Box, collections::BTreeMap, string::ToString, sync::Arc, vec::Vec};
use core::{fmt, ops::Range};

use ax_sync::SpinLock;
use axdevice_base::DeviceId;

use super::{
    EndpointRouteToken, FOUR_GIB, PciBarIndex, PciBdf, PciCommandState, PciConfigReadEffect,
    PciConfigWriteEffect, PciError, PciResult, ResolvedPciTopology,
    config::{BarWriteAction, FunctionState},
};
use crate::{AccessWidth, ConfigOffset};

pub(crate) enum PciConfigReadOutcome {
    Value(u64),
    Effect {
        token: EndpointRouteToken,
        command: PciCommandState,
        effect: Box<PciConfigReadEffect>,
    },
}

pub(crate) enum PciConfigWriteOutcome {
    Complete,
    Effect {
        token: EndpointRouteToken,
        command: PciCommandState,
        effect: Box<PciConfigWriteEffect>,
    },
    CommandChanged {
        token: Option<EndpointRouteToken>,
        command: PciCommandState,
    },
}

/// Shared root state for one frozen PCI topology.
pub struct PciRootState {
    topology: Arc<ResolvedPciTopology>,
    state: SpinLock<RootState>,
}

impl PciRootState {
    /// Creates power-on config and BAR decode state from a frozen topology.
    pub fn new(topology: Arc<ResolvedPciTopology>) -> Self {
        let functions = topology
            .function_plans()
            .iter()
            .map(|function| {
                FunctionState::new(function.bdf(), function.power_on.clone(), function.bars())
            })
            .collect();
        Self {
            state: SpinLock::new(RootState {
                functions,
                bindings: BTreeMap::new(),
            }),
            topology,
        }
    }

    /// Returns the immutable topology that produced this root state.
    pub fn topology(&self) -> &ResolvedPciTopology {
        &self.topology
    }

    pub(crate) fn topology_arc(&self) -> &Arc<ResolvedPciTopology> {
        &self.topology
    }

    /// Reads one conventional config access.
    ///
    /// An absent BDF reads as all ones for the requested width.
    ///
    /// # Errors
    ///
    /// Returns [`PciError::InvalidConfigAccess`] for qword, misaligned, or
    /// out-of-range accesses.
    pub fn read_config(
        &self,
        bdf: PciBdf,
        offset: ConfigOffset,
        width: AccessWidth,
    ) -> PciResult<u64> {
        match self.prepare_read_config(bdf, offset, width)? {
            PciConfigReadOutcome::Value(value) => Ok(value),
            PciConfigReadOutcome::Effect { .. } => Err(PciError::ConfigEffectUnavailable {
                detail: "an endpoint binding is required for this config read",
            }),
        }
    }

    /// Applies one conventional config write.
    ///
    /// Writes to absent functions or read-only fields have no effect. BAR
    /// probe and relocation writes are classified after merging the complete
    /// dword; invalid relocations preserve both config readback and decode.
    ///
    /// # Errors
    ///
    /// Returns [`PciError::InvalidConfigAccess`] under the same conditions as
    /// [`PciRootState::read_config`].
    pub fn write_config(
        &self,
        bdf: PciBdf,
        offset: ConfigOffset,
        width: AccessWidth,
        value: u64,
    ) -> PciResult {
        match self.prepare_write_config(bdf, offset, width, value)? {
            PciConfigWriteOutcome::Complete | PciConfigWriteOutcome::CommandChanged { .. } => {
                Ok(())
            }
            PciConfigWriteOutcome::Effect { .. } => Err(PciError::ConfigEffectUnavailable {
                detail: "an endpoint binding is required for this config write",
            }),
        }
    }

    pub(crate) fn prepare_read_config(
        &self,
        bdf: PciBdf,
        offset: ConfigOffset,
        width: AccessWidth,
    ) -> PciResult<PciConfigReadOutcome> {
        let (offset, size) = offset.validate_access(width)?;
        let (token, command, capability, effect, relative, snapshot) = {
            let state = self.state.lock_irqsave();
            let Some(function_index) = state.function_index(bdf) else {
                return Ok(PciConfigReadOutcome::Value(all_ones(size)));
            };
            let function = &state.functions[function_index];
            let Some((capability, effect, relative, snapshot)) =
                function.config_effect(offset, size, width, false)?
            else {
                return Ok(PciConfigReadOutcome::Value(function.read(offset, size)));
            };
            let token = state
                .bindings
                .get(&bdf)
                .and_then(EndpointRouteToken::snapshot_if_admitted)
                .ok_or(PciError::ConfigEffectUnavailable {
                    detail: "an admitted endpoint binding is required for this config read",
                })?;
            (
                token,
                function.command_state(),
                capability,
                effect,
                relative,
                snapshot,
            )
        };
        Ok(PciConfigReadOutcome::Effect {
            token,
            command,
            effect: Box::new(PciConfigReadEffect::new(
                capability,
                effect.effect(),
                relative,
                width,
                snapshot,
            )),
        })
    }

    pub(crate) fn config_access_intersects_effect(
        &self,
        bdf: PciBdf,
        offset: ConfigOffset,
        width: AccessWidth,
    ) -> PciResult<bool> {
        let size = width.size();
        let offset = usize::from(offset.value());
        let end = offset
            .checked_add(size)
            .ok_or(PciError::InvalidConfigAccess {
                offset: offset as u16,
                width,
                detail: "config access range overflows",
            })?;
        if end > 0x100 {
            return Err(PciError::InvalidConfigAccess {
                offset: offset as u16,
                width,
                detail: "config access leaves the function boundary",
            });
        }
        let state = self.state.lock_irqsave();
        Ok(state
            .function_index(bdf)
            .is_some_and(|index| state.functions[index].intersects_config_effect(offset, size)))
    }

    pub(crate) fn prepare_write_config(
        &self,
        bdf: PciBdf,
        offset: ConfigOffset,
        width: AccessWidth,
        value: u64,
    ) -> PciResult<PciConfigWriteOutcome> {
        let (offset, size) = offset.validate_access(width)?;
        let (token, command, capability, effect, relative, snapshot) = {
            let mut state = self.state.lock_irqsave();
            let Some(function_index) = state.function_index(bdf) else {
                return Ok(PciConfigWriteOutcome::Complete);
            };
            if let Some((capability, effect, relative, snapshot)) =
                state.functions[function_index].config_effect(offset, size, width, true)?
            {
                let token = state
                    .bindings
                    .get(&bdf)
                    .and_then(EndpointRouteToken::snapshot_if_admitted)
                    .ok_or(PciError::ConfigEffectUnavailable {
                        detail: "an admitted endpoint binding is required for this config write",
                    })?;
                (
                    token,
                    state.functions[function_index].command_state(),
                    capability,
                    effect,
                    relative,
                    snapshot,
                )
            } else {
                let bar_action =
                    state.functions[function_index].prepare_bar_write(offset, size, value);
                if let Some(action) = bar_action {
                    match action {
                        BarWriteAction::Probe { bar } => {
                            state.functions[function_index].apply_probe(bar)
                        }
                        BarWriteAction::Relocate { bar, candidate } => {
                            let accepted = state.bar_address_available(
                                self.topology.memory_aperture(),
                                function_index,
                                bar,
                                candidate,
                            );
                            state.functions[function_index]
                                .finish_relocation(bar, accepted.then_some(candidate));
                        }
                    }
                    return Ok(PciConfigWriteOutcome::Complete);
                }
                let previous = state.functions[function_index].command_state();
                state.functions[function_index].write_non_bar(offset, size, value);
                let command = state.functions[function_index].command_state();
                let command_changed = previous.bus_master_enable() != command.bus_master_enable()
                    || previous.interrupt_disable() != command.interrupt_disable();
                if command_changed {
                    return Ok(PciConfigWriteOutcome::CommandChanged {
                        token: state
                            .bindings
                            .get(&bdf)
                            .and_then(EndpointRouteToken::snapshot_if_admitted),
                        command,
                    });
                }
                return Ok(PciConfigWriteOutcome::Complete);
            }
        };
        Ok(PciConfigWriteOutcome::Effect {
            token,
            command,
            effect: Box::new(PciConfigWriteEffect::new(
                PciConfigReadEffect::new(capability, effect.effect(), relative, width, snapshot),
                value,
            )),
        })
    }

    /// Resolves one complete memory access against the current enabled BARs.
    ///
    /// Returns `None` for overflow, disabled decode, or an unmapped address.
    pub fn resolve_bar(&self, address: u64, width: AccessWidth) -> Option<PciBarRoute> {
        let access_end = address.checked_add(width.size() as u64)?;
        let state = self.state.lock_irqsave();
        resolve_route(&state.functions, address, access_end, width).map(|(_, route)| route)
    }

    pub(crate) fn resolve_bound_bar(
        &self,
        address: u64,
        width: AccessWidth,
    ) -> Option<(EndpointRouteToken, PciBarRoute, PciCommandState)> {
        let access_end = address.checked_add(width.size() as u64)?;
        let state = self.state.lock_irqsave();
        let (bdf, route) = resolve_route(&state.functions, address, access_end, width)?;
        let function = state
            .functions
            .iter()
            .find(|function| function.bdf() == bdf)?;
        Some((
            state.bindings.get(&bdf)?.snapshot_if_admitted()?,
            route,
            function.command_state(),
        ))
    }

    pub(crate) fn bind_endpoint(
        &self,
        function_id: &crate::DeviceNodeId,
        token: EndpointRouteToken,
    ) -> PciResult {
        let function =
            self.topology
                .function(function_id)
                .ok_or_else(|| PciError::UnknownFunction {
                    function: function_id.to_string(),
                })?;
        let mut state = self.state.lock_irqsave();
        if state.bindings.contains_key(&function.bdf()) {
            return Err(PciError::FunctionAlreadyBound {
                function: function_id.to_string(),
            });
        }
        state.bindings.insert(function.bdf(), token);
        Ok(())
    }

    pub(crate) fn replace_endpoint_tokens(
        &self,
        replacements: &[(EndpointRouteToken, EndpointRouteToken)],
    ) {
        let mut state = self.state.lock_irqsave();
        for token in state.bindings.values_mut() {
            if let Some((_, replacement)) = replacements.iter().find(|(old, _)| old == token) {
                *token = replacement.clone();
            }
        }
    }

    pub(crate) fn unbind_device(&self, device: DeviceId) {
        self.state
            .lock_irqsave()
            .bindings
            .retain(|_, token| token.device_id() != device);
    }

    /// Restores every function's root-owned power-on config and BAR route.
    pub fn reset(&self) {
        for function in &mut self.state.lock_irqsave().functions {
            function.reset();
        }
    }

    /// Resets root-owned state and snapshots the fresh command state for all
    /// currently bound endpoint device identities.
    pub(crate) fn reset_and_snapshot_commands(&self) -> Vec<(DeviceId, PciCommandState)> {
        let mut state = self.state.lock_irqsave();
        for function in &mut state.functions {
            function.reset();
        }
        state
            .bindings
            .iter()
            .filter_map(|(bdf, token)| {
                state
                    .functions
                    .iter()
                    .find(|function| function.bdf() == *bdf)
                    .map(|function| (token.device_id(), function.command_state()))
            })
            .collect()
    }
}

fn resolve_route(
    functions: &[FunctionState],
    address: u64,
    access_end: u64,
    width: AccessWidth,
) -> Option<(PciBdf, PciBarRoute)> {
    for function in functions {
        if !function.memory_decode_enabled() {
            continue;
        }
        for bar in function.bars() {
            let Some(range) = bar.range() else { continue };
            if range.start <= address && access_end <= range.end {
                let bdf = function.bdf();
                return Some((
                    bdf,
                    PciBarRoute {
                        bdf,
                        bar: bar.index(),
                        offset: address - range.start,
                        width,
                    },
                ));
            }
        }
    }
    None
}

impl fmt::Debug for PciRootState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PciRootState")
            .field("topology", &self.topology)
            .finish_non_exhaustive()
    }
}

struct RootState {
    functions: alloc::vec::Vec<FunctionState>,
    bindings: BTreeMap<PciBdf, EndpointRouteToken>,
}

impl RootState {
    fn function_index(&self, bdf: PciBdf) -> Option<usize> {
        self.functions
            .binary_search_by_key(&bdf, FunctionState::bdf)
            .ok()
    }

    fn bar_address_available(
        &self,
        memory_aperture: &Range<u64>,
        owner: usize,
        owner_bar: usize,
        address: u64,
    ) -> bool {
        let bar = &self.functions[owner].bars()[owner_bar];
        let Some(end) = address.checked_add(bar.size()) else {
            return false;
        };
        if address & (bar.size() - 1) != 0
            || address < memory_aperture.start
            || end > memory_aperture.end
            || end > FOUR_GIB
        {
            return false;
        }
        !self
            .functions
            .iter()
            .enumerate()
            .any(|(function_index, function)| {
                function
                    .bars()
                    .iter()
                    .enumerate()
                    .any(|(bar_index, existing)| {
                        if function_index == owner && bar_index == owner_bar {
                            return false;
                        }
                        existing
                            .range()
                            .is_some_and(|range| address < range.end && range.start < end)
                    })
            })
    }
}

/// One current BAR route resolved without entering an endpoint callback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciBarRoute {
    bdf: PciBdf,
    bar: PciBarIndex,
    offset: u64,
    width: AccessWidth,
}

impl PciBarRoute {
    /// Returns the selected function.
    pub const fn bdf(self) -> PciBdf {
        self.bdf
    }

    /// Returns the selected BAR.
    pub const fn bar(self) -> PciBarIndex {
        self.bar
    }

    /// Returns the function-relative BAR offset.
    pub const fn offset(self) -> u64 {
        self.offset
    }

    /// Returns the complete access width.
    pub const fn width(self) -> AccessWidth {
        self.width
    }
}

pub(crate) fn all_ones(size: usize) -> u64 {
    u64::MAX >> ((8 - size) * 8)
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;

    use super::*;
    use crate::{
        ConfigOffset, DeviceNodeId, PciCapabilityId, PciCapabilitySpec, PciClass,
        PciEndpointIdentity, PciFunctionSpec, PciMemoryBar, PciSegment, PciTopologyBuilder,
        ResourceRequest,
    };

    const APERTURE_START: u64 = 0x2000_0000;
    const APERTURE_END: u64 = 0x2040_0000;
    const BAR_SIZE: u64 = 0x1_0000;

    #[test]
    fn exposes_a_256_byte_type_zero_config_image() {
        let (root, endpoint_bdf, _) = root_with_bar();

        assert_eq!(
            root.read_config(endpoint_bdf, offset(0), AccessWidth::Dword)
                .unwrap(),
            0x5678_1234
        );
        assert_eq!(
            root.read_config(endpoint_bdf, offset(8), AccessWidth::Dword)
                .unwrap(),
            0x0500_0001
        );
        assert_eq!(
            root.read_config(endpoint_bdf, offset(0x0e), AccessWidth::Byte)
                .unwrap(),
            0
        );
        assert!(matches!(
            ConfigOffset::new(0x100),
            Err(PciError::InvalidAddress { .. })
        ));
    }

    #[test]
    fn absent_functions_read_all_ones_and_ignore_writes() {
        let (root, ..) = root_with_bar();
        let absent = bdf(30, 0);

        assert_eq!(
            root.read_config(absent, offset(0), AccessWidth::Word)
                .unwrap(),
            0xffff
        );
        root.write_config(absent, offset(4), AccessWidth::Word, 0xffff)
            .unwrap();
        assert_eq!(
            root.read_config(absent, offset(4), AccessWidth::Word)
                .unwrap(),
            0xffff
        );
    }

    #[test]
    fn rejects_misaligned_and_qword_config_accesses() {
        let (root, endpoint_bdf, _) = root_with_bar();

        assert!(matches!(
            root.read_config(endpoint_bdf, offset(1), AccessWidth::Word),
            Err(PciError::InvalidConfigAccess { .. })
        ));
        assert!(matches!(
            root.read_config(endpoint_bdf, offset(0), AccessWidth::Qword),
            Err(PciError::InvalidConfigAccess { .. })
        ));
    }

    #[test]
    fn serializes_capabilities_and_subsystem_ids_into_root_owned_config() {
        let first = PciCapabilitySpec::new(
            PciCapabilityId::new(1),
            alloc::vec![0xa1, 0xb2],
            alloc::vec![0, 0],
        )
        .unwrap();
        let second = PciCapabilitySpec::new(
            PciCapabilityId::new(2),
            alloc::vec![0, 0, 0x11, 0x22],
            alloc::vec![0, 0, 0xff, 0xff],
        )
        .unwrap();
        let endpoint = PciFunctionSpec::new(
            node("capability-endpoint"),
            PciEndpointIdentity::new(0x1234, 0x5678, PciClass::new(0x05, 0, 0))
                .with_subsystem_ids(0xabcd, 0x1234),
        )
        .with_capability(first)
        .with_capability(second);
        let mut builder = PciTopologyBuilder::new();
        builder.add_function(endpoint).unwrap();
        let topology = Arc::new(builder.resolve(APERTURE_START..APERTURE_END).unwrap());
        let resolved = topology.function(&node("capability-endpoint")).unwrap();
        let endpoint_bdf = resolved.bdf();
        let root = PciRootState::new(topology);

        assert_eq!(
            root.read_config(endpoint_bdf, offset(0x2c), AccessWidth::Dword)
                .unwrap(),
            0x1234_abcd
        );
        assert_eq!(
            root.read_config(endpoint_bdf, offset(0x06), AccessWidth::Byte)
                .unwrap(),
            0x10
        );
        assert_eq!(
            root.read_config(endpoint_bdf, offset(0x34), AccessWidth::Byte)
                .unwrap(),
            0x40
        );
        assert_eq!(
            root.read_config(endpoint_bdf, offset(0x40), AccessWidth::Dword)
                .unwrap(),
            0xb2a1_4401
        );
        assert_eq!(
            root.read_config(endpoint_bdf, offset(0x44), AccessWidth::Dword)
                .unwrap(),
            0x0000_0002
        );
        assert_eq!(
            root.read_config(endpoint_bdf, offset(0x48), AccessWidth::Word)
                .unwrap(),
            0x2211
        );

        root.write_config(endpoint_bdf, offset(0x2c), AccessWidth::Word, u64::MAX)
            .unwrap();
        root.write_config(endpoint_bdf, offset(0x40), AccessWidth::Byte, u64::MAX)
            .unwrap();
        root.write_config(endpoint_bdf, offset(0x48), AccessWidth::Word, 0x1234)
            .unwrap();
        assert_eq!(
            root.read_config(endpoint_bdf, offset(0x2c), AccessWidth::Dword)
                .unwrap(),
            0x1234_abcd
        );
        assert_eq!(
            root.read_config(endpoint_bdf, offset(0x40), AccessWidth::Byte)
                .unwrap(),
            1
        );
        assert_eq!(
            root.read_config(endpoint_bdf, offset(0x48), AccessWidth::Word)
                .unwrap(),
            0x1234
        );
    }

    #[test]
    fn platform_config_bytes_cannot_override_core_identity_or_bars() {
        let function = PciFunctionSpec::new(
            node("platform"),
            PciEndpointIdentity::new(0x1234, 0x5678, PciClass::new(0x06, 0, 0)),
        );
        assert!(matches!(
            function
                .clone()
                .with_platform_config_byte(ConfigOffset::new(0).unwrap(), 0, u8::MAX),
            Err(PciError::InvalidConfigPatch { offset: 0, .. })
        ));
        assert!(matches!(
            function.with_platform_config_byte(ConfigOffset::new(0x10).unwrap(), 0, u8::MAX),
            Err(PciError::InvalidConfigPatch { offset: 0x10, .. })
        ));
    }

    #[test]
    fn command_register_accepts_memory_space_bus_master_and_interrupt_disable() {
        let (root, endpoint_bdf, bar_base) = root_with_bar();

        assert!(root.resolve_bar(bar_base, AccessWidth::Dword).is_none());
        root.write_config(endpoint_bdf, offset(4), AccessWidth::Word, 0xffff)
            .unwrap();

        assert_eq!(
            root.read_config(endpoint_bdf, offset(4), AccessWidth::Word)
                .unwrap(),
            0x0406
        );
        let route = root.resolve_bar(bar_base + 8, AccessWidth::Dword).unwrap();
        assert_eq!(route.bdf(), endpoint_bdf);
        assert_eq!(route.bar(), PciBarIndex::new(2).unwrap());
        assert_eq!(route.offset(), 8);
        assert_eq!(route.width(), AccessWidth::Dword);
        assert!(
            root.resolve_bar(bar_base + BAR_SIZE - 2, AccessWidth::Dword)
                .is_none()
        );
    }

    #[test]
    fn bar_probe_reports_size_without_changing_the_runtime_route() {
        let (root, endpoint_bdf, bar_base) = enabled_root_with_bar();

        root.write_config(
            endpoint_bdf,
            offset(0x18),
            AccessWidth::Dword,
            u64::from(u32::MAX),
        )
        .unwrap();

        assert_eq!(
            root.read_config(endpoint_bdf, offset(0x18), AccessWidth::Dword)
                .unwrap(),
            0xffff_0000
        );
        assert!(root.resolve_bar(bar_base, AccessWidth::Byte).is_some());
    }

    #[test]
    fn valid_bar_relocation_moves_the_route() {
        let (root, endpoint_bdf, old_base) = enabled_root_with_bar();
        let new_base = APERTURE_START + 0x10_0000;

        root.write_config(endpoint_bdf, offset(0x18), AccessWidth::Dword, new_base)
            .unwrap();

        assert_eq!(
            root.read_config(endpoint_bdf, offset(0x18), AccessWidth::Dword)
                .unwrap(),
            new_base
        );
        assert!(root.resolve_bar(old_base, AccessWidth::Byte).is_none());
        assert!(root.resolve_bar(new_base, AccessWidth::Byte).is_some());
    }

    #[test]
    fn partial_bar_write_uses_the_merged_dword_and_preserves_attributes() {
        let (root, endpoint_bdf, old_base) = enabled_root_with_bar();
        let new_base = APERTURE_START + 0x10_0000;

        root.write_config(
            endpoint_bdf,
            offset(0x1a),
            AccessWidth::Word,
            new_base >> 16,
        )
        .unwrap();

        assert_eq!(
            root.read_config(endpoint_bdf, offset(0x18), AccessWidth::Dword)
                .unwrap(),
            new_base
        );
        assert_eq!(
            root.read_config(endpoint_bdf, offset(0x18), AccessWidth::Byte)
                .unwrap(),
            0
        );
        assert!(root.resolve_bar(old_base, AccessWidth::Byte).is_none());
        assert!(root.resolve_bar(new_base, AccessWidth::Byte).is_some());
    }

    #[test]
    fn invalid_bar_relocation_preserves_config_and_route() {
        let (root, endpoint_bdf, old_base) = enabled_root_with_bar();

        root.write_config(
            endpoint_bdf,
            offset(0x18),
            AccessWidth::Dword,
            APERTURE_START + 0x10,
        )
        .unwrap();

        assert_eq!(
            root.read_config(endpoint_bdf, offset(0x18), AccessWidth::Dword)
                .unwrap(),
            old_base
        );
        assert!(root.resolve_bar(old_base, AccessWidth::Byte).is_some());
    }

    #[test]
    fn overlapping_bar_relocation_preserves_the_previous_route() {
        let bar2 = PciBarIndex::new(2).unwrap();
        let alpha_base = APERTURE_START;
        let beta_base = APERTURE_START + BAR_SIZE;
        let mut builder = PciTopologyBuilder::new();
        for (id, base) in [("alpha", alpha_base), ("beta", beta_base)] {
            builder
                .add_function(
                    function(id)
                        .with_bar(
                            PciMemoryBar::new(bar2, BAR_SIZE)
                                .unwrap()
                                .with_address(ResourceRequest::Fixed(base)),
                        )
                        .unwrap(),
                )
                .unwrap();
        }
        let topology = Arc::new(builder.resolve(APERTURE_START..APERTURE_END).unwrap());
        let alpha_bdf = topology.function(&node("alpha")).unwrap().bdf();
        let beta_bdf = topology.function(&node("beta")).unwrap().bdf();
        let root = PciRootState::new(topology);
        for endpoint_bdf in [alpha_bdf, beta_bdf] {
            root.write_config(endpoint_bdf, offset(4), AccessWidth::Word, 0x0002)
                .unwrap();
        }

        root.write_config(beta_bdf, offset(0x18), AccessWidth::Dword, alpha_base)
            .unwrap();

        assert_eq!(
            root.read_config(beta_bdf, offset(0x18), AccessWidth::Dword)
                .unwrap(),
            beta_base
        );
        assert_eq!(
            root.resolve_bar(alpha_base, AccessWidth::Byte)
                .unwrap()
                .bdf(),
            alpha_bdf
        );
        assert_eq!(
            root.resolve_bar(beta_base, AccessWidth::Byte)
                .unwrap()
                .bdf(),
            beta_bdf
        );
    }

    #[test]
    fn reset_restores_root_owned_command_bar_and_route_state() {
        let (root, endpoint_bdf, power_on_base) = enabled_root_with_bar();
        let relocated = APERTURE_START + 0x10_0000;
        root.write_config(endpoint_bdf, offset(0x18), AccessWidth::Dword, relocated)
            .unwrap();
        assert!(root.resolve_bar(relocated, AccessWidth::Byte).is_some());

        root.reset();

        assert_eq!(
            root.read_config(endpoint_bdf, offset(4), AccessWidth::Word)
                .unwrap(),
            0
        );
        assert_eq!(
            root.read_config(endpoint_bdf, offset(0x18), AccessWidth::Dword)
                .unwrap(),
            power_on_base
        );
        assert!(root.resolve_bar(power_on_base, AccessWidth::Byte).is_none());
    }

    fn function(id: &str) -> PciFunctionSpec {
        PciFunctionSpec::new(
            node(id),
            PciEndpointIdentity::new(0x1234, 0x5678, PciClass::new(0x05, 0x00, 0x00))
                .with_revision(1),
        )
    }

    fn node(id: &str) -> DeviceNodeId {
        DeviceNodeId::new(id).unwrap()
    }

    fn bdf(device: u8, function: u8) -> PciBdf {
        PciBdf::new(PciSegment::new(0), 0, device, function).unwrap()
    }

    fn offset(value: u16) -> ConfigOffset {
        ConfigOffset::new(value).unwrap()
    }

    fn root_with_bar() -> (PciRootState, PciBdf, u64) {
        let bar2 = PciBarIndex::new(2).unwrap();
        let endpoint = function("endpoint")
            .with_bar(PciMemoryBar::new(bar2, BAR_SIZE).unwrap())
            .unwrap();
        let mut builder = PciTopologyBuilder::new();
        builder.add_function(endpoint).unwrap();
        let topology = Arc::new(builder.resolve(APERTURE_START..APERTURE_END).unwrap());
        let function = topology.function(&node("endpoint")).unwrap();
        let endpoint_bdf = function.bdf();
        let bar_base = function.bar(bar2).unwrap().address();
        (PciRootState::new(topology), endpoint_bdf, bar_base)
    }

    fn enabled_root_with_bar() -> (PciRootState, PciBdf, u64) {
        let (root, endpoint_bdf, bar_base) = root_with_bar();
        root.write_config(endpoint_bdf, offset(4), AccessWidth::Word, 0x0002)
            .unwrap();
        (root, endpoint_bdf, bar_base)
    }
}
