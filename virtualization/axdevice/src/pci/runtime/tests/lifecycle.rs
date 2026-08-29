use super::*;

#[test]
fn withdrawal_operation_reports_busy_lifecycle_without_spinning() {
    let function_id = DeviceNodeId::new("deferred-withdrawal-endpoint").unwrap();
    let mut builder = PciTopologyBuilder::new();
    builder
        .add_function(PciFunctionSpec::new(
            function_id.clone(),
            PciEndpointIdentity::new(0x1af4, 0x1042, PciClass::new(0xff, 0, 0)),
        ))
        .unwrap();
    let topology = Arc::new(builder.resolve(0xc000_0000..0xc100_0000).unwrap());
    let binding = Arc::new(PciRootBinding::new(
        DeviceNodeId::new("host").unwrap(),
        Arc::new(PciRootState::new(topology)),
    ));
    let mut grants = Vec::new();
    let lease = binding
        .bind_registered(
            &function_id,
            DeviceId::new(5),
            Arc::new(StubFunction {
                fail_command: false,
            }),
            &mut grants,
        )
        .unwrap();
    let reset = binding.begin_reset_operation().unwrap();

    drop(lease);
    assert_eq!(
        binding
            .pending_binding_withdrawals
            .lock_irqsave()
            .as_slice(),
        &[DeviceId::new(5)]
    );
    reset.finish(BindingLifecycleState::Running);
}

#[test]
fn reset_reclaims_lease_dropped_during_completion_handoff() {
    let function_id = DeviceNodeId::new("handoff-endpoint").unwrap();
    let mut builder = PciTopologyBuilder::new();
    builder
        .add_function(PciFunctionSpec::new(
            function_id.clone(),
            PciEndpointIdentity::new(0x1af4, 0x1042, PciClass::new(0xff, 0, 0)),
        ))
        .unwrap();
    let topology = Arc::new(builder.resolve(0xc000_0000..0xc100_0000).unwrap());
    let root = Arc::new(PciRootState::new(Arc::clone(&topology)));
    let binding = Arc::new(PciRootBinding::new(
        DeviceNodeId::new("host").unwrap(),
        Arc::clone(&root),
    ));
    let recording = Arc::new(RecordingFunction {
        root,
        bdf: topology.function(&function_id).unwrap().bdf(),
        reads: SpinLock::new(Vec::new()),
        writes: SpinLock::new(Vec::new()),
        commands: SpinLock::new(Vec::new()),
        resets: SpinLock::new(Vec::new()),
        reset_failures: SpinLock::new(0),
        withdrawals: SpinLock::new(0),
        withdraw_failures: SpinLock::new(0),
        irq_line: None,
        supports_effects: false,
        pending: false,
    });
    let mut grants = Vec::new();
    let lease = binding
        .bind_registered(&function_id, DeviceId::new(13), recording, &mut grants)
        .unwrap();
    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    binding.set_reset_handoff_hook({
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        Arc::new(move || {
            entered.wait();
            release.wait();
        })
    });
    let defer_entered = Arc::new(std::sync::Barrier::new(2));
    let defer_release = Arc::new(std::sync::Barrier::new(2));
    binding.set_deferred_withdrawal_hook({
        let entered = Arc::clone(&defer_entered);
        let release = Arc::clone(&defer_release);
        Arc::new(move || {
            entered.wait();
            release.wait();
        })
    });

    let reset_binding = Arc::clone(&binding);
    let reset_thread = std::thread::spawn(move || reset_binding.reset_lifecycle());
    entered.wait();
    let drop_thread = std::thread::spawn(move || drop(lease));
    defer_entered.wait();
    release.wait();
    defer_release.wait();

    assert!(reset_thread.join().unwrap().is_ok());
    drop_thread.join().unwrap();
    assert!(
        binding
            .pending_binding_withdrawals
            .lock_irqsave()
            .is_empty()
    );
    assert!(
        !binding
            .router
            .state
            .lock_irqsave()
            .endpoints
            .contains_key(&DeviceId::new(13))
    );
}

#[test]
fn lifecycle_reset_advances_only_the_admission_epoch() {
    let router = router();
    let function: Arc<dyn PciFunction> = Arc::new(StubFunction {
        fail_command: false,
    });
    let old = router
        .activate(DeviceId::new(5), function)
        .expect("test route activation succeeds");
    let old_grant = old.grant(false);
    let replacements = router.reset_admissions().unwrap();
    assert_eq!(replacements.len(), 1);
    assert!(matches!(
        old.admission.clone().acquire(&old),
        Err(DeviceError::InvalidState { .. })
    ));
    assert!(!old_grant.admission_is_open());

    router.open_admissions();
    let (_, fresh) = &replacements[0];
    assert_eq!(fresh.binding_generation(), old.binding_generation());
    assert_eq!(fresh.admission_epoch(), old.admission_epoch() + 1);
    assert!(router.endpoint(fresh).is_ok());
    assert!(fresh.admission.clone().acquire(fresh).is_ok());
    assert!(fresh.grant(false).admission_is_open());
}

#[test]
fn full_lifecycle_reset_resets_endpoint_before_reopening_admission() {
    let function_id = DeviceNodeId::new("resettable-endpoint").unwrap();
    let mut builder = PciTopologyBuilder::new();
    builder
        .add_function(PciFunctionSpec::new(
            function_id.clone(),
            PciEndpointIdentity::new(0x1af4, 0x1042, PciClass::new(0xff, 0, 0)),
        ))
        .unwrap();
    let topology = Arc::new(builder.resolve(0xc000_0000..0xc100_0000).unwrap());
    let root = Arc::new(PciRootState::new(Arc::clone(&topology)));
    let binding = Arc::new(PciRootBinding::new(
        DeviceNodeId::new("host").unwrap(),
        Arc::clone(&root),
    ));
    let sink = Arc::new(FailingDeassertSink {
        fail_deassert: AtomicBool::new(false),
        asserted: AtomicBool::new(false),
    });
    let line = WiredIrqInput::new(
        InterruptControllerId::new(0),
        ControllerInputId::new(19),
        InterruptTrigger::LevelTriggered,
        sink.clone(),
    )
    .connect()
    .unwrap();
    line.assert().unwrap();
    let recording = Arc::new(RecordingFunction {
        root,
        bdf: topology.function(&function_id).unwrap().bdf(),
        reads: SpinLock::new(Vec::new()),
        writes: SpinLock::new(Vec::new()),
        commands: SpinLock::new(Vec::new()),
        resets: SpinLock::new(Vec::new()),
        reset_failures: SpinLock::new(0),
        withdrawals: SpinLock::new(0),
        withdraw_failures: SpinLock::new(0),
        irq_line: Some(line),
        supports_effects: false,
        pending: false,
    });
    let mut grants = Vec::new();
    let lease = binding
        .bind_registered(
            &function_id,
            DeviceId::new(7),
            recording.clone(),
            &mut grants,
        )
        .unwrap();

    binding.reset_lifecycle().unwrap();

    assert_eq!(*recording.withdrawals.lock_irqsave(), 1);
    assert!(!sink.asserted.load(Ordering::Relaxed));

    let resets = recording.resets.lock_irqsave();
    assert_eq!(resets.len(), 1);
    assert!(!resets[0].bus_master_enable());
    drop(resets);
    let token = binding
        .router
        .state
        .lock_irqsave()
        .endpoints
        .get(&DeviceId::new(7))
        .unwrap()
        .token
        .clone();
    assert_eq!(token.binding_generation(), lease.token.binding_generation());
    assert_eq!(token.admission_epoch(), 2);
    assert!(token.admission.clone().acquire(&token).is_ok());
}

#[test]
fn full_lifecycle_reset_failure_keeps_endpoint_admission_closed() {
    let function_id = DeviceNodeId::new("unresettable-endpoint").unwrap();
    let mut builder = PciTopologyBuilder::new();
    builder
        .add_function(PciFunctionSpec::new(
            function_id.clone(),
            PciEndpointIdentity::new(0x1af4, 0x1042, PciClass::new(0xff, 0, 0)),
        ))
        .unwrap();
    let topology = Arc::new(builder.resolve(0xc000_0000..0xc100_0000).unwrap());
    let root = Arc::new(PciRootState::new(Arc::clone(&topology)));
    let binding = Arc::new(PciRootBinding::new(
        DeviceNodeId::new("host").unwrap(),
        Arc::clone(&root),
    ));
    let sink = Arc::new(FailingDeassertSink {
        fail_deassert: AtomicBool::new(false),
        asserted: AtomicBool::new(false),
    });
    let line = WiredIrqInput::new(
        InterruptControllerId::new(0),
        ControllerInputId::new(19),
        InterruptTrigger::LevelTriggered,
        sink.clone(),
    )
    .connect()
    .unwrap();
    line.assert().unwrap();
    let recording = Arc::new(RecordingFunction {
        root,
        bdf: topology.function(&function_id).unwrap().bdf(),
        reads: SpinLock::new(Vec::new()),
        writes: SpinLock::new(Vec::new()),
        commands: SpinLock::new(Vec::new()),
        resets: SpinLock::new(Vec::new()),
        reset_failures: SpinLock::new(1),
        withdrawals: SpinLock::new(0),
        withdraw_failures: SpinLock::new(0),
        irq_line: Some(line),
        supports_effects: false,
        pending: false,
    });
    let mut grants = Vec::new();
    let lease = binding
        .bind_registered(
            &function_id,
            DeviceId::new(7),
            recording.clone(),
            &mut grants,
        )
        .unwrap();

    assert!(matches!(
        binding.reset_lifecycle(),
        Err(DeviceManagerError::Device(DeviceError::Backend { .. }))
    ));
    assert_eq!(recording.resets.lock_irqsave().len(), 1);
    assert_eq!(*recording.withdrawals.lock_irqsave(), 1);
    assert!(!sink.asserted.load(Ordering::Relaxed));
    assert_eq!(
        *binding.lifecycle.lock_irqsave(),
        BindingLifecycleState::ResetFailed
    );
    let token = binding
        .router
        .state
        .lock_irqsave()
        .endpoints
        .get(&DeviceId::new(7))
        .unwrap()
        .token
        .clone();
    assert!(!token.grant(false).admission_is_open());
    assert!(matches!(
        token.admission.clone().acquire(&token),
        Err(DeviceError::InvalidState { .. })
    ));
    drop(lease);
}

#[test]
fn reset_irq_cleanup_failure_stays_closed_until_teardown_retries_withdrawal() {
    let function_id = DeviceNodeId::new("reset-cleanup-failure-endpoint").unwrap();
    let mut builder = PciTopologyBuilder::new();
    builder
        .add_function(PciFunctionSpec::new(
            function_id.clone(),
            PciEndpointIdentity::new(0x1af4, 0x1042, PciClass::new(0xff, 0, 0)),
        ))
        .unwrap();
    let topology = Arc::new(builder.resolve(0xc000_0000..0xc100_0000).unwrap());
    let root = Arc::new(PciRootState::new(Arc::clone(&topology)));
    let binding = Arc::new(PciRootBinding::new(
        DeviceNodeId::new("host").unwrap(),
        Arc::clone(&root),
    ));
    let sink = Arc::new(FailingDeassertSink {
        fail_deassert: AtomicBool::new(false),
        asserted: AtomicBool::new(false),
    });
    let line = WiredIrqInput::new(
        InterruptControllerId::new(0),
        ControllerInputId::new(19),
        InterruptTrigger::LevelTriggered,
        sink.clone(),
    )
    .connect()
    .unwrap();
    line.assert().unwrap();
    let recording = Arc::new(RecordingFunction {
        root,
        bdf: topology.function(&function_id).unwrap().bdf(),
        reads: SpinLock::new(Vec::new()),
        writes: SpinLock::new(Vec::new()),
        commands: SpinLock::new(Vec::new()),
        resets: SpinLock::new(Vec::new()),
        reset_failures: SpinLock::new(0),
        withdrawals: SpinLock::new(0),
        withdraw_failures: SpinLock::new(1),
        irq_line: Some(line),
        supports_effects: false,
        pending: false,
    });
    let mut grants = Vec::new();
    let lease = binding
        .bind_registered(
            &function_id,
            DeviceId::new(12),
            recording.clone(),
            &mut grants,
        )
        .unwrap();

    assert!(matches!(
        binding.reset_lifecycle(),
        Err(DeviceManagerError::Device(DeviceError::Backend { .. }))
    ));
    assert_eq!(
        *binding.lifecycle.lock_irqsave(),
        BindingLifecycleState::ResetFailed
    );
    assert!(
        !binding
            .router
            .state
            .lock_irqsave()
            .endpoints
            .get(&DeviceId::new(12))
            .unwrap()
            .token
            .grant(false)
            .admission_is_open()
    );
    assert!(sink.asserted.load(Ordering::Relaxed));

    *recording.withdraw_failures.lock_irqsave() = 0;
    drop(lease);
    assert!(!sink.asserted.load(Ordering::Relaxed));
    assert_eq!(*recording.withdrawals.lock_irqsave(), 1);
    drop(binding);
}

#[test]
fn binding_callback_can_reenter_lifecycle_without_holding_its_lock() {
    let function_id = DeviceNodeId::new("reentrant-endpoint").unwrap();
    let mut builder = PciTopologyBuilder::new();
    builder
        .add_function(PciFunctionSpec::new(
            function_id.clone(),
            PciEndpointIdentity::new(0x1af4, 0x1042, PciClass::new(0xff, 0, 0)),
        ))
        .unwrap();
    let topology = Arc::new(builder.resolve(0xc000_0000..0xc100_0000).unwrap());
    let binding = Arc::new(PciRootBinding::new(
        DeviceNodeId::new("host").unwrap(),
        Arc::new(PciRootState::new(topology)),
    ));
    let function = Arc::new(ReentrantLifecycleFunction {
        binding: Arc::downgrade(&binding),
    });
    let mut grants = Vec::new();

    let lease = binding
        .bind_registered(&function_id, DeviceId::new(7), function, &mut grants)
        .unwrap();
    drop(lease);
}

#[test]
fn endpoint_binding_waits_for_the_lifecycle_owner_gate() {
    let function_id = DeviceNodeId::new("gated-endpoint").unwrap();
    let mut builder = PciTopologyBuilder::new();
    builder
        .add_function(PciFunctionSpec::new(
            function_id.clone(),
            PciEndpointIdentity::new(0x1af4, 0x1042, PciClass::new(0xff, 0, 0)),
        ))
        .unwrap();
    let topology = Arc::new(builder.resolve(0xc000_0000..0xc100_0000).unwrap());
    let binding = Arc::new(PciRootBinding::new(
        DeviceNodeId::new("host").unwrap(),
        Arc::new(PciRootState::new(topology)),
    ));
    let gate = binding.lifecycle.lock_irqsave();
    let (sender, receiver) = std::sync::mpsc::channel();
    let bind_binding = Arc::clone(&binding);
    std::thread::spawn(move || {
        let mut grants = Vec::new();
        let result = bind_binding.bind_registered(
            &function_id,
            DeviceId::new(7),
            Arc::new(StubFunction {
                fail_command: false,
            }),
            &mut grants,
        );
        sender.send(result.is_ok()).unwrap();
    });

    assert!(receiver.try_recv().is_err());
    drop(gate);
    assert!(receiver.recv().unwrap());
}

#[test]
fn root_rejects_a_second_binding_for_the_same_function() {
    use crate::{PciClass, PciEndpointIdentity, PciFunctionSpec, PciTopologyBuilder};

    let mut builder = PciTopologyBuilder::new();
    builder
        .add_function(PciFunctionSpec::new(
            DeviceNodeId::new("endpoint").unwrap(),
            PciEndpointIdentity::new(0x1af4, 0x1110, PciClass::new(0xff, 0, 0)),
        ))
        .unwrap();
    let topology = Arc::new(builder.resolve(0xc000_0000..0xc100_0000).unwrap());
    let root = PciRootState::new(Arc::clone(&topology));
    let function_id = DeviceNodeId::new("endpoint").unwrap();

    let router = router();
    let function: Arc<dyn PciFunction> = Arc::new(StubFunction {
        fail_command: false,
    });
    let first = router
        .activate(DeviceId::new(1), Arc::clone(&function))
        .unwrap();
    root.reserve_endpoint_binding(&function_id)
        .unwrap()
        .commit(first.clone())
        .unwrap();
    assert!(matches!(
        root.reserve_endpoint_binding(&function_id),
        Err(PciError::FunctionAlreadyBound { .. })
    ));

    // Unbind invalidates the route; the same token never revives.
    drop(router.invalidate(&first));
    root.unbind_device(first.device_id());
    assert_eq!(root.resolve_bound_bar(0xc000_0000, AccessWidth::Byte), None);
    let second = router
        .activate(DeviceId::new(1), Arc::clone(&function))
        .unwrap();
    root.reserve_endpoint_binding(&function_id)
        .unwrap()
        .commit(second)
        .unwrap();
}
