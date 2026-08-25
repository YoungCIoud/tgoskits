//! Architecture-neutral PCI root-owned config and BAR decode state.
//!
//! Frontends pass already-decoded BDF/config accesses into this object. The
//! root owns only conventional config bytes and BAR routes; endpoint objects,
//! runtime identities, and lifecycle callbacks are introduced by later
//! integration layers.

use alloc::{collections::BTreeMap, string::ToString, sync::Arc};
use core::{fmt, ops::Range};

use ax_sync::SpinLock;

use super::{
    EndpointRouteToken, FOUR_GIB, PciBarIndex, PciBdf, PciError, PciResult, ResolvedPciTopology,
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
        resolve_route(&state.functions, address, access_end, width).map(|(_, route)| route)
    }

    pub(crate) fn resolve_bound_bar(
        &self,
        address: u64,
        width: AccessWidth,
    ) -> Option<(EndpointRouteToken, PciBarRoute)> {
        let access_end = address.checked_add(width.size() as u64)?;
        let state = self.state.lock_irqsave();
        let (bdf, route) = resolve_route(&state.functions, address, access_end, width)?;
        Some((*state.bindings.get(&bdf)?, route))
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

    pub(crate) fn unbind_endpoint(&self, token: EndpointRouteToken) {
        self.state
            .lock_irqsave()
            .bindings
            .retain(|_, registered| *registered != token);
    }

    /// Restores every function's root-owned power-on config and BAR route.
    pub fn reset(&self) {
        for function in &mut self.state.lock_irqsave().functions {
            function.reset();
        }
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

fn all_ones(size: usize) -> u64 {
    u64::MAX >> ((8 - size) * 8)
}
