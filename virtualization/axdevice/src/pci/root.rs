//! Architecture-neutral PCI root state shared by config frontends.
//!
//! [`PciRootState`] owns function config images, BAR decode state, runtime
//! bindings, and the reset transition. It never implements a device trait and
//! it holds no ECAM base, CF8 address register, or architecture firmware
//! field: config frontends decode their own bus cycles and delegate
//! already-validated typed accesses here. Endpoint handler callbacks always
//! run outside the root lock.
//!
//! A future bus-owned DMA grant would be injected through the root device's
//! bundle registration; the ECAM and memory-aperture frontends never own one.

use alloc::{
    string::ToString,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::ops::Range;

use ax_sync::SpinLock;
use axdevice_base::{BusKind, DeviceAccess, DeviceError, DeviceResult};

use super::{
    FOUR_GIB, PciBarAccess, PciBdf, PciError, PciFunction, PciMemoryBarWidth, PciResult,
    ResolvedPciTopology,
    address::CONFIG_SPACE_SIZE,
    bar::PciBarDecodePolicy,
    config::{BarWriteAction, FunctionState, PciCommandState},
};
use crate::{DeviceBundle, DeviceNodeId};

/// Shared handle to one root's mutable state, held by every config frontend.
pub(crate) type SharedPciRoot = Arc<SpinLock<PciRootState>>;

pub(crate) struct PciRootState {
    memory_aperture: Range<u64>,
    functions: Vec<FunctionState>,
    next_binding_generation: u64,
}

impl PciRootState {
    /// Creates mutable config/BAR state from one frozen topology.
    pub(crate) fn new(topology: &ResolvedPciTopology) -> Self {
        let functions = topology
            .function_plans()
            .iter()
            .map(|function| {
                FunctionState::new(
                    function.id().clone(),
                    function.bdf(),
                    function.power_on.clone(),
                    function.bars(),
                )
            })
            .collect();
        Self {
            memory_aperture: topology.memory_aperture().clone(),
            functions,
            next_binding_generation: 1,
        }
    }

    pub(crate) fn bind_function(
        &mut self,
        id: &DeviceNodeId,
        handler: &Arc<dyn PciFunction>,
    ) -> PciResult<u64> {
        let index = self
            .functions
            .iter()
            .position(|function| function.id() == id)
            .ok_or_else(|| PciError::FunctionNotFound {
                function: id.to_string(),
            })?;
        let generation = self.next_binding_generation;
        let next_generation = generation
            .checked_add(1)
            .ok_or(PciError::BindingGenerationExhausted)?;
        self.functions[index].bind_handler(generation, handler)?;
        self.next_binding_generation = next_generation;
        Ok(generation)
    }

    pub(crate) fn unbind_function(&mut self, id: &DeviceNodeId, generation: u64) {
        if let Some(function) = self
            .functions
            .iter_mut()
            .find(|function| function.id() == id)
        {
            function.unbind_handler(generation);
        }
    }

    /// Reads one already width-checked config access.
    ///
    /// An absent BDF reads as all ones for the access size.
    ///
    /// # Errors
    ///
    /// Returns [`PciError::InvalidDirectConfigAccess`] when the access is not
    /// a supported config width or crosses the function boundary.
    pub(crate) fn read_config(&self, bdf: PciBdf, offset: usize, size: usize) -> PciResult<u64> {
        Self::validate_direct_access(offset, size)?;
        Ok(self.function_index(bdf).map_or_else(
            || all_ones(size),
            |index| self.functions[index].read(offset, size),
        ))
    }

    /// Applies one already width-checked config write and snapshots any
    /// standard command transition.
    ///
    /// An absent BDF ignores the write. Callers hold the root lock while this
    /// runs; endpoint effects are dispatched outside it through
    /// [`dispatch_command_effect`].
    ///
    /// # Errors
    ///
    /// Returns [`PciError::InvalidDirectConfigAccess`] under the same
    /// conditions as [`PciRootState::read_config`].
    pub(crate) fn write_config_locked(
        &mut self,
        bdf: PciBdf,
        offset: usize,
        size: usize,
        value: u64,
    ) -> PciResult<Option<PciConfigWriteEffect>> {
        Self::validate_direct_access(offset, size)?;
        let Some(index) = self.function_index(bdf) else {
            return Ok(None);
        };
        let command_before = self.functions[index].command_state();
        let Some(action) = self.functions[index].prepare_bar_write(offset, size, value) else {
            self.functions[index].write_non_bar(offset, size, value);
            let command_after = self.functions[index].command_state();
            let command = (command_after != command_before).then_some(command_after);
            return Ok(command.map(|command| PciConfigWriteEffect {
                command: Some(command),
            }));
        };
        match action {
            BarWriteAction::Probe { bar, high } => {
                self.functions[index].apply_probe(bar, high);
            }
            BarWriteAction::Relocate {
                bar,
                high,
                candidate,
            } => {
                let target = &self.functions[index].bars()[bar];
                let accepted = match target.decode_policy() {
                    PciBarDecodePolicy::Fixed => {
                        // The planned base is permanent. Rewriting it is
                        // accepted (it is already the decoded value); any
                        // other address is ignored with a diagnostic so the
                        // guest-visible readback and the decode never drift.
                        let planned = target.planned_address();
                        if candidate == planned {
                            true
                        } else {
                            log::warn!(
                                "ignoring PCI BAR{} relocation to {candidate:#x}: fixed policy \
                                 keeps the planned base {planned:#x}",
                                self.functions[index].bars()[bar].index().value()
                            );
                            false
                        }
                    }
                    PciBarDecodePolicy::RelocatableWithinHostAperture => {
                        self.bar_address_available(index, bar, candidate)
                    }
                };
                self.functions[index].finish_relocation(bar, high, accepted.then_some(candidate));
            }
        }
        Ok(None)
    }

    fn validate_direct_access(offset: usize, size: usize) -> PciResult {
        if !matches!(size, 1 | 2 | 4) {
            return Err(PciError::InvalidDirectConfigAccess {
                offset,
                size,
                detail: "access size is not a supported config width",
            });
        }
        let valid_end = offset
            .checked_add(size)
            .is_some_and(|end| end <= CONFIG_SPACE_SIZE);
        if !valid_end {
            return Err(PciError::InvalidDirectConfigAccess {
                offset,
                size,
                detail: "access crosses the function config-space boundary",
            });
        }
        Ok(())
    }

    pub(crate) fn resolve_bar(&self, access: &DeviceAccess) -> Option<BarRoute> {
        for function in &self.functions {
            if !function.memory_decode_enabled() {
                continue;
            }
            for bar in function.bars() {
                let range = bar.range()?;
                let end = access.address().checked_add(access.width().size() as u64)?;
                if range.start <= access.address() && end <= range.end {
                    return Some(BarRoute {
                        handler: function.handler()?,
                        access: PciBarAccess::new(
                            access.source_vcpu(),
                            function.bdf(),
                            bar.index(),
                            access.address() - range.start,
                            access.width(),
                        ),
                    });
                }
            }
        }
        None
    }

    fn bar_address_available(&self, owner: usize, owner_bar: usize, address: u64) -> bool {
        let bar = &self.functions[owner].bars()[owner_bar];
        let Some(end) = address.checked_add(bar.size()) else {
            return false;
        };
        if address & (bar.size() - 1) != 0
            || address < self.memory_aperture.start
            || end > self.memory_aperture.end
            || (bar.width() == PciMemoryBarWidth::Bits32 && end > FOUR_GIB)
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

    fn function_index(&self, bdf: PciBdf) -> Option<usize> {
        self.functions
            .binary_search_by_key(&bdf, FunctionState::bdf)
            .ok()
    }

    /// Recovers root-owned power-on state and collects the live endpoint
    /// handlers with their power-on command snapshots.
    ///
    /// Callers hold the root lock only for this recovery; handler resets run
    /// outside it so a failing or sleeping endpoint cannot stall the bus.
    pub(crate) fn reset_collecting_handlers(
        &mut self,
    ) -> Vec<(Arc<dyn PciFunction>, PciCommandState)> {
        let mut collected = Vec::new();
        for function in &mut self.functions {
            function.reset();
            if let Some(handler) = function.handler() {
                collected.push((handler, function.power_on_command_state()));
            }
        }
        collected
    }
}

/// One resolved BAR dispatch target handed to an endpoint handler outside the
/// root lock.
pub(crate) struct BarRoute {
    pub(crate) handler: Arc<dyn PciFunction>,
    pub(crate) access: PciBarAccess,
}

/// A standard-state change produced inside the root lock, applied outside it.
///
/// The dynamic config window and endpoint function-access effects ride on
/// this seam once a real consumer exists; command transitions are the first
/// dispatched kind and the reset path reuses the same out-of-lock pattern.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PciConfigWriteEffect {
    pub(crate) command: Option<PciCommandState>,
}

/// Applies one width-checked config write through the shared root state.
///
/// The lock scope covers validation, the standard-state merge, and the
/// effect snapshot only; transport, IRQ, DMA, and BAR handlers never run
/// inside it.
///
/// # Errors
///
/// Returns [`PciError::InvalidDirectConfigAccess`] when the access violates
/// root-level width or boundary rules.
pub(crate) fn write_config(
    root: &SharedPciRoot,
    bdf: PciBdf,
    offset: usize,
    size: usize,
    value: u64,
) -> PciResult {
    let effect = root
        .lock_irqsave()
        .write_config_locked(bdf, offset, size, value)?;
    if let Some(effect) = effect {
        dispatch_command_effect(root, effect);
    }
    Ok(())
}

fn dispatch_command_effect(_root: &SharedPciRoot, effect: PciConfigWriteEffect) {
    // Runs outside the root lock by contract. No endpoint consumes command
    // transitions yet; log so guest-visible changes stay diagnosable until
    // the observer arrives with the first dynamic consumer.
    if let Some(command) = effect.command {
        log::debug!(
            "PCI command state: memory_space={}, bus_master={}, intx_disabled={}",
            command.memory_space_enabled,
            command.bus_master_enabled,
            command.intx_disabled
        );
    }
}

/// Binds one runtime function for exactly the lifetime of `bundle`.
///
/// The binding lease is retained by `bundle`, so bundle registration failure
/// or a sealed-runtime drop releases it. Configuration remains enumerable
/// while unbound, but BAR accesses have no target.
///
/// # Errors
///
/// Returns [`PciError::FunctionNotFound`] for an undeclared identity,
/// [`PciError::FunctionAlreadyBound`] for a duplicate live binding, or
/// [`PciError::BindingGenerationExhausted`] after generation overflow.
pub(crate) fn bind_function(
    root: &SharedPciRoot,
    id: &DeviceNodeId,
    handler: Arc<dyn PciFunction>,
    bundle: &mut DeviceBundle,
) -> PciResult {
    let generation = root.lock_irqsave().bind_function(id, &handler)?;
    bundle.retain_registration(PciFunctionBindingLease {
        root: Arc::downgrade(root),
        function: id.clone(),
        generation,
        _handler: handler,
    });
    Ok(())
}

struct PciFunctionBindingLease {
    root: Weak<SpinLock<PciRootState>>,
    function: DeviceNodeId,
    generation: u64,
    _handler: Arc<dyn PciFunction>,
}

impl Drop for PciFunctionBindingLease {
    fn drop(&mut self) {
        if let Some(root) = self.root.upgrade() {
            root.lock_irqsave()
                .unbind_function(&self.function, self.generation);
        }
    }
}

/// Rejects non-MMIO accesses before any window lookup.
pub(crate) fn ensure_mmio_access(access: &DeviceAccess) -> DeviceResult {
    if access.bus() == BusKind::Mmio {
        Ok(())
    } else {
        Err(DeviceError::OutOfRange {
            addr: access.address(),
        })
    }
}

/// Checks whether one whole access of `width` fits inside `range`.
pub(crate) fn contains_access(range: &Range<u64>, access: &DeviceAccess) -> bool {
    access
        .address()
        .checked_add(access.width().size() as u64)
        .is_some_and(|end| range.start <= access.address() && end <= range.end)
}

fn all_ones(size: usize) -> u64 {
    u64::MAX >> ((8 - size) * 8)
}

#[cfg(test)]
mod tests {
    use super::{
        super::{PciClass, PciEndpointIdentity, PciFunctionSpec, PciTopologyBuilder},
        *,
    };
    use crate::DeviceNodeId;

    const APERTURE: Range<u64> = 0x2000_0000..0x2010_0000;

    fn root_with_one_function() -> PciRootState {
        let mut builder = PciTopologyBuilder::new();
        builder
            .add_function(PciFunctionSpec::new(
                DeviceNodeId::new("endpoint").unwrap(),
                PciEndpointIdentity::new(0x1234, 0x5678, PciClass::new(0xff, 0, 0)),
            ))
            .unwrap();
        let topology = builder.resolve(APERTURE).unwrap();
        PciRootState::new(&topology)
    }

    /// Direct public callers must get typed errors instead of panics or
    /// silently wrapped reads; the ECAM frontend rejects these earlier, so
    /// root-level enforcement guards only non-ECAM frontends and direct use.
    fn assert_rejected(state: &mut PciRootState, offset: usize, size: usize) {
        let bdf = state.functions[0].bdf();
        assert!(
            matches!(
                state.read_config(bdf, offset, size),
                Err(PciError::InvalidDirectConfigAccess { .. })
            ),
            "read offset {offset:#x} size {size} must be rejected"
        );
        assert!(
            matches!(
                state.write_config_locked(bdf, offset, size, 0),
                Err(PciError::InvalidDirectConfigAccess { .. })
            ),
            "write offset {offset:#x} size {size} must be rejected"
        );
    }

    #[test]
    fn rejects_zero_length_config_accesses() {
        let mut state = root_with_one_function();
        assert_rejected(&mut state, 0, 0);
    }

    #[test]
    fn rejects_qword_config_accesses() {
        let mut state = root_with_one_function();
        assert_rejected(&mut state, 0, 8);
        assert_rejected(&mut state, 0x100, 8);
    }

    #[test]
    fn rejects_size_width_mismatches() {
        let mut state = root_with_one_function();
        for size in [3usize, 5, 16] {
            assert_rejected(&mut state, 0, size);
        }
    }

    #[test]
    fn rejects_config_accesses_past_the_tail() {
        let mut state = root_with_one_function();
        assert_rejected(&mut state, 0x1000 - 3, 4);
        assert_rejected(&mut state, CONFIG_SPACE_SIZE - 1, 2);
    }

    #[test]
    fn rejects_overflowing_config_offsets() {
        let mut state = root_with_one_function();
        assert_rejected(&mut state, usize::MAX - 2, 4);
    }

    #[test]
    fn absent_bdf_reads_all_ones_and_ignores_writes_without_effects() {
        let mut state = root_with_one_function();
        let absent = PciBdf::bus_zero(31 * 8 + 7);
        assert_eq!(state.read_config(absent, 0, 1).unwrap(), 0xff);
        assert!(
            state
                .write_config_locked(absent, 4, 2, 0xffff)
                .unwrap()
                .is_none()
        );
    }
}
