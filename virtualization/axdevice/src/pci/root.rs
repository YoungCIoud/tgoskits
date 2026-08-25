//! Architecture-neutral PCI root-owned config and BAR decode state.
//!
//! Frontends pass already-decoded BDF/config accesses into this object. The
//! root owns only conventional config bytes and BAR routes; endpoint objects,
//! runtime identities, and lifecycle callbacks are introduced by later
//! integration layers.

use alloc::sync::Arc;
use core::{fmt, ops::Range};

use ax_sync::SpinLock;

use super::{
    FOUR_GIB, PciBarIndex, PciBdf, PciResult, ResolvedPciTopology,
    config::{BarWriteAction, FunctionState},
};
use crate::{AccessWidth, ConfigOffset};

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
            state: SpinLock::new(RootState { functions }),
            topology,
        }
    }

    /// Returns the immutable topology that produced this root state.
    pub fn topology(&self) -> &ResolvedPciTopology {
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
        let (offset, size) = offset.validate_access(width)?;
        let state = self.state.lock_irqsave();
        Ok(state.function_index(bdf).map_or_else(
            || all_ones(size),
            |index| state.functions[index].read(offset, size),
        ))
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
        let (offset, size) = offset.validate_access(width)?;
        let mut state = self.state.lock_irqsave();
        let Some(function_index) = state.function_index(bdf) else {
            return Ok(());
        };
        let Some(action) = state.functions[function_index].prepare_bar_write(offset, size, value)
        else {
            state.functions[function_index].write_non_bar(offset, size, value);
            return Ok(());
        };
        match action {
            BarWriteAction::Probe { bar } => state.functions[function_index].apply_probe(bar),
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
        Ok(())
    }

    /// Resolves one complete memory access against the current enabled BARs.
    ///
    /// Returns `None` for overflow, disabled decode, or an unmapped address.
    pub fn resolve_bar(&self, address: u64, width: AccessWidth) -> Option<PciBarRoute> {
        let access_end = address.checked_add(width.size() as u64)?;
        let state = self.state.lock_irqsave();
        for function in &state.functions {
            if !function.memory_decode_enabled() {
                continue;
            }
            for bar in function.bars() {
                let Some(range) = bar.range() else {
                    continue;
                };
                if range.start <= address && access_end <= range.end {
                    return Some(PciBarRoute {
                        bdf: function.bdf(),
                        bar: bar.index(),
                        offset: address - range.start,
                        width,
                    });
                }
            }
        }
        None
    }

    /// Restores every function's root-owned power-on config and BAR route.
    pub fn reset(&self) {
        for function in &mut self.state.lock_irqsave().functions {
            function.reset();
        }
    }
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

fn all_ones(size: usize) -> u64 {
    u64::MAX >> ((8 - size) * 8)
}
