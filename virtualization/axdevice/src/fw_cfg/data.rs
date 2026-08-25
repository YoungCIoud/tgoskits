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

pub(super) fn build_smbios_blobs(cpu_num: u16) -> SmbiosBlobs {
    let tables = build_smbios_tables(cpu_num);
    let anchor = build_smbios_anchor(tables.len() as u32);
    SmbiosBlobs { tables, anchor }
}

pub(super) fn build_smbios_tables(cpu_num: u16) -> Vec<u8> {
    let _ = cpu_num;
    let mut table = Vec::with_capacity(6);
    table.push(127);
    table.push(4);
    table.extend_from_slice(&0x7f00u16.to_le_bytes());
    table.extend_from_slice(&[0, 0]);
    table
}

pub(super) fn build_smbios_anchor(max_structure_size: u32) -> Vec<u8> {
    let mut anchor = Vec::with_capacity(24);
    anchor.extend_from_slice(b"_SM3_");
    anchor.push(0);
    anchor.push(24);
    anchor.push(3);
    anchor.push(0);
    anchor.push(0);
    anchor.push(1);
    anchor.push(0);
    anchor.extend_from_slice(&max_structure_size.to_le_bytes());
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
