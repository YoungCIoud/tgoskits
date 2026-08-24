//! Runtime models shared by graph-backed PCI host and endpoint nodes.

use alloc::{
    string::ToString,
    sync::{Arc, Weak},
};

use ax_sync::SpinLock;

use super::{
    super::ecam::PciRootComplex, ECAM_SLOT, MEMORY_SLOT, PciEndpointModel,
    PciHostResourceRequirements, graph_integration_error, pci_build_error,
};
use crate::{
    DeviceBuildContext, DeviceBundle, DeviceFirmwareSpec, DeviceLifecycle, DeviceManagerResult,
    DeviceModel, DeviceNodeId, DeviceRegistration, DeviceRequirements, PciError, PciResult,
    ResolvedPciTopology,
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
        let ecam = context.mmio(ECAM_SLOT)?;
        let memory = context.mmio(MEMORY_SLOT)?;
        let topology = self
            .shared
            .topology()
            .map_err(|error| pci_build_error("build PCI host", error))?;
        let host = topology.host();
        let aperture = host.memory_aperture();
        if ecam != (host.ecam_base(), host.ecam_size())
            || memory != (aperture.start, aperture.end - aperture.start)
        {
            return Err(graph_integration_error(
                "build PCI host",
                "claimed host ranges differ from the resolved PCI topology",
            ));
        }

        let root = Arc::new(PciRootComplex::new(topology));
        self.shared
            .publish_root(&root)
            .map_err(|error| pci_build_error("publish PCI root complex", error))?;
        let device: Arc<dyn crate::Device> = root.clone();
        let lifecycle: Arc<dyn DeviceLifecycle> = root;
        Ok(
            DeviceBundle::from_registration(DeviceRegistration::Device(device))
                .with_lifecycle(lifecycle),
        )
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
            .root(&self.id)
            .map_err(|error| pci_build_error("bind PCI endpoint", error))?;
        root.bind_function(&self.id, function, &mut bundle)
            .map_err(|error| pci_build_error("bind PCI endpoint", error))?;
        Ok(bundle)
    }
}

pub(super) struct PciBusShared {
    state: SpinLock<PciBusSharedState>,
}

struct PciBusSharedState {
    topology: Option<Arc<ResolvedPciTopology>>,
    root: Weak<PciRootComplex>,
}

impl PciBusShared {
    pub(super) const fn new() -> Self {
        Self {
            state: SpinLock::new(PciBusSharedState {
                topology: None,
                root: Weak::new(),
            }),
        }
    }

    pub(super) fn publish_topology(&self, topology: Arc<ResolvedPciTopology>) -> PciResult {
        let mut state = self.state.lock_irqsave();
        if state.topology.is_some() {
            return Err(PciError::TopologyAlreadyResolved);
        }
        state.topology = Some(topology);
        Ok(())
    }

    fn topology(&self) -> PciResult<Arc<ResolvedPciTopology>> {
        self.state
            .lock_irqsave()
            .topology
            .clone()
            .ok_or(PciError::TopologyNotResolved)
    }

    fn publish_root(&self, root: &Arc<PciRootComplex>) -> PciResult {
        let mut state = self.state.lock_irqsave();
        if state.root.upgrade().is_some() {
            return Err(PciError::HostAlreadyRegistered);
        }
        state.root = Arc::downgrade(root);
        Ok(())
    }

    fn root(&self, function: &DeviceNodeId) -> PciResult<Arc<PciRootComplex>> {
        self.state
            .lock_irqsave()
            .root
            .upgrade()
            .ok_or_else(|| PciError::HostUnavailable {
                function: function.to_string(),
            })
    }
}
