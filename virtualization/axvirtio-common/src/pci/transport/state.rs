use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::VirtioQueueGeneration;
use crate::{NoGuestMemoryAccessor, VirtioQueue, constants::VIRTIO_STATUS_DEVICE_NEEDS_RESET};

#[derive(Debug)]
pub(super) struct QueueActivity {
    pub(super) accepting: AtomicBool,
    pub(super) active: AtomicUsize,
    pub(super) resetting: AtomicBool,
}

impl QueueActivity {
    pub(super) const fn new() -> Self {
        Self {
            accepting: AtomicBool::new(true),
            active: AtomicUsize::new(0),
            resetting: AtomicBool::new(false),
        }
    }

    pub(super) fn begin_reset(&self) -> bool {
        self.resetting
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(super) fn acquire(
        self: &Arc<Self>,
        generation: VirtioQueueGeneration,
    ) -> Option<ActivityPermit> {
        if !self.accepting.load(Ordering::Acquire) {
            return None;
        }
        self.active.fetch_add(1, Ordering::AcqRel);
        if self.accepting.load(Ordering::Acquire) {
            Some(ActivityPermit {
                activity: Arc::clone(self),
                generation,
            })
        } else {
            self.active.fetch_sub(1, Ordering::AcqRel);
            None
        }
    }

    pub(super) fn close_and_drain(&self) -> bool {
        self.accepting.store(false, Ordering::Release);
        for _ in 0..super::RESET_DRAIN_SPIN_LIMIT {
            if self.active.load(Ordering::Acquire) == 0 {
                return true;
            }
            core::hint::spin_loop();
        }
        false
    }

    pub(super) fn reopen(&self) {
        self.accepting.store(true, Ordering::Release);
    }

    pub(super) fn finish_reset(&self) {
        self.resetting.store(false, Ordering::Release);
    }
}

/// Permit covering synchronous queue activity through completion publication.
#[derive(Debug)]
pub struct ActivityPermit {
    pub(super) activity: Arc<QueueActivity>,
    pub(super) generation: VirtioQueueGeneration,
}

impl Drop for ActivityPermit {
    fn drop(&mut self) {
        self.activity.active.fetch_sub(1, Ordering::Release);
    }
}

#[derive(Debug)]
pub(super) struct QueueState {
    pub(super) queue: VirtioQueue<NoGuestMemoryAccessor>,
    pub(super) enabled: bool,
    pub(super) processing: bool,
}

pub(super) struct TransportState {
    pub(super) device_feature_select: u32,
    pub(super) driver_feature_select: u32,
    pub(super) driver_features: u64,
    pub(super) msix_config: u16,
    pub(super) status: u8,
    pub(super) queue_select: u16,
    pub(super) queue_size: u16,
    pub(super) queues: alloc::vec::Vec<QueueState>,
    pub(super) queue_generation: u64,
    pub(super) config_generation: u8,
    pub(super) fault_reported: bool,
    pub(super) device_needs_reset: bool,
    pub(super) reset_pending: bool,
}

impl TransportState {
    pub(super) fn new(queue_num_max: u16, queue_size_max: u16) -> Self {
        let queues = (0..queue_num_max)
            .map(|index| QueueState {
                queue: VirtioQueue::new(index, queue_size_max, Arc::new(NoGuestMemoryAccessor)),
                enabled: false,
                processing: false,
            })
            .collect();
        Self {
            device_feature_select: 0,
            driver_feature_select: 0,
            driver_features: 0,
            msix_config: u16::MAX,
            status: 0,
            queue_select: 0,
            queue_size: queue_size_max,
            queues,
            queue_generation: 0,
            config_generation: 0,
            fault_reported: false,
            device_needs_reset: false,
            reset_pending: false,
        }
    }

    pub(super) fn reset(&mut self, queue_size_max: u16) {
        self.device_feature_select = 0;
        self.driver_feature_select = 0;
        self.driver_features = 0;
        self.msix_config = u16::MAX;
        self.status = VIRTIO_STATUS_DEVICE_NEEDS_RESET as u8;
        self.queue_select = 0;
        self.queue_size = queue_size_max;
        self.queue_generation = self.queue_generation.wrapping_add(1);
        self.config_generation = self.config_generation.wrapping_add(1);
        self.fault_reported = false;
        self.device_needs_reset = true;
        self.reset_pending = true;
        for queue in &mut self.queues {
            queue.enabled = false;
            queue.processing = false;
            queue.queue.reset();
        }
    }
}
