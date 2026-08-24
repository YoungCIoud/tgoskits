//! Architecture-neutral virtual PCI Type-0 and ECAM foundation.
//!
//! The PCI root state (`root`) owns function config images, BAR decode,
//! bindings, and reset. Config frontends are separate runtime devices: the
//! ECAM device decodes its own window, and the memory-aperture device owns
//! the whole aperture. Both delegate already-typed accesses to one shared
//! root state, so config state and MMIO routing cannot diverge after the
//! frontends are registered in a sealed runtime.
//!
//! [`PciBusGraphBuilder`] derives the host windows and frozen PCI topology
//! from the same resolved device graph used for runtime claims. Endpoint BAR
//! handlers are constructed later and held by bundle-owned binding leases.

mod address;
mod bar;
mod config;
mod ecam;
mod error;
mod function;
mod graph;
mod memory;
mod root;
mod topology;

pub(crate) const FOUR_GIB: u64 = 1 << 32;

pub use address::{ConfigOffset, PciBarIndex, PciBdf, PciSegment};
pub use bar::{PciMemoryBar, PciMemoryBarWidth};
pub use config::PciCapability;
pub use ecam::validate_host_windows;
pub use error::{PciError, PciResult};
pub use function::{PciBarAccess, PciClass, PciEndpointIdentity, PciFunction, PciFunctionSpec};
pub use graph::{
    PciBusGraphBuilder, PciEndpointBundle, PciEndpointModel, PciHostResourceRequirements,
    ResolvedPciBus,
};
pub use topology::{PciTopologyBuilder, ResolvedPciBar, ResolvedPciFunction, ResolvedPciTopology};
