use super::*;

#[test]
fn adapter_retries_completion_assert_after_irq_failure() {
    let (root, binding, bdf, _runtime, sink) = build_bound_endpoint_with_options(false);
    let function = root.topology().function(&node("virtio-pci")).unwrap();
    let bar = function.bar(PciBarIndex::new(0).unwrap()).unwrap();
    let mut context = TestEndpointContext::new();
    configure_running_endpoint(&root, &binding, bdf, bar.address(), &mut context);

    sink.fail_assert.store(true, Ordering::Relaxed);
    binding
        .write_bar_with_context(bar.address() + 0x100, AccessWidth::Word, 0, &mut context)
        .expect("completion IRQ failure is not a queue access failure");
    assert_eq!(sink.assert_calls.load(Ordering::Relaxed), 1);

    sink.fail_assert.store(false, Ordering::Relaxed);
    binding
        .write_config_with_context(
            bdf,
            ConfigOffset::new(4).unwrap(),
            AccessWidth::Word,
            0x406,
            &mut context,
        )
        .expect("disabling INTx should reconcile the failed Assert state");
    binding
        .write_config_with_context(
            bdf,
            ConfigOffset::new(4).unwrap(),
            AccessWidth::Word,
            6,
            &mut context,
        )
        .expect("reenabling INTx should retry the pending Assert");
    assert_eq!(sink.assert_calls.load(Ordering::Relaxed), 2);
}

#[test]
fn adapter_retries_fault_assert_before_status_reset() {
    let (root, binding, bdf, _runtime, sink) = build_bound_endpoint_with_options(true);
    let function = root.topology().function(&node("virtio-pci")).unwrap();
    let bar = function.bar(PciBarIndex::new(0).unwrap()).unwrap();
    let mut context = TestEndpointContext::new();
    configure_running_endpoint(&root, &binding, bdf, bar.address(), &mut context);

    sink.fail_assert.store(true, Ordering::Relaxed);
    binding
        .write_bar_with_context(bar.address() + 0x100, AccessWidth::Word, 0, &mut context)
        .expect_err("queue fault should be reported after failed Assert publication");
    assert_eq!(sink.assert_calls.load(Ordering::Relaxed), 1);
    assert_ne!(
        binding
            .read_bar_with_context(bar.address() + 0x14, AccessWidth::Byte, &mut context)
            .expect("status read should succeed")
            & axvirtio_common::VIRTIO_STATUS_DEVICE_NEEDS_RESET as u64,
        0
    );

    sink.fail_assert.store(false, Ordering::Relaxed);
    binding
        .write_bar_with_context(bar.address() + 0x14, AccessWidth::Byte, 0, &mut context)
        .expect("status reset should deassert the resynchronized line");
    assert_eq!(
        binding
            .read_bar_with_context(bar.address() + 0x14, AccessWidth::Byte, &mut context)
            .expect("reset status read should succeed"),
        0
    );
    // The failed Assert never raised the wired source, so IrqLine's
    // idempotent Deassert needs no backend call. The successful status
    // reset still consumed the coordinator's forced Deassert transition.
    assert_eq!(sink.assert_calls.load(Ordering::Relaxed), 1);
}

#[test]
fn adapter_records_command_deassert_failure_for_later_isr_retry() {
    let (root, binding, bdf, _runtime, sink) = build_bound_endpoint_with_options(false);
    let function = root.topology().function(&node("virtio-pci")).unwrap();
    let bar = function.bar(PciBarIndex::new(0).unwrap()).unwrap();
    let mut context = TestEndpointContext::new();
    configure_running_endpoint(&root, &binding, bdf, bar.address(), &mut context);
    binding
        .write_bar_with_context(bar.address() + 0x100, AccessWidth::Word, 0, &mut context)
        .expect("completion should assert the line");

    sink.fail_deassert.store(true, Ordering::Relaxed);
    binding
        .write_config_with_context(
            bdf,
            ConfigOffset::new(4).unwrap(),
            AccessWidth::Word,
            0x406,
            &mut context,
        )
        .expect_err("command transition failure should reach the adapter");
    assert_eq!(sink.deassert_calls.load(Ordering::Relaxed), 1);

    sink.fail_deassert.store(false, Ordering::Relaxed);
    assert_eq!(
        binding
            .read_bar_with_context(bar.address() + 0x200, AccessWidth::Byte, &mut context)
            .expect("ISR read should retain its value"),
        1
    );
    assert_eq!(sink.deassert_calls.load(Ordering::Relaxed), 2);
}

#[test]
fn adapter_records_isr_deassert_failure_for_command_retry() {
    let (root, binding, bdf, _runtime, sink) = build_bound_endpoint_with_options(false);
    let function = root.topology().function(&node("virtio-pci")).unwrap();
    let bar = function.bar(PciBarIndex::new(0).unwrap()).unwrap();
    let mut context = TestEndpointContext::new();
    configure_running_endpoint(&root, &binding, bdf, bar.address(), &mut context);
    binding
        .write_bar_with_context(bar.address() + 0x100, AccessWidth::Word, 0, &mut context)
        .expect("completion should assert the line");

    sink.fail_deassert.store(true, Ordering::Relaxed);
    assert_eq!(
        binding
            .read_bar_with_context(bar.address() + 0x200, AccessWidth::Byte, &mut context)
            .expect("ISR read value should not depend on line publication"),
        1
    );
    assert_eq!(sink.deassert_calls.load(Ordering::Relaxed), 1);

    sink.fail_deassert.store(false, Ordering::Relaxed);
    binding
        .write_config_with_context(
            bdf,
            ConfigOffset::new(4).unwrap(),
            AccessWidth::Word,
            0x406,
            &mut context,
        )
        .expect("command transition should retry ISR deassertion");
    assert_eq!(sink.deassert_calls.load(Ordering::Relaxed), 2);
}

#[test]
fn bound_virtio_endpoint_serializes_dispatches_and_relocates_bar() {
    let (root, binding, bdf, _runtime) = build_bound_endpoint();
    let function = root.topology().function(&node("virtio-pci")).unwrap();
    let bar = function.bar(PciBarIndex::new(0).unwrap()).unwrap();
    let pci_cfg = function.capabilities().nth(4).unwrap();
    let capability_offset = u64::from(pci_cfg.offset().value());

    assert_eq!(
        root.read_config(bdf, ConfigOffset::new(0x40).unwrap(), AccessWidth::Byte),
        Ok(9)
    );
    assert_eq!(
        root.read_config(
            bdf,
            ConfigOffset::new(capability_offset as u16 + 2).unwrap(),
            AccessWidth::Byte,
        ),
        Ok(20)
    );
    assert_eq!(
        root.read_config(
            bdf,
            ConfigOffset::new(capability_offset as u16 + 3).unwrap(),
            AccessWidth::Byte,
        ),
        Ok(5)
    );

    root.write_config(bdf, ConfigOffset::new(4).unwrap(), AccessWidth::Word, 2)
        .unwrap();
    assert_eq!(
        root.read_config(bdf, ConfigOffset::new(4).unwrap(), AccessWidth::Word),
        Ok(2)
    );
    assert!(
        root.resolve_bar(bar.address() + 0x300, AccessWidth::Dword)
            .is_some()
    );
    let direct = binding
        .read_bar(bar.address() + 0x300, AccessWidth::Dword)
        .unwrap();

    root.write_config(
        bdf,
        ConfigOffset::new(capability_offset as u16 + 4).unwrap(),
        AccessWidth::Byte,
        0,
    )
    .unwrap();
    root.write_config(
        bdf,
        ConfigOffset::new(capability_offset as u16 + 8).unwrap(),
        AccessWidth::Dword,
        0x300,
    )
    .unwrap();
    root.write_config(
        bdf,
        ConfigOffset::new(capability_offset as u16 + 12).unwrap(),
        AccessWidth::Dword,
        4,
    )
    .unwrap();
    let through_pci_cfg = binding
        .read_config(
            bdf,
            ConfigOffset::new(capability_offset as u16 + 16).unwrap(),
            AccessWidth::Dword,
        )
        .unwrap();
    assert_eq!(through_pci_cfg, direct);

    let mut context = TestEndpointContext::new();
    for (offset, width, value) in [
        (0x14, AccessWidth::Byte, 0x0f),
        (0x20, AccessWidth::Qword, 0x1000),
        (0x28, AccessWidth::Qword, 0x2000),
        (0x30, AccessWidth::Qword, 0x3000),
    ] {
        binding
            .write_bar_with_context(bar.address() + offset, width, value, &mut context)
            .unwrap();
    }
    binding
        .write_bar_with_context(bar.address() + 0x1c, AccessWidth::Word, 1, &mut context)
        .unwrap();
    binding
        .write_bar_with_context(bar.address() + 0x100, AccessWidth::Word, 0, &mut context)
        .unwrap();
    assert_eq!(context.reads.load(Ordering::Relaxed), 0);
    assert_eq!(context.writes.load(Ordering::Relaxed), 0);

    root.write_config(bdf, ConfigOffset::new(4).unwrap(), AccessWidth::Word, 6)
        .unwrap();
    assert_eq!(
        root.read_config(bdf, ConfigOffset::new(4).unwrap(), AccessWidth::Word),
        Ok(6)
    );

    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let race_context =
        TestEndpointContext::new().paused(Arc::clone(&entered), Arc::clone(&release));
    let race_binding = Arc::clone(&binding);
    let notify_address = bar.address() + 0x100;
    let race_thread = thread::spawn(move || {
        let mut race_context = race_context;
        race_binding.write_bar_with_context(notify_address, AccessWidth::Word, 0, &mut race_context)
    });
    entered.wait();
    root.write_config(bdf, ConfigOffset::new(4).unwrap(), AccessWidth::Word, 2)
        .unwrap();
    release.wait();
    race_thread
        .join()
        .expect("BME race operation should finish")
        .unwrap();

    let reads_before_stopped_notify = context.reads.load(Ordering::Relaxed);
    binding
        .write_bar_with_context(bar.address() + 0x100, AccessWidth::Word, 0, &mut context)
        .unwrap();
    assert_eq!(
        context.reads.load(Ordering::Relaxed),
        reads_before_stopped_notify
    );
    root.write_config(bdf, ConfigOffset::new(4).unwrap(), AccessWidth::Word, 6)
        .unwrap();
    binding
        .write_bar_with_context(bar.address() + 0x100, AccessWidth::Word, 0, &mut context)
        .unwrap();
    assert!(context.reads.load(Ordering::Relaxed) >= 7);
    assert!(context.writes.load(Ordering::Relaxed) >= 1);
    assert_eq!(
        binding.read_bar(bar.address() + 0x200, AccessWidth::Byte),
        Ok(1)
    );

    root.write_config(
        bdf,
        ConfigOffset::new(capability_offset as u16 + 8).unwrap(),
        AccessWidth::Dword,
        0x100,
    )
    .unwrap();
    root.write_config(
        bdf,
        ConfigOffset::new(capability_offset as u16 + 12).unwrap(),
        AccessWidth::Dword,
        2,
    )
    .unwrap();
    binding
        .write_config_with_context(
            bdf,
            ConfigOffset::new(capability_offset as u16 + 16).unwrap(),
            AccessWidth::Word,
            0,
            &mut context,
        )
        .unwrap();
    assert_eq!(
        binding.read_bar(bar.address() + 0x200, AccessWidth::Byte),
        Ok(1)
    );

    let relocated = APERTURE_BASE + 0x80000;
    root.write_config(
        bdf,
        ConfigOffset::new(0x10).unwrap(),
        AccessWidth::Dword,
        relocated,
    )
    .unwrap();
    assert_eq!(
        binding.read_bar(relocated + 0x300, AccessWidth::Dword),
        Ok(direct)
    );
    assert_eq!(
        binding.read_bar(bar.address() + 0x300, AccessWidth::Dword),
        Err(DeviceError::NotFound)
    );
}
