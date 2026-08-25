use super::*;

pub(super) struct FwCfgFile<'a> {
    pub(super) name: &'a str,
    pub(super) selector: u16,
    pub(super) size: u32,
}

pub(super) fn build_file_dir(files: &[FwCfgFile<'_>]) -> Vec<u8> {
    let mut dir = Vec::with_capacity(4 + files.len() * (4 + 2 + 2 + FW_CFG_FILE_NAME_SIZE));
    dir.extend_from_slice(&(files.len() as u32).to_be_bytes());
    for file in files {
        dir.extend_from_slice(&file.size.to_be_bytes());
        dir.extend_from_slice(&file.selector.to_be_bytes());
        dir.extend_from_slice(&0u16.to_be_bytes());
        let name = file.name.as_bytes();
        let name_len = core::cmp::min(name.len(), FW_CFG_FILE_NAME_SIZE);
        dir.extend_from_slice(&name[..name_len]);
        dir.resize(dir.len() + FW_CFG_FILE_NAME_SIZE - name_len, 0);
    }
    dir
}

pub(super) fn build_memmap(regions: &[FwCfgRamRegion]) -> Vec<u8> {
    let mut memmap = Vec::with_capacity(regions.len() * 24);
    for region in regions {
        if region.size != 0 {
            push_memmap_entry(&mut memmap, region.base, region.size);
        }
    }
    memmap
}

fn push_memmap_entry(memmap: &mut Vec<u8>, base: u64, length: u64) {
    memmap.extend_from_slice(&base.to_le_bytes());
    memmap.extend_from_slice(&length.to_le_bytes());
    memmap.extend_from_slice(&MEMMAP_RAM_TYPE.to_le_bytes());
    memmap.extend_from_slice(&0u32.to_le_bytes());
}

/// Prebuilt SMBIOS 3.0 blobs handed to the fw_cfg file directory.
pub(super) struct SmbiosBlobs {
    pub(super) tables: Vec<u8>,
    pub(super) anchor: Vec<u8>,
}

/// Upper bound on emitted Type 4 structures. Handles are assigned sequentially
/// in emission order, so this cap keeps the total structure count (Types 0/1/3
/// plus processors plus end-of-table, i.e. at most `0xF004`) far below the
/// u16 handle space.
const SMBIOS_MAX_TYPE4_STRUCTURES: u16 = 0xF000;

const SMBIOS_ANCHOR_LENGTH: usize = 24;

// Deterministic firmware identity constants: no host DMI, time, or RNG input.
const SMBIOS_BIOS_VENDOR: &str = "TGOSKITS";
const SMBIOS_BIOS_VERSION: &str = "1.0";
const SMBIOS_BIOS_RELEASE_DATE: &str = "01/01/2024";
const SMBIOS_MANUFACTURER: &str = "TGOSKITS";
const SMBIOS_PRODUCT_NAME: &str = "TGOSKITS Virtual Machine";
const SMBIOS_SYSTEM_VERSION: &str = "1.0";
const SMBIOS_SERIAL_NUMBER: &str = "0";
const SMBIOS_SKU_NUMBER: &str = "Default";
const SMBIOS_FAMILY_NAME: &str = "TGOSKITS";
const SMBIOS_PROCESSOR_VERSION: &str = "Virtual CPU";
const SMBIOS_PROCESSOR_MAX_SPEED_MHZ: u16 = 2000;
const SMBIOS_PROCESSOR_CURRENT_SPEED_MHZ: u16 = 2000;

/// Fixed deterministic system UUID copied verbatim into the Type 1 UUID field.
/// SMBIOS leaves the byte order of this field to the OEM, so no swap or other
/// interpretation is applied to these bytes.
const SMBIOS_SYSTEM_UUID: [u8; 16] = [
    0x54, 0x47, 0x4f, 0x53, 0x4b, 0x49, 0x54, 0x53, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
];

pub(super) fn build_smbios_blobs(cpu_num: u16) -> SmbiosBlobs {
    let tables = build_smbios_tables(cpu_num);
    // The anchor max-structure-size field describes the largest single
    // structure (formatted area + strings + terminator), so derive it from the
    // actual emitted bytes instead of the whole-blob length.
    let anchor = build_smbios_anchor(largest_structure_size(&tables));
    SmbiosBlobs { tables, anchor }
}

/// Emits the SMBIOS structure table: Types 0, 1, 3, one Type 4 per vCPU, and
/// the Type 127 end-of-table marker, concatenated in that order.
///
/// Structures are deterministic given `cpu_num`: every byte comes from the
/// constants above or from the per-CPU loop index.
pub(super) fn build_smbios_tables(cpu_num: u16) -> Vec<u8> {
    let mut table = Vec::new();
    let mut next_handle: u16 = 0;
    push_type0_bios_information(&mut table, allocate_handle(&mut next_handle));
    push_type1_system_information(&mut table, allocate_handle(&mut next_handle));
    push_type3_system_enclosure(&mut table, allocate_handle(&mut next_handle));

    let processor_count = core::cmp::min(cpu_num, SMBIOS_MAX_TYPE4_STRUCTURES);
    for cpu_index in 0..processor_count {
        push_type4_processor(
            &mut table,
            allocate_handle(&mut next_handle),
            cpu_index as usize,
        );
    }

    push_end_of_table(&mut table, allocate_handle(&mut next_handle));
    table
}

/// Hands out sequential u16 handles in emission order; uniqueness holds by
/// construction because the structure count is capped well below `u16::MAX`.
fn allocate_handle(next_handle: &mut u16) -> u16 {
    let handle = *next_handle;
    *next_handle += 1;
    handle
}

fn push_structure_header(
    table: &mut Vec<u8>,
    structure_type: u8,
    formatted_length: u8,
    handle: u16,
) {
    table.push(structure_type);
    table.push(formatted_length);
    table.extend_from_slice(&handle.to_le_bytes());
}

/// Appends the double-NUL terminated string set of one structure.
///
/// Builders never emit an empty string: absent strings are omitted entirely
/// and referenced by index 0. That keeps the first NUL pair after a formatted
/// area the true terminator, which the blob walk in `largest_structure_size`
/// relies on.
fn push_strings(table: &mut Vec<u8>, strings: &[&str]) {
    for string in strings {
        table.extend_from_slice(string.as_bytes());
        table.push(0);
    }
    table.push(0);
}

fn push_type0_bios_information(table: &mut Vec<u8>, handle: u16) {
    const TYPE0_FORMATTED_LENGTH: u8 = 0x18;
    push_structure_header(table, 0, TYPE0_FORMATTED_LENGTH, handle);
    table.extend_from_slice(&[
        1, // BIOS vendor string
        2, // BIOS version string
    ]);
    table.extend_from_slice(&0xE800u16.to_le_bytes()); // starting address segment
    table.push(3); // BIOS release date string
    table.push(0); // BIOS ROM size not reported
    // Byte 0 bit 3: "BIOS Characteristics Not Supported".
    table.extend_from_slice(&[0x08, 0, 0, 0, 0, 0, 0, 0]);
    table.extend_from_slice(&[0, 0]); // characteristic extension bytes
    // System BIOS and embedded controller firmware releases unsupported.
    table.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
    push_strings(
        table,
        &[
            SMBIOS_BIOS_VENDOR,
            SMBIOS_BIOS_VERSION,
            SMBIOS_BIOS_RELEASE_DATE,
        ],
    );
}

fn push_type1_system_information(table: &mut Vec<u8>, handle: u16) {
    const TYPE1_FORMATTED_LENGTH: u8 = 0x1B;
    push_structure_header(table, 1, TYPE1_FORMATTED_LENGTH, handle);
    table.extend_from_slice(&[
        1, // manufacturer string
        2, // product name string
        3, // version string
        4, // serial number string
    ]);
    table.extend_from_slice(&SMBIOS_SYSTEM_UUID);
    table.push(2); // wake-up type "Unknown": no host power state is available
    table.extend_from_slice(&[
        5, // SKU number string
        6, // family string
    ]);
    push_strings(
        table,
        &[
            SMBIOS_MANUFACTURER,
            SMBIOS_PRODUCT_NAME,
            SMBIOS_SYSTEM_VERSION,
            SMBIOS_SERIAL_NUMBER,
            SMBIOS_SKU_NUMBER,
            SMBIOS_FAMILY_NAME,
        ],
    );
}

fn push_type3_system_enclosure(table: &mut Vec<u8>, handle: u16) {
    const TYPE3_FORMATTED_LENGTH: u8 = 0x15;
    push_structure_header(table, 3, TYPE3_FORMATTED_LENGTH, handle);
    table.extend_from_slice(&[
        1, // manufacturer string
        1, // chassis type: Other
        0, // version string absent
        0, // serial number absent
        0, // asset tag absent
        3, // boot-up state: Safe
        3, // power supply state: Safe
        3, // thermal state: Safe
        3, // security status: None
    ]);
    table.extend_from_slice(&[0; 4]); // OEM-defined
    table.extend_from_slice(&[
        0, // height
        1, // number of power cords
        0, // contained element count
        0, // contained element record length
    ]);
    push_strings(table, &[SMBIOS_MANUFACTURER]);
}

fn push_type4_processor(table: &mut Vec<u8>, handle: u16, cpu_index: usize) {
    const TYPE4_FORMATTED_LENGTH: u8 = 0x2A;
    push_structure_header(table, 4, TYPE4_FORMATTED_LENGTH, handle);
    table.push(1); // socket designation string
    table.push(3); // processor type: CPU
    table.push(1); // processor family: Other
    table.extend_from_slice(&0u64.to_le_bytes()); // processor ID not reported
    table.push(2); // manufacturer string
    table.push(3); // version string
    table.push(0); // voltage capability not reported
    table.extend_from_slice(&0u16.to_le_bytes()); // external clock unknown
    table.extend_from_slice(&SMBIOS_PROCESSOR_MAX_SPEED_MHZ.to_le_bytes());
    table.extend_from_slice(&SMBIOS_PROCESSOR_CURRENT_SPEED_MHZ.to_le_bytes());
    table.push(0x41); // status: populated and enabled
    table.push(1); // upgrade: Other
    table.extend_from_slice(&u16::MAX.to_le_bytes()); // L1 cache handle absent
    table.extend_from_slice(&u16::MAX.to_le_bytes()); // L2 cache handle absent
    table.extend_from_slice(&u16::MAX.to_le_bytes()); // L3 cache handle absent
    table.push(0); // serial number string absent
    table.push(0); // asset tag string absent
    table.push(0); // part number string absent
    table.push(1); // core count
    table.push(1); // cores enabled
    table.push(1); // thread count
    table.extend_from_slice(&0x0004u16.to_le_bytes()); // 64-bit capable
    // Family 2 word accompanying the Family = 01h byte above.
    table.extend_from_slice(&0xFE01u16.to_le_bytes());

    let socket = alloc::format!("CPU {cpu_index}");
    push_strings(
        table,
        &[
            socket.as_str(),
            SMBIOS_MANUFACTURER,
            SMBIOS_PROCESSOR_VERSION,
        ],
    );
}

fn push_end_of_table(table: &mut Vec<u8>, handle: u16) {
    // The end-of-table entry has no string set: it ends right after its
    // four-byte formatted area plus the mandatory terminator pair.
    push_structure_header(table, 127, 4, handle);
    table.extend_from_slice(&[0, 0]);
}

/// Builds the 24-byte `_SM3_` 64-bit entry point. `max_structure_size` is the
/// size of the largest single structure including its string set and
/// terminator; the checksum makes all 24 bytes sum to zero mod 256.
pub(super) fn build_smbios_anchor(max_structure_size: u32) -> Vec<u8> {
    let mut anchor = Vec::with_capacity(SMBIOS_ANCHOR_LENGTH);
    anchor.extend_from_slice(b"_SM3_");
    anchor.push(0); // checksum, filled in below
    anchor.push(SMBIOS_ANCHOR_LENGTH as u8);
    anchor.push(3); // SMBIOS major version
    anchor.push(0); // SMBIOS minor version
    anchor.push(0); // SMBIOS document revision
    anchor.push(1); // entry point revision
    anchor.push(0); // reserved
    anchor.extend_from_slice(&max_structure_size.to_le_bytes());
    anchor.extend_from_slice(&0u64.to_le_bytes()); // reserved
    let checksum = (0u8).wrapping_sub(anchor.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)));
    anchor[5] = checksum;
    anchor
}

/// Walks concatenated structures and returns the size of the largest one
/// (formatted area + strings + terminator). Like the blob walkers on the guest
/// side, it relies on builders never emitting an empty string, so the first
/// NUL pair after a formatted area ends that structure. A malformed tail stops
/// the walk and yields the largest size measured so far.
fn largest_structure_size(tables: &[u8]) -> u32 {
    let mut largest = 0u32;
    let mut offset = 0usize;
    while offset + 4 <= tables.len() {
        let formatted_length = tables[offset + 1] as usize;
        if formatted_length < 4 || offset + formatted_length > tables.len() {
            break;
        }
        let mut cursor = offset + formatted_length;
        let end = loop {
            if cursor + 2 > tables.len() {
                break tables.len();
            }
            if tables[cursor] == 0 && tables[cursor + 1] == 0 {
                break cursor + 2;
            }
            cursor += 1;
        };
        largest = largest.max((end - offset) as u32);
        offset = end;
    }
    largest
}

pub(super) enum FwCfgEntry<'a> {
    Bytes(&'a [u8]),
    Owned(Vec<u8>),
}

impl<'a> FwCfgEntry<'a> {
    pub(super) fn as_slice(&'a self) -> &'a [u8] {
        match self {
            Self::Bytes(bytes) => bytes,
            Self::Owned(bytes) => bytes,
        }
    }
}
