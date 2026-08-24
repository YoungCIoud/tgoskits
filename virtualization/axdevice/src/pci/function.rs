//! Type-0 function descriptors and nested BAR access boundary.

use alloc::vec::Vec;
use core::fmt;

use axdevice_base::{DeviceContext, DeviceResult, DeviceVcpuId};

use super::{
    PciBarIndex, PciBdf, PciCapability, PciError, PciMemoryBar, PciResult,
    config::capability_chain_offsets,
};
use crate::{AccessWidth, DeviceNodeId, ResourceRequest};

/// PCI class-code triplet for a Type-0 function.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciClass {
    base: u8,
    subclass: u8,
    programming_interface: u8,
}

impl PciClass {
    /// Creates a class-code triplet.
    pub const fn new(base: u8, subclass: u8, programming_interface: u8) -> Self {
        Self {
            base,
            subclass,
            programming_interface,
        }
    }

    pub(crate) const fn base(self) -> u8 {
        self.base
    }

    pub(crate) const fn subclass(self) -> u8 {
        self.subclass
    }

    pub(crate) const fn programming_interface(self) -> u8 {
        self.programming_interface
    }
}

/// Immutable identity fields of one Type-0 PCI function.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciEndpointIdentity {
    vendor_id: u16,
    device_id: u16,
    class: PciClass,
    revision: u8,
    subsystem_vendor_id: u16,
    subsystem_id: u16,
}

impl PciEndpointIdentity {
    /// Creates an endpoint identity with revision and subsystem fields set to zero.
    pub const fn new(vendor_id: u16, device_id: u16, class: PciClass) -> Self {
        Self {
            vendor_id,
            device_id,
            class,
            revision: 0,
            subsystem_vendor_id: 0,
            subsystem_id: 0,
        }
    }

    /// Sets the revision ID.
    pub const fn with_revision(mut self, revision: u8) -> Self {
        self.revision = revision;
        self
    }

    /// Sets subsystem vendor and device IDs.
    pub const fn with_subsystem(mut self, vendor_id: u16, subsystem_id: u16) -> Self {
        self.subsystem_vendor_id = vendor_id;
        self.subsystem_id = subsystem_id;
        self
    }

    pub(crate) const fn vendor_id(self) -> u16 {
        self.vendor_id
    }

    pub(crate) const fn device_id(self) -> u16 {
        self.device_id
    }

    pub(crate) const fn class(self) -> PciClass {
        self.class
    }

    pub(crate) const fn revision(self) -> u8 {
        self.revision
    }

    pub(crate) const fn subsystem_vendor_id(self) -> u16 {
        self.subsystem_vendor_id
    }

    pub(crate) const fn subsystem_id(self) -> u16 {
        self.subsystem_id
    }
}

/// One function-relative access routed through a committed memory BAR.
#[derive(Clone, Copy, Debug)]
pub struct PciBarAccess {
    source_vcpu: DeviceVcpuId,
    bdf: PciBdf,
    bar: PciBarIndex,
    offset: u64,
    width: AccessWidth,
}

impl PciBarAccess {
    pub(crate) const fn new(
        source_vcpu: DeviceVcpuId,
        bdf: PciBdf,
        bar: PciBarIndex,
        offset: u64,
        width: AccessWidth,
    ) -> Self {
        Self {
            source_vcpu,
            bdf,
            bar,
            offset,
            width,
        }
    }

    /// Returns the vCPU that issued the BAR access.
    pub const fn source_vcpu(&self) -> DeviceVcpuId {
        self.source_vcpu
    }

    /// Returns the function selected by the root complex.
    pub const fn bdf(&self) -> PciBdf {
        self.bdf
    }

    /// Returns the BAR selected by the root complex.
    pub const fn bar(&self) -> PciBarIndex {
        self.bar
    }

    /// Returns the byte offset from the current BAR base.
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// Returns the access width.
    pub const fn width(&self) -> AccessWidth {
        self.width
    }
}

/// Endpoint behavior invoked after the root complex resolves a memory BAR.
pub trait PciFunction: Send + Sync {
    /// Returns a diagnostic function name.
    fn name(&self) -> &str;

    /// Handles one function-relative BAR read.
    ///
    /// The root-complex config lock is not held while this method runs.
    fn read_bar(&self, access: &PciBarAccess, context: &mut dyn DeviceContext)
    -> DeviceResult<u64>;

    /// Handles one function-relative BAR write.
    ///
    /// The root-complex config lock is not held while this method runs.
    fn write_bar(
        &self,
        access: &PciBarAccess,
        value: u64,
        context: &mut dyn DeviceContext,
    ) -> DeviceResult;
}

/// Unresolved Type-0 function declaration consumed by [`PciTopologyBuilder`](super::PciTopologyBuilder).
///
/// This is pure configuration and resource metadata. The runtime
/// [`PciFunction`] is created later by a graph-backed endpoint model and bound
/// with the lifetime of its [`DeviceBundle`](crate::DeviceBundle).
pub struct PciFunctionSpec {
    pub(crate) id: DeviceNodeId,
    pub(crate) identity: PciEndpointIdentity,
    pub(crate) bdf: ResourceRequest<PciBdf>,
    pub(crate) bars: Vec<PciMemoryBar>,
    pub(crate) capabilities: Vec<PciCapability>,
}

impl fmt::Debug for PciFunctionSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PciFunctionSpec")
            .field("id", &self.id)
            .field("identity", &self.identity)
            .field("bdf", &self.bdf)
            .field("bars", &self.bars)
            .field("capabilities", &self.capabilities)
            .finish()
    }
}

impl PciFunctionSpec {
    /// Creates an automatically placed function with no BARs or capabilities.
    pub fn new(id: DeviceNodeId, identity: PciEndpointIdentity) -> Self {
        Self {
            id,
            identity,
            bdf: ResourceRequest::Auto,
            bars: Vec::new(),
            capabilities: Vec::new(),
        }
    }

    /// Returns the device-graph identity of this function.
    pub const fn id(&self) -> &DeviceNodeId {
        &self.id
    }

    /// Selects automatic or fixed BDF placement.
    pub fn with_bdf(mut self, bdf: ResourceRequest<PciBdf>) -> Self {
        self.bdf = bdf;
        self
    }

    /// Adds one memory BAR.
    ///
    /// # Errors
    ///
    /// Returns [`PciError::InvalidBar`] when this descriptor reuses a BAR slot,
    /// including the upper slot owned by a 64-bit BAR.
    pub fn with_bar(mut self, bar: PciMemoryBar) -> PciResult<Self> {
        let start = bar.index().value();
        let end = start + bar.occupied_slots();
        let overlaps = self.bars.iter().any(|existing| {
            let existing_start = existing.index().value();
            let existing_end = existing_start + existing.occupied_slots();
            start < existing_end && existing_start < end
        });
        if overlaps {
            return Err(PciError::InvalidBar {
                bar: bar.index(),
                detail: "BAR slot is already occupied by this function".into(),
            });
        }
        self.bars.push(bar);
        Ok(self)
    }

    /// Adds one standard capability to the function.
    ///
    /// # Errors
    ///
    /// Returns [`PciError::InvalidCapability`] when the complete aligned chain
    /// no longer fits below legacy config offset `0x100`.
    pub fn with_capability(mut self, capability: PciCapability) -> PciResult<Self> {
        capability_chain_offsets(
            self.capabilities
                .iter()
                .chain(core::iter::once(&capability)),
        )?;
        self.capabilities.push(capability);
        Ok(self)
    }
}
