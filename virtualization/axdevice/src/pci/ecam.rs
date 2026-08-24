//! ECAM host device and dynamic memory-BAR routing.

use alloc::{
    boxed::Box,
    string::ToString,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{fmt, ops::Range};

use ax_sync::SpinLock;
use axdevice_base::{
    BusKind, Device, DeviceAccess, DeviceContext, DeviceError, DeviceResult, Resource,
};

use super::{
    FOUR_GIB, PciBarAccess, PciBdf, PciError, PciFunction, PciResult, ResolvedPciTopology,
    address::decode_ecam_offset,
    config::{BarWriteAction, FunctionState},
};
use crate::{DeviceBundle, DeviceLifecycle, DeviceManagerResult, DeviceNodeId};

pub(crate) const ECAM_SIZE: u64 = 1 << 20;
const ECAM_ALIGNMENT: u64 = ECAM_SIZE;

/// Fixed ECAM and non-prefetchable memory apertures of one PCI host bridge.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PciHostBridgeConfig {
    ecam_base: u64,
    memory_aperture: Range<u64>,
}

impl PciHostBridgeConfig {
    /// Creates a segment-0, bus-0 generic ECAM host descriptor.
    ///
    /// # Errors
    ///
    /// Returns [`PciError::InvalidHostAperture`] when ECAM is not 1 MiB
    /// aligned, the memory aperture is empty or above 4 GiB, either range
    /// overflows, or the two ranges overlap.
    pub fn new(ecam_base: u64, memory_aperture: Range<u64>) -> PciResult<Self> {
        let config = Self {
            ecam_base,
            memory_aperture,
        };
        config.validate()?;
        Ok(config)
    }

    /// Returns the ECAM base. Its size is always 1 MiB.
    pub const fn ecam_base(&self) -> u64 {
        self.ecam_base
    }

    /// Returns the ECAM size for bus 0.
    pub const fn ecam_size(&self) -> u64 {
        ECAM_SIZE
    }

    /// Returns the CPU-visible, identity-mapped PCI memory aperture.
    pub fn memory_aperture(&self) -> Range<u64> {
        self.memory_aperture.clone()
    }

    pub(crate) fn validate(&self) -> PciResult {
        if self.ecam_base & (ECAM_ALIGNMENT - 1) != 0 {
            return Err(invalid_host("ECAM base is not 1 MiB aligned"));
        }
        let ecam_end = self
            .ecam_base
            .checked_add(ECAM_SIZE)
            .ok_or_else(|| invalid_host("ECAM range overflows u64"))?;
        if self.memory_aperture.start >= self.memory_aperture.end {
            return Err(invalid_host("memory aperture is empty"));
        }
        if self.memory_aperture.end > FOUR_GIB {
            return Err(invalid_host("memory aperture extends above 4 GiB"));
        }
        if self.ecam_base < self.memory_aperture.end && self.memory_aperture.start < ecam_end {
            return Err(invalid_host("ECAM and memory aperture overlap"));
        }
        Ok(())
    }

    fn ecam_range(&self) -> Range<u64> {
        self.ecam_base..self.ecam_base + ECAM_SIZE
    }
}

/// Segment-0, bus-0 virtual PCI root complex registered as one runtime device.
pub(crate) struct PciRootComplex {
    topology: Arc<ResolvedPciTopology>,
    state: SpinLock<PciRuntimeState>,
    resources: Box<[Resource]>,
}

impl fmt::Debug for PciRootComplex {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PciRootComplex")
            .field("topology", &self.topology)
            .field("resources", &self.resources)
            .finish_non_exhaustive()
    }
}

impl PciRootComplex {
    /// Creates mutable config/BAR state from one frozen topology.
    pub(crate) fn new(topology: Arc<ResolvedPciTopology>) -> Self {
        let host = topology.host();
        let memory = host.memory_aperture();
        let resources = alloc::vec![
            Resource::MmioRange {
                base: host.ecam_base(),
                size: host.ecam_size(),
            },
            Resource::MmioRange {
                base: memory.start,
                size: memory.end - memory.start,
            },
        ]
        .into_boxed_slice();
        let state = PciRuntimeState::new(&topology);
        Self {
            topology,
            state: SpinLock::new(state),
            resources,
        }
    }

    /// Binds one runtime function for exactly the lifetime of `bundle`.
    ///
    /// The binding is removed if bundle registration fails or when the sealed
    /// runtime is dropped. Configuration remains enumerable while unbound, but
    /// BAR accesses have no target.
    ///
    /// # Errors
    ///
    /// Returns [`PciError::FunctionNotFound`] for an undeclared identity,
    /// [`PciError::FunctionAlreadyBound`] for a duplicate live binding, or
    /// [`PciError::BindingGenerationExhausted`] after generation overflow.
    pub(crate) fn bind_function(
        self: &Arc<Self>,
        id: &DeviceNodeId,
        handler: Arc<dyn PciFunction>,
        bundle: &mut DeviceBundle,
    ) -> PciResult {
        let generation = self.state.lock_irqsave().bind_function(id, &handler)?;
        bundle.retain_registration(PciFunctionBindingLease {
            root: Arc::downgrade(self),
            function: id.clone(),
            generation,
            _handler: handler,
        });
        Ok(())
    }

    fn unbind_function(&self, id: &DeviceNodeId, generation: u64) {
        self.state.lock_irqsave().unbind_function(id, generation);
    }

    fn validate_mmio_access(&self, access: &DeviceAccess) -> DeviceResult {
        if access.bus() == BusKind::Mmio {
            Ok(())
        } else {
            Err(DeviceError::OutOfRange {
                addr: access.address(),
            })
        }
    }

    fn read_ecam(&self, access: &DeviceAccess) -> DeviceResult<u64> {
        let relative = access
            .address()
            .checked_sub(self.topology.host().ecam_base())
            .ok_or(DeviceError::OutOfRange {
                addr: access.address(),
            })?;
        let (bdf, offset) = decode_ecam_offset(relative);
        let size = offset
            .validate_access(access.width())
            .map_err(config_access_error)?;
        Ok(self
            .state
            .lock_irqsave()
            .read_config(bdf, usize::from(offset.value()), size))
    }

    fn write_ecam(&self, access: &DeviceAccess, value: u64) -> DeviceResult {
        let relative = access
            .address()
            .checked_sub(self.topology.host().ecam_base())
            .ok_or(DeviceError::OutOfRange {
                addr: access.address(),
            })?;
        let (bdf, offset) = decode_ecam_offset(relative);
        let size = offset
            .validate_access(access.width())
            .map_err(config_access_error)?;
        self.state
            .lock_irqsave()
            .write_config(bdf, usize::from(offset.value()), size, value);
        Ok(())
    }

    fn read_memory_bar(
        &self,
        access: &DeviceAccess,
        context: &mut dyn DeviceContext,
    ) -> DeviceResult<u64> {
        let route = {
            let state = self.state.lock_irqsave();
            state.resolve_bar(access).ok_or(DeviceError::NotFound)?
        };
        route.handler.read_bar(&route.access, context)
    }

    fn write_memory_bar(
        &self,
        access: &DeviceAccess,
        value: u64,
        context: &mut dyn DeviceContext,
    ) -> DeviceResult {
        let route = {
            let state = self.state.lock_irqsave();
            state.resolve_bar(access).ok_or(DeviceError::NotFound)?
        };
        route.handler.write_bar(&route.access, value, context)
    }
}

impl Device for PciRootComplex {
    fn name(&self) -> &str {
        "pci-root-complex"
    }

    fn resources(&self) -> &[Resource] {
        &self.resources
    }

    fn read(&self, access: &DeviceAccess, context: &mut dyn DeviceContext) -> DeviceResult<u64> {
        self.validate_mmio_access(access)?;
        if contains_access(self.topology.host().ecam_range(), access) {
            self.read_ecam(access)
        } else if contains_access(self.topology.host().memory_aperture(), access) {
            self.read_memory_bar(access, context)
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
        context: &mut dyn DeviceContext,
    ) -> DeviceResult {
        self.validate_mmio_access(access)?;
        if contains_access(self.topology.host().ecam_range(), access) {
            self.write_ecam(access, value)
        } else if contains_access(self.topology.host().memory_aperture(), access) {
            self.write_memory_bar(access, value, context)
        } else {
            Err(DeviceError::OutOfRange {
                addr: access.address(),
            })
        }
    }
}

impl DeviceLifecycle for PciRootComplex {
    fn reset(&self) -> DeviceManagerResult {
        self.state.lock_irqsave().reset();
        Ok(())
    }

    fn suspend(&self) -> DeviceManagerResult {
        Ok(())
    }

    fn resume(&self) -> DeviceManagerResult {
        Ok(())
    }
}

struct PciRuntimeState {
    memory_aperture: Range<u64>,
    functions: Vec<FunctionState>,
    next_binding_generation: u64,
}

impl PciRuntimeState {
    fn new(topology: &ResolvedPciTopology) -> Self {
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
            memory_aperture: topology.host().memory_aperture(),
            functions,
            next_binding_generation: 1,
        }
    }

    fn bind_function(
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

    fn unbind_function(&mut self, id: &DeviceNodeId, generation: u64) {
        if let Some(function) = self
            .functions
            .iter_mut()
            .find(|function| function.id() == id)
        {
            function.unbind_handler(generation);
        }
    }

    fn read_config(&self, bdf: PciBdf, offset: usize, size: usize) -> u64 {
        self.function_index(bdf).map_or_else(
            || all_ones(size),
            |index| self.functions[index].read(offset, size),
        )
    }

    fn write_config(&mut self, bdf: PciBdf, offset: usize, size: usize, value: u64) {
        let Some(index) = self.function_index(bdf) else {
            return;
        };
        let Some(action) = self.functions[index].prepare_bar_write(offset, size, value) else {
            self.functions[index].write_non_bar(offset, size, value);
            return;
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
    }

    fn resolve_bar(&self, access: &DeviceAccess) -> Option<BarRoute> {
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
            || (bar.width() == super::PciMemoryBarWidth::Bits32 && end > FOUR_GIB)
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

    fn reset(&mut self) {
        for function in &mut self.functions {
            function.reset();
        }
    }
}

struct BarRoute {
    handler: Arc<dyn PciFunction>,
    access: PciBarAccess,
}

struct PciFunctionBindingLease {
    root: Weak<PciRootComplex>,
    function: DeviceNodeId,
    generation: u64,
    _handler: Arc<dyn PciFunction>,
}

impl Drop for PciFunctionBindingLease {
    fn drop(&mut self) {
        if let Some(root) = self.root.upgrade() {
            root.unbind_function(&self.function, self.generation);
        }
    }
}

fn contains_access(range: Range<u64>, access: &DeviceAccess) -> bool {
    access
        .address()
        .checked_add(access.width().size() as u64)
        .is_some_and(|end| range.start <= access.address() && end <= range.end)
}

fn all_ones(size: usize) -> u64 {
    u64::MAX >> ((8 - size) * 8)
}

fn invalid_host(detail: &'static str) -> PciError {
    PciError::InvalidHostAperture { detail }
}

fn config_access_error(error: PciError) -> DeviceError {
    DeviceError::InvalidInput {
        operation: "access PCI configuration space",
        detail: alloc::format!("{error}"),
    }
}
