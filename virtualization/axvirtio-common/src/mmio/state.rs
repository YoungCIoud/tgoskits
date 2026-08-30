//! Shared VirtIO MMIO transport state machine.
//!
//! Device implementations (block, net, ...) own only their device-specific
//! config space and data paths; the standard MMIO register set — magic,
//! version, feature selectors, driver features, queue selector/size/ready,
//! queue address LOW/HIGH, status, interrupt status/ack, config generation —
//! is handled here so it is not duplicated per device.

use alloc::{sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use ax_sync::SpinLock as Mutex;
use axaddrspace::GuestMemoryAccessor;
use axvm_types::{AccessWidth, GuestPhysAddr};

use crate::{
    VirtioQueue, VirtioResult, constants as vc, error::VirtioError, mmio::transport,
    pci::VirtioQueueGeneration,
};

/// Result of a standard-register MMIO read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MmioReadOutcome {
    /// A standard register value.
    Standard(u32),
    /// A read inside the device-specific config region; the device interprets it.
    DeviceConfig { offset: u64, width: AccessWidth },
}

/// Side effect an MMIO write asks the device driver (block/net) to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MmioWriteAction {
    /// Nothing for the device to do.
    None,
    /// The guest kicked a queue; the device runs its data path.
    QueueNotified {
        /// Queue selected by the guest notification.
        index: u16,
        /// Queue configuration lifetime observed for this notification.
        generation: VirtioQueueGeneration,
    },
    /// The guest started a reset; the device adapter must complete
    /// device-specific cleanup before final status publication.
    Reset,
    /// An acknowledged interrupt bit was raised again after the guest's last
    /// interrupt-status read and must be signalled again.
    InterruptPending,
}

#[derive(Default)]
struct InterruptState {
    pending: u32,
    raised_after_read: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct QueueConfigurationSnapshot {
    size: u16,
    ready: bool,
    desc_table_addr: GuestPhysAddr,
    avail_ring_addr: GuestPhysAddr,
    used_ring_addr: GuestPhysAddr,
    event_idx_enabled: bool,
}

impl QueueConfigurationSnapshot {
    fn from_queue<T: GuestMemoryAccessor + Clone>(queue: &VirtioQueue<T>) -> Self {
        Self {
            size: queue.size,
            ready: queue.ready,
            desc_table_addr: queue.desc_table_addr,
            avail_ring_addr: queue.avail_ring_addr,
            used_ring_addr: queue.used_ring_addr,
            event_idx_enabled: queue.event_idx_enabled,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct QueueReadyValidationToken {
    generation: VirtioQueueGeneration,
    configuration: QueueConfigurationSnapshot,
}

struct QueueReadyValidationSnapshot<T: GuestMemoryAccessor + Clone> {
    token: QueueReadyValidationToken,
    queue: VirtioQueue<T>,
}

struct QueueActivity {
    accepting: AtomicBool,
    active: AtomicUsize,
    resetting: AtomicBool,
    generation: AtomicU64,
}

impl QueueActivity {
    fn new() -> Self {
        Self {
            accepting: AtomicBool::new(true),
            active: AtomicUsize::new(0),
            resetting: AtomicBool::new(false),
            generation: AtomicU64::new(0),
        }
    }

    fn begin_reset(&self) -> bool {
        if self
            .resetting
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        self.accepting.store(false, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
        true
    }

    fn current_generation(&self) -> VirtioQueueGeneration {
        VirtioQueueGeneration::from_value(self.generation.load(Ordering::Acquire))
    }

    fn is_accepting(&self) -> bool {
        self.accepting.load(Ordering::Acquire)
    }

    fn acquire(
        self: &Arc<Self>,
        expected_generation: VirtioQueueGeneration,
    ) -> Option<MmioActivityPermit> {
        if !self.is_accepting()
            || self.generation.load(Ordering::Acquire) != expected_generation.value()
        {
            return None;
        }
        self.active.fetch_add(1, Ordering::AcqRel);
        if self.is_accepting()
            && self.generation.load(Ordering::Acquire) == expected_generation.value()
        {
            Some(MmioActivityPermit {
                activity: Arc::clone(self),
            })
        } else {
            self.active.fetch_sub(1, Ordering::Release);
            None
        }
    }

    fn close_and_drain(&self) -> bool {
        self.accepting.store(false, Ordering::Release);
        for _ in 0..(1 << 20) {
            if self.active.load(Ordering::Acquire) == 0 {
                return true;
            }
            core::hint::spin_loop();
        }
        false
    }

    fn reopen(&self) {
        self.accepting.store(true, Ordering::Release);
        self.resetting.store(false, Ordering::Release);
    }

    fn abort_reset(&self) {
        // Keep admission closed after a bounded drain failure. A later
        // status-zero write may retry the reset once the active operation has
        // released its permit.
        self.resetting.store(false, Ordering::Release);
    }
}

struct MmioActivityPermit {
    activity: Arc<QueueActivity>,
}

impl Drop for MmioActivityPermit {
    fn drop(&mut self) {
        self.activity.active.fetch_sub(1, Ordering::Release);
    }
}

/// An MMIO queue temporarily owned by one queue-processing operation.
pub struct MmioQueueLease<'state, T: GuestMemoryAccessor + Clone> {
    state: &'state VirtioMmioState<T>,
    index: u16,
    generation: VirtioQueueGeneration,
    queue: Option<VirtioQueue<T>>,
    _activity: Option<MmioActivityPermit>,
}

impl<T: GuestMemoryAccessor + Clone> MmioQueueLease<'_, T> {
    /// Accesses the queue owned by this processing operation, if not restored.
    pub fn queue(&self) -> Option<&VirtioQueue<T>> {
        self.queue.as_ref()
    }

    /// Mutably accesses the queue owned by this processing operation, if not restored.
    pub fn queue_mut(&mut self) -> Option<&mut VirtioQueue<T>> {
        self.queue.as_mut()
    }

    /// Returns the queue configuration lifetime admitted by this lease.
    pub const fn generation(&self) -> VirtioQueueGeneration {
        self.generation
    }

    /// Returns the queue while retaining the activity permit.
    ///
    /// The owner must keep this lease alive until any interrupt or other
    /// completion notification has been published or suppressed.
    pub fn restore_queue(&mut self) {
        self.restore();
    }

    fn restore(&mut self) {
        let Some(queue) = self.queue.take() else {
            return;
        };
        let mut processing = self.state.queue_processing.lock_irqsave();
        let mut queues = self.state.queues.lock_irqsave();
        if let Some(slot) = queues.get_mut(self.index as usize) {
            *slot = queue;
        }
        if let Some(in_progress) = processing.get_mut(self.index as usize) {
            *in_progress = false;
        }
    }
}

impl<T: GuestMemoryAccessor + Clone> Drop for MmioQueueLease<'_, T> {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Shared VirtIO MMIO transport state plus the device's queues.
///
/// `device_id`, `vendor_id` and `device_features` are fixed at construction.
/// Feature negotiation is validated here (`driver_features` must be a subset of
/// `device_features` when the driver seals `FEATURES_OK`).
pub struct VirtioMmioState<T: GuestMemoryAccessor + Clone> {
    base_ipa: GuestPhysAddr,
    length: usize,
    device_id: u32,
    vendor_id: u32,
    device_features: u64,
    status: Mutex<u32>,
    driver_features: Mutex<u64>,
    features_sealed: Mutex<bool>,
    device_features_sel: Mutex<u32>,
    driver_features_sel: Mutex<u32>,
    queue_sel: Mutex<u16>,
    /// Serializes queue configuration register writes without blocking the data path.
    queue_config_transaction: Mutex<()>,
    queues: Mutex<Vec<VirtioQueue<T>>>,
    queue_processing: Mutex<Vec<bool>>,
    activity: Arc<QueueActivity>,
    interrupt_status: Mutex<InterruptState>,
    config_generation: Mutex<u32>,
}

impl<T: GuestMemoryAccessor + Clone> VirtioMmioState<T> {
    /// Construct the transport state with the given device identity, advertised
    /// features and pre-created queues.
    pub fn new(
        base_ipa: GuestPhysAddr,
        length: usize,
        device_id: u32,
        vendor_id: u32,
        device_features: u64,
        queues: Vec<VirtioQueue<T>>,
    ) -> Self {
        Self {
            base_ipa,
            length,
            device_id,
            vendor_id,
            device_features,
            status: Mutex::new(0),
            driver_features: Mutex::new(0),
            features_sealed: Mutex::new(false),
            device_features_sel: Mutex::new(0),
            driver_features_sel: Mutex::new(0),
            queue_sel: Mutex::new(0),
            queue_config_transaction: Mutex::new(()),
            queue_processing: Mutex::new(vec![false; queues.len()]),
            queues: Mutex::new(queues),
            activity: Arc::new(QueueActivity::new()),
            interrupt_status: Mutex::new(InterruptState::default()),
            config_generation: Mutex::new(0),
        }
    }

    /// Base IPA of the MMIO region.
    pub fn base_ipa(&self) -> GuestPhysAddr {
        self.base_ipa
    }

    /// Number of queues.
    pub fn num_queues(&self) -> usize {
        let _processing = self.queue_processing.lock_irqsave();
        self.queues.lock_irqsave().len()
    }

    /// Returns the queue configuration lifetime currently admitted for work.
    pub fn queue_generation(&self) -> VirtioQueueGeneration {
        self.activity.current_generation()
    }

    /// Returns a queue's size and ready state as one configuration snapshot.
    pub fn queue_configuration(&self, index: u16) -> Option<(u16, bool)> {
        let _processing = self.queue_processing.lock_irqsave();
        self.queues
            .lock_irqsave()
            .get(index as usize)
            .map(|queue| (queue.size, queue.ready))
    }

    /// Returns whether one queue has latched a runtime fault.
    pub fn is_queue_faulted(&self, index: usize) -> bool {
        let _processing = self.queue_processing.lock_irqsave();
        self.queues
            .lock_irqsave()
            .get(index)
            .is_some_and(VirtioQueue::is_faulted)
    }

    /// Clones one queue for diagnostic or configuration inspection.
    pub fn queue_snapshot(&self, index: u16) -> Option<VirtioQueue<T>> {
        let _processing = self.queue_processing.lock_irqsave();
        self.queues.lock_irqsave().get(index as usize).cloned()
    }

    /// Takes exclusive ownership of one queue without holding the state lock.
    pub fn take_queue_for_processing(
        &self,
        index: u16,
        generation: VirtioQueueGeneration,
    ) -> Option<MmioQueueLease<'_, T>> {
        let activity = self.activity.acquire(generation)?;
        if !self.is_driver_ok() {
            drop(activity);
            return None;
        }
        let mut processing = self.queue_processing.lock_irqsave();
        let in_progress = processing.get_mut(index as usize)?;
        if *in_progress {
            return None;
        }
        let mut queues = self.queues.lock_irqsave();
        let queue = queues.get_mut(index as usize)?;
        *in_progress = true;
        // Keep a read-only configuration image visible while the real queue is
        // owned by the lease. Configuration writes are rejected by the
        // processing flag, while MMIO reads must not observe an empty queue.
        let replacement = queue.clone();
        let queue = core::mem::replace(queue, replacement);
        Some(MmioQueueLease {
            state: self,
            index,
            generation,
            queue: Some(queue),
            _activity: Some(activity),
        })
    }

    /// Whether the driver has set `DRIVER_OK`.
    pub fn is_driver_ok(&self) -> bool {
        (*self.status.lock_irqsave() & vc::VIRTIO_STATUS_DRIVER_OK) != 0
    }

    /// Raw status register value.
    pub fn status(&self) -> u32 {
        *self.status.lock_irqsave()
    }

    /// Set the status register directly, bypassing validation.
    ///
    /// Intended only for device bring-up helpers that emulate the full driver
    /// sequence; normal status transitions must go through [`mmio_write`](Self::mmio_write).
    pub fn set_status(&self, status: u32) {
        *self.status.lock_irqsave() = status;
    }

    /// The currently selected queue index, if it is in range.
    pub fn selected_queue_index(&self) -> Option<u16> {
        let sel = *self.queue_sel.lock_irqsave();
        if (sel as usize) < self.queues.lock_irqsave().len() {
            Some(sel)
        } else {
            None
        }
    }

    /// Currently negotiated driver features.
    pub fn driver_features(&self) -> u64 {
        *self.driver_features.lock_irqsave()
    }

    /// Advertised device features.
    pub fn device_features(&self) -> u64 {
        self.device_features
    }

    /// Current interrupt status bits.
    pub fn interrupt_status(&self) -> u32 {
        self.interrupt_status.lock_irqsave().pending
    }

    /// OR interrupt bits in (used-ring or config-change notification).
    pub fn set_interrupt(&self, bits: u32) {
        let mut interrupt = self.interrupt_status.lock_irqsave();
        interrupt.pending |= bits;
        interrupt.raised_after_read |= bits;
    }

    /// Increment the config-space generation (call after changing config).
    pub fn bump_config_generation(&self) {
        let mut g = self.config_generation.lock_irqsave();
        *g = g.wrapping_add(1);
    }

    /// Full transport reset: clears driver features, selectors, interrupt
    /// status, status and every queue. Device identity and features are kept.
    pub fn reset(&self) -> VirtioResult<()> {
        self.begin_reset()?;
        self.complete_reset()
    }

    /// Closes MMIO activity admission and drains all active queue operations.
    ///
    /// The caller must invoke [`complete_reset`](Self::complete_reset) after
    /// resetting device-specific state. Until then the old non-zero status and
    /// the closed activity gate remain visible.
    pub fn begin_reset(&self) -> VirtioResult<()> {
        if !self.activity.begin_reset() {
            return Err(VirtioError::WouldBlock);
        }
        if !self.activity.close_and_drain() {
            self.activity.abort_reset();
            return Err(VirtioError::WouldBlock);
        }
        Ok(())
    }

    /// Publishes completion of a reset after device-specific cleanup.
    pub fn complete_reset(&self) -> VirtioResult<()> {
        if !self.activity.resetting.load(Ordering::Acquire) {
            return Err(VirtioError::WouldBlock);
        }
        let _queue_config_guard = self.queue_config_transaction.lock();
        let mut features_sealed = self.features_sealed.lock_irqsave();
        *self.driver_features.lock_irqsave() = 0;
        *self.driver_features_sel.lock_irqsave() = 0;
        *self.device_features_sel.lock_irqsave() = 0;
        *self.queue_sel.lock_irqsave() = 0;
        {
            let mut processing = self.queue_processing.lock_irqsave();
            let mut queues = self.queues.lock_irqsave();
            for (in_progress, queue) in processing.iter_mut().zip(queues.iter_mut()) {
                *in_progress = false;
                queue.reset();
            }
        }
        *self.interrupt_status.lock_irqsave() = InterruptState::default();
        // Keep admission closed while status is still the pre-reset value.
        // Status zero is the final guest-visible reset completion marker.
        *self.status.lock_irqsave() = 0;
        *features_sealed = false;
        self.activity.reopen();
        Ok(())
    }

    /// Handle a standard MMIO read. Out-of-range reads yield `Standard(0)`;
    /// reads inside the config region yield [`MmioReadOutcome::DeviceConfig`].
    pub fn mmio_read(
        &self,
        addr: GuestPhysAddr,
        width: AccessWidth,
    ) -> VirtioResult<MmioReadOutcome> {
        if !transport::is_address_in_range(addr, self.base_ipa, self.length) {
            return Ok(MmioReadOutcome::Standard(0));
        }
        let offset = transport::calculate_offset(addr, self.base_ipa);
        if offset < vc::VIRTIO_MMIO_CONFIG_OFFSET {
            transport::validate_access_width(width)?;
        }

        let value = match offset {
            vc::VIRTIO_MMIO_MAGIC_VALUE => vc::MMIO_MAGIC_VALUE,
            vc::VIRTIO_MMIO_VERSION => vc::MMIO_VERSION,
            vc::VIRTIO_MMIO_DEVICE_ID => self.device_id,
            vc::VIRTIO_MMIO_VENDOR_ID => self.vendor_id,
            vc::VIRTIO_MMIO_DEVICE_FEATURES => {
                let sel = *self.device_features_sel.lock_irqsave();
                if sel >= 2 {
                    0
                } else {
                    (self.device_features >> ((sel as u64) * 32)) as u32
                }
            }
            vc::VIRTIO_MMIO_DEVICE_FEATURES_SEL => *self.device_features_sel.lock_irqsave(),
            vc::VIRTIO_MMIO_DRIVER_FEATURES => {
                let sel = *self.driver_features_sel.lock_irqsave();
                if sel >= 2 {
                    0
                } else {
                    (*self.driver_features.lock_irqsave() >> ((sel as u64) * 32)) as u32
                }
            }
            vc::VIRTIO_MMIO_DRIVER_FEATURES_SEL => *self.driver_features_sel.lock_irqsave(),
            vc::VIRTIO_MMIO_QUEUE_SEL => *self.queue_sel.lock_irqsave() as u32,
            vc::VIRTIO_MMIO_QUEUE_NUM_MAX => vc::DEFAULT_QUEUE_SIZE as u32,
            vc::VIRTIO_MMIO_QUEUE_NUM => {
                let sel = *self.queue_sel.lock_irqsave();
                self.queue_configuration(sel)
                    .map_or(0, |(size, _)| size as u32)
            }
            vc::VIRTIO_MMIO_QUEUE_READY => {
                let sel = *self.queue_sel.lock_irqsave();
                self.queue_configuration(sel)
                    .map_or(0, |(_, ready)| u32::from(ready))
            }
            vc::VIRTIO_MMIO_INTERRUPT_STATUS => {
                let mut interrupt = self.interrupt_status.lock_irqsave();
                let pending = interrupt.pending;
                interrupt.raised_after_read = 0;
                pending
            }
            vc::VIRTIO_MMIO_STATUS => *self.status.lock_irqsave(),
            vc::VIRTIO_MMIO_CONFIG_GENERATION => *self.config_generation.lock_irqsave(),
            _ => {
                if offset >= vc::VIRTIO_MMIO_CONFIG_OFFSET {
                    return Ok(MmioReadOutcome::DeviceConfig {
                        offset: (offset - vc::VIRTIO_MMIO_CONFIG_OFFSET) as u64,
                        width,
                    });
                }
                return Err(VirtioError::InvalidRegister);
            }
        };
        Ok(MmioReadOutcome::Standard(value))
    }

    /// Handle a standard MMIO write and report any action the device must take.
    ///
    /// The `QUEUE_READY` layout validation is screened against the selected
    /// queue's own guest-memory accessor. Runtimes whose real guest memory
    /// only exists as a scoped capability at MMIO access time must use
    /// [`mmio_write_with_memory`](Self::mmio_write_with_memory) instead.
    pub fn mmio_write(
        &self,
        addr: GuestPhysAddr,
        width: AccessWidth,
        val: usize,
    ) -> VirtioResult<MmioWriteAction> {
        self.mmio_write_inner(addr, width, val, None)
    }

    /// Handles a standard MMIO write using a scoped guest-memory capability.
    ///
    /// The capability is used for the `QUEUE_READY` layout validation and
    /// must be backed by the same guest memory the queues' runtime accesses
    /// use; passing a capability over different memory makes the layout
    /// check vacuous.
    pub fn mmio_write_with_memory(
        &self,
        addr: GuestPhysAddr,
        width: AccessWidth,
        val: usize,
        memory: &mut dyn crate::GuestMemory,
    ) -> VirtioResult<MmioWriteAction> {
        self.mmio_write_inner(addr, width, val, Some(memory))
    }

    /// Shared MMIO write implementation.
    ///
    /// `ready_memory` is the scoped capability used to screen the selected
    /// queue's ring layout on a `QUEUE_READY` write; `None` falls back to a
    /// capability built from the queue's own accessor.
    fn mmio_write_inner(
        &self,
        addr: GuestPhysAddr,
        width: AccessWidth,
        val: usize,
        ready_memory: Option<&mut dyn crate::GuestMemory>,
    ) -> VirtioResult<MmioWriteAction> {
        if !transport::is_address_in_range(addr, self.base_ipa, self.length) {
            return Ok(MmioWriteAction::None);
        }
        let offset = transport::calculate_offset(addr, self.base_ipa);
        if offset < vc::VIRTIO_MMIO_CONFIG_OFFSET {
            transport::validate_access_width(width)?;
        }
        let val = val as u32;
        let _queue_config_guard =
            is_queue_config_register(offset).then(|| self.queue_config_transaction.lock());

        match offset {
            vc::VIRTIO_MMIO_DEVICE_FEATURES_SEL => *self.device_features_sel.lock_irqsave() = val,
            vc::VIRTIO_MMIO_DRIVER_FEATURES_SEL => *self.driver_features_sel.lock_irqsave() = val,
            vc::VIRTIO_MMIO_DRIVER_FEATURES => {
                let features_sealed = self.features_sealed.lock_irqsave();
                let sel = *self.driver_features_sel.lock_irqsave() as u64;
                if !*features_sealed && sel < 2 {
                    let mask: u64 = (val as u64) << (sel * 32);
                    let clear: u64 = !(((1u64) << 32) - 1).wrapping_shl((sel * 32) as u32);
                    let mut f = self.driver_features.lock_irqsave();
                    *f = (*f & clear) | mask;
                }
            }
            vc::VIRTIO_MMIO_QUEUE_SEL => {
                let sel = val as u16;
                let in_range = {
                    let _processing = self.queue_processing.lock_irqsave();
                    (sel as usize) < self.queues.lock_irqsave().len()
                };
                if in_range {
                    *self.queue_sel.lock_irqsave() = sel;
                }
            }
            vc::VIRTIO_MMIO_QUEUE_NUM => {
                let sel = *self.queue_sel.lock_irqsave();
                let processing = self.queue_processing.lock_irqsave();
                if let Some(q) = self.queues.lock_irqsave().get_mut(sel as usize)
                    && !processing.get(sel as usize).copied().unwrap_or(true)
                {
                    let _ = q.set_size(val as u16);
                }
            }
            vc::VIRTIO_MMIO_QUEUE_READY => {
                let sel = *self.queue_sel.lock_irqsave();
                if let Some(mut validation) = self.snapshot_queue_ready_validation(sel) {
                    let layout_ok = if val == 0 {
                        true
                    } else if validation.queue.is_configured() {
                        match ready_memory {
                            Some(memory) => validation
                                .queue
                                .validate_layout_with_memory(memory)
                                .and_then(|()| {
                                    validation.queue.set_ready(true);
                                    validation
                                        .queue
                                        .rearm_available_event_with_memory(memory)
                                        .map(|_| ())
                                }),
                            None => {
                                let accessor = validation.queue.accessor().clone();
                                let mut memory = crate::AddressSpaceMemory::new(&*accessor);
                                validation
                                    .queue
                                    .validate_layout_with_memory(&mut memory)
                                    .and_then(|()| {
                                        validation.queue.set_ready(true);
                                        validation
                                            .queue
                                            .rearm_available_event_with_memory(&mut memory)
                                            .map(|_| ())
                                    })
                            }
                        }
                        .is_ok()
                    } else {
                        false
                    };
                    self.commit_queue_ready_validation(sel, val, validation.token, layout_ok);
                }
            }
            vc::VIRTIO_MMIO_QUEUE_NOTIFY => {
                let generation = self.activity.current_generation();
                let status = *self.status.lock_irqsave();
                if status & vc::VIRTIO_STATUS_DRIVER_OK == 0 || !self.activity.is_accepting() {
                    return Ok(MmioWriteAction::None);
                }
                return Ok(MmioWriteAction::QueueNotified {
                    index: val as u16,
                    generation,
                });
            }
            vc::VIRTIO_MMIO_INTERRUPT_ACK => {
                let mut interrupt = self.interrupt_status.lock_irqsave();
                let raised_after_read = interrupt.raised_after_read & val;
                interrupt.pending &= !(val & !raised_after_read);
                if raised_after_read != 0 {
                    return Ok(MmioWriteAction::InterruptPending);
                }
            }
            vc::VIRTIO_MMIO_STATUS => return self.handle_status_write(val),
            reg @ (vc::VIRTIO_MMIO_QUEUE_DESC_LOW
            | vc::VIRTIO_MMIO_QUEUE_DESC_HIGH
            | vc::VIRTIO_MMIO_QUEUE_AVAIL_LOW
            | vc::VIRTIO_MMIO_QUEUE_AVAIL_HIGH
            | vc::VIRTIO_MMIO_QUEUE_USED_LOW
            | vc::VIRTIO_MMIO_QUEUE_USED_HIGH) => self.write_queue_address(reg, val),
            _ => return Err(VirtioError::InvalidRegister),
        }
        Ok(MmioWriteAction::None)
    }

    fn snapshot_queue_ready_validation(
        &self,
        index: u16,
    ) -> Option<QueueReadyValidationSnapshot<T>> {
        let generation_before = self.activity.current_generation();
        let accepting_before = self.activity.is_accepting();
        let processing = self.queue_processing.lock_irqsave();
        let queues = self.queues.lock_irqsave();
        let generation_after = self.activity.current_generation();
        let accepting_after = self.activity.is_accepting();
        if !accepting_before
            || !accepting_after
            || generation_before != generation_after
            || processing.get(index as usize).copied().unwrap_or(true)
        {
            return None;
        }
        queues
            .get(index as usize)
            .map(|queue| QueueReadyValidationSnapshot {
                token: QueueReadyValidationToken {
                    generation: generation_before,
                    configuration: QueueConfigurationSnapshot::from_queue(queue),
                },
                queue: queue.clone(),
            })
    }

    fn commit_queue_ready_validation(
        &self,
        index: u16,
        value: u32,
        validation: QueueReadyValidationToken,
        layout_ok: bool,
    ) {
        let processing = self.queue_processing.lock_irqsave();
        let mut queues = self.queues.lock_irqsave();
        if self.activity.is_accepting()
            && self.activity.current_generation() == validation.generation
            && !processing.get(index as usize).copied().unwrap_or(true)
            && let Some(queue) = queues.get_mut(index as usize)
            && QueueConfigurationSnapshot::from_queue(queue) == validation.configuration
        {
            if value != 0 && queue.is_configured() && !layout_ok {
                queue.report_layout_rejection();
            }
            queue.set_ready(value != 0 && layout_ok);
        }
    }

    /// Validate a status write. Writing 0 resets; sealing `FEATURES_OK` is
    /// rejected unless driver features are a subset of device features.
    fn handle_status_write(&self, val: u32) -> VirtioResult<MmioWriteAction> {
        if val == 0 {
            self.begin_reset()?;
            return Ok(MmioWriteAction::Reset);
        }
        let mut features_sealed = self.features_sealed.lock_irqsave();
        let features_already_ok = *features_sealed;
        let mut new_status = if features_already_ok {
            val | vc::VIRTIO_STATUS_FEATURES_OK
        } else {
            val
        };
        if !features_already_ok && (new_status & vc::VIRTIO_STATUS_FEATURES_OK) != 0 {
            let driver_feats = *self.driver_features.lock_irqsave();
            if (driver_feats & !self.device_features) != 0 {
                new_status &= !vc::VIRTIO_STATUS_FEATURES_OK;
                new_status |= vc::VIRTIO_STATUS_FAILED;
            } else {
                let event_idx_enabled = driver_feats & vc::VIRTIO_F_RING_EVENT_IDX != 0;
                for queue in self.queues.lock_irqsave().iter_mut() {
                    queue.event_idx_enabled = event_idx_enabled;
                }
                *features_sealed = true;
            }
        }
        *self.status.lock_irqsave() = new_status;
        Ok(MmioWriteAction::None)
    }

    /// Combine a 32-bit LOW/HIGH half into a queue address (overwrite semantics).
    fn write_queue_address(&self, reg: usize, val: u32) {
        let sel = *self.queue_sel.lock_irqsave();
        let processing = self.queue_processing.lock_irqsave();
        if processing.get(sel as usize).copied().unwrap_or(true) {
            return;
        }
        let mut queues = self.queues.lock_irqsave();
        let Some(q) = queues.get_mut(sel as usize) else {
            return;
        };
        match reg {
            vc::VIRTIO_MMIO_QUEUE_DESC_LOW => {
                let _ = q.set_desc_table_addr(GuestPhysAddr::from(combine_addr(
                    q.desc_table_addr.as_usize(),
                    val,
                    true,
                )));
            }
            vc::VIRTIO_MMIO_QUEUE_DESC_HIGH => {
                let _ = q.set_desc_table_addr(GuestPhysAddr::from(combine_addr(
                    q.desc_table_addr.as_usize(),
                    val,
                    false,
                )));
            }
            vc::VIRTIO_MMIO_QUEUE_AVAIL_LOW => {
                let _ = q.set_avail_ring_addr(GuestPhysAddr::from(combine_addr(
                    q.avail_ring_addr.as_usize(),
                    val,
                    true,
                )));
            }
            vc::VIRTIO_MMIO_QUEUE_AVAIL_HIGH => {
                let _ = q.set_avail_ring_addr(GuestPhysAddr::from(combine_addr(
                    q.avail_ring_addr.as_usize(),
                    val,
                    false,
                )));
            }
            vc::VIRTIO_MMIO_QUEUE_USED_LOW => {
                let _ = q.set_used_ring_addr(GuestPhysAddr::from(combine_addr(
                    q.used_ring_addr.as_usize(),
                    val,
                    true,
                )));
            }
            vc::VIRTIO_MMIO_QUEUE_USED_HIGH => {
                let _ = q.set_used_ring_addr(GuestPhysAddr::from(combine_addr(
                    q.used_ring_addr.as_usize(),
                    val,
                    false,
                )));
            }
            _ => {}
        }
    }
}

const fn is_queue_config_register(offset: usize) -> bool {
    matches!(
        offset,
        vc::VIRTIO_MMIO_QUEUE_SEL
            | vc::VIRTIO_MMIO_QUEUE_NUM
            | vc::VIRTIO_MMIO_QUEUE_READY
            | vc::VIRTIO_MMIO_QUEUE_DESC_LOW
            | vc::VIRTIO_MMIO_QUEUE_DESC_HIGH
            | vc::VIRTIO_MMIO_QUEUE_AVAIL_LOW
            | vc::VIRTIO_MMIO_QUEUE_AVAIL_HIGH
            | vc::VIRTIO_MMIO_QUEUE_USED_LOW
            | vc::VIRTIO_MMIO_QUEUE_USED_HIGH
    )
}

/// Combine a 32-bit LOW/HIGH half with the current address into a 64-bit value.
fn combine_addr(current: usize, half: u32, low: bool) -> usize {
    let cur = current as u64;
    let h = half as u64;
    let combined = if low {
        (cur & 0xffff_ffff_0000_0000) | h
    } else {
        (cur & 0x0000_0000_ffff_ffff) | (h << 32)
    };
    combined as usize
}
