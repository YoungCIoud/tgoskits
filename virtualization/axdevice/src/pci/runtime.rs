//! Runtime-authenticated PCI endpoint binding and BAR dispatch.

use alloc::{collections::BTreeMap, format, string::ToString, sync::Arc, vec::Vec};
use core::fmt;

use ax_sync::SpinLock;
use axdevice_base::{
    Device, DeviceContext, DeviceError, DeviceId, DeviceResult, IrqLine,
    RoutedAdmissionEpoch as RoutedGrantAdmissionEpoch, RoutedBindingGeneration, RoutedDeviceGrant,
};

use super::{
    PciBarIndex, PciBarRoute, PciBdf, PciCapabilityId, PciCapabilitySnapshot, PciConfigEffectId,
    PciError, PciRootState,
    root::{PciConfigReadOutcome, PciConfigWriteOutcome},
};
use crate::{
    AccessWidth, DeviceManagerError, DeviceManagerResult, DeviceNodeId, ServiceCardinality,
    ServiceKey,
};

const DEFAULT_DRAIN_ATTEMPTS: usize = 1_000_000;

// A root binding may be dropped while an endpoint backend is temporarily
// unable to withdraw its IRQ. Keep that owner in a process-lifetime,
// fail-closed queue so dropping the root cannot drop an asserted line.
static ORPHANED_IRQ_WITHDRAWALS: SpinLock<Vec<PendingIrqWithdrawal>> = SpinLock::new(Vec::new());

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
}

impl PciConfigReadEffect {
    pub(crate) const fn new(
        capability: PciCapabilityId,
        effect: PciConfigEffectId,
        offset: u8,
        width: AccessWidth,
        capability_snapshot: PciCapabilitySnapshot,
    ) -> Self {
        Self {
            capability,
            effect,
            offset,
            width,
            capability_snapshot,
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
    _private: (),
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

struct RoutedPciEndpointContext<'a> {
    inner: &'a mut dyn DeviceContext,
    admission: Arc<EndpointAdmission>,
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

struct LegacyPciEndpointContext {
    device_id: DeviceId,
    admission: Arc<EndpointAdmission>,
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

struct AdmissionState {
    open: bool,
    leases: usize,
    permits: usize,
}

struct EndpointAdmission {
    generation: EndpointBindingGeneration,
    epoch: RoutedAdmissionEpoch,
    state: SpinLock<AdmissionState>,
}

impl EndpointAdmission {
    fn new(generation: EndpointBindingGeneration, epoch: RoutedAdmissionEpoch) -> Self {
        Self {
            generation,
            epoch,
            state: SpinLock::new(AdmissionState {
                open: true,
                leases: 0,
                permits: 0,
            }),
        }
    }

    fn acquire(self: &Arc<Self>, token: &EndpointRouteToken) -> DeviceResult<AdmissionLease> {
        let mut state = self.state.lock_irqsave();
        if !state.open
            || token.binding_generation != self.generation
            || token.admission_epoch != self.epoch
        {
            return Err(DeviceError::InvalidState {
                operation: "admit PCI endpoint route",
                detail: "PCI endpoint route admission is closed or stale".into(),
            });
        }
        state.leases = state
            .leases
            .checked_add(1)
            .ok_or(DeviceError::InvalidState {
                operation: "admit PCI endpoint route",
                detail: "PCI endpoint route lease count is exhausted".into(),
            })?;
        Ok(AdmissionLease {
            admission: self.clone(),
        })
    }

    fn acquire_irq_permit(self: &Arc<Self>) -> DeviceResult<IrqPermitLease> {
        let mut state = self.state.lock_irqsave();
        if !state.open {
            return Err(DeviceError::InvalidState {
                operation: "publish PCI endpoint interrupt state",
                detail: "PCI endpoint route admission is closed".into(),
            });
        }
        state.permits = state
            .permits
            .checked_add(1)
            .ok_or(DeviceError::InvalidState {
                operation: "publish PCI endpoint interrupt state",
                detail: "PCI interrupt transition permit count is exhausted".into(),
            })?;
        Ok(IrqPermitLease {
            admission: self.clone(),
        })
    }

    fn close(&self) {
        self.state.lock_irqsave().open = false;
    }

    fn wait_for_irq_permits(&self) -> DeviceManagerResult {
        self.wait_for_irq_permits_with_budget(DEFAULT_DRAIN_ATTEMPTS)
    }

    fn wait_for_irq_permits_with_budget(&self, attempts: usize) -> DeviceManagerResult {
        for _ in 0..=attempts {
            if self.state.lock_irqsave().permits == 0 {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(DeviceManagerError::InvalidState {
            operation: "drain PCI endpoint IRQ permits",
            detail: "PCI endpoint IRQ permit drain exceeded its bounded wait budget".into(),
        })
    }

    fn wait_for_idle(&self) -> DeviceManagerResult {
        self.wait_for_idle_with_budget(DEFAULT_DRAIN_ATTEMPTS)
    }

    fn wait_for_idle_with_budget(&self, attempts: usize) -> DeviceManagerResult {
        for _ in 0..=attempts {
            let idle = {
                let state = self.state.lock_irqsave();
                state.leases == 0 && state.permits == 0
            };
            if idle {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(DeviceManagerError::InvalidState {
            operation: "drain PCI endpoint route leases",
            detail: "PCI endpoint route drain exceeded its bounded wait budget".into(),
        })
    }

    fn open(&self) {
        self.state.lock_irqsave().open = true;
    }
}

struct AdmissionLease {
    admission: Arc<EndpointAdmission>,
}

impl Drop for AdmissionLease {
    fn drop(&mut self) {
        self.admission.state.lock_irqsave().leases -= 1;
    }
}

struct IrqPermitLease {
    admission: Arc<EndpointAdmission>,
}

impl Drop for IrqPermitLease {
    fn drop(&mut self) {
        self.admission.state.lock_irqsave().permits -= 1;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EndpointBindingGeneration(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RoutedAdmissionEpoch(u64);

/// Non-capability token identifying one active endpoint binding generation and
/// routed admission epoch.
#[derive(Clone)]
pub struct EndpointRouteToken {
    device: DeviceId,
    binding_generation: EndpointBindingGeneration,
    admission_epoch: RoutedAdmissionEpoch,
    admission: Arc<EndpointAdmission>,
    grant: RoutedDeviceGrant,
}

impl EndpointRouteToken {
    /// Returns the final device selected by this route.
    pub const fn device_id(&self) -> DeviceId {
        self.device
    }

    /// Returns the binding generation selected by this route.
    pub const fn binding_generation(&self) -> u64 {
        self.binding_generation.0
    }

    /// Returns the admission epoch selected by this route.
    pub const fn admission_epoch(&self) -> u64 {
        self.admission_epoch.0
    }

    fn grant(&self, dma_enabled: bool) -> RoutedDeviceGrant {
        self.grant.with_dma_enabled(dma_enabled)
    }

    pub(crate) fn snapshot_if_admitted(&self) -> Option<Self> {
        let state = self.admission.state.lock_irqsave();
        state.open.then(|| self.clone())
    }
}

impl PartialEq for EndpointRouteToken {
    fn eq(&self, other: &Self) -> bool {
        self.device == other.device
            && self.binding_generation == other.binding_generation
            && self.admission_epoch == other.admission_epoch
            && Arc::ptr_eq(&self.admission, &other.admission)
    }
}

impl Eq for EndpointRouteToken {}

impl fmt::Debug for EndpointRouteToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EndpointRouteToken")
            .field("device", &self.device)
            .field("binding_generation", &self.binding_generation())
            .field("admission_epoch", &self.admission_epoch())
            .finish_non_exhaustive()
    }
}

struct RoutedEndpointLease {
    endpoint: Arc<dyn PciFunction>,
    _admission: Arc<EndpointAdmission>,
    _lease: AdmissionLease,
    grant: RoutedDeviceGrant,
}

struct RoutedEndpoint {
    token: EndpointRouteToken,
    function: Arc<dyn PciFunction>,
}

#[derive(Default)]
struct EndpointRouterState {
    next_generation: u64,
    endpoints: BTreeMap<DeviceId, RoutedEndpoint>,
}

struct EndpointRouter {
    state: SpinLock<EndpointRouterState>,
}

impl EndpointRouter {
    fn new() -> Self {
        Self {
            state: SpinLock::new(EndpointRouterState::default()),
        }
    }

    fn activate(
        &self,
        device: DeviceId,
        function: Arc<dyn PciFunction>,
    ) -> DeviceManagerResult<EndpointRouteToken> {
        let mut state = self.state.lock_irqsave();
        if state.endpoints.contains_key(&device) {
            return Err(DeviceManagerError::ResourceConflict {
                operation: "bind PCI endpoint route",
                detail: alloc::format!(
                    "device {} already has an active PCI route",
                    device.as_u32()
                ),
            });
        }
        state.next_generation = state.next_generation.checked_add(1).ok_or_else(|| {
            DeviceManagerError::InvalidState {
                operation: "bind PCI endpoint route",
                detail: "PCI binding generation is exhausted".into(),
            }
        })?;
        let generation = EndpointBindingGeneration(state.next_generation);
        let epoch = RoutedAdmissionEpoch(1);
        let admission = Arc::new(EndpointAdmission::new(generation, epoch));
        let grant = RoutedDeviceGrant::new(
            device,
            RoutedBindingGeneration::new(state.next_generation),
            RoutedGrantAdmissionEpoch::new(1),
            false,
        );
        let token = EndpointRouteToken {
            device,
            binding_generation: generation,
            admission_epoch: epoch,
            admission: admission.clone(),
            grant,
        };
        state.endpoints.insert(
            device,
            RoutedEndpoint {
                token: token.clone(),
                function,
            },
        );
        Ok(token)
    }

    fn invalidate(&self, token: &EndpointRouteToken) -> Option<Arc<dyn PciFunction>> {
        let mut state = self.state.lock_irqsave();
        if state
            .endpoints
            .get(&token.device)
            .is_some_and(|entry| entry.token == *token)
        {
            let entry = state.endpoints.remove(&token.device)?;
            entry.token.admission.close();
            entry.token.grant.close_admission();
            return Some(entry.function);
        }
        None
    }

    fn invalidate_device(
        &self,
        device: DeviceId,
    ) -> Option<(Arc<dyn PciFunction>, Arc<EndpointAdmission>)> {
        let mut state = self.state.lock_irqsave();
        let entry = state.endpoints.remove(&device)?;
        entry.token.admission.close();
        entry.token.grant.close_admission();
        Some((entry.function, entry.token.admission))
    }

    fn endpoint(&self, token: &EndpointRouteToken) -> DeviceResult<Arc<dyn PciFunction>> {
        let state = self.state.lock_irqsave();
        state
            .endpoints
            .get(&token.device)
            .filter(|entry| entry.token == *token)
            .map(|entry| entry.function.clone())
            .ok_or_else(|| DeviceError::InvalidState {
                operation: "dispatch PCI endpoint route",
                detail: "PCI endpoint route token is stale".into(),
            })
    }

    fn lease(
        &self,
        token: &EndpointRouteToken,
        dma_enabled: bool,
    ) -> DeviceResult<RoutedEndpointLease> {
        let endpoint = self.endpoint(token)?;
        let lease = token.admission.clone().acquire(token)?;
        Ok(RoutedEndpointLease {
            endpoint,
            _admission: token.admission.clone(),
            _lease: lease,
            grant: token.grant(dma_enabled),
        })
    }

    fn reset_endpoints(&self, commands: &[(DeviceId, PciCommandState)]) -> DeviceManagerResult {
        let endpoints = {
            let state = self.state.lock_irqsave();
            if commands.len() != state.endpoints.len() {
                return Err(DeviceManagerError::InvalidState {
                    operation: "reset PCI endpoints",
                    detail: "PCI root and endpoint route sets are inconsistent".into(),
                });
            }
            commands
                .iter()
                .filter_map(|(device, command)| {
                    state
                        .endpoints
                        .get(device)
                        .map(|endpoint| (endpoint.function.clone(), *command))
                })
                .collect::<Vec<_>>()
        };
        let mut first_error = None;
        for (endpoint, command) in endpoints {
            if let Err(error) = endpoint.reset(command).map_err(DeviceManagerError::Device)
                && first_error.is_none()
            {
                first_error = Some(error);
            }

            // The fresh route admission is intentionally still closed here.
            // The lifecycle owner has drained the old admission, so this
            // owner-side transition is authorized directly by the reset
            // phase's permit rather than by a routed callback permit.
            let mut permit = EndpointIrqTransitionPermit { _private: () };
            if let Err(error) = endpoint
                .withdraw_irq(&mut permit)
                .map_err(DeviceManagerError::Device)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn reset_admissions(
        &self,
    ) -> DeviceManagerResult<Vec<(EndpointRouteToken, EndpointRouteToken)>> {
        let (replacements, old_admissions) = {
            let mut state = self.state.lock_irqsave();
            for endpoint in state.endpoints.values() {
                if endpoint.token.admission_epoch() == u64::MAX {
                    return Err(DeviceManagerError::InvalidState {
                        operation: "reset PCI endpoint route admission",
                        detail: "PCI route admission epoch is exhausted".into(),
                    });
                }
            }
            let mut replacements = Vec::with_capacity(state.endpoints.len());
            let mut old_admissions = Vec::with_capacity(state.endpoints.len());
            for endpoint in state.endpoints.values_mut() {
                let old = endpoint.token.clone();
                let epoch = old.admission_epoch() + 1;
                old.admission.close();
                old.grant.close_admission();
                old_admissions.push(old.admission.clone());
                let admission = Arc::new(EndpointAdmission::new(
                    old.binding_generation,
                    RoutedAdmissionEpoch(epoch),
                ));
                admission.close();
                let token = EndpointRouteToken {
                    device: old.device,
                    binding_generation: old.binding_generation,
                    admission_epoch: RoutedAdmissionEpoch(epoch),
                    admission,
                    grant: old
                        .grant
                        .with_admission_epoch(RoutedGrantAdmissionEpoch::new(epoch)),
                };
                endpoint.token = token.clone();
                replacements.push((old, token));
            }
            (replacements, old_admissions)
        };
        for admission in old_admissions {
            admission.wait_for_idle()?;
        }
        Ok(replacements)
    }

    fn close_admissions_and_drain(&self) -> DeviceManagerResult {
        let admissions = {
            let state = self.state.lock_irqsave();
            for endpoint in state.endpoints.values() {
                endpoint.token.admission.close();
                endpoint.token.grant.close_admission();
            }
            state
                .endpoints
                .values()
                .map(|endpoint| endpoint.token.admission.clone())
                .collect::<Vec<_>>()
        };
        for admission in admissions {
            admission.wait_for_idle()?;
        }
        Ok(())
    }

    fn invalidate_all(&self) -> (Vec<PendingIrqWithdrawal>, DeviceManagerResult) {
        let pending = {
            let mut state = self.state.lock_irqsave();
            let pending = state
                .endpoints
                .values()
                .map(|endpoint| {
                    endpoint.token.admission.close();
                    endpoint.token.grant.close_admission();
                    PendingIrqWithdrawal {
                        device: endpoint.token.device_id(),
                        function: endpoint.function.clone(),
                        admission: endpoint.token.admission.clone(),
                    }
                })
                .collect::<Vec<_>>();
            state.endpoints.clear();
            pending
        };
        let mut first_error = None;
        for withdrawal in &pending {
            if let Err(error) = withdrawal.admission.wait_for_irq_permits()
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        (pending, first_error.map_or(Ok(()), Err))
    }

    fn open_admissions(&self) {
        let state = self.state.lock_irqsave();
        for endpoint in state.endpoints.values() {
            endpoint.token.admission.open();
            endpoint
                .token
                .grant
                .reopen_admission(RoutedGrantAdmissionEpoch::new(
                    endpoint.token.admission_epoch(),
                ));
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BindingLifecycleState {
    Running,
    Resetting,
    ResetFailed,
    Stopping,
    Dead,
}

/// Host-owned root binding published as a typed bundle service.
pub struct PciRootBinding {
    host: DeviceNodeId,
    root: Arc<PciRootState>,
    router: Arc<EndpointRouter>,
    lifecycle: SpinLock<BindingLifecycleState>,
    pending_irq_withdrawals: SpinLock<Vec<PendingIrqWithdrawal>>,
}

struct PendingIrqWithdrawal {
    device: DeviceId,
    function: Arc<dyn PciFunction>,
    admission: Arc<EndpointAdmission>,
}

impl PciRootBinding {
    /// Creates a binding service for one resolved host root.
    pub fn new(host: DeviceNodeId, root: Arc<PciRootState>) -> Self {
        Self {
            host,
            root,
            router: Arc::new(EndpointRouter::new()),
            lifecycle: SpinLock::new(BindingLifecycleState::Running),
            pending_irq_withdrawals: SpinLock::new(Vec::new()),
        }
    }

    /// Returns the host graph identity publishing this service.
    pub const fn host(&self) -> &DeviceNodeId {
        &self.host
    }

    /// Retries endpoint-owned IRQ withdrawals that could not complete during
    /// bounded binding teardown.
    ///
    /// A pending withdrawal keeps the endpoint owner and its closed admission
    /// alive. Rebinding the same device is rejected until this method drains
    /// the owner-side cleanup successfully.
    pub fn retry_irq_withdrawals(&self) -> DeviceManagerResult {
        let _lifecycle = self.lifecycle.lock_irqsave();
        self.retry_irq_withdrawals_locked()
    }

    /// Retries endpoint IRQ withdrawals orphaned by a previous root teardown.
    ///
    /// The orphan queue retains each endpoint owner and closed admission until
    /// this method succeeds. A failed retry leaves the entry fail-closed for a
    /// later owner or teardown supervisor to retry.
    pub fn retry_orphaned_irq_withdrawals() -> DeviceManagerResult {
        retry_pending_irq_withdrawals(&ORPHANED_IRQ_WITHDRAWALS)
    }

    fn retry_irq_withdrawals_locked(&self) -> DeviceManagerResult {
        retry_pending_irq_withdrawals(&self.pending_irq_withdrawals)
    }
}

fn retry_pending_irq_withdrawals(
    pending_storage: &SpinLock<Vec<PendingIrqWithdrawal>>,
) -> DeviceManagerResult {
    let pending = core::mem::take(&mut *pending_storage.lock_irqsave());
    let mut remaining = Vec::new();
    let mut first_error = None;
    for withdrawal in pending {
        let mut permit = EndpointIrqTransitionPermit { _private: () };
        let result = withdrawal.admission.wait_for_irq_permits().and_then(|()| {
            withdrawal
                .function
                .withdraw_irq(&mut permit)
                .map_err(DeviceManagerError::Device)
        });
        if let Err(error) = result {
            if first_error.is_none() {
                first_error = Some(error);
            }
            remaining.push(withdrawal);
        }
    }
    // A root teardown may transfer another owner while callbacks run. Merge
    // with the current queue instead of replacing it, preserving both the
    // retry results and owners arriving concurrently.
    pending_storage.lock_irqsave().extend(remaining);
    first_error.map_or(Ok(()), Err)
}

fn transfer_pending_irq_withdrawals(pending_storage: &SpinLock<Vec<PendingIrqWithdrawal>>) {
    let pending = core::mem::take(&mut *pending_storage.lock_irqsave());
    if pending.is_empty() {
        return;
    }
    ORPHANED_IRQ_WITHDRAWALS.lock_irqsave().extend(pending);
    warn!("PCI endpoint IRQ withdrawals transferred to the fail-closed orphan queue");
}

impl PciRootBinding {
    fn queue_irq_withdrawal(&self, withdrawal: PendingIrqWithdrawal) {
        self.pending_irq_withdrawals.lock_irqsave().push(withdrawal);
    }

    fn has_pending_irq_withdrawal(&self, device: DeviceId) -> bool {
        self.pending_irq_withdrawals
            .lock_irqsave()
            .iter()
            .any(|withdrawal| withdrawal.device == device)
    }

    pub(crate) fn matches_topology(&self, topology: &Arc<super::ResolvedPciTopology>) -> bool {
        Arc::ptr_eq(self.root.topology_arc(), topology)
    }

    pub(crate) fn reset_lifecycle(&self) -> DeviceManagerResult {
        let mut lifecycle = self.lifecycle.lock_irqsave();
        if *lifecycle != BindingLifecycleState::Running {
            return Err(DeviceManagerError::InvalidState {
                operation: "reset PCI root binding",
                detail: "PCI root binding is not running".into(),
            });
        }
        *lifecycle = BindingLifecycleState::Resetting;

        let result = self.reset_routes();
        if result.is_err()
            && let Err(error) = self.router.close_admissions_and_drain()
        {
            warn!("PCI reset failure cleanup could not drain routed endpoint activity: {error}");
        }
        *lifecycle = if result.is_ok() {
            BindingLifecycleState::Running
        } else {
            BindingLifecycleState::ResetFailed
        };
        result
    }

    fn reset_routes(&self) -> DeviceManagerResult {
        let replacements = self.router.reset_admissions()?;
        let commands = self.root.reset_and_snapshot_commands();
        self.router.reset_endpoints(&commands)?;
        self.root.replace_endpoint_tokens(&replacements);
        self.router.open_admissions();
        Ok(())
    }

    pub(crate) fn bind_registered(
        self: &Arc<Self>,
        function_id: &DeviceNodeId,
        device: DeviceId,
        function: Arc<dyn PciFunction>,
        routed_grants: &mut Vec<RoutedDeviceGrant>,
    ) -> DeviceManagerResult<PciBindingLease> {
        self.bind_registered_inner(function_id, device, function, Some(routed_grants))
    }

    fn bind_registered_inner(
        self: &Arc<Self>,
        function_id: &DeviceNodeId,
        device: DeviceId,
        function: Arc<dyn PciFunction>,
        mut routed_grants: Option<&mut Vec<RoutedDeviceGrant>>,
    ) -> DeviceManagerResult<PciBindingLease> {
        self.validate_config_effect_contract(function_id, function.as_ref())?;
        if !function.resources().is_empty() {
            return Err(DeviceManagerError::InvalidConfig {
                operation: "bind PCI endpoint route",
                detail: alloc::format!(
                    "endpoint {} must not publish ordinary device resources",
                    function_id
                ),
            });
        }
        // Binding publication is an owner-side lifecycle operation. Keep the
        // gate held from the state check through root publication so a full
        // reset cannot close and rotate the route between those steps.
        let _lifecycle = self.lifecycle.lock_irqsave();
        if *_lifecycle != BindingLifecycleState::Running {
            return Err(DeviceManagerError::InvalidState {
                operation: "bind PCI endpoint route",
                detail: "PCI root binding is not running".into(),
            });
        }
        if self.has_pending_irq_withdrawal(device) {
            return Err(DeviceManagerError::InvalidState {
                operation: "bind PCI endpoint route",
                detail: "the previous PCI endpoint IRQ withdrawal is still pending".into(),
            });
        }
        let token = self.router.activate(device, function)?;
        let registered = routed_grants.is_some();
        if let Some(grants) = routed_grants.as_deref_mut() {
            grants.push(token.grant(false));
        }
        if let Err(error) = self.root.bind_endpoint(function_id, token.clone()) {
            if registered && let Some(grants) = routed_grants {
                grants.pop();
            }
            drop(self.router.invalidate(&token));
            return Err(error.into());
        }
        Ok(PciBindingLease {
            binding: self.clone(),
            token,
        })
    }

    fn validate_config_effect_contract(
        &self,
        function_id: &DeviceNodeId,
        function: &dyn PciFunction,
    ) -> DeviceManagerResult {
        let resolved = self.root.topology().function(function_id).ok_or_else(|| {
            DeviceManagerError::Pci(PciError::UnknownFunction {
                function: function_id.to_string(),
            })
        })?;
        let supported = function.supported_config_effects();

        for (index, effect) in supported.iter().enumerate() {
            if supported[..index].contains(effect) {
                return Err(DeviceManagerError::InvalidConfig {
                    operation: "bind PCI endpoint route",
                    detail: alloc::format!(
                        "endpoint {} advertises duplicate PCI config effect {}",
                        function_id,
                        effect.value()
                    ),
                });
            }
            if !resolved.capabilities().any(|capability| {
                capability
                    .effects()
                    .iter()
                    .any(|declared| declared.effect() == *effect)
            }) {
                return Err(DeviceManagerError::InvalidConfig {
                    operation: "bind PCI endpoint route",
                    detail: alloc::format!(
                        "endpoint {} advertises undeclared PCI config effect {}",
                        function_id,
                        effect.value()
                    ),
                });
            }
        }

        for capability in resolved.capabilities() {
            for declared in capability.effects() {
                if !supported.contains(&declared.effect()) {
                    return Err(DeviceManagerError::InvalidConfig {
                        operation: "bind PCI endpoint route",
                        detail: alloc::format!(
                            "endpoint {} does not support declared PCI config effect {}",
                            function_id,
                            declared.effect().value()
                        ),
                    });
                }
            }
        }
        Ok(())
    }

    fn dispatch_legacy<T>(
        &self,
        token: &EndpointRouteToken,
        dma_enabled: bool,
        mut callback: impl FnMut(&Arc<dyn PciFunction>, &mut dyn PciEndpointContext) -> DeviceResult<T>,
    ) -> DeviceResult<T> {
        let lease = self.router.lease(token, dma_enabled)?;
        let mut context = LegacyPciEndpointContext {
            device_id: token.device_id(),
            admission: token.admission.clone(),
        };
        callback(&lease.endpoint, &mut context)
    }

    fn dispatch_with_context<T>(
        &self,
        token: &EndpointRouteToken,
        dma_enabled: bool,
        context: &mut dyn DeviceContext,
        mut callback: impl FnMut(&Arc<dyn PciFunction>, &mut dyn PciEndpointContext) -> DeviceResult<T>,
    ) -> DeviceResult<T> {
        let lease = self.router.lease(token, dma_enabled)?;
        let grant = lease.grant.clone();
        let admission = lease._admission.clone();
        let endpoint = lease.endpoint.clone();
        let mut result = None;
        let mut invoke = |nested: &mut dyn DeviceContext| {
            let mut endpoint_context = RoutedPciEndpointContext {
                inner: nested,
                admission: admission.clone(),
            };
            result = Some(callback(&endpoint, &mut endpoint_context));
            Ok(())
        };
        context.with_routed_device(&grant, &mut invoke)?;
        result.ok_or(DeviceError::InvalidState {
            operation: "dispatch PCI endpoint route",
            detail: "routed context callback did not execute".into(),
        })?
    }

    /// Dispatches a BAR read after root lookup and token validation.
    pub fn read_bar(&self, address: u64, width: AccessWidth) -> DeviceResult<u64> {
        let (token, route, command) = self
            .root
            .resolve_bound_bar(address, width)
            .ok_or(DeviceError::NotFound)?;
        self.dispatch_legacy(&token, command.bus_master_enable(), |endpoint, context| {
            endpoint.read_bar(
                PciBarAccess::new(route, command.bus_master_enable()),
                context,
            )
        })
    }

    /// Dispatches a BAR read through an authenticated runtime context.
    pub fn read_bar_with_context(
        &self,
        address: u64,
        width: AccessWidth,
        context: &mut dyn DeviceContext,
    ) -> DeviceResult<u64> {
        let (token, route, command) = self
            .root
            .resolve_bound_bar(address, width)
            .ok_or(DeviceError::NotFound)?;
        self.dispatch_with_context(
            &token,
            command.bus_master_enable(),
            context,
            |endpoint, context| {
                endpoint.read_bar(
                    PciBarAccess::new(route, command.bus_master_enable()),
                    context,
                )
            },
        )
    }

    /// Dispatches a BAR write after root lookup and token validation.
    pub fn write_bar(&self, address: u64, width: AccessWidth, value: u64) -> DeviceResult {
        let (token, route, command) = self
            .root
            .resolve_bound_bar(address, width)
            .ok_or(DeviceError::NotFound)?;
        self.dispatch_legacy(&token, command.bus_master_enable(), |endpoint, context| {
            endpoint.write_bar(
                PciBarAccess::new(route, command.bus_master_enable()),
                value,
                context,
            )
        })
    }

    /// Dispatches a BAR write through an authenticated runtime context.
    pub fn write_bar_with_context(
        &self,
        address: u64,
        width: AccessWidth,
        value: u64,
        context: &mut dyn DeviceContext,
    ) -> DeviceResult {
        let (token, route, command) = self
            .root
            .resolve_bound_bar(address, width)
            .ok_or(DeviceError::NotFound)?;
        self.dispatch_with_context(
            &token,
            command.bus_master_enable(),
            context,
            |endpoint, context| {
                endpoint.write_bar(
                    PciBarAccess::new(route, command.bus_master_enable()),
                    value,
                    context,
                )
            },
        )
    }

    /// Dispatches one complete conventional config read.
    pub fn read_config(
        &self,
        bdf: PciBdf,
        offset: crate::ConfigOffset,
        width: AccessWidth,
    ) -> DeviceResult<u64> {
        match self
            .root
            .prepare_read_config(bdf, offset, width)
            .map_err(pci_config_error)?
        {
            PciConfigReadOutcome::Value(value) => Ok(value),
            PciConfigReadOutcome::DynamicStatus {
                token,
                command,
                value,
                interrupt_status_mask,
            } => {
                let pending = self.dispatch_legacy(
                    &token,
                    command.bus_master_enable(),
                    |endpoint, _context| Ok(endpoint.intx_pending()),
                )?;
                Ok(if pending {
                    value | interrupt_status_mask
                } else {
                    value & !interrupt_status_mask
                })
            }
            PciConfigReadOutcome::Effect {
                token,
                command,
                effect,
            } => self.dispatch_legacy(&token, command.bus_master_enable(), |endpoint, context| {
                endpoint.read_config_effect(*effect, context)
            }),
        }
    }

    pub(crate) fn config_access_intersects_effect(
        &self,
        bdf: PciBdf,
        offset: crate::ConfigOffset,
        width: AccessWidth,
    ) -> DeviceResult<bool> {
        self.root
            .config_access_intersects_effect(bdf, offset, width)
            .map_err(pci_config_error)
    }

    /// Dispatches one complete conventional config write.
    pub fn write_config(
        &self,
        bdf: PciBdf,
        offset: crate::ConfigOffset,
        width: AccessWidth,
        value: u64,
    ) -> DeviceResult {
        match self
            .root
            .prepare_write_config(bdf, offset, width, value)
            .map_err(pci_config_error)?
        {
            PciConfigWriteOutcome::Complete => Ok(()),
            PciConfigWriteOutcome::Effect {
                token,
                command,
                effect,
            } => self.dispatch_legacy(&token, command.bus_master_enable(), |endpoint, context| {
                endpoint.write_config_effect(*effect, context)
            }),
            PciConfigWriteOutcome::CommandChanged { token, command } => {
                let Some(token) = token else {
                    return Ok(());
                };
                self.dispatch_legacy(&token, command.bus_master_enable(), |endpoint, context| {
                    endpoint.command_changed(command, context)
                })
            }
        }
    }

    /// Dispatches a config read effect through an authenticated runtime
    /// context.
    pub fn read_config_with_context(
        &self,
        bdf: PciBdf,
        offset: crate::ConfigOffset,
        width: AccessWidth,
        context: &mut dyn DeviceContext,
    ) -> DeviceResult<u64> {
        match self
            .root
            .prepare_read_config(bdf, offset, width)
            .map_err(pci_config_error)?
        {
            PciConfigReadOutcome::Value(value) => Ok(value),
            PciConfigReadOutcome::DynamicStatus {
                token,
                command,
                value,
                interrupt_status_mask,
            } => {
                let pending = self.dispatch_with_context(
                    &token,
                    command.bus_master_enable(),
                    context,
                    |endpoint, _context| Ok(endpoint.intx_pending()),
                )?;
                Ok(if pending {
                    value | interrupt_status_mask
                } else {
                    value & !interrupt_status_mask
                })
            }
            PciConfigReadOutcome::Effect {
                token,
                command,
                effect,
            } => self.dispatch_with_context(
                &token,
                command.bus_master_enable(),
                context,
                |endpoint, context| endpoint.read_config_effect(*effect, context),
            ),
        }
    }

    /// Dispatches a config write effect through an authenticated runtime
    /// context.
    pub fn write_config_with_context(
        &self,
        bdf: PciBdf,
        offset: crate::ConfigOffset,
        width: AccessWidth,
        value: u64,
        context: &mut dyn DeviceContext,
    ) -> DeviceResult {
        match self
            .root
            .prepare_write_config(bdf, offset, width, value)
            .map_err(pci_config_error)?
        {
            PciConfigWriteOutcome::Complete => Ok(()),
            PciConfigWriteOutcome::Effect {
                token,
                command,
                effect,
            } => self.dispatch_with_context(
                &token,
                command.bus_master_enable(),
                context,
                |endpoint, context| endpoint.write_config_effect(*effect, context),
            ),
            PciConfigWriteOutcome::CommandChanged { token, command } => {
                let Some(token) = token else {
                    return Ok(());
                };
                self.dispatch_with_context(
                    &token,
                    command.bus_master_enable(),
                    context,
                    |endpoint, context| endpoint.command_changed(command, context),
                )
            }
        }
    }
}

impl Drop for PciRootBinding {
    fn drop(&mut self) {
        let mut lifecycle = self.lifecycle.lock_irqsave();
        *lifecycle = BindingLifecycleState::Stopping;
        let (pending, drain_result) = self.router.invalidate_all();
        for withdrawal in pending {
            self.queue_irq_withdrawal(withdrawal);
        }
        if let Err(error) = drain_result {
            warn!("PCI root teardown could not drain IRQ permits: {error}");
        }
        if let Err(error) = self.retry_irq_withdrawals_locked() {
            warn!("PCI root teardown could not complete pending IRQ withdrawals: {error}");
        }
        transfer_pending_irq_withdrawals(&self.pending_irq_withdrawals);
        *lifecycle = BindingLifecycleState::Dead;
    }
}

/// Typed service key published only by a PCI host bundle.
///
/// Bindings stay enumerable through `DeviceRuntime::services()` for
/// diagnostics and host-side verification; endpoint models never receive a
/// `DeviceRuntime`, so route resolution remains dependency-scoped.
pub struct PciRootBindingKey;

impl ServiceKey for PciRootBindingKey {
    type Service = PciRootBinding;
    const NAME: &'static str = "pci-root-binding";
    const CARDINALITY: ServiceCardinality = ServiceCardinality::Multiple;
}

pub(crate) struct PciBindingLease {
    binding: Arc<PciRootBinding>,
    token: EndpointRouteToken,
}

impl Drop for PciBindingLease {
    fn drop(&mut self) {
        let _lifecycle = self.binding.lifecycle.lock_irqsave();
        // Withdraw the root route first. The admission close is the second
        // linearization point; callbacks that already acquired a lease keep
        // their endpoint Arc, while new validation and IRQ permits fail.
        self.binding.root.unbind_device(self.token.device_id());
        if let Some((function, admission)) = self
            .binding
            .router
            .invalidate_device(self.token.device_id())
        {
            let withdrawal = PendingIrqWithdrawal {
                device: self.token.device_id(),
                function,
                admission,
            };
            if let Err(error) = withdrawal.admission.wait_for_irq_permits().and_then(|()| {
                let mut permit = EndpointIrqTransitionPermit { _private: () };
                withdrawal
                    .function
                    .withdraw_irq(&mut permit)
                    .map_err(DeviceManagerError::Device)
            }) {
                // Do not race an in-flight routed transition with the final
                // owner-side withdrawal. Keep the endpoint owner and closed
                // admission for an explicit retry.
                warn!("PCI endpoint teardown queued a pending IRQ withdrawal: {error}");
                self.binding.queue_irq_withdrawal(withdrawal);
            }
        }
    }
}

fn pci_config_error(error: super::PciError) -> DeviceError {
    DeviceError::InvalidInput {
        operation: "access PCI configuration",
        detail: alloc::format!("{error}"),
    }
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;

    use axdevice_base::{
        ControllerInputId, DeviceAccess, InterruptControllerId, InterruptEndpoint,
        InterruptSharing, InterruptTrigger, IrqError, IrqResult, Resource, WiredIrqInput,
        WiredIrqSink,
    };

    use super::*;
    use crate::{
        ConfigOffset, PciCapabilityEffectAccess, PciCapabilityEffectRegion, PciCapabilityId,
        PciCapabilitySpec, PciClass, PciConfigEffectId, PciEndpointIdentity, PciError,
        PciFunctionSpec, PciTopologyBuilder,
    };

    static ORPHAN_QUEUE_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct StubFunction {
        fail_command: bool,
    }

    struct FailingDeassertSink {
        fail_deassert: AtomicBool,
        asserted: AtomicBool,
    }

    struct BlockingWithdrawalFunction {
        started: AtomicBool,
        release: AtomicBool,
        withdrawals: AtomicUsize,
    }

    impl Device for BlockingWithdrawalFunction {
        fn name(&self) -> &str {
            "blocking-withdrawal-function"
        }

        fn resources(&self) -> &[Resource] {
            &[]
        }

        fn read(
            &self,
            _access: &DeviceAccess,
            _context: &mut dyn DeviceContext,
        ) -> DeviceResult<u64> {
            Err(DeviceError::NotFound)
        }

        fn write(
            &self,
            _access: &DeviceAccess,
            _value: u64,
            _context: &mut dyn DeviceContext,
        ) -> DeviceResult {
            Err(DeviceError::NotFound)
        }
    }

    impl PciFunction for BlockingWithdrawalFunction {
        fn read_bar(
            &self,
            _access: PciBarAccess,
            _context: &mut dyn PciEndpointContext,
        ) -> DeviceResult<u64> {
            Ok(0)
        }

        fn write_bar(
            &self,
            _access: PciBarAccess,
            _value: u64,
            _context: &mut dyn PciEndpointContext,
        ) -> DeviceResult {
            Ok(())
        }

        fn withdraw_irq(&self, _permit: &mut EndpointIrqTransitionPermit) -> DeviceResult {
            self.started.store(true, Ordering::Release);
            while !self.release.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            self.withdrawals.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    fn pending_withdrawal(device: u32, function: Arc<dyn PciFunction>) -> PendingIrqWithdrawal {
        let admission = Arc::new(EndpointAdmission::new(
            EndpointBindingGeneration(1),
            RoutedAdmissionEpoch(1),
        ));
        admission.close();
        PendingIrqWithdrawal {
            device: DeviceId::new(device),
            function,
            admission,
        }
    }

    impl WiredIrqSink for FailingDeassertSink {
        fn set_level(&self, input: ControllerInputId, asserted: bool) -> IrqResult {
            if !asserted && self.fail_deassert.load(Ordering::Relaxed) {
                return Err(IrqError::Backend {
                    endpoint: InterruptEndpoint::Wired {
                        controller: InterruptControllerId::new(0),
                        input,
                    },
                    operation: "test deassert",
                    detail: "injected test failure".into(),
                });
            }
            self.asserted.store(asserted, Ordering::Relaxed);
            Ok(())
        }

        fn pulse(&self, input: ControllerInputId) -> IrqResult {
            Err(IrqError::Backend {
                endpoint: InterruptEndpoint::Wired {
                    controller: InterruptControllerId::new(0),
                    input,
                },
                operation: "test pulse",
                detail: "not used by this test".into(),
            })
        }
    }

    #[test]
    fn irq_transition_permit_surfaces_real_line_backend_failures() {
        let sink = Arc::new(FailingDeassertSink {
            fail_deassert: AtomicBool::new(false),
            asserted: AtomicBool::new(false),
        });
        let line = WiredIrqInput::new(
            InterruptControllerId::new(0),
            ControllerInputId::new(19),
            InterruptTrigger::LevelTriggered,
            sink.clone(),
        )
        .connect()
        .unwrap();
        let mut permit = EndpointIrqTransitionPermit { _private: () };

        permit.assert(&line).unwrap();
        sink.fail_deassert.store(true, Ordering::Relaxed);
        let error = permit.deassert(&line).unwrap_err();
        assert!(matches!(
            error,
            DeviceError::Backend {
                operation: "deassert PCI endpoint INTx line",
                ..
            }
        ));
    }

    #[test]
    fn orphan_retry_merges_concurrent_transfers_without_dropping_owners() {
        let _test_lock = ORPHAN_QUEUE_TEST_LOCK.lock().unwrap();
        PciRootBinding::retry_orphaned_irq_withdrawals().unwrap();

        let first = Arc::new(BlockingWithdrawalFunction {
            started: AtomicBool::new(false),
            release: AtomicBool::new(false),
            withdrawals: AtomicUsize::new(0),
        });
        ORPHANED_IRQ_WITHDRAWALS
            .lock_irqsave()
            .push(pending_withdrawal(1, first.clone()));

        let retry = std::thread::spawn(PciRootBinding::retry_orphaned_irq_withdrawals);
        while !first.started.load(Ordering::Acquire) {
            std::thread::yield_now();
        }

        let second = Arc::new(BlockingWithdrawalFunction {
            started: AtomicBool::new(false),
            release: AtomicBool::new(true),
            withdrawals: AtomicUsize::new(0),
        });
        let incoming = SpinLock::new(vec![pending_withdrawal(2, second.clone())]);
        transfer_pending_irq_withdrawals(&incoming);
        first.release.store(true, Ordering::Release);
        retry.join().unwrap().unwrap();

        assert_eq!(first.withdrawals.load(Ordering::Relaxed), 1);
        assert_eq!(second.withdrawals.load(Ordering::Relaxed), 0);
        PciRootBinding::retry_orphaned_irq_withdrawals().unwrap();
        assert_eq!(second.withdrawals.load(Ordering::Relaxed), 1);
    }

    impl Device for StubFunction {
        fn name(&self) -> &str {
            "stub-pci-function"
        }
        fn resources(&self) -> &[Resource] {
            &[]
        }
        fn read(
            &self,
            _access: &DeviceAccess,
            _context: &mut dyn DeviceContext,
        ) -> DeviceResult<u64> {
            Err(DeviceError::NotFound)
        }
        fn write(
            &self,
            _access: &DeviceAccess,
            _value: u64,
            _context: &mut dyn DeviceContext,
        ) -> DeviceResult {
            Ok(())
        }
    }

    impl PciFunction for StubFunction {
        fn read_bar(
            &self,
            _access: PciBarAccess,
            _context: &mut dyn PciEndpointContext,
        ) -> DeviceResult<u64> {
            Ok(0)
        }
        fn write_bar(
            &self,
            _access: PciBarAccess,
            _value: u64,
            _context: &mut dyn PciEndpointContext,
        ) -> DeviceResult {
            Ok(())
        }

        fn command_changed(
            &self,
            _command: PciCommandState,
            _context: &mut dyn PciEndpointContext,
        ) -> DeviceResult {
            if self.fail_command {
                return Err(DeviceError::Unsupported {
                    operation: "synchronize PCI command state",
                    detail: "test endpoint rejected the command transition".into(),
                });
            }
            Ok(())
        }

        fn reset(&self, _command: PciCommandState) -> DeviceResult {
            Err(DeviceError::Unsupported {
                operation: "reset PCI endpoint",
                detail: "test endpoint does not implement reset".into(),
            })
        }
    }

    struct RecordingFunction {
        root: Arc<PciRootState>,
        bdf: PciBdf,
        reads: SpinLock<Vec<(PciConfigReadEffect, DeviceId, u64)>>,
        writes: SpinLock<Vec<(PciConfigWriteEffect, DeviceId)>>,
        commands: SpinLock<Vec<(PciCommandState, DeviceId)>>,
        resets: SpinLock<Vec<PciCommandState>>,
        reset_failures: SpinLock<usize>,
        withdrawals: SpinLock<usize>,
        withdraw_failures: SpinLock<usize>,
        irq_line: Option<IrqLine>,
        supports_effects: bool,
        pending: bool,
    }

    impl Device for RecordingFunction {
        fn name(&self) -> &str {
            "recording-pci-function"
        }

        fn resources(&self) -> &[Resource] {
            &[]
        }

        fn read(
            &self,
            _access: &DeviceAccess,
            _context: &mut dyn DeviceContext,
        ) -> DeviceResult<u64> {
            Err(DeviceError::NotFound)
        }

        fn write(
            &self,
            _access: &DeviceAccess,
            _value: u64,
            _context: &mut dyn DeviceContext,
        ) -> DeviceResult {
            Ok(())
        }
    }

    impl PciFunction for RecordingFunction {
        fn intx_pending(&self) -> bool {
            // A dynamic status query must not retain the root state lock while
            // entering endpoint-owned behavior.
            let _ =
                self.root
                    .read_config(self.bdf, ConfigOffset::new(0).unwrap(), AccessWidth::Dword);
            self.pending
        }

        fn supported_config_effects(&self) -> &[PciConfigEffectId] {
            const EFFECTS: &[PciConfigEffectId] = &[PciConfigEffectId::new(7)];
            if self.supports_effects { EFFECTS } else { &[] }
        }

        fn read_bar(
            &self,
            _access: PciBarAccess,
            _context: &mut dyn PciEndpointContext,
        ) -> DeviceResult<u64> {
            Ok(0)
        }

        fn write_bar(
            &self,
            _access: PciBarAccess,
            _value: u64,
            _context: &mut dyn PciEndpointContext,
        ) -> DeviceResult {
            Ok(())
        }

        fn read_config_effect(
            &self,
            effect: PciConfigReadEffect,
            context: &mut dyn PciEndpointContext,
        ) -> DeviceResult<u64> {
            // This nested root read proves that dispatch released the root
            // state lock before entering endpoint-owned behavior.
            let vendor_device = self
                .root
                .read_config(self.bdf, ConfigOffset::new(0).unwrap(), AccessWidth::Dword)
                .map_err(|error| DeviceError::InvalidInput {
                    operation: "read recording PCI function",
                    detail: alloc::format!("{error}"),
                })?;
            self.reads
                .lock_irqsave()
                .push((effect, context.device_id(), vendor_device));
            Ok(0x5a)
        }

        fn write_config_effect(
            &self,
            effect: PciConfigWriteEffect,
            context: &mut dyn PciEndpointContext,
        ) -> DeviceResult {
            self.writes
                .lock_irqsave()
                .push((effect, context.device_id()));
            Ok(())
        }

        fn command_changed(
            &self,
            command: PciCommandState,
            context: &mut dyn PciEndpointContext,
        ) -> DeviceResult {
            self.commands
                .lock_irqsave()
                .push((command, context.device_id()));
            Ok(())
        }

        fn reset(&self, command: PciCommandState) -> DeviceResult {
            self.resets.lock_irqsave().push(command);
            let mut failures = self.reset_failures.lock_irqsave();
            if *failures != 0 {
                *failures -= 1;
                return Err(DeviceError::Backend {
                    operation: "reset test PCI endpoint",
                    detail: "injected test failure".into(),
                });
            }
            Ok(())
        }

        fn withdraw_irq(&self, permit: &mut EndpointIrqTransitionPermit) -> DeviceResult {
            let mut failures = self.withdraw_failures.lock_irqsave();
            if *failures != 0 {
                *failures -= 1;
                return Err(DeviceError::Backend {
                    operation: "withdraw test PCI endpoint IRQ",
                    detail: "injected test failure".into(),
                });
            }
            if let Some(line) = &self.irq_line {
                permit.deassert(line)?;
            }
            *self.withdrawals.lock_irqsave() += 1;
            Ok(())
        }
    }

    #[test]
    fn binding_rejects_an_unsupported_config_effect_before_publishing_route() {
        let effect = PciCapabilityEffectRegion::new(
            PciConfigEffectId::new(7),
            2,
            1,
            PciCapabilityEffectAccess::ReadWrite,
        )
        .unwrap();
        let capability =
            PciCapabilitySpec::new(PciCapabilityId::new(9), alloc::vec![0], alloc::vec![0])
                .unwrap()
                .with_effect(effect)
                .unwrap();
        let function_id = DeviceNodeId::new("unsupported-effect-endpoint").unwrap();
        let mut builder = PciTopologyBuilder::new();
        builder
            .add_function(
                PciFunctionSpec::new(
                    function_id.clone(),
                    PciEndpointIdentity::new(0x1af4, 0x1041, PciClass::new(0xff, 0, 0)),
                )
                .with_capability(capability),
            )
            .unwrap();
        let topology = Arc::new(builder.resolve(0xc000_0000..0xc100_0000).unwrap());
        let bdf = topology.function(&function_id).unwrap().bdf();
        let binding = Arc::new(PciRootBinding::new(
            DeviceNodeId::new("host").unwrap(),
            Arc::new(PciRootState::new(topology)),
        ));
        let function: Arc<dyn PciFunction> = Arc::new(StubFunction {
            fail_command: false,
        });
        let mut grants = Vec::new();

        assert!(matches!(
            binding.bind_registered(&function_id, DeviceId::new(7), function, &mut grants),
            Err(DeviceManagerError::InvalidConfig { .. })
        ));
        assert!(grants.is_empty());
        assert!(matches!(
            binding.read_config(bdf, ConfigOffset::new(0x42).unwrap(), AccessWidth::Byte,),
            Err(DeviceError::InvalidInput { .. })
        ));
    }

    fn router() -> EndpointRouter {
        EndpointRouter {
            state: SpinLock::new(EndpointRouterState::default()),
        }
    }

    #[test]
    fn rebind_mints_a_new_generation_and_rejects_stale_tokens() {
        let router = router();
        let function: Arc<dyn PciFunction> = Arc::new(StubFunction {
            fail_command: false,
        });
        let device = DeviceId::new(7);

        let first = router.activate(device, Arc::clone(&function)).unwrap();
        assert_eq!(first.binding_generation(), 1);
        assert!(router.endpoint(&first).is_ok());

        let removed = router.invalidate(&first).unwrap();
        assert!(Arc::ptr_eq(&removed, &function));
        let second = router.activate(device, Arc::clone(&function)).unwrap();
        assert_eq!(second.binding_generation(), 2);

        // The old generation can never dispatch again, before or after the
        // new binding exists.
        assert!(matches!(
            router.endpoint(&first),
            Err(DeviceError::InvalidState { .. })
        ));
        drop(router.endpoint(&second).unwrap());
    }

    #[test]
    fn invalidate_returns_none_for_unknown_or_stale_tokens() {
        let router = router();
        let function: Arc<dyn PciFunction> = Arc::new(StubFunction {
            fail_command: false,
        });
        let device = DeviceId::new(3);

        let token = router.activate(device, Arc::clone(&function)).unwrap();
        let forged = EndpointRouteToken {
            binding_generation: EndpointBindingGeneration(
                token.binding_generation().saturating_add(1),
            ),
            ..token.clone()
        };
        assert!(router.invalidate(&forged).is_none());
        assert!(router.endpoint(&token).is_ok());
        assert_eq!(
            router.invalidate(&token).map(|arc| Arc::strong_count(&arc)),
            Some(2)
        );
        assert!(router.invalidate(&token).is_none());
    }

    #[test]
    fn invalidation_closes_new_irq_permits_but_does_not_revoke_an_acquired_one() {
        let router = router();
        let function: Arc<dyn PciFunction> = Arc::new(StubFunction {
            fail_command: false,
        });
        let token = router
            .activate(DeviceId::new(4), function)
            .expect("test route activation succeeds");
        let permit = token
            .admission
            .clone()
            .acquire_irq_permit()
            .expect("permit is admitted before teardown");

        drop(router.invalidate(&token));
        assert!(matches!(
            token.admission.clone().acquire_irq_permit(),
            Err(DeviceError::InvalidState { .. })
        ));
        drop(permit);
    }

    #[test]
    fn irq_permit_drain_has_a_bounded_failure_path() {
        let router = router();
        let function: Arc<dyn PciFunction> = Arc::new(StubFunction {
            fail_command: false,
        });
        let token = router
            .activate(DeviceId::new(4), function)
            .expect("test route activation succeeds");
        let permit = token
            .admission
            .clone()
            .acquire_irq_permit()
            .expect("permit is admitted before drain");

        assert!(matches!(
            token.admission.wait_for_irq_permits_with_budget(0),
            Err(DeviceManagerError::InvalidState { .. })
        ));
        drop(permit);
        token.admission.wait_for_irq_permits_with_budget(0).unwrap();
    }

    #[test]
    fn lifecycle_reset_advances_only_the_admission_epoch() {
        let router = router();
        let function: Arc<dyn PciFunction> = Arc::new(StubFunction {
            fail_command: false,
        });
        let old = router
            .activate(DeviceId::new(5), function)
            .expect("test route activation succeeds");
        let old_grant = old.grant(false);
        let replacements = router.reset_admissions().unwrap();
        assert_eq!(replacements.len(), 1);
        assert!(matches!(
            old.admission.clone().acquire(&old),
            Err(DeviceError::InvalidState { .. })
        ));
        assert!(!old_grant.admission_is_open());

        router.open_admissions();
        let (_, fresh) = &replacements[0];
        assert_eq!(fresh.binding_generation(), old.binding_generation());
        assert_eq!(fresh.admission_epoch(), old.admission_epoch() + 1);
        assert!(router.endpoint(fresh).is_ok());
        assert!(fresh.admission.clone().acquire(fresh).is_ok());
        assert!(fresh.grant(false).admission_is_open());
    }

    #[test]
    fn full_lifecycle_reset_resets_endpoint_before_reopening_admission() {
        let function_id = DeviceNodeId::new("resettable-endpoint").unwrap();
        let mut builder = PciTopologyBuilder::new();
        builder
            .add_function(PciFunctionSpec::new(
                function_id.clone(),
                PciEndpointIdentity::new(0x1af4, 0x1042, PciClass::new(0xff, 0, 0)),
            ))
            .unwrap();
        let topology = Arc::new(builder.resolve(0xc000_0000..0xc100_0000).unwrap());
        let root = Arc::new(PciRootState::new(Arc::clone(&topology)));
        let binding = Arc::new(PciRootBinding::new(
            DeviceNodeId::new("host").unwrap(),
            Arc::clone(&root),
        ));
        let sink = Arc::new(FailingDeassertSink {
            fail_deassert: AtomicBool::new(false),
            asserted: AtomicBool::new(false),
        });
        let line = WiredIrqInput::new(
            InterruptControllerId::new(0),
            ControllerInputId::new(19),
            InterruptTrigger::LevelTriggered,
            sink.clone(),
        )
        .connect()
        .unwrap();
        line.assert().unwrap();
        let recording = Arc::new(RecordingFunction {
            root,
            bdf: topology.function(&function_id).unwrap().bdf(),
            reads: SpinLock::new(Vec::new()),
            writes: SpinLock::new(Vec::new()),
            commands: SpinLock::new(Vec::new()),
            resets: SpinLock::new(Vec::new()),
            reset_failures: SpinLock::new(0),
            withdrawals: SpinLock::new(0),
            withdraw_failures: SpinLock::new(0),
            irq_line: Some(line),
            supports_effects: false,
            pending: false,
        });
        let mut grants = Vec::new();
        let lease = binding
            .bind_registered(
                &function_id,
                DeviceId::new(7),
                recording.clone(),
                &mut grants,
            )
            .unwrap();

        binding.reset_lifecycle().unwrap();

        assert_eq!(*recording.withdrawals.lock_irqsave(), 1);
        assert!(!sink.asserted.load(Ordering::Relaxed));

        let resets = recording.resets.lock_irqsave();
        assert_eq!(resets.len(), 1);
        assert!(!resets[0].bus_master_enable());
        drop(resets);
        let token = binding
            .router
            .state
            .lock_irqsave()
            .endpoints
            .get(&DeviceId::new(7))
            .unwrap()
            .token
            .clone();
        assert_eq!(token.binding_generation(), lease.token.binding_generation());
        assert_eq!(token.admission_epoch(), 2);
        assert!(token.admission.clone().acquire(&token).is_ok());
    }

    #[test]
    fn full_lifecycle_reset_failure_keeps_endpoint_admission_closed() {
        let function_id = DeviceNodeId::new("unresettable-endpoint").unwrap();
        let mut builder = PciTopologyBuilder::new();
        builder
            .add_function(PciFunctionSpec::new(
                function_id.clone(),
                PciEndpointIdentity::new(0x1af4, 0x1042, PciClass::new(0xff, 0, 0)),
            ))
            .unwrap();
        let topology = Arc::new(builder.resolve(0xc000_0000..0xc100_0000).unwrap());
        let root = Arc::new(PciRootState::new(Arc::clone(&topology)));
        let binding = Arc::new(PciRootBinding::new(
            DeviceNodeId::new("host").unwrap(),
            Arc::clone(&root),
        ));
        let sink = Arc::new(FailingDeassertSink {
            fail_deassert: AtomicBool::new(false),
            asserted: AtomicBool::new(false),
        });
        let line = WiredIrqInput::new(
            InterruptControllerId::new(0),
            ControllerInputId::new(19),
            InterruptTrigger::LevelTriggered,
            sink.clone(),
        )
        .connect()
        .unwrap();
        line.assert().unwrap();
        let recording = Arc::new(RecordingFunction {
            root,
            bdf: topology.function(&function_id).unwrap().bdf(),
            reads: SpinLock::new(Vec::new()),
            writes: SpinLock::new(Vec::new()),
            commands: SpinLock::new(Vec::new()),
            resets: SpinLock::new(Vec::new()),
            reset_failures: SpinLock::new(1),
            withdrawals: SpinLock::new(0),
            withdraw_failures: SpinLock::new(0),
            irq_line: Some(line),
            supports_effects: false,
            pending: false,
        });
        let mut grants = Vec::new();
        let lease = binding
            .bind_registered(
                &function_id,
                DeviceId::new(7),
                recording.clone(),
                &mut grants,
            )
            .unwrap();

        assert!(matches!(
            binding.reset_lifecycle(),
            Err(DeviceManagerError::Device(DeviceError::Backend { .. }))
        ));
        assert_eq!(recording.resets.lock_irqsave().len(), 1);
        assert_eq!(*recording.withdrawals.lock_irqsave(), 1);
        assert!(!sink.asserted.load(Ordering::Relaxed));
        assert_eq!(
            *binding.lifecycle.lock_irqsave(),
            BindingLifecycleState::ResetFailed
        );
        let token = binding
            .router
            .state
            .lock_irqsave()
            .endpoints
            .get(&DeviceId::new(7))
            .unwrap()
            .token
            .clone();
        assert!(!token.grant(false).admission_is_open());
        assert!(matches!(
            token.admission.clone().acquire(&token),
            Err(DeviceError::InvalidState { .. })
        ));
        drop(lease);
    }

    #[test]
    fn reset_irq_cleanup_failure_stays_closed_until_teardown_retries_withdrawal() {
        let function_id = DeviceNodeId::new("reset-cleanup-failure-endpoint").unwrap();
        let mut builder = PciTopologyBuilder::new();
        builder
            .add_function(PciFunctionSpec::new(
                function_id.clone(),
                PciEndpointIdentity::new(0x1af4, 0x1042, PciClass::new(0xff, 0, 0)),
            ))
            .unwrap();
        let topology = Arc::new(builder.resolve(0xc000_0000..0xc100_0000).unwrap());
        let root = Arc::new(PciRootState::new(Arc::clone(&topology)));
        let binding = Arc::new(PciRootBinding::new(
            DeviceNodeId::new("host").unwrap(),
            Arc::clone(&root),
        ));
        let sink = Arc::new(FailingDeassertSink {
            fail_deassert: AtomicBool::new(false),
            asserted: AtomicBool::new(false),
        });
        let line = WiredIrqInput::new(
            InterruptControllerId::new(0),
            ControllerInputId::new(19),
            InterruptTrigger::LevelTriggered,
            sink.clone(),
        )
        .connect()
        .unwrap();
        line.assert().unwrap();
        let recording = Arc::new(RecordingFunction {
            root,
            bdf: topology.function(&function_id).unwrap().bdf(),
            reads: SpinLock::new(Vec::new()),
            writes: SpinLock::new(Vec::new()),
            commands: SpinLock::new(Vec::new()),
            resets: SpinLock::new(Vec::new()),
            reset_failures: SpinLock::new(0),
            withdrawals: SpinLock::new(0),
            withdraw_failures: SpinLock::new(1),
            irq_line: Some(line),
            supports_effects: false,
            pending: false,
        });
        let mut grants = Vec::new();
        let lease = binding
            .bind_registered(
                &function_id,
                DeviceId::new(12),
                recording.clone(),
                &mut grants,
            )
            .unwrap();

        assert!(matches!(
            binding.reset_lifecycle(),
            Err(DeviceManagerError::Device(DeviceError::Backend { .. }))
        ));
        assert_eq!(
            *binding.lifecycle.lock_irqsave(),
            BindingLifecycleState::ResetFailed
        );
        assert!(
            !binding
                .router
                .state
                .lock_irqsave()
                .endpoints
                .get(&DeviceId::new(12))
                .unwrap()
                .token
                .grant(false)
                .admission_is_open()
        );
        assert!(sink.asserted.load(Ordering::Relaxed));

        *recording.withdraw_failures.lock_irqsave() = 0;
        drop(lease);
        assert!(!sink.asserted.load(Ordering::Relaxed));
        assert_eq!(*recording.withdrawals.lock_irqsave(), 1);
        drop(binding);
    }

    #[test]
    fn endpoint_binding_waits_for_the_lifecycle_owner_gate() {
        let function_id = DeviceNodeId::new("gated-endpoint").unwrap();
        let mut builder = PciTopologyBuilder::new();
        builder
            .add_function(PciFunctionSpec::new(
                function_id.clone(),
                PciEndpointIdentity::new(0x1af4, 0x1042, PciClass::new(0xff, 0, 0)),
            ))
            .unwrap();
        let topology = Arc::new(builder.resolve(0xc000_0000..0xc100_0000).unwrap());
        let binding = Arc::new(PciRootBinding::new(
            DeviceNodeId::new("host").unwrap(),
            Arc::new(PciRootState::new(topology)),
        ));
        let gate = binding.lifecycle.lock_irqsave();
        let (sender, receiver) = std::sync::mpsc::channel();
        let bind_binding = Arc::clone(&binding);
        std::thread::spawn(move || {
            let mut grants = Vec::new();
            let result = bind_binding.bind_registered(
                &function_id,
                DeviceId::new(7),
                Arc::new(StubFunction {
                    fail_command: false,
                }),
                &mut grants,
            );
            sender.send(result.is_ok()).unwrap();
        });

        assert!(receiver.try_recv().is_err());
        drop(gate);
        assert!(receiver.recv().unwrap());
    }

    #[test]
    fn root_rejects_a_second_binding_for_the_same_function() {
        use crate::{PciClass, PciEndpointIdentity, PciFunctionSpec, PciTopologyBuilder};

        let mut builder = PciTopologyBuilder::new();
        builder
            .add_function(PciFunctionSpec::new(
                DeviceNodeId::new("endpoint").unwrap(),
                PciEndpointIdentity::new(0x1af4, 0x1110, PciClass::new(0xff, 0, 0)),
            ))
            .unwrap();
        let topology = Arc::new(builder.resolve(0xc000_0000..0xc100_0000).unwrap());
        let root = PciRootState::new(Arc::clone(&topology));
        let function_id = DeviceNodeId::new("endpoint").unwrap();

        let router = router();
        let function: Arc<dyn PciFunction> = Arc::new(StubFunction {
            fail_command: false,
        });
        let first = router
            .activate(DeviceId::new(1), Arc::clone(&function))
            .unwrap();
        root.bind_endpoint(&function_id, first.clone()).unwrap();
        assert!(matches!(
            root.bind_endpoint(&function_id, first.clone()),
            Err(PciError::FunctionAlreadyBound { .. })
        ));

        // Unbind invalidates the route; the same token never revives.
        drop(router.invalidate(&first));
        root.unbind_device(first.device_id());
        assert_eq!(root.resolve_bound_bar(0xc000_0000, AccessWidth::Byte), None);
        let second = router
            .activate(DeviceId::new(1), Arc::clone(&function))
            .unwrap();
        root.bind_endpoint(&function_id, second).unwrap();
    }

    #[test]
    fn binding_dispatches_config_effects_and_command_transitions() {
        let effect = PciCapabilityEffectRegion::new(
            PciConfigEffectId::new(7),
            8,
            6,
            PciCapabilityEffectAccess::ReadWrite,
        )
        .unwrap();
        let capability = PciCapabilitySpec::new(
            PciCapabilityId::new(9),
            alloc::vec![0, 0, 0x11, 0x22, 0x33, 0x44, 0, 0, 0, 0, 0, 0, 0, 0,],
            alloc::vec![0, 0, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0, 0, 0, 0, 0],
        )
        .unwrap()
        .with_effect(effect)
        .unwrap();
        let function_id = DeviceNodeId::new("effect-endpoint").unwrap();
        let mut builder = PciTopologyBuilder::new();
        builder
            .add_function(
                PciFunctionSpec::new(
                    function_id.clone(),
                    PciEndpointIdentity::new(0x1af4, 0x1041, PciClass::new(0xff, 0, 0)),
                )
                .with_capability(capability),
            )
            .unwrap();
        let topology = Arc::new(builder.resolve(0xc000_0000..0xc100_0000).unwrap());
        let bdf = topology.function(&function_id).unwrap().bdf();
        let root = Arc::new(PciRootState::new(Arc::clone(&topology)));
        let binding = Arc::new(PciRootBinding::new(
            DeviceNodeId::new("host").unwrap(),
            Arc::clone(&root),
        ));
        let recording = Arc::new(RecordingFunction {
            root,
            bdf,
            reads: SpinLock::new(Vec::new()),
            writes: SpinLock::new(Vec::new()),
            commands: SpinLock::new(Vec::new()),
            resets: SpinLock::new(Vec::new()),
            reset_failures: SpinLock::new(0),
            withdrawals: SpinLock::new(0),
            withdraw_failures: SpinLock::new(0),
            irq_line: None,
            supports_effects: true,
            pending: false,
        });
        let mut grants = Vec::new();
        let lease = binding
            .bind_registered(
                &function_id,
                DeviceId::new(7),
                recording.clone(),
                &mut grants,
            )
            .unwrap();
        let capability_offset = topology
            .function(&function_id)
            .unwrap()
            .capabilities()
            .next()
            .unwrap()
            .offset()
            .value();

        // Selector bytes are ordinary root-owned storage. The effect must
        // observe their value captured by the same transaction.
        binding
            .write_config(
                bdf,
                ConfigOffset::new(capability_offset + 4).unwrap(),
                AccessWidth::Dword,
                0x6655_4433,
            )
            .unwrap();
        assert!(recording.reads.lock_irqsave().is_empty());
        assert!(recording.writes.lock_irqsave().is_empty());

        assert_eq!(
            binding
                .read_config(
                    bdf,
                    ConfigOffset::new(capability_offset + 8).unwrap(),
                    AccessWidth::Dword,
                )
                .unwrap(),
            0x5a
        );
        let read = recording.reads.lock_irqsave().pop().unwrap();
        assert_eq!(read.0.capability(), PciCapabilityId::new(9));
        assert_eq!(read.0.effect(), PciConfigEffectId::new(7));
        assert_eq!(read.0.offset(), 8);
        assert_eq!(read.0.width(), AccessWidth::Dword);
        assert_eq!(read.1, DeviceId::new(7));
        assert_eq!(read.2, 0x1041_1af4);
        assert_eq!(
            &read.0.capability_snapshot().bytes()[..8],
            &[0, 0, 0x33, 0x44, 0x55, 0x66, 0, 0]
        );

        binding
            .write_config(
                bdf,
                ConfigOffset::new(capability_offset + 8).unwrap(),
                AccessWidth::Dword,
                0xfeed_beef,
            )
            .unwrap();
        let write = recording.writes.lock_irqsave().pop().unwrap();
        assert_eq!(write.0.value(), 0xfeed_beef);
        assert_eq!(write.1, DeviceId::new(7));
        assert_eq!(
            &write.0.capability_snapshot().bytes()[..8],
            &[0, 0, 0x33, 0x44, 0x55, 0x66, 0, 0]
        );

        // Effect results are not copied into root config storage: the next
        // read reaches the endpoint again and returns its fresh result.
        assert_eq!(
            binding
                .read_config(
                    bdf,
                    ConfigOffset::new(capability_offset + 8).unwrap(),
                    AccessWidth::Dword,
                )
                .unwrap(),
            0x5a
        );
        let second_read = recording.reads.lock_irqsave().pop().unwrap();
        assert_eq!(second_read.0.effect(), PciConfigEffectId::new(7));
        assert_eq!(second_read.1, DeviceId::new(7));

        binding
            .write_config(
                bdf,
                ConfigOffset::new(4).unwrap(),
                AccessWidth::Word,
                0x0406,
            )
            .unwrap();
        let command = recording.commands.lock_irqsave().pop().unwrap();
        assert!(command.0.memory_space_enable());
        assert!(command.0.bus_master_enable());
        assert!(command.0.interrupt_disable());
        assert_eq!(command.1, DeviceId::new(7));

        assert!(matches!(
            binding.read_config(
                bdf,
                ConfigOffset::new(capability_offset + 12).unwrap(),
                AccessWidth::Dword,
            ),
            Err(DeviceError::InvalidInput { .. })
        ));
        assert!(recording.reads.lock_irqsave().is_empty());

        drop(lease);
        assert!(matches!(
            binding.read_config(
                bdf,
                ConfigOffset::new(capability_offset + 8).unwrap(),
                AccessWidth::Dword,
            ),
            Err(DeviceError::InvalidInput { .. })
        ));
    }

    #[test]
    fn dynamic_interrupt_status_is_read_from_the_bound_endpoint() {
        let function_id = DeviceNodeId::new("intx-endpoint").unwrap();
        let mut builder = PciTopologyBuilder::new();
        builder
            .add_function(
                PciFunctionSpec::new(
                    function_id.clone(),
                    PciEndpointIdentity::new(0x1af4, 0x1041, PciClass::new(0xff, 0, 0)),
                )
                .with_intx(crate::PciIntxRequirement::new(
                    crate::PciIntxPin::A,
                    crate::ResourceSlot::new("intx").unwrap(),
                ))
                .unwrap(),
            )
            .unwrap();
        let route = crate::PciIntxRouter::new(
            InterruptControllerId::new(0),
            [
                ControllerInputId::new(16),
                ControllerInputId::new(17),
                ControllerInputId::new(18),
                ControllerInputId::new(19),
            ],
            [16, 17, 18, 19],
            InterruptTrigger::LevelTriggered,
            InterruptSharing::Shared,
        )
        .resolve(&function_id, PciBdf::bus_zero(0), crate::PciIntxPin::A)
        .unwrap();
        builder.set_intx_route(&function_id, route).unwrap();
        let topology = Arc::new(builder.resolve(0xc000_0000..0xc100_0000).unwrap());
        let bdf = topology.function(&function_id).unwrap().bdf();
        assert!(topology.function(&function_id).unwrap().intx().is_some());
        let root = Arc::new(PciRootState::new(Arc::clone(&topology)));
        let binding = Arc::new(PciRootBinding::new(
            DeviceNodeId::new("host").unwrap(),
            root,
        ));
        let recording = Arc::new(RecordingFunction {
            root: Arc::clone(&binding.root),
            bdf,
            reads: SpinLock::new(Vec::new()),
            writes: SpinLock::new(Vec::new()),
            commands: SpinLock::new(Vec::new()),
            resets: SpinLock::new(Vec::new()),
            reset_failures: SpinLock::new(0),
            withdrawals: SpinLock::new(0),
            withdraw_failures: SpinLock::new(0),
            irq_line: None,
            supports_effects: false,
            pending: true,
        });
        let mut grants = Vec::new();
        let lease = binding
            .bind_registered(
                &function_id,
                DeviceId::new(9),
                recording.clone(),
                &mut grants,
            )
            .unwrap();

        assert_eq!(
            binding
                .read_config(bdf, ConfigOffset::new(0x06).unwrap(), AccessWidth::Byte)
                .unwrap()
                & 0x08,
            0x08
        );
        drop(lease);
        // Teardown invokes endpoint-owned final IRQ withdrawal after the
        // binding admission has been closed and drained.
        assert_eq!(*recording.withdrawals.lock_irqsave(), 1);
        assert_eq!(
            binding
                .read_config(bdf, ConfigOffset::new(0x06).unwrap(), AccessWidth::Byte)
                .unwrap()
                & 0x08,
            0
        );
    }

    #[test]
    fn failed_irq_withdrawal_survives_root_binding_destruction() {
        let _test_lock = ORPHAN_QUEUE_TEST_LOCK.lock().unwrap();
        let function_id = DeviceNodeId::new("pending-withdrawal-endpoint").unwrap();
        let mut builder = PciTopologyBuilder::new();
        builder
            .add_function(
                PciFunctionSpec::new(
                    function_id.clone(),
                    PciEndpointIdentity::new(0x1af4, 0x1041, PciClass::new(0xff, 0, 0)),
                )
                .with_intx(crate::PciIntxRequirement::new(
                    crate::PciIntxPin::A,
                    crate::ResourceSlot::new("intx").unwrap(),
                ))
                .unwrap(),
            )
            .unwrap();
        let route = crate::PciIntxRouter::new(
            InterruptControllerId::new(0),
            [
                ControllerInputId::new(16),
                ControllerInputId::new(17),
                ControllerInputId::new(18),
                ControllerInputId::new(19),
            ],
            [16, 17, 18, 19],
            InterruptTrigger::LevelTriggered,
            InterruptSharing::Shared,
        )
        .resolve(&function_id, PciBdf::bus_zero(0), crate::PciIntxPin::A)
        .unwrap();
        builder.set_intx_route(&function_id, route).unwrap();
        let topology = Arc::new(builder.resolve(0xc000_0000..0xc100_0000).unwrap());
        let bdf = topology.function(&function_id).unwrap().bdf();
        let binding = Arc::new(PciRootBinding::new(
            DeviceNodeId::new("host").unwrap(),
            Arc::new(PciRootState::new(Arc::clone(&topology))),
        ));
        let recording = Arc::new(RecordingFunction {
            root: Arc::clone(&binding.root),
            bdf,
            reads: SpinLock::new(Vec::new()),
            writes: SpinLock::new(Vec::new()),
            commands: SpinLock::new(Vec::new()),
            resets: SpinLock::new(Vec::new()),
            reset_failures: SpinLock::new(0),
            withdrawals: SpinLock::new(0),
            withdraw_failures: SpinLock::new(0),
            irq_line: None,
            supports_effects: false,
            pending: false,
        });
        let mut grants = Vec::new();
        let lease = binding
            .bind_registered(
                &function_id,
                DeviceId::new(10),
                recording.clone(),
                &mut grants,
            )
            .unwrap();
        let permit = lease.token.admission.acquire_irq_permit().unwrap();

        drop(lease);
        drop(binding);
        assert_eq!(*recording.withdrawals.lock_irqsave(), 0);

        drop(permit);
        PciRootBinding::retry_orphaned_irq_withdrawals().unwrap();
        assert_eq!(*recording.withdrawals.lock_irqsave(), 1);
    }

    #[test]
    fn failed_owner_irq_withdrawal_is_retryable() {
        let function_id = DeviceNodeId::new("failed-withdrawal-endpoint").unwrap();
        let mut builder = PciTopologyBuilder::new();
        builder
            .add_function(
                PciFunctionSpec::new(
                    function_id.clone(),
                    PciEndpointIdentity::new(0x1af4, 0x1041, PciClass::new(0xff, 0, 0)),
                )
                .with_intx(crate::PciIntxRequirement::new(
                    crate::PciIntxPin::A,
                    crate::ResourceSlot::new("intx").unwrap(),
                ))
                .unwrap(),
            )
            .unwrap();
        let route = crate::PciIntxRouter::new(
            InterruptControllerId::new(0),
            [
                ControllerInputId::new(16),
                ControllerInputId::new(17),
                ControllerInputId::new(18),
                ControllerInputId::new(19),
            ],
            [16, 17, 18, 19],
            InterruptTrigger::LevelTriggered,
            InterruptSharing::Shared,
        )
        .resolve(&function_id, PciBdf::bus_zero(0), crate::PciIntxPin::A)
        .unwrap();
        builder.set_intx_route(&function_id, route).unwrap();
        let topology = Arc::new(builder.resolve(0xc000_0000..0xc100_0000).unwrap());
        let bdf = topology.function(&function_id).unwrap().bdf();
        let binding = Arc::new(PciRootBinding::new(
            DeviceNodeId::new("host").unwrap(),
            Arc::new(PciRootState::new(Arc::clone(&topology))),
        ));
        let recording = Arc::new(RecordingFunction {
            root: Arc::clone(&binding.root),
            bdf,
            reads: SpinLock::new(Vec::new()),
            writes: SpinLock::new(Vec::new()),
            commands: SpinLock::new(Vec::new()),
            resets: SpinLock::new(Vec::new()),
            reset_failures: SpinLock::new(0),
            withdrawals: SpinLock::new(0),
            withdraw_failures: SpinLock::new(1),
            irq_line: None,
            supports_effects: false,
            pending: false,
        });
        let mut grants = Vec::new();
        let lease = binding
            .bind_registered(
                &function_id,
                DeviceId::new(11),
                recording.clone(),
                &mut grants,
            )
            .unwrap();

        drop(lease);
        assert_eq!(*recording.withdrawals.lock_irqsave(), 0);
        assert!(binding.retry_irq_withdrawals().is_ok());
        assert_eq!(*recording.withdrawals.lock_irqsave(), 1);
    }

    #[test]
    fn command_callback_failure_keeps_the_root_owned_command_commit() {
        let function_id = DeviceNodeId::new("failing-command-endpoint").unwrap();
        let mut builder = PciTopologyBuilder::new();
        builder
            .add_function(PciFunctionSpec::new(
                function_id.clone(),
                PciEndpointIdentity::new(0x1af4, 0x1041, PciClass::new(0xff, 0, 0)),
            ))
            .unwrap();
        let topology = Arc::new(builder.resolve(0xc000_0000..0xc100_0000).unwrap());
        let bdf = topology.function(&function_id).unwrap().bdf();
        let binding = Arc::new(PciRootBinding::new(
            DeviceNodeId::new("host").unwrap(),
            Arc::new(PciRootState::new(topology)),
        ));
        let function: Arc<dyn PciFunction> = Arc::new(StubFunction { fail_command: true });
        let mut grants = Vec::new();
        let _lease = binding
            .bind_registered(&function_id, DeviceId::new(8), function, &mut grants)
            .unwrap();

        assert!(matches!(
            binding.write_config(
                bdf,
                ConfigOffset::new(4).unwrap(),
                AccessWidth::Word,
                0x0406
            ),
            Err(DeviceError::Unsupported { .. })
        ));
        assert_eq!(
            binding
                .read_config(bdf, ConfigOffset::new(4).unwrap(), AccessWidth::Word)
                .unwrap(),
            0x0406
        );
    }
}
