use core::cell::Cell;
use std::{
    sync::{Arc as StdArc, Barrier, mpsc},
    thread,
};

use axdevice_base::{AccessWidth, DeviceError};

use super::{super::*, fixtures::*};
use crate::VIRTIO_STATUS_DEVICE_NEEDS_RESET;

#[test]
fn queue_notify_probes_the_programmed_ring() {
    let transport = VirtioPciTransport::try_new(TestCore).expect("valid test transport");
    let mut memory = TestMemory {
        reads: Cell::new(0),
    };

    write(
        &transport,
        DEVICE_STATUS,
        AccessWidth::Byte,
        0x0b,
        &mut memory,
    );
    write(
        &transport,
        DEVICE_STATUS,
        AccessWidth::Byte,
        0x0f,
        &mut memory,
    );
    write(
        &transport,
        QUEUE_DESC,
        AccessWidth::Qword,
        0x1000,
        &mut memory,
    );
    write(
        &transport,
        QUEUE_DRIVER,
        AccessWidth::Qword,
        0x2000,
        &mut memory,
    );
    write(
        &transport,
        QUEUE_DEVICE,
        AccessWidth::Qword,
        0x3000,
        &mut memory,
    );
    write(&transport, QUEUE_ENABLE, AccessWidth::Word, 1, &mut memory);
    let outcome = transport
        .write_mmio_with_dma(
            NOTIFY_CONFIG_OFFSET,
            AccessWidth::Word,
            0,
            true,
            &mut memory,
        )
        .expect("queue notify should succeed");

    let VirtioPciWriteOutcome::QueueNotified(notification) = outcome else {
        panic!("expected queue notification");
    };
    assert_eq!(notification.outcome(), QueueNotifyOutcome::Idle);
    notification.complete();
    assert_eq!(memory.reads.get(), 6);
}

#[test]
fn admitted_queue_fault_sets_needs_reset_and_config_isr_once() {
    let transport = VirtioPciTransport::try_new(TestCore).expect("valid test transport");
    let mut configuration_memory = TestMemory {
        reads: Cell::new(0),
    };
    write(
        &transport,
        DEVICE_STATUS,
        AccessWidth::Byte,
        0x0b,
        &mut configuration_memory,
    );
    write(
        &transport,
        DEVICE_STATUS,
        AccessWidth::Byte,
        0x0f,
        &mut configuration_memory,
    );
    write(
        &transport,
        QUEUE_DESC,
        AccessWidth::Qword,
        0x1000,
        &mut configuration_memory,
    );
    write(
        &transport,
        QUEUE_DRIVER,
        AccessWidth::Qword,
        0x2000,
        &mut configuration_memory,
    );
    write(
        &transport,
        QUEUE_DEVICE,
        AccessWidth::Qword,
        0x3000,
        &mut configuration_memory,
    );
    write(
        &transport,
        QUEUE_ENABLE,
        AccessWidth::Word,
        1,
        &mut configuration_memory,
    );

    let mut failing_memory = FailingMemory;
    let outcome = transport
        .write_mmio_with_dma(
            NOTIFY_CONFIG_OFFSET,
            AccessWidth::Word,
            0,
            true,
            &mut failing_memory,
        )
        .expect("queue faults are reported as a transport outcome");
    let VirtioPciWriteOutcome::Fault { interrupt, .. } = outcome else {
        panic!("expected a queue fault outcome");
    };
    assert_eq!(interrupt, InterruptTransition::Assert);
    assert_ne!(
        transport.status() & VIRTIO_STATUS_DEVICE_NEEDS_RESET as u8,
        0
    );
    transport.complete_interrupt_transition(interrupt, true);
    let (value, request) = transport
        .read_bar_with_interrupt(ISR_CONFIG_OFFSET, AccessWidth::Byte)
        .expect("ISR read should succeed");
    assert_eq!(value, 2);
    transport.complete_interrupt_transition(request.transition(), true);
    drop(request);
}

#[test]
fn queue_fault_activity_blocks_reset_until_terminal_publication() {
    let transport = VirtioPciTransport::try_new(TestCore).expect("valid test transport");
    let mut configuration_memory = TestMemory {
        reads: Cell::new(0),
    };
    write(
        &transport,
        DEVICE_STATUS,
        AccessWidth::Byte,
        0x0b,
        &mut configuration_memory,
    );
    write(
        &transport,
        DEVICE_STATUS,
        AccessWidth::Byte,
        0x0f,
        &mut configuration_memory,
    );
    write(
        &transport,
        QUEUE_DESC,
        AccessWidth::Qword,
        0x1000,
        &mut configuration_memory,
    );
    write(
        &transport,
        QUEUE_DRIVER,
        AccessWidth::Qword,
        0x2000,
        &mut configuration_memory,
    );
    write(
        &transport,
        QUEUE_DEVICE,
        AccessWidth::Qword,
        0x3000,
        &mut configuration_memory,
    );
    write(
        &transport,
        QUEUE_ENABLE,
        AccessWidth::Word,
        1,
        &mut configuration_memory,
    );

    let transport = StdArc::new(transport);
    let fault_transport = StdArc::clone(&transport);
    let (sender, receiver) = mpsc::channel();
    let fault_thread = thread::spawn(move || {
        let mut failing_memory = FailingMemory;
        let outcome = fault_transport
            .write_mmio_with_dma(
                NOTIFY_CONFIG_OFFSET,
                AccessWidth::Word,
                0,
                true,
                &mut failing_memory,
            )
            .expect("queue faults are returned as an outcome");
        sender
            .send(outcome)
            .expect("queue fault should be delivered");
    });
    let outcome = receiver
        .recv()
        .expect("queue fault should be available before reset");
    fault_thread.join().expect("fault thread should finish");
    let VirtioPciWriteOutcome::Fault { interrupt, .. } = outcome else {
        panic!("expected a queue fault");
    };

    let reset_transport = StdArc::clone(&transport);
    let reset_thread = thread::spawn(move || reset_transport.reset());
    assert!(matches!(
        reset_thread.join().expect("reset thread should finish"),
        Err(DeviceError::InvalidState { .. })
    ));
    transport.complete_interrupt_transition(interrupt, true);
    drop(outcome);

    let reset_transition = transport
        .reset()
        .expect("reset should proceed after fault publication");
    transport.complete_interrupt_transition(reset_transition, true);
    transport.complete_reset();
    assert_eq!(transport.status(), 0);
}

#[test]
fn concurrent_notify_same_queue_does_not_fault_or_replace_owner_queue() {
    let entered = StdArc::new(Barrier::new(2));
    let release = StdArc::new(Barrier::new(2));
    let transport = StdArc::new(
        VirtioPciTransport::try_new(BlockingNotifyCore {
            entered: StdArc::clone(&entered),
            release: StdArc::clone(&release),
        })
        .expect("valid test transport"),
    );
    let mut configuration_memory = TestMemory {
        reads: Cell::new(0),
    };
    write(
        &transport,
        DEVICE_STATUS,
        AccessWidth::Byte,
        0x0b,
        &mut configuration_memory,
    );
    write(
        &transport,
        DEVICE_STATUS,
        AccessWidth::Byte,
        0x0f,
        &mut configuration_memory,
    );
    write(
        &transport,
        QUEUE_DESC,
        AccessWidth::Qword,
        0x1000,
        &mut configuration_memory,
    );
    write(
        &transport,
        QUEUE_DRIVER,
        AccessWidth::Qword,
        0x2000,
        &mut configuration_memory,
    );
    write(
        &transport,
        QUEUE_DEVICE,
        AccessWidth::Qword,
        0x3000,
        &mut configuration_memory,
    );
    write(
        &transport,
        QUEUE_ENABLE,
        AccessWidth::Word,
        1,
        &mut configuration_memory,
    );

    let first_transport = StdArc::clone(&transport);
    let (sender, receiver) = mpsc::channel();
    let first = thread::spawn(move || {
        let mut memory = TestMemory {
            reads: Cell::new(0),
        };
        let result = first_transport.write_mmio_with_dma(
            NOTIFY_CONFIG_OFFSET,
            AccessWidth::Word,
            0,
            true,
            &mut memory,
        );
        sender
            .send(result)
            .expect("first notify result should be delivered");
    });
    entered.wait();

    let second_transport = StdArc::clone(&transport);
    let second = thread::spawn(move || {
        let mut memory = TestMemory {
            reads: Cell::new(0),
        };
        second_transport.write_mmio_with_dma(
            NOTIFY_CONFIG_OFFSET,
            AccessWidth::Word,
            0,
            true,
            &mut memory,
        )
    });
    let second_result = second.join().expect("second notify should finish");
    let second_outcome = second_result.expect("second notify should not fail");
    let VirtioPciWriteOutcome::QueueNotified(notification) = second_outcome else {
        panic!("expected an idle queue notification");
    };
    assert_eq!(notification.outcome(), QueueNotifyOutcome::Idle);
    notification.complete();
    assert_eq!(
        transport.status() & VIRTIO_STATUS_DEVICE_NEEDS_RESET as u8,
        0
    );

    release.wait();
    let first_outcome = receiver
        .recv()
        .expect("first notify result should be available")
        .expect("first notify should succeed");
    let VirtioPciWriteOutcome::QueueNotified(notification) = first_outcome else {
        panic!("expected first queue notification");
    };
    notification.complete();
    first.join().expect("first notify should finish");
}
