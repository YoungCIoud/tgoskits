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
    config::{BarWriteAction, FunctionState},
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

    /// Applies one already width-checked config write.
    ///
    /// An absent BDF ignores the write.
    ///
    /// # Errors
    ///
    /// Returns [`PciError::InvalidDirectConfigAccess`] under the same
    /// conditions as [`PciRootState::read_config`].
    pub(crate) fn write_config(
        &mut self,
        bdf: PciBdf,
        offset: usize,
        size: usize,
        value: u64,
    ) -> PciResult {
        Self::validate_direct_access(offset, size)?;
        let Some(index) = self.function_index(bdf) else {
            return Ok(());
        };
        let Some(action) = self.functions[index].prepare_bar_write(offset, size, value) else {
            self.functions[index].write_non_bar(offset, size, value);
            return Ok(());
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
                let accepted = self
                    .bar_address_available(index, bar, candidate)
                    .then_some(candidate);
                self.functions[index].finish_relocation(bar, high, accepted);
            }
        }
        Ok(())
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

    pub(crate) fn reset(&mut self) {
        for function in &mut self.functions {
            function.reset();
        }
    }
}

/// One resolved BAR dispatch target handed to an endpoint handler outside the
/// root lock.
pub(crate) struct BarRoute {
    pub(crate) handler: Arc<dyn PciFunction>,
    pub(crate) access: PciBarAccess,
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
