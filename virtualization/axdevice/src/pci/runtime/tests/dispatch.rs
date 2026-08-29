use super::*;

#[test]
fn binding_dispatches_config_effects_and_command_transitions() {
    let effect = PciCapabilityEffectRegion::new(
        PciConfigEffectId::new(7),
        8,
        6,
        PciCapabilityEffectAccess::ReadWrite,
    )
    .unwrap();
    let capability = PciCapabilitySpec::new(
        PciCapabilityId::new(9),
        alloc::vec![0, 0, 0x11, 0x22, 0x33, 0x44, 0, 0, 0, 0, 0, 0, 0, 0,],
        alloc::vec![0, 0, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0, 0, 0, 0, 0],
    )
    .unwrap()
    .with_effect(effect)
    .unwrap();
    let function_id = DeviceNodeId::new("effect-endpoint").unwrap();
    let mut builder = PciTopologyBuilder::new();
    builder
        .add_function(
            PciFunctionSpec::new(
                function_id.clone(),
                PciEndpointIdentity::new(0x1af4, 0x1041, PciClass::new(0xff, 0, 0)),
            )
            .with_capability(capability),
        )
        .unwrap();
    let topology = Arc::new(builder.resolve(0xc000_0000..0xc100_0000).unwrap());
    let bdf = topology.function(&function_id).unwrap().bdf();
    let root = Arc::new(PciRootState::new(Arc::clone(&topology)));
    let binding = Arc::new(PciRootBinding::new(
        DeviceNodeId::new("host").unwrap(),
        Arc::clone(&root),
    ));
    let recording = Arc::new(RecordingFunction {
        root,
        bdf,
        reads: SpinLock::new(Vec::new()),
        writes: SpinLock::new(Vec::new()),
        commands: SpinLock::new(Vec::new()),
        resets: SpinLock::new(Vec::new()),
        reset_failures: SpinLock::new(0),
        withdrawals: SpinLock::new(0),
        withdraw_failures: SpinLock::new(0),
        irq_line: None,
        supports_effects: true,
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
    let capability_offset = topology
        .function(&function_id)
        .unwrap()
        .capabilities()
        .next()
        .unwrap()
        .offset()
        .value();

    // Selector bytes are ordinary root-owned storage. The effect must
    // observe their value captured by the same transaction.
    binding
        .write_config(
            bdf,
            ConfigOffset::new(capability_offset + 4).unwrap(),
            AccessWidth::Dword,
            0x6655_4433,
        )
        .unwrap();
    assert!(recording.reads.lock_irqsave().is_empty());
    assert!(recording.writes.lock_irqsave().is_empty());

    assert_eq!(
        binding
            .read_config(
                bdf,
                ConfigOffset::new(capability_offset + 8).unwrap(),
                AccessWidth::Dword,
            )
            .unwrap(),
        0x5a
    );
    let read = recording.reads.lock_irqsave().pop().unwrap();
    assert_eq!(read.0.capability(), PciCapabilityId::new(9));
    assert_eq!(read.0.effect(), PciConfigEffectId::new(7));
    assert_eq!(read.0.offset(), 8);
    assert_eq!(read.0.width(), AccessWidth::Dword);
    assert_eq!(read.1, DeviceId::new(7));
    assert_eq!(read.2, 0x1041_1af4);
    assert_eq!(
        &read.0.capability_snapshot().bytes()[..8],
        &[0, 0, 0x33, 0x44, 0x55, 0x66, 0, 0]
    );

    binding
        .write_config(
            bdf,
            ConfigOffset::new(capability_offset + 8).unwrap(),
            AccessWidth::Dword,
            0xfeed_beef,
        )
        .unwrap();
    let write = recording.writes.lock_irqsave().pop().unwrap();
    assert_eq!(write.0.value(), 0xfeed_beef);
    assert_eq!(write.1, DeviceId::new(7));
    assert_eq!(
        &write.0.capability_snapshot().bytes()[..8],
        &[0, 0, 0x33, 0x44, 0x55, 0x66, 0, 0]
    );

    // Effect results are not copied into root config storage: the next
    // read reaches the endpoint again and returns its fresh result.
    assert_eq!(
        binding
            .read_config(
                bdf,
                ConfigOffset::new(capability_offset + 8).unwrap(),
                AccessWidth::Dword,
            )
            .unwrap(),
        0x5a
    );
    let second_read = recording.reads.lock_irqsave().pop().unwrap();
    assert_eq!(second_read.0.effect(), PciConfigEffectId::new(7));
    assert_eq!(second_read.1, DeviceId::new(7));

    binding
        .write_config(
            bdf,
            ConfigOffset::new(4).unwrap(),
            AccessWidth::Word,
            0x0406,
        )
        .unwrap();
    let command = recording.commands.lock_irqsave().pop().unwrap();
    assert!(command.0.memory_space_enable());
    assert!(command.0.bus_master_enable());
    assert!(command.0.interrupt_disable());
    assert_eq!(command.1, DeviceId::new(7));

    assert!(matches!(
        binding.read_config(
            bdf,
            ConfigOffset::new(capability_offset + 12).unwrap(),
            AccessWidth::Dword,
        ),
        Err(DeviceError::InvalidInput { .. })
    ));
    assert!(recording.reads.lock_irqsave().is_empty());

    drop(lease);
    assert!(matches!(
        binding.read_config(
            bdf,
            ConfigOffset::new(capability_offset + 8).unwrap(),
            AccessWidth::Dword,
        ),
        Err(DeviceError::InvalidInput { .. })
    ));
}

#[test]
fn dynamic_interrupt_status_is_read_from_the_bound_endpoint() {
    let function_id = DeviceNodeId::new("intx-endpoint").unwrap();
    let mut builder = PciTopologyBuilder::new();
    builder
        .add_function(
            PciFunctionSpec::new(
                function_id.clone(),
                PciEndpointIdentity::new(0x1af4, 0x1041, PciClass::new(0xff, 0, 0)),
            )
            .with_intx(crate::PciIntxRequirement::new(
                crate::PciIntxPin::A,
                crate::ResourceSlot::new("intx").unwrap(),
            ))
            .unwrap(),
        )
        .unwrap();
    let route = crate::PciIntxRouter::new(
        InterruptControllerId::new(0),
        [
            ControllerInputId::new(16),
            ControllerInputId::new(17),
            ControllerInputId::new(18),
            ControllerInputId::new(19),
        ],
        [16, 17, 18, 19],
        InterruptTrigger::LevelTriggered,
        InterruptSharing::Shared,
    )
    .resolve(&function_id, PciBdf::bus_zero(0), crate::PciIntxPin::A)
    .unwrap();
    builder.set_intx_route(&function_id, route).unwrap();
    let topology = Arc::new(builder.resolve(0xc000_0000..0xc100_0000).unwrap());
    let bdf = topology.function(&function_id).unwrap().bdf();
    assert!(topology.function(&function_id).unwrap().intx().is_some());
    let root = Arc::new(PciRootState::new(Arc::clone(&topology)));
    let binding = Arc::new(PciRootBinding::new(
        DeviceNodeId::new("host").unwrap(),
        root,
    ));
    let recording = Arc::new(RecordingFunction {
        root: Arc::clone(&binding.root),
        bdf,
        reads: SpinLock::new(Vec::new()),
        writes: SpinLock::new(Vec::new()),
        commands: SpinLock::new(Vec::new()),
        resets: SpinLock::new(Vec::new()),
        reset_failures: SpinLock::new(0),
        withdrawals: SpinLock::new(0),
        withdraw_failures: SpinLock::new(0),
        irq_line: None,
        supports_effects: false,
        pending: true,
    });
    let mut grants = Vec::new();
    let lease = binding
        .bind_registered(
            &function_id,
            DeviceId::new(9),
            recording.clone(),
            &mut grants,
        )
        .unwrap();

    assert_eq!(
        binding
            .read_config(bdf, ConfigOffset::new(0x06).unwrap(), AccessWidth::Byte)
            .unwrap()
            & 0x08,
        0x08
    );
    drop(lease);
    // Teardown invokes endpoint-owned final IRQ withdrawal after the
    // binding admission has been closed and drained.
    assert_eq!(*recording.withdrawals.lock_irqsave(), 1);
    assert_eq!(
        binding
            .read_config(bdf, ConfigOffset::new(0x06).unwrap(), AccessWidth::Byte)
            .unwrap()
            & 0x08,
        0
    );
}
