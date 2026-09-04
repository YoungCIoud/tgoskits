//! OVMF image preparation for nested x86 Axvisor tests.

use std::{
    fmt, fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, ensure};
use ostool::ovmf::Arch;
use tempfile::NamedTempFile;

use crate::support::{download::file_sha256, ovmf::OvmfFirmware};

const OVMF_SIZE: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OvmfLayout {
    SplitCodeVars,
    MonolithicCode,
}

impl fmt::Display for OvmfLayout {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SplitCodeVars => formatter.write_str("split CODE/VARS"),
            Self::MonolithicCode => formatter.write_str("monolithic CODE"),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct FirmwareFileEvidence {
    path: PathBuf,
    size: u64,
    sha256: String,
}

impl FirmwareFileEvidence {
    fn collect(path: &Path) -> anyhow::Result<Self> {
        let size = fs::metadata(path)
            .with_context(|| format!("failed to inspect firmware {}", path.display()))?
            .len();
        Ok(Self {
            path: path.to_path_buf(),
            size,
            sha256: file_sha256(path)?,
        })
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct OvmfEvidence {
    layout: OvmfLayout,
    code: FirmwareFileEvidence,
    vars: FirmwareFileEvidence,
    guest: FirmwareFileEvidence,
}

impl fmt::Display for OvmfEvidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let vars_usage = match self.layout {
            OvmfLayout::SplitCodeVars => "prefix",
            OvmfLayout::MonolithicCode => "unused",
        };
        write!(
            formatter,
            "Axvisor x86 OVMF firmware evidence:\nlayout: {}\nOVMF CODE: path={} size={} \
             sha256={}\nOVMF VARS: path={} size={} sha256={} usage={}\nguest image: path={} \
             size={} sha256={}",
            self.layout,
            self.code.path.display(),
            self.code.size,
            self.code.sha256,
            self.vars.path.display(),
            self.vars.size,
            self.vars.sha256,
            vars_usage,
            self.guest.path.display(),
            self.guest.size,
            self.guest.sha256,
        )
    }
}

pub(super) async fn prepare_x86_ovmf(output_path: &Path) -> anyhow::Result<OvmfEvidence> {
    let firmware = OvmfFirmware::fetch(Arch::X64).await?;
    prepare_x86_ovmf_from_firmware(output_path, &firmware)
}

pub(super) fn prepare_x86_ovmf_from_monolithic_path(
    output_path: &Path,
    source_path: &Path,
) -> anyhow::Result<OvmfEvidence> {
    let source = FirmwareFileEvidence::collect(source_path)?;
    ensure!(
        source.size > 0 && source.size <= OVMF_SIZE as u64,
        "monolithic OVMF image {} must be between 1 byte and 4 MiB ({} bytes)",
        source_path.display(),
        source.size
    );
    let source_image = fs::read(source_path)
        .with_context(|| format!("failed to read OVMF image {}", source_path.display()))?;
    let mut image = vec![0; OVMF_SIZE - source_image.len()];
    image.extend_from_slice(&source_image);
    install_ovmf_image(output_path, &image)?;
    Ok(OvmfEvidence {
        layout: OvmfLayout::MonolithicCode,
        code: source,
        vars: FirmwareFileEvidence::collect(source_path)?,
        guest: FirmwareFileEvidence::collect(output_path)?,
    })
}

pub(super) fn prepare_x86_ovmf_from_split_paths(
    output_path: &Path,
    code_path: &Path,
    vars_path: &Path,
) -> anyhow::Result<OvmfEvidence> {
    let firmware = OvmfFirmware::from_paths(code_path.to_path_buf(), vars_path.to_path_buf());
    prepare_x86_ovmf_from_firmware(output_path, &firmware)
}

fn prepare_x86_ovmf_from_firmware(
    output_path: &Path,
    firmware: &OvmfFirmware,
) -> anyhow::Result<OvmfEvidence> {
    let code_path = firmware.code();
    let code = fs::read(code_path)
        .with_context(|| format!("failed to read OVMF code image {}", code_path.display()))?;
    let (layout, vars) = if code.len() == OVMF_SIZE {
        (OvmfLayout::MonolithicCode, None)
    } else {
        OVMF_SIZE.checked_sub(code.len()).with_context(|| {
            format!(
                "OVMF code image {} is larger than 4 MiB",
                code_path.display()
            )
        })?;
        (
            OvmfLayout::SplitCodeVars,
            Some(fs::read(firmware.vars()).with_context(|| {
                format!(
                    "failed to read OVMF variable store {}",
                    firmware.vars().display()
                )
            })?),
        )
    };
    let image = assemble_ovmf_image(&code, vars.as_deref())?;

    install_ovmf_image(output_path, &image)?;

    Ok(OvmfEvidence {
        layout,
        code: FirmwareFileEvidence::collect(code_path)?,
        vars: FirmwareFileEvidence::collect(firmware.vars())?,
        guest: FirmwareFileEvidence::collect(output_path)?,
    })
}

fn install_ovmf_image(output_path: &Path, image: &[u8]) -> anyhow::Result<()> {
    let parent = output_path
        .parent()
        .with_context(|| format!("OVMF output path has no parent: {}", output_path.display()))?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create OVMF output directory {}",
            parent.display()
        )
    })?;
    let mut temporary = NamedTempFile::new_in(parent).with_context(|| {
        format!(
            "failed to create temporary OVMF image in {}",
            parent.display()
        )
    })?;
    temporary
        .write_all(image)
        .with_context(|| format!("failed to write {}", output_path.display()))?;
    temporary
        .persist(output_path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to install {}", output_path.display()))?;
    Ok(())
}

fn assemble_ovmf_image(code: &[u8], vars: Option<&[u8]>) -> anyhow::Result<Vec<u8>> {
    ensure!(
        code.len() <= OVMF_SIZE,
        "OVMF code image is larger than 4 MiB"
    );
    let prefix_len = OVMF_SIZE - code.len();
    let vars = vars.unwrap_or_default();
    ensure!(
        vars.len() == prefix_len,
        "OVMF variable-store prefix is {:#x} bytes, expected {prefix_len:#x}",
        vars.len()
    );

    let mut image = Vec::with_capacity(OVMF_SIZE);
    image.extend_from_slice(vars);
    image.extend_from_slice(code);
    Ok(image)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn split_ovmf_is_assembled_at_the_top_of_the_four_mib_window() {
        let vars = vec![0xa5; 0x84_000];
        let code = vec![0x5a; OVMF_SIZE - vars.len()];
        let image = assemble_ovmf_image(&code, Some(&vars)).unwrap();

        assert_eq!(image.len(), OVMF_SIZE);
        assert_eq!(&image[..vars.len()], vars);
        assert_eq!(&image[vars.len()..], code);
        assert!(assemble_ovmf_image(&code, None).is_err());
    }

    #[test]
    fn split_firmware_evidence_matches_the_installed_guest_image() {
        let root = tempdir().unwrap();
        let code_path = root.path().join("OVMF_CODE.fd");
        let vars_path = root.path().join("OVMF_VARS.fd");
        let output_path = root.path().join("guest/OVMF_CODE_4M.fd");
        let vars = vec![0xa5; 0x84_000];
        let code = vec![0x5a; OVMF_SIZE - vars.len()];
        fs::write(&code_path, &code).unwrap();
        fs::write(&vars_path, &vars).unwrap();
        let firmware = OvmfFirmware::from_paths(code_path.clone(), vars_path.clone());

        let evidence = prepare_x86_ovmf_from_firmware(&output_path, &firmware).unwrap();
        let image = fs::read(&output_path).unwrap();

        assert_eq!(evidence.layout, OvmfLayout::SplitCodeVars);
        assert_eq!(image.len(), OVMF_SIZE);
        assert_eq!(&image[..vars.len()], vars);
        assert_eq!(&image[vars.len()..], code);
        assert_eq!(evidence.code.path, code_path);
        assert_eq!(evidence.code.size, code.len() as u64);
        assert_eq!(evidence.code.sha256, file_sha256(&code_path).unwrap());
        assert_eq!(evidence.vars.path, vars_path);
        assert_eq!(evidence.vars.size, vars.len() as u64);
        assert_eq!(evidence.vars.sha256, file_sha256(&vars_path).unwrap());
        assert_eq!(evidence.guest.path, output_path);
        assert_eq!(evidence.guest.size, OVMF_SIZE as u64);
        assert_eq!(evidence.guest.sha256, file_sha256(&output_path).unwrap());

        let text = evidence.to_string();
        assert!(text.contains("layout: split CODE/VARS"));
        assert!(text.contains(&format!("OVMF CODE: path={}", code_path.display())));
        assert!(text.contains(&format!("OVMF VARS: path={}", vars_path.display())));
        assert!(text.contains("usage=prefix"));
        assert!(text.contains(&format!("guest image: path={}", output_path.display())));
    }

    #[test]
    fn monolithic_firmware_reports_the_variable_store_as_unused() {
        let root = tempdir().unwrap();
        let code_path = root.path().join("OVMF_CODE.fd");
        let vars_path = root.path().join("OVMF_VARS.fd");
        let output_path = root.path().join("OVMF_CODE_4M.fd");
        fs::write(&code_path, vec![0x5a; OVMF_SIZE]).unwrap();
        fs::write(&vars_path, b"unused vars").unwrap();
        let firmware = OvmfFirmware::from_paths(code_path, vars_path);

        let evidence = prepare_x86_ovmf_from_firmware(&output_path, &firmware).unwrap();

        assert_eq!(evidence.layout, OvmfLayout::MonolithicCode);
        assert!(evidence.to_string().contains("usage=unused"));
        assert_eq!(fs::metadata(output_path).unwrap().len(), OVMF_SIZE as u64);
    }

    #[test]
    fn external_monolithic_firmware_is_copied_to_the_guest_image() {
        let root = tempdir().unwrap();
        let source_path = root.path().join("OVMF.fd");
        let output_path = root.path().join("guest/OVMF_CODE_4M.fd");
        fs::write(&source_path, vec![0x5a; 2 * 1024 * 1024]).unwrap();

        let evidence = prepare_x86_ovmf_from_monolithic_path(&output_path, &source_path).unwrap();

        assert_eq!(evidence.layout, OvmfLayout::MonolithicCode);
        let image = fs::read(&output_path).unwrap();
        assert_eq!(image.len(), OVMF_SIZE);
        assert!(image[..2 * 1024 * 1024].iter().all(|byte| *byte == 0));
        assert_eq!(&image[2 * 1024 * 1024..], &fs::read(&source_path).unwrap());
        assert!(evidence.to_string().contains("usage=unused"));
    }
}
