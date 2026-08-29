use alloc::{sync::Arc, vec::Vec};

use ax_sync::SpinLock;
use axdevice_base::DeviceId;

use super::{
    super::PciRootState,
    ORPHANED_IRQ_WITHDRAWALS,
    endpoint::{EndpointIrqTransitionPermit, PciFunction},
    routing::{EndpointAdmission, EndpointRouter},
};
use crate::{DeviceManagerError, DeviceManagerResult, DeviceNodeId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BindingLifecycleState {
    Running,
    Binding,
    Resetting,
    ResetFailed,
    Withdrawing,
    Stopping,
    Dead,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WithdrawalStartError {
    Busy,
    Terminal,
}

#[derive(Clone, Copy)]
enum LifecycleCompletion {
    Restore(BindingLifecycleState),
    Reset,
    Stop,
}

/// Owns one logical lifecycle operation without holding the state lock.
pub(super) struct LifecycleOperation<'a> {
    binding: &'a PciRootBinding,
    completion: LifecycleCompletion,
    completed: bool,
}

impl LifecycleOperation<'_> {
    pub(super) fn finish(mut self, state: BindingLifecycleState) {
        *self.binding.lifecycle.lock_irqsave() = state;
        self.completed = true;
    }

    pub(super) fn finish_restore(self) {
        let LifecycleCompletion::Restore(state) = self.completion else {
            return;
        };
        self.finish(state);
    }

    pub(super) fn finish_reset(mut self) -> DeviceManagerResult {
        loop {
            self.binding.drain_pending_binding_withdrawals()?;
            self.binding.notify_reset_handoff();

            let no_deferred_withdrawals = {
                let mut lifecycle = self.binding.lifecycle.lock_irqsave();
                let pending = self.binding.pending_binding_withdrawals.lock_irqsave();
                if pending.is_empty() {
                    *lifecycle = BindingLifecycleState::Running;
                    true
                } else {
                    false
                }
            };
            if no_deferred_withdrawals {
                self.binding.router.open_admissions();
                self.completed = true;
                return Ok(());
            }
        }
    }
}

impl Drop for LifecycleOperation<'_> {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let fallback = match self.completion {
            LifecycleCompletion::Restore(state) => state,
            // A reset that unwinds before publishing its result must remain
            // fail-closed instead of reopening partially reset state.
            LifecycleCompletion::Reset => BindingLifecycleState::ResetFailed,
            LifecycleCompletion::Stop => BindingLifecycleState::Dead,
        };
        *self.binding.lifecycle.lock_irqsave() = fallback;
    }
}

/// Host-owned root binding published as a typed bundle service.
pub struct PciRootBinding {
    pub(super) host: DeviceNodeId,
    pub(super) root: Arc<PciRootState>,
    pub(super) router: Arc<EndpointRouter>,
    pub(super) lifecycle: SpinLock<BindingLifecycleState>,
    pub(super) pending_irq_withdrawals: SpinLock<Vec<PendingIrqWithdrawal>>,
    pub(super) pending_binding_withdrawals: SpinLock<Vec<DeviceId>>,
    #[cfg(test)]
    reset_handoff_hook: SpinLock<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    deferred_withdrawal_hook: SpinLock<Option<Arc<dyn Fn() + Send + Sync>>>,
}

pub(super) struct PendingIrqWithdrawal {
    pub(super) device: DeviceId,
    pub(super) function: Arc<dyn PciFunction>,
    pub(super) admission: Arc<EndpointAdmission>,
}

impl PciRootBinding {
    /// Creates a binding service for one resolved host root.
    pub fn new(host: DeviceNodeId, root: Arc<PciRootState>) -> Self {
        Self {
            host,
            root,
            router: Arc::new(EndpointRouter::new()),
            lifecycle: SpinLock::new(BindingLifecycleState::Running),
            pending_irq_withdrawals: SpinLock::new(Vec::new()),
            pending_binding_withdrawals: SpinLock::new(Vec::new()),
            #[cfg(test)]
            reset_handoff_hook: SpinLock::new(None),
            #[cfg(test)]
            deferred_withdrawal_hook: SpinLock::new(None),
        }
    }

    #[cfg(test)]
    pub(super) fn set_reset_handoff_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.reset_handoff_hook.lock_irqsave() = Some(hook);
    }

    #[cfg(test)]
    pub(super) fn set_deferred_withdrawal_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.deferred_withdrawal_hook.lock_irqsave() = Some(hook);
    }

    #[cfg(test)]
    fn notify_reset_handoff(&self) {
        let hook = self.reset_handoff_hook.lock_irqsave().take();
        if let Some(hook) = hook {
            hook();
        }
    }

    #[cfg(not(test))]
    fn notify_reset_handoff(&self) {}

    #[cfg(test)]
    fn notify_deferred_withdrawal(&self) {
        let hook = self.deferred_withdrawal_hook.lock_irqsave().take();
        if let Some(hook) = hook {
            hook();
        }
    }

    #[cfg(not(test))]
    fn notify_deferred_withdrawal(&self) {}

    /// Returns the host graph identity publishing this service.
    pub const fn host(&self) -> &DeviceNodeId {
        &self.host
    }

    /// Retries deferred endpoint binding and endpoint-owned IRQ withdrawals.
    ///
    /// A pending withdrawal keeps the endpoint owner and its closed admission
    /// alive. Rebinding the same device is rejected until this method drains
    /// the owner-side cleanup successfully.
    pub fn retry_irq_withdrawals(&self) -> DeviceManagerResult {
        let operation = self.begin_withdrawal_operation()?;
        let deferred_result = self.drain_pending_binding_withdrawals();
        let irq_result = retry_pending_irq_withdrawals(&self.pending_irq_withdrawals);
        operation.finish_restore();
        deferred_result.and(irq_result)
    }

    /// Retries endpoint IRQ withdrawals orphaned by a previous root teardown.
    ///
    /// The orphan queue retains each endpoint owner and closed admission until
    /// this method succeeds. A failed retry leaves the entry fail-closed for a
    /// later owner or teardown supervisor to retry.
    pub fn retry_orphaned_irq_withdrawals() -> DeviceManagerResult {
        retry_pending_irq_withdrawals(&ORPHANED_IRQ_WITHDRAWALS)
    }

    pub(super) fn begin_binding_operation(&self) -> DeviceManagerResult<LifecycleOperation<'_>> {
        let mut lifecycle = self.lifecycle.lock_irqsave();
        if *lifecycle != BindingLifecycleState::Running {
            return Err(DeviceManagerError::InvalidState {
                operation: "bind PCI endpoint route",
                detail: "PCI root binding is not running".into(),
            });
        }
        *lifecycle = BindingLifecycleState::Binding;
        Ok(LifecycleOperation {
            binding: self,
            completion: LifecycleCompletion::Restore(BindingLifecycleState::Running),
            completed: false,
        })
    }

    pub(super) fn begin_reset_operation(&self) -> DeviceManagerResult<LifecycleOperation<'_>> {
        let mut lifecycle = self.lifecycle.lock_irqsave();
        if *lifecycle != BindingLifecycleState::Running {
            return Err(DeviceManagerError::InvalidState {
                operation: "reset PCI root binding",
                detail: "PCI root binding is not running".into(),
            });
        }
        *lifecycle = BindingLifecycleState::Resetting;
        Ok(LifecycleOperation {
            binding: self,
            completion: LifecycleCompletion::Reset,
            completed: false,
        })
    }

    pub(super) fn begin_withdrawal_operation(&self) -> DeviceManagerResult<LifecycleOperation<'_>> {
        self.try_begin_withdrawal_operation().map_err(|error| {
            let detail = match error {
                WithdrawalStartError::Busy => {
                    "PCI root binding lifecycle operation is already in progress"
                }
                WithdrawalStartError::Terminal => "PCI root binding is stopping or dead",
            };
            DeviceManagerError::InvalidState {
                operation: "withdraw PCI endpoint IRQ",
                detail: detail.into(),
            }
        })
    }

    pub(super) fn try_begin_withdrawal_operation(
        &self,
    ) -> Result<LifecycleOperation<'_>, WithdrawalStartError> {
        let mut lifecycle = self.lifecycle.lock_irqsave();
        let previous = *lifecycle;
        match previous {
            BindingLifecycleState::Running | BindingLifecycleState::ResetFailed => {}
            BindingLifecycleState::Binding
            | BindingLifecycleState::Resetting
            | BindingLifecycleState::Withdrawing => {
                return Err(WithdrawalStartError::Busy);
            }
            BindingLifecycleState::Stopping | BindingLifecycleState::Dead => {
                return Err(WithdrawalStartError::Terminal);
            }
        }
        *lifecycle = BindingLifecycleState::Withdrawing;
        Ok(LifecycleOperation {
            binding: self,
            completion: LifecycleCompletion::Restore(previous),
            completed: false,
        })
    }

    /// Starts withdrawal or completes deferred/terminal handling while holding
    /// the lifecycle lock.
    ///
    /// Reset completion checks the deferred queue while holding the same lock,
    /// so a lease cannot observe a busy reset and enqueue after the reset has
    /// already published `Running`.
    pub(super) fn try_begin_withdrawal_or_defer(
        &self,
        device: DeviceId,
    ) -> Option<LifecycleOperation<'_>> {
        let mut lifecycle = self.lifecycle.lock_irqsave();
        match *lifecycle {
            BindingLifecycleState::Running | BindingLifecycleState::ResetFailed => {
                let previous = *lifecycle;
                *lifecycle = BindingLifecycleState::Withdrawing;
                Some(LifecycleOperation {
                    binding: self,
                    completion: LifecycleCompletion::Restore(previous),
                    completed: false,
                })
            }
            BindingLifecycleState::Binding
            | BindingLifecycleState::Resetting
            | BindingLifecycleState::Withdrawing => {
                self.notify_deferred_withdrawal();
                self.pending_binding_withdrawals.lock_irqsave().push(device);
                None
            }
            BindingLifecycleState::Stopping | BindingLifecycleState::Dead => None,
        }
    }

    pub(super) fn begin_stop_operation(&self) -> LifecycleOperation<'_> {
        *self.lifecycle.lock_irqsave() = BindingLifecycleState::Stopping;
        LifecycleOperation {
            binding: self,
            completion: LifecycleCompletion::Stop,
            completed: false,
        }
    }
}

pub(super) fn retry_pending_irq_withdrawals(
    pending_storage: &SpinLock<Vec<PendingIrqWithdrawal>>,
) -> DeviceManagerResult {
    let pending = core::mem::take(&mut *pending_storage.lock_irqsave());
    let mut remaining = Vec::new();
    let mut first_error = None;
    for withdrawal in pending {
        let mut permit = EndpointIrqTransitionPermit { _private: () };
        let result = withdrawal.admission.wait_for_irq_permits().and_then(|()| {
            withdrawal
                .function
                .withdraw_irq(&mut permit)
                .map_err(DeviceManagerError::Device)
        });
        if let Err(error) = result {
            if first_error.is_none() {
                first_error = Some(error);
            }
            remaining.push(withdrawal);
        }
    }
    // A root teardown may transfer another owner while callbacks run. Merge
    // with the current queue instead of replacing it, preserving both the
    // retry results and owners arriving concurrently.
    pending_storage.lock_irqsave().extend(remaining);
    first_error.map_or(Ok(()), Err)
}

pub(super) fn transfer_pending_irq_withdrawals(
    pending_storage: &SpinLock<Vec<PendingIrqWithdrawal>>,
) {
    let pending = core::mem::take(&mut *pending_storage.lock_irqsave());
    if pending.is_empty() {
        return;
    }
    ORPHANED_IRQ_WITHDRAWALS.lock_irqsave().extend(pending);
    warn!("PCI endpoint IRQ withdrawals transferred to the fail-closed orphan queue");
}
