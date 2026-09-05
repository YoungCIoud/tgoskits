use core::sync::atomic::{AtomicU64, Ordering};

use crate::{lock::SpinMutex as Mutex, *};

const MASTER_COMMAND: u16 = 0x20;
const MASTER_DATA: u16 = 0x21;
const SLAVE_COMMAND: u16 = 0xa0;
const SLAVE_DATA: u16 = 0xa1;
const CASCADE_IRQ: u8 = 2;

static PIC_READ_COUNT: AtomicU64 = AtomicU64::new(0);
static PIC_WRITE_COUNT: AtomicU64 = AtomicU64::new(0);
static PIC_EOI_COUNT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug)]
struct PicChip {
    vector_base: u8,
    mask: u8,
    request: u8,
    in_service: u8,
    init_step: u8,
    needs_icw4: bool,
    single: bool,
    auto_eoi: bool,
    read_isr: bool,
}

impl PicChip {
    const fn new(vector_base: u8) -> Self {
        Self {
            vector_base,
            mask: u8::MAX,
            request: 0,
            in_service: 0,
            init_step: 0,
            needs_icw4: false,
            single: false,
            auto_eoi: false,
            read_isr: false,
        }
    }

    fn command(&mut self, value: u8) {
        if value & 0x10 != 0 {
            self.request = 0;
            self.in_service = 0;
            self.init_step = 1;
            self.needs_icw4 = value & 0x01 != 0;
            self.single = value & 0x02 != 0;
            self.auto_eoi = false;
            self.read_isr = false;
            return;
        }

        if value & 0x18 == 0x08 {
            self.read_isr = value & 0x02 != 0;
            return;
        }

        if value & 0x20 != 0 {
            if value & 0x40 != 0 {
                self.in_service &= !(1 << (value & 0x07));
            } else if let Some(irq) = highest_priority(self.in_service) {
                self.in_service &= !(1 << irq);
            }
        }
    }

    fn data(&mut self, value: u8) {
        match self.init_step {
            1 => {
                self.vector_base = value & 0xf8;
                self.init_step = if self.single {
                    u8::from(self.needs_icw4) * 3
                } else {
                    2
                };
            }
            2 => {
                self.init_step = if self.needs_icw4 { 3 } else { 0 };
            }
            3 => {
                self.auto_eoi = value & 0x02 != 0;
                self.init_step = 0;
            }
            _ => self.mask = value,
        }
    }

    fn read_command(&self) -> u8 {
        if self.read_isr {
            self.in_service
        } else {
            self.request
        }
    }

    fn pulse(&mut self, irq: u8) {
        self.request |= 1 << irq;
    }

    fn pending_irq(&self) -> Option<u8> {
        let irq = highest_priority(self.request & !self.mask)?;
        let Some(in_service) = highest_priority(self.in_service) else {
            return Some(irq);
        };
        (irq < in_service).then_some(irq)
    }

    fn acknowledge(&mut self, irq: u8) -> u8 {
        self.request &= !(1 << irq);
        if !self.auto_eoi {
            self.in_service |= 1 << irq;
        }
        self.vector_base.wrapping_add(irq)
    }
}

fn highest_priority(bits: u8) -> Option<u8> {
    (bits != 0).then(|| bits.trailing_zeros() as u8)
}

#[derive(Clone, Copy, Debug)]
struct PicState {
    master: PicChip,
    slave: PicChip,
}

impl PicState {
    const fn new() -> Self {
        Self {
            master: PicChip::new(0x08),
            slave: PicChip::new(0x70),
        }
    }

    fn claim_irq(&mut self, irq: u8) -> Option<PicInterruptClaim> {
        if irq < 8 {
            self.master.pulse(irq);
        } else if irq < 16 {
            self.slave.pulse(irq - 8);
            self.master.pulse(CASCADE_IRQ);
        } else {
            return None;
        }
        self.claim_pending_interrupt()
    }

    fn claim_pending_interrupt(&mut self) -> Option<PicInterruptClaim> {
        let master_irq = self.master.pending_irq()?;
        if master_irq != CASCADE_IRQ {
            let master_in_service = !self.master.auto_eoi;
            return Some(PicInterruptClaim {
                vector: self.master.acknowledge(master_irq),
                irq: master_irq,
                master_in_service,
                slave_in_service: false,
            });
        }

        let Some(slave_irq) = self.slave.pending_irq() else {
            let master_in_service = !self.master.auto_eoi;
            return Some(PicInterruptClaim {
                vector: self.master.acknowledge(master_irq),
                irq: master_irq,
                master_in_service,
                slave_in_service: false,
            });
        };
        let master_in_service = !self.master.auto_eoi;
        let slave_in_service = !self.slave.auto_eoi;
        self.master.acknowledge(master_irq);
        Some(PicInterruptClaim {
            vector: self.slave.acknowledge(slave_irq),
            irq: slave_irq + 8,
            master_in_service,
            slave_in_service,
        })
    }

    fn restore_interrupt(&mut self, claim: PicInterruptClaim) {
        if claim.irq < 8 {
            self.master.pulse(claim.irq);
            if claim.master_in_service {
                self.master.in_service &= !(1 << claim.irq);
            }
            return;
        }

        let slave_irq = claim.irq - 8;
        self.slave.pulse(slave_irq);
        self.master.pulse(CASCADE_IRQ);
        if claim.slave_in_service {
            self.slave.in_service &= !(1 << slave_irq);
        }
        if claim.master_in_service {
            self.master.in_service &= !(1 << CASCADE_IRQ);
        }
    }
}

/// A pending legacy PIC interrupt removed from the IRR for delivery.
///
/// The claim can be restored when the runtime cannot publish its vector. It is
/// intentionally neither [`Copy`] nor [`Clone`] so one claim cannot be restored
/// more than once through safe code.
#[derive(Debug, Eq, PartialEq)]
pub struct PicInterruptClaim {
    vector: u8,
    irq: u8,
    master_in_service: bool,
    slave_in_service: bool,
}

impl PicInterruptClaim {
    /// Returns the guest-programmed interrupt vector owned by this claim.
    pub const fn vector(&self) -> u8 {
        self.vector
    }
}

/// Guest-owned pair of legacy 8259-compatible interrupt controllers.
pub struct EmulatedPic {
    state: Mutex<PicState>,
    claim_attempt_count: AtomicU64,
    claim_no_result_count: AtomicU64,
}

impl EmulatedPic {
    /// Creates the reset-compatible master and slave PIC state.
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(PicState::new()),
            claim_attempt_count: AtomicU64::new(0),
            claim_no_result_count: AtomicU64::new(0),
        }
    }

    /// Returns the two standard command/data port ranges.
    pub const fn port_ranges() -> [X86PortRange; 2] {
        [
            X86PortRange::new(X86Port::new(MASTER_COMMAND), X86Port::new(MASTER_DATA)),
            X86PortRange::new(X86Port::new(SLAVE_COMMAND), X86Port::new(SLAVE_DATA)),
        ]
    }

    /// Latches an edge on one legacy IRQ and returns an immediately deliverable vector.
    pub fn pulse_irq(&self, irq: u8) -> Option<u8> {
        self.claim_irq(irq).map(|claim| claim.vector())
    }

    /// Re-evaluates the latched requests after a guest PIC state change.
    ///
    /// In particular, an EOI or an interrupt-mask update can make an edge that
    /// was already present in the IRR deliverable without another source edge.
    pub fn next_interrupt(&self) -> Option<u8> {
        self.claim_pending_interrupt().map(|claim| claim.vector())
    }

    /// Latches one legacy IRQ edge and claims an immediately deliverable interrupt.
    pub fn claim_irq(&self, irq: u8) -> Option<PicInterruptClaim> {
        let mut state = self.state.lock();
        let claim = state.claim_irq(irq);
        let attempts = self.claim_attempt_count.fetch_add(1, Ordering::Relaxed) + 1;
        if attempts == 1 || attempts.is_power_of_two() {
            info!(
                "[HV] PIC claim sample: attempts={} irq={} result={:?} master={:?} slave={:?}",
                attempts,
                irq,
                claim.as_ref().map(PicInterruptClaim::vector),
                state.master,
                state.slave,
            );
        }
        if claim.is_none() {
            let no_result = self.claim_no_result_count.fetch_add(1, Ordering::Relaxed) + 1;
            if no_result == 1 || no_result.is_power_of_two() {
                info!(
                    "[HV] PIC claim unavailable: no_result={} irq={} master={:?} slave={:?}",
                    no_result, irq, state.master, state.slave,
                );
            }
        }
        claim
    }

    /// Claims a request that became deliverable after a guest PIC state change.
    pub fn claim_pending_interrupt(&self) -> Option<PicInterruptClaim> {
        self.state.lock().claim_pending_interrupt()
    }

    /// Restores a claimed interrupt after publication to the vCPU failed.
    pub fn restore_interrupt(&self, claim: PicInterruptClaim) {
        self.state.lock().restore_interrupt(claim);
    }

    /// Handles one byte-wide PIC port read.
    pub fn handle_read(&self, port: X86Port, width: X86AccessWidth) -> X86VlapicResult<usize> {
        if width != X86AccessWidth::Byte {
            return Err(X86VlapicError::Unsupported);
        }
        let (value, master, slave) = {
            let state = self.state.lock();
            let value = match port.number() {
                MASTER_COMMAND => state.master.read_command(),
                MASTER_DATA => state.master.mask,
                SLAVE_COMMAND => state.slave.read_command(),
                SLAVE_DATA => state.slave.mask,
                _ => return Err(X86VlapicError::Unsupported),
            };
            (value, state.master, state.slave)
        };
        let reads = PIC_READ_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if reads <= 16 || reads.is_power_of_two() {
            info!(
                "[HV] PIC read: count={} port={:#x} value={:#x} master={:?} slave={:?}",
                reads,
                port.number(),
                value,
                master,
                slave,
            );
        }
        Ok(value as usize)
    }

    /// Handles one byte-wide PIC port write.
    pub fn handle_write(
        &self,
        port: X86Port,
        width: X86AccessWidth,
        value: usize,
    ) -> X86VlapicResult {
        if width != X86AccessWidth::Byte {
            return Err(X86VlapicError::Unsupported);
        }
        let value = value as u8;
        let (before_master, before_slave, after_master, after_slave) = {
            let mut state = self.state.lock();
            let before_master = state.master;
            let before_slave = state.slave;
            match port.number() {
                MASTER_COMMAND => state.master.command(value),
                MASTER_DATA => state.master.data(value),
                SLAVE_COMMAND => state.slave.command(value),
                SLAVE_DATA => state.slave.data(value),
                _ => return Err(X86VlapicError::Unsupported),
            }
            (before_master, before_slave, state.master, state.slave)
        };
        let writes = PIC_WRITE_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if writes <= 16 || writes.is_power_of_two() {
            info!(
                "[HV] PIC write: count={} port={:#x} value={value:#x} \
                 before_master={before_master:?} before_slave={before_slave:?} \
                 after_master={after_master:?} after_slave={after_slave:?}",
                writes,
                port.number(),
            );
        }
        if matches!(port.number(), MASTER_COMMAND | SLAVE_COMMAND) && value & 0x20 != 0 {
            let eois = PIC_EOI_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
            if eois <= 8 || eois.is_power_of_two() {
                info!(
                    "[HV] PIC EOI: count={} port={:#x} value={value:#x} master={after_master:?} \
                     slave={after_slave:?}",
                    eois,
                    port.number(),
                );
            }
        }
        Ok(())
    }
}

impl Default for EmulatedPic {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(pic: &EmulatedPic, port: u16, value: u8) {
        pic.handle_write(X86Port::new(port), X86AccessWidth::Byte, value as usize)
            .unwrap();
    }

    #[test]
    fn firmware_can_reprogram_and_service_pit_irq0() {
        let pic = EmulatedPic::new();
        write(&pic, MASTER_COMMAND, 0x11);
        write(&pic, MASTER_DATA, 0x68);
        write(&pic, MASTER_DATA, 0x04);
        write(&pic, MASTER_DATA, 0x01);
        write(&pic, MASTER_DATA, 0xfe);

        assert_eq!(pic.pulse_irq(0), Some(0x68));
        assert_eq!(pic.pulse_irq(0), None);
        write(&pic, MASTER_COMMAND, 0x20);
        let claim = pic.claim_pending_interrupt().unwrap();
        assert_eq!(claim.vector(), 0x68);
        pic.restore_interrupt(claim);
        assert_eq!(pic.next_interrupt(), Some(0x68));
    }
}
