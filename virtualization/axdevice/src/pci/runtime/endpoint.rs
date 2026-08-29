use alloc::{format, sync::Arc};

use axdevice_base::{
    Device, DeviceContext, DeviceError, DeviceId, DeviceResult, IrqLine, RoutedDeviceGrant,
};

use super::{
    super::{
        PciBarIndex, PciBarRoute, PciBdf, PciCapabilityId, PciCapabilitySnapshot, PciConfigEffectId,
    },
    routing::EndpointAdmission,
};
use crate::AccessWidth;

/// Metadata passed to one endpoint BAR callback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciBarAccess {
    route: PciBarRoute,
    dma_enabled: bool,
}

impl PciBarAccess {
    pub(crate) const fn new(route: PciBarRoute, dma_enabled: bool) -> Self {
        Self { route, dma_enabled }
    }

    /// Returns the root-captured bus-master-enable state for this access.
    pub const fn bus_master_enable(self) -> bool {
        self.dma_enabled
    }
}

/// Immutable command-register state captured for one endpoint notification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciCommandState {
    memory_space_enable: bool,
    bus_master_enable: bool,
    interrupt_disable: bool,
}

impl PciCommandState {
    pub(crate) const fn new(
        memory_space_enable: bool,
        bus_master_enable: bool,
        interrupt_disable: bool,
    ) -> Self {
        Self {
            memory_space_enable,
            bus_master_enable,
            interrupt_disable,
        }
    }

    /// Returns whether the PCI function's memory BAR decode is enabled.
    pub const fn memory_space_enable(self) -> bool {
        self.memory_space_enable
    }

    /// Returns whether the PCI function may initiate bus-master DMA.
    pub const fn bus_master_enable(self) -> bool {
        self.bus_master_enable
    }

    /// Returns whether legacy INTx delivery is disabled.
    pub const fn interrupt_disable(self) -> bool {
        self.interrupt_disable
    }
}

/// Snapshot of one endpoint configuration read effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciConfigReadEffect {
    capability: PciCapabilityId,
    effect: PciConfigEffectId,
    offset: u8,
    width: AccessWidth,
    capability_snapshot: PciCapabilitySnapshot,
    bus_master_enable: bool,
}

impl PciConfigReadEffect {
    pub(crate) const fn new(
        capability: PciCapabilityId,
        effect: PciConfigEffectId,
        offset: u8,
        width: AccessWidth,
        capability_snapshot: PciCapabilitySnapshot,
        bus_master_enable: bool,
    ) -> Self {
        Self {
            capability,
            effect,
            offset,
            width,
            capability_snapshot,
            bus_master_enable,
        }
    }

    /// Returns the capability containing this effect.
    pub const fn capability(self) -> PciCapabilityId {
        self.capability
    }

    /// Returns the effect identifier.
    pub const fn effect(self) -> PciConfigEffectId {
        self.effect
    }

    /// Returns the capability-relative access offset.
    pub const fn offset(self) -> u8 {
        self.offset
    }

    /// Returns the complete access width.
    pub const fn width(self) -> AccessWidth {
        self.width
    }

    /// Returns the root-time capability body snapshot for this access.
    pub const fn capability_snapshot(self) -> PciCapabilitySnapshot {
        self.capability_snapshot
    }

    /// Returns the root-captured BME state for this config effect.
    pub const fn bus_master_enable(self) -> bool {
        self.bus_master_enable
    }
}

/// Snapshot of one endpoint configuration write effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciConfigWriteEffect {
    read: PciConfigReadEffect,
    value: u64,
}

impl PciConfigWriteEffect {
    pub(crate) const fn new(read: PciConfigReadEffect, value: u64) -> Self {
        Self { read, value }
    }

    /// Returns the capability containing this effect.
    pub const fn capability(self) -> PciCapabilityId {
        self.read.capability()
    }

    /// Returns the effect identifier.
    pub const fn effect(self) -> PciConfigEffectId {
        self.read.effect()
    }

    /// Returns the capability-relative access offset.
    pub const fn offset(self) -> u8 {
        self.read.offset()
    }

    /// Returns the complete access width.
    pub const fn width(self) -> AccessWidth {
        self.read.width()
    }

    /// Returns the root-time capability body snapshot for this access.
    pub const fn capability_snapshot(self) -> PciCapabilitySnapshot {
        self.read.capability_snapshot()
    }

    /// Returns the root-captured BME state for this config effect.
    pub const fn bus_master_enable(self) -> bool {
        self.read.bus_master_enable()
    }

    /// Returns the guest-provided write value.
    pub const fn value(self) -> u64 {
        self.value
    }
}

impl PciBarAccess {
    /// Returns the selected function BDF.
    pub const fn bdf(self) -> PciBdf {
        self.route.bdf()
    }
    /// Returns the selected BAR slot.
    pub const fn bar(self) -> PciBarIndex {
        self.route.bar()
    }
    /// Returns the BAR-relative byte offset.
    pub const fn offset(self) -> u64 {
        self.route.offset()
    }
    /// Returns the complete access width.
    pub const fn width(self) -> AccessWidth {
        self.route.width()
    }
}

/// Permit held while an endpoint publishes one interrupt-line transition.
///
/// The permit exposes only the line operations needed by an admitted callback;
/// the admission gate remains private and the endpoint cannot retain the
/// permit beyond the callback that acquired it.
pub struct EndpointIrqTransitionPermit {
    pub(super) _private: (),
}

impl EndpointIrqTransitionPermit {
    /// Asserts one endpoint-owned level-triggered source.
    pub fn assert(&mut self, line: &IrqLine) -> DeviceResult {
        line.assert().map_err(|error| DeviceError::Backend {
            operation: "assert PCI endpoint INTx line",
            detail: format!("{error}"),
        })
    }

    /// Deasserts one endpoint-owned level-triggered source.
    pub fn deassert(&mut self, line: &IrqLine) -> DeviceResult {
        line.deassert().map_err(|error| DeviceError::Backend {
            operation: "deassert PCI endpoint INTx line",
            detail: format!("{error}"),
        })
    }
}

/// Context supplied to endpoint-owned PCI callbacks.
pub trait PciEndpointContext: DeviceContext {
    /// Runs one interrupt publication transition while the endpoint binding
    /// remains admitted and the transition permit is held.
    fn with_irq_transition(
        &mut self,
        callback: &mut dyn FnMut(&mut EndpointIrqTransitionPermit) -> DeviceResult,
    ) -> DeviceResult;
}

pub(super) struct RoutedPciEndpointContext<'a> {
    pub(super) inner: &'a mut dyn DeviceContext,
    pub(super) admission: Arc<EndpointAdmission>,
}

impl DeviceContext for RoutedPciEndpointContext<'_> {
    fn device_id(&self) -> DeviceId {
        self.inner.device_id()
    }

    fn with_routed_device(
        &mut self,
        grant: &RoutedDeviceGrant,
        callback: &mut dyn FnMut(&mut dyn DeviceContext) -> DeviceResult,
    ) -> DeviceResult {
        self.inner.with_routed_device(grant, callback)
    }

    fn read_guest_memory(
        &mut self,
        grant: &axdevice_base::DmaGrant,
        addr: axvm_types::GuestPhysAddr,
        data: &mut [u8],
    ) -> DeviceResult {
        self.inner.read_guest_memory(grant, addr, data)
    }

    fn write_guest_memory(
        &mut self,
        grant: &axdevice_base::DmaGrant,
        addr: axvm_types::GuestPhysAddr,
        data: &[u8],
    ) -> DeviceResult {
        self.inner.write_guest_memory(grant, addr, data)
    }

    fn schedule_timer(
        &mut self,
        grant: &axdevice_base::TimerGrant,
        deadline_ns: u64,
    ) -> DeviceResult {
        self.inner.schedule_timer(grant, deadline_ns)
    }

    fn wake_vcpu(&mut self, grant: &axdevice_base::WakeGrant, vcpu_id: usize) -> DeviceResult {
        self.inner.wake_vcpu(grant, vcpu_id)
    }

    fn request_vm_stop(&mut self, grant: &axdevice_base::StopGrant, reason: &str) -> DeviceResult {
        self.inner.request_vm_stop(grant, reason)
    }
}

impl PciEndpointContext for RoutedPciEndpointContext<'_> {
    fn with_irq_transition(
        &mut self,
        callback: &mut dyn FnMut(&mut EndpointIrqTransitionPermit) -> DeviceResult,
    ) -> DeviceResult {
        let _permit = self.admission.acquire_irq_permit()?;
        let mut permit = EndpointIrqTransitionPermit { _private: () };
        callback(&mut permit)
    }
}

pub(super) struct LegacyPciEndpointContext {
    pub(super) device_id: DeviceId,
    pub(super) admission: Arc<EndpointAdmission>,
}

impl DeviceContext for LegacyPciEndpointContext {
    fn device_id(&self) -> DeviceId {
        self.device_id
    }
}

impl PciEndpointContext for LegacyPciEndpointContext {
    fn with_irq_transition(
        &mut self,
        callback: &mut dyn FnMut(&mut EndpointIrqTransitionPermit) -> DeviceResult,
    ) -> DeviceResult {
        let _permit = self.admission.acquire_irq_permit()?;
        let mut permit = EndpointIrqTransitionPermit { _private: () };
        callback(&mut permit)
    }
}

pub(super) struct OwnerPciEndpointContext {
    pub(super) device_id: DeviceId,
}

impl DeviceContext for OwnerPciEndpointContext {
    fn device_id(&self) -> DeviceId {
        self.device_id
    }
}

impl PciEndpointContext for OwnerPciEndpointContext {
    fn with_irq_transition(
        &mut self,
        callback: &mut dyn FnMut(&mut EndpointIrqTransitionPermit) -> DeviceResult,
    ) -> DeviceResult {
        // Registration has not published a guest-visible route yet. This is
        // binding-owner authority, so no routed admission permit is needed.
        let mut permit = EndpointIrqTransitionPermit { _private: () };
        callback(&mut permit)
    }
}

/// Endpoint-owned behavior reached after authenticated PCI routing.
///
/// # Device context contract
///
/// Callbacks run strictly outside the root lock. The runtime first validates
/// the route and enters the endpoint through [`DeviceContext::with_routed_device`].
/// The route grant is not a substitute for endpoint DMA registration: guest
/// memory still requires the endpoint's matching [`DmaGrant`].
pub trait PciFunction: Device {
    /// Returns whether endpoint-owned interrupt state is pending.
    fn intx_pending(&self) -> bool {
        false
    }

    /// Returns the endpoint-owned config effects implemented by this function.
    ///
    /// The runtime compares this list with the effect IDs declared in the
    /// resolved PCI capabilities before publishing a route. An empty default
    /// keeps functions without endpoint config effects source-compatible.
    fn supported_config_effects(&self) -> &[PciConfigEffectId] {
        &[]
    }

    /// Reads one complete memory BAR access.
    fn read_bar(
        &self,
        access: PciBarAccess,
        context: &mut dyn PciEndpointContext,
    ) -> DeviceResult<u64>;
    /// Writes one complete memory BAR access.
    fn write_bar(
        &self,
        access: PciBarAccess,
        value: u64,
        context: &mut dyn PciEndpointContext,
    ) -> DeviceResult;

    /// Handles one endpoint-owned conventional config read effect.
    fn read_config_effect(
        &self,
        _effect: PciConfigReadEffect,
        _context: &mut dyn PciEndpointContext,
    ) -> DeviceResult<u64> {
        Err(DeviceError::Unsupported {
            operation: "read PCI config effect",
            detail: "the endpoint does not implement this config effect".into(),
        })
    }

    /// Handles one endpoint-owned conventional config write effect.
    fn write_config_effect(
        &self,
        _effect: PciConfigWriteEffect,
        _context: &mut dyn PciEndpointContext,
    ) -> DeviceResult {
        Err(DeviceError::Unsupported {
            operation: "write PCI config effect",
            detail: "the endpoint does not implement this config effect".into(),
        })
    }

    /// Observes a root-owned command-register transition.
    ///
    /// This callback runs without the root state lock or the binding
    /// lifecycle operation lock held. An implementation may re-enter the
    /// root or binding APIs; an operation that conflicts with the current
    /// lifecycle phase returns a typed busy/invalid-state error.
    fn command_changed(
        &self,
        _command: PciCommandState,
        _context: &mut dyn PciEndpointContext,
    ) -> DeviceResult {
        Ok(())
    }

    /// Resets endpoint-owned state after the PCI root has restored its
    /// power-on configuration.
    ///
    /// Endpoints that participate in full lifecycle reset must override this
    /// method. Returning `Unsupported` by default prevents the runtime from
    /// reopening a binding whose endpoint state was not reset.
    ///
    /// This callback runs while the binding's lifecycle owner gate is held,
    /// after old route admissions have been closed and drained. An
    /// implementation must therefore be bounded and non-blocking: it must
    /// not wait for another runtime operation, re-enter binding or route
    /// publication, acquire the lifecycle gate again, or call back into the
    /// root binding. It may only reset state owned by this endpoint.
    fn reset(&self, _command: PciCommandState) -> DeviceResult {
        Err(DeviceError::Unsupported {
            operation: "reset PCI endpoint",
            detail: "the endpoint does not implement lifecycle reset".into(),
        })
    }

    /// Withdraws the endpoint-owned IRQ source during binding teardown or
    /// lifecycle reset.
    ///
    /// The runtime invokes this only after the root route is withdrawn or
    /// reset, new IRQ permits are closed, and previously acquired permits are
    /// drained. An INTx endpoint should deassert its owned [`IrqLine`] through
    /// the supplied transition permit. The operation must be idempotent so a
    /// failed reset can be followed by final teardown. The default is
    /// suitable for functions without an interrupt source.
    fn withdraw_irq(&self, _permit: &mut EndpointIrqTransitionPermit) -> DeviceResult {
        Ok(())
    }
}
