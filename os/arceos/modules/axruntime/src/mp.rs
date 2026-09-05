// Copyright 2025 The Axvisor Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use core::sync::atomic::{AtomicUsize, Ordering};

use ax_hal::mem::VirtAddr;

static SECONDARY_CPUID_BY_SLOT: [AtomicUsize; crate::build_info::CPU_CAPACITY - 1] =
    [const { AtomicUsize::new(usize::MAX) }; crate::build_info::CPU_CAPACITY - 1];

static ENTERED_CPUS: AtomicUsize = AtomicUsize::new(1);

// Keep the last observed secondary-CPU initialization stage available to the
// primary CPU while it waits for the secondary to publish its entry.
const SECONDARY_STAGE_NONE: usize = 0;
const SECONDARY_STAGE_ENTERED: usize = 1;
const SECONDARY_STAGE_PERCPU_INIT_BEGIN: usize = 2;
const SECONDARY_STAGE_PERCPU_READY: usize = 3;
const SECONDARY_STAGE_SLAB_INIT_BEGIN: usize = 4;
const SECONDARY_STAGE_SLAB_READY: usize = 5;
const SECONDARY_STAGE_EARLY_INIT_BEGIN: usize = 6;
const SECONDARY_STAGE_EARLY_READY: usize = 7;
const SECONDARY_STAGE_ENTERED_PUBLISHED: usize = 8;
#[cfg(feature = "paging")]
const SECONDARY_STAGE_MEMORY_INIT_BEGIN: usize = 9;
#[cfg(feature = "paging")]
const SECONDARY_STAGE_MEMORY_READY: usize = 10;
const SECONDARY_STAGE_LATER_INIT_BEGIN: usize = 11;
const SECONDARY_STAGE_LATER_READY: usize = 12;
const SECONDARY_STAGE_SCHEDULER_INIT_BEGIN: usize = 13;
const SECONDARY_STAGE_SCHEDULER_READY: usize = 14;
const SECONDARY_STAGE_IRQ_INIT_BEGIN: usize = 15;
const SECONDARY_STAGE_IRQ_READY: usize = 16;
const SECONDARY_STAGE_INITED_PUBLISHED: usize = 17;

static SECONDARY_STAGE_BY_CPU: [AtomicUsize; crate::build_info::CPU_CAPACITY] =
    [const { AtomicUsize::new(SECONDARY_STAGE_NONE) }; crate::build_info::CPU_CAPACITY];

fn set_secondary_stage(cpu_id: usize, stage: usize) {
    if let Some(state) = SECONDARY_STAGE_BY_CPU.get(cpu_id) {
        state.store(stage, Ordering::Release);
    }
}

fn secondary_stage(cpu_id: usize) -> usize {
    SECONDARY_STAGE_BY_CPU
        .get(cpu_id)
        .map_or(SECONDARY_STAGE_NONE, |state| state.load(Ordering::Acquire))
}

fn secondary_boot_stack_bounds(cpu_id: usize) -> (VirtAddr, usize) {
    ax_hal::mem::boot_stack_bounds(cpu_id)
}

fn prepare_secondary_boot_stack(slot: usize, cpu_id: usize) {
    SECONDARY_CPUID_BY_SLOT[slot].store(cpu_id, Ordering::Release);
}

#[allow(clippy::absurd_extreme_comparisons)]
pub fn start_secondary_cpus(primary_cpu_id: usize) {
    let mut slot = 0;
    let cpu_num = ax_hal::cpu_num();
    assert_eq!(
        ax_hal::mem::cpu_shared_memory_model(),
        ax_hal::mem::CpuSharedMemoryModel::Coherent,
        "SMP requires coherent cacheable memory shared by all CPUs"
    );
    for i in 0..cpu_num {
        if i != primary_cpu_id && slot < cpu_num - 1 {
            prepare_secondary_boot_stack(slot, i);

            let stack_top = 0;

            info!("[HV] SMP primary: booting secondary CPU {i} (slot={slot})");
            ax_hal::power::cpu_boot(i, stack_top);
            info!("[HV] SMP primary: cpu_boot returned for secondary CPU {i}");
            slot += 1;

            let mut wait_spins: usize = 0;
            while ENTERED_CPUS.load(Ordering::Acquire) <= slot {
                wait_spins = wait_spins.saturating_add(1);
                if wait_spins == 1 || wait_spins.is_power_of_two() {
                    info!(
                        "[HV] SMP primary: waiting for secondary CPU {i}: spins={wait_spins}, \
                         entered={}, stage={}",
                        ENTERED_CPUS.load(Ordering::Acquire),
                        secondary_stage(i),
                    );
                }
                core::hint::spin_loop();
            }
            info!("[HV] SMP primary: secondary CPU {i} entered runtime");
        }
    }
}

/// The main entry point of the ArceOS runtime for secondary cores.
///
/// It is called from the bootstrapping code in the specific platform crate.
#[ax_plat::secondary_main]
pub fn rust_main_secondary(cpu_id: usize) -> ! {
    set_secondary_stage(cpu_id, SECONDARY_STAGE_ENTERED);
    info!("[HV] SMP secondary {cpu_id}: entered rust_main_secondary");
    // Park harts whose logical index is beyond the compile-time CPU count: QEMU
    // may start more harts (`-smp M`) than the kernel was built for
    // (`CPU_CAPACITY == N`). Mirror Linux — run on the first N CPUs and park the
    // excess, rather than panicking in `percpu::init_secondary(cpu_id)` /
    // `AxCpuMask::one_shot(cpu_id)` / `RUN_QUEUES[cpu_id]`, which all assert
    // `index < CPU_CAPACITY`. Must precede `init_secondary`, which would otherwise
    // mis-index the per-CPU area first.
    if cpu_id >= crate::build_info::CPU_CAPACITY {
        loop {
            ax_hal::asm::wait_for_irqs();
        }
    }
    set_secondary_stage(cpu_id, SECONDARY_STAGE_PERCPU_INIT_BEGIN);
    ax_hal::percpu::init_secondary(cpu_id);
    set_secondary_stage(cpu_id, SECONDARY_STAGE_PERCPU_READY);
    info!("[HV] SMP secondary {cpu_id}: per-CPU state initialized");
    // After per-CPU init, before scheduler/IPI/IRQ paths can allocate.
    // This is a no-op for allocator backends that do not need per-CPU state.
    set_secondary_stage(cpu_id, SECONDARY_STAGE_SLAB_INIT_BEGIN);
    ax_alloc::init_percpu_slab(cpu_id);
    set_secondary_stage(cpu_id, SECONDARY_STAGE_SLAB_READY);
    set_secondary_stage(cpu_id, SECONDARY_STAGE_EARLY_INIT_BEGIN);
    ax_hal::init_early_secondary(cpu_id);
    set_secondary_stage(cpu_id, SECONDARY_STAGE_EARLY_READY);
    info!("[HV] SMP secondary {cpu_id}: early platform init complete");

    set_secondary_stage(cpu_id, SECONDARY_STAGE_ENTERED_PUBLISHED);
    ENTERED_CPUS.fetch_add(1, Ordering::Release);
    info!("Secondary CPU {cpu_id} started.");

    #[cfg(feature = "paging")]
    set_secondary_stage(cpu_id, SECONDARY_STAGE_MEMORY_INIT_BEGIN);
    #[cfg(feature = "paging")]
    ax_mm::init_memory_management_secondary();
    #[cfg(feature = "paging")]
    set_secondary_stage(cpu_id, SECONDARY_STAGE_MEMORY_READY);

    set_secondary_stage(cpu_id, SECONDARY_STAGE_LATER_INIT_BEGIN);
    ax_hal::init_later_secondary(cpu_id);
    set_secondary_stage(cpu_id, SECONDARY_STAGE_LATER_READY);

    set_secondary_stage(cpu_id, SECONDARY_STAGE_SCHEDULER_INIT_BEGIN);
    let (stack_ptr, stack_size) = secondary_boot_stack_bounds(cpu_id);
    ax_task::init_scheduler_secondary(stack_ptr, stack_size);
    set_secondary_stage(cpu_id, SECONDARY_STAGE_SCHEDULER_READY);
    super::preempt::release_bootstrap();

    #[cfg(feature = "ipi")]
    ax_ipi::init();

    // Bring up local IRQ/IPI delivery before publishing INITED_CPUS so the
    // primary cannot enter user-visible init while remote CPUs still lack SGI
    // handlers or pending per-CPU IRQ enables.
    set_secondary_stage(cpu_id, SECONDARY_STAGE_IRQ_INIT_BEGIN);
    super::init_percpu_irq(cpu_id);

    ax_hal::asm::enable_irqs();

    #[cfg(feature = "ipi")]
    {
        ax_hal::asm::flush_tlb(None);
        ax_ipi::mark_current_cpu_ready();
    }
    set_secondary_stage(cpu_id, SECONDARY_STAGE_IRQ_READY);

    // Publishing a log record is safe as soon as the per-CPU area exists, but
    // waking the owner worker may select a run queue or send an IPI. Publish
    // that separate capability only after this CPU has completed every
    // scheduler, IRQ, and IPI prerequisite compiled into this runtime.
    super::serial::mark_log_wake_ready(cpu_id);

    info!("Secondary CPU {cpu_id:x} init OK.");
    set_secondary_stage(cpu_id, SECONDARY_STAGE_INITED_PUBLISHED);
    super::INITED_CPUS.fetch_add(1, Ordering::Release);

    while !super::is_init_ok() {
        core::hint::spin_loop();
    }

    ax_task::run_idle();
}
