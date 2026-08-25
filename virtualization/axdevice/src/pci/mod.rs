//! Architecture-neutral conventional PCI topology and root state.
//!
//! This module owns Type-0 function identities, deterministic BDF and
//! 32-bit memory-BAR placement, the 256-byte conventional config image, and
//! mutable root-owned config/BAR decode state. Architecture frontends and
//! runtime endpoint binding are separate layers.

mod address;
mod bar;
mod config;
mod error;
mod function;
mod placement;
mod root;
mod topology;

pub(crate) const FOUR_GIB: u64 = 1 << 32;

pub use address::{ConfigOffset, PciBarIndex, PciBdf, PciSegment};
pub use bar::PciMemoryBar;
pub use error::{PciError, PciResult};
pub use function::{PciClass, PciEndpointIdentity, PciFunctionSpec};
pub use root::{PciBarRoute, PciRootState};
pub use topology::{PciTopologyBuilder, ResolvedPciBar, ResolvedPciFunction, ResolvedPciTopology};
