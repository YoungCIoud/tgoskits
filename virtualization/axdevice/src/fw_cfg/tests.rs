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
        bytes: alloc::vec![0; 0x200],
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
        bytes: alloc::vec![0; 0x200],
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
        bytes: alloc::vec![0; 0x200],
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

struct ParsedSmbiosStructure {
    structure_type: u8,
    formatted_length: usize,
    handle: u16,
    formatted: Vec<u8>,
    strings: Vec<String>,
    total_length: usize,
}

/// Test-side SMBIOS walker: parses concatenated structures back out of a table
/// blob independently of any production helper.
fn parse_smbios_tables(tables: &[u8]) -> Vec<ParsedSmbiosStructure> {
    let mut structures = Vec::new();
    let mut offset = 0usize;
    while offset < tables.len() {
        assert!(offset + 4 <= tables.len(), "truncated SMBIOS header");
        let structure_type = tables[offset];
        let formatted_length = tables[offset + 1] as usize;
        let handle = u16::from_le_bytes([tables[offset + 2], tables[offset + 3]]);
        assert!(
            formatted_length >= 4,
            "SMBIOS formatted length below header size"
        );
        let string_area = offset + formatted_length;
        assert!(
            string_area <= tables.len(),
            "SMBIOS structure exceeds blob length"
        );
        let mut cursor = string_area;
        let terminator = loop {
            assert!(cursor + 2 <= tables.len(), "unterminated SMBIOS string set");
            if tables[cursor] == 0 && tables[cursor + 1] == 0 {
                break cursor + 2;
            }
            cursor += 1;
        };

        let mut strings = Vec::new();
        let mut start = string_area;
        for (scan, byte) in tables[string_area..terminator - 1]
            .iter()
            .enumerate()
            .map(|(index, byte)| (string_area + index, *byte))
        {
            if byte == 0 {
                // A leading NUL belongs to the terminating pair of an empty
                // string set, not to a zero-length string.
                if scan > start {
                    strings.push(String::from_utf8_lossy(&tables[start..scan]).into_owned());
                }
                start = scan + 1;
            }
        }

        structures.push(ParsedSmbiosStructure {
            structure_type,
            formatted_length,
            handle,
            formatted: tables[offset..string_area].to_vec(),
            strings,
            total_length: terminator - offset,
        });
        offset = terminator;
    }
    structures
}

fn expected_smbios_types(cpu_num: u16) -> Vec<u8> {
    [0u8, 1, 3]
        .into_iter()
        .chain(core::iter::repeat(4).take(cpu_num as usize))
        .chain([127])
        .collect()
}

#[test]
fn smbios_anchor_signature_length_and_checksum_sum_to_zero() {
    let blobs = build_smbios_blobs(1);
    assert_eq!(blobs.anchor.len(), 24);
    assert_eq!(&blobs.anchor[..5], b"_SM3_");
    assert_eq!(blobs.anchor[6], 24);
    assert_eq!(blobs.anchor[7], 3, "major version");
    assert_eq!(blobs.anchor[8], 0, "minor version");
    assert_eq!(blobs.anchor[9], 0, "document revision");
    assert_eq!(blobs.anchor[10], 1, "entry point revision");

    let checksum = blobs
        .anchor
        .iter()
        .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
    assert_eq!(checksum, 0, "all 24 anchor bytes must sum to zero");

    let rebuilt = build_smbios_blobs(1);
    assert_eq!(blobs.anchor, rebuilt.anchor, "anchor build is deterministic");
    assert_eq!(blobs.tables, rebuilt.tables, "tables build is deterministic");
}

#[test]
fn smbios_anchor_max_structure_size_matches_largest_walked_structure() {
    for cpu_num in [0u16, 1, 4] {
        let blobs = build_smbios_blobs(cpu_num);
        let largest = parse_smbios_tables(&blobs.tables)
            .iter()
            .map(|structure| structure.total_length as u32)
            .max()
            .unwrap();
        assert_eq!(
            u32::from_le_bytes(blobs.anchor[0x0c..0x10].try_into().unwrap()),
            largest,
            "anchor max-structure-size mismatch for cpu_num={cpu_num}"
        );
    }
}

#[test]
fn smbios_tables_walk_yields_expected_types_handles_and_lengths() {
    let cpu_num = 4u16;
    let blobs = build_smbios_blobs(cpu_num);
    let structures = parse_smbios_tables(&blobs.tables);

    assert_eq!(
        structures
            .iter()
            .map(|structure| structure.structure_type)
            .collect::<Vec<_>>(),
        expected_smbios_types(cpu_num),
        "structure emission order"
    );

    let spec_minimum_lengths = |structure_type: u8| match structure_type {
        0 => 0x18,
        1 => 0x1B,
        3 => 0x15,
        4 => 0x2A,
        127 => 4usize,
        other => panic!("unexpected structure type {other}"),
    };
    for structure in &structures {
        assert_eq!(
            structure.formatted_length,
            spec_minimum_lengths(structure.structure_type),
            "type {} formatted length",
            structure.structure_type
        );
    }

    let mut handles = structures
        .iter()
        .map(|structure| structure.handle)
        .collect::<Vec<_>>();
    assert_eq!(handles.len(), structures.len());
    handles.sort_unstable();
    handles.dedup();
    assert_eq!(
        handles.len(),
        structures.len(),
        "handles must be unique across all structures"
    );

    let walked: usize = structures.iter().map(|structure| structure.total_length).sum();
    assert_eq!(walked, blobs.tables.len(), "walk must end at blob end");
}

#[test]
fn smbios_string_sets_encode_null_terminators_and_empty_set_edge() {
    let cpu_num = 2u16;
    let blobs = build_smbios_blobs(cpu_num);
    assert_eq!(&blobs.tables[blobs.tables.len() - 2..], &[0, 0]);

    let structures = parse_smbios_tables(&blobs.tables);
    let processor_strings = structures
        .iter()
        .filter(|structure| structure.structure_type == 4)
        .map(|structure| structure.strings.clone())
        .collect::<Vec<_>>();
    for (index, strings) in processor_strings.iter().enumerate() {
        assert_eq!(
            strings,
            &[
                alloc::format!("CPU {index}"),
                String::from("TGOSKITS"),
                String::from("Virtual CPU"),
            ]
        );
    }
    for structure in structures {
        match structure.structure_type {
            0 => {
                assert_eq!(structure.strings, ["TGOSKITS", "1.0", "01/01/2024"]);
            }
            1 => {
                assert_eq!(
                    structure.strings,
                    [
                        "TGOSKITS",
                        "TGOSKITS Virtual Machine",
                        "1.0",
                        "0",
                        "Default",
                        "TGOSKITS",
                    ]
                );
            }
            3 => {
                assert_eq!(structure.strings, ["TGOSKITS"]);
            }
            127 => {
                assert!(structure.strings.is_empty(), "end-of-table has no strings");
                assert_eq!(
                    structure.total_length,
                    structure.formatted_length + 2,
                    "type 127 ends right after its double-NUL terminator"
                );
            }
            other => panic!("unexpected structure type {other}"),
        }
    }
}

#[test]
fn smbios_type4_count_matches_vcpu_number_with_unique_socket_labels() {
    for cpu_num in [1u16, 4] {
        let structures = parse_smbios_tables(&build_smbios_blobs(cpu_num).tables);
        let processors = structures
            .iter()
            .filter(|structure| structure.structure_type == 4)
            .collect::<Vec<_>>();
        assert_eq!(processors.len(), cpu_num as usize);

        let mut handles = processors.iter().map(|p| p.handle).collect::<Vec<_>>();
        handles.sort_unstable();
        handles.dedup();
        assert_eq!(handles.len(), cpu_num as usize, "per-cpu handles distinct");

        for (index, processor) in processors.iter().enumerate() {
            assert_eq!(processor.strings[0], alloc::format!("CPU {index}"));
            assert_eq!(processor.formatted[0x05], 3, "processor type: CPU");
            assert_eq!(
                u16::from_le_bytes(processor.formatted[0x14..0x16].try_into().unwrap()),
                2000,
                "max speed"
            );
            assert_eq!(processor.formatted[0x18], 0x41, "populated and enabled");
            assert_eq!(processor.formatted[0x23], 1, "core count");
            assert_eq!(processor.formatted[0x24], 1, "cores enabled");
            assert_eq!(processor.formatted[0x25], 1, "thread count");
        }
    }
}

#[test]
fn smbios_type127_is_last_structure_of_table() {
    for cpu_num in [0u16, 1, 5] {
        let structures = parse_smbios_tables(&build_smbios_blobs(cpu_num).tables);
        assert_eq!(
            structures
                .iter()
                .filter(|structure| structure.structure_type == 127)
                .count(),
            1,
            "exactly one end-of-table structure"
        );
        let last = structures.last().unwrap();
        assert_eq!(last.structure_type, 127);
        assert_eq!(last.formatted_length, 4);
        assert_eq!(last.total_length, 6);
    }
}

#[test]
fn fw_cfg_file_dir_sizes_match_built_smbios_blobs() {
    let fw_cfg = FwCfg::new(
        GuestPhysAddr::from_usize(0),
        0x20,
        FwCfgKernelPayload::unsplit(Arc::from(&b"kernel"[..])),
        None,
        None,
        4,
        FwCfgPlatformConfig::default(),
    );

    let dir = &fw_cfg.file_dir;
    let count = u32::from_be_bytes(dir[..4].try_into().unwrap()) as usize;
    let record_size = 4 + 2 + 2 + FW_CFG_FILE_NAME_SIZE;
    let mut found = Vec::new();
    for record in dir[4..].chunks_exact(record_size).take(count) {
        let size = u32::from_be_bytes(record[..4].try_into().unwrap()) as usize;
        let name = String::from_utf8_lossy(&record[8..]).into_owned();
        found.push((name, size));
    }
    assert_eq!(found.len(), count);

    let anchor_size = found
        .iter()
        .find(|(name, _)| name.starts_with("etc/smbios/smbios-anchor\0"))
        .map(|(_, size)| *size)
        .unwrap();
    let tables_size = found
        .iter()
        .find(|(name, _)| name.starts_with("etc/smbios/smbios-tables\0"))
        .map(|(_, size)| *size)
        .unwrap();
    assert_eq!(anchor_size, fw_cfg.smbios_anchor.len());
    assert_eq!(tables_size, fw_cfg.smbios_tables.len());
}
