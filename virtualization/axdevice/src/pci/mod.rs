//! Architecture-neutral virtual PCI Type-0 and ECAM foundation.
//!
//! The root complex is registered as one top-level runtime device that owns
//! both ECAM and the complete PCI memory aperture. Guest-programmable BAR
//! decode remains inside that root so config state and MMIO routing cannot
//! diverge after the internal root-complex device is registered in a sealed runtime.
//!
//! [`PciBusGraphBuilder`] derives the host apertures and frozen PCI topology
//! from the same resolved device graph used for runtime claims. Endpoint BAR
//! handlers are constructed later and held by bundle-owned binding leases.

mod address;
mod bar;
mod config;
mod ecam;
mod error;
mod function;
mod graph;
mod topology;

pub(crate) const FOUR_GIB: u64 = 1 << 32;

pub use address::{ConfigOffset, PciBarIndex, PciBdf, PciSegment};
pub use bar::{PciMemoryBar, PciMemoryBarWidth};
pub use config::PciCapability;
pub use ecam::PciHostBridgeConfig;
pub use error::{PciError, PciResult};
pub use function::{PciBarAccess, PciClass, PciEndpointIdentity, PciFunction, PciFunctionSpec};
pub use graph::{
    PciBusGraphBuilder, PciEndpointBundle, PciEndpointModel, PciHostResourceRequirements,
    ResolvedPciBus,
};
pub use topology::{PciTopologyBuilder, ResolvedPciBar, ResolvedPciFunction, ResolvedPciTopology};
