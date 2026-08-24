//! Shared secondary-vCPU boot flow for architectures that expose CPU-up exits.

use axvm_types::GuestPhysAddr;

use crate::{
    AxVmResult,
    architecture::{Architecture, BoundVcpuExit, VcpuRunAction},
    runtime::vcpus::VcpuOnError,
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct CpuUpExit {
    pub(crate) target_cpu: u64,
    pub(crate) entry_point: GuestPhysAddr,
    pub(crate) arg: u64,
}

/// Failure of one secondary-vCPU boot attempt.
#[derive(Debug)]
pub(crate) enum SecondaryCpuBootFailure {
    /// Architecture-visible CPU ID did not resolve to a VM-local vCPU.
    UnresolvedTarget(u64),
    /// The target vCPU rejected or failed the boot attempt.
    Boot(VcpuOnError),
}

/// Resolves architecture-visible CPU IDs through the VM-owned topology.
pub(crate) trait VmArchCpuIdResolver {
    fn vcpu_id_for_arch_cpu_id(&self, arch_cpu_id: usize) -> Option<usize>;
}

impl VmArchCpuIdResolver for crate::AxVM {
    fn vcpu_id_for_arch_cpu_id(&self, arch_cpu_id: usize) -> Option<usize> {
        self.get_vcpu_affinities_pcpu_ids().into_iter().find_map(
            |(vcpu_id, _, configured_cpu_id)| (configured_cpu_id == arch_cpu_id).then_some(vcpu_id),
        )
    }
}

/// Boots one secondary vCPU and reports which VM-local vCPU was started.
///
/// This is the transport-independent core shared by hypercall-driven CPU-up
/// (PSCI/SBI) and interrupt-controller-driven CPU-up (x86 INIT-SIPI): resolve
/// the architecture CPU ID, then hand the entry point to the generic vCPU
/// runtime. Sender-visible success or error signaling stays with the caller so
/// transports without a return-value ABI can omit it.
pub(crate) fn boot_secondary_cpu(
    vm: &crate::AxVMRef,
    exit: CpuUpExit,
) -> Result<usize, SecondaryCpuBootFailure> {
    let vm_id = vm.id();
    let target_vcpu_id = usize::try_from(exit.target_cpu)
        .ok()
        .and_then(|arch_cpu_id| vm.vcpu_id_for_arch_cpu_id(arch_cpu_id));
    let Some(target_vcpu_id) = target_vcpu_id else {
        warn!(
            "VM[{vm_id}] cannot resolve architecture CPU target {} to a VM-local vCPU",
            exit.target_cpu
        );
        return Err(SecondaryCpuBootFailure::UnresolvedTarget(exit.target_cpu));
    };

    match crate::runtime::vcpus::vcpu_on(
        vm.clone(),
        target_vcpu_id,
        exit.entry_point,
        exit.arg as _,
    ) {
        Ok(()) => Ok(target_vcpu_id),
        Err(err) => {
            warn!("Failed to boot VM[{vm_id}] VCpu[{target_vcpu_id}]: {err:?}");
            Err(SecondaryCpuBootFailure::Boot(err))
        }
    }
}

pub(crate) trait CpuUpOps: Architecture {
    /// Marks a successful CPU-up on the calling vCPU.
    ///
    /// The default clears the common success register. Architectures whose
    /// transport has no return-value ABI must override this with a no-op so
    /// live guest registers are never touched.
    fn set_cpu_up_success(vcpu: &crate::vcpu::AxVCpu<Self::VCpu>) {
        vcpu.set_gpr(0, 0);
    }

    /// Marks a failed CPU-up attempt on the calling vCPU.
    ///
    /// The default follows the hypercall convention of returning an error
    /// value; see [`CpuUpOps::set_cpu_up_success`] for when to override.
    fn report_cpu_up_failure(vcpu: &crate::vcpu::AxVCpu<Self::VCpu>) {
        vcpu.set_return_value(usize::MAX);
    }

    fn target_vcpu_id(vm: &crate::AxVMRef, target_cpu: u64) -> Option<usize> {
        usize::try_from(target_cpu)
            .ok()
            .and_then(|arch_cpu_id| vm.vcpu_id_for_arch_cpu_id(arch_cpu_id))
    }
}

pub(crate) fn handle<A: CpuUpOps>(
    vm: &crate::AxVMRef,
    vcpu: &crate::vcpu::AxVCpu<A::VCpu>,
    exit: CpuUpExit,
) -> AxVmResult<BoundVcpuExit<A::DeferredRunWork>> {
    let vm_id = vm.id();
    let vcpu_id = vcpu.id();
    info!(
        "VM[{vm_id}]'s VCpu[{vcpu_id}] try to boot target_cpu [{}] entry_point={:x} arg={:#x}",
        exit.target_cpu, exit.entry_point, exit.arg
    );

    match boot_secondary_cpu(vm, exit) {
        Ok(_) => A::set_cpu_up_success(vcpu),
        Err(_) => A::report_cpu_up_failure(vcpu),
    }

    Ok(BoundVcpuExit::Complete(VcpuRunAction {
        waits_for_event: false,
        stop_reason: None,
        resets_vm: false,
        exits_vcpu: false,
    }))
}

#[cfg(all(test, feature = "host-test"))]
mod tests {
    use std::sync::Arc;

    use ax_std::os::arceos::sync::IrqSafeMutex;
    use axvm_types::{
        GuestPhysAddr, NestedPagingConfig, VCpuId, VMId, VmArchPerCpuOps, VmArchVcpuOps,
        VmBackendResult,
    };

    use super::*;
    use crate::architecture::{
        Architecture, BootImagePlatform, GuestBootPlatform, HostTimePlatform, MachinePlatform,
    };

    struct RecordingVcpu {
        registers: Arc<IrqSafeMutex<[usize; 2]>>,
    }

    impl VmArchVcpuOps for RecordingVcpu {
        type CreateConfig = Arc<IrqSafeMutex<[usize; 2]>>;
        type SetupConfig = ();
        type Exit = ();

        fn new(
            _vm_id: VMId,
            _vcpu_id: VCpuId,
            registers: Self::CreateConfig,
        ) -> VmBackendResult<Self> {
            Ok(Self { registers })
        }

        fn set_entry(&mut self, _entry: GuestPhysAddr) -> VmBackendResult {
            Ok(())
        }

        fn set_nested_page_table(&mut self, _config: NestedPagingConfig) -> VmBackendResult {
            Ok(())
        }

        fn setup(&mut self, _config: Self::SetupConfig) -> VmBackendResult {
            Ok(())
        }

        fn run(&mut self) -> VmBackendResult<Self::Exit> {
            Ok(())
        }

        fn bind(&mut self) -> VmBackendResult {
            Ok(())
        }

        fn unbind(&mut self) -> VmBackendResult {
            Ok(())
        }

        fn set_gpr(&mut self, reg: usize, val: usize) {
            self.registers.lock()[reg] = val;
        }

        fn inject_interrupt(&mut self, _vector: usize) -> VmBackendResult {
            Ok(())
        }

        fn set_return_value(&mut self, _val: usize) {}
    }

    struct RecordingPerCpu;

    impl VmArchPerCpuOps for RecordingPerCpu {
        fn new(_cpu_id: usize) -> VmBackendResult<Self> {
            Ok(Self)
        }

        fn is_enabled(&self) -> bool {
            true
        }

        fn hardware_enable(&mut self) -> VmBackendResult {
            Ok(())
        }

        fn hardware_disable(&mut self) -> VmBackendResult {
            Ok(())
        }
    }

    struct RecordingArch<const SUCCESS_REGISTER: usize>;

    impl<const SUCCESS_REGISTER: usize> crate::architecture::ArchOps
        for RecordingArch<SUCCESS_REGISTER>
    {
        type VCpu = RecordingVcpu;
        type PerCpu = RecordingPerCpu;
        type DeferredRunWork = ();
        type NestedPageTable = crate::arch::current::ArchNestedPageTable;

        fn has_hardware_support() -> bool {
            true
        }

        fn handle_vcpu_exit_bound(
            _vm: &crate::AxVMRef,
            _vcpu: &crate::vm::AxVCpuRef<Self::VCpu>,
            _exit: <Self::VCpu as VmArchVcpuOps>::Exit,
        ) -> AxVmResult<BoundVcpuExit<Self::DeferredRunWork>> {
            unreachable!("the CPU-up method test never runs a vCPU")
        }

        fn finish_deferred_run_work(
            _vm: &crate::AxVMRef,
            _vcpu: &crate::vm::AxVCpuRef<Self::VCpu>,
            _work: Self::DeferredRunWork,
        ) -> AxVmResult<VcpuRunAction> {
            unreachable!("the CPU-up method test has no deferred work")
        }
    }

    impl<const SUCCESS_REGISTER: usize> MachinePlatform for RecordingArch<SUCCESS_REGISTER> {
        const MACHINE_ARCHITECTURE: crate::machine::MachineArchitecture =
            crate::machine::MachineArchitecture::X86_64;
    }
    impl<const SUCCESS_REGISTER: usize> GuestBootPlatform for RecordingArch<SUCCESS_REGISTER> {}
    impl<const SUCCESS_REGISTER: usize> BootImagePlatform for RecordingArch<SUCCESS_REGISTER> {}
    impl<const SUCCESS_REGISTER: usize> HostTimePlatform for RecordingArch<SUCCESS_REGISTER> {}
    impl<const SUCCESS_REGISTER: usize> Architecture for RecordingArch<SUCCESS_REGISTER> {}

    impl CpuUpOps for RecordingArch<0> {}

    impl CpuUpOps for RecordingArch<1> {
        fn set_cpu_up_success(vcpu: &crate::vcpu::AxVCpu<Self::VCpu>) {
            vcpu.set_gpr(1, 0);
        }
    }

    fn recording_vcpu(
        registers: Arc<IrqSafeMutex<[usize; 2]>>,
    ) -> crate::vm::AxVCpuRef<RecordingVcpu> {
        // AxVCpuRef intentionally uses Arc because production vCPUs can outlive the creating
        // task. This test never sends its fake vCPU to another thread.
        #[expect(
            clippy::arc_with_non_send_sync,
            reason = "the AxVCpuRef API requires Arc while this fake vCPU remains single-threaded"
        )]
        Arc::new(crate::vcpu::AxVCpu::<RecordingVcpu>::new(1, 0, None, registers).unwrap())
    }

    #[test]
    fn cpu_up_success_supports_common_default_and_architecture_override() {
        let default_registers = Arc::new(IrqSafeMutex::new([usize::MAX; 2]));
        let default_vcpu = recording_vcpu(default_registers.clone());
        RecordingArch::<0>::set_cpu_up_success(&default_vcpu);
        assert_eq!(*default_registers.lock(), [0, usize::MAX]);

        let override_registers = Arc::new(IrqSafeMutex::new([usize::MAX; 2]));
        let override_vcpu = recording_vcpu(override_registers.clone());
        RecordingArch::<1>::set_cpu_up_success(&override_vcpu);
        assert_eq!(*override_registers.lock(), [usize::MAX, 0]);
    }
}
