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
