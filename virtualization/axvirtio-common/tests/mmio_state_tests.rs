//! Shared MMIO transport state tests (plan section 13.2).

use std::{
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

use ax_memory_addr::PhysAddr;
use axaddrspace::GuestMemoryAccessor;
use axvirtio_common::{
    GuestMemory, MmioReadOutcome, MmioWriteAction, VirtioError, VirtioMmioState, VirtioQueue,
    constants as vc,
};
use axvm_types::{AccessWidth, GuestPhysAddr};

const BASE: usize = 0x0a00_0000;
const LEN: usize = 0x200;

/// Mock guest memory: a flat backing buffer where guest physical address `gpa`
/// maps to `buf[gpa..]` for `gpa < 0x10000`. The accessor returns real host
/// pointers, so the memory-based ring layout check and the trait defaults work
/// against the same buffer.
#[derive(Clone)]
struct Mem {
    buf: std::sync::Arc<Vec<u8>>,
}
impl GuestMemoryAccessor for Mem {
    fn translate_and_get_limit(&self, guest_addr: GuestPhysAddr) -> Option<(PhysAddr, usize)> {
        let off = guest_addr.as_usize();
        if off < self.buf.len() {
            Some((
                PhysAddr::from(self.buf.as_ptr() as usize + off),
                self.buf.len() - off,
            ))
        } else {
            None
        }
    }
}

fn mem() -> Mem {
    Mem {
        buf: std::sync::Arc::new(vec![0u8; 0x1_0000]),
    }
}

fn state(device_features: u64) -> VirtioMmioState<Mem> {
    let accessor = Arc::new(mem());
    let queue = VirtioQueue::new(0, vc::DEFAULT_QUEUE_SIZE, accessor);
    VirtioMmioState::new(
        GuestPhysAddr::from(BASE),
        LEN,
        2, // device id (block, arbitrary for these tests)
        vc::VIRTIO_VENDOR_ID,
        device_features,
        vec![queue],
    )
}

fn bounded_state(device_features: u64) -> VirtioMmioState<Mem> {
    // Same mock: backing covers `[0, 0x10000)`, so a used ring at 0xfff8
    // crosses the end of the guest address space.
    state(device_features)
}

fn bounded_rd<T: GuestMemoryAccessor + Clone>(s: &VirtioMmioState<T>, reg: usize) -> u32 {
    match s
        .mmio_read(GuestPhysAddr::from(BASE + reg), AccessWidth::Dword)
        .unwrap()
    {
        MmioReadOutcome::Standard(v) => v,
        MmioReadOutcome::DeviceConfig { .. } => panic!("expected standard register"),
    }
}
fn bounded_wr(s: &VirtioMmioState<Mem>, reg: usize, val: u32) -> MmioWriteAction {
    s.mmio_write(
        GuestPhysAddr::from(BASE + reg),
        AccessWidth::Dword,
        val as usize,
    )
    .unwrap()
}

fn rd<T: GuestMemoryAccessor + Clone>(s: &VirtioMmioState<T>, reg: usize) -> u32 {
    match s
        .mmio_read(GuestPhysAddr::from(BASE + reg), AccessWidth::Dword)
        .unwrap()
    {
        MmioReadOutcome::Standard(v) => v,
        MmioReadOutcome::DeviceConfig { .. } => panic!("expected standard register"),
    }
}
fn wr(s: &VirtioMmioState<Mem>, reg: usize, val: u32) -> MmioWriteAction {
    s.mmio_write(
        GuestPhysAddr::from(BASE + reg),
        AccessWidth::Dword,
        val as usize,
    )
    .unwrap()
}

#[test]
fn identity_registers() {
    let s = state(0);
    assert_eq!(rd(&s, vc::VIRTIO_MMIO_MAGIC_VALUE), vc::MMIO_MAGIC_VALUE);
    assert_eq!(rd(&s, vc::VIRTIO_MMIO_VERSION), vc::MMIO_VERSION);
    assert_eq!(rd(&s, vc::VIRTIO_MMIO_DEVICE_ID), 2);
    assert_eq!(rd(&s, vc::VIRTIO_MMIO_VENDOR_ID), vc::VIRTIO_VENDOR_ID);
}

#[test]
fn device_features_split_by_selector() {
    // Bit 5 (lo) and bit 33 (hi).
    let s = state((1u64 << 5) | (1u64 << 33));
    wr(&s, vc::VIRTIO_MMIO_DEVICE_FEATURES_SEL, 0);
    assert_eq!(rd(&s, vc::VIRTIO_MMIO_DEVICE_FEATURES), 1u32 << 5);
    wr(&s, vc::VIRTIO_MMIO_DEVICE_FEATURES_SEL, 1);
    assert_eq!(rd(&s, vc::VIRTIO_MMIO_DEVICE_FEATURES), 1u32 << 1);
    wr(&s, vc::VIRTIO_MMIO_DEVICE_FEATURES_SEL, 5);
    assert_eq!(rd(&s, vc::VIRTIO_MMIO_DEVICE_FEATURES), 0);
}

#[test]
fn driver_features_low_high_combine() {
    let s = state(0);
    wr(&s, vc::VIRTIO_MMIO_DRIVER_FEATURES_SEL, 0);
    wr(&s, vc::VIRTIO_MMIO_DRIVER_FEATURES, 0x0000_00ff);
    wr(&s, vc::VIRTIO_MMIO_DRIVER_FEATURES_SEL, 1);
    wr(&s, vc::VIRTIO_MMIO_DRIVER_FEATURES, 0x0000_00aa);

    wr(&s, vc::VIRTIO_MMIO_DRIVER_FEATURES_SEL, 0);
    assert_eq!(rd(&s, vc::VIRTIO_MMIO_DRIVER_FEATURES), 0xff);
    wr(&s, vc::VIRTIO_MMIO_DRIVER_FEATURES_SEL, 1);
    assert_eq!(rd(&s, vc::VIRTIO_MMIO_DRIVER_FEATURES), 0xaa);
}

#[test]
fn feature_negotiation_rejects_non_subset() {
    let s = state(1u64 << 5); // only bit 5 supported
    wr(&s, vc::VIRTIO_MMIO_DRIVER_FEATURES_SEL, 0);
    wr(&s, vc::VIRTIO_MMIO_DRIVER_FEATURES, 0xffff_ffff); // claims unsupported bits
    let action = wr(&s, vc::VIRTIO_MMIO_STATUS, vc::VIRTIO_STATUS_FEATURES_OK);
    assert_eq!(action, MmioWriteAction::None);
    let status = rd(&s, vc::VIRTIO_MMIO_STATUS);
    assert_eq!(status & vc::VIRTIO_STATUS_FEATURES_OK, 0);
    assert_ne!(status & vc::VIRTIO_STATUS_FAILED, 0);
}

#[test]
fn event_idx_is_enabled_only_after_successful_feature_negotiation() {
    let s = state(vc::VIRTIO_F_RING_EVENT_IDX);
    assert!(!s.queues_lock()[0].event_idx_enabled);

    wr(&s, vc::VIRTIO_MMIO_DRIVER_FEATURES_SEL, 0);
    wr(
        &s,
        vc::VIRTIO_MMIO_DRIVER_FEATURES,
        vc::VIRTIO_F_RING_EVENT_IDX as u32,
    );
    wr(&s, vc::VIRTIO_MMIO_STATUS, vc::VIRTIO_STATUS_FEATURES_OK);

    assert!(s.queues_lock()[0].event_idx_enabled);
}

#[test]
fn transport_reset_disables_event_idx_on_every_queue() {
    let s = state(vc::VIRTIO_F_RING_EVENT_IDX);
    wr(&s, vc::VIRTIO_MMIO_DRIVER_FEATURES_SEL, 0);
    wr(
        &s,
        vc::VIRTIO_MMIO_DRIVER_FEATURES,
        vc::VIRTIO_F_RING_EVENT_IDX as u32,
    );
    wr(&s, vc::VIRTIO_MMIO_STATUS, vc::VIRTIO_STATUS_FEATURES_OK);
    assert!(s.queues_lock()[0].event_idx_enabled);

    wr(&s, vc::VIRTIO_MMIO_STATUS, 0);

    assert!(!s.queues_lock()[0].event_idx_enabled);
}

#[test]
fn accepted_features_are_sealed_until_transport_reset() {
    let s = state(vc::VIRTIO_F_RING_EVENT_IDX);
    wr(&s, vc::VIRTIO_MMIO_DRIVER_FEATURES_SEL, 0);
    wr(
        &s,
        vc::VIRTIO_MMIO_DRIVER_FEATURES,
        vc::VIRTIO_F_RING_EVENT_IDX as u32,
    );
    wr(&s, vc::VIRTIO_MMIO_STATUS, vc::VIRTIO_STATUS_FEATURES_OK);
    assert!(s.queues_lock()[0].event_idx_enabled);

    wr(&s, vc::VIRTIO_MMIO_DRIVER_FEATURES, 0);
    wr(
        &s,
        vc::VIRTIO_MMIO_STATUS,
        vc::VIRTIO_STATUS_FEATURES_OK | vc::VIRTIO_STATUS_DRIVER_OK,
    );

    assert_eq!(s.driver_features(), vc::VIRTIO_F_RING_EVENT_IDX);
    assert!(s.queues_lock()[0].event_idx_enabled);
    assert_ne!(
        rd(&s, vc::VIRTIO_MMIO_STATUS) & vc::VIRTIO_STATUS_FEATURES_OK,
        0
    );
}

#[test]
fn queue_notify_returns_action() {
    let s = state(0);
    s.set_status(vc::VIRTIO_STATUS_DRIVER_OK);
    let generation = s.queue_generation();
    assert_eq!(
        wr(&s, vc::VIRTIO_MMIO_QUEUE_NOTIFY, 0),
        MmioWriteAction::QueueNotified {
            index: 0,
            generation,
        }
    );
}

#[test]
fn status_zero_resets() {
    let s = state(0);
    wr(&s, vc::VIRTIO_MMIO_DRIVER_FEATURES_SEL, 0);
    wr(&s, vc::VIRTIO_MMIO_DRIVER_FEATURES, 0x1234);
    assert_eq!(wr(&s, vc::VIRTIO_MMIO_STATUS, 0), MmioWriteAction::Reset);
    s.complete_reset().unwrap();
    assert_eq!(rd(&s, vc::VIRTIO_MMIO_STATUS), 0);
    wr(&s, vc::VIRTIO_MMIO_DRIVER_FEATURES_SEL, 0);
    assert_eq!(
        rd(&s, vc::VIRTIO_MMIO_DRIVER_FEATURES),
        0,
        "reset must clear driver features"
    );
}

#[test]
fn queue_processing_lease_excludes_duplicate_and_reset_state_revival() {
    let s = state(0);
    wr(&s, vc::VIRTIO_MMIO_QUEUE_SEL, 0);
    wr(&s, vc::VIRTIO_MMIO_QUEUE_NUM, 4);
    s.set_status(vc::VIRTIO_STATUS_DRIVER_OK);

    let mut lease = s
        .take_queue_for_processing(0, s.queue_generation())
        .expect("the first queue processor must acquire the queue");
    assert!(
        s.take_queue_for_processing(0, lease.generation()).is_none(),
        "a second MMIO processor must not take the same queue"
    );

    // Queue configuration writes target only the temporary placeholder while
    // processing owns the real queue; they must not mutate the owned queue.
    wr(&s, vc::VIRTIO_MMIO_QUEUE_NUM, 8);
    assert_eq!(lease.queue().expect("lease owns queue").size, 4);
    assert!(matches!(s.reset(), Err(VirtioError::WouldBlock)));
    assert_eq!(s.status(), vc::VIRTIO_STATUS_DRIVER_OK);

    lease.restore_queue();
    drop(lease);
    assert!(s.reset().is_ok());
    assert_eq!(s.status(), 0);
    assert!(
        s.take_queue_for_processing(0, s.queue_generation())
            .is_none(),
        "a reset queue must not be admitted before DRIVER_OK"
    );
    s.set_status(vc::VIRTIO_STATUS_DRIVER_OK);
    assert!(
        s.take_queue_for_processing(0, s.queue_generation())
            .is_some()
    );
}

#[test]
fn queue_reads_and_writes_are_serialized_with_processing_lease() {
    let s = state(0);
    wr(&s, vc::VIRTIO_MMIO_QUEUE_SEL, 0);
    wr(&s, vc::VIRTIO_MMIO_QUEUE_NUM, 4);
    s.set_status(vc::VIRTIO_STATUS_DRIVER_OK);

    let mut lease = s
        .take_queue_for_processing(0, s.queue_generation())
        .expect("the queue operation must acquire the queue");
    let original_size = lease.queue().expect("lease owns queue").size;
    let original_desc = lease.queue().expect("lease owns queue").desc_table_addr;

    wr(&s, vc::VIRTIO_MMIO_QUEUE_NUM, 8);
    wr(&s, vc::VIRTIO_MMIO_QUEUE_DESC_LOW, 0x1000);
    assert_eq!(rd(&s, vc::VIRTIO_MMIO_QUEUE_NUM), original_size as u32);
    assert_eq!(lease.queue().expect("lease owns queue").size, original_size);
    assert_eq!(
        lease.queue().expect("lease owns queue").desc_table_addr,
        original_desc
    );

    lease.restore_queue();
    assert!(lease.queue().is_none());
    assert!(lease.queue_mut().is_none());
}

struct ReentrantMemory {
    state: Arc<VirtioMmioState<Mem>>,
    backing: Mem,
}

impl GuestMemory for ReentrantMemory {
    fn read(
        &mut self,
        guest_addr: GuestPhysAddr,
        data: &mut [u8],
    ) -> axvirtio_common::VirtioResult<()> {
        let (done, result) = mpsc::channel();
        let state = Arc::clone(&self.state);
        thread::spawn(move || {
            let readable = state
                .mmio_read(
                    GuestPhysAddr::from(BASE + vc::VIRTIO_MMIO_QUEUE_NUM),
                    AccessWidth::Dword,
                )
                .is_ok();
            let _ = done.send(readable);
        });
        if !result
            .recv_timeout(Duration::from_millis(100))
            .unwrap_or(false)
        {
            return Err(VirtioError::MemoryError);
        }

        let offset = guest_addr.as_usize();
        let Some(end) = offset.checked_add(data.len()) else {
            return Err(VirtioError::InvalidAddress);
        };
        let Some(source) = self.backing.buf.get(offset..end) else {
            return Err(VirtioError::InvalidAddress);
        };
        data.copy_from_slice(source);
        Ok(())
    }

    fn write(
        &mut self,
        guest_addr: GuestPhysAddr,
        data: &[u8],
    ) -> axvirtio_common::VirtioResult<()> {
        let offset = guest_addr.as_usize();
        let Some(end) = offset.checked_add(data.len()) else {
            return Err(VirtioError::InvalidAddress);
        };
        let Some(destination) =
            Arc::get_mut(&mut self.backing.buf).and_then(|buf| buf.get_mut(offset..end))
        else {
            return Err(VirtioError::InvalidAddress);
        };
        destination.copy_from_slice(data);
        Ok(())
    }
}

struct BlockingMemory {
    backing: Mem,
    started: Option<mpsc::Sender<()>>,
    release: mpsc::Receiver<()>,
}

impl GuestMemory for BlockingMemory {
    fn read(
        &mut self,
        guest_addr: GuestPhysAddr,
        data: &mut [u8],
    ) -> axvirtio_common::VirtioResult<()> {
        if let Some(started) = self.started.take() {
            started.send(()).unwrap();
            self.release.recv().unwrap();
        }

        let offset = guest_addr.as_usize();
        let Some(end) = offset.checked_add(data.len()) else {
            return Err(VirtioError::InvalidAddress);
        };
        let Some(source) = self.backing.buf.get(offset..end) else {
            return Err(VirtioError::InvalidAddress);
        };
        data.copy_from_slice(source);
        Ok(())
    }

    fn write(
        &mut self,
        _guest_addr: GuestPhysAddr,
        _data: &[u8],
    ) -> axvirtio_common::VirtioResult<()> {
        Err(VirtioError::InvalidAddress)
    }
}

#[test]
fn queue_ready_validates_memory_without_holding_queue_lock() {
    let state = Arc::new(state(0));
    for (reg, val) in [
        (vc::VIRTIO_MMIO_QUEUE_SEL, 0),
        (vc::VIRTIO_MMIO_QUEUE_NUM, 4),
        (vc::VIRTIO_MMIO_QUEUE_DESC_LOW, 0x1000),
        (vc::VIRTIO_MMIO_QUEUE_AVAIL_LOW, 0x2000),
        (vc::VIRTIO_MMIO_QUEUE_USED_LOW, 0x3000),
    ] {
        wr(&state, reg, val);
    }

    let backing = mem();
    let mut memory = ReentrantMemory {
        state: Arc::clone(&state),
        backing,
    };
    state
        .mmio_write_with_memory(
            GuestPhysAddr::from(BASE + vc::VIRTIO_MMIO_QUEUE_READY),
            AccessWidth::Dword,
            1,
            &mut memory,
        )
        .unwrap();

    assert_eq!(rd(&state, vc::VIRTIO_MMIO_QUEUE_READY), 1);
}

#[test]
fn queue_ready_validation_rejects_stale_generation() {
    let state = Arc::new(state(0));
    for (reg, val) in [
        (vc::VIRTIO_MMIO_QUEUE_SEL, 0),
        (vc::VIRTIO_MMIO_QUEUE_NUM, 4),
        (vc::VIRTIO_MMIO_QUEUE_DESC_LOW, 0x1000),
        (vc::VIRTIO_MMIO_QUEUE_AVAIL_LOW, 0x2000),
        (vc::VIRTIO_MMIO_QUEUE_USED_LOW, 0x3000),
    ] {
        wr(&state, reg, val);
    }

    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let mut memory = BlockingMemory {
        backing: mem(),
        started: Some(started_tx),
        release: release_rx,
    };
    let worker_state = Arc::clone(&state);
    let worker = thread::spawn(move || {
        worker_state
            .mmio_write_with_memory(
                GuestPhysAddr::from(BASE + vc::VIRTIO_MMIO_QUEUE_READY),
                AccessWidth::Dword,
                1,
                &mut memory,
            )
            .unwrap()
    });

    started_rx
        .recv_timeout(Duration::from_millis(100))
        .expect("QUEUE_READY validation must reach guest memory");
    state.reset().unwrap();
    for (reg, val) in [
        (vc::VIRTIO_MMIO_QUEUE_SEL, 0),
        (vc::VIRTIO_MMIO_QUEUE_NUM, 4),
        (vc::VIRTIO_MMIO_QUEUE_DESC_LOW, 0x1000),
        (vc::VIRTIO_MMIO_QUEUE_AVAIL_LOW, 0x2000),
        (vc::VIRTIO_MMIO_QUEUE_USED_LOW, 0x3000),
    ] {
        wr(&state, reg, val);
    }
    release_tx.send(()).unwrap();

    assert_eq!(worker.join().unwrap(), MmioWriteAction::None);
    assert_eq!(rd(&state, vc::VIRTIO_MMIO_QUEUE_READY), 0);
}

#[test]
fn queue_activity_covers_interrupt_publication() {
    let s = state(0);
    s.set_status(vc::VIRTIO_STATUS_DRIVER_OK);
    let mut lease = s
        .take_queue_for_processing(0, s.queue_generation())
        .expect("the queue operation must acquire activity");

    // The queue can be returned before the interrupt bit is published, but
    // the lease must remain alive so reset cannot clear that publication.
    lease.restore_queue();
    s.set_interrupt(vc::VIRTIO_MMIO_INT_VRING);
    assert!(matches!(s.reset(), Err(VirtioError::WouldBlock)));
    assert_eq!(s.interrupt_status(), vc::VIRTIO_MMIO_INT_VRING);

    drop(lease);
    s.reset()
        .expect("reset should proceed after interrupt publication");
    assert_eq!(s.status(), 0);
    assert_eq!(s.interrupt_status(), 0);
}

#[test]
fn stale_mmio_generation_cannot_admit_after_reset() {
    let s = state(0);
    s.set_status(vc::VIRTIO_STATUS_DRIVER_OK);
    let old_generation = s.queue_generation();

    s.reset()
        .expect("reset should advance the queue generation");
    s.set_status(vc::VIRTIO_STATUS_DRIVER_OK);

    assert_ne!(s.queue_generation(), old_generation);
    assert!(
        s.take_queue_for_processing(0, old_generation).is_none(),
        "a pre-reset notification must not enter the new queue generation"
    );
    assert!(
        s.take_queue_for_processing(0, s.queue_generation())
            .is_some(),
        "the current queue generation remains admissible"
    );
}

#[test]
fn queue_ready_rejected_when_ring_outside_guest_address_space() {
    let s = bounded_state(0);
    bounded_wr(&s, vc::VIRTIO_MMIO_QUEUE_SEL, 0);
    bounded_wr(&s, vc::VIRTIO_MMIO_QUEUE_NUM, 4);
    bounded_wr(&s, vc::VIRTIO_MMIO_QUEUE_DESC_LOW, 0x1000);
    bounded_wr(&s, vc::VIRTIO_MMIO_QUEUE_AVAIL_LOW, 0x2000);
    bounded_wr(&s, vc::VIRTIO_MMIO_QUEUE_USED_LOW, 0xfff8); // tail beyond 0x10000
    bounded_wr(&s, vc::VIRTIO_MMIO_QUEUE_READY, 1);
    assert_eq!(
        bounded_rd(&s, vc::VIRTIO_MMIO_QUEUE_READY),
        0,
        "a ring crossing the guest address-space boundary must not become ready"
    );

    // Re-programming a fully in-space layout lets the queue become ready.
    bounded_wr(&s, vc::VIRTIO_MMIO_QUEUE_USED_LOW, 0xf000);
    bounded_wr(&s, vc::VIRTIO_MMIO_QUEUE_READY, 1);
    assert_eq!(bounded_rd(&s, vc::VIRTIO_MMIO_QUEUE_READY), 1);
}

#[test]
fn queue_ready_initializes_negotiated_event_idx_fields() {
    let memory = mem();
    let queue = VirtioQueue::new(0, vc::DEFAULT_QUEUE_SIZE, Arc::new(memory.clone()));
    let s = VirtioMmioState::new(
        GuestPhysAddr::from(BASE),
        LEN,
        2,
        vc::VIRTIO_VENDOR_ID,
        vc::VIRTIO_F_RING_EVENT_IDX,
        vec![queue],
    );
    wr(&s, vc::VIRTIO_MMIO_DRIVER_FEATURES_SEL, 0);
    wr(
        &s,
        vc::VIRTIO_MMIO_DRIVER_FEATURES,
        vc::VIRTIO_F_RING_EVENT_IDX as u32,
    );
    wr(&s, vc::VIRTIO_MMIO_STATUS, vc::VIRTIO_STATUS_FEATURES_OK);
    wr(&s, vc::VIRTIO_MMIO_QUEUE_SEL, 0);
    wr(&s, vc::VIRTIO_MMIO_QUEUE_NUM, 4);
    wr(&s, vc::VIRTIO_MMIO_QUEUE_DESC_LOW, 0x1000);
    wr(&s, vc::VIRTIO_MMIO_QUEUE_AVAIL_LOW, 0x2000);
    wr(&s, vc::VIRTIO_MMIO_QUEUE_USED_LOW, 0x3000);
    memory
        .write_buffer(GuestPhysAddr::from(0x3000), &1u16.to_le_bytes())
        .unwrap();
    memory
        .write_buffer(GuestPhysAddr::from(0x3024), &0x1234u16.to_le_bytes())
        .unwrap();

    wr(&s, vc::VIRTIO_MMIO_QUEUE_READY, 1);

    assert_eq!(rd(&s, vc::VIRTIO_MMIO_QUEUE_READY), 1);
    assert_eq!(&memory.buf[0x3000..0x3002], &0u16.to_le_bytes());
    assert_eq!(&memory.buf[0x3024..0x3026], &0u16.to_le_bytes());

    memory
        .write_buffer(GuestPhysAddr::from(0x3024), &2u16.to_le_bytes())
        .unwrap();
    wr(&s, vc::VIRTIO_MMIO_QUEUE_READY, 1);
    assert_eq!(
        &memory.buf[0x3024..0x3026],
        &2u16.to_le_bytes(),
        "a duplicate ready write must not rewind avail_event"
    );
}

#[test]
fn replacing_used_ring_starts_a_new_notification_epoch() {
    const AVAIL_BASE: usize = 0x2000;
    const FIRST_USED_BASE: usize = 0x3000;
    const REPLACEMENT_USED_BASE: usize = 0x4000;

    let memory = mem();
    let queue = VirtioQueue::new(0, vc::DEFAULT_QUEUE_SIZE, Arc::new(memory.clone()));
    let s = VirtioMmioState::new(
        GuestPhysAddr::from(BASE),
        LEN,
        2,
        vc::VIRTIO_VENDOR_ID,
        vc::VIRTIO_F_RING_EVENT_IDX,
        vec![queue],
    );
    wr(&s, vc::VIRTIO_MMIO_DRIVER_FEATURES_SEL, 0);
    wr(
        &s,
        vc::VIRTIO_MMIO_DRIVER_FEATURES,
        vc::VIRTIO_F_RING_EVENT_IDX as u32,
    );
    wr(&s, vc::VIRTIO_MMIO_STATUS, vc::VIRTIO_STATUS_FEATURES_OK);
    wr(&s, vc::VIRTIO_MMIO_QUEUE_SEL, 0);
    wr(&s, vc::VIRTIO_MMIO_QUEUE_NUM, 4);
    wr(&s, vc::VIRTIO_MMIO_QUEUE_DESC_LOW, 0x1000);
    wr(&s, vc::VIRTIO_MMIO_QUEUE_AVAIL_LOW, AVAIL_BASE as u32);
    wr(&s, vc::VIRTIO_MMIO_QUEUE_USED_LOW, FIRST_USED_BASE as u32);
    wr(&s, vc::VIRTIO_MMIO_QUEUE_READY, 1);
    memory
        .write_buffer(
            GuestPhysAddr::from(AVAIL_BASE + 4 + 4 * 2),
            &0u16.to_le_bytes(),
        )
        .unwrap();

    assert!(s.queues_lock()[0].complete(0, 8).unwrap());

    wr(&s, vc::VIRTIO_MMIO_QUEUE_READY, 0);
    wr(
        &s,
        vc::VIRTIO_MMIO_QUEUE_USED_LOW,
        REPLACEMENT_USED_BASE as u32,
    );
    wr(&s, vc::VIRTIO_MMIO_QUEUE_READY, 1);

    assert!(
        s.queues_lock()[0].complete(1, 8).unwrap(),
        "the first completion on a replacement used ring must notify for used_event 0"
    );
}

#[test]
fn queue_ready_rejected_when_ring_footer_crosses_guest_address_space() {
    let s = bounded_state(0);
    bounded_wr(&s, vc::VIRTIO_MMIO_QUEUE_SEL, 0);
    bounded_wr(&s, vc::VIRTIO_MMIO_QUEUE_NUM, 4);
    bounded_wr(&s, vc::VIRTIO_MMIO_QUEUE_DESC_LOW, 0x1000);
    bounded_wr(&s, vc::VIRTIO_MMIO_QUEUE_AVAIL_LOW, 0x2000);
    // The used ring data ends exactly at the end of the guest address space
    // (0xffdc + 4 + 4*8 == 0x10000); only the 2-byte `avail_event` footer
    // crosses it. The footer is part of the ring region, so the layout must
    // be rejected.
    bounded_wr(&s, vc::VIRTIO_MMIO_QUEUE_USED_LOW, 0xffdc);
    bounded_wr(&s, vc::VIRTIO_MMIO_QUEUE_READY, 1);
    assert_eq!(
        bounded_rd(&s, vc::VIRTIO_MMIO_QUEUE_READY),
        0,
        "a ring whose event-index footer crosses the guest address-space boundary must not become \
         ready"
    );

    // Moving the whole ring, footer included, inside the address space lets
    // the queue become ready.
    bounded_wr(&s, vc::VIRTIO_MMIO_QUEUE_USED_LOW, 0xffd8);
    bounded_wr(&s, vc::VIRTIO_MMIO_QUEUE_READY, 1);
    assert_eq!(bounded_rd(&s, vc::VIRTIO_MMIO_QUEUE_READY), 1);
}

#[test]
fn queue_ready_rejected_on_malformed_ring_layout() {
    let s = state(0);
    wr(&s, vc::VIRTIO_MMIO_QUEUE_SEL, 0);
    wr(&s, vc::VIRTIO_MMIO_QUEUE_NUM, 4);
    wr(&s, vc::VIRTIO_MMIO_QUEUE_DESC_LOW, 0x1000);
    wr(&s, vc::VIRTIO_MMIO_QUEUE_AVAIL_LOW, 0x2000);
    wr(&s, vc::VIRTIO_MMIO_QUEUE_USED_LOW, 0x3003); // not 4-byte aligned
    wr(&s, vc::VIRTIO_MMIO_QUEUE_READY, 1);
    assert_eq!(
        rd(&s, vc::VIRTIO_MMIO_QUEUE_READY),
        0,
        "ready must not become effective on a malformed layout"
    );

    // Re-programming a valid layout lets the same queue become ready.
    wr(&s, vc::VIRTIO_MMIO_QUEUE_USED_LOW, 0x3000);
    wr(&s, vc::VIRTIO_MMIO_QUEUE_READY, 1);
    assert_eq!(rd(&s, vc::VIRTIO_MMIO_QUEUE_READY), 1);

    // Writing ready=0 clears it again.
    wr(&s, vc::VIRTIO_MMIO_QUEUE_READY, 0);
    assert_eq!(rd(&s, vc::VIRTIO_MMIO_QUEUE_READY), 0);
}

/// An accessor that never translates any guest address. Mirrors the runtime
/// arrangement where real guest memory only exists as a scoped capability at
/// MMIO access time (axvisor's `NoGuestMemoryAccessor`), so the queue's own
/// accessor cannot satisfy any memory probe.
#[derive(Clone, Copy, Default)]
struct NoMem;

impl GuestMemoryAccessor for NoMem {
    fn translate_and_get_limit(&self, _guest_addr: GuestPhysAddr) -> Option<(PhysAddr, usize)> {
        None
    }
}

#[test]
fn queue_ready_uses_scoped_memory_for_layout_validation() {
    // The queue's own accessor translates nothing; only the scoped memory
    // capability passed at MMIO-write time can. The QUEUE_READY write must
    // validate the layout against that scoped capability, or real guests
    // could never make a queue ready.
    let accessor = Arc::new(NoMem);
    let queue = VirtioQueue::new(0, vc::DEFAULT_QUEUE_SIZE, accessor);
    let s = VirtioMmioState::new(
        GuestPhysAddr::from(BASE),
        LEN,
        2,
        vc::VIRTIO_VENDOR_ID,
        0,
        vec![queue],
    );

    // Program a valid in-bounds layout (desc 0x1000, avail 0x2000,
    // used 0x3000; all inside the 0x10000-byte backing below).
    for (reg, val) in [
        (vc::VIRTIO_MMIO_QUEUE_SEL, 0),
        (vc::VIRTIO_MMIO_QUEUE_NUM, 4),
        (vc::VIRTIO_MMIO_QUEUE_DESC_LOW, 0x1000),
        (vc::VIRTIO_MMIO_QUEUE_AVAIL_LOW, 0x2000),
        (vc::VIRTIO_MMIO_QUEUE_USED_LOW, 0x3000),
    ] {
        s.mmio_write(
            GuestPhysAddr::from(BASE + reg),
            AccessWidth::Dword,
            val as usize,
        )
        .unwrap();
    }

    // The accessor-based path must reject the layout: the queue's own
    // accessor cannot translate any guest address.
    s.mmio_write(
        GuestPhysAddr::from(BASE + vc::VIRTIO_MMIO_QUEUE_READY),
        AccessWidth::Dword,
        1,
    )
    .unwrap();
    assert_eq!(
        rd(&s, vc::VIRTIO_MMIO_QUEUE_READY),
        0,
        "the queue's own accessor cannot satisfy the layout probe"
    );

    // The scoped-memory path validates the same addresses against the real
    // backing and must make the queue ready.
    let backing = mem();
    let mut memory = axvirtio_common::AddressSpaceMemory::new(&backing);
    s.mmio_write_with_memory(
        GuestPhysAddr::from(BASE + vc::VIRTIO_MMIO_QUEUE_READY),
        AccessWidth::Dword,
        1,
        &mut memory,
    )
    .unwrap();
    assert_eq!(
        rd(&s, vc::VIRTIO_MMIO_QUEUE_READY),
        1,
        "the scoped-memory path must make the queue ready"
    );

    // Writing ready=0 through the scoped path clears it again.
    s.mmio_write_with_memory(
        GuestPhysAddr::from(BASE + vc::VIRTIO_MMIO_QUEUE_READY),
        AccessWidth::Dword,
        0,
        &mut memory,
    )
    .unwrap();
    assert_eq!(rd(&s, vc::VIRTIO_MMIO_QUEUE_READY), 0);
}

#[test]
fn out_of_range_read_returns_zero_not_magic() {
    // Regression: a read past the MMIO region must return 0, not be mistaken for
    // offset 0 (the magic register).
    let s = state(0);
    let out = s
        .mmio_read(GuestPhysAddr::from(BASE + LEN + 16), AccessWidth::Dword)
        .unwrap();
    assert_eq!(out, MmioReadOutcome::Standard(0));
}

#[test]
fn non_dword_standard_register_rejected() {
    let s = state(0);
    assert!(
        s.mmio_read(
            GuestPhysAddr::from(BASE + vc::VIRTIO_MMIO_MAGIC_VALUE),
            AccessWidth::Byte
        )
        .is_err()
    );
}

#[test]
fn interrupt_ack_preserves_same_bit_raised_after_status_read() {
    let s = state(0);
    s.set_interrupt(vc::VIRTIO_MMIO_INT_VRING);
    assert_eq!(
        rd(&s, vc::VIRTIO_MMIO_INTERRUPT_STATUS),
        vc::VIRTIO_MMIO_INT_VRING
    );

    // A second completion races in after the driver's status read. Its event
    // must not be consumed by the acknowledgement for the first completion.
    s.set_interrupt(vc::VIRTIO_MMIO_INT_VRING);
    assert_eq!(
        wr(&s, vc::VIRTIO_MMIO_INTERRUPT_ACK, vc::VIRTIO_MMIO_INT_VRING,),
        MmioWriteAction::InterruptPending
    );

    assert_eq!(
        s.interrupt_status(),
        vc::VIRTIO_MMIO_INT_VRING,
        "the post-read completion must remain pending"
    );

    assert_eq!(
        rd(&s, vc::VIRTIO_MMIO_INTERRUPT_STATUS),
        vc::VIRTIO_MMIO_INT_VRING
    );
    assert_eq!(
        wr(&s, vc::VIRTIO_MMIO_INTERRUPT_ACK, vc::VIRTIO_MMIO_INT_VRING,),
        MmioWriteAction::None
    );
    assert_eq!(s.interrupt_status(), 0);
}
