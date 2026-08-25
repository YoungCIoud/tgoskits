//! x86 guest AP bring-up: INIT-SIPI acceptance for the emulated local APIC.
//!
//! The MP initialization protocol (Intel SDM Vol.3 §9.4) needs two host
//! operations from the vLAPIC: INIT puts a target into wait-for-SIPI, and SIPI
//! starts one wait-for-SIPI target in real mode at `CS = vector << 12`. This
//! module owns the wait-for-SIPI gate and routes SIPI deliveries through the
//! shared [`crate::architecture::cpu_up`] flow so guest APs reuse the same
//! lazy task spawn as PSCI/SBI CPU-up.
//!
//! Locking invariant: the gate lock is never held across the blocking
//! secondary-CPU start or across any runtime lock. Each request performs its
//! gate update in a short critical section and drops the lock before any
//! wait-for-ack work begins.

use std::collections::BTreeMap;

use ax_std::os::arceos::sync::IrqSafeMutex;
use axvm_types::GuestPhysAddr;

use super::X86_64Arch;
use crate::{
    AxVmResult,
    architecture::cpu_up::{self, SecondaryCpuBootFailure},
    ax_err_type,
    runtime::vcpus::VcpuOnError,
};

/// Guest physical entry address of a SIPI vector (`vector << 12`).
const fn sipi_entry_gpa(start_vector: u8) -> usize {
    (start_vector as usize) << 12
}

fn wait_for_sipi_bit(vcpu_id: usize) -> AxVmResult<usize> {
    1usize
        .checked_shl(vcpu_id as u32)
        .ok_or_else(|| ax_err_type!(InvalidInput, format!("vCPU {vcpu_id} exceeds SIPI bitmap")))
}

/// Wait-for-SIPI bitmask per VM, registered while the VM runtime is entered
/// and dropped when it exits, so start/reset/stop rebuilds state cleanly.
static WAIT_FOR_SIPI_MASKS: IrqSafeMutex<BTreeMap<usize, usize>> =
    IrqSafeMutex::new(BTreeMap::new());

/// Registers empty gate state for a starting VM.
pub(crate) fn enter_runtime(vm_id: usize) -> AxVmResult {
    WAIT_FOR_SIPI_MASKS.lock().insert(vm_id, 0);
    Ok(())
}

/// Drops all gate state of a stopped or failed VM. Absent state is normal for
/// a start that rolled back before any AP was initialized.
pub(crate) fn exit_runtime(vm_id: usize) -> AxVmResult {
    WAIT_FOR_SIPI_MASKS.lock().remove(&vm_id);
    Ok(())
}

/// Accepts an INIT request: marks `vcpu_id` as waiting for its first SIPI.
///
/// INIT addressed to an already-running vCPU is logged and ignored because
/// cross-thread reset of live guest registers is not supported; the delivery
/// itself stays non-fatal to the writing guest.
pub(crate) fn handle_ap_init_request(vm_id: usize, vcpu_id: usize) -> AxVmResult {
    let vm = crate::get_vm_by_id(vm_id)
        .ok_or_else(|| ax_err_type!(BadState, format!("VM[{vm_id}] not found")))?;
    if vm.runtime_handle()?.has_vcpu_task(vcpu_id) {
        warn!(
            "VM[{vm_id}] VCpu[{vcpu_id}] received INIT while running; resetting a running vCPU is \
             not supported, ignoring"
        );
        return Ok(());
    }

    let bit = wait_for_sipi_bit(vcpu_id)?;
    *WAIT_FOR_SIPI_MASKS.lock().entry(vm_id).or_default() |= bit;
    info!("VM[{vm_id}] VCpu[{vcpu_id}] accepted INIT: waiting for SIPI");
    Ok(())
}

/// Accepts a SIPI request by starting the target through the shared CPU-up
/// flow with the protocol entry address `vector << 12`.
///
/// SIPIs without a preceding INIT are dropped exactly like hardware drops a
/// STARTUP aimed at a processor that is not in wait-for-SIPI. A retryable
/// start failure restores the gate bit so the guest's follow-up SIPI can
/// succeed (Linux issues two STARTUP IPIs per AP).
pub(crate) fn handle_sipi_startup_request(
    vm_id: usize,
    vcpu_id: usize,
    start_vector: u8,
) -> AxVmResult {
    let vm = crate::get_vm_by_id(vm_id)
        .ok_or_else(|| ax_err_type!(BadState, format!("VM[{vm_id}] not found")))?;

    let consumed = take_wait_for_sipi(vm_id, vcpu_id)?;
    if !consumed {
        warn!(
            "VM[{vm_id}] VCpu[{vcpu_id}] dropped SIPI vector {start_vector:#x}: target is not in \
             wait-for-SIPI state"
        );
        return Ok(());
    }

    // The CPU-up argument carries the start vector: x86 has no handoff GPR
    // ABI, so ArchOps::set_vcpu_on_args records it on the backend instead.
    let exit = cpu_up::CpuUpExit {
        target_cpu: vcpu_id as u64,
        entry_point: GuestPhysAddr::from(sipi_entry_gpa(start_vector)),
        arg: u64::from(start_vector),
    };

    // Deliver inside the current-vCPU scope: cpu_up signals the sending vCPU,
    // whose registers may only be touched on this pinned host thread.
    crate::vcpu::with_current_vcpu::<super::AxvmX86Vcpu, AxVmResult>(|sender| {
        let Some(sender) = sender else {
            return Err(ax_err_type!(
                BadState,
                "SIPI startup requires vCPU execution context"
            ));
        };
        let (_, outcome) = cpu_up::handle::<X86_64Arch>(&vm, sender, exit)?;
        if let Err(failure) = &outcome {
            warn!(
                "VM[{vm_id}] VCpu[{vcpu_id}] SIPI startup failed for vector {start_vector:#x}: \
                 {failure:?}"
            );
            if is_retryable_start_failure(failure) {
                restore_wait_for_sipi(vm_id, vcpu_id)?;
            }
        }
        Ok(())
    })
}

/// Only a failed spawn may be retried: `AlreadyOn`/`OnPending` mean another
/// start owns the target, so the gate stays closed until the next INIT.
fn is_retryable_start_failure(failure: &SecondaryCpuBootFailure) -> bool {
    matches!(
        failure,
        SecondaryCpuBootFailure::Boot(VcpuOnError::StartFailed)
    )
}

/// Atomically consumes the wait-for-SIPI marker of one target.
fn take_wait_for_sipi(vm_id: usize, vcpu_id: usize) -> AxVmResult<bool> {
    let bit = wait_for_sipi_bit(vcpu_id)?;
    let mut masks = WAIT_FOR_SIPI_MASKS.lock();
    match masks.get_mut(&vm_id) {
        Some(mask) if *mask & bit != 0 => {
            *mask &= !bit;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Restores the gate after a retryable failure so a follow-up SIPI can retry.
fn restore_wait_for_sipi(vm_id: usize, vcpu_id: usize) -> AxVmResult {
    let bit = wait_for_sipi_bit(vcpu_id)?;
    *WAIT_FOR_SIPI_MASKS.lock().entry(vm_id).or_default() |= bit;
    Ok(())
}

#[cfg(all(test, feature = "host-test"))]
mod tests {
    use super::*;

    /// Tests need stable per-test VM IDs so parallel runs cannot collide on
    /// the shared static gate map.
    fn unique_vm_id(tag: u8) -> usize {
        0xa000 + tag as usize
    }

    #[test]
    fn sipi_entry_gpa_shifts_vector_by_page_bits() {
        assert_eq!(sipi_entry_gpa(0x08), 0x8000);
        assert_eq!(sipi_entry_gpa(0xd2), 0xd2_000);
        assert_eq!(sipi_entry_gpa(0xff), 0xff_000);
    }

    #[test]
    fn init_marks_gate_and_sipi_consumes_it_exactly_once() -> AxVmResult {
        let vm_id = unique_vm_id(1);
        enter_runtime(vm_id)?;
        assert!(!take_wait_for_sipi(vm_id, 1)?);

        // Repeated INIT stays idempotent while waiting.
        restore_wait_for_sipi(vm_id, 1)?;
        restore_wait_for_sipi(vm_id, 1)?;
        assert_eq!(
            (take_wait_for_sipi(vm_id, 1)?, take_wait_for_sipi(vm_id, 1)?),
            (true, false)
        );

        exit_runtime(vm_id)?;
        assert!(!WAIT_FOR_SIPI_MASKS.lock().contains_key(&vm_id));
        Ok(())
    }

    #[test]
    fn exit_runtime_drops_all_pending_wait_for_sipi_state() -> AxVmResult {
        let vm_id = unique_vm_id(2);
        enter_runtime(vm_id)?;
        restore_wait_for_sipi(vm_id, 0)?;
        restore_wait_for_sipi(vm_id, 3)?;

        exit_runtime(vm_id)?;
        assert!(!WAIT_FOR_SIPI_MASKS.lock().contains_key(&vm_id));
        Ok(())
    }

    #[test]
    fn sipl_bit_rejects_targets_beyond_bitmap_width() {
        assert!(wait_for_sipi_bit(usize::BITS as usize).is_err());
        assert!(wait_for_sipi_bit(usize::BITS as usize - 1).is_ok());
    }

    #[test]
    fn only_failed_spawns_are_retryable() {
        use SecondaryCpuBootFailure::{Boot, UnresolvedTarget};

        assert!(is_retryable_start_failure(&Boot(VcpuOnError::StartFailed)));
        assert!(!is_retryable_start_failure(&Boot(VcpuOnError::AlreadyOn)));
        assert!(!is_retryable_start_failure(&Boot(VcpuOnError::OnPending)));
        assert!(!is_retryable_start_failure(&UnresolvedTarget));
    }
}
