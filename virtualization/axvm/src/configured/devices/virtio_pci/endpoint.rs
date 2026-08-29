use std::format;

use axdevice::{
    EndpointIrqTransitionPermit, PciBarAccess, PciCommandState, PciConfigEffectId,
    PciConfigReadEffect, PciConfigWriteEffect, PciEndpointContext, PciFunction,
};
use axdevice_base::{Device, DeviceAccess, DeviceContext, DeviceError, DeviceResult, Resource};
use axvirtio_common::pci::{InterruptTransition, VirtioDeviceCore};

use super::{PCI_CFG_EFFECTS, VirtioPciFunction, config::decode_pci_cfg};

impl<D: VirtioDeviceCore> VirtioPciFunction<D> {
    fn require_bar_zero(access: PciBarAccess) -> DeviceResult {
        if access.bar().value() == 0 {
            Ok(())
        } else {
            Err(DeviceError::OutOfRange {
                addr: access.offset(),
            })
        }
    }
}

impl<D: VirtioDeviceCore> Device for VirtioPciFunction<D> {
    fn name(&self) -> &str {
        "virtio-pci"
    }

    fn resources(&self) -> &[Resource] {
        &self.resources
    }

    fn read(&self, access: &DeviceAccess, _context: &mut dyn DeviceContext) -> DeviceResult<u64> {
        Err(DeviceError::Unsupported {
            operation: "read VirtIO PCI endpoint as a generic device",
            detail: format!(
                "endpoint access on {:?} is dispatched by the PCI root",
                access.bus()
            ),
        })
    }

    fn write(
        &self,
        access: &DeviceAccess,
        _value: u64,
        _context: &mut dyn DeviceContext,
    ) -> DeviceResult {
        Err(DeviceError::Unsupported {
            operation: "write VirtIO PCI endpoint as a generic device",
            detail: format!(
                "endpoint access on {:?} is dispatched by the PCI root",
                access.bus()
            ),
        })
    }
}

impl<D: VirtioDeviceCore> PciFunction for VirtioPciFunction<D> {
    fn intx_pending(&self) -> bool {
        self.transport.interrupt_pending()
    }

    fn supported_config_effects(&self) -> &[PciConfigEffectId] {
        &PCI_CFG_EFFECTS
    }

    fn read_bar(
        &self,
        access: PciBarAccess,
        context: &mut dyn PciEndpointContext,
    ) -> DeviceResult<u64> {
        Self::require_bar_zero(access)?;
        let (value, transition) = self
            .transport
            .read_bar_with_interrupt(access.offset(), access.width())?;
        self.finish_read_transition(context, transition);
        Ok(value)
    }

    fn write_bar(
        &self,
        access: PciBarAccess,
        value: u64,
        context: &mut dyn PciEndpointContext,
    ) -> DeviceResult {
        Self::require_bar_zero(access)?;
        self.write_transport(
            access.offset(),
            access.width(),
            value,
            access.bus_master_enable(),
            context,
        )
    }

    fn read_config_effect(
        &self,
        effect: PciConfigReadEffect,
        context: &mut dyn PciEndpointContext,
    ) -> DeviceResult<u64> {
        let target = decode_pci_cfg(
            effect.effect(),
            effect.offset(),
            effect.width(),
            effect.capability_snapshot(),
        )?;
        let (value, transition) = self
            .transport
            .read_bar_with_interrupt(target, effect.width())?;
        self.finish_read_transition(context, transition);
        Ok(value)
    }

    fn write_config_effect(
        &self,
        effect: PciConfigWriteEffect,
        context: &mut dyn PciEndpointContext,
    ) -> DeviceResult {
        let target = decode_pci_cfg(
            effect.effect(),
            effect.offset(),
            effect.width(),
            effect.capability_snapshot(),
        )?;
        self.write_transport(
            target,
            effect.width(),
            effect.value(),
            effect.bus_master_enable(),
            context,
        )
    }

    fn command_changed(
        &self,
        command: PciCommandState,
        context: &mut dyn PciEndpointContext,
    ) -> DeviceResult {
        let transition = self
            .transport
            .set_interrupt_disabled(command.interrupt_disable())?;
        self.finish_transition_request(context, transition)
    }

    fn reset(&self, command: PciCommandState) -> DeviceResult {
        let _interrupt = self.transport.reset()?;
        let transition = self
            .transport
            .set_interrupt_disabled_during_reset(command.interrupt_disable())?;
        if transition != InterruptTransition::None {
            self.transport
                .complete_interrupt_transition(transition, false);
        }
        Ok(())
    }

    fn withdraw_irq(&self, permit: &mut EndpointIrqTransitionPermit) -> DeviceResult {
        permit.deassert(&self.irq_line)?;
        // Lifecycle reset performs the physical cleanup at this owner-side
        // boundary, after the callback admission has drained. Completing a
        // possible reset-generated deassertion keeps the coordinator's
        // logical state consistent; ordinary teardown has no in-flight
        // transition and is therefore unaffected.
        self.transport
            .complete_interrupt_transition(InterruptTransition::Deassert, true);
        self.transport.complete_reset();
        Ok(())
    }
}
