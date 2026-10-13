# x86 UEFI Writable VARS — Implementation Plan

Worktree: `.worktrees/uefi-writable-vars` (feat/x86-uefi-writable-vars @ feat/fw-cfg-e820-memmap). MVP: guest OVMF variable writes persist across guest reset within VM lifetime; firmware CODE read-only; CODE/VARS boundary = explicit optional GuestConfig kernel field (absent ⇒ byte-identical legacy); window declared Reserved via existing `non_ram_regions` producers; red-first host tests; E2E wired for qemu-acpi-ovmf TOML, NOT run locally (no /dev/kvm).

## 1. Goals / Non-goals

- G1: Optional split of 4 MiB firmware window @ `bios_load_gpa=0xffc00000` into CODE prefix (RO) + VARS suffix (RW, RAM-backed, reset-persistent).
- G2: Boundary absent ⇒ identical config parse bytes, identical stage-2 mappings, identical loader behavior.
- G3: Declare firmware window `E820NonRamType::Reserved` via `FwCfgPlatformConfig::non_ram_regions` producer site(s).
- G4: Red-first host tests at axvmconfig/axvm layers; E2E entry added, not executed.
- Non-goals: persistence across Axvisor restarts; pflash write-back; SecureBoot; variable-services emulation.

## 2. Current-behavior finding (verification #1)

**The firmware window IS guest-writable today.** Chain: `AxVMResources.prepare_guest_address_space` collects `self.memory_regions` into `GuestOwnedRegion::new(gpa, size, VmRegionKind::Memory)` — no permission attribute exists on the region type (virtualization/axvm/src/vm/prepare/address_space.rs:106–113; reserved ranges become `VmRegionKind::Reserved`, :122–124). `build_address_layout` reserves each owned region uniformly (virtualization/axvm/src/layout.rs:452–494) and maps them via one loop (`map_linear(gpa,hpa,size,flags)`, address_space.rs:87–99); only devices get an explicit flag constructor (`READ|WRITE|USER|DEVICE`, layout.rs:496–498). `impl VmRegionKind` carries only a display `name()`, no flags (layout.rs:624–634). Since `[0xffc00000,0x400000)` rides in `kernel.memory_regions` as plain `Memory`, it is mapped like guest RAM (uniformly writable; exact RAM flag literal not pinned to a line — see §7). **Honest implication:** nothing enforces ROM semantics today; OVMF runs, but any guest store into CODE persists silently and VARS writes do NOT survive anything that re-copies the blob. MVP therefore needs (a) a permission dimension in the shared layout/mapping path, and (b) VARS backed by VM-owned pages that the reset path never re-copies.

## 3. Design

Chosen mechanism — **region-kind split in the shared stage-2 planner + VM-owned VARS buffer**:
- New optional field `GuestConfig.kernel.bios_vars_size: Option<usize>` (bytes of the firmware-window *prefix* assembled as VARS by `assemble_ovmf_image`, which already lays out VARS-prefix-then-CODE in scripts/axbuild/src/axvisor/test/ovmf.rs). Serde default `None`.
- When `Some(n)` and `enable_bios`: axvm splits the single firmware `VmMemConfig` into two `GuestOwnedRegion`s: CODE `[base, base+n)` as new `VmRegionKind::FirmwareRom`, VARS `[base+n, base+0x400000)` as `Memory` but backed by freshly allocated VM-owned frames (registered in `AxVMResources.memory_regions`) instead of the static blob pages. Planner gains one flag rule: `FirmwareRom ⇒ READ | EXEC-ish (no WRITE) | USER`; VARS keeps normal RAM flags.
- Loader change (virtualization/axvm/src/arch/x86_64/boot/mod.rs, load_boot_image_from_memory/filesystem): copy blob CODE portion to its GPA as today; skip copying the VARS prefix range into guest-visible memory beyond initial content — seed VARS frames once at VM create; guest reset re-runs vCPU reset only, so contents persist (G1).
- Reserved declaration: extend the `non_ram_regions` producer in arch/x86_64/boot/mod.rs to emit `[bios_load_gpa, 0x400000)` as `E820NonRamType::Reserved` when `enable_bios`; loongarch64 producer (same file pattern) left empty (no OVMF there in MVP).
- Alternatives rejected: (A) pflash/MMIO device emulation of the varstore — full device model + write-back, explicitly a non-goal and far larger surface. (B) Per-entry permission column on `memory_regions` TOML tuples — mutates an existing schema consumed by all four arches and risks breaking legacy byte-identity (G2); a separate opt-in field keeps legacy parsing untouched. (C) Fault-and-emulate stores to the window — reintroduces variable-services-like emulation (non-goal) and adds a hot trap path.

## 4. Step-by-step changes (file-level)

1. `virtualization/axvmconfig/src/lib.rs`: add `bios_vars_size: Option<usize>` to kernel section (~:297–372 struct block); validation ~:379–427 — error unless `None`, or `n>0 && n%0x1000==0` (page-aligned; caller validates ≤ window size against actual region). Typed error text.
2. `os/axvisor/src/config.rs`: thread field through `build_axvm_config` (:179–233) into `AxVMConfigParams` next to `memory_regions` (:226); extend `sync_axvm_config_from_crate_config` (:235–237).
3. `virtualization/axvm/src/config.rs`: add field to `AxVMConfigParams`/`AxVMConfig` (+getter/setter mirroring `set_memory_regions`, :277).
4. `virtualization/axvm/src/vm/prepare/address_space.rs` + `virtualization/axvm/src/layout.rs` (+ `axvm-types` if `VmRegionKind` lives there): add `FirmwareRom` kind; split rule in `guest_owned_regions`; flag rule in planner; overlap-tolerance table (layout.rs:615–621) updated if needed.
5. `virtualization/axvm/src/arch/x86_64/boot/mod.rs`: loader split-copy + VARS frame seeding; `non_ram_regions` producer emits Reserved window.
6. `scripts/axbuild/src/axvisor/test/{ovmf,initramfs}.rs` + qemu-acpi-ovmf TOML: expose/set boundary env/TOML knob; E2E entry only.
7. Docs touch-up where the boot flow is documented (lifecycle-internals.md mentions region rebuilds).

## 5. Red-first test matrix

- axvmconfig `src/test.rs`: `bios_vars_size_parses_when_present` — before fix: field unknown ⇒ serde deny/error (fails). After: parses value.
- axvmconfig `src/test.rs`: `bios_vars_size_rejects_unaligned_and_zero` — before fix: accepted ⇒ assert-Err fails. After: typed error.
- axvm layout tests (`layout.rs` tests module, cf. `prepare_memory_layout_maps_configured_reserved_region_once` in vm/memory.rs:234): `firmware_rom_region_maps_without_write_flag` — before fix: kind doesn't exist / WRITE present ⇒ compile-or-assert failure. After: mapping flags exclude WRITE.
- axvm x86 boot host test: `vars_frames_survive_region_rebuild` — before fix: rebuild re-copies blob ⇒ content equality fails. After: seeded content retained.
- Legacy identity: `absent_boundary_keeps_legacy_layout` — before fix passes trivially; guards G2 permanently (assert single Memory region, unchanged flags).

## 6. Validation commands

- `cargo xtask clippy --package axvmconfig` ; `--package axvm` ; `--package axvisor` (baseline lint large_enum_variant at axdevice fw_cfg/factory.rs:63 pre-exists, ignore).
- `cargo xtask test --package axvmconfig` / `--package axvm` (host unit/red-first suites).
- `cargo fmt` after edits. E2E qemu-acpi-ovmf: wire only; do not run locally (no /dev/kvm) — note in PR body.

## 7. Risks & open questions

- OPEN (unverified): exact `MappingFlags` literal used for `VmRegionKind::Memory` mappings (constructed outside layout.rs:496; likely in planner finish()/map path). Implementer pins it before adding the ROM rule.
- OPEN (unverified): guest-reset path — confirm axvm does NOT rerun `load_boot_image_*` on guest reset (design assumes once-at-create seeding). If it does, gate re-copy to CODE only.
- LoongArch shares the mapping code path (see §8-V3): the permission dimension lands in shared code; keep it inert unless the new field is set, so loongarch behavior is unchanged by construction.
- EXEC permission semantics differ per arch (x86 NX bit vs loongarch); FirmwareRom flags must be expressed as "no WRITE" plus whatever READ/EXEC bits RAM uses today, verified per-arch at map time.

## 8. Verification log

- V1: Stage-2 owner = `AxVMResources::prepare_guest_address_space` → `guest_owned_regions` (address_space.rs:106–127, memory_regions→`VmRegionKind::Memory` at :108–112) → `build_address_layout` (layout.rs:452–494) → `map_linear(flags)` (address_space.rs:97–99). No permission attribute anywhere in the chain ⇒ window writable today (§2).
- V2: Conversion spot = `os/axvisor/src/config.rs:179 build_axvm_config`; analogous-to-entry_point copies at :206–209 (`AxVCpuConfig{bsp_entry,ap_entry}`); `memory_regions.clone()` into params at :226; re-sync hook :235–237. Carrier into axvm = `AxVMConfig` via `AxVMConfigParams` (virtualization/axvm/src/config.rs:98/:127/:155), consumed by x86 loader (`loader.config.kernel…`, arch/x86_64/boot/mod.rs:420/:521).
- V3: Shared path confirmed — `prepare_guest_address_space(map_guest_address_space)` is arch-agnostic in vm/prepare/; arches only contribute extra `architecture_regions`. LoongArch consumes `kernel.memory_regions` in its own boot glue (arch/loongarch64/boot/mod.rs:584/:617; boot/resources.rs:104) exactly like x86 (:420/:521). Decision: shared-core permission dimension (config-gated), per-arch producer edits only.
- V4: Test location/pattern = `virtualization/axvmconfig/src/test.rs` (362 lines): embedded TOML consts (`MINIMAL_CONFIG` :17–42, memory_regions fixture :31–33) + `GuestConfig::from_toml(...)` + direct kernel-field asserts (`parses_structured_guest_config` :44–60, incl. `configured_memory_region_count` :56). Extend with new consts/tests there; validation fn under test lives in lib.rs ~:379–427.

PLAN-COMPLETE

## 9. Implementation notes (2025 implementation agent, open items resolved)

- OPEN#1 (MappingFlags literal for VM-owned RAM): pinned to `MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE | MappingFlags::USER`, duplicated at virtualization/axvm/src/vm/mod.rs:822–825 (`reset_transient_resources` re-map), :2069–2072 (`alloc_memory_region`), :2127–2130 (`map_reserved_memory_region`). The firmware window (MapAlloc, map_type=0 per `VmMemMappingTypeSerde`, axvmconfig/src/lib.rs:40–47) is mapped through `alloc_memory_region`, hence RWX+USER today.
- OPEN#2 (does guest reset re-run load_boot_image_*): NO. `AxVM::reset` (vm/mod.rs:1653–1664) = stop_and_join_runtime → `reset_transient_resources` → `prepare()` → x86 `init_vm`/`prepare_guest_address_space`; boot-image loading happens only once at create (`prepared_boot.load_images`, os/axvisor/src/config.rs:152). `reset_transient_resources` (vm/mod.rs:807–835) preserves `resources.memory_regions` and re-maps each with hardcoded RWX|USER, so once-at-create VARS seeding survives reset provided the re-map loop becomes kind-aware (otherwise it would silently re-enable CODE writes on reset — handled in step 2).
- Plan correction noted: §3's split sentence swaps CODE/VARS ("CODE [base, base+n)") relative to its own field description and scripts/axbuild ovmf.rs reality; `assemble_ovmf_image` emits `[VARS prefix][CODE]` loaded at window base, so VARS = `[base, base+n)` and FirmwareRom CODE = `[base+n, base+size)`. Implemented per the field description.
- Extra finding: owned regions never produce stage-2 mappings from `build_address_layout` (they only punch passthrough holes); the real window mapping is installed by `alloc_memory_region` during `prepare_memory_layout` (os/axvisor/src/config.rs:144 precedes :152 load_images). The permission dimension therefore lands in the runtime region kind + mapping-flag rule shared by create AND reset paths, not in the planner's mapping emission.

## 10. Final resume-note (2025 implementation complete)

**Implemented (all of §4 steps 1–7), commits on feat/x86-uefi-writable-vars after 6f746fc4f:**
1. `34bd94be2` feat(axvmconfig): add optional bios_vars_size kernel boundary — field + `InvalidBiosVarsSize` validation (non-zero, 4 KiB-aligned) + parse/reject/legacy tests.
2. `df211e03b` feat(axvm): carry bios_vars_size boundary in VM config params — `AxVMConfigParams.bios_vars_size`, getter/setter, `firmware_window_boundary()`, os/axvisor threading incl. `sync_axvm_config_from_crate_config`.
3. `8f9557321` feat(axvm): map firmware rom regions without stage-2 write — `VmRegionKind::FirmwareRom`, centralized `ram_mapping_flags()`/`firmware_rom_mapping_flags()` + `owned_mapping_flags()` rule, `VMMemoryRegion.kind`, kind-aware create-time alloc and reset-time re-map (reset no longer restores WRITE on CODE), `guest_owned_regions` emits region kinds.
4. `3aff2463f` feat(axvm): split firmware window into vars and code plans — pure `plan_memory_regions(configs, boundary)` splits the window into RAM-backed VARS prefix + FirmwareRom CODE suffix plans (requires MapAlloc window strictly larger than vars size), `MemoryLayoutBuilder` consumes plans, new `map_firmware_rom_region` mapper path; tests incl. `vars_frames_survive_region_rebuild`.
5. `130d8541d` feat(axvm): seed uefi vars frames and split-copy firmware code — loader installs blobs via `firmware_blob_chunks` ([VARS]@base seeded once + [CODE]@base+n) across memory/fs/uefi/default paths (configured-path UEFI now buffers the image to split); etc/e820 producer declares the window `E820NonRamType::Reserved` and excludes it from fw_cfg RAM **only when the boundary is set** (legacy e820 byte-identical).
6. `b09d126d5` test(axvisor): wire bios_vars_size into qemu-acpi-ovmf case — guest TOML `bios_vars_size = 0x84_000`, both build TOMLs set `AXVISOR_TEST_X86_OVMF_VARS_SIZE=0x84000`, axbuild cross-checks it against the assembled OVMF vars prefix (evidence now reports `vars prefix: 0x…`) and fails pre-boot on mismatch.
7. `3643230d8` docs(arch-platform-porting): SKILL.md contract paragraph + lifecycle-internals reset-permission note.

**Open-item resolutions:** see §9 (item 1 literal pinned; item 2 = reset never re-runs image loading; kind-aware re-map implemented so VARS survives reset and CODE stays RO).

**RED-first evidence:** axvmconfig RED = compile failure (unknown field / unknown variant) before commit 1; axvm layout RED = `no variant FirmwareRom` before commit 3; split-planning RED = `cannot find function plan_memory_regions` / missing mapper before commit 4; boot-loader RED = `cannot find firmware_blob_chunks` etc. before commit 5. All captured in-session before implementing each unit.

**Validation:** `cargo fmt --check` clean (axvmconfig/axvm/axbuild); `cargo test -p axvmconfig` 15 passed; `cargo test -p axvm --features host-test` 331 passed; `cargo test -p axbuild` 958 passed (incl. new ovmf/initramfs tests); `cargo xtask clippy --package axvmconfig --package axvm` 8/8 checks passed with `-D warnings`; `--package axbuild` also clean. Known pre-existing baseline lints elsewhere untouched (axdevice factory.rs large_enum_variant; axtask needless_return — sibling-owned).

**Deviations from plan text:** (a) §3 sentence swap CODE/VARS corrected per field description + ovmf.rs reality (VARS = window prefix); (b) permission dimension realized via runtime region-kind flag rule instead of a planner-emitted mapping (planner only records kinds; mappings are installed by the allocator path) — same shared-code property, less duplication; (c) configured-path filesystem firmware loads buffer fully instead of streaming when enable_bios (needed for split-copy); (d) `VMMemoryRegion` gained a public `kind` field (plain data, all constructors updated).

**Limitations:** E2E qemu-acpi-ovmf wired, NOT run (no /dev/kvm locally); axvisor binary not built here (raw cargo check lacks the axbuild platform flow — errors are missing-crate plumbing unrelated to this diff; the two-line axvisor edit mirrors adjacent code and depends only on APIs covered by axvm's suite); multiboot+`bios_vars_size` combination is honored mechanically but nonsensical — misconfigurations fail loudly at load time rather than being validated against protocol; loongarch keeps legacy behavior by construction (boundary absent ⇒ empty non_ram_regions, Memory kinds unchanged).
