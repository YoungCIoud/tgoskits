// Copyright 2025 The Axvisor Team
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

use core::ptr::NonNull;

use bit::BitIndex;
use tock_registers::interfaces::{ReadWriteable, Readable, Writeable};

pub use crate::regs::lvt::LVT_TIMER::TimerMode::Value as TimerMode;
use crate::{
    X86AccessWidth, X86HostPhysAddr, X86VcpuId, X86VlapicError, X86VlapicResult, X86VmId,
    consts::{
        APIC_LVT_DS, APIC_LVT_M, APIC_LVT_VECTOR, ApicRegOffset, LAPIC_TRIG_EDGE,
        RESET_SPURIOUS_INTERRUPT_VECTOR, xapic::DEFAULT_APIC_BASE,
    },
    host::{self, PhysFrame, X86VlapicHostOps},
    regs::{
        APIC_BASE, ApicBaseRegisterMsr,
        DESTINATION_FORMAT::{self, Model::Value as APICDestinationFormat},
        ERROR_STATUS, ErrorStatusRegisterLocal, ErrorStatusRegisterValue, INTERRUPT_COMMAND_HIGH,
        INTERRUPT_COMMAND_LOW::{
            self, DeliveryMode::Value as APICDeliveryMode,
            DestinationShorthand::Value as APICDestination,
        },
        InterruptCommandRegisterLowLocal, LocalAPICRegs, SPURIOUS_INTERRUPT_VECTOR,
        SpuriousInterruptVectorRegisterLocal,
        lvt::{
            LVT_CMCI, LVT_ERROR, LVT_LINT0, LVT_LINT1, LVT_PERFORMANCE_COUNTER,
            LVT_THERMAL_MONITOR, LVT_TIMER, LocalVectorTable,
        },
    },
    timer::ApicTimer,
    utils::fls32,
};

const APIC_BASE_ENABLE: u64 = 1 << 11;
const APIC_BASE_X2APIC_ENABLE: u64 = 1 << 10;
const APIC_BASE_BSP: u64 = 1 << 8;
const APIC_VERSION_INTEGRATED: u32 = 0x14;
const APIC_VERSION_MAX_LVT_ENTRIES: u32 = 6 << 16;

/// Virtual-APIC Registers.
pub struct VirtualApicRegs<H: X86VlapicHostOps> {
    /// The virtual-APIC page is a 4-KByte region of memory
    /// that the processor uses to virtualize certain accesses to APIC registers and to manage virtual interrupts.
    /// The physical address of the virtual-APIC page is the virtual-APIC address,
    /// a 64-bit VM-execution control field in the VMCS (see Section 25.6.8).
    virtual_lapic: NonNull<LocalAPICRegs>,

    /// Todo: distinguish between APIC ID and vCPU ID.
    vapic_id: u32,
    esr_pending: ErrorStatusRegisterLocal,
    esr_firing: i32,

    virtual_timer: ApicTimer<H>,

    /// Vector number for the highest priority bit that is set in the ISR
    isrv: u32,

    apic_base: ApicBaseRegisterMsr,

    /// Copies of some registers in the virtual APIC page,
    /// to be able to detect what changed (e.g. svr_last)
    svr_last: SpuriousInterruptVectorRegisterLocal,
    /// Copies of some registers in the virtual APIC page,
    /// to maintain a coherent snapshot of the register (e.g. lvt_last)
    lvt_last: LocalVectorTable,
    apic_page: PhysFrame<H>,
}

impl<H: X86VlapicHostOps> VirtualApicRegs<H> {
    /// Create new virtual-APIC registers by allocating a 4-KByte page for the virtual-APIC page.
    pub fn new(vm_id: X86VmId, vcpu_id: X86VcpuId) -> Self {
        let apic_frame = PhysFrame::<H>::alloc_zero().expect("allocate virtual-APIC page failed");
        let regs = Self {
            // virtual-APIC ID is the same as the VCPU ID.
            vapic_id: vcpu_id as _,
            esr_pending: ErrorStatusRegisterLocal::new(0),
            esr_firing: 0,
            virtual_lapic: NonNull::new(apic_frame.as_mut_ptr().cast()).unwrap(),
            apic_page: apic_frame,
            svr_last: SpuriousInterruptVectorRegisterLocal::new(RESET_SPURIOUS_INTERRUPT_VECTOR),
            lvt_last: LocalVectorTable::default(),
            isrv: 0,
            apic_base: ApicBaseRegisterMsr::new(
                DEFAULT_APIC_BASE as u64
                    | APIC_BASE_ENABLE
                    | if vcpu_id == 0 { APIC_BASE_BSP } else { 0 },
            ),
            virtual_timer: ApicTimer::<H>::new(vm_id, vcpu_id),
        };
        regs.regs().ID.set((vcpu_id as u32) << 24);
        regs.regs()
            .VERSION
            .set(APIC_VERSION_INTEGRATED | APIC_VERSION_MAX_LVT_ENTRIES);
        regs
    }

    const fn regs(&self) -> &LocalAPICRegs {
        unsafe { self.virtual_lapic.as_ref() }
    }

    /// Virtual-APIC address (64 bits).
    /// This field contains the physical address of the 4-KByte virtual-APIC page.
    /// The processor uses the virtual-APIC page to virtualize certain accesses to APIC registers and to manage virtual interrupts;
    /// see Chapter 30.
    pub fn virtual_apic_page_addr(&self) -> X86HostPhysAddr {
        self.apic_page.start_paddr()
    }

    /// Gets the APIC base MSR value.
    #[allow(dead_code)]
    pub fn apic_base(&self) -> u64 {
        self.apic_base.get()
    }

    /// Sets the APIC base MSR value.
    pub fn set_apic_base(&mut self, value: u64) -> X86VlapicResult {
        if value & APIC_BASE_X2APIC_ENABLE != 0 && value & APIC_BASE_ENABLE == 0 {
            return Err(X86VlapicError::InvalidInput);
        }

        self.apic_base.set(value);
        Ok(())
    }

    /// Returns whether the x2APIC mode is enabled.
    pub fn is_x2apic_enabled(&self) -> bool {
        self.apic_base.is_set(APIC_BASE::XAPIC_ENABLED)
            && self.apic_base.is_set(APIC_BASE::X2APIC_Enabled)
    }

    /// Returns whether the xAPIC mode is enabled.
    #[allow(dead_code)]
    pub fn is_xapic_enabled(&self) -> bool {
        self.apic_base.is_set(APIC_BASE::XAPIC_ENABLED)
            && !self.apic_base.is_set(APIC_BASE::X2APIC_Enabled)
    }

    /// Returns the current timer mode.
    pub fn timer_mode(&self) -> X86VlapicResult<TimerMode> {
        self.regs()
            .LVT_TIMER
            .read_as_enum(LVT_TIMER::TimerMode)
            .ok_or(X86VlapicError::InvalidData)
    }

    /// 30.1.4 EOI Virtualization
    /// IF any bits set in VISR
    ///     THEN SVI := highest index of bit set in VISR
    ///     ELSE SVI := 0;
    /// FI;
    fn find_isrv(&self) -> u32 {
        let mut isrv = 0;
        // i ranges effectively from 7 to 1
        for i in (1..8).rev() {
            let val = self.regs().ISR[i].get() as u32;
            if val != 0 {
                isrv = ((i as u32) << 5) | fls32(val) as u32;
                break;
            }
        }

        isrv
    }

    fn update_ppr(&mut self) {
        let isrv = self.isrv;
        let tpr = self.regs().TPR.get();
        // IF VTPR[7:4] ≥ SVI[7:4]
        let ppr = if prio(tpr) >= prio(isrv) {
            // THEN VPPR := VTPR & FFH;
            tpr
        } else {
            // ELSE VPPR := SVI & F0H;
            isrv & 0xf0
        };
        self.regs().PPR.set(ppr as _);
    }

    /// Process the EOI operation triggered by a write to the EOI register.
    /// 11.8.5 Signaling Interrupt Servicing Completion
    /// 30.1.4 EOI Virtualization
    fn process_eoi(&mut self) -> Option<u8> {
        let vector = self.isrv;

        if vector == 0 {
            return None;
        }

        let (idx, bitpos) = extract_index_and_bitpos_u32(vector);

        // Upon receiving an EOI, the APIC clears the highest priority bit in the ISR
        // and dispatches the next highest priority interrupt to the processor.

        // VISR[Vector] := 0; (see Section 30.1.1 for definition of VISR)
        let mut isr = self.regs().ISR[idx].get();
        isr &= !(1 << bitpos);
        self.regs().ISR[idx].set(isr);

        // IF any bits set in VISR
        // THEN SVI := highest index of bit set in VISR
        // ELSE SVI := 0;
        self.isrv = self.find_isrv();

        // perform PPR virtualiation (see Section 30.1.3);
        self.update_ppr();

        // The trigger mode register (TMR) indicates the trigger mode of the interrupt (see Figure 11-20).
        // Upon acceptance of an interrupt into the IRR, the corresponding TMR bit is cleared for edge-triggered interrupts and set for leveltriggered interrupts.
        // If a TMR bit is set when an EOI cycle for its corresponding interrupt vector is generated, an EOI message is sent to all I/O APICs.
        // (see 11.8.4 Interrupt Acceptance for Fixed Interrupts)
        let broadcast_eoi = (self.regs().TMR[idx].get() as u32).bit(bitpos)
            && !self
                .regs()
                .SVR
                .is_set(SPURIOUS_INTERRUPT_VECTOR::EOIBroadcastSuppression);

        debug!("Gratuitous EOI vector: {vector:#010X}");
        broadcast_eoi.then_some(vector as u8)
    }

    /// Post an interrupt to the vcpu running on 'hostcpu'.
    /// This will use a hardware assist if available (e.g. Posted Interrupt)
    /// or fall back to sending an 'ipinum' to interrupt the 'hostcpu'.
    fn set_err(&mut self, mask: ErrorStatusRegisterValue) {
        self.esr_pending.modify(mask);

        self.esr_firing = 1;
        if self.esr_firing == 0 {
            self.esr_firing = 1;
            let _lvt = self.regs().LVT_ERROR.get();
            //  if ((lvt & APIC_LVT_M) == 0U) {
            //     vec = lvt & APIC_LVT_VECTOR;
            //     if (vec >= 16U) {
            //         vlapic_accept_intr(vlapic, vec, LAPIC_TRIG_EDGE);
            //     }
            // }
            unimplemented!("vlapic_accept_intr(vlapic, vec, LAPIC_TRIG_EDGE)");
            // self.esr_firing = 0;
        }
    }

    fn is_dest_field_matched(&self, dest: u32) -> X86VlapicResult<bool> {
        let mut ret = false;

        let ldr = self.regs().LDR.get();

        if self.is_x2apic_enabled() {
            return Ok(true);
        } else {
            match self
                .regs()
                .DFR
                .read_as_enum::<APICDestinationFormat>(DESTINATION_FORMAT::Model)
                .ok_or(X86VlapicError::InvalidData)?
            {
                APICDestinationFormat::Flat => {
                    // In the "Flat Model" the MDA is interpreted as an 8-bit wide
                    // bitmask. This model is available in the xAPIC mode only.
                    let logical_id = ldr >> 24;
                    let dest_logical_id = dest & 0xff;
                    if logical_id & dest_logical_id != 0 {
                        ret = true;
                    }
                }
                APICDestinationFormat::Cluster => {
                    // In the "Cluster Model" the MDA is used to identify a
                    // specific cluster and a set of APICs in that cluster.
                    let logical_id = (ldr >> 24) & 0xf;
                    let cluster_id = ldr >> 28;
                    let dest_logical_id = dest & 0xf;
                    let dest_cluster_id = (dest >> 4) & 0xf;
                    if (cluster_id == dest_cluster_id) && ((logical_id & dest_logical_id) != 0) {
                        ret = true;
                    }
                }
            }
        }
        Ok(ret)
    }

    /// This function populates 'dmask' with the set of vcpus that match the
    /// addressing specified by the (dest, phys, lowprio) tuple.
    fn calculate_dest_no_shorthand(
        &self,
        is_broadcast: bool,
        dest: u32,
        is_phys: bool,
        lowprio: bool,
    ) -> X86VlapicResult<u64> {
        let mut dmask = 0;

        if is_broadcast {
            // Broadcast in both logical and physical modes.
            dmask = host::current_vm_active_vcpus::<H>() as u64;
        } else if is_phys {
            // Physical mode: "dest" is local APIC ID.
            // Todo: distinguish between APIC ID and vCPU ID.
            dmask = 1 << dest;
        } else if lowprio {
            // lowprio is not supported.
            // Refer to 11.6.2.4 Lowest Priority Delivery Mode.
            unimplemented!("lowprio");
        } else {
            // Logical mode: "dest" is message destination addr
            // to be compared with the logical APIC ID in LDR.

            let vcpu_mask = host::active_vcpus::<H>(H::current_vm_id()).unwrap();
            for i in 0..host::current_vm_vcpu_num::<H>() {
                if vcpu_mask & (1 << i) != 0 {
                    if !self.is_dest_field_matched(dest)? {
                        continue;
                    }
                    dmask |= 1 << i;
                }
            }
        }

        Ok(dmask)
    }

    fn calculate_dest(
        &self,
        shorthand: APICDestination,
        is_broadcast: bool,
        dest: u32,
        is_phys: bool,
        lowprio: bool,
    ) -> X86VlapicResult<u64> {
        let mut dmask = 0;
        match shorthand {
            APICDestination::NoShorthand => {
                dmask = self.calculate_dest_no_shorthand(is_broadcast, dest, is_phys, lowprio)?;
            }
            APICDestination::SELF => {
                dmask.set_bit(self.vapic_id as usize, true);
            }
            APICDestination::AllIncludingSelf => {
                dmask = host::current_vm_active_vcpus::<H>() as u64;
            }
            APICDestination::AllExcludingSelf => {
                dmask = host::current_vm_active_vcpus::<H>() as u64;
                dmask &= !(1 << self.vapic_id);
            }
        }

        Ok(dmask)
    }

    fn handle_self_ipi(&mut self, vector: u32) {
        if vector < 16 {
            self.set_err(ERROR_STATUS::SendIllegalVector::SET);
            warn!("[VLAPIC] ignoring illegal self IPI vector {vector:#x}");
            return;
        }

        self.set_intr(self.vapic_id, vector, LAPIC_TRIG_EDGE);
    }

    fn set_intr(&mut self, vcpu_id: u32, vector: u32, _level: bool) {
        if vector > u8::MAX as u32 {
            warn!("[VLAPIC] ignoring out-of-range vector {vector:#x}");
            return;
        }

        debug!("[VLAPIC] injecting vector {vector:#x} to vcpu {vcpu_id}");
        let _ = host::inject_interrupt::<H>(H::current_vm_id(), vcpu_id as usize, vector as u8);
    }

    /// Record interrupt acceptance in the virtual APIC page.
    pub fn accept_interrupt(&mut self, vector: u8, level_triggered: bool) {
        let vector = vector as u32;
        let (idx, bitpos) = extract_index_and_bitpos_u32(vector);
        let bit = 1 << bitpos;

        self.regs().ISR[idx].set(self.regs().ISR[idx].get() | bit);
        self.regs().TMR[idx].set(if level_triggered {
            self.regs().TMR[idx].get() | bit
        } else {
            self.regs().TMR[idx].get() & !bit
        });
        self.isrv = self.find_isrv();
        self.update_ppr();
    }

    fn inject_nmi(&mut self, vcpu_id: u32) {
        warn!("[VLAPIC] ignoring NMI IPI to vcpu {vcpu_id}: NMI injection is not implemented yet");
    }

    /// Handles an INIT delivery accepted by `write_icr`.
    ///
    /// Per Intel SDM Vol.3 §11.6.2 the INIT level de-assert form (trigger mode
    /// = level ∧ level = de-assert) is an arbitration synchronization message,
    /// not a reset; KVM applies the same rule (`!trig_mode || level`). Every
    /// other INIT encoding resets the targets, which this device models as a
    /// wait-for-SIPI request to the host. Failures are logged and dropped: an
    /// ICR write must never fail guest execution.
    fn process_init(&mut self, vcpu_id: u32, icr_low: InterruptCommandRegisterLowLocal) {
        let level_triggered = icr_low.is_set(INTERRUPT_COMMAND_LOW::TriggerMode);
        let level_asserted = icr_low.is_set(INTERRUPT_COMMAND_LOW::Level);
        if level_triggered && !level_asserted {
            debug!(
                "[VLAPIC] ignoring INIT level de-assert to vcpu {vcpu_id}: arbitration sync \
                 message"
            );
            return;
        }

        if vcpu_id == self.vapic_id {
            warn!(
                "[VLAPIC] ignoring INIT addressed to the sending vCPU {vcpu_id}: a running vCPU \
                 cannot reset itself"
            );
            return;
        }

        if let Err(err) = host::request_ap_init::<H>(H::current_vm_id(), vcpu_id as X86VcpuId) {
            warn!("[VLAPIC] host rejected wait-for-SIPI request for vcpu {vcpu_id}: {err:?}");
        }
    }

    /// Handles a SIPI delivery accepted by `write_icr` by asking the host to
    /// start the target in real mode at `CS = vector << 12`, `IP = 0`.
    ///
    /// Wait-for-SIPI gating is the host's decision; this layer only forwards
    /// protocol-exact requests so that unsupported forms stay logged-and-ignored
    /// without ever failing the writing guest.
    fn process_sipi(&mut self, vcpu_id: u32, icr_low: InterruptCommandRegisterLowLocal) {
        let start_vector = icr_low.read(INTERRUPT_COMMAND_LOW::Vector) as u8;

        if vcpu_id == self.vapic_id {
            warn!("[VLAPIC] ignoring SIPI addressed to the sending vCPU {vcpu_id}");
            return;
        }

        if let Err(err) =
            host::request_sipi_startup::<H>(H::current_vm_id(), vcpu_id as X86VcpuId, start_vector)
        {
            warn!("[VLAPIC] host rejected SIPI startup for vcpu {vcpu_id}: {err:?}");
        }
    }

    fn process_init_sipi(
        &mut self,
        vcpu_id: u32,
        mode: APICDeliveryMode,
        icr_low: InterruptCommandRegisterLowLocal,
    ) {
        match mode {
            APICDeliveryMode::INIT => self.process_init(vcpu_id, icr_low),
            APICDeliveryMode::StartUp => self.process_sipi(vcpu_id, icr_low),
            _ => unreachable!("write_icr dispatches only INIT and StartUp here"),
        }
    }

    /// Figure 11-13. Logical Destination Register (LDR)
    fn write_ldr(&mut self) {
        const LDR_RESERVED: u32 = 0x00ffffff;

        let mut ldr = self.regs().LDR.get();
        let apic_id = ldr >> 24;
        ldr &= !LDR_RESERVED;

        self.regs().LDR.set(ldr);
        debug!("[VLAPIC] apic_id={apic_id:#010X} write LDR register to {ldr:#010X}");
    }

    fn write_dfr(&mut self) {
        use crate::regs::DESTINATION_FORMAT;

        const APIC_DFR_RESERVED: u32 = 0x0fff_ffff;
        const APIC_DFR_MODEL_MASK: u32 = 0xf000_0000;

        let mut dfr = self.regs().DFR.get();
        dfr &= APIC_DFR_MODEL_MASK;
        dfr |= APIC_DFR_RESERVED;
        self.regs().DFR.set(dfr);

        debug!("[VLAPIC] write DFR register to {dfr:#010X}");

        match self.regs().DFR.read_as_enum(DESTINATION_FORMAT::Model) {
            Some(DESTINATION_FORMAT::Model::Value::Flat) => {
                debug!("[VLAPIC] DFR in Flat Model");
            }
            Some(DESTINATION_FORMAT::Model::Value::Cluster) => {
                debug!("[VLAPIC] DFR in Cluster Model");
            }
            None => {
                debug!("[VLAPIC] DFR in Unknown Model {dfr:#010X}");
            }
        }
    }

    /// Figure 11-14. Spurious-Interrupt Vector Register (SVR)
    /// Handle writes to the SVR register.
    fn write_svr(&mut self) -> X86VlapicResult {
        let new = self.regs().SVR.extract();
        let old = self.svr_last;

        self.svr_last = new;

        if old.is_set(SPURIOUS_INTERRUPT_VECTOR::APICSoftwareEnableDisable)
            && !new.is_set(SPURIOUS_INTERRUPT_VECTOR::APICSoftwareEnableDisable)
        {
            debug!("[VLAPIC] vlapic [{}] is software-disabled", self.vapic_id);
            // The apic is now disabled so stop the apic timer
            // and mask all the LVT entries.
            self.virtual_timer.stop_timer()?;
            self.mask_lvts()?;
            warn!("VM wire mode should be changed to INTR here, unimplemented");
        } else if !old.is_set(SPURIOUS_INTERRUPT_VECTOR::APICSoftwareEnableDisable)
            && new.is_set(SPURIOUS_INTERRUPT_VECTOR::APICSoftwareEnableDisable)
        {
            debug!("[VLAPIC] vlapic [{}] is software-enabled", self.vapic_id);

            // The apic is now enabled so restart the apic timer
            // if it is configured in periodic mode.
            if self.virtual_timer.is_periodic() {
                debug!("Restarting the apic timer");
                self.virtual_timer.restart_timer()?;
            }
        }

        Ok(())
    }

    fn write_esr(&mut self) {
        let esr = self.regs().ESR.get();
        debug!("[VLAPIC] write ESR register to {esr:#010X}");
        self.regs().ESR.set(self.esr_pending.get());
        self.esr_pending.set(0);
    }

    fn write_icr(&mut self) -> X86VlapicResult {
        self.regs()
            .ICR_LO
            .modify(INTERRUPT_COMMAND_LOW::DeliveryStatus::Idle);

        let icr_low = self.regs().ICR_LO.extract();

        let (dest, is_broadcast) = if self.is_x2apic_enabled() {
            use crate::consts::x2apic::X2APIC_BROADCAST_DEST_ID;
            let dest = self.regs().ICR_HI.get();
            (dest, dest == X2APIC_BROADCAST_DEST_ID)
        } else {
            use crate::consts::xapic::XAPIC_BROADCAST_DEST_ID;
            let dest = self.regs().ICR_HI.read(INTERRUPT_COMMAND_HIGH::Destination);
            (dest, dest == XAPIC_BROADCAST_DEST_ID)
        };

        let vec = icr_low.read(INTERRUPT_COMMAND_LOW::Vector);
        let mode = icr_low
            .read_as_enum::<APICDeliveryMode>(INTERRUPT_COMMAND_LOW::DeliveryMode)
            .ok_or(X86VlapicError::InvalidData)?;
        let is_phys = !icr_low.is_set(INTERRUPT_COMMAND_LOW::DestinationMode);
        let shorthand = icr_low
            .read_as_enum::<APICDestination>(INTERRUPT_COMMAND_LOW::DestinationShorthand)
            .ok_or(X86VlapicError::InvalidData)?;

        if mode == APICDeliveryMode::Fixed && vec < 16 {
            self.set_err(ERROR_STATUS::SendIllegalVector::SET);
            debug!("[VLAPIC] Ignoring invalid IPI {vec:#010X}");
        } else if (shorthand == APICDestination::SELF
            || shorthand == APICDestination::AllIncludingSelf)
            && (mode == APICDeliveryMode::NMI
                || mode == APICDeliveryMode::INIT
                || mode == APICDeliveryMode::StartUp)
        {
            debug!("[VLAPIC] Invalid ICR value {vec:#010X}");
        } else {
            debug!(
                "icrlow {:#010X} icrhi {:#010X} triggered ipi {:#010X}",
                icr_low.get(),
                self.regs().ICR_HI.get(),
                vec
            );
            let dmask = self.calculate_dest(shorthand, is_broadcast, dest, is_phys, false)?;

            // TODO: we need to get the specific vcpu number somehow.
            for i in 0..host::current_vm_vcpu_num::<H>() as u32 {
                if dmask & (1 << i) != 0 {
                    match mode {
                        APICDeliveryMode::Fixed => {
                            self.set_intr(i, vec, LAPIC_TRIG_EDGE);
                            debug!("[VLAPIC] sending IPI {vec} to vcpu {i}");
                        }
                        APICDeliveryMode::NMI => {
                            self.inject_nmi(i);
                            debug!("[VLAPIC] sending NMI to vcpu {i}");
                        }
                        APICDeliveryMode::INIT | APICDeliveryMode::StartUp => {
                            // MP initialization only uses physical addressing or
                            // broadcast shorthands; logical-mode INIT/SIPI has no
                            // hardware meaning, so it stays a logged no-op.
                            if is_phys {
                                self.process_init_sipi(i, mode, icr_low);
                            } else {
                                warn!("[VLAPIC] ignoring logical-mode {mode:?} IPI to vcpu {i}");
                            }
                        }
                        APICDeliveryMode::SMI => {
                            warn!("[VLPAIC] SMI IPI do not support");
                        }
                        _ => {
                            error!("Unhandled icrlo write with mode {mode:?}\n");
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn extract_lvt_val(&self, offset: ApicRegOffset) -> u32 {
        match offset {
            ApicRegOffset::LvtCMCI => self.regs().LVT_CMCI.get(),
            ApicRegOffset::LvtTimer => self.regs().LVT_TIMER.get(),
            ApicRegOffset::LvtThermal => self.regs().LVT_THERMAL.get(),
            ApicRegOffset::LvtPmc => self.regs().LVT_PMI.get(),
            ApicRegOffset::LvtLint0 => self.regs().LVT_LINT0.get(),
            ApicRegOffset::LvtLint1 => self.regs().LVT_LINT1.get(),
            ApicRegOffset::LvtErr => self.regs().LVT_ERROR.get(),
            _ => {
                warn!("[VLAPIC] read unsupported APIC register: {offset:?}");
                0
            }
        }
    }

    fn write_lvt(&mut self, offset: ApicRegOffset) -> X86VlapicResult {
        let mut val = self.extract_lvt_val(offset);

        // Mask::Masked, Delivery Status:SendPending, Vector::SET(0xff)
        let mut mask = APIC_LVT_M | APIC_LVT_DS | APIC_LVT_VECTOR;

        match offset {
            ApicRegOffset::LvtTimer => {
                mask |= LVT_TIMER::TimerMode::SET.mask();
                val &= mask;
                self.regs().LVT_TIMER.set(val); // Duplicated, which one should be removed?
                self.lvt_last.lvt_timer.set(val);

                self.virtual_timer.write_lvt(val)?;
            }
            ApicRegOffset::LvtErr => {
                val &= mask;
                self.regs().LVT_ERROR.set(val);
                self.lvt_last.lvt_err.set(val);
            }
            ApicRegOffset::LvtLint0 => {
                mask |= LVT_LINT0::TriggerMode::SET.mask();
                mask |= LVT_LINT0::RemoteIRR::SET.mask();
                mask |= LVT_LINT0::InterruptInputPinPolarity::SET.mask();
                mask |= LVT_LINT0::DeliveryMode::SET.mask();
                val &= mask;

                // vlapic mask/unmask LINT0 for ExtINT?
                if (val & LVT_LINT0::DeliveryMode::SET.mask())
                    == LVT_LINT0::DeliveryMode::ExtINT.mask()
                {
                    let last = self.lvt_last.lvt_lint0;
                    if last.is_set(LVT_LINT0::Mask) && val & LVT_LINT0::Mask::SET.mask() == 0 {
                        // mask -> unmask: may from every vlapic in the vm
                        warn!("vpic wire mode change to LAPIC, unimplemented");
                    } else if !last.is_set(LVT_LINT0::Mask)
                        && val & LVT_LINT0::Mask::SET.mask() != 0
                    {
                        // unmask -> mask: only from the vlapic LINT0-ExtINT enabled
                        warn!("vpic wire mode change to NULL, unimplemented");
                    } else {
                        // APIC_LVT_M unchanged. No action required.
                    }
                }

                self.regs().LVT_LINT0.set(val);
                self.lvt_last.lvt_lint0.set(val);
            }
            ApicRegOffset::LvtLint1 => {
                mask |= LVT_LINT1::TriggerMode::SET.mask();
                mask |= LVT_LINT1::RemoteIRR::SET.mask();
                mask |= LVT_LINT1::InterruptInputPinPolarity::SET.mask();
                mask |= LVT_LINT1::DeliveryMode::SET.mask();
                val &= mask;

                self.regs().LVT_LINT1.set(val);
                self.lvt_last.lvt_lint1.set(val);
            }
            ApicRegOffset::LvtCMCI => {
                mask |= LVT_CMCI::DeliveryMode::SET.mask();
                val &= mask;
                self.regs().LVT_CMCI.set(val);
                self.lvt_last.lvt_cmci.set(val);
            }
            ApicRegOffset::LvtPmc => {
                mask |= LVT_PERFORMANCE_COUNTER::DeliveryMode::SET.mask();
                val &= mask;
                self.regs().LVT_PMI.set(val);
                self.lvt_last.lvt_perf_count.set(val);
            }
            ApicRegOffset::LvtThermal => {
                mask |= LVT_THERMAL_MONITOR::DeliveryMode::SET.mask();
                val &= mask;
                self.regs().LVT_THERMAL.set(val);
                self.lvt_last.lvt_thermal.set(val);
            }
            _ => {
                warn!("[VLAPIC] write unsupported APIC register: {offset:?}");
                return Err(X86VlapicError::InvalidInput);
            }
        }
        Ok(())
    }

    fn mask_lvts(&mut self) -> X86VlapicResult {
        self.regs().LVT_CMCI.modify(LVT_CMCI::Mask::SET);
        self.write_lvt(ApicRegOffset::LvtCMCI)?;

        self.regs().LVT_TIMER.modify(LVT_TIMER::Mask::SET);
        self.write_lvt(ApicRegOffset::LvtTimer)?;

        self.regs()
            .LVT_THERMAL
            .modify(LVT_THERMAL_MONITOR::Mask::SET);
        self.write_lvt(ApicRegOffset::LvtThermal)?;

        self.regs()
            .LVT_PMI
            .modify(LVT_PERFORMANCE_COUNTER::Mask::SET);
        self.write_lvt(ApicRegOffset::LvtPmc)?;

        self.regs().LVT_LINT0.modify(LVT_LINT0::Mask::SET);
        self.write_lvt(ApicRegOffset::LvtLint0)?;

        self.regs().LVT_LINT1.modify(LVT_LINT1::Mask::SET);
        self.write_lvt(ApicRegOffset::LvtLint1)?;

        self.regs().LVT_ERROR.modify(LVT_ERROR::Mask::SET);
        self.write_lvt(ApicRegOffset::LvtErr)?;

        Ok(())
    }

    fn write_icrtmr(&mut self) -> X86VlapicResult {
        self.virtual_timer.write_icr(self.regs().ICR_TIMER.get())
    }

    fn write_dcr(&mut self) -> X86VlapicResult {
        self.virtual_timer.write_dcr(self.regs().DCR_TIMER.get());
        Ok(())
    }
}

fn extract_index_u32(vector: u32) -> usize {
    vector as usize >> 5
}

fn extract_index_and_bitpos_u32(vector: u32) -> (usize, usize) {
    (extract_index_u32(vector), vector as usize & 0x1F)
}

/// Figure 11-18. Task-Priority Register (TPR)
/// [7:4]: Task-Priority Class
/// [3:0]: Task-Priority Sub-Class
fn prio(x: u32) -> u32 {
    (x >> 4) & 0xf
}

impl<H: X86VlapicHostOps> VirtualApicRegs<H> {
    pub fn handle_read(
        &self,
        offset: ApicRegOffset,
        width: X86AccessWidth,
    ) -> X86VlapicResult<usize> {
        let mut value: usize = 0;
        match offset {
            ApicRegOffset::ID => {
                value = self.regs().ID.get() as _;
            }
            ApicRegOffset::Version => {
                value = self.regs().VERSION.get() as _;
            }
            ApicRegOffset::TPR => {
                value = self.regs().TPR.get() as _;
            }
            ApicRegOffset::PPR => {
                value = self.regs().PPR.get() as _;
            }
            ApicRegOffset::EOI => {
                // value = self.regs().EOI.get() as _;
                warn!("[VLAPIC] read EOI register: {value:#010X}");
            }
            ApicRegOffset::LDR => {
                value = self.regs().LDR.get() as _;
            }
            ApicRegOffset::DFR => {
                value = self.regs().DFR.get() as _;
            }
            ApicRegOffset::SIVR => {
                value = self.regs().SVR.get() as _;
            }
            ApicRegOffset::ISR(index) => {
                value = self.regs().ISR[index.as_usize()].get() as _;
            }
            ApicRegOffset::TMR(index) => {
                value = self.regs().TMR[index.as_usize()].get() as _;
            }
            ApicRegOffset::IRR(index) => {
                value = self.regs().IRR[index.as_usize()].get() as _;
            }
            ApicRegOffset::ESR => {
                value = self.regs().ESR.get() as _;
            }
            ApicRegOffset::ICRLow => {
                value = self.regs().ICR_LO.get() as _;
                if self.is_x2apic_enabled() && width == X86AccessWidth::Qword {
                    let icr_hi = self.regs().ICR_HI.get() as usize;
                    value |= icr_hi << 32;
                    debug!("[VLAPIC] read ICR register: {value:#018X}");
                } else if self.is_x2apic_enabled() ^ (width == X86AccessWidth::Qword) {
                    warn!(
                        "[VLAPIC] Illegal read attempt of ICR register at width {:?} with X2APIC \
                         {}",
                        width,
                        if self.is_x2apic_enabled() {
                            "enabled"
                        } else {
                            "disabled"
                        }
                    );
                    return Err(X86VlapicError::InvalidInput);
                }
            }
            ApicRegOffset::ICRHi => {
                value = self.regs().ICR_HI.get() as _;
            }
            // Local Vector Table registers.
            ApicRegOffset::LvtCMCI => {
                value = self.lvt_last.lvt_cmci.get() as _;
            }
            ApicRegOffset::LvtTimer => {
                value = self.lvt_last.lvt_timer.get() as _;
            }
            ApicRegOffset::LvtThermal => {
                value = self.lvt_last.lvt_thermal.get() as _;
            }
            ApicRegOffset::LvtPmc => {
                value = self.lvt_last.lvt_perf_count.get() as _;
            }
            ApicRegOffset::LvtLint0 => {
                value = self.lvt_last.lvt_lint0.get() as _;
            }
            ApicRegOffset::LvtLint1 => {
                value = self.lvt_last.lvt_lint1.get() as _;
            }
            ApicRegOffset::LvtErr => {
                value = self.lvt_last.lvt_err.get() as _;
            }
            // Timer registers.
            ApicRegOffset::TimerInitCount => {
                match self.timer_mode() {
                    Ok(TimerMode::OneShot) | Ok(TimerMode::Periodic) => {
                        value = self.regs().ICR_TIMER.get() as _;
                    }
                    Ok(TimerMode::TSCDeadline) => {
                        // if TSCDEADLINE mode always return 0
                        value = 0;
                    }
                    _ => {
                        warn!("[VLAPIC] read TimerInitCount register: invalid timer mode");
                    }
                }
                debug!("[VLAPIC] read TimerInitCount register: {value:#010X}");
            }
            ApicRegOffset::TimerCurCount => {
                value = self.virtual_timer.read_ccr() as _;
            }
            ApicRegOffset::TimerDivConf => {
                value = self.regs().DCR_TIMER.get() as _;
            }
            _ => {
                warn!("[VLAPIC] read unknown APIC register: {offset:?}");
            }
        }
        debug!("[VLAPIC] read {offset} register: {value:#010X}");
        Ok(value)
    }

    pub fn handle_write(
        &mut self,
        offset: ApicRegOffset,
        val: usize,
        width: X86AccessWidth,
    ) -> X86VlapicResult {
        let data32 = val as u32;

        match offset {
            ApicRegOffset::ID => {
                // Force APIC ID to be read-only.
                // self.regs().ID.set(val as _);
            }
            ApicRegOffset::TPR => {
                self.regs().TPR.set(data32 & 0xff);
            }
            ApicRegOffset::EOI => {
                let _ = self.process_eoi();
            }
            ApicRegOffset::LDR => {
                self.regs().LDR.set(data32);
                self.write_ldr();
            }
            ApicRegOffset::DFR => {
                self.regs().DFR.set(data32);
                self.write_dfr();
            }
            ApicRegOffset::SIVR => {
                self.regs().SVR.set(data32);
                self.write_svr()?;
            }
            ApicRegOffset::ESR => {
                self.regs().ESR.set(data32);
                self.write_esr();
            }
            ApicRegOffset::ICRLow => {
                if self.is_x2apic_enabled() && width == X86AccessWidth::Qword {
                    debug!("[VLAPIC] write ICR register: {val:#018X} in X2APIC mode");
                    self.regs().ICR_HI.set((val >> 32) as u32);
                } else if self.is_x2apic_enabled() ^ (width == X86AccessWidth::Qword) {
                    warn!(
                        "[VLAPIC] Illegal read attempt of ICR register at width {:?} with X2APIC \
                         {}",
                        width,
                        if self.is_x2apic_enabled() {
                            "enabled"
                        } else {
                            "disabled"
                        }
                    );
                    return Err(X86VlapicError::InvalidInput);
                }
                self.regs().ICR_LO.set(data32);
                self.write_icr()?;
            }
            ApicRegOffset::ICRHi => {
                if self.is_x2apic_enabled() {
                    warn!("[VLAPIC] write ICRHi register: unsupported in x2APIC mode");
                    return Err(X86VlapicError::InvalidInput);
                }
                self.regs()
                    .ICR_HI
                    .set(data32 & INTERRUPT_COMMAND_HIGH::Destination::SET.mask());
            }
            // Local Vector Table registers.
            ApicRegOffset::LvtCMCI => {
                self.regs().LVT_CMCI.set(data32);
                self.write_lvt(offset)?;
            }
            ApicRegOffset::LvtTimer => {
                self.regs().LVT_TIMER.set(data32);
                self.write_lvt(offset)?;
            }
            ApicRegOffset::LvtThermal => {
                self.regs().LVT_THERMAL.set(data32);
                self.write_lvt(offset)?;
            }
            ApicRegOffset::LvtPmc => {
                self.regs().LVT_PMI.set(data32);
                self.write_lvt(offset)?;
            }
            ApicRegOffset::LvtLint0 => {
                self.regs().LVT_LINT0.set(data32);
                self.write_lvt(offset)?;
            }
            ApicRegOffset::LvtLint1 => {
                self.regs().LVT_LINT1.set(data32);
                self.write_lvt(offset)?;
            }
            ApicRegOffset::LvtErr => {
                self.regs().LVT_ERROR.set(data32);
                self.write_lvt(offset)?;
            }
            // Timer registers.
            ApicRegOffset::TimerInitCount => {
                // if TSCDEADLINE mode ignore icr_timer
                if self.timer_mode()? == TimerMode::TSCDeadline {
                    warn!(
                        "[VLAPIC] write TimerInitCount register: ignore icr_timer in TSCDEADLINE \
                         mode"
                    );
                    return Ok(());
                }
                self.regs().ICR_TIMER.set(data32);
                self.write_icrtmr()?;
            }
            ApicRegOffset::TimerDivConf => {
                self.regs().DCR_TIMER.set(data32);
                self.write_dcr()?;
            }
            ApicRegOffset::SelfIPI => {
                if self.is_x2apic_enabled() {
                    self.regs().SELF_IPI.set(data32);
                    self.handle_self_ipi(data32 & 0xff);
                } else {
                    warn!("[VLAPIC] write SelfIPI register: unsupported in xAPIC mode");
                    return Err(X86VlapicError::InvalidInput);
                }
            }
            _ => {
                warn!("[VLAPIC] write unsupported APIC register: {offset:?}");
                return Err(X86VlapicError::InvalidInput);
            }
        }

        debug!("[VLAPIC] write {offset} register: {val:#010X}");

        Ok(())
    }

    /// Process a guest EOI and return the vector that needs an IO APIC EOI broadcast.
    pub fn handle_eoi(&mut self) -> Option<u8> {
        self.process_eoi()
    }
}

#[cfg(test)]
mod tests {
    use alloc::{alloc::alloc_zeroed, vec, vec::Vec};
    use core::{
        alloc::Layout,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use crate::{
        host::X86_PAGE_SIZE_4K,
        lock::SpinMutex,
        types::{X86HostVirtAddr, X86InterruptVector, X86TimerCallback},
    };

    /// One CPU-up request observed at the host boundary.
    #[derive(Clone, Debug, Eq, PartialEq)]
    enum RecordedCpuUpRequest {
        ApInit {
            vm_id: X86VmId,
            vcpu_id: X86VcpuId,
        },
        SipiStartup {
            vm_id: X86VmId,
            vcpu_id: X86VcpuId,
            start_vector: u8,
        },
    }

    struct MockConfig {
        vm_id: X86VmId,
        vcpu_num: usize,
        active_vcpus: usize,
    }

    static NEXT_MOCK_FRAME: AtomicUsize = AtomicUsize::new(1);
    /// Serializes tests against the process-wide mock statics below: cargo runs
    /// tests in parallel threads, and each mock test reconfigures the host.
    static MOCK_TEST_LOCK: SpinMutex<()> = SpinMutex::new(());
    static MOCK_CONFIG: SpinMutex<MockConfig> = SpinMutex::new(MockConfig {
        vm_id: 7,
        vcpu_num: 2,
        active_vcpus: 0b11,
    });
    static CPU_UP_REQUESTS: SpinMutex<Vec<RecordedCpuUpRequest>> = SpinMutex::new(Vec::new());
    static INJECTED_VECTORS: SpinMutex<Vec<(X86VmId, X86VcpuId, X86InterruptVector)>> =
        SpinMutex::new(Vec::new());

    /// Locks the process-wide mock statics, reconfigures the host topology, and
    /// clears recorded requests. Hold the returned guard for the whole test.
    fn mock_session(
        vm_id: X86VmId,
        vcpu_num: usize,
        active_vcpus: usize,
    ) -> crate::lock::SpinMutexGuard<'static, ()> {
        let guard = MOCK_TEST_LOCK.lock();
        *MOCK_CONFIG.lock() = MockConfig {
            vm_id,
            vcpu_num,
            active_vcpus,
        };
        CPU_UP_REQUESTS.lock().clear();
        INJECTED_VECTORS.lock().clear();
        guard
    }

    fn recorded_cpu_up_requests() -> Vec<RecordedCpuUpRequest> {
        CPU_UP_REQUESTS.lock().clone()
    }

    fn injected_vectors() -> Vec<(X86VmId, X86VcpuId, X86InterruptVector)> {
        INJECTED_VECTORS.lock().clone()
    }

    /// Host stub whose identity phys↔virt mapping hands out leaked zeroed frames.
    ///
    /// Mirrors the `DummyHost` style used by the APIC timer tests. Frames are
    /// intentionally never freed: each test allocates one virtual-APIC page and
    /// the test process is short-lived.
    struct RecordingHost;

    impl host::X86VlapicHostOps for RecordingHost {
        fn alloc_frame() -> Option<X86HostPhysAddr> {
            let frame = unsafe {
                alloc_zeroed(
                    Layout::from_size_align(X86_PAGE_SIZE_4K, 4096).expect("valid 4 KiB layout"),
                )
                .cast::<u8>()
            };
            if frame.is_null() {
                return None;
            }
            NEXT_MOCK_FRAME.fetch_add(1, Ordering::Relaxed);
            Some(X86HostPhysAddr::from_usize(frame as usize))
        }

        fn dealloc_frame(_paddr: X86HostPhysAddr) {}

        fn phys_to_virt(paddr: X86HostPhysAddr) -> X86HostVirtAddr {
            X86HostVirtAddr::from_usize(paddr.as_usize())
        }

        fn virt_to_phys(vaddr: X86HostVirtAddr) -> X86HostPhysAddr {
            X86HostPhysAddr::from_usize(vaddr.as_usize())
        }

        fn current_time_nanos() -> u64 {
            0
        }

        fn register_timer(_deadline_nanos: u64, _callback: X86TimerCallback) -> Option<usize> {
            None
        }

        fn cancel_timer(_token: usize) {}

        fn current_vm_id() -> X86VmId {
            MOCK_CONFIG.lock().vm_id
        }

        fn current_vm_vcpu_num() -> usize {
            MOCK_CONFIG.lock().vcpu_num
        }

        fn current_vm_active_vcpus() -> usize {
            MOCK_CONFIG.lock().active_vcpus
        }

        fn active_vcpus(_vm_id: X86VmId) -> Option<usize> {
            Some(MOCK_CONFIG.lock().active_vcpus)
        }

        fn inject_interrupt(
            vm_id: X86VmId,
            vcpu_id: X86VcpuId,
            vector: X86InterruptVector,
        ) -> X86VlapicResult {
            INJECTED_VECTORS.lock().push((vm_id, vcpu_id, vector));
            Ok(())
        }

        fn request_ap_init(vm_id: X86VmId, vcpu_id: X86VcpuId) -> X86VlapicResult {
            CPU_UP_REQUESTS
                .lock()
                .push(RecordedCpuUpRequest::ApInit { vm_id, vcpu_id });
            Ok(())
        }

        fn request_sipi_startup(
            vm_id: X86VmId,
            vcpu_id: X86VcpuId,
            start_vector: u8,
        ) -> X86VlapicResult {
            CPU_UP_REQUESTS
                .lock()
                .push(RecordedCpuUpRequest::SipiStartup {
                    vm_id,
                    vcpu_id,
                    start_vector,
                });
            Ok(())
        }
    }

    /// Writes ICR destination and low halves exactly like xAPIC guest software.
    fn write_icr(vlapic: &mut VirtualApicRegs<RecordingHost>, dest: u32, icr_low: u32) {
        vlapic
            .handle_write(
                ApicRegOffset::ICRHi,
                (dest << 24) as usize,
                X86AccessWidth::Dword,
            )
            .expect("ICR-high write must succeed");
        vlapic
            .handle_write(
                ApicRegOffset::ICRLow,
                icr_low as usize,
                X86AccessWidth::Dword,
            )
            .expect("ICR-low write must succeed");
    }

    const DELIVERY_INIT: u32 = 0x5 << 8;
    const DELIVERY_STARTUP: u32 = 0x6 << 8;
    const LEVEL_TRIGGERED: u32 = 1 << 15;
    const LEVEL_ASSERTED: u32 = 1 << 14;

    #[test]
    fn asserted_init_requests_wait_for_sipi_at_physical_target() {
        let _mock = mock_session(7, 2, 0b11);
        let mut vlapic = VirtualApicRegs::<RecordingHost>::new(7, 0);

        // Linux's wakeup sequence: level-triggered, level-asserted INIT.
        write_icr(
            &mut vlapic,
            1,
            DELIVERY_INIT | LEVEL_TRIGGERED | LEVEL_ASSERTED,
        );

        assert_eq!(
            recorded_cpu_up_requests(),
            vec![RecordedCpuUpRequest::ApInit {
                vm_id: 7,
                vcpu_id: 1
            }]
        );
    }

    #[test]
    fn edge_triggered_init_with_deassert_level_flag_is_still_an_init() {
        // Pentium-4-style INIT leaves both the trigger-mode and level flags
        // cleared; per KVM the INIT accept rule is `!trig_mode || level`, so this
        // must not be mistaken for the level de-assert form.
        let _mock = mock_session(7, 2, 0b11);
        let mut vlapic = VirtualApicRegs::<RecordingHost>::new(7, 0);

        write_icr(&mut vlapic, 1, DELIVERY_INIT);

        assert_eq!(
            recorded_cpu_up_requests(),
            vec![RecordedCpuUpRequest::ApInit {
                vm_id: 7,
                vcpu_id: 1
            }]
        );
    }

    #[test]
    fn sipi_requests_startup_at_the_shifted_start_vector() {
        let _mock = mock_session(7, 2, 0b11);
        let mut vlapic = VirtualApicRegs::<RecordingHost>::new(7, 0);

        write_icr(&mut vlapic, 1, DELIVERY_STARTUP | 0x08);

        assert_eq!(
            recorded_cpu_up_requests(),
            vec![RecordedCpuUpRequest::SipiStartup {
                vm_id: 7,
                vcpu_id: 1,
                start_vector: 0x08,
            }]
        );
    }

    #[test]
    fn physical_miss_records_no_cpu_up_request() {
        let _mock = mock_session(7, 2, 0b11);
        let mut vlapic = VirtualApicRegs::<RecordingHost>::new(7, 0);

        write_icr(&mut vlapic, 5, DELIVERY_INIT | LEVEL_ASSERTED);
        write_icr(&mut vlapic, 5, DELIVERY_STARTUP | 0x08);

        assert!(recorded_cpu_up_requests().is_empty());
    }

    #[test]
    fn all_excluding_self_broadcast_reaches_every_other_target() {
        let _mock = mock_session(7, 3, 0b111);
        let mut vlapic = VirtualApicRegs::<RecordingHost>::new(7, 0);
        let shorthand_all_excluding_self = 0b11 << 18;

        write_icr(
            &mut vlapic,
            0,
            DELIVERY_INIT | LEVEL_ASSERTED | shorthand_all_excluding_self,
        );

        assert_eq!(
            recorded_cpu_up_requests(),
            vec![
                RecordedCpuUpRequest::ApInit {
                    vm_id: 7,
                    vcpu_id: 1
                },
                RecordedCpuUpRequest::ApInit {
                    vm_id: 7,
                    vcpu_id: 2
                },
            ]
        );
    }

    #[test]
    fn logical_destination_mode_is_ignored_for_init_and_sipi() {
        let _mock = mock_session(7, 2, 0b11);
        let mut vlapic = VirtualApicRegs::<RecordingHost>::new(7, 0);
        let destination_mode_logical = 1 << 11;

        write_icr(
            &mut vlapic,
            1,
            DELIVERY_INIT | LEVEL_ASSERTED | destination_mode_logical,
        );
        write_icr(
            &mut vlapic,
            1,
            DELIVERY_STARTUP | 0x08 | destination_mode_logical,
        );

        assert!(recorded_cpu_up_requests().is_empty());
    }

    #[test]
    fn self_shorthand_startup_is_rejected_without_a_request() {
        let _mock = mock_session(7, 2, 0b11);
        let mut vlapic = VirtualApicRegs::<RecordingHost>::new(7, 0);
        let shorthand_self = 0b01 << 18;

        write_icr(&mut vlapic, 0, DELIVERY_STARTUP | 0x08 | shorthand_self);

        assert!(recorded_cpu_up_requests().is_empty());
    }

    #[test]
    fn init_level_de_assert_does_not_request_another_reset() {
        let _mock = mock_session(7, 2, 0b11);
        let mut vlapic = VirtualApicRegs::<RecordingHost>::new(7, 0);

        write_icr(
            &mut vlapic,
            1,
            DELIVERY_INIT | LEVEL_TRIGGERED | LEVEL_ASSERTED,
        );
        // Legacy P6 arbitration sync message: trigger=level ∧ level=de-assert.
        write_icr(&mut vlapic, 1, DELIVERY_INIT | LEVEL_TRIGGERED);

        assert_eq!(
            recorded_cpu_up_requests(),
            vec![RecordedCpuUpRequest::ApInit {
                vm_id: 7,
                vcpu_id: 1
            }]
        );
    }

    #[test]
    fn fixed_delivery_keeps_injecting_vectors_without_cpu_up_requests() {
        let _mock = mock_session(7, 2, 0b11);
        let mut vlapic = VirtualApicRegs::<RecordingHost>::new(7, 0);

        write_icr(&mut vlapic, 1, 0x41);

        assert!(recorded_cpu_up_requests().is_empty());
        assert_eq!(injected_vectors(), vec![(7, 1, 0x41)]);
    }
}
