use axdevice::{PciCapabilityEffectAccess, PciConfigEffectId};
use axvirtio_common::pci::VirtioPciCapabilityType;

use super::{super::config::decode_pci_cfg_bytes, *};

#[test]
fn conversion_preserves_derived_lengths_and_pci_cfg_effect() {
    let specs = virtio_capabilities(&VirtioPciCapabilitySet::new(16)).unwrap();
    assert_eq!(specs.len(), 5);
    assert_eq!(specs[0].body().len(), 14);
    assert_eq!(specs[1].body().len(), 18);
    assert_eq!(specs[3].body().len(), 14);
    assert_eq!(specs[4].body().len(), 18);
    assert_eq!(specs[4].effects().len(), 1);
    assert_eq!(specs[0].body()[0], 16);
    assert_eq!(specs[1].body()[0], 20);
    assert_eq!(specs[4].body()[0], 20);
    let effect = specs[4].effects()[0];
    assert_eq!(effect.effect(), PciConfigEffectId::new(1));
    assert_eq!(effect.offset(), 16);
    assert_eq!(effect.length(), 4);
    assert_eq!(effect.access(), PciCapabilityEffectAccess::ReadWrite);
    assert_eq!(specs[4].write_mask()[2], u8::MAX);
    assert!(
        specs[4].write_mask()[6..14]
            .iter()
            .all(|mask| *mask == u8::MAX)
    );
    assert!(specs[4].write_mask()[14..].iter().all(|mask| *mask == 0));
}

#[test]
fn pci_cfg_selector_targets_bar_zero_without_including_access_width() {
    let mut body = [0; 18];
    body[0] = 20;
    body[1] = VirtioPciCapabilityType::PciConfig as u8;
    body[2] = 0;
    body[6..10].copy_from_slice(&0x2f0_u32.to_le_bytes());
    body[10..14].copy_from_slice(&4_u32.to_le_bytes());

    assert_eq!(
        decode_pci_cfg_bytes(PciConfigEffectId::new(1), 16, AccessWidth::Dword, &body,),
        Ok(0x2f0)
    );

    body[10..14].copy_from_slice(&2_u32.to_le_bytes());
    assert_eq!(
        decode_pci_cfg_bytes(PciConfigEffectId::new(1), 17, AccessWidth::Word, &body,),
        Ok(0x2f1)
    );

    body[10..14].copy_from_slice(&1_u32.to_le_bytes());
    assert_eq!(
        decode_pci_cfg_bytes(PciConfigEffectId::new(1), 18, AccessWidth::Byte, &body,),
        Ok(0x2f2)
    );
}

#[test]
fn pci_cfg_selector_rejects_wrong_bar_width_and_boundary() {
    let mut body = [0; 18];
    body[0] = 20;
    body[1] = VirtioPciCapabilityType::PciConfig as u8;
    body[2] = 1;
    body[6..10].copy_from_slice(&0x0_u32.to_le_bytes());
    body[10..14].copy_from_slice(&1_u32.to_le_bytes());
    assert!(decode_pci_cfg_bytes(PciConfigEffectId::new(1), 16, AccessWidth::Byte, &body).is_err());

    body[2] = 0;
    body[6..10].copy_from_slice(&0xfff_u32.to_le_bytes());
    body[10..14].copy_from_slice(&4_u32.to_le_bytes());
    assert!(
        decode_pci_cfg_bytes(PciConfigEffectId::new(1), 16, AccessWidth::Dword, &body,).is_err()
    );
}
