//! Runtime-authenticated PCI endpoint binding and BAR dispatch.

use alloc::{collections::BTreeMap, string::ToString, sync::Arc};

use ax_sync::SpinLock;
use axdevice_base::{
    Device, DeviceContext, DeviceError, DeviceId, DeviceResult, NoopDeviceContext,
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

/// Metadata passed to one endpoint BAR callback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciBarAccess {
    route: PciBarRoute,
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

/// Endpoint-owned behavior reached after authenticated PCI routing.
///
/// # Device context contract
///
/// Callbacks run strictly outside the root lock, after the runtime validated
/// the [`EndpointRouteToken`] and pinned the endpoint with a strong reference.
/// They currently receive an identity-correct but capability-free
/// [`NoopDeviceContext`] carrying the endpoint's final [`DeviceId`]: grants
/// registered through the bundle (guest memory, timers, wake, stop) are not
/// reachable from this path yet. Routing and dispatch ownership stays with
/// [`DeviceRuntime`](crate::DeviceRuntime); the first grant-bearing endpoint
/// must extend that seam in its own design together with a
/// grant-through-BAR-callback regression test. The route token itself never
/// carries or mints capabilities.
pub trait PciFunction: Device {
    /// Returns the endpoint-owned config effects implemented by this function.
    ///
    /// The runtime compares this list with the effect IDs declared in the
    /// resolved PCI capabilities before publishing a route. An empty default
    /// keeps functions without endpoint config effects source-compatible.
    fn supported_config_effects(&self) -> &[PciConfigEffectId] {
        &[]
    }

    /// Reads one complete memory BAR access.
    fn read_bar(&self, access: PciBarAccess, context: &mut dyn DeviceContext) -> DeviceResult<u64>;
    /// Writes one complete memory BAR access.
    fn write_bar(
        &self,
        access: PciBarAccess,
        value: u64,
        context: &mut dyn DeviceContext,
    ) -> DeviceResult;

    /// Handles one endpoint-owned conventional config read effect.
    fn read_config_effect(
        &self,
        _effect: PciConfigReadEffect,
        _context: &mut dyn DeviceContext,
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
        _context: &mut dyn DeviceContext,
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
        _context: &mut dyn DeviceContext,
    ) -> DeviceResult {
        Ok(())
    }
}

/// Non-capability token identifying one active endpoint binding generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EndpointRouteToken {
    device: DeviceId,
    generation: u64,
}

struct RoutedEndpoint {
    generation: u64,
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
        let token = EndpointRouteToken {
            device,
            generation: state.next_generation,
        };
        state.endpoints.insert(
            device,
            RoutedEndpoint {
                generation: token.generation,
                function,
            },
        );
        Ok(token)
    }

    fn invalidate(&self, token: EndpointRouteToken) -> Option<Arc<dyn PciFunction>> {
        let mut state = self.state.lock_irqsave();
        if state
            .endpoints
            .get(&token.device)
            .is_some_and(|entry| entry.generation == token.generation)
        {
            return state
                .endpoints
                .remove(&token.device)
                .map(|entry| entry.function);
        }
        None
    }

    fn endpoint(&self, token: EndpointRouteToken) -> DeviceResult<Arc<dyn PciFunction>> {
        let state = self.state.lock_irqsave();
        state
            .endpoints
            .get(&token.device)
            .filter(|entry| entry.generation == token.generation)
            .map(|entry| entry.function.clone())
            .ok_or_else(|| DeviceError::InvalidState {
                operation: "dispatch PCI endpoint route",
                detail: "PCI endpoint route token is stale".into(),
            })
    }
}

/// Host-owned root binding published as a typed bundle service.
pub struct PciRootBinding {
    host: DeviceNodeId,
    root: Arc<PciRootState>,
    router: Arc<EndpointRouter>,
}

impl PciRootBinding {
    /// Creates a binding service for one resolved host root.
    pub fn new(host: DeviceNodeId, root: Arc<PciRootState>) -> Self {
        Self {
            host,
            root,
            router: Arc::new(EndpointRouter::new()),
        }
    }

    /// Returns the host graph identity publishing this service.
    pub const fn host(&self) -> &DeviceNodeId {
        &self.host
    }

    pub(crate) fn matches_topology(&self, topology: &Arc<super::ResolvedPciTopology>) -> bool {
        Arc::ptr_eq(self.root.topology_arc(), topology)
    }

    pub(crate) fn bind(
        self: &Arc<Self>,
        function_id: &DeviceNodeId,
        device: DeviceId,
        function: Arc<dyn PciFunction>,
    ) -> DeviceManagerResult<PciBindingLease> {
        self.validate_config_effect_contract(function_id, function.as_ref())?;
        let token = self.router.activate(device, function)?;
        if let Err(error) = self.root.bind_endpoint(function_id, token) {
            drop(self.router.invalidate(token));
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

    /// Dispatches a BAR read after root lookup and token validation.
    pub fn read_bar(&self, address: u64, width: AccessWidth) -> DeviceResult<u64> {
        let (token, route) = self
            .root
            .resolve_bound_bar(address, width)
            .ok_or(DeviceError::NotFound)?;
        let endpoint = self.router.endpoint(token)?;
        let mut context = NoopDeviceContext::new(token.device);
        endpoint.read_bar(PciBarAccess { route }, &mut context)
    }

    /// Dispatches a BAR write after root lookup and token validation.
    pub fn write_bar(&self, address: u64, width: AccessWidth, value: u64) -> DeviceResult {
        let (token, route) = self
            .root
            .resolve_bound_bar(address, width)
            .ok_or(DeviceError::NotFound)?;
        let endpoint = self.router.endpoint(token)?;
        let mut context = NoopDeviceContext::new(token.device);
        endpoint.write_bar(PciBarAccess { route }, value, &mut context)
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
            PciConfigReadOutcome::Effect { token, effect } => {
                let endpoint = self.router.endpoint(token)?;
                let mut context = NoopDeviceContext::new(token.device);
                endpoint.read_config_effect(*effect, &mut context)
            }
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
            PciConfigWriteOutcome::Effect { token, effect } => {
                let endpoint = self.router.endpoint(token)?;
                let mut context = NoopDeviceContext::new(token.device);
                endpoint.write_config_effect(*effect, &mut context)
            }
            PciConfigWriteOutcome::CommandChanged { token, command } => {
                let Some(token) = token else {
                    return Ok(());
                };
                let endpoint = self.router.endpoint(token)?;
                let mut context = NoopDeviceContext::new(token.device);
                endpoint.command_changed(command, &mut context)
            }
        }
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
        // Teardown order from the design (§7.1): invalidate the binding
        // generation first so new validations fail, then withdraw the root
        // route, and only then release the strong endpoint reference kept
        // since dispatch validation - in-flight callbacks finish safely.
        let endpoint = self.binding.router.invalidate(self.token);
        self.binding.root.unbind_endpoint(self.token);
        drop(endpoint);
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
    use alloc::vec::Vec;

    use axdevice_base::{DeviceAccess, Resource};

    use super::*;
    use crate::{
        ConfigOffset, PciCapabilityEffectAccess, PciCapabilityEffectRegion, PciCapabilityId,
        PciCapabilitySpec, PciClass, PciConfigEffectId, PciEndpointIdentity, PciError,
        PciFunctionSpec, PciTopologyBuilder,
    };

    struct StubFunction {
        fail_command: bool,
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
            _context: &mut dyn DeviceContext,
        ) -> DeviceResult<u64> {
            Ok(0)
        }
        fn write_bar(
            &self,
            _access: PciBarAccess,
            _value: u64,
            _context: &mut dyn DeviceContext,
        ) -> DeviceResult {
            Ok(())
        }

        fn command_changed(
            &self,
            _command: PciCommandState,
            _context: &mut dyn DeviceContext,
        ) -> DeviceResult {
            if self.fail_command {
                return Err(DeviceError::Unsupported {
                    operation: "synchronize PCI command state",
                    detail: "test endpoint rejected the command transition".into(),
                });
            }
            Ok(())
        }
    }

    struct RecordingFunction {
        root: Arc<PciRootState>,
        bdf: PciBdf,
        reads: SpinLock<Vec<(PciConfigReadEffect, DeviceId, u64)>>,
        writes: SpinLock<Vec<(PciConfigWriteEffect, DeviceId)>>,
        commands: SpinLock<Vec<(PciCommandState, DeviceId)>>,
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
        fn supported_config_effects(&self) -> &[PciConfigEffectId] {
            const EFFECTS: &[PciConfigEffectId] = &[PciConfigEffectId::new(7)];
            EFFECTS
        }

        fn read_bar(
            &self,
            _access: PciBarAccess,
            _context: &mut dyn DeviceContext,
        ) -> DeviceResult<u64> {
            Ok(0)
        }

        fn write_bar(
            &self,
            _access: PciBarAccess,
            _value: u64,
            _context: &mut dyn DeviceContext,
        ) -> DeviceResult {
            Ok(())
        }

        fn read_config_effect(
            &self,
            effect: PciConfigReadEffect,
            context: &mut dyn DeviceContext,
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
            context: &mut dyn DeviceContext,
        ) -> DeviceResult {
            self.writes
                .lock_irqsave()
                .push((effect, context.device_id()));
            Ok(())
        }

        fn command_changed(
            &self,
            command: PciCommandState,
            context: &mut dyn DeviceContext,
        ) -> DeviceResult {
            self.commands
                .lock_irqsave()
                .push((command, context.device_id()));
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

        assert!(matches!(
            binding.bind(&function_id, DeviceId::new(7), function),
            Err(DeviceManagerError::InvalidConfig { .. })
        ));
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
        assert_eq!(first.generation, 1);
        assert!(router.endpoint(first).is_ok());

        let removed = router.invalidate(first).unwrap();
        assert!(Arc::ptr_eq(&removed, &function));
        let second = router.activate(device, Arc::clone(&function)).unwrap();
        assert_eq!(second.generation, 2);

        // The old generation can never dispatch again, before or after the
        // new binding exists.
        assert!(matches!(
            router.endpoint(first),
            Err(DeviceError::InvalidState { .. })
        ));
        drop(router.endpoint(second).unwrap());
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
            device,
            generation: token.generation + 1,
        };
        assert!(router.invalidate(forged).is_none());
        assert!(router.endpoint(token).is_ok());
        assert_eq!(
            router.invalidate(token).map(|arc| Arc::strong_count(&arc)),
            Some(2)
        );
        assert!(router.invalidate(token).is_none());
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
        root.bind_endpoint(&function_id, first).unwrap();
        assert!(matches!(
            root.bind_endpoint(&function_id, first),
            Err(PciError::FunctionAlreadyBound { .. })
        ));

        // Unbind invalidates the route; the same token never revives.
        drop(router.invalidate(first));
        root.unbind_endpoint(first);
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
        });
        let lease = binding
            .bind(&function_id, DeviceId::new(7), recording.clone())
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
        let _lease = binding
            .bind(&function_id, DeviceId::new(8), function)
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
