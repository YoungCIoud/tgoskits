//! x86-only port, nested-fault, and deferred exit handling.

use std::sync::atomic::{AtomicU64, Ordering};

use axdevice_base::{BusKind, DeviceAccess, DeviceVcpuId};
use axvm_types::{AccessWidth, GuestPhysAddr, MappingFlags, Port};
use x86_vcpu::{X86PortIoDirection, X86PortIoStringExit};

use super::*;

const DIAGNOSTIC_PORT_8: u16 = 0x8;
const PCI_CONFIG_PORT_START: u16 = 0xcf8;
const PCI_CONFIG_DATA_PORT: u16 = 0xcfc;
const PCI_CONFIG_PORT_END: u16 = 0xcff;
const TLS_DXE_RUNTIME_IMAGE_BASE: u64 = 0x1e956000;
const TLS_DXE_INIT_START: u64 = TLS_DXE_RUNTIME_IMAGE_BASE + 0x63a94;
const TLS_DXE_INIT_END: u64 = TLS_DXE_RUNTIME_IMAGE_BASE + 0x63b6b;
const TLS_DXE_PM_CONFIG_SELECTOR: u32 = 0x8000_f840;
const DIAGNOSTIC_PCI_CONFIG_RESPONSE_LIMIT: u64 = 128;
const DIAGNOSTIC_TLS_DXE_PCI_LIMIT: u64 = 128;
static DIAGNOSTIC_PORT_8_READ_COUNT: AtomicU64 = AtomicU64::new(0);
static DIAGNOSTIC_ACPI_SNAPSHOT_COUNT: AtomicU64 = AtomicU64::new(0);
static DIAGNOSTIC_PCI_CONFIG_RESPONSE_COUNT: AtomicU64 = AtomicU64::new(0);
static DIAGNOSTIC_TLS_DXE_PCI_ACCESS_COUNT: AtomicU64 = AtomicU64::new(0);
static DIAGNOSTIC_PCI_CONFIG_ADDRESS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug)]
pub(crate) enum DeferredRunWork {
    ExternalInterrupt { vector: usize },
    PreemptionTimer,
    InterruptEnd { vector: Option<u8> },
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct IoReadExit {
    pub(crate) port: Port,
    pub(crate) width: AccessWidth,
    pub(crate) guest_rip: u64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct IoWriteExit {
    pub(crate) port: Port,
    pub(crate) width: AccessWidth,
    pub(crate) data: u64,
    pub(crate) guest_rip: u64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct NestedPageFaultExit {
    pub(crate) addr: GuestPhysAddr,
    pub(crate) access_flags: MappingFlags,
}

pub(crate) fn handle_io_read(
    vm: &crate::AxVM,
    vcpu: &crate::vm::AxVCpuRef<AxvmX86Vcpu>,
    exit: IoReadExit,
) -> AxVmResult<BoundVcpuExit<DeferredRunWork>> {
    let access = DeviceAccess::new(
        DeviceVcpuId::new(vcpu.id()),
        BusKind::Port,
        exit.port.number() as u64,
        exit.width,
    );
    let mapped_value = vm
        .get_devices()?
        .try_read(&access)
        .map_err(|error| AxVmError::device("read guest I/O port", error))?;
    let mapped = mapped_value.is_some();
    let val = mapped_value
        .map(|value| value as usize)
        .unwrap_or_else(|| unmapped_port_value(exit.width));
    log_tls_dxe_pci_read(&exit, val, mapped);
    if (PCI_CONFIG_PORT_START..=PCI_CONFIG_PORT_END).contains(&exit.port.number()) {
        let count = DIAGNOSTIC_PCI_CONFIG_RESPONSE_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if count <= DIAGNOSTIC_PCI_CONFIG_RESPONSE_LIMIT || count.is_power_of_two() {
            info!(
                "[HV] guest PCI config response: responses={} port={:#x} width={:?} mapped={} \
                 value={:#x} guest_rip={:#x} vcpu={}",
                count,
                exit.port.number(),
                exit.width,
                mapped,
                val,
                exit.guest_rip,
                vcpu.id(),
            );
        }
    }
    if exit.port.number() == DIAGNOSTIC_PORT_8 {
        let reads = DIAGNOSTIC_PORT_8_READ_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if reads == 1 || reads.is_power_of_two() {
            info!(
                "[HV] guest diagnostic port read: reads={} port={:#x} width={:?} mapped={} \
                 value={:#x} vcpu={}",
                reads,
                exit.port.number(),
                exit.width,
                mapped,
                val,
                vcpu.id(),
            );
        }
        if reads == 1
            && DIAGNOSTIC_ACPI_SNAPSHOT_COUNT
                .compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            log_guest_acpi_snapshot(vm);
        }
    }
    vcpu.set_gpr(0, val);
    Ok(BoundVcpuExit::Continue)
}

fn log_guest_acpi_snapshot(vm: &crate::AxVM) {
    let rsdp_gpa = super::boot::DIRECT_ACPI_BASE;
    let mut rsdp = [0u8; 36];
    if let Err(error) = vm.read_from_guest(GuestPhysAddr::from(rsdp_gpa as usize), &mut rsdp) {
        warn!("[HV] guest ACPI snapshot failed at {rsdp_gpa:#x}: {error:?}");
        return;
    }
    let xsdt_gpa = u64::from_le_bytes(rsdp[24..32].try_into().unwrap());
    let mut xsdt_header = [0u8; 36];
    if let Err(error) = vm.read_from_guest(GuestPhysAddr::from(xsdt_gpa as usize), &mut xsdt_header)
    {
        warn!("[HV] guest ACPI snapshot failed at XSDT {xsdt_gpa:#x}: {error:?}");
        return;
    }
    let xsdt_length = u32::from_le_bytes(xsdt_header[4..8].try_into().unwrap()) as usize;
    if !(36..=4096).contains(&xsdt_length) || !(xsdt_length - 36).is_multiple_of(8) {
        warn!(
            "[HV] guest ACPI snapshot has invalid XSDT length: gpa={xsdt_gpa:#x} \
             length={xsdt_length:#x}"
        );
        return;
    }
    let mut xsdt = vec![0u8; xsdt_length];
    if let Err(error) = vm.read_from_guest(GuestPhysAddr::from(xsdt_gpa as usize), &mut xsdt) {
        warn!("[HV] guest ACPI snapshot failed reading XSDT body: {error:?}");
        return;
    }
    info!(
        "[HV] guest ACPI snapshot: rsdp_gpa={rsdp_gpa:#x} rsdp={rsdp:02x?} xsdt_gpa={xsdt_gpa:#x} \
         xsdt_length={xsdt_length:#x}"
    );
    for entry in xsdt[36..].as_chunks::<8>().0 {
        let table_gpa = u64::from_le_bytes(*entry);
        let mut header = [0u8; 36];
        if let Err(error) = vm.read_from_guest(GuestPhysAddr::from(table_gpa as usize), &mut header)
        {
            warn!("[HV] guest ACPI snapshot failed at table {table_gpa:#x}: {error:?}");
            continue;
        }
        let signature = &header[..4];
        let length = u32::from_le_bytes(header[4..8].try_into().unwrap());
        info!(
            "[HV] guest ACPI table: signature={signature:02x?} gpa={table_gpa:#x} \
             length={length:#x}"
        );
        if signature == b"FACP" && length as usize >= 220 && length as usize <= 4096 {
            let mut fadt = vec![0u8; length as usize];
            if let Err(error) =
                vm.read_from_guest(GuestPhysAddr::from(table_gpa as usize), &mut fadt)
            {
                warn!("[HV] guest ACPI snapshot failed reading FADT body: {error:?}");
                continue;
            }
            let legacy_pm_timer = u32::from_le_bytes(fadt[76..80].try_into().unwrap());
            let extended_pm_timer = u64::from_le_bytes(fadt[212..220].try_into().unwrap());
            info!(
                "[HV] guest ACPI FADT timer fields: gpa={table_gpa:#x} \
                 pm_tmr_blk={legacy_pm_timer:#x} x_pm_tmr_blk={extended_pm_timer:#x} pm_tmr_len={}",
                fadt[91],
            );
        }
    }
}

pub(crate) fn handle_io_write(
    vm: &crate::AxVM,
    vcpu: &crate::vm::AxVCpuRef<AxvmX86Vcpu>,
    exit: IoWriteExit,
) -> AxVmResult<BoundVcpuExit<DeferredRunWork>> {
    let access = DeviceAccess::new(
        DeviceVcpuId::new(vcpu.id()),
        BusKind::Port,
        exit.port.number() as u64,
        exit.width,
    );
    track_pci_config_address(&exit);
    if (PCI_CONFIG_PORT_START..=PCI_CONFIG_PORT_END).contains(&exit.port.number()) {
        let count = DIAGNOSTIC_PCI_CONFIG_RESPONSE_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if count <= DIAGNOSTIC_PCI_CONFIG_RESPONSE_LIMIT || count.is_power_of_two() {
            info!(
                "[HV] guest PCI config write dispatch: accesses={} port={:#x} width={:?} \
                 value={:#x} guest_rip={:#x} vcpu={}",
                count,
                exit.port.number(),
                exit.width,
                exit.data,
                exit.guest_rip,
                vcpu.id(),
            );
        }
    }
    vm.try_write_device(&access, exit.data)
        .map_err(|error| AxVmError::device("write guest I/O port", error))?;
    publish_pic_interrupt_if_needed(vm, vcpu.id(), exit.port, exit.data)?;
    Ok(BoundVcpuExit::Continue)
}

fn track_pci_config_address(exit: &IoWriteExit) {
    let port = exit.port.number();
    if !(PCI_CONFIG_PORT_START..PCI_CONFIG_DATA_PORT).contains(&port) {
        return;
    }
    let offset = usize::from(port - PCI_CONFIG_PORT_START);
    let size = exit.width.size();
    if offset.checked_add(size).is_none_or(|end| end > 4) {
        return;
    }
    let mut address = DIAGNOSTIC_PCI_CONFIG_ADDRESS.load(Ordering::Relaxed);
    for index in 0..size {
        let shift = (offset + index) * 8;
        let mask = !(0xff_u64 << shift);
        address = (address & mask) | (((exit.data >> (index * 8)) & 0xff) << shift);
    }
    DIAGNOSTIC_PCI_CONFIG_ADDRESS.store(address, Ordering::Relaxed);
    if is_tls_dxe_init_rip(exit.guest_rip) {
        log_tls_dxe_pci_access(
            "address write",
            exit.guest_rip,
            Some(address as u32),
            exit.port,
            exit.width,
            exit.data as usize,
            true,
        );
    }
}

fn log_tls_dxe_pci_read(exit: &IoReadExit, value: usize, mapped: bool) {
    if !is_tls_dxe_init_rip(exit.guest_rip)
        || !(PCI_CONFIG_DATA_PORT..PCI_CONFIG_DATA_PORT + 4).contains(&exit.port.number())
    {
        return;
    }
    let selector = DIAGNOSTIC_PCI_CONFIG_ADDRESS.load(Ordering::Relaxed) as u32;
    log_tls_dxe_pci_access(
        "data read",
        exit.guest_rip,
        Some(selector),
        exit.port,
        exit.width,
        value,
        mapped,
    );
}

fn log_tls_dxe_pci_access(
    operation: &str,
    guest_rip: u64,
    selector: Option<u32>,
    port: Port,
    width: AccessWidth,
    value: usize,
    mapped: bool,
) {
    let count = DIAGNOSTIC_TLS_DXE_PCI_ACCESS_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if count <= DIAGNOSTIC_TLS_DXE_PCI_LIMIT || count.is_power_of_two() {
        info!(
            "[HV] TlsDxe init PCI {operation}: accesses={count} rip={guest_rip:#x} \
             selector={selector:?} pm_selector={} port={:#x} width={width:?} value={value:#x} \
             mapped={mapped}",
            selector == Some(TLS_DXE_PM_CONFIG_SELECTOR),
            port.number(),
        );
    }
}

fn is_tls_dxe_init_rip(guest_rip: u64) -> bool {
    (TLS_DXE_INIT_START..TLS_DXE_INIT_END).contains(&guest_rip)
}

pub(crate) fn handle_io_string(
    vm: &crate::AxVM,
    vcpu: &crate::vm::AxVCpuRef<AxvmX86Vcpu>,
    exit: X86PortIoStringExit,
) -> AxVmResult<BoundVcpuExit<DeferredRunWork>> {
    let port = super::x86_port_to_ax(exit.port());
    let width = super::x86_access_width_to_ax(exit.width());
    let size = width.size();
    let guest_paddr = super::x86_guest_phys_addr_to_ax(exit.guest_paddr());
    let access = DeviceAccess::new(
        DeviceVcpuId::new(vcpu.id()),
        BusKind::Port,
        port.number() as u64,
        width,
    );

    match exit.direction() {
        X86PortIoDirection::In => {
            let value = vm
                .get_devices()?
                .try_read(&access)
                .map_err(|error| AxVmError::device("read guest string I/O port", error))?
                .map(|value| value as usize)
                .unwrap_or_else(|| unmapped_port_value(width));
            vm.write_to_guest(guest_paddr, &value.to_le_bytes()[..size])?;
        }
        X86PortIoDirection::Out => {
            let mut bytes = [0u8; 8];
            vm.read_from_guest(guest_paddr, &mut bytes[..size])?;
            let value = u64::from_le_bytes(bytes);
            vm.try_write_device(&access, value)
                .map_err(|error| AxVmError::device("write guest string I/O port", error))?;
            publish_pic_interrupt_if_needed(vm, vcpu.id(), port, value)?;
        }
    }

    vcpu.get_arch_vcpu().complete_port_io_string(exit)?;
    Ok(BoundVcpuExit::Continue)
}

fn publish_pic_interrupt_if_needed(
    vm: &crate::AxVM,
    vcpu_id: usize,
    port: Port,
    value: u64,
) -> AxVmResult {
    let port = x86_vlapic::X86Port::new(port.number());
    if EmulatedPic::port_ranges()
        .iter()
        .any(|range| range.contains(port))
    {
        super::publish_pic_interrupt_after_write(vm, vcpu_id, port.number(), value)?;
    }
    Ok(())
}

fn unmapped_port_value(width: AccessWidth) -> usize {
    usize::MAX >> ((core::mem::size_of::<usize>() - width.size()) * 8)
}

pub(crate) fn finish(
    vm: &crate::AxVMRef,
    vcpu: &crate::vm::AxVCpuRef<AxvmX86Vcpu>,
    work: DeferredRunWork,
) -> AxVmResult<VcpuRunAction> {
    match work {
        DeferredRunWork::ExternalInterrupt { vector } => {
            crate::architecture::exit::finish_external_interrupt(vector);
        }
        DeferredRunWork::PreemptionTimer => {}
        DeferredRunWork::InterruptEnd { vector } => {
            if let Some(vector) = vector {
                super::irq::inject_pending_ioapic_irq_after_eoi(vm, vcpu, vector);
            }
        }
    }
    Ok(VcpuRunAction {
        waits_for_event: false,
        stop_reason: None,
        resets_vm: false,
        exits_vcpu: false,
    })
}
