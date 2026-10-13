# Typed Guest Memory Map (`etc/e820`) for fw_cfg — Implementation Plan

Branch: `feat/fw-cfg-e820-memmap` @ `.worktrees/fwcfg-e820` (base dev `f70cf8d0e`).

## Goals
- Publish fw_cfg file **`etc/e820`**: packed little-endian 20-byte records `{u64 addr; u64 size; u32 type}`
  so guest OVMF (`OvmfPkg PlatformInitLib PlatformScanE820`) sees firmware-consumable memory topology.
- Additive `FwCfgPlatformConfig` extension carrying typed non-RAM region inputs
  (reserved / ACPI reclaim / ACPI NVS).
- Every emitted entry traceable to a declared source; no invented entries.
- Zero-regression: `AXVISOR_X86_OVMF_ACPI_PASSED` / `AXVISOR_X86_DIRECT_ACPI_PASSED` E2E semantics intact.

## Non-goals
- No auto-derived reserved windows (firmware flash `[0xffc00000,0x4000000)` etc. stay excluded;
  deriving them is a separate validated decision).
- No overlap merging/validation policy change beyond documented caller contract.
- No SMBIOS builder changes (cross-track: sibling branch owns `build_smbios_*` in the same `data.rs`).
- No SRAT/loongarch semantic changes.

## Evidence (repo @ f70cf8d0e)
- `virtualization/axdevice/src/fw_cfg/data.rs:24-39` — `build_memmap()`: all entries type-1,
  **24-byte stride** (8+8+4+4 trailing pad) via `push_memmap_entry`; `MEMMAP_RAM_TYPE=1`.
- `virtualization/axdevice/src/fw_cfg/mod.rs:160-165` — file-dir entry `"etc/memmap"` at
  `FW_CFG_FILE_FIRST` (0x20); `mod.rs:271` maps that selector to the payload; `mod.rs:78-105`
  `FwCfgPlatformConfig { ram_regions, srat_regions, acpi }` + manual `Default` (low/high RAM).
- `virtualization/axvm/src/arch/x86_64/boot/mod.rs:412-434` — `fw_cfg_ram_regions()`:
  `MapAlloc`-filtered `memory_regions` + main memory, sorted by base; non-RAM windows excluded today.
- Constructors of `FwCfgPlatformConfig`: `x86_64/boot/mod.rs:403`, `loongarch64/boot/mod.rs:143`
  (struct literals ⇒ both need the new field listed).
- Re-export surface: `virtualization/axdevice/src/lib.rs:60-64`.
- Consumers of `"etc/memmap"`: repo-wide grep finds **only** `fw_cfg/mod.rs` itself. No test,
  script, or tool reads it. No existing `"etc/e820"` anywhere.
- Format facts (verified upstream; per task brief): QEMU `hw/i386/fw_cfg.c` `struct e820_entry`
  packed `{u64,u64,u32}` LE; edk2 `OvmfPkg/Library/PlatformInitLib/MemDetect.c` `PlatformScanE820`.

## Wire format spec (`etc/e820`)
- Array of exactly 20-byte records, little-endian: `addr:u64`, `size:u64`, `type:u32`. No padding —
  consumers compute record count as `total_len / 20` (today's `etc/memmap` 24-byte stride is the bug).
- Types: `1` usable RAM, `2` reserved, `3` ACPI reclaimable, `4` ACPI NVS.
- Ordering: ascending `addr`; stable sort so equal-base ties keep declaration order (RAM pushed first).
- Zero-size declared regions are dropped (matches today's `size != 0` filter).

## Provenance rule per emitted entry
| Entry source | Type | Provenance |
| --- | --- | --- |
| `platform.ram_regions[i]` (MapAlloc-filtered + main memory) | 1 | declared RAM |
| `platform.non_ram_regions[i]` (new field) | enum-declared 2/3/4 | explicitly declared non-RAM window; reclaim/NVS only ever enter through this typed declaration |
| nothing else | — | never invented |

With `non_ram_regions` empty, decoded `(addr,size,type)` triples are identical to today's
`build_memmap` output triples — only the stride shrinks 24→20 (the intended fix).

**Interpretation note ("byte-identical to today's shape"):** literal byte identity is impossible
and wrong — the defect being fixed is the 24-byte padded stride, and `etc/e820` consumers compute
count as `len/20`. Equivalence therefore means: same entry set, same order, same `(addr,size)`
values, same effective type (1) — verified by decoding both payloads into triples.

## API extension (additive)
```rust
/// Firmware-visible guest memory region type for windows that are not usable RAM
/// (`etc/e820` record `type` field; usable RAM itself comes only from `ram_regions`
/// and is encoded as type 1 by construction, never through this type).
#[repr(u32)]
pub enum E820NonRamType { Reserved = 2, AcpiReclaimable = 3, AcpiNvs = 4 }
impl E820NonRamType { pub fn wire_value(self) -> u32 { self as u32 } }

/// A guest physical window firmware must see as something other than usable RAM.
pub struct FwCfgNonRamRegion { pub base: u64, pub size: u64, pub memory_type: E820NonRamType }
```
Deliberately no `UsableRam` variant: a caller cannot smuggle type-1 entries through the
non-RAM declaration channel (illegal state unrepresentable).
- `FwCfgPlatformConfig` gains `pub non_ram_regions: Arc<[FwCfgNonRamRegion]>` (Default: empty).
  Caller contract: regions pairwise disjoint, not overlapping declared RAM; encoder sorts, never merges.
- Re-exported from `lib.rs` next to `FwCfgRamRegion`.
- **Extension point for sibling task:** declaring a writable EFI varstore window =
  push `FwCfgNonRamRegion { base, size, memory_type: E820NonRamType::Reserved }` into
  `non_ram_regions` at the arch boot site; it flows to `etc/e820` type-2 with zero further changes.

## `"etc/memmap"` transition decision: RENAME, keep selector
Replace the file name with `"etc/e820"` at the **same selector** `FW_CFG_FILE_FIRST`.
Rationale:
- Guests discover fw_cfg files by name via the file-dir; selectors are internal numbering.
  Renaming keeps every existing selector assignment bit-stable (compat matrix below).
- No in-repo consumer of `"etc/memmap"` (grep evidence above); it is a nonstandard invented name,
  while `etc/e820` is the upstream-consumed name (QEMU emits it; OVMF scans it).
- Publishing both would advertise a dead nonstandard file forever (dir grows, two sources of truth)
  for a consumer nobody can point to.
Accepted risk: a hypothetical out-of-tree `"etc/memmap"` reader breaks; mitigation is the
compat matrix + commit message note.

Compatibility matrix:
| Consumer | Before | After |
| --- | --- | --- |
| In-repo readers of etc/memmap | none exist | n/a |
| OVMF x86 (E2E) | fallback path (absent etc/e820) | consumes etc/e820; RAM-only content until declarations arrive; markers must stay green |
| Out-of-tree etc/memmap readers | worked | break (accepted, documented) |
| Selector numbering FILE_FIRST..+8 | stable | unchanged |

## Steps
1. Commit A `test(axdevice): add failing etc/e820 wire-format regression tests`
   - Add `E820MemoryType`, `FwCfgNonRamRegion` (real definitions) + `build_e820_map()` stub
     returning `Vec::new()` (documented placeholder) so tests compile and fail at runtime (red).
   - Tests in `fw_cfg/tests.rs`: byte-exact LE packing & 20-byte stride; ordering across
     interleaved ram/non-ram input; type values 2/3/4; zero-size dropped;
     zero-non-ram-input triple-equivalence vs test-local replica of legacy 24-byte format.
   - Verify red: `cargo test -p axdevice`.
2. Commit B `feat(axdevice): emit typed etc/e820 guest memory map`
   - Implement `build_e820_map(ram, non_ram)` in `data.rs` (collect→sort→encode 20B LE);
     English comments stating wire invariants. Remove `build_memmap` + `MEMMAP_RAM_TYPE`.
   - `mod.rs`: rename field `memmap`→`e820`, file name `"etc/memmap"`→`"etc/e820"`
     (selector untouched); `FwCfgPlatformConfig.non_ram_regions` + `Default` empty; lib.rs exports.
3. Commit C `feat(axvm): declare empty non-RAM window set for guest fw_cfg`
   - List `non_ram_regions` explicitly (empty) at both constructor sites with comment pointing at
     the extension point (varstore follow-up).
4. Validate green; fmt/clippy/test suite (below).

## Red-first tests (names)
- `e820_records_pack_little_endian_with_twenty_byte_stride` (byte-exact expected vector)
- `e820_orders_entries_by_base_across_ram_and_reserved_windows`
- `e820_encodes_reserved_reclaim_and_nvs_wire_types`
- `e820_drops_zero_length_declared_regions`
- `e820_without_non_ram_input_preserves_legacy_memmap_coverage` (decoded-triple equality)

## Validation
```
cargo fmt --all
cargo test -p axdevice
cargo test -p axvm --features host-test
cargo xtask clippy --package axdevice --package axvm
```
Plus: attempt the ovmf-acpi E2E if an xtask target exists; else report as limitation.

## Risks
- OVMF switches from fallback to our map even while RAM-only ⇒ E2E marker regression risk;
  mitigated by keeping coverage identical to today and running the E2E if available.
- Overlapping/malformed declarations are not rejected by the encoder (documented contract;
  typed enum prevents wrong type values).
- Cross-track: edits confined to memmap/e820 functions + config plumbing; `git diff` audited
  against smbios symbols before each commit.

## Review round notes

### Round 1 (pre-implementation, plan-level)
- Ordering/no-overlap: ascending `addr`, stable sort, RAM-before-non-RAM on equal base; no
  merging — caller contract documented in rustdoc. OK.
- Zero-reserved-input equivalence: literal byte identity impossible/wrong (stride IS the fix);
  defined as decoded-triple equality — interpretation note added above. Test updated mentally:
  compares triples, not bytes.
- API additivity: new field defaults empty; both struct-literal constructor sites enumerated;
  lib.rs re-export listed. No existing signature changes. OK.
- Provenance hardening: removed `UsableRam` from the public input enum so type-1 records can only
  originate from `ram_regions` (illegal state unrepresentable). Renamed to `E820NonRamType`.
- Selector/name matrix: rename keeps selector bit-stable; publish-both rejected with rationale. OK.
- no_std check: `alloc::Vec` + `sort_by_key` fine; derives are `no_std`-safe; `#[repr(u32)]`
  added for deterministic wire discriminants.

### Round 2 (post-implementation, diff-vs-plan audit)
Branch state: 3 commits (`2d1c40a1e` red tests → `a2e440bd5` encoder+wiring →
`6f746fc4f` axvm plumbing). Verified against every plan invariant:
- Wire format: byte-exact 20-byte LE records confirmed by
  `e820_records_pack_little_endian_with_twenty_byte_stride`; types 2/3/4 confirmed;
  zero-size drop confirmed; ascending-base ordering across channels confirmed. All green.
- Zero-reserved equivalence: decoded-triple equality test green (lowmem/highmem default shape).
- API additivity: field defaults empty; both struct-literal sites updated; lib.rs exports added;
  full workspace consumers compile (axvm 318 host tests pass).
- Selector stability: rename only; file-dir entry count unchanged; selectors untouched.
- Provenance: no RAM variant on input enum; encoder invents nothing; rustdoc states contract.
- Cross-track audit: `git diff dev..HEAD | grep smbios` → 0 lines; only 6 files touched,
  all memmap/e820 or platform-config plumbing.
- Red→green discipline honored: commit A verified red (exactly the 5 new tests failing,
  41 existing passing) before implementation landed.
Validation outcome:
- `cargo fmt --all --check`: clean.
- `cargo test -p axdevice`: 55 passed / 0 failed (46 lib incl. 5 new e820).
- `cargo test -p axvm --features host-test`: 318 passed / 0 failed.
- `cargo xtask clippy --package axvm`: all 6 checks pass.
- `cargo xtask clippy --package axdevice`: FAILS on **pre-existing** baseline lint
  `clippy::large_enum_variant` at `fw_cfg/factory.rs:63` (`FwCfgPayloadSource::Fixed`) —
  factory.rs/payload.rs are byte-identical to dev base f70cf8d0e and the failure reproduces
  with this branch's changes stashed. Per the CRITICAL cross-track confinement constraint
  this out-of-scope file was deliberately left untouched; `cargo clippy -p axdevice
  --all-targets` reports zero diagnostics in files changed by this branch.
- E2E ovmf-acpi not run: requires building axvisor + OVMF guest initramfs boot flow
  (self-hosted/board category, outside the mandated validation list). Residual risk noted:
  after the rename OVMF consumes etc/e820 (RAM-only content) instead of its absent-file
  fallback; coverage equals the legacy payload's, so markers are expected to stay green.

