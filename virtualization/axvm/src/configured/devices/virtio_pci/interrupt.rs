use axdevice::PciEndpointContext;
use axdevice_base::{AccessWidth, DeviceError, DeviceResult};
use axvirtio_common::{
    DeviceContextMemory,
    pci::{
        InterruptPublication, InterruptTransition, InterruptTransitionRequest, VirtioDeviceCore,
        VirtioPciWriteOutcome,
    },
};

use super::VirtioPciFunction;

enum TransitionResult {
    Published,
    Suppressed,
    Failed(DeviceError),
}

impl<D: VirtioDeviceCore> VirtioPciFunction<D> {
    fn execute_transition(
        &self,
        context: &mut dyn PciEndpointContext,
        transition: InterruptTransition,
    ) -> TransitionResult {
        if transition == InterruptTransition::None {
            return TransitionResult::Published;
        }
        let result = context.with_irq_transition(&mut |permit| match transition {
            InterruptTransition::Assert => permit.assert(&self.irq_line),
            InterruptTransition::Deassert => permit.deassert(&self.irq_line),
            InterruptTransition::None => Ok(()),
        });
        match result {
            Ok(()) => TransitionResult::Published,
            Err(DeviceError::InvalidState { .. }) => TransitionResult::Suppressed,
            Err(error) => TransitionResult::Failed(error),
        }
    }

    pub(super) fn finish_transition(
        &self,
        context: &mut dyn PciEndpointContext,
        transition: InterruptTransition,
    ) -> DeviceResult {
        let mut transition = transition;
        loop {
            match self.execute_transition(context, transition) {
                TransitionResult::Published => {
                    transition = self
                        .transport
                        .complete_interrupt_transition(transition, true);
                }
                TransitionResult::Suppressed => {
                    self.transport.suppress_interrupt_transition(transition);
                    transition = InterruptTransition::None;
                }
                TransitionResult::Failed(error) => {
                    self.transport
                        .complete_interrupt_transition(transition, false);
                    return Err(error);
                }
            }
            if transition == InterruptTransition::None {
                return Ok(());
            }
        }
    }

    pub(super) fn finish_transition_request(
        &self,
        context: &mut dyn PciEndpointContext,
        request: InterruptTransitionRequest,
    ) -> DeviceResult {
        let transition = request.transition();
        let result = self.finish_transition(context, transition);
        drop(request);
        result
    }

    pub(super) fn finish_read_transition(
        &self,
        context: &mut dyn PciEndpointContext,
        request: InterruptTransitionRequest,
    ) {
        // ISR is read-to-clear: the value already captured for the guest is
        // not revoked when the physical line backend fails. `finish_transition`
        // records that failure in the coordinator, leaving the next admitted
        // transition responsible for retrying the deassertion.
        let _ = self.finish_transition_request(context, request);
    }

    fn publish_queue_notification(
        &self,
        notification: axvirtio_common::pci::QueueNotification,
        context: &mut dyn PciEndpointContext,
    ) {
        notification.publish(
            |transition| match self.execute_transition(context, transition) {
                TransitionResult::Published => InterruptPublication::Published,
                TransitionResult::Suppressed => InterruptPublication::Suppressed,
                TransitionResult::Failed(_) => InterruptPublication::Failed,
            },
        );
    }

    pub(super) fn write_transport(
        &self,
        offset: u64,
        width: AccessWidth,
        value: u64,
        dma_enabled: bool,
        context: &mut dyn PciEndpointContext,
    ) -> DeviceResult {
        let outcome = {
            let mut memory = DeviceContextMemory::new(context, &self.dma_grant);
            self.transport
                .write_bar_with_dma(offset, width, value, dma_enabled, &mut memory)?
        };
        match outcome {
            VirtioPciWriteOutcome::None => Ok(()),
            VirtioPciWriteOutcome::Reset { interrupt } => {
                if let Err(error) = self.finish_transition(context, interrupt) {
                    self.transport.abort_reset();
                    return Err(error);
                }
                self.transport.complete_reset();
                Ok(())
            }
            VirtioPciWriteOutcome::Fault {
                error,
                interrupt,
                activity,
            } => {
                let transition_result = self.finish_transition(context, interrupt);
                drop(activity);
                transition_result?;
                Err(error)
            }
            VirtioPciWriteOutcome::QueueNotified(notification) => {
                self.publish_queue_notification(notification, context);
                Ok(())
            }
        }
    }
}
