//! Runtime models shared by graph-backed PCI host and endpoint nodes.

use alloc::{
    string::ToString,
    sync::{Arc, Weak},
};

use ax_sync::SpinLock;

use super::{
    super::{
        ecam::PciEcamDevice,
        memory::PciMemoryApertureDevice,
        root::{PciRootState, SharedPciRoot},
    },
    ECAM_SLOT, MEMORY_SLOT, PciEndpointModel, PciHostPlan, PciHostResourceRequirements,
    graph_integration_error, pci_build_error,
};
use crate::{
    DeviceBuildContext, DeviceBundle, DeviceFirmwareSpec, DeviceManagerResult, DeviceModel,
    DeviceNodeId, DeviceRequirements, PciError, PciResult,
};

pub(super) struct PciHostModel {
    resources: PciHostResourceRequirements,
    shared: Arc<PciBusShared>,
}

impl PciHostModel {
    pub(super) const fn new(
        resources: PciHostResourceRequirements,
        shared: Arc<PciBusShared>,
    ) -> Self {
        Self { resources, shared }
    }
}

impl DeviceModel for PciHostModel {
    fn requirements(&self) -> DeviceManagerResult<DeviceRequirements> {
        self.resources.device_requirements()
    }

    fn firmware(&self) -> DeviceFirmwareSpec {
        DeviceFirmwareSpec::None
    }

    fn build(&self, context: &mut DeviceBuildContext<'_>) -> DeviceManagerResult<DeviceBundle> {
        let claimed_ecam = context.mmio(ECAM_SLOT)?;
        let claimed_memory = context.mmio(MEMORY_SLOT)?;
        let plan = self
            .shared
            .plan()
            .map_err(|error| pci_build_error("build PCI host", error))?;
        let aperture = plan.topology.memory_aperture();
        // Drift guard: the claimed ECAM/memory ranges must equal the resolved
        // host windows before any runtime device, BAR route, or lease is
        // published for this bus.
        if claimed_ecam
            != (
                plan.ecam_window.start,
                plan.ecam_window.end - plan.ecam_window.start,
            )
            || claimed_memory != (aperture.start, aperture.end - aperture.start)
        {
            return Err(graph_integration_error(
                "build PCI host",
                "claimed host ranges differ from the resolved PCI topology",
            ));
        }

        let root: SharedPciRoot = Arc::new(SpinLock::new(PciRootState::new(&plan.topology)));
        self.shared
            .publish_root(&root)
            .map_err(|error| pci_build_error("publish PCI root complex", error))?;
        let ecam_device = Arc::new(PciEcamDevice::new(plan.ecam_window.clone(), root.clone()));
        let memory_device = Arc::new(PciMemoryApertureDevice::new(aperture.clone(), root));
        let mut bundle = DeviceBundle::new();
        bundle.add_device(ecam_device);
        bundle.add_device(memory_device.clone());
        bundle.add_lifecycle(memory_device);
        Ok(bundle)
    }
}

pub(super) struct PciEndpointGraphModel {
    id: DeviceNodeId,
    model: Arc<dyn PciEndpointModel>,
    shared: Arc<PciBusShared>,
}

impl PciEndpointGraphModel {
    pub(super) const fn new(
        id: DeviceNodeId,
        model: Arc<dyn PciEndpointModel>,
        shared: Arc<PciBusShared>,
    ) -> Self {
        Self { id, model, shared }
    }
}

impl DeviceModel for PciEndpointGraphModel {
    fn requirements(&self) -> DeviceManagerResult<DeviceRequirements> {
        self.model.requirements()
    }

    fn firmware(&self) -> DeviceFirmwareSpec {
        DeviceFirmwareSpec::None
    }

    fn build(&self, context: &mut DeviceBuildContext<'_>) -> DeviceManagerResult<DeviceBundle> {
        let endpoint = self.model.build(context)?;
        let (function, mut bundle) = endpoint.into_parts();
        let root = self
            .shared
            .root_lock(&self.id)
            .map_err(|error| pci_build_error("bind PCI endpoint", error))?;
        super::super::root::bind_function(&root, &self.id, function, &mut bundle)
            .map_err(|error| pci_build_error("bind PCI endpoint", error))?;
        Ok(bundle)
    }
}

pub(super) struct PciBusShared {
    state: SpinLock<PciBusSharedState>,
}

struct PciBusSharedState {
    plan: Option<Arc<PciHostPlan>>,
    root: Weak<SpinLock<PciRootState>>,
}

impl PciBusShared {
    pub(super) const fn new() -> Self {
        Self {
            state: SpinLock::new(PciBusSharedState {
                plan: None,
                root: Weak::new(),
            }),
        }
    }

    pub(super) fn publish_plan(&self, plan: Arc<PciHostPlan>) -> PciResult {
        let mut state = self.state.lock_irqsave();
        if state.plan.is_some() {
            return Err(PciError::TopologyAlreadyResolved);
        }
        state.plan = Some(plan);
        Ok(())
    }

    fn plan(&self) -> PciResult<Arc<PciHostPlan>> {
        self.state
            .lock_irqsave()
            .plan
            .clone()
            .ok_or(PciError::TopologyNotResolved)
    }

    fn publish_root(&self, root: &SharedPciRoot) -> PciResult {
        let mut state = self.state.lock_irqsave();
        if state.root.upgrade().is_some() {
            return Err(PciError::HostAlreadyRegistered);
        }
        state.root = Arc::downgrade(root);
        Ok(())
    }

    fn root_lock(&self, function: &DeviceNodeId) -> PciResult<SharedPciRoot> {
        self.state
            .lock_irqsave()
            .root
            .upgrade()
            .ok_or_else(|| PciError::HostUnavailable {
                function: function.to_string(),
            })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{
        DeviceGraphBuilder, DeviceNodeSpec, ResourcePools,
        interrupt::InterruptRegistry,
        pci::{PciClass, PciEndpointIdentity, PciFunctionSpec, PciTopologyBuilder},
    };

    /// Model that only exists so the planner hands out host-shaped claims.
    struct RequirementsModel(PciHostResourceRequirements);

    impl DeviceModel for RequirementsModel {
        fn requirements(&self) -> DeviceManagerResult<DeviceRequirements> {
            self.0.device_requirements()
        }

        fn firmware(&self) -> DeviceFirmwareSpec {
            DeviceFirmwareSpec::None
        }

        fn build(
            &self,
            _context: &mut DeviceBuildContext<'_>,
        ) -> DeviceManagerResult<DeviceBundle> {
            unreachable!("the drift-guard test never builds through this model")
        }
    }

    /// Publishes a resolved plan whose windows deliberately differ from the
    /// ranges the planner-backed claim set below hands to `build`.
    fn published_plan_and_model() -> (Arc<PciBusShared>, PciHostModel) {
        let shared = Arc::new(PciBusShared::new());
        let requirements = PciHostResourceRequirements::new(0x10_0000, 0x10_0000).unwrap();
        let model = PciHostModel::new(requirements, shared.clone());
        let mut builder = PciTopologyBuilder::new();
        builder
            .add_function(PciFunctionSpec::new(
                DeviceNodeId::new("endpoint").unwrap(),
                PciEndpointIdentity::new(0x1234, 0x5678, PciClass::new(0xff, 0, 0)),
            ))
            .unwrap();
        let topology = Arc::new(builder.resolve(0x2000_0000..0x2010_0000).unwrap());
        shared
            .publish_plan(Arc::new(PciHostPlan {
                topology,
                ecam_window: 0x3000_0000..0x3010_0000,
            }))
            .unwrap();
        (shared, model)
    }

    #[test]
    fn host_build_rejects_claims_that_drift_from_the_resolved_plan() {
        let (shared, model) = published_plan_and_model();

        // Real planner claims for a host whose slots resolve far away from
        // the hand-published windows above.
        let mut graph = DeviceGraphBuilder::new();
        graph
            .add(DeviceNodeSpec::virtual_device(
                DeviceNodeId::new("pci-host").unwrap(),
                Arc::new(RequirementsModel(
                    PciHostResourceRequirements::new(0x10_0000, 0x10_0000).unwrap(),
                )),
            ))
            .unwrap();
        let mut pools = ResourcePools::new();
        pools.add_auto_mmio(0x1000_0000..0x1100_0000).unwrap();
        let resolved = graph.declare().unwrap().resolve(pools).unwrap();
        let claims = resolved.resource_plan().claim_device("pci-host").unwrap();

        let interrupts = InterruptRegistry::new();
        let mut context = DeviceBuildContext::planned(&interrupts, claims);
        let error = match model.build(&mut context) {
            Err(error) => error,
            Ok(_) => unreachable!("drifting claims must fail the host build"),
        };

        assert!(
            error
                .to_string()
                .contains("claimed host ranges differ from the resolved PCI topology"),
            "unexpected error: {error}"
        );
        // The drift guard must fire before anything is published.
        assert!(matches!(
            shared.root_lock(&DeviceNodeId::new("pci-host").unwrap()),
            Err(PciError::HostUnavailable { .. })
        ));
    }
}
