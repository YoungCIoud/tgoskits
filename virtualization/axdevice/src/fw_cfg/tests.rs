use core::cell::Cell;

use super::*;

#[test]
fn dma_read_uses_bounded_chunks_for_large_guest_length() {
    let writes = Cell::new(0usize);
    dma_read_entry(
        b"abc",
        0,
        FW_CFG_DMA_SCRATCH_SIZE * 2 + 17,
        GuestPhysAddr::from_usize(0x8000),
        &mut |_addr, buffer| {
            assert!(buffer.len() <= FW_CFG_DMA_SCRATCH_SIZE);
            writes.set(writes.get() + 1);
            Ok(())
        },
    )
    .unwrap();

    assert!(writes.get() > 1);
}

#[test]
fn dma_write_discard_uses_bounded_chunks_for_large_guest_length() {
    let reads = Cell::new(0usize);
    dma_discard_guest_write(
        FW_CFG_DMA_SCRATCH_SIZE * 2 + 17,
        GuestPhysAddr::from_usize(0x8000),
        &mut |_addr, buffer| {
            assert!(buffer.len() <= FW_CFG_DMA_SCRATCH_SIZE);
            buffer.fill(0xaa);
            reads.set(reads.get() + 1);
            Ok(())
        },
    )
    .unwrap();

    assert!(reads.get() > 1);
}

#[test]
fn dma_rejects_buffer_address_overflow() {
    assert!(validate_dma_buffer(GuestPhysAddr::from_usize(usize::MAX), 2).is_err());
}

#[test]
fn dma_descriptor_fault_is_propagated_when_status_cannot_be_written() {
    let fw_cfg = FwCfg::new(
        GuestPhysAddr::from_usize(0),
        0x20,
        FwCfgKernelPayload::unsplit(Arc::from(&b"kernel"[..])),
        None,
        None,
        1,
        FwCfgPlatformConfig::default(),
    );
    let status_writes = Cell::new(0usize);

    let error = fw_cfg
        .process_dma(
            GuestPhysAddr::from_usize(0x1ff1_48b0),
            |_addr, _buffer| {
                Err(DeviceManagerError::InvalidInput {
                    operation: "read test guest memory",
                    detail: "descriptor is not mapped".into(),
                })
            },
            |_addr, status| {
                assert_eq!(status, FW_CFG_DMA_CTL_ERROR.to_be_bytes());
                status_writes.set(status_writes.get() + 1);
                Err(DeviceManagerError::InvalidInput {
                    operation: "write test guest memory",
                    detail: "descriptor is not mapped".into(),
                })
            },
        )
        .unwrap_err();

    assert!(matches!(
        error,
        DeviceManagerError::InvalidInput {
            operation: "read test guest memory",
            ..
        }
    ));
    assert_eq!(status_writes.get(), 1);
}

#[cfg(target_pointer_width = "64")]
#[test]
fn dma_address_register_preserves_high_half_and_resets_after_commit() {
    let fw_cfg = FwCfg::new(
        GuestPhysAddr::from_usize(0),
        0x20,
        FwCfgKernelPayload::unsplit(Arc::from(&b"kernel"[..])),
        None,
        None,
        1,
        FwCfgPlatformConfig::default(),
    );

    assert_eq!(
        fw_cfg
            .write_dma_port(0, AccessWidth::Dword, 0xdead_beefu32.swap_bytes() as usize)
            .unwrap(),
        None
    );
    assert_eq!(
        fw_cfg
            .write_dma_port(4, AccessWidth::Dword, 0x8000u32.swap_bytes() as usize)
            .unwrap(),
        Some(GuestPhysAddr::from_usize(0xdead_beef_0000_8000))
    );
    assert_eq!(
        fw_cfg
            .write_dma_port(4, AccessWidth::Dword, 0x80u32.swap_bytes() as usize)
            .unwrap(),
        Some(GuestPhysAddr::from_usize(0x80))
    );
}

#[cfg(feature = "host-test")]
struct TestGuestMemory {
    bytes: Vec<u8>,
}

#[cfg(feature = "host-test")]
impl GuestMemoryAccess for TestGuestMemory {
    fn read(&mut self, addr: GuestPhysAddr, data: &mut [u8]) -> DeviceResult {
        let start = addr.as_usize();
        let end = start
            .checked_add(data.len())
            .filter(|end| *end <= self.bytes.len())
            .ok_or(axdevice_base::DeviceError::OutOfRange { addr: start as u64 })?;
        data.copy_from_slice(&self.bytes[start..end]);
        Ok(())
    }

    fn write(&mut self, addr: GuestPhysAddr, data: &[u8]) -> DeviceResult {
        let start = addr.as_usize();
        let end = start
            .checked_add(data.len())
            .filter(|end| *end <= self.bytes.len())
            .ok_or(axdevice_base::DeviceError::OutOfRange { addr: start as u64 })?;
        self.bytes[start..end].copy_from_slice(data);
        Ok(())
    }
}

#[cfg(feature = "host-test")]
fn guest_access(bus: BusKind, address: u64, width: AccessWidth) -> DeviceAccess {
    DeviceAccess::new(DeviceVcpuId::new(0), bus, address, width)
}

#[cfg(feature = "host-test")]
fn read(runtime: &crate::DeviceRuntime, access: DeviceAccess) -> u64 {
    runtime.try_read(&access).unwrap().unwrap()
}

#[cfg(feature = "host-test")]
fn write(
    runtime: &crate::DeviceRuntime,
    access: DeviceAccess,
    value: u64,
    memory: Option<&mut dyn GuestMemoryAccess>,
) {
    assert!(runtime.try_write(&access, value, memory).unwrap());
}

#[cfg(feature = "host-test")]
#[test]
fn dma_descriptor_uses_the_runtime_granted_memory_port() {
    const BASE: usize = 0x1000;
    const DESCRIPTOR: usize = 0x80;
    const BUFFER: usize = 0x100;
    let bundle = FwCfgDeviceFactory::new()
        .build(FwCfgBuildConfig {
            base: GuestPhysAddr::from_usize(BASE),
            size: 0x20,
            kernel: FwCfgKernelPayload::unsplit(Arc::from(&b"kernel"[..])),
            initrd: None,
            cmdline: None,
            cpu_num: 1,
            platform: FwCfgPlatformConfig::default(),
        })
        .unwrap();
    let mut runtime = crate::DeviceRuntime::empty();
    runtime.register_bundle(bundle).unwrap();
    let mut memory = TestGuestMemory {
        bytes: alloc::alloc::vec![0; 0x200],
    };
    let control = FW_CFG_DMA_CTL_SELECT | FW_CFG_DMA_CTL_READ;
    memory.bytes[DESCRIPTOR..DESCRIPTOR + 4].copy_from_slice(&control.to_be_bytes());
    memory.bytes[DESCRIPTOR + 4..DESCRIPTOR + 8].copy_from_slice(&4u32.to_be_bytes());
    memory.bytes[DESCRIPTOR + 8..DESCRIPTOR + 16].copy_from_slice(&(BUFFER as u64).to_be_bytes());

    write(
        &runtime,
        guest_access(
            BusKind::Mmio,
            (BASE + FW_CFG_DMA_OFFSET) as u64,
            AccessWidth::Qword,
        ),
        (DESCRIPTOR as u64).swap_bytes(),
        Some(&mut memory),
    );

    assert_eq!(&memory.bytes[BUFFER..BUFFER + 4], b"QEMU");
    assert_eq!(&memory.bytes[DESCRIPTOR..DESCRIPTOR + 4], &[0, 0, 0, 0]);
}

#[cfg(feature = "host-test")]
#[test]
fn pio_selector_data_and_dma_share_one_fw_cfg_state() {
    const DESCRIPTOR: usize = 0x80;
    const BUFFER: usize = 0x100;
    let bundle = FwCfgDeviceFactory::new()
        .build_pio(
            0x510,
            2,
            0x514,
            8,
            FwCfgBuildConfig {
                base: GuestPhysAddr::from_usize(0x510),
                size: 0x0c,
                kernel: FwCfgKernelPayload::split(
                    Arc::from(&b"setup"[..]),
                    Arc::from(&b"kernel"[..]),
                ),
                initrd: None,
                cmdline: None,
                cpu_num: 1,
                platform: FwCfgPlatformConfig::default(),
            },
        )
        .unwrap();
    let mut runtime = crate::DeviceRuntime::empty();
    runtime.register_bundle(bundle).unwrap();

    write(
        &runtime,
        guest_access(BusKind::Port, 0x510, AccessWidth::Word),
        FW_CFG_SIGNATURE.swap_bytes() as u64,
        None,
    );
    let signature = (0..4)
        .map(|_| {
            read(
                &runtime,
                guest_access(BusKind::Port, 0x511, AccessWidth::Byte),
            ) as u8
        })
        .collect::<Vec<_>>();
    assert_eq!(signature, b"QEMU");

    write(
        &runtime,
        guest_access(BusKind::Port, 0x510, AccessWidth::Word),
        FW_CFG_ID as u64,
        None,
    );
    let version = (0..4).fold(0u64, |version, shift| {
        version
            | read(
                &runtime,
                guest_access(BusKind::Port, 0x511, AccessWidth::Byte),
            ) << (shift * 8)
    });
    assert_eq!(version, (FW_CFG_VERSION | FW_CFG_VERSION_DMA) as u64);

    write(
        &runtime,
        guest_access(BusKind::Port, 0x510, AccessWidth::Word),
        FW_CFG_KERNEL_SETUP_SIZE as u64,
        None,
    );
    let setup_size = (0..4).fold(0u64, |size, shift| {
        size | read(
            &runtime,
            guest_access(BusKind::Port, 0x511, AccessWidth::Byte),
        ) << (shift * 8)
    });
    assert_eq!(setup_size, b"setup".len() as u64);

    write(
        &runtime,
        guest_access(BusKind::Port, 0x510, AccessWidth::Word),
        FW_CFG_KERNEL_SETUP_DATA as u64,
        None,
    );
    let setup = (0..setup_size)
        .map(|_| {
            read(
                &runtime,
                guest_access(BusKind::Port, 0x511, AccessWidth::Byte),
            ) as u8
        })
        .collect::<Vec<_>>();
    assert_eq!(setup, b"setup");

    let mut memory = TestGuestMemory {
        bytes: alloc::alloc::vec![0; 0x200],
    };
    let control = FW_CFG_DMA_CTL_SELECT | FW_CFG_DMA_CTL_READ;
    memory.bytes[DESCRIPTOR..DESCRIPTOR + 4].copy_from_slice(&control.to_be_bytes());
    memory.bytes[DESCRIPTOR + 4..DESCRIPTOR + 8].copy_from_slice(&4u32.to_be_bytes());
    memory.bytes[DESCRIPTOR + 8..DESCRIPTOR + 16].copy_from_slice(&(BUFFER as u64).to_be_bytes());
    write(
        &runtime,
        guest_access(BusKind::Port, 0x514, AccessWidth::Qword),
        (DESCRIPTOR as u64).swap_bytes(),
        Some(&mut memory),
    );
    assert_eq!(&memory.bytes[BUFFER..BUFFER + 4], b"QEMU");
}

#[cfg(feature = "host-test")]
#[test]
fn pio_dma_fault_does_not_poison_the_next_32_bit_transfer() {
    const DESCRIPTOR: usize = 0x80;
    const BUFFER: usize = 0x100;
    let bundle = FwCfgDeviceFactory::new()
        .build_pio(
            0x510,
            2,
            0x514,
            8,
            FwCfgBuildConfig {
                base: GuestPhysAddr::from_usize(0x510),
                size: 0x0c,
                kernel: FwCfgKernelPayload::unsplit(Arc::from(&b"kernel"[..])),
                initrd: None,
                cmdline: None,
                cpu_num: 1,
                platform: FwCfgPlatformConfig::default(),
            },
        )
        .unwrap();
    let mut runtime = crate::DeviceRuntime::empty();
    runtime.register_bundle(bundle).unwrap();
    let mut memory = TestGuestMemory {
        bytes: alloc::alloc::vec![0; 0x200],
    };

    write(
        &runtime,
        guest_access(BusKind::Port, 0x514, AccessWidth::Dword),
        0xdead_beefu32.swap_bytes() as u64,
        Some(&mut memory),
    );
    let error = runtime
        .try_write(
            &guest_access(BusKind::Port, 0x518, AccessWidth::Dword),
            0x8000u32.swap_bytes() as u64,
            Some(&mut memory),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        DeviceManagerError::Access {
            source: axdevice_base::DeviceError::OutOfRange { .. },
            ..
        }
    ));

    let control = FW_CFG_DMA_CTL_SELECT | FW_CFG_DMA_CTL_READ;
    memory.bytes[DESCRIPTOR..DESCRIPTOR + 4].copy_from_slice(&control.to_be_bytes());
    memory.bytes[DESCRIPTOR + 4..DESCRIPTOR + 8].copy_from_slice(&4u32.to_be_bytes());
    memory.bytes[DESCRIPTOR + 8..DESCRIPTOR + 16].copy_from_slice(&(BUFFER as u64).to_be_bytes());
    write(
        &runtime,
        guest_access(BusKind::Port, 0x518, AccessWidth::Dword),
        (DESCRIPTOR as u32).swap_bytes() as u64,
        Some(&mut memory),
    );

    assert_eq!(&memory.bytes[BUFFER..BUFFER + 4], b"QEMU");
}

#[test]
fn e820_records_pack_little_endian_with_twenty_byte_stride() {
    let ram = [FwCfgRamRegion {
        base: 0x0,
        size: 0xa_0000,
    }];
    let non_ram = [FwCfgNonRamRegion {
        base: 0xa_0000,
        size: 0x6_0000,
        memory_type: E820NonRamType::Reserved,
    }];

    let payload = build_e820_map(&ram, &non_ram);

    // Byte-exact first record pins field order, little-endian packing, and the
    // 20-byte stride consumers derive entry counts from (no padding bytes).
    let mut expected = Vec::new();
    expected.extend_from_slice(&[
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // addr = 0
        0x00, 0x00, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00, // size = 0xa0000
        0x01, 0x00, 0x00, 0x00, // type = usable RAM
    ]);
    expected.extend_from_slice(&0xa_0000u64.to_le_bytes());
    expected.extend_from_slice(&0x6_0000u64.to_le_bytes());
    expected.extend_from_slice(&2u32.to_le_bytes());

    assert_eq!(payload.len(), 2 * 20);
    assert_eq!(payload, expected);
}

#[test]
fn e820_orders_entries_by_base_across_ram_and_reserved_windows() {
    let ram = [
        FwCfgRamRegion {
            base: 0x8000_0000,
            size: 0x1000,
        },
        FwCfgRamRegion {
            base: 0x0,
            size: 0xa_0000,
        },
    ];
    let non_ram = [
        FwCfgNonRamRegion {
            base: 0xc000_0000,
            size: 0x1000,
            memory_type: E820NonRamType::AcpiNvs,
        },
        FwCfgNonRamRegion {
            base: 0xf_0000,
            size: 0x1_0000,
            memory_type: E820NonRamType::Reserved,
        },
    ];

    let entries = decode_e820_records(&build_e820_map(&ram, &non_ram));

    let bases: Vec<u64> = entries.iter().map(|entry| entry.0).collect();
    assert_eq!(bases, alloc::vec![0x0, 0xf_0000, 0x8000_0000, 0xc000_0000]);
}

#[test]
fn e820_encodes_reserved_reclaim_and_nvs_wire_types() {
    let non_ram = [
        FwCfgNonRamRegion {
            base: 0x1000,
            size: 0x1000,
            memory_type: E820NonRamType::Reserved,
        },
        FwCfgNonRamRegion {
            base: 0x2000,
            size: 0x1000,
            memory_type: E820NonRamType::AcpiReclaimable,
        },
        FwCfgNonRamRegion {
            base: 0x3000,
            size: 0x1000,
            memory_type: E820NonRamType::AcpiNvs,
        },
    ];

    let entries = decode_e820_records(&build_e820_map(&[], &non_ram));

    let types: Vec<u32> = entries.iter().map(|entry| entry.2).collect();
    assert_eq!(types, alloc::vec![2, 3, 4]);
}

#[test]
fn e820_drops_zero_length_declared_regions() {
    let ram = [
        FwCfgRamRegion {
            base: 0x0,
            size: 0,
        },
        FwCfgRamRegion {
            base: 0x1000,
            size: 0x1000,
        },
    ];
    let non_ram = [FwCfgNonRamRegion {
        base: 0x9000,
        size: 0,
        memory_type: E820NonRamType::Reserved,
    }];

    let entries = decode_e820_records(&build_e820_map(&ram, &non_ram));

    assert_eq!(entries, alloc::vec![(0x1000, 0x1000, 1)]);
}

#[test]
fn e820_without_non_ram_input_preserves_legacy_memmap_coverage() {
    let ram = [
        FwCfgRamRegion {
            base: 0x0,
            size: 0x1000_0000,
        },
        FwCfgRamRegion {
            base: 0x8000_0000,
            size: 0x2400_0000,
        },
    ];

    // The pre-e820 "etc/memmap" payload and the typed encoder must expose exactly
    // the same (addr, size, type=RAM) coverage when nothing else is declared; only
    // the record stride differs (24-byte padded vs packed 20-byte).
    let legacy_entries = decode_legacy_memmap_records(&legacy_memmap_bytes(&ram));
    let e820_entries = decode_e820_records(&build_e820_map(&ram, &[]));

    assert_eq!(e820_entries, legacy_entries);
}

fn decode_e820_records(payload: &[u8]) -> Vec<(u64, u64, u32)> {
    payload
        .chunks_exact(20)
        .map(|record| {
            (
                u64::from_le_bytes(record[0..8].try_into().unwrap()),
                u64::from_le_bytes(record[8..16].try_into().unwrap()),
                u32::from_le_bytes(record[16..20].try_into().unwrap()),
            )
        })
        .collect()
}

// Mirrors the retired `build_memmap` wire shape: 24-byte records with four trailing
// padding bytes, all entries typed as usable RAM.
fn legacy_memmap_bytes(regions: &[FwCfgRamRegion]) -> Vec<u8> {
    let mut memmap = Vec::new();
    for region in regions.iter().filter(|region| region.size != 0) {
        memmap.extend_from_slice(&region.base.to_le_bytes());
        memmap.extend_from_slice(&region.size.to_le_bytes());
        memmap.extend_from_slice(&1u32.to_le_bytes());
        memmap.extend_from_slice(&0u32.to_le_bytes());
    }
    memmap
}

fn decode_legacy_memmap_records(payload: &[u8]) -> Vec<(u64, u64, u32)> {
    payload
        .chunks_exact(24)
        .map(|record| {
            (
                u64::from_le_bytes(record[0..8].try_into().unwrap()),
                u64::from_le_bytes(record[8..16].try_into().unwrap()),
                u32::from_le_bytes(record[16..20].try_into().unwrap()),
            )
        })
        .collect()
}
