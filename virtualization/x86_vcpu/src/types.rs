// Copyright 2026 The Axvisor Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use core::{
    fmt::{Debug, Formatter, LowerHex, UpperHex},
    ops::{Add, AddAssign},
};

use bitflags::bitflags;

/// Size of a 4 KiB page.
pub const X86_PAGE_SIZE_4K: usize = 0x1000;

/// Result type returned by the OS-neutral x86 vCPU core.
pub type X86VcpuResult<T = ()> = Result<T, X86VcpuError>;

/// Errors produced by the OS-neutral x86 vCPU core.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum X86VcpuError {
    /// A caller supplied an invalid argument or unsupported hardware encoding.
    InvalidInput,
    /// Hardware register or exit data could not be decoded as a valid value.
    InvalidData,
    /// The requested operation is not supported by this CPU or vCPU backend.
    Unsupported,
    /// Hardware or software state is inconsistent with the requested transition.
    BadState,
    /// A host allocation failed.
    NoMemory,
    /// The requested hardware resource is already in use.
    ResourceBusy,
}

impl From<x86_vlapic::X86VlapicError> for X86VcpuError {
    fn from(err: x86_vlapic::X86VlapicError) -> Self {
        match err {
            x86_vlapic::X86VlapicError::InvalidInput => Self::InvalidInput,
            x86_vlapic::X86VlapicError::InvalidData => Self::InvalidData,
            x86_vlapic::X86VlapicError::Unsupported => Self::Unsupported,
            x86_vlapic::X86VlapicError::NoMemory => Self::NoMemory,
            x86_vlapic::X86VlapicError::BadState => Self::BadState,
        }
    }
}

macro_rules! define_addr_type {
    ($name:ident, $debug_prefix:literal) => {
        #[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
        pub struct $name(usize);

        impl $name {
            /// Creates an address from a raw `usize`.
            pub const fn from_usize(addr: usize) -> Self {
                Self(addr)
            }

            /// Returns the raw address value.
            pub const fn as_usize(self) -> usize {
                self.0
            }

            /// Returns this address as an immutable pointer.
            pub const fn as_ptr<T>(self) -> *const T {
                self.0 as *const T
            }

            /// Returns this address as a mutable pointer.
            pub const fn as_mut_ptr<T>(self) -> *mut T {
                self.0 as *mut T
            }
        }

        impl From<usize> for $name {
            fn from(value: usize) -> Self {
                Self::from_usize(value)
            }
        }

        impl From<$name> for usize {
            fn from(value: $name) -> Self {
                value.as_usize()
            }
        }

        impl Add<usize> for $name {
            type Output = Self;

            fn add(self, rhs: usize) -> Self::Output {
                Self(self.0 + rhs)
            }
        }

        impl AddAssign<usize> for $name {
            fn add_assign(&mut self, rhs: usize) {
                self.0 += rhs;
            }
        }

        impl Debug for $name {
            fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
                write!(f, "{}({:#x})", $debug_prefix, self.0)
            }
        }

        impl LowerHex for $name {
            fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
                write!(f, "{:#x}", self.0)
            }
        }

        impl UpperHex for $name {
            fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
                write!(f, "{:#X}", self.0)
            }
        }
    };
}

define_addr_type!(X86GuestPhysAddr, "GPA");
define_addr_type!(X86GuestVirtAddr, "GVA");
define_addr_type!(X86HostPhysAddr, "HPA");
define_addr_type!(X86HostVirtAddr, "HVA");

/// The port number of an x86 I/O operation.
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub struct X86Port(u16);

impl X86Port {
    /// Creates a new x86 I/O port.
    pub const fn new(port: u16) -> Self {
        Self(port)
    }

    /// Returns the raw port number.
    pub const fn number(self) -> u16 {
        self.0
    }
}

impl From<u16> for X86Port {
    fn from(value: u16) -> Self {
        Self::new(value)
    }
}

impl Debug for X86Port {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        write!(f, "X86Port({:#x})", self.0)
    }
}

/// x86 MSR address.
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub struct X86MsrAddr(usize);

impl X86MsrAddr {
    /// Creates an MSR address from the raw MSR number.
    pub const fn new(addr: usize) -> Self {
        Self(addr)
    }

    /// Returns the raw MSR number.
    pub const fn addr(self) -> usize {
        self.0
    }
}

impl From<usize> for X86MsrAddr {
    fn from(value: usize) -> Self {
        Self::new(value)
    }
}

impl Debug for X86MsrAddr {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        write!(f, "MSR({:#x})", self.0)
    }
}

/// Width of a trapped guest bus access.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum X86AccessWidth {
    /// 8-bit access.
    Byte,
    /// 16-bit access.
    Word,
    /// 32-bit access.
    Dword,
    /// 64-bit access.
    Qword,
}

impl X86AccessWidth {
    /// Returns this access width in bytes.
    pub const fn size(self) -> usize {
        match self {
            Self::Byte => 1,
            Self::Word => 2,
            Self::Dword => 4,
            Self::Qword => 8,
        }
    }

    /// Returns the bit range covered by this access.
    pub fn bits_range(self) -> core::ops::Range<usize> {
        match self {
            Self::Byte => 0..8,
            Self::Word => 0..16,
            Self::Dword => 0..32,
            Self::Qword => 0..64,
        }
    }

    /// Returns the memory-operand width encoded by a MOV opcode plus its
    /// operand-size and REX prefixes.
    pub(crate) fn for_mov_opcode(
        opcode: u8,
        operand_size_override: bool,
        rex_w: bool,
    ) -> Option<Self> {
        match opcode {
            0x88 | 0x8a | 0xc6 => Some(Self::Byte),
            0x89 | 0x8b | 0xc7 => {
                if rex_w {
                    Some(Self::Qword)
                } else if operand_size_override {
                    Some(Self::Word)
                } else {
                    Some(Self::Dword)
                }
            }
            _ => None,
        }
    }

    /// Masks a value down to this width.
    pub(crate) fn mask_value(self, value: u64) -> u64 {
        match self {
            Self::Byte => value & 0xff,
            Self::Word => value & 0xffff,
            Self::Dword => value & 0xffff_ffff,
            Self::Qword => value,
        }
    }
}

impl TryFrom<usize> for X86AccessWidth {
    type Error = X86VcpuError;

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Byte),
            2 => Ok(Self::Word),
            4 => Ok(Self::Dword),
            8 => Ok(Self::Qword),
            _ => Err(X86VcpuError::InvalidInput),
        }
    }
}

impl From<X86AccessWidth> for usize {
    fn from(value: X86AccessWidth) -> Self {
        value.size()
    }
}

/// Byte register selected by a ModRM.reg field for byte MOV instructions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct X86ByteRegister {
    /// General-purpose register index in the vCPU register file.
    pub(crate) gpr: u8,
    /// When true, the access targets the high byte (AH/CH/DH/BH).
    pub(crate) high: bool,
}

/// Decodes the byte register referenced by `modrm_reg` and a REX prefix byte.
///
/// Without REX, `modrm_reg` 0..3 selects AL/CL/DL/BL and 4..7 selects
/// AH/CH/DH/BH. With REX, `modrm_reg` plus REX.R selects the low byte of the
/// corresponding 64-bit general-purpose register, including SPL/BPL/SIL/DIL.
pub(crate) fn x86_byte_register(modrm_reg: u8, rex: u8) -> Option<X86ByteRegister> {
    if modrm_reg > 7 {
        return None;
    }
    if rex == 0 {
        Some(if modrm_reg < 4 {
            X86ByteRegister {
                gpr: modrm_reg,
                high: false,
            }
        } else {
            X86ByteRegister {
                gpr: modrm_reg - 4,
                high: true,
            }
        })
    } else {
        Some(X86ByteRegister {
            gpr: modrm_reg | ((rex & 0x4) << 1),
            high: false,
        })
    }
}

/// Extracts the byte selected by `byte_reg` from a full GPR value.
pub(crate) fn x86_byte_register_value(gpr_value: u64, byte_reg: X86ByteRegister) -> u8 {
    if byte_reg.high {
        (gpr_value >> 8) as u8
    } else {
        gpr_value as u8
    }
}

/// Merges a byte into a full GPR value without disturbing adjacent bytes.
pub(crate) fn x86_byte_register_merge(gpr_value: u64, byte_reg: X86ByteRegister, value: u8) -> u64 {
    if byte_reg.high {
        (gpr_value & !0xff00) | (u64::from(value) << 8)
    } else {
        (gpr_value & !0xff) | u64::from(value)
    }
}

/// Applies one x86 instruction-prefix byte to decoder state.
///
/// Returns true when `byte` is a supported prefix. Legacy prefixes after a REX
/// prefix invalidate that REX, matching the x86 rule that only the last REX
/// immediately preceding the opcode is effective.
pub(crate) fn x86_simple_prefix_update(
    byte: u8,
    rex: &mut u8,
    operand_size_override: &mut bool,
) -> bool {
    if byte == 0x66 {
        *operand_size_override = true;
        *rex = 0;
        true
    } else if (0x40..=0x4f).contains(&byte) {
        *rex = byte;
        true
    } else {
        false
    }
}

/// Returns the displacement size encoded by a memory-operand ModRM byte.
pub(crate) fn x86_modrm_displacement_size(modrm: u8, sib: Option<u8>, rex: u8) -> Option<usize> {
    let mode = modrm >> 6;
    let rm = modrm & 0x7;
    if mode == 0b11 {
        return None;
    }

    Some(match mode {
        0 => {
            let is_rip_relative = if rm == 0b100 {
                let sib = sib?;
                (sib & 0x7) == 0b101
            } else {
                rm == 0b101
            };
            if is_rip_relative && rex & 0x1 == 0 {
                4
            } else {
                0
            }
        }
        1 => 1,
        2 => 4,
        _ => return None,
    })
}

bitflags! {
    /// Access flags reported for a nested page fault.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct X86AccessFlags: usize {
        /// Read access.
        const READ = 1 << 0;
        /// Write access.
        const WRITE = 1 << 1;
        /// Execute access.
        const EXECUTE = 1 << 2;
    }
}

/// Information about a nested guest page-table fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct X86NestedPageFaultInfo {
    /// Faulting guest physical address.
    pub fault_guest_paddr: X86GuestPhysAddr,
    /// Fault access flags.
    pub access_flags: X86AccessFlags,
}

/// Nested page table configuration selected by the embedding VMM.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct X86NestedPagingConfig {
    /// Root physical address of the nested page table.
    pub root_paddr: X86HostPhysAddr,
    /// Number of nested page-table levels.
    pub levels: usize,
    /// Guest physical address width in bits.
    pub gpa_bits: usize,
    /// Hardware-specific mode value.
    pub mode: usize,
}

impl X86NestedPagingConfig {
    /// Creates a nested paging configuration.
    pub const fn new(
        root_paddr: X86HostPhysAddr,
        levels: usize,
        gpa_bits: usize,
        mode: usize,
    ) -> Self {
        Self {
            root_paddr,
            levels,
            gpa_bits,
            mode,
        }
    }
}

/// VM-exit reason returned by the x86 vCPU core.
#[derive(Debug)]
#[non_exhaustive]
pub enum X86VmExit {
    /// A guest instruction triggered a hypercall.
    Hypercall {
        /// Hypercall number.
        nr: u64,
        /// Hypercall arguments.
        args: [u64; 6],
    },
    /// The guest performed a port I/O read.
    PortIoRead {
        /// I/O port.
        port: X86Port,
        /// Access width.
        width: X86AccessWidth,
    },
    /// The guest performed a port I/O write.
    PortIoWrite {
        /// I/O port.
        port: X86Port,
        /// Access width.
        width: X86AccessWidth,
        /// Value written by the guest.
        data: u64,
    },
    /// The guest performed one element of a string port-I/O instruction.
    PortIoString(crate::X86PortIoStringExit),
    /// The guest performed an MMIO read.
    MmioRead {
        /// Guest physical address.
        addr: X86GuestPhysAddr,
        /// Access width.
        width: X86AccessWidth,
        /// Destination guest register.
        reg: usize,
        /// Destination register width.
        reg_width: X86AccessWidth,
        /// Whether the value should be sign-extended.
        signed_ext: bool,
        /// Byte-register destination for byte-width MOV reads.
        byte_reg: Option<X86ByteRegister>,
    },
    /// The guest performed an MMIO write.
    MmioWrite {
        /// Guest physical address.
        addr: X86GuestPhysAddr,
        /// Access width.
        width: X86AccessWidth,
        /// Value written by the guest.
        data: u64,
    },
    /// The guest performed an MSR read.
    MsrRead {
        /// MSR address.
        addr: X86MsrAddr,
    },
    /// The guest performed an MSR write.
    MsrWrite {
        /// MSR address.
        addr: X86MsrAddr,
        /// Value written by the guest.
        value: u64,
    },
    /// A nested page fault occurred.
    NestedPageFault {
        /// Faulting guest physical address.
        addr: X86GuestPhysAddr,
        /// Access flags.
        access_flags: X86AccessFlags,
    },
    /// A physical host interrupt should be handled by the embedding VMM.
    ExternalInterrupt {
        /// Host vector reported by the backend.
        vector: u8,
    },
    /// The preemption timer expired or the backend wants the VMM to poll timers.
    PreemptionTimer,
    /// A guest EOI completed.
    InterruptEnd {
        /// Vector that may require IOAPIC EOI propagation.
        vector: Option<u8>,
    },
    /// The guest halted.
    Halt,
    /// The guest requested system power-off.
    SystemDown,
    /// VM entry failed in hardware.
    FailEntry {
        /// Hardware entry-failure reason.
        hardware_entry_failure_reason: usize,
    },
    /// The exit was handled inside the x86 core.
    Nothing,
}

/// Builds an [`X86VmExit::MmioWrite`] for a decoded MOV register-to-memory
/// device MMIO instruction.
pub(crate) fn mov_mmio_write_exit(
    addr: X86GuestPhysAddr,
    opcode: u8,
    operand_size_override: bool,
    rex_w: bool,
    data: u64,
) -> Option<X86VmExit> {
    let width = X86AccessWidth::for_mov_opcode(opcode, operand_size_override, rex_w)?;
    match opcode {
        0x88 | 0x89 => Some(X86VmExit::MmioWrite {
            addr,
            width,
            data: width.mask_value(data),
        }),
        _ => None,
    }
}

/// Decoded instruction context for the LAPIC/IOAPIC MMIO fast path.
pub(crate) struct X86ApicMmioDecode {
    pub(crate) start: X86GuestVirtAddr,
    pub(crate) rip: X86GuestVirtAddr,
    pub(crate) modrm: u8,
    pub(crate) rex: u8,
    pub(crate) opcode: u8,
    pub(crate) addr: X86GuestPhysAddr,
    pub(crate) write: bool,
    pub(crate) local_apic: bool,
}

/// Returns the encoded immediate size of a MOV memory-store instruction.
pub(crate) fn mov_immediate_size(width: X86AccessWidth) -> usize {
    match width {
        X86AccessWidth::Byte => core::mem::size_of::<u8>(),
        X86AccessWidth::Word => core::mem::size_of::<u16>(),
        // C7 with REX.W still encodes a 32-bit immediate.
        X86AccessWidth::Dword | X86AccessWidth::Qword => core::mem::size_of::<u32>(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_register_decodes_high_bytes_without_rex() {
        assert_eq!(
            x86_byte_register(0, 0),
            Some(X86ByteRegister {
                gpr: 0,
                high: false
            })
        );
        assert_eq!(
            x86_byte_register(3, 0),
            Some(X86ByteRegister {
                gpr: 3,
                high: false
            })
        );
        assert_eq!(
            x86_byte_register(4, 0),
            Some(X86ByteRegister { gpr: 0, high: true })
        );
        assert_eq!(
            x86_byte_register(5, 0),
            Some(X86ByteRegister { gpr: 1, high: true })
        );
        assert_eq!(
            x86_byte_register(6, 0),
            Some(X86ByteRegister { gpr: 2, high: true })
        );
        assert_eq!(
            x86_byte_register(7, 0),
            Some(X86ByteRegister { gpr: 3, high: true })
        );
    }

    #[test]
    fn byte_register_decodes_rex_low_bytes_including_spl() {
        assert_eq!(
            x86_byte_register(4, 0x40),
            Some(X86ByteRegister {
                gpr: 4,
                high: false
            })
        );
        assert_eq!(
            x86_byte_register(4, 0x44),
            Some(X86ByteRegister {
                gpr: 12,
                high: false
            })
        );
        assert_eq!(
            x86_byte_register(5, 0x41),
            Some(X86ByteRegister {
                gpr: 5,
                high: false
            })
        );
        assert_eq!(
            x86_byte_register(1, 0x44),
            Some(X86ByteRegister {
                gpr: 9,
                high: false
            })
        );
    }

    #[test]
    fn byte_register_value_extracts_high_and_low_bytes() {
        let ah = X86ByteRegister { gpr: 0, high: true };
        assert_eq!(x86_byte_register_value(0x1234_5678_9abc_def0, ah), 0xde);
        let al = X86ByteRegister {
            gpr: 0,
            high: false,
        };
        assert_eq!(x86_byte_register_value(0x1234_5678_9abc_def0, al), 0xf0);
        let ch = X86ByteRegister { gpr: 1, high: true };
        assert_eq!(x86_byte_register_value(0x1234_5678_9abc_def0, ch), 0xde);
    }

    #[test]
    fn byte_register_merge_preserves_adjacent_bytes() {
        let ah = X86ByteRegister { gpr: 0, high: true };
        assert_eq!(
            x86_byte_register_merge(0x1234_5678_9abc_def0, ah, 0x11),
            0x1234_5678_9abc_11f0
        );
        let al = X86ByteRegister {
            gpr: 0,
            high: false,
        };
        assert_eq!(
            x86_byte_register_merge(0x1234_5678_9abc_def0, al, 0x11),
            0x1234_5678_9abc_de11
        );
        let bpl = X86ByteRegister {
            gpr: 5,
            high: false,
        };
        assert_eq!(
            x86_byte_register_merge(0x1234_5678_9abc_def0, bpl, 0x11),
            0x1234_5678_9abc_de11
        );
    }

    #[test]
    fn simple_prefix_update_clears_rex_after_operand_size_override() {
        let mut rex = 0x40;
        let mut operand_size_override = false;

        assert!(x86_simple_prefix_update(
            0x66,
            &mut rex,
            &mut operand_size_override
        ));
        assert_eq!(rex, 0);
        assert!(operand_size_override);

        assert!(x86_simple_prefix_update(
            0x40,
            &mut rex,
            &mut operand_size_override
        ));
        assert_eq!(rex, 0x40);
        assert!(operand_size_override);
    }

    #[test]
    fn simple_prefix_update_keeps_last_rex_before_opcode() {
        let mut rex = 0;
        let mut operand_size_override = false;

        assert!(x86_simple_prefix_update(
            0x66,
            &mut rex,
            &mut operand_size_override
        ));
        assert!(x86_simple_prefix_update(
            0x48,
            &mut rex,
            &mut operand_size_override
        ));
        assert_eq!(rex, 0x48);

        assert!(x86_simple_prefix_update(
            0x66,
            &mut rex,
            &mut operand_size_override
        ));
        assert_eq!(rex, 0);
    }

    #[test]
    fn modrm_displacement_size_handles_sib_rip_relative_and_r13() {
        assert_eq!(x86_modrm_displacement_size(0x04, Some(0x25), 0), Some(4));
        assert_eq!(x86_modrm_displacement_size(0x04, Some(0x25), 1), Some(0));
        assert_eq!(x86_modrm_displacement_size(0x05, None, 0), Some(4));
        assert_eq!(x86_modrm_displacement_size(0x05, None, 1), Some(0));
        assert_eq!(x86_modrm_displacement_size(0x40, None, 0), Some(1));
        assert_eq!(x86_modrm_displacement_size(0x80, None, 0), Some(4));
        assert_eq!(x86_modrm_displacement_size(0xc0, None, 0), None);
    }

    #[test]
    fn mov_mmio_write_exit_decodes_byte_word_dword_widths() {
        let addr = X86GuestPhysAddr::from_usize(0x8000_0014);
        let cases: [(u8, bool, bool, X86AccessWidth, u64); 3] = [
            (0x88, false, false, X86AccessWidth::Byte, 0x5a),
            (0x89, true, false, X86AccessWidth::Word, 0x5678),
            (0x89, false, false, X86AccessWidth::Dword, 0x1234_5678),
        ];

        for (opcode, operand_size_override, rex_w, width, expected_data) in cases {
            let data = match width {
                X86AccessWidth::Byte => 0x1234_5678_5a,
                X86AccessWidth::Word => 0x1234_5678,
                X86AccessWidth::Dword => 0x1234_5678,
                X86AccessWidth::Qword => unreachable!(),
            };
            let exit =
                mov_mmio_write_exit(addr, opcode, operand_size_override, rex_w, data).unwrap();

            match exit {
                X86VmExit::MmioWrite {
                    addr: actual_addr,
                    width: actual_width,
                    data: actual_data,
                } => {
                    assert_eq!(actual_addr, addr);
                    assert_eq!(actual_width, width);
                    assert_eq!(actual_data, expected_data);
                }
                other => panic!("unexpected MMIO exit: {other:?}"),
            }
        }
    }
}
