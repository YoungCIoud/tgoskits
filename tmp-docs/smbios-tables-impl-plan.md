# SMBIOS 3.0 Tables for fw_cfg — Implementation Plan (branch feat/fw-cfg-smbios-tables)

## Goals
- Replace stub `build_smbios_tables`/`build_smbios_anchor` in
  `virtualization/axdevice/src/fw_cfg/data.rs` with spec-correct SMBIOS 3.0 output:
  anchor + Type 0/1/3/4×cpu_num/127 table blob, deterministic constants only.
- Regression-first: parse-back unit tests written to fail against the stub, then green.
- Thread `cpu_num` into builders; confine edits strictly to SMBIOS code paths
  (never memmap/e820/build_file_dir logic — sibling e820 branch touches those).

## Non-goals
- No DMI/UUID from host, no time/RNG, no per-structure optional tables (Type 2, 9, 16+),
  no blob-level checksum (SMBIOS defines none; only the anchor is checksummed).
- No changes outside `fw_cfg/{data.rs, mod.rs, tests.rs}` unless compile demands it.

## Evidence findings (appended as discovered)
- E1 data.rs:41–48 stub table = Type127 only: `[127,4] + handle 0x7F00 LE + [0,0]`
  (4-byte formatted area + double-null terminator — this part is already spec-shaped).
- E2 data.rs:50–66 stub anchor layout traced byte-by-byte:
  0x00 "_SM3_", 0x05 checksum placeholder, 0x06 len=24, 0x07 major=3, 0x08 minor=0,
  0x09 docrev=0, 0x0A eprev=1, 0x0B reserved, 0x0C u32 max-size, 0x10 reserved[8].
  Layout matches the _SM3_ spec table. DEVIATION: max-size field uses whole-blob
  `table.len()` (=6), but spec says "size of the LARGEST SMBIOS structure" incl. its
  strings + terminator. Checksum math (`wrapping_sub(sum)`, set at index 5) is correct.
- E3 mod.rs:154–155 call both builders argless; `FwCfg::new` already has `cpu_num: u16`
  (line 141, stored line 128). Files "etc/smbios/smbios-anchor" (FW_CFG_SMBIOS_ANCHOR)
  and "etc/smbios/smbios-tables" (FW_CFG_SMBIOS_TABLES) sized from blob lens (167–175).
- E4 tests.rs: child module can reach parent's private items via `use super::*`
  (existing tests use FW_CFG_* consts and dma fns imported privately in mod.rs), so
  new tests may call data.rs builders directly. Existing FwCfg::new test ctor pattern:
  `FwCfg::new(base,size,kernel,None,None,1,FwCfgPlatformConfig::default())`.

## Per-table design (all constants deterministic, no host/time input)

Anchor `_SM3_` (24B): keep stub layout (E2); fix max-size field to the largest
single structure size (formatted + strings + terminator), computed by walking the
built blob; major=3 minor=0 docrev=0 eprev=1 reserved=0; checksum over all 24 bytes
sums to 0 mod 256.

| Type | Handle | Formatted len | Fixed strings (string-set order → indices) |
| ---- | ------ | ------------- | ------------------------------------------ |
| 0 BIOS Information | 0x0000 | 0x18 | vendor="TGOSKITS"(1), version="1.0"(2), date="01/01/2024"(3) |
| 1 System Information | 0x0100 | 0x1B | manufacturer="TGOSKITS"(1), product="TGOSKITS Virtual Machine"(2), version="1.0"(3), serial="0"(4), sku="Default"(5), family="TGOSKITS"(6); UUID = fixed const GUID bytes written verbatim |
| 3 System Enclosure | 0x0300 | 0x15 | manufacturer="TGOSKITS"(1); chassis type=1(Other), boot/psu/thermal=3(Safe/OK), security=3(None), oem=0, height=0, power cords=1, contained elems=0 |
| 4 Processor ×cpu_num | 0x0400+i | 0x2A | socket="CPU {i}"(i from 0)(1), manufacturer="TGOSKITS"(2), version="Virtual CPU"(3); serial/asset/part = index 0 (absent) |
| 127 End-of-Table | 0x7F00 | 4 | none |

Type 0 fields: characteristics byte0 bit3 set (BIOS Characteristics Not Supported),
extension bytes 0, system-BIOS release / EC release = 0xFF ("not supported"),
starting segment 0xE800, ROM size byte 0.
Type 4 fields: type=3(CPU), family=1(Other)+family2=0xFE01, processor ID QWORD = 0,
voltage 0, ext clock 0, max speed=2000 MHz, current speed=2000 MHz, status=0x41
(populated+enabled), upgrade=1(Other), L1/L2/L3 handles=0xFFFF, core count=1,
cores enabled=1, thread count=1, characteristics=0x0004 (64-bit capable).
Handles scheme: per-type fixed bases + i for Type 4 ⇒ globally unique by construction;
Type 4 handles differ per vCPU ⇒ uniqueness holds for any cpu_num.

Encoding invariants: string area = concat(string_i + '\0') + final '\0'; structure
without strings ends immediately with two nulls (Type 127 edge). Table blob =
structures concatenated in order 0,1,3,4…,127; walk of blob must end exactly at
blob length.

## API changes (minimal)
- data.rs: `build_smbios_tables(cpu_num: u16) -> Vec<u8>`;
  `build_smbios_anchor(max_structure_size: u32) -> Vec<u8>`;
  new `pub(super) struct SmbiosBlobs { tables: Vec<u8>, anchor: Vec<u8> }` +
  `build_smbios_blobs(cpu_num: u16) -> SmbiosBlobs` as the single entry mod.rs uses;
  private `largest_structure_size(&tables) -> u32` walks the blob so anchor.max-size
  is derived from the actual bytes (invariant enforced at construction).
- mod.rs: lines 154–155 become `let smbios = build_smbios_blobs(cpu_num);`; file-dir
  sizes and struct fields read from `smbios.tables` / `smbios.anchor`. Nothing else.
- Task item "table checksum": SMBIOS defines no table-level checksum; only the anchor
  has one. Covered by tests as anchor-checksum-over-final-blobs + full walk-parse
  validation. Documented here as an intentional spec-correct interpretation.

## Step list
1. Commit 1 `test(axdevice): add failing smbios table parse-back regressions`:
   thread cpu_num/max-size through stub signatures minimally (stub bodies unchanged
   otherwise), append new tests to fw_cfg/tests.rs; `cargo test -p axdevice` shows
   the new tests RED.
2. Commit 2 `feat(axdevice): build spec-correct smbios 3.0 anchor and tables`:
   implement builders per design; same tests GREEN.
3. cargo fmt; cargo test -p axdevice; clippy filtered to my paths
   (`cargo clippy -p axdevice --all-targets` — expect only pre-existing
   large_enum_variant at fw_cfg/factory.rs:63); then `cargo xtask clippy --package axdevice`
   and confirm failure is exactly that known baseline lint.

## Test plan (tests.rs, all deterministic parse-back)
T1 anchor_signature_length_and_checksum_sum_to_zero (+ determinism: two builds equal).
T2 anchor_max_structure_size_matches_largest_walked_structure.
T3 tables_walk_yields_expected_types_handles_and_lengths (types order incl. 4×N,
   spec minimum lengths, handles unique across ALL structures, walk ends at blob end).
T4 string_sets_encode_null_terminators_and_empty_set_edge (each struct's string area
   double-null terminated; Type 127 = formatted area + exactly two nulls).
T5 type4_count_matches_vcpu_number_with_unique_socket_labels ("CPU {i}", cores=1,
   threads=1, per-cpu handles distinct) for cpu_num=1 and cpu_num=4.
T6 type127_is_last_structure_of_table.
T7 file_dir_sizes_match_built_smbios_blobs (FwCfg::new → dir record size == selector
   blob len for both smbios files).

## Risks
- R1 e820 sibling branch edits data.rs too ⇒ keep diff hunk-scoped to SMBIOS fns;
  never touch build_memmap/push_memmap_entry/build_file_dir/FwCfgRamRegion code.
- R2 xtask clippy fails on pre-existing large_enum_variant (factory.rs:63) on dev;
  verify it is that lint only and report as baseline, keep my files warning-free.
- R3 cpu_num=0 edge: zero Type 4 structures; walk/uniqueness must still hold.
- R4 uuid byte-order ambiguity: bytes written verbatim; documented constant, no
  interpretation claimed.

## Review round 1 (self-review before implementation)
- Handles: fixed bases break for cpu_num ≥ 0xFC00 (u16 overflow of 0x400+i).
  REVISED: sequential u16 handle allocator in emission order
  (0:Type0, 1:Type1, 2:Type3, 3..:Type4, last:Type127) — unique by construction;
  Type 4 count capped by SMBIOS_MAX_TYPE4_STRUCTURES = 0xF000 (named const +
  comment) so total structures can never exhaust handle space.
- Checksum re-derived independently for T1: fold-wrapping-add of all 24 anchor
  bytes must be 0; builder computes `0u8.wrapping_add-sum` complement into idx 5
  AFTER max-size bytes are placed (stub order already does this).
- Empty-string-set edge: walk finds first double-null after formatted length;
  safe because builders never emit an empty string (absent strings are omitted,
  referenced by index 0) — invariant goes into a code comment near the writer.
- Max-size field: derived from walked blob, asserted by T2 against an independent
  test-side walker (test reimplements the scan, does not call the prod fn).
- Determinism: every byte from consts or cpu_num loop index; T1 asserts two
  builds produce identical vectors.

## Resume-note 2025-08-25 (implementation complete)
State found: worktree clean on feat/fw-cfg-smbios-tables @ f70cf8d0e; stubs at
data.rs:41–66 called argless from mod.rs:154–155.
Implemented:
- Commit d47803556 `test(axdevice): add failing smbios structure regression tests`:
  signatures threaded (`build_smbios_tables(cpu_num)`,
  `build_smbios_anchor(max_structure_size)`), `SmbiosBlobs` +
  `build_smbios_blobs(cpu_num)` added as stub entry point, mod.rs switched to it
  once (final shape); T1–T7 appended to tests.rs with an independent test-side
  walker. RED captured: T3 (emission order [127] vs [0,1,3,4×4,127]) and T5
  (Type4 count 0 vs cpu_num) failed as designed; T4 initially failed via a
  walker artifact (bare "\0\0" parsed as one empty string) — fixed in the test
  parser, so T4/T6/T7 pass vs the degenerate stub and guard the real builder.
- Commit d28af6adb `feat(axdevice): emit real smbios structures`: full builders
  per design — sequential u16 handles 0=Type0,1=Type1,2=Type3,3..=Type4,last=
  Type127 with SMBIOS_MAX_TYPE4_STRUCTURES=0xF000 cap; Type0/1/3/4 field values
  per plan tables (family2=0xFE01 written verbatim; Type1 wake-up byte = 2
  "Unknown", plan left it unspecified); anchor max-size derived via private
  largest_structure_size() walk; empty-string invariant documented near the
  string writer. GREEN: cargo test -p axdevice → 48 passed / 0 failed (was
  46+2 red).
Validation: cargo fmt clean; `cargo clippy -p axdevice --all-targets` → zero
diagnostics from data.rs/mod.rs/tests.rs (one manual_repeat_n warning found in
my own tests.rs was root-caused to repeat_n, not allow'd). NOTE: the expected
baseline large_enum_variant at fw_cfg/factory.rs:63 did NOT appear in the raw
all-targets run on this toolchain; a follow-up `cargo xtask clippy --package
axdevice` was interrupted mid-run by the session and its outcome is unknown —
its truncated incremental dep-graph cache warnings for unrelated crates
(ax-cpumask/ax-memory-addr/ax-crate-interface) may appear on the next clippy
run and are harmless.
Deviations: none from plan substance. Limitations: no blob-level checksum
(SMBIOS defines none — intentional); UUID bytes arbitrary-but-fixed constants
written verbatim (R4); xtask-clippy rerun recommended before PR submission to
confirm the baseline lint state end-to-end.
