//! VM memory region planning.

use std::{alloc::Layout, vec::Vec};

use axvm_types::{GuestPhysAddr, VmMemConfig, VmMemMappingType};

use super::{AxVM, VMMemoryRegion};
use crate::{AxVmResult, ax_err_type, layout::VmRegionKind};

const VM_MEMORY_ALIGN: usize = 2 * 1024 * 1024;

/// Prepared memory regions for one VM.
#[derive(Debug, Clone)]
pub struct PreparedMemoryLayout {
    main_memory: VMMemoryRegion,
    regions: Vec<VMMemoryRegion>,
}

impl PreparedMemoryLayout {
    fn new(regions: Vec<VMMemoryRegion>) -> AxVmResult<Self> {
        let main_memory = regions
            .first()
            .cloned()
            .ok_or_else(|| ax_err_type!(InvalidData, "VM must have at least one memory region"))?;
        Ok(Self {
            main_memory,
            regions,
        })
    }

    /// Returns the primary memory region used by image and boot planning.
    pub fn main_memory(&self) -> &VMMemoryRegion {
        &self.main_memory
    }

    /// Returns all prepared VM memory regions.
    pub fn regions(&self) -> &[VMMemoryRegion] {
        &self.regions
    }
}

pub(crate) trait MemoryRegionMapper {
    fn prepared_memory_regions(&self) -> Vec<VMMemoryRegion>;
    fn allocate_memory_region(&self, layout: Layout, gpa: Option<GuestPhysAddr>) -> AxVmResult<()>;
    fn map_reserved_memory_region(&self, layout: Layout, gpa: Option<GuestPhysAddr>) -> AxVmResult;
    /// Allocates fresh backing frames for guest-visible firmware CODE and maps
    /// them without stage-2 WRITE permission.
    fn map_firmware_rom_region(&self, layout: Layout, gpa: Option<GuestPhysAddr>) -> AxVmResult;
}

impl MemoryRegionMapper for AxVM {
    fn prepared_memory_regions(&self) -> Vec<VMMemoryRegion> {
        self.memory_regions()
    }

    fn allocate_memory_region(&self, layout: Layout, gpa: Option<GuestPhysAddr>) -> AxVmResult<()> {
        self.alloc_memory_region(layout, gpa).map(|_| ())
    }

    fn map_reserved_memory_region(&self, layout: Layout, gpa: Option<GuestPhysAddr>) -> AxVmResult {
        self.map_reserved_memory_region(layout, gpa)
    }

    fn map_firmware_rom_region(&self, layout: Layout, gpa: Option<GuestPhysAddr>) -> AxVmResult {
        self.map_firmware_rom_region(layout, gpa)
    }
}

/// Turns configured memory regions into mapping plans.
///
/// When a firmware window boundary `(window base GPA, VARS prefix bytes)` is
/// configured, the matching region splits into a RAM-backed VARS prefix plan
/// plus a firmware-ROM CODE suffix plan; every other region maps unchanged.
/// Without a boundary the plans mirror the configuration one-to-one (legacy
/// behavior).
pub(crate) fn plan_memory_regions(
    configs: &[VmMemConfig],
    firmware_window: Option<(GuestPhysAddr, usize)>,
) -> AxVmResult<Vec<MemoryRegionPlan>> {
    let Some((window_base, vars_size)) = firmware_window else {
        return configs.iter().map(MemoryRegionPlan::from_config).collect();
    };

    let mut plans = Vec::with_capacity(configs.len() + 1);
    let mut window_split = false;
    for config in configs {
        if config.gpa != window_base.as_usize() {
            plans.push(MemoryRegionPlan::from_config(config)?);
            continue;
        }
        if config.map_type != VmMemMappingType::MapAlloc {
            return Err(ax_err_type!(
                InvalidInput,
                std::format!(
                    "bios_vars_size requires the firmware window at {:#x} to use MapAlloc, but it \
                     uses {:?}",
                    config.gpa,
                    config.map_type
                )
            ));
        }
        if config.size <= vars_size {
            return Err(ax_err_type!(
                InvalidInput,
                std::format!(
                    "firmware window {:#x} ({:#x} bytes) must be larger than bios_vars_size \
                     ({vars_size:#x} bytes) to leave a CODE suffix",
                    config.gpa,
                    config.size
                )
            ));
        }
        plans.push(MemoryRegionPlan::split_prefix(config, vars_size)?);
        plans.push(MemoryRegionPlan::split_suffix(config, vars_size)?);
        window_split = true;
    }
    if !window_split {
        return Err(ax_err_type!(
            InvalidInput,
            std::format!(
                "bios_vars_size requires a firmware window memory region at {:#x}, but none was \
                 configured",
                window_base.as_usize()
            )
        ));
    }
    Ok(plans)
}

pub(crate) struct MemoryLayoutBuilder<'a, M: MemoryRegionMapper + ?Sized> {
    mapper: &'a M,
    plans: &'a [MemoryRegionPlan],
}

impl<'a, M: MemoryRegionMapper + ?Sized> MemoryLayoutBuilder<'a, M> {
    pub(crate) const fn new(mapper: &'a M, plans: &'a [MemoryRegionPlan]) -> Self {
        Self { mapper, plans }
    }

    pub(crate) fn prepare(&self) -> AxVmResult<PreparedMemoryLayout> {
        let existing = self.mapper.prepared_memory_regions();
        if !existing.is_empty() {
            return PreparedMemoryLayout::new(existing);
        }

        for plan in self.plans {
            if plan.maps_reserved_memory() {
                self.mapper
                    .map_reserved_memory_region(plan.layout(), plan.configured_gpa())?;
            } else if plan.kind() == VmRegionKind::FirmwareRom {
                self.mapper
                    .map_firmware_rom_region(plan.layout(), plan.configured_gpa())?;
            } else {
                self.mapper
                    .allocate_memory_region(plan.layout(), plan.configured_gpa())?;
            }
        }

        PreparedMemoryLayout::new(self.mapper.prepared_memory_regions())
    }
}

/// One planned guest memory mapping operation.
#[derive(Debug, Clone)]
pub(crate) struct MemoryRegionPlan {
    configured_gpa: Option<GuestPhysAddr>,
    layout: Layout,
    map_type: VmMemMappingType,
    kind: VmRegionKind,
}

impl MemoryRegionPlan {
    pub(crate) fn from_config(config: &VmMemConfig) -> AxVmResult<Self> {
        Self::from_parts(config, VmRegionKind::Memory)
    }

    /// Plans the RAM-backed VARS prefix `[base, base + vars_size)` of a split
    /// firmware window.
    fn split_prefix(config: &VmMemConfig, vars_size: usize) -> AxVmResult<Self> {
        let prefix = VmMemConfig {
            gpa: config.gpa,
            size: vars_size,
            ..config.clone()
        };
        Self::from_parts(&prefix, VmRegionKind::Memory)
    }

    /// Plans the firmware-ROM CODE suffix `[base + vars_size, end)` of a split
    /// firmware window.
    fn split_suffix(config: &VmMemConfig, vars_size: usize) -> AxVmResult<Self> {
        let suffix = VmMemConfig {
            gpa: config.gpa.checked_add(vars_size).ok_or_else(|| {
                ax_err_type!(
                    InvalidInput,
                    std::format!(
                        "firmware window base {:#x} overflows when split at {vars_size:#x}",
                        config.gpa
                    )
                )
            })?,
            size: config.size - vars_size,
            ..config.clone()
        };
        Self::from_parts(&suffix, VmRegionKind::FirmwareRom)
    }

    fn from_parts(config: &VmMemConfig, kind: VmRegionKind) -> AxVmResult<Self> {
        let layout = Layout::from_size_align(config.size, VM_MEMORY_ALIGN).map_err(|err| {
            ax_err_type!(
                InvalidInput,
                std::format!("invalid VM memory region {config:?}: {err:?}")
            )
        })?;
        let configured_gpa = match config.map_type {
            VmMemMappingType::MapIdentical => None,
            VmMemMappingType::MapAlloc | VmMemMappingType::MapReserved => {
                Some(GuestPhysAddr::from(config.gpa))
            }
        };
        Ok(Self {
            configured_gpa,
            layout,
            map_type: config.map_type.clone(),
            kind,
        })
    }

    pub(crate) fn configured_gpa(&self) -> Option<GuestPhysAddr> {
        self.configured_gpa
    }

    pub(crate) const fn layout(&self) -> Layout {
        self.layout
    }

    pub(crate) const fn maps_reserved_memory(&self) -> bool {
        matches!(self.map_type, VmMemMappingType::MapReserved)
    }

    /// Returns the ownership class installed for this mapping.
    pub(crate) const fn kind(&self) -> VmRegionKind {
        self.kind
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        vec,
    };

    use super::*;
    use crate::layout::VmRegionKind;

    #[derive(Default)]
    struct FakeMemoryMapper {
        regions: RefCell<Vec<VMMemoryRegion>>,
        map_reserved_calls: Cell<usize>,
        allocate_calls: Cell<usize>,
        firmware_rom_calls: Cell<usize>,
    }

    impl MemoryRegionMapper for FakeMemoryMapper {
        fn prepared_memory_regions(&self) -> Vec<VMMemoryRegion> {
            self.regions.borrow().clone()
        }

        fn allocate_memory_region(
            &self,
            layout: Layout,
            gpa: Option<GuestPhysAddr>,
        ) -> AxVmResult<()> {
            self.allocate_calls.set(self.allocate_calls.get() + 1);
            let gpa = gpa.unwrap_or_else(|| GuestPhysAddr::from(0x5000_0000));
            self.regions.borrow_mut().push(VMMemoryRegion {
                gpa,
                hva: gpa.as_usize().into(),
                layout,
                needs_dealloc: true,
                kind: VmRegionKind::Memory,
            });
            Ok(())
        }

        fn map_reserved_memory_region(
            &self,
            layout: Layout,
            gpa: Option<GuestPhysAddr>,
        ) -> AxVmResult {
            let gpa = gpa.ok_or_else(|| ax_err_type!(InvalidInput, "reserved GPA is required"))?;
            self.map_reserved_calls
                .set(self.map_reserved_calls.get() + 1);
            self.regions.borrow_mut().push(VMMemoryRegion {
                gpa,
                hva: gpa.as_usize().into(),
                layout,
                needs_dealloc: false,
                kind: VmRegionKind::Memory,
            });
            Ok(())
        }

        fn map_firmware_rom_region(
            &self,
            layout: Layout,
            gpa: Option<GuestPhysAddr>,
        ) -> AxVmResult {
            self.firmware_rom_calls
                .set(self.firmware_rom_calls.get() + 1);
            let gpa =
                gpa.ok_or_else(|| ax_err_type!(InvalidInput, "firmware ROM GPA is required"))?;
            self.regions.borrow_mut().push(VMMemoryRegion {
                gpa,
                hva: gpa.as_usize().into(),
                layout,
                needs_dealloc: true,
                kind: VmRegionKind::FirmwareRom,
            });
            Ok(())
        }
    }

    #[test]
    fn memory_region_plan_preserves_mapping_intent_from_vm_config() {
        let alloc = VmMemConfig {
            gpa: 0x8000_0000,
            size: 0x20_0000,
            flags: 0,
            map_type: VmMemMappingType::MapAlloc,
        };
        let reserved = VmMemConfig {
            gpa: 0x9000_0000,
            size: 0x10_0000,
            flags: 0,
            map_type: VmMemMappingType::MapReserved,
        };
        let identical = VmMemConfig {
            gpa: 0,
            size: 0x10_0000,
            flags: 0,
            map_type: VmMemMappingType::MapIdentical,
        };

        assert_eq!(
            MemoryRegionPlan::from_config(&alloc)
                .unwrap()
                .configured_gpa(),
            Some(GuestPhysAddr::from(0x8000_0000))
        );
        assert!(
            !MemoryRegionPlan::from_config(&alloc)
                .unwrap()
                .maps_reserved_memory()
        );
        assert!(
            MemoryRegionPlan::from_config(&reserved)
                .unwrap()
                .maps_reserved_memory()
        );
        assert!(
            MemoryRegionPlan::from_config(&identical)
                .unwrap()
                .configured_gpa()
                .is_none()
        );
    }

    #[test]
    fn prepare_memory_layout_maps_configured_reserved_region_once() {
        let mapper = FakeMemoryMapper::default();
        let configs = vec![VmMemConfig {
            gpa: 0x4000_0000,
            size: 0x20_0000,
            flags: 0,
            map_type: VmMemMappingType::MapReserved,
        }];
        let plans = plan_memory_regions(&configs, None).unwrap();
        let builder = MemoryLayoutBuilder::new(&mapper, &plans);

        let layout = builder.prepare().unwrap();
        let again = builder.prepare().unwrap();

        assert_eq!(layout.main_memory().gpa, GuestPhysAddr::from(0x4000_0000));
        assert_eq!(layout.regions().len(), 1);
        assert_eq!(again.regions().len(), 1);
        assert_eq!(mapper.map_reserved_calls.get(), 1);
    }

    const WINDOW_BASE: usize = 0xffc0_0000;
    const WINDOW_SIZE: usize = 0x40_0000;
    const VARS_SIZE: usize = 0x84_000;

    fn window_config() -> VmMemConfig {
        VmMemConfig {
            gpa: WINDOW_BASE,
            size: WINDOW_SIZE,
            flags: 0,
            map_type: VmMemMappingType::MapAlloc,
        }
    }

    fn split_window_plans(configs: &[VmMemConfig]) -> AxVmResult<std::vec::Vec<MemoryRegionPlan>> {
        plan_memory_regions(configs, Some((GuestPhysAddr::from(WINDOW_BASE), VARS_SIZE)))
    }

    #[test]
    fn absent_boundary_keeps_legacy_plans() {
        let configs = vec![window_config()];
        let plans = plan_memory_regions(&configs, None).unwrap();

        assert_eq!(plans.len(), 1);
        assert_eq!(
            plans[0].configured_gpa(),
            Some(GuestPhysAddr::from(WINDOW_BASE))
        );
        assert_eq!(plans[0].kind(), VmRegionKind::Memory);
    }

    #[test]
    fn firmware_window_splits_into_vars_prefix_and_rom_suffix() {
        let configs = vec![window_config()];
        let plans = split_window_plans(&configs).unwrap();

        assert_eq!(plans.len(), 2);
        let vars = &plans[0];
        let code = &plans[1];
        assert_eq!(
            vars.configured_gpa(),
            Some(GuestPhysAddr::from(WINDOW_BASE))
        );
        assert_eq!(vars.layout().size(), VARS_SIZE);
        assert_eq!(vars.kind(), VmRegionKind::Memory);
        assert_eq!(
            code.configured_gpa(),
            Some(GuestPhysAddr::from(WINDOW_BASE + VARS_SIZE))
        );
        assert_eq!(code.layout().size(), WINDOW_SIZE - VARS_SIZE);
        assert_eq!(code.kind(), VmRegionKind::FirmwareRom);
    }

    #[test]
    fn firmware_window_split_leaves_other_regions_alone() {
        let low_ram = VmMemConfig {
            gpa: 0x0,
            size: 0x200_0000,
            flags: 0,
            map_type: VmMemMappingType::MapAlloc,
        };
        let configs = vec![low_ram, window_config()];
        let plans = split_window_plans(&configs).unwrap();

        assert_eq!(plans.len(), 3);
        assert_eq!(plans[0].kind(), VmRegionKind::Memory);
        assert_eq!(plans[0].layout().size(), 0x200_0000);
        assert_eq!(plans[2].kind(), VmRegionKind::FirmwareRom);
    }

    #[test]
    fn firmware_window_split_rejects_unusable_windows() {
        let missing_base = vec![];
        assert!(split_window_plans(&missing_base).is_err());

        let reserved_window = VmMemConfig {
            map_type: VmMemMappingType::MapReserved,
            ..window_config()
        };
        assert!(split_window_plans(&[reserved_window]).is_err());

        let tiny_window = VmMemConfig {
            size: VARS_SIZE - 0x1000,
            ..window_config()
        };
        assert!(split_window_plans(&[tiny_window]).is_err());
    }

    #[test]
    fn vars_frames_survive_region_rebuild() {
        let mapper = FakeMemoryMapper::default();
        let configs = vec![
            VmMemConfig {
                gpa: 0x0,
                size: 0x200_0000,
                flags: 0,
                map_type: VmMemMappingType::MapAlloc,
            },
            window_config(),
        ];
        let plans = split_window_plans(&configs).unwrap();
        let builder = MemoryLayoutBuilder::new(&mapper, &plans);

        builder.prepare().unwrap();
        let again = builder.prepare().unwrap();

        // A rebuild (guest reset) must reuse the seeded frames instead of
        // re-allocating or re-mapping CODE as writable RAM.
        assert_eq!(mapper.allocate_calls.get(), 2); // guest RAM + VARS prefix
        assert_eq!(mapper.firmware_rom_calls.get(), 1); // CODE suffix only
        assert_eq!(again.regions().len(), 3);
        let kinds = again
            .regions()
            .iter()
            .map(|region| region.kind())
            .collect::<std::vec::Vec<_>>();
        assert_eq!(
            kinds,
            [
                VmRegionKind::Memory,
                VmRegionKind::Memory,
                VmRegionKind::FirmwareRom
            ]
        );
    }
}
