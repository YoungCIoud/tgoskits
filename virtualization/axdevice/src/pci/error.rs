//! Structured virtual PCI construction failures.

use alloc::string::String;

use super::{PciBarIndex, PciBdf};
use crate::AccessWidth;

/// Result returned by virtual PCI topology and configuration operations.
pub type PciResult<T = ()> = Result<T, PciError>;

/// A virtual PCI address, descriptor, or topology is invalid.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PciError {
    /// One numeric PCI address component is outside its architectural range.
    #[error("PCI {component} value {value:#x} is outside the supported range")]
    InvalidAddress {
        /// Address component being validated.
        component: &'static str,
        /// Rejected numeric value.
        value: u64,
    },
    /// A Type-0 identity would be interpreted as an absent function.
    #[error("invalid PCI endpoint identity: {detail}")]
    InvalidEndpointIdentity {
        /// Diagnostic reason.
        detail: &'static str,
    },
    /// A config-space access violates width, alignment, or range rules.
    #[error("invalid PCI config access at {offset:#x} with width {width:?}: {detail}")]
    InvalidConfigAccess {
        /// Function-relative config-space offset.
        offset: u16,
        /// Requested access width.
        width: AccessWidth,
        /// Diagnostic reason.
        detail: &'static str,
    },
    /// The root-complex ECAM or memory aperture is malformed.
    #[error("invalid PCI host aperture: {detail}")]
    InvalidHostAperture {
        /// Diagnostic reason.
        detail: &'static str,
    },
    /// A graph-backed host was built before its topology was resolved.
    #[error("PCI topology has not been resolved from the device graph")]
    TopologyNotResolved,
    /// One graph-backed PCI bus attempted to publish a second topology.
    #[error("PCI topology is already resolved for this graph bus")]
    TopologyAlreadyResolved,
    /// A graph-backed bus attempted to publish a second live root complex.
    #[error("PCI root complex already has a live graph registration")]
    HostAlreadyRegistered,
    /// An endpoint was built before its host root complex was registered.
    #[error("PCI root complex is unavailable while binding function {function}")]
    HostUnavailable {
        /// Function waiting for its host dependency.
        function: String,
    },
    /// A runtime binding names no function in the resolved topology.
    #[error("PCI function {function} is absent from the resolved topology")]
    FunctionNotFound {
        /// Missing stable function identity.
        function: String,
    },
    /// A runtime function already has a live binding.
    #[error("PCI function {function} already has a runtime binding")]
    FunctionAlreadyBound {
        /// Duplicate stable function identity.
        function: String,
    },
    /// The per-root binding generation cannot be advanced safely.
    #[error("PCI function binding generation is exhausted")]
    BindingGenerationExhausted,
    /// One function identity was declared more than once.
    #[error("PCI function identity {function} is declared more than once")]
    DuplicateFunction {
        /// Stable function identity.
        function: String,
    },
    /// Two functions request the same BDF.
    #[error("PCI BDF {bdf} is requested by both {first} and {second}")]
    DuplicateBdf {
        /// Conflicting BDF.
        bdf: PciBdf,
        /// First stable function identity.
        first: String,
        /// Second stable function identity.
        second: String,
    },
    /// A non-zero function has no function zero at the same device.
    #[error("PCI function {bdf} has no function zero at the same device")]
    MissingFunctionZero {
        /// Orphan BDF.
        bdf: PciBdf,
    },
    /// No BDF remains in the supported segment and bus.
    #[error("PCI bus 0000:00 has no free function for {function}")]
    BdfExhausted {
        /// Stable function identity that could not be placed.
        function: String,
    },
    /// A BAR descriptor is malformed or conflicts with another slot.
    #[error("invalid PCI BAR{bar}: {detail}")]
    InvalidBar {
        /// BAR index.
        bar: PciBarIndex,
        /// Diagnostic reason.
        detail: String,
    },
    /// A fixed BAR overlaps an already resolved BAR.
    #[error(
        "PCI BAR range [{start:#x}, {end:#x}) for {function} BAR{bar} conflicts with another BAR"
    )]
    BarConflict {
        /// Stable function identity.
        function: String,
        /// BAR index.
        bar: PciBarIndex,
        /// Start of the rejected range.
        start: u64,
        /// End of the rejected range.
        end: u64,
    },
    /// The PCI memory aperture cannot fit a BAR.
    #[error("PCI memory aperture cannot place {function} BAR{bar} with size {size:#x}")]
    BarApertureExhausted {
        /// Stable function identity.
        function: String,
        /// BAR index.
        bar: PciBarIndex,
        /// Required BAR size.
        size: u64,
    },
    /// A standard capability descriptor or layout is malformed.
    #[error("invalid PCI capability {id:#x}: {detail}")]
    InvalidCapability {
        /// Capability identifier.
        id: u8,
        /// Diagnostic reason.
        detail: String,
    },
}
