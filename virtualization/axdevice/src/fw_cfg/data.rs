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

/// Size in bytes of one packed `etc/e820` record: `{ u64 addr; u64 size; u32 type }`.
const E820_RECORD_SIZE: usize = 20;
/// `etc/e820` record type for usable guest RAM.
const E820_RAM_TYPE: u32 = 1;

/// Builds the `etc/e820` fw_cfg payload: a packed little-endian array of records
/// `{ addr: u64, size: u64, type: u32 }`, one per declared region.
///
/// Wire invariants consumers rely on (QEMU `hw/i386/fw_cfg.c`, edk2 OVMF
/// `PlatformScanE820`): records carry no padding bytes because readers derive the
/// entry count from `payload_len / 20`. Records are emitted in ascending base order
/// so the map is deterministic and monotonic; the sort is stable, so equal-base ties
/// keep declaration order with declared RAM ahead of declared non-RAM windows.
/// Zero-sized declarations are dropped. Every record traces back to exactly one
/// declared input (`ram_regions` as type 1, `non_ram_regions` at their declared
/// type); this builder never invents or merges entries — callers must keep the two
/// input sets pairwise disjoint.
pub(super) fn build_e820_map(
    ram_regions: &[FwCfgRamRegion],
    non_ram_regions: &[FwCfgNonRamRegion],
) -> Vec<u8> {
    let mut entries = Vec::with_capacity(ram_regions.len() + non_ram_regions.len());
    entries.extend(
        ram_regions
            .iter()
            .filter(|region| region.size != 0)
            .map(|region| (region.base, region.size, E820_RAM_TYPE)),
    );
    // Pushed after the RAM regions so equal-base ties resolve RAM-first.
    entries.extend(
        non_ram_regions
            .iter()
            .filter(|region| region.size != 0)
            .map(|region| (region.base, region.size, region.memory_type.wire_value())),
    );
    entries.sort_by_key(|&(base, ..)| base);

    let mut e820 = Vec::with_capacity(entries.len() * E820_RECORD_SIZE);
    for (base, size, memory_type) in entries {
        push_e820_record(&mut e820, base, size, memory_type);
    }
    e820
}

fn push_e820_record(e820: &mut Vec<u8>, base: u64, length: u64, memory_type: u32) {
    // Packed little-endian layout with no trailing padding: consumers count records
    // by dividing the payload length by 20.
    e820.extend_from_slice(&base.to_le_bytes());
    e820.extend_from_slice(&length.to_le_bytes());
    e820.extend_from_slice(&memory_type.to_le_bytes());
}

pub(super) fn build_smbios_tables() -> Vec<u8> {
    let mut table = Vec::with_capacity(6);
    table.push(127);
    table.push(4);
    table.extend_from_slice(&0x7f00u16.to_le_bytes());
    table.extend_from_slice(&[0, 0]);
    table
}

pub(super) fn build_smbios_anchor() -> Vec<u8> {
    let table = build_smbios_tables();
    let mut anchor = Vec::with_capacity(24);
    anchor.extend_from_slice(b"_SM3_");
    anchor.push(0);
    anchor.push(24);
    anchor.push(3);
    anchor.push(0);
    anchor.push(0);
    anchor.push(1);
    anchor.push(0);
    anchor.extend_from_slice(&(table.len() as u32).to_le_bytes());
    anchor.extend_from_slice(&0u64.to_le_bytes());
    let checksum = (0u8).wrapping_sub(anchor.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)));
    anchor[5] = checksum;
    anchor
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
