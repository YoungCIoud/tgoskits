# x86_64 Guest vCPU SMP Bring-Up (INIT-SIPI / AP-hold) — Implementation Plan

Branch: `feat/x86-guest-smp` (worktree `/home/young/code/tgoskits/.worktrees/x86-guest-smp`, base `dev` @ f70cf8d0e)
Status: v1 (initial draft, 2025 review round 0)

## 1. Problem, users, success criteria

**Problem.** Every x86_64 Linux guest test case pins `cpu_num = 1`; the shared initramfs
check asserts online CPUs == `"0"`. The emulated local APIC logs-and-drops INIT/SIPI
(`virtualization/x86_vlapic/src/vlapic.rs:402-415`), so multi-vCPU x86 guests can never
bring secondary CPUs online, for either direct-boot or guest-UEFI(OVMF) boot.

**Users / callers.**
- Axvisor x86_64 hosts running SMP Linux guests (direct boot and OVMF boot).
- CI/KVM-runner E2E coverage (`cargo xtask axvisor test qemu`).

**Success criteria.**
1. INIT delivery to another vCPU puts that AP into wait-for-SIPI; SIPI starts it at
   real-mode `CS = vector<<12, IP = 0` (Intel MP initialization protocol).
2. Unsupported ICR forms for INIT/SIPI (logical destination mode, SELF/AllIncludingSelf
   shorthand, SMI/NMI/Remote-Read deliveries) are logged and ignored — never silently
   misrouted, never fatal to the guest.
3. The INIT level-de-assert variant (trigger=Level ∧ level=DeAssert) is a no-op.
4. `cpu_num == 1` behavior is unchanged (no new host-op calls with other targets;
   broadcast masks are empty).
5. CPU-up wiring mirrors RISC-V/AArch64: typed `CpuUpExit` through
   `architecture::cpu_up`, `CpuUpOps` implemented for `X86_64Arch`.
6. Deterministic red-first regression tests at the x86_vlapic layer; host-level tests for
   the axvm glue; a new E2E axvisor normal case (`cpu_num = 2`) + initramfs check.

**Non-goals (this change).**
- Resetting an *already-running* AP on INIT (guest reboot/hot-unplug paths): logged and
  ignored; requires cross-thread vCPU register access which the `AxVCpu !Send/Sync`
  design forbids today.
- Logical-mode INIT/SIPI addressing, lowest-priority delivery, SMI/NMI/Remote-Read IPIs.
- x2APIC-specific INIT/SIPI testing (xAPIC path is exercised; x2APIC decode reuses the
  same ICR-low fields).
- AP CPU_OFF/hotplug lifecycle for x86 guests.
- OVMF E2E case (the mechanism is identical; only a direct-boot case is added per task).

## 2. Current-state evidence (file:line, worktree @ f70cf8d0e)

| Fact | Evidence |
| --- | --- |
| INIT/SIPI is a logged no-op | `virtualization/x86_vlapic/src/vlapic.rs:402-415` (`process_init_sipi`) |
| ICR write dispatch: mode match, dmask fan-out over `0..vcpu_num` | `vlapic.rs:495-567` (`write_icr`); INIT/StartUp arm at `552-554` |
| Illegal shorthands for NMI/INIT/StartUp already rejected w/ log | `vlapic.rs:524-530` |
| ICR bitfields incl. Level(14) Assert/DeAssert, TriggerMode(15) | `virtualization/x86_vlapic/src/regs/icr.rs:36-135` |
| Host-op boundary pattern (static trait, default impls) | `virtualization/x86_vlapic/src/host.rs:11-56` |
| `active_vcpu_mask` = all configured vCPUs `(1<<n)-1`, not "running" | `virtualization/axvm/src/manager.rs:68-77` |
| `with_vm` clones Arc under short lock; safe to sleep in callback | `manager.rs:59-65` |
| Only vCPU 0's task is spawned at VM start; secondaries spawn lazily via `vcpu_on` | `virtualization/axvm/src/vm/mod.rs:1482-1519`; `runtime/vcpus.rs:255-333` |
| `vcpu_on` requires state `Free`, reserves → Starting, spawns task, waits for bind ack | `runtime/vcpus.rs:255-333`; `vcpu.rs:228-231` |
| Generic CpuUp flow + `CpuUpOps` consumed by riscv64/aarch64 | `architecture/cpu_up.rs:42-86`; `arch/riscv64/mod.rs:125-137`; `arch/aarch64/mod.rs:162-175` |
| `CpuUpOps::set_cpu_up_success` writes sender GPR by default | `architecture/cpu_up.rs:31-33` |
| Target resolution via VM-owned topology (`VmArchCpuIdResolver`) | `architecture/cpu_up.rs:17-28` |
| x86 exit dispatcher has no CPU-up arm | `arch/x86_64/mod.rs:88-235` |
| vLAPIC MMIO/MSR accesses are handled *inside* the backend (exit usually `Nothing`) | `x86_vcpu/src/vmx/vcpu.rs:1005-1103,2054` |
| Guest entry programmed once during `setup()` from recorded `self.entry` | `x86_vcpu/src/vmx/vcpu.rs:1850-1867,564-621`; `svm/vcpu.rs:1831-1849,303-358` |
| Real-mode entry-state helper exists (reset vector special case) | `x86_vcpu/src/lib.rs:212-237` |
| VMX `bind()` = vmptrld + host state (guest fields writable afterwards) | `vmx/vcpu.rs:243-253,2110-2112` |
| SVM VMCB is plain memory; bind is a no-op | `svm/vcpu.rs:451-457` |
| MADT enumerates one LAPIC per cpu_num with apic_ids 0..N-1 | `axvm/src/arch/x86_64/boot/acpi/config.rs:87-125`; `tables.rs:106` |
| fw_cfg NB_CPUS/MAX_CPUS wired from cpu_num | `virtualization/axdevice/src/fw_cfg/mod.rs:229,251-252` |
| bsp_entry == ap_entry == kernel.entry_point | `os/axvisor/src/config.rs:206-208`; applied per-vCPU at `vm/prepare/vcpus.rs:67-78` |
| Existing single-CPU initramfs check (online == "0") | `scripts/axbuild/src/axvisor/test/initramfs.rs:34-63` |
| axvisor E2E case layout (build wrapper dir + subdir/qemu-*.toml) | `test-suit/axvisor/normal/qemu-acpi/{build-x86_64-unknown-none-vmx.toml,x86-linux-acpi-direct.toml,direct-acpi/qemu-x86_64-vmx.toml}` |
| axvm host tests gated by feature `host-test` | `architecture/cpu_up.rs:88`; `axvm/Cargo.toml:15` |

## 3. External prior art (recorded per feature-development.md §4)

1. **Intel SDM Vol.3** (Order 325384, §§9.4, 10.6, 11.6, Table 10-6):
   - SIPI: target executes at `CS selector = vector<<8`, `CS base = vector<<12`, `IP = 0`.
   - INIT level de-assert form requires `level = 0 ∧ trigger = 1`; it is an arbitration
     sync message, not a reset.
   - Destination shorthands Self / AllIncludingSelf are not valid with NMI/INIT/StartUp.
2. **KVM**, `arch/x86/kvm/lapic.c` (current master):
   - `__apic_accept_irq` `APIC_DM_INIT`: accepted when `!trig_mode || level`; the
     `trig_mode=1 ∧ level=0` combination is ignored. Commit
     [a52315e1d549](https://github.com/torvalds/linux/commit/a52315e1d549dad80ff443151927226c11fd8c2b)
     fixed exactly this edge/deassert confusion ("For this delivery mode [INIT
     de-assert], the level flag must be set to 0 and trigger mode flag to 1").
   - Non-BSP vCPU enters `KVM_MP_STATE_INIT_RECEIVED` on INIT; SIPI is *dropped unless*
     mp_state is INIT_RECEIVED ("SIPIs are dropped if the CPU isn't in wait-for-SIPI").
3. **QEMU**, `hw/intc/apic.c` (current master):
   - `apic_deliver`: `DM_INIT` with `level==0 && trig_mode==1` only refreshes arb IDs.
   - `apic_sipi`: `if (!s->wait_for_sipi) return;` then
     `cpu_x86_load_seg_cache_sipi(vector)` (CS sel/base per SDM), clearing the flag.
4. **Linux guest side**: `wakeup_cpu_via_init` sends
   `APIC_INT_LEVELTRIG | APIC_INT_ASSERT | APIC_DM_INIT` (assert), waits ≥10 ms, then two
   STARTUP IPIs — so the VMM must accept plain asserted INIT and tolerate repeated SIPI,
   while treating trigger=level+deassert as a no-op.

Conclusion: implement the KVM/QEMU rule set — INIT accept condition
`!(trig_mode == LEVEL ∧ level == DEASSERT)`; SIPI gated on wait-for-SIPI state;
protocol-exact CS/IP mapping; keep existing illegal-shorthand rejection.

## 4. Design decisions (with alternatives)

### D1. Event channel: vLAPIC host-ops callbacks (chosen)
Add two optional methods to `X86VlapicHostOps`:
`request_ap_init(vm_id, vcpu_id)` and
`request_sipi_startup(vm_id, vcpu_id, start_vector: u8)`,
defaulting to `Err(X86VlapicError::Unsupported)` so existing/future hosts fail closed
loudly instead of dropping SMP silently. The three existing impls
(`AxvmX86HostOps`, x86_vcpu `MockMmHal`, vlapic-test `DummyHost`) keep compiling via the
defaults.

*Alternatives.*
- *Typed `X86VmExit::CpuUp` variant like RISC-V:* rejected — x86 APIC-access/MMIO exits
  for the LAPIC page are fully handled inside the x86_vcpu backend and surface as
  `X86VmExit::Nothing` (`vmx/vcpu.rs:1056-1103,2054`); there is no exit to intercept
  without re-plumbing the backend, and the device-manager boundary cannot carry custom
  outcomes.
- *Global event queue polled by the dispatcher:* rejected — hidden mutable state, extra
  ordering hazards, no consumer today.
The host-ops callback is the established boundary (`inject_interrupt`,
`register_timer`) and runs synchronously inside the BSP's exit dispatch.

### D2. Wait-for-SIPI gate: x86-owned per-VM registry with runtime-hook lifecycle (chosen)
New private module `virtualization/axvm/src/arch/x86_64/smp.rs` owns
`WAIT_FOR_SIPI_MASKS: IrqSafeMutex<BTreeMap<VMId, X86SipiGate>>` where `X86SipiGate`
wraps a `usize` bitmask (`mark/test_and_clear/restore`). Registration/cleanup hook into
`ArchOps::enter_runtime` / `exit_runtime` (same lifecycle as the RISC-V vplic
activate/deactivate), so VM start/reset/stop rebuilds or drops state deterministically.

*Alternatives.* Field on `VmRuntimeHandle`: rejected — pollutes the arch-neutral runtime
shared by four architectures. New `VmVcpuState` variant: rejected — shared enum across
archs plus cross-thread state mutation constraints. Device-graph service: rejected —
this is not a device.

### D3. SIPI entry state: protocol-exact, programmed at bind time on the target thread (chosen)
`x86_vcpu` gains `x86_sipi_entry_state(vector) -> {cs_selector: vector<<8, cs_base:
vector<<12, rip: 0}` and both backends gain `set_sipi_vector(u8)` which records a pending
vector consumed in `bind()` — after `vmptrld` on VMX (VMCS current), directly on VMCB
memory for SVM. This is required because `set_entry()` merely records the address while
`setup()` programs guest state once at prepare time; a live rewrite must happen on the
thread where the VMCS/VMCB is owned.

*Alternatives.*
- Writing VMCS guest fields from the sender thread inside `set_entry`: rejected —
  `vmptrld` of the AP's VMCS would clobber the current-VMCS pointer of the sending
  physical CPU mid-exit-handling (BSP entry relies on its own VMCS being current).
- Overloading `x86_real_mode_entry_state` with a "page-aligned low address ⇒ SIPI"
  heuristic: rejected — ambiguous for arbitrary low entries; explicit vector recording
  keeps BSP semantics bit-identical.
- Flat `{sel:0, base:0, rip:vector<<12}` (equivalent linear address): rejected —
  trampoline code legitimately derives its base from `%cs` on real hardware; matching
  silicon maximizes guest compatibility.

### D4. Shared CPU-up core: extract `boot_secondary_cpu` + arch-overridable signaling (chosen)
`architecture/cpu_up.rs` is refactored into a non-generic
`boot_secondary_cpu(vm, CpuUpExit) -> Result<usize, SecondaryCpuBootFailure>`
(resolve arch CPU id → `runtime::vcpus::vcpu_on`) plus `CpuUpOps` signaling hooks:
existing `set_cpu_up_success` (default `gpr[0]=0`) and new
`report_cpu_up_failure(vcpu)` (default `set_return_value(usize::MAX)`).
`handle`, `set_cpu_up_success`, and `report_cpu_up_failure` take
`&crate::vm::AxVCpu<A::VCpu>` (was `&AxVCpuRef`); riscv64/aarch64 **call sites are
unchanged** (`&Arc<T>` deref-coerces); only the trait declaration, the
`Riscv64Arch::set_cpu_up_success` override signature (`arch/riscv64/mod.rs:30-34`),
and the in-file `RecordingArch` test impls need updating.
`X86_64Arch: CpuUpOps` overrides both hooks to no-ops: an INIT/SIPI delivery has no
hypercall-style return ABI, and clobbering the *sending* vCPU's RAX would corrupt live
guest state. The x86 smp glue calls `cpu_up::handle::<X86_64Arch>(vm, sender_vcpu, exit)`
with the sender obtained via `with_current_vcpu::<AxvmX86Vcpu, _>`.

*Alternatives.* Calling `vcpu_on` directly from smp.rs and duplicating resolve/log logic:
rejected — duplicated knowledge. Forcing default failure signaling on x86: rejected —
guest-visible corruption on a failure path violates fail-closed-without-side-effects.

### D8. Dead-code expectation boundary must move with the consumer set
`architecture/mod.rs:4-11` gates `cpu_up` with
`#[cfg_attr(not(any(target_arch = "aarch64", target_arch = "riscv64")), expect(dead_code,…))]`.
Once x86_64 consumes `cpu_up`, that unfulfilled `expect` becomes a `-D warnings`
failure. The condition must be widened to include `x86_64` in the same commit that adds
the consumer (per code-quality.md §7.10, keep cfg at the narrowest common module
boundary).

### D5. Failure semantics: log-and-continue, never fail the guest vCPU
Every error inside the INIT/SIPI handlers (unknown VM, unresolvable target, gate
violations, `vcpu_on` failures) is logged (`warn!`) and swallowed at the vlapic
boundary. An ICR MMIO/MSR write must never propagate an error into
`handle_vcpu_exit_bound` (which would stop the whole VM with `Fault`). Transient start
failures (`VcpuOnError::StartFailed`) restore the wait-for-SIPI bit so the guest's
retry SIPI (Linux sends two) can succeed; `AlreadyOn`/`OnPending` leave it cleared.

### D6. Addressing scope (MVP)
Physical destination mode only, plus broadcast shorthands. Logical destination mode with
INIT/SIPI → warn+ignore. `SELF`/`AllIncludingSelf` × NMI/INIT/StartUp keep the existing
reject-with-log (`vlapic.rs:524-530`). AllExcludingSelf uses `current_vm_active_vcpus()`
which already covers all configured vCPUs regardless of task existence
(`manager.rs:68-77`). Physical miss (dest ≥ vcpu_num) falls out of the dmask loop as a
silent hardware-style drop with a debug log. INIT addressed to the sending vCPU itself
is warned and ignored (a running vCPU cannot reset itself mid-exit-handling).

### D7. INIT-to-running-AP limitation
INIT targeting an AP whose vCPU task exists is warned and ignored. Cross-thread reset of
live guest registers is impossible under the current ownership model
(`AxVCpu` non-Send/Sync by design; FIXME documented in `vm/prepare/vcpus.rs:41-44`).
Consequence: guest reboot that INIT-shoots down still-running APs will not reset them
(logged); tracked as follow-up work.

## 5. Step-by-step change list (commits)

1. **`feat(x86_vlapic): add CPU-up request host-op contract`**
   - `host.rs`: `request_ap_init`, `request_sipi_startup` trait methods with
     `Err(Unsupported)` defaults + `pub(crate)` wrapper fns (mirroring
     `inject_interrupt`). No behavior change.
2. **`test(x86_vlapic): cover INIT-SIPI decode and dispatch (red)`**
   - New `RecordingHost` mock (mirrors the existing `DummyHost` pattern in
     `timer.rs:421-470`: `SpinMutex` + atomics, identity phys↔virt mapping, leaked
     4-KiB-aligned frame per `alloc_frame`) + tests in `vlapic.rs`
     `#[cfg(test)] mod tests` (appendix position). Tests drive the public
     `handle_write(ApicRegOffset::ICRLow/ICRHi, …)` path and assert on recorded
     host-op requests:
     physical hit INIT, physical hit SIPI, physical miss, AllExcludingSelf broadcast,
     logical-mode ignored, SELF×SIPI rejected, INIT-deassert idempotence, illegal-vector
     Fixed unchanged. Verify RED (requests missing / `Unsupported`).
3. **`feat(x86_vlapic): dispatch INIT and SIPI through CPU-up requests`**
   - `process_init_sipi` implements D5/D6 rules using `Level`/`TriggerMode`/`Vector`
     fields; logical-mode guard added at the dispatch arm; self-INIT filtered; errors
     from host ops logged, never propagated.
4. **`refactor(axvm): extract architecture-neutral secondary CPU boot core`**
   - `cpu_up.rs`: `boot_secondary_cpu` + `SecondaryCpuBootFailure` +
     `report_cpu_up_failure` hook; `handle` and hook signatures moved to
     `&AxVCpu<A::VCpu>` (D4); riscv64/aarch64 call sites untouched via deref coercion;
     update `Riscv64Arch::set_cpu_up_success` signature + in-file test impls.
   - Widen the `cpu_up` dead-code `expect` condition to include `x86_64`
     (`architecture/mod.rs:4-11`, D8).
   - Existing `host-test`s stay green.
5. **`feat(x86_vcpu): program protocol-exact SIPI entry state at bind`**
   - `lib.rs`: `x86_sipi_entry_state`; `runtime.rs`: `X86Vcpu::set_sipi_vector`;
     vmx/svm backends record pending vector and apply CS/RIP at `bind()`; unit tests for
     entry-state math and apply-once semantics (SVM path testable headlessly; VMX
     covered by pure-function tests since VMCS writes need hardware).
6. **`feat(axvm): bring up x86 guest APs via INIT-SIPI CPU-up flow`**
   - New `arch/x86_64/smp.rs`: gate registry + decision logic + `handle_init_request` /
     `handle_startup_request` (calls `cpu_up::handle::<X86_64Arch>` with sender vCPU);
     `mod.rs`: `impl CpuUpOps for X86_64Arch` (both signaling hooks = no-ops, with
     safety/invariant docs), `set_vcpu_on_args` override (arg = SIPI vector →
     `AxvmX86Vcpu::set_sipi_vector`), `enter_runtime`/`exit_runtime` overrides,
     `AxvmX86HostOps::{request_ap_init, request_sipi_startup}` impls.
   - Host-tests (feature `host-test`): gate state machine, SIPI entry math, outcome
     mapping, compile-time contract assertions.
7. **`feat(axbuild): add x86 SMP axvisor E2E case and initramfs check`**
   - `test-suit/axvisor/normal/qemu-acpi-smp/` build wrapper dir:
     `build-x86_64-unknown-none-vmx.toml`, guest config `x86-linux-acpi-smp.toml`
     (`cpu_num=2`, `phys_cpu_ids=[0,1]`, cmdline adds `axvisor.smp_case=x86`),
     `smp-direct/qemu-x86_64-vmx.toml` (kvm, `-smp 2`, success `AXVISOR_X86_SMP_PASSED`,
     fails incl. `AXVISOR_X86_SMP_FAILED`).
   - `scripts/axbuild/src/axvisor/test/initramfs.rs`: new
     `run_x86_smp_check()` + cmdline arm placed before existing cases; asserts online
     contains "1", `/sys/devices/system/cpu/cpu1` exists, the MADT (`APIC`) table is
     readable, and IO-APIC appears in /proc/interrupts; existing checks untouched;
     marker assertions added to the archive-content unit test.
8. **`docs(axvm): document x86 guest SMP INIT-SIPI design and porting contract`**
   - `docs/design/axvisor-x86-guest-smp-init-sipi.md` (Chinese, repo style: 背景/方案
     比较/所有权边界/数据流/错误与原子性/锁与并发/验证与回滚).
   - `.claude/skills/arch-platform-porting/SKILL.md`: add Axvisor guest x86 SMP contract
     note under SMP Bring-Up Rules + reference pointer.

## 6. Red-first regression plan

- Round A (commit 2 before commit 3): run `cargo test -p x86_vlapic`. Tests assert
  recorded host-op requests; implementation absent ⇒ assertions fail
  (mock records empty; `request_*` defaults return `Unsupported`). Record output.
- Round B: same command after commit 3 ⇒ green.
- Guard rails: existing vioapic/timer/vpic tests must stay green throughout;
  `cargo test -p axvm --features host-test` green after commits 4–6;
  `cargo test -p x86_vcpu` green after commit 5.

## 7. Validation commands (worktree only)

```bash
cargo fmt
cargo test -p x86_vlapic
cargo test -p x86_vcpu
cargo test -p axvm --features host-test
cargo test -p axbuild            # initramfs script assertions + discovery consistency
cargo xtask clippy --package x86_vlapic --package x86_vcpu --package axvm --package axbuild
```

E2E (cannot run locally, no /dev/kvm — reported as KVM-runner-unverified):

```bash
cargo xtask axvisor test qemu --list --arch x86_64 --test-group normal
cargo xtask axvisor test qemu --arch x86_64 --test-group normal --test-case smp-direct-vmx
```

## 8. Risks / open questions

- **Blocking `vcpu_on` inside pinned exit handling**: identical context to the shipping
  RISC-V SBI CPU-up path (sleepable wait under `with_current_cpu_set`); validated only
  by sibling precedent until E2E runs.
- **Guest kernel image**: `tmp/axbuild/images/qemu-x86_64/linux/linux-qemu` is built
  outside this repo; `CONFIG_SMP=y` is assumed but unverifiable here.
- **phys_cpu_ids alignment**: MADT hardcodes APIC IDs 0..N-1 while target resolution uses
  configured IDs; the new E2E config uses identity `[0,1]`. Mismatched configs produce a
  clear warn at resolution failure rather than silent misrouting.
- **INIT to running APs** (reboot/shootdown): unsupported, logged (D7).
- **x2APIC guests**: decode path shared but untested; `nox2apic` used by test guests.

## 9. Self-review rounds

### Review round 1 (plan revision applied)
Checked against: fail-closed diagnostics, locking/concurrency, config validation,
cpu_num==1 identity, failure-mode test coverage, naming/conventions.

Findings and resolutions:
1. **Blocking (build)** — `architecture/mod.rs:4-11` `expect(dead_code)` on `cpu_up`
   becomes unfulfilled once x86_64 consumes it ⇒ `-D warnings` failure. Added D8 +
   commit-4 step.
2. **Design flaw** — `cpu_up::handle`/hooks took `&AxVCpuRef` (`Arc`), but the x86
   sender is only reachable as `&AxVCpu` via `with_current_vcpu`; an `Arc` cannot be
   reconstructed there. Moved signatures to `&AxVCpu<A::VCpu>`; verified
   `AxVCpuRef<A> = Arc<AxVCpu<A>>` (`vm/mod.rs:54`) so riscv/aarch64 call sites need no
   change. Updated D4/commit 4.
3. **Coverage gap** — E2E check did not assert MADT presence, although CPU discovery
   depends on it. `run_x86_smp_check` now requires `/sys/firmware/acpi/tables/APIC`.
4. **Convention** — reuse the established in-crate mock style (`DummyHost`,
   `timer.rs:421-470`: `SpinMutex`, atomics, identity phys↔virt) for the new
   `RecordingHost`; verified `ApicTimer::new` performs no timer registration at
   construction (`timer.rs:86`, registration only at `timer.rs:360`) so mock stubs are
   sufficient.
5. **Naming** — host ops renamed to protocol-exact `request_ap_init` /
   `request_sipi_startup` (INIT/SIPI are the domain invariants).
6. Verified only three `X86VlapicHostOps` impls exist; trait-default additions keep the
   two mocks compiling unchanged.

### Review round 2 (final pass)
1. **Locking/concurrency**: gate map lock (`WAIT_FOR_SIPI_MASKS`, IrqSafeMutex) must
   never be held across `vcpu_on` (which sleeps waiting for the AP bind ack) nor across
   `runtime.has_vcpu_task`. Sequence per request: registry lookup → task-liveness check →
   short gate critical section → drop lock → blocking start → optional restore in its own
   critical section. No lock nesting cycle with `VM_REGISTRY` / `vcpu_task_list` (each is
   acquired alone). Recorded as invariant for smp.rs docs and design doc.
2. **Failure-mode matrix** (each path = diagnostic + no guest-visible corruption):
   - unknown VM / missing runtime → warn, ignore;
   - INIT to running AP → warn "reset unsupported", ignore (D7);
   - INIT to self → warn, ignore;
   - SIPI without wait-for-SIPI state → warn "dropped", ignore;
   - unresolvable arch CPU id → warn via `boot_secondary_cpu`, no sender-register write;
   - `vcpu_on` StartFailed → warn + restore gate bit (retry SIPI works);
     AlreadyOn/OnPending → warn, bit stays cleared;
   - logical-mode INIT/SIPI, SELF/AllIncludingSelf shorthand, SMI/NMI/Reserved modes →
     existing/new log-and-ignore arms; ICR write still returns Ok.
3. **Red-first fidelity**: red tests drive `handle_write(ICRLow/ICRHi)` (public path),
   not private fns; commit order 2→3 guarantees they fail before implementation and pass
   after, with output captured in the final report.
4. **cpu_num==1 identity re-check**: single-vCPU guests hit at most the self-INIT warn
   (log-only), broadcast masks are empty, BSP bind gains one `None` branch, no new
   host-op calls with live targets, firmware tables unchanged.
5. **Config validation**: `phys_cpu_ids` length mismatch already fails loudly
   (`config.rs:500-508`); identity `[0,1]` used by the E2E config; target-resolution
   failures warn at runtime rather than misroute.
6. **Conventions**: Conventional Commit titles per commit; English comments explaining
   invariants (SDM citations, VMCS-currency rationale, gate-lock rules); tests appended
   as `#[cfg(test)] mod tests` at file end; typed errors reused
   (`X86VlapicError::Unsupported`, new `SecondaryCpuBootFailure` enum); no clippy
   `allow` suppressions planned.

## 10. Resume note (2025 resume session, agent ox-alpha)

**State found.** Commits 1–5 of §5 landed as listed; step 6 existed only as uncommitted
WIP: `smp.rs` complete but failing to compile (`ax_err_type!` yields a bare error, so an
early `return ax_err_type!(...)` lacked `Err(..)`; one missing deref in
`take_wait_for_sipi`), the D8 `architecture/mod.rs` boundary widening absent, and
`cpu_up::handle`'s outcome-tuple return already threaded through riscv64/aarch64 call
sites via `.map(|(bound, _)| bound)`. Steps 7–8 entirely absent.

**Finished.**
- Fixed both WIP compile errors; widened the D8 `expect(dead_code)` cfg to include
  `x86_64` (commit c467d2b4c).
- Root-caused two latent dead-code items surfaced by removing the x86_64 expectation:
  deleted the caller-less `CpuUpOps::target_vcpu_id` hook (resolution was inlined into
  `boot_secondary_cpu` by commit 4) and dropped the never-read `UnresolvedTarget(u64)`
  payload (the ID is logged at the source). Both folded into commit c467d2b4c.
- Step 7 (commit 9e85bb727): `test-suit/axvisor/normal/qemu-acpi-smp/`
  (build wrapper + guest config with `cpu_num=2`, `phys_cpu_ids=[0,1]` +
  `smp-direct/qemu-x86_64-vmx.toml`); `run_x86_smp_check` in initramfs.rs asserts a
  readable MADT/APIC table, online set exactly "0-1", cpu1 sysfs presence, and IO-APIC
  initialization; the `axvisor.smp_case=x86` arm is dispatched before the `acpi_case`
  arms (the SMP cmdline carries both markers) with an order assertion added to the
  archive unit test; CI entry `test-axvisor-x86-64-smp-boot-vmx` (kvm-intel) added to
  `.github/ci/checks/axvisor.toml`. Discovery verified locally: `cargo xtask axvisor
  test qemu --list --arch x86_64 --test-group normal` lists `smp-direct-vmx`.
- Step 8 (commit 85d8dbfdb): design doc `docs/design/axvisor-x86-guest-smp-init-sipi.md`
  + SKILL.md SMP Bring-Up Rules item 9.
- Commit 711c9b903: factored `handle()`'s tuple return into named `CpuUpOutcome<A>` to
  satisfy `clippy::type_complexity` at root cause (projection is
  `<A as ArchOps>::DeferredRunWork`; `Architecture` itself does not declare it).

**Validation (worktree).**
- `cargo fmt`: clean.
- `cargo test -p x86_vlapic`: 21 passed (+1 doctest).
- `cargo test -p axvm --features host-test`: 323 passed, incl. 5 new smp gate tests.
- `cargo test -p x86_vcpu`: 85 passed.
- `cargo test -p axbuild`: 956 passed (initramfs marker/order assertions included).
- `cargo xtask clippy --package x86_vlapic --package axvm`: 7/7 checks passed.

**Leftovers / limitations.**
1. E2E `smp-direct-vmx` unverified: no `/dev/kvm` here; needs the KVM runner
   (commands in §7). Guest kernel `CONFIG_SMP=y` still assumed per §8.
2. `.github/ci/checks/axvisor.toml` not re-parsed locally:
   `scripts/test/test_ci_plan.py` requires Python ≥3.11, host has 3.10; the new
   `[[check]]` mirrors adjacent entries' schema exactly, so risk is minimal.
3. Pre-existing baseline warning outside this branch:
   `clippy::needless_return` at `os/arceos/modules/axtask/src/run_queue.rs:188`
   (`os/` untouched by feat/x86-guest-smp); left alone deliberately.
