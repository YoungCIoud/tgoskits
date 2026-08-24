//! Resolved-device-graph adapter and transactional function binding.

mod runtime;

use alloc::{collections::BTreeMap, format, string::ToString, sync::Arc};
use core::fmt;

use runtime::{PciBusShared, PciEndpointGraphModel, PciHostModel};

use super::{
    FOUR_GIB, PciError, PciFunction, PciFunctionSpec, PciHostBridgeConfig, PciResult,
    PciTopologyBuilder, ResolvedPciTopology, ecam::ECAM_SIZE,
};
use crate::{
    DeviceBuildContext, DeviceBundle, DeviceManagerError, DeviceManagerResult, DeviceModel,
    DeviceNodeId, DeviceNodeSpec, DeviceRequirements, ResolvedDeviceGraph, ResourceRequest,
    ResourceSlot,
};

const ECAM_SLOT: &str = "ecam";
const MEMORY_SLOT: &str = "memory";

/// Planner requirements for one graph-backed PCI host bridge.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciHostResourceRequirements {
    memory_size: u64,
    memory_alignment: u64,
}

impl PciHostResourceRequirements {
    /// Creates requirements for one 32-bit non-prefetchable memory aperture.
    ///
    /// ECAM is always declared as a 1 MiB, 1 MiB-aligned automatic MMIO
    /// allocation. The memory aperture is also automatically allocated.
    ///
    /// # Errors
    ///
    /// Returns [`PciError::InvalidHostAperture`] when the memory size is zero
    /// or above 4 GiB, or when alignment is not a power of two.
    pub fn new(memory_size: u64, memory_alignment: u64) -> PciResult<Self> {
        if memory_size == 0 || memory_size > FOUR_GIB {
            return Err(PciError::InvalidHostAperture {
                detail: "graph-planned memory aperture size must be within 1..=4 GiB",
            });
        }
        if !memory_alignment.is_power_of_two() {
            return Err(PciError::InvalidHostAperture {
                detail: "graph-planned memory aperture alignment must be a power of two",
            });
        }
        Ok(Self {
            memory_size,
            memory_alignment,
        })
    }

    /// Returns the requested PCI memory aperture size.
    pub const fn memory_size(self) -> u64 {
        self.memory_size
    }

    /// Returns the requested PCI memory aperture alignment.
    pub const fn memory_alignment(self) -> u64 {
        self.memory_alignment
    }

    fn device_requirements(self) -> DeviceManagerResult<DeviceRequirements> {
        DeviceRequirements::new()
            .with_mmio(
                ResourceSlot::new(ECAM_SLOT)?,
                ECAM_SIZE,
                ECAM_SIZE,
                ResourceRequest::Auto,
            )?
            .with_mmio(
                ResourceSlot::new(MEMORY_SLOT)?,
                self.memory_size,
                self.memory_alignment,
                ResourceRequest::Auto,
            )
    }
}

/// Builds PCI host and endpoint nodes before freezing their shared topology.
pub struct PciBusGraphBuilder {
    host_id: DeviceNodeId,
    host_resources: PciHostResourceRequirements,
    topology: PciTopologyBuilder,
    host_model: Arc<dyn DeviceModel>,
    endpoint_models: BTreeMap<DeviceNodeId, Arc<dyn DeviceModel>>,
    shared: Arc<PciBusShared>,
}

impl fmt::Debug for PciBusGraphBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PciBusGraphBuilder")
            .field("host_id", &self.host_id)
            .field("host_resources", &self.host_resources)
            .field("topology", &self.topology)
            .finish_non_exhaustive()
    }
}

impl PciBusGraphBuilder {
    /// Creates one unresolved graph-backed PCI bus.
    pub fn new(host_id: DeviceNodeId, host_resources: PciHostResourceRequirements) -> Self {
        let shared = Arc::new(PciBusShared::new());
        let host_model: Arc<dyn DeviceModel> =
            Arc::new(PciHostModel::new(host_resources, shared.clone()));
        Self {
            host_id,
            host_resources,
            topology: PciTopologyBuilder::new(),
            host_model,
            endpoint_models: BTreeMap::new(),
            shared,
        }
    }

    /// Creates the host node that must be inserted into the architecture graph.
    pub fn host_node(&self) -> DeviceNodeSpec {
        DeviceNodeSpec::virtual_device(self.host_id.clone(), self.host_model.clone())
    }

    /// Adds a pure PCI declaration and creates its dependent endpoint node.
    ///
    /// # Errors
    ///
    /// Returns [`PciError::DuplicateFunction`] when the endpoint identity was
    /// already added to this bus.
    pub fn endpoint_node(
        &mut self,
        function: PciFunctionSpec,
        model: Arc<dyn PciEndpointModel>,
    ) -> PciResult<DeviceNodeSpec> {
        let id = function.id.clone();
        self.topology.add_function(function)?;
        let wrapper: Arc<dyn DeviceModel> = Arc::new(PciEndpointGraphModel::new(
            id.clone(),
            model,
            self.shared.clone(),
        ));
        self.endpoint_models.insert(id.clone(), wrapper.clone());
        Ok(DeviceNodeSpec::virtual_device(id, wrapper).with_dependency(self.host_id.clone()))
    }

    /// Freezes PCI BDF/BAR state from this graph's resolved host resources.
    ///
    /// This must run after [`ResolvedDeviceGraph`] creation and before any PCI
    /// graph node is built.
    ///
    /// # Errors
    ///
    /// Returns a device-manager error when the host node or slots are absent,
    /// the resolved ranges violate the PCI host contract, or topology
    /// resolution fails. No topology is published on failure.
    pub fn resolve(self, graph: &ResolvedDeviceGraph) -> DeviceManagerResult<ResolvedPciBus> {
        self.validate_graph_models(graph)?;
        let resources = graph.resources_for(&self.host_id)?;
        let ecam = resources.mmio(&ResourceSlot::new(ECAM_SLOT)?)?;
        let memory = resources.mmio(&ResourceSlot::new(MEMORY_SLOT)?)?;
        self.validate_resolved_sizes(ecam, memory)?;
        let memory_end = memory.0.checked_add(memory.1).ok_or_else(|| {
            graph_integration_error(
                "resolve PCI host resources",
                "memory aperture overflows u64",
            )
        })?;
        let host = PciHostBridgeConfig::new(ecam.0, memory.0..memory_end)
            .map_err(|error| pci_config_error("resolve PCI host resources", error))?;
        let topology = Arc::new(
            self.topology
                .resolve(host)
                .map_err(|error| pci_config_error("resolve PCI topology", error))?,
        );
        self.shared
            .publish_topology(topology.clone())
            .map_err(|error| pci_build_error("publish PCI topology", error))?;
        Ok(ResolvedPciBus {
            host_id: self.host_id,
            topology,
            _shared: self.shared,
        })
    }

    fn validate_graph_models(&self, graph: &ResolvedDeviceGraph) -> DeviceManagerResult {
        validate_exact_model(graph, &self.host_id, &self.host_model)?;
        for (id, model) in &self.endpoint_models {
            validate_exact_model(graph, id, model)?;
        }
        Ok(())
    }

    fn validate_resolved_sizes(&self, ecam: (u64, u64), memory: (u64, u64)) -> DeviceManagerResult {
        if ecam.1 != ECAM_SIZE {
            return Err(graph_integration_error(
                "resolve PCI host resources",
                "ECAM slot does not have the declared 1 MiB size",
            ));
        }
        if memory.1 != self.host_resources.memory_size {
            return Err(graph_integration_error(
                "resolve PCI host resources",
                "memory slot size differs from the host declaration",
            ));
        }
        if memory.0 & (self.host_resources.memory_alignment - 1) != 0 {
            return Err(graph_integration_error(
                "resolve PCI host resources",
                "memory slot base violates the declared alignment",
            ));
        }
        Ok(())
    }
}

/// Immutable PCI bus view shared by architecture firmware and runtime plans.
pub struct ResolvedPciBus {
    host_id: DeviceNodeId,
    topology: Arc<ResolvedPciTopology>,
    _shared: Arc<PciBusShared>,
}

impl fmt::Debug for ResolvedPciBus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedPciBus")
            .field("host_id", &self.host_id)
            .field("topology", &self.topology)
            .finish()
    }
}

impl ResolvedPciBus {
    /// Returns the graph node that owns ECAM and the memory aperture.
    pub const fn host_id(&self) -> &DeviceNodeId {
        &self.host_id
    }

    /// Returns the single topology consumed by firmware and runtime creation.
    pub fn topology(&self) -> &ResolvedPciTopology {
        &self.topology
    }
}

/// Builds one PCI function after consuming its graph-planned resources.
pub trait PciEndpointModel: Send + Sync {
    /// Declares endpoint-owned resources such as an MSI range.
    ///
    /// # Errors
    ///
    /// Returns a device-manager error when the validated endpoint
    /// configuration cannot be represented by planner requirements.
    fn requirements(&self) -> DeviceManagerResult<DeviceRequirements>;

    /// Builds the BAR handler and all endpoint-owned registrations.
    ///
    /// # Errors
    ///
    /// Returns a device-manager error when a planned claim cannot be consumed
    /// or endpoint runtime construction fails.
    fn build(&self, context: &mut DeviceBuildContext<'_>)
    -> DeviceManagerResult<PciEndpointBundle>;
}

/// Runtime function and registrations produced by one endpoint model.
pub struct PciEndpointBundle {
    function: Arc<dyn PciFunction>,
    bundle: DeviceBundle,
}

impl fmt::Debug for PciEndpointBundle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PciEndpointBundle")
            .field("function", &self.function.name())
            .field("bundle_is_empty", &self.bundle.is_empty())
            .finish()
    }
}

impl PciEndpointBundle {
    /// Creates an endpoint with no additional runtime registrations.
    pub fn new(function: Arc<dyn PciFunction>) -> Self {
        Self {
            function,
            bundle: DeviceBundle::new(),
        }
    }

    /// Creates an endpoint with lifecycle, polling, interrupt, or device
    /// registrations that must share the function binding lifetime.
    pub fn with_bundle(function: Arc<dyn PciFunction>, bundle: DeviceBundle) -> Self {
        Self { function, bundle }
    }

    fn into_parts(self) -> (Arc<dyn PciFunction>, DeviceBundle) {
        (self.function, self.bundle)
    }
}

fn validate_exact_model(
    graph: &ResolvedDeviceGraph,
    id: &DeviceNodeId,
    expected: &Arc<dyn DeviceModel>,
) -> DeviceManagerResult {
    let actual = graph
        .nodes()
        .find(|node| node.id() == id)
        .and_then(|node| node.model())
        .ok_or_else(|| {
            graph_integration_error(
                "resolve PCI graph models",
                format!("PCI node {id} is absent or has no runtime model"),
            )
        })?;
    if !Arc::ptr_eq(actual, expected) {
        return Err(graph_integration_error(
            "resolve PCI graph models",
            format!("PCI node {id} does not retain the model created by this bus builder"),
        ));
    }
    Ok(())
}

fn pci_config_error(operation: &'static str, error: PciError) -> DeviceManagerError {
    DeviceManagerError::InvalidConfig {
        operation,
        detail: error.to_string(),
    }
}

fn pci_build_error(operation: &'static str, error: PciError) -> DeviceManagerError {
    DeviceManagerError::InvalidState {
        operation,
        detail: error.to_string(),
    }
}

fn graph_integration_error(
    operation: &'static str,
    detail: impl Into<alloc::string::String>,
) -> DeviceManagerError {
    DeviceManagerError::InvalidConfig {
        operation,
        detail: detail.into(),
    }
}
