//! Deterministic Type-0 function and BAR resolution.

use alloc::{
    collections::{BTreeMap, BTreeSet},
    string::ToString,
    vec::Vec,
};
use core::{cmp::Reverse, fmt, ops::Range};

use super::{
    PciBarIndex, PciBdf, PciError, PciFunctionSpec, PciHostBridgeConfig, PciMemoryBarWidth,
    PciResult, bar::ResolvedBarPlan, config::PowerOnConfig,
};
use crate::{DeviceNodeId, ResourceRequest};

const BDF_COUNT: u16 = 32 * 8;

/// Mutable PCI topology declaration sealed before VM execution.
pub struct PciTopologyBuilder {
    functions: BTreeMap<DeviceNodeId, PciFunctionSpec>,
}

impl fmt::Debug for PciTopologyBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PciTopologyBuilder")
            .field("function_count", &self.functions.len())
            .finish()
    }
}

impl PciTopologyBuilder {
    /// Creates an empty topology awaiting a graph-resolved host bridge.
    pub const fn new() -> Self {
        Self {
            functions: BTreeMap::new(),
        }
    }

    /// Adds one function declaration.
    ///
    /// # Errors
    ///
    /// Returns [`PciError::DuplicateFunction`] when the stable identity is
    /// already present.
    pub fn add_function(&mut self, function: PciFunctionSpec) -> PciResult {
        let id = function.id.clone();
        if self.functions.contains_key(&id) {
            return Err(PciError::DuplicateFunction {
                function: id.to_string(),
            });
        }
        self.functions.insert(id, function);
        Ok(())
    }

    /// Resolves all fixed requests, performs deterministic automatic
    /// placement, validates the complete topology, and freezes it.
    ///
    /// # Errors
    ///
    /// Returns a typed [`PciError`] for any BDF, BAR, capability, or aperture
    /// conflict. No partially resolved topology is published.
    pub fn resolve(self, host: PciHostBridgeConfig) -> PciResult<ResolvedPciTopology> {
        let bdfs = resolve_bdfs(&self.functions)?;
        validate_function_zero(&bdfs)?;
        let bar_addresses = resolve_bar_addresses(&host, &self.functions)?;
        let multifunction_devices = multifunction_devices(&bdfs);
        let mut functions = Vec::with_capacity(self.functions.len());
        for (id, spec) in self.functions {
            let bdf = bdfs[&id];
            let bars = spec
                .bars
                .iter()
                .map(|bar| ResolvedBarPlan {
                    index: bar.index(),
                    size: bar.size(),
                    width: bar.width(),
                    address: bar_addresses[&(id.clone(), bar.index())],
                })
                .collect::<Vec<_>>();
            let power_on = PowerOnConfig::build(
                spec.identity,
                &bars,
                &spec.capabilities,
                multifunction_devices.contains(&bdf.device()),
            )?;
            functions.push(ResolvedPciFunction {
                id,
                bdf,
                bars,
                power_on,
            });
        }
        functions.sort_by_key(|function| function.bdf);
        Ok(ResolvedPciTopology { host, functions })
    }
}

impl Default for PciTopologyBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// One immutable, resolved PCI function.
pub struct ResolvedPciFunction {
    id: DeviceNodeId,
    bdf: PciBdf,
    bars: Vec<ResolvedBarPlan>,
    pub(crate) power_on: PowerOnConfig,
}

impl fmt::Debug for ResolvedPciFunction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedPciFunction")
            .field("id", &self.id)
            .field("bdf", &self.bdf)
            .field("bars", &self.bars)
            .finish()
    }
}

impl ResolvedPciFunction {
    /// Returns the stable function identity.
    pub const fn id(&self) -> &DeviceNodeId {
        &self.id
    }

    /// Returns the resolved BDF.
    pub const fn bdf(&self) -> PciBdf {
        self.bdf
    }

    /// Returns one resolved BAR descriptor.
    pub fn bar(&self, index: PciBarIndex) -> Option<ResolvedPciBar> {
        self.bars
            .iter()
            .find(|bar| bar.index == index)
            .copied()
            .map(ResolvedPciBar)
    }

    pub(crate) fn bars(&self) -> &[ResolvedBarPlan] {
        &self.bars
    }
}

/// Public immutable view of one resolved memory BAR.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedPciBar(ResolvedBarPlan);

impl ResolvedPciBar {
    /// Returns the first BAR slot.
    pub const fn index(self) -> PciBarIndex {
        self.0.index
    }

    /// Returns the assigned PCI/guest-physical address.
    pub const fn address(self) -> u64 {
        self.0.address
    }

    /// Returns the fixed size.
    pub const fn size(self) -> u64 {
        self.0.size
    }

    /// Returns the encoded width.
    pub const fn width(self) -> PciMemoryBarWidth {
        self.0.width
    }
}

/// Immutable PCI topology shared by firmware planning and runtime creation.
pub struct ResolvedPciTopology {
    host: PciHostBridgeConfig,
    functions: Vec<ResolvedPciFunction>,
}

impl fmt::Debug for ResolvedPciTopology {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedPciTopology")
            .field("host", &self.host)
            .field("functions", &self.functions)
            .finish()
    }
}

impl ResolvedPciTopology {
    /// Returns the host bridge aperture descriptor.
    pub const fn host(&self) -> &PciHostBridgeConfig {
        &self.host
    }

    /// Returns functions in BDF order.
    pub fn functions(&self) -> impl Iterator<Item = &ResolvedPciFunction> {
        self.functions.iter()
    }

    /// Finds one function by stable identity.
    pub fn function(&self, id: &DeviceNodeId) -> Option<&ResolvedPciFunction> {
        self.functions.iter().find(|function| function.id() == id)
    }

    pub(crate) fn function_plans(&self) -> &[ResolvedPciFunction] {
        &self.functions
    }
}

fn resolve_bdfs(
    functions: &BTreeMap<DeviceNodeId, PciFunctionSpec>,
) -> PciResult<BTreeMap<DeviceNodeId, PciBdf>> {
    let mut resolved = BTreeMap::new();
    let mut occupied = BTreeMap::<PciBdf, DeviceNodeId>::new();
    for (id, spec) in functions {
        if let ResourceRequest::Fixed(bdf) = spec.bdf {
            validate_supported_bdf(bdf)?;
            if let Some(existing) = occupied.insert(bdf, id.clone()) {
                return Err(PciError::DuplicateBdf {
                    bdf,
                    first: existing.to_string(),
                    second: id.to_string(),
                });
            }
            resolved.insert(id.clone(), bdf);
        }
    }
    for (id, spec) in functions {
        if spec.bdf == ResourceRequest::Auto {
            let bdf = (0..BDF_COUNT)
                .map(PciBdf::bus_zero)
                .find(|candidate| !occupied.contains_key(candidate))
                .ok_or_else(|| PciError::BdfExhausted {
                    function: id.to_string(),
                })?;
            occupied.insert(bdf, id.clone());
            resolved.insert(id.clone(), bdf);
        }
    }
    Ok(resolved)
}

fn validate_supported_bdf(bdf: PciBdf) -> PciResult {
    if bdf.segment().value() != 0 {
        return Err(PciError::InvalidAddress {
            component: "segment",
            value: u64::from(bdf.segment().value()),
        });
    }
    if bdf.bus() != 0 {
        return Err(PciError::InvalidAddress {
            component: "bus",
            value: u64::from(bdf.bus()),
        });
    }
    Ok(())
}

fn validate_function_zero(bdfs: &BTreeMap<DeviceNodeId, PciBdf>) -> PciResult {
    let present = bdfs.values().copied().collect::<BTreeSet<_>>();
    for bdf in bdfs.values().copied().filter(|bdf| bdf.function() != 0) {
        let function_zero = PciBdf::new(bdf.segment(), bdf.bus(), bdf.device(), 0)
            .expect("an existing BDF has a valid device number");
        if !present.contains(&function_zero) {
            return Err(PciError::MissingFunctionZero { bdf });
        }
    }
    Ok(())
}

fn multifunction_devices(bdfs: &BTreeMap<DeviceNodeId, PciBdf>) -> BTreeSet<u8> {
    let mut counts = [0u8; 32];
    for bdf in bdfs.values() {
        counts[usize::from(bdf.device())] += 1;
    }
    counts
        .iter()
        .enumerate()
        .filter_map(|(device, count)| (*count > 1).then_some(device as u8))
        .collect()
}

fn resolve_bar_addresses(
    host: &PciHostBridgeConfig,
    functions: &BTreeMap<DeviceNodeId, PciFunctionSpec>,
) -> PciResult<BTreeMap<(DeviceNodeId, PciBarIndex), u64>> {
    let mut fixed = Vec::new();
    let mut automatic = Vec::new();
    for (id, spec) in functions {
        for bar in &spec.bars {
            let placement = BarPlacement {
                function: id.clone(),
                index: bar.index(),
                size: bar.size(),
                width: bar.width(),
                request: bar.address_request(),
            };
            match placement.request {
                ResourceRequest::Fixed(_) => fixed.push(placement),
                ResourceRequest::Auto => automatic.push(placement),
            }
        }
    }
    fixed.sort_by(|left, right| {
        left.function
            .cmp(&right.function)
            .then_with(|| left.index.cmp(&right.index))
    });
    automatic.sort_by(|left, right| {
        Reverse(left.size)
            .cmp(&Reverse(right.size))
            .then_with(|| left.function.cmp(&right.function))
            .then_with(|| left.index.cmp(&right.index))
    });

    let mut occupied = Vec::<Range<u64>>::new();
    let mut resolved = BTreeMap::new();
    for placement in fixed {
        let ResourceRequest::Fixed(address) = placement.request else {
            unreachable!("fixed placement list contains only fixed requests");
        };
        let range = checked_bar_range(host, &placement, address)?;
        if overlaps_any(&occupied, &range) {
            return Err(PciError::BarConflict {
                function: placement.function.to_string(),
                bar: placement.index,
                start: range.start,
                end: range.end,
            });
        }
        occupied.push(range);
        resolved.insert((placement.function, placement.index), address);
    }
    for placement in automatic {
        let address =
            first_fit(host.memory_aperture(), placement.size, &occupied).ok_or_else(|| {
                PciError::BarApertureExhausted {
                    function: placement.function.to_string(),
                    bar: placement.index,
                    size: placement.size,
                }
            })?;
        let range = checked_bar_range(host, &placement, address)?;
        occupied.push(range);
        resolved.insert((placement.function, placement.index), address);
    }
    Ok(resolved)
}

struct BarPlacement {
    function: DeviceNodeId,
    index: PciBarIndex,
    size: u64,
    width: PciMemoryBarWidth,
    request: ResourceRequest<u64>,
}

fn checked_bar_range(
    host: &PciHostBridgeConfig,
    placement: &BarPlacement,
    address: u64,
) -> PciResult<Range<u64>> {
    if address & (placement.size - 1) != 0 {
        return Err(PciError::InvalidBar {
            bar: placement.index,
            detail: "fixed address is not aligned to BAR size".into(),
        });
    }
    let end = address
        .checked_add(placement.size)
        .ok_or_else(|| PciError::InvalidBar {
            bar: placement.index,
            detail: "BAR range overflows u64".into(),
        })?;
    let aperture = host.memory_aperture();
    if address < aperture.start || end > aperture.end {
        return Err(PciError::InvalidBar {
            bar: placement.index,
            detail: "fixed address lies outside the host memory aperture".into(),
        });
    }
    if placement.width == PciMemoryBarWidth::Bits32 && end > u64::from(u32::MAX) + 1 {
        return Err(PciError::InvalidBar {
            bar: placement.index,
            detail: "32-bit BAR range exceeds 4 GiB".into(),
        });
    }
    Ok(address..end)
}

fn first_fit(aperture: Range<u64>, size: u64, occupied: &[Range<u64>]) -> Option<u64> {
    let mut candidate = align_up(aperture.start, size)?;
    loop {
        let end = candidate.checked_add(size)?;
        if end > aperture.end {
            return None;
        }
        if let Some(conflict) = occupied
            .iter()
            .filter(|range| candidate < range.end && range.start < end)
            .min_by_key(|range| range.start)
        {
            candidate = align_up(conflict.end, size)?;
        } else {
            return Some(candidate);
        }
    }
}

fn align_up(value: u64, alignment: u64) -> Option<u64> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}

fn overlaps_any(occupied: &[Range<u64>], candidate: &Range<u64>) -> bool {
    occupied
        .iter()
        .any(|range| candidate.start < range.end && range.start < candidate.end)
}
