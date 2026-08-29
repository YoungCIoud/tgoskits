use alloc::sync::Arc;

use super::{ActivityPermit, QueueNotifyOutcome};
use crate::pci::{InterruptTransition, VirtioPciInterruptCoordinator};

/// Result of attempting to publish one queue-completion transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterruptPublication {
    /// The endpoint IRQ permit was acquired and the line operation succeeded.
    Published,
    /// The binding or queue generation was stale; suppress this completion.
    Suppressed,
    /// The line operation failed after admission.
    Failed,
}

/// Queue notification result whose activity permit remains alive until the
/// endpoint has published or deliberately suppressed the completion interrupt.
pub struct QueueNotification {
    pub(super) outcome: QueueNotifyOutcome,
    pub(super) interrupt: InterruptTransition,
    pub(super) activity: Option<ActivityPermit>,
    pub(super) interrupts: Arc<VirtioPciInterruptCoordinator>,
}

impl QueueNotification {
    /// Returns the device-core result.
    pub const fn outcome(&self) -> QueueNotifyOutcome {
        self.outcome
    }

    /// Returns the interrupt transition to execute under the endpoint IRQ
    /// transition permit.
    pub const fn interrupt_transition(&self) -> InterruptTransition {
        self.interrupt
    }

    /// Returns the queue configuration generation covered by this terminal
    /// notification, if processing was admitted.
    pub const fn generation(&self) -> Option<VirtioQueueGeneration> {
        match &self.activity {
            Some(activity) => Some(activity.generation),
            None => None,
        }
    }

    /// Explicitly ends the activity lifetime after completion publication.
    pub fn complete(mut self) {
        if self.interrupt != InterruptTransition::None {
            self.interrupts.suppress_queue_completion(self.interrupt);
        }
        self.activity.take();
    }

    /// Publishes the completion transition and then releases queue activity.
    pub fn publish<F>(mut self, mut publish_transition: F)
    where
        F: FnMut(InterruptTransition) -> InterruptPublication,
    {
        let mut transition = self.interrupt;
        loop {
            match publish_transition(transition) {
                InterruptPublication::Published => {
                    transition = self.interrupts.complete_transition(transition, true);
                }
                InterruptPublication::Suppressed => {
                    transition = self.interrupts.suppress_queue_completion(transition);
                }
                InterruptPublication::Failed => {
                    self.interrupts.complete_transition(transition, false);
                    transition = InterruptTransition::None;
                }
            }
            if transition == InterruptTransition::None {
                break;
            }
        }
        self.activity.take();
    }
}

/// Identity of one queue configuration lifetime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VirtioQueueGeneration(pub(super) u64);

impl VirtioQueueGeneration {
    /// Returns the numeric generation for diagnostics and tests.
    pub const fn value(self) -> u64 {
        self.0
    }
}

/// Interrupt transition intent retained until its endpoint callback finishes.
pub struct InterruptTransitionRequest {
    transition: InterruptTransition,
    activity: Option<ActivityPermit>,
    interrupts: Arc<VirtioPciInterruptCoordinator>,
}

impl core::fmt::Debug for InterruptTransitionRequest {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("InterruptTransitionRequest")
            .field("transition", &self.transition)
            .field("has_activity", &self.activity.is_some())
            .finish_non_exhaustive()
    }
}

impl InterruptTransitionRequest {
    pub(super) fn new(
        interrupts: Arc<VirtioPciInterruptCoordinator>,
        transition: InterruptTransition,
        activity: Option<ActivityPermit>,
    ) -> Self {
        Self {
            transition,
            activity,
            interrupts,
        }
    }

    pub(super) fn without_activity(
        interrupts: Arc<VirtioPciInterruptCoordinator>,
        transition: InterruptTransition,
    ) -> Self {
        Self::new(interrupts, transition, None)
    }

    /// Returns the physical transition that the endpoint must publish.
    pub const fn transition(&self) -> InterruptTransition {
        self.transition
    }
}

impl Drop for InterruptTransitionRequest {
    fn drop(&mut self) {
        self.interrupts.cancel_transition(self.transition);
        self.activity.take();
    }
}
