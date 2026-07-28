# PR 1b：VmInterruptSender + 集成 dispatcher 到 VmRuntimeHandle

## 范围

将 PR 1a 的 `VcpuIrqDispatcher` 嵌入 `VmRuntimeHandle`，提供 Router→runtime 的发送通道（`VmInterruptSender` + `dispatch_vcpu_interrupt`）。此时发送通道存在但**尚无消费者**（vCPU run loop 尚未 drain 新队列，等待 PR 1c），新旧队列并存。

## 前置依赖

- PR 1a（#1661）已合入：`VirtualInterruptId`, `PendingVcpuInterrupt`, `VcpuIrqDispatcher`, `VcpuInterruptQueue` 可用
- PR 1a 实际实现与原始设计的差异：
  - 额外提取了 `VcpuInterruptQueue`（`runtime/queue.rs`）分离队列语义与 task 依赖
  - 使用 `#[allow(dead_code)]` 逐项标注（非 `#[expect(dead_code)]`）

## 设计

### 1. VmInterruptSender

新文件 `virtualization/axvm/src/irq/sender.rs`：

```rust
use alloc::sync::Weak;

use crate::{irq::model::PendingVcpuInterrupt, AxVM, AxVmResult, AxVmError, VmStatus, ax_err_type};

/// Router 到 runtime 的稳定发送通道。
///
/// 只持有 `Weak<AxVM>`，不缓存 runtime 或 dispatcher 引用。
/// 每次 `send()` 时重新查找当前 runtime，因此 VM stop/start/reset 后
/// 不会引用旧 dispatcher。
#[derive(Clone)]
pub struct VmInterruptSender {
    vm: Weak<AxVM>,
}

impl VmInterruptSender {
    /// 从 `AxVMRef`（即 `Arc<AxVM>`）构造 sender。
    pub fn new(vm: &Arc<AxVM>) -> Self {
        Self { vm: Arc::downgrade(vm) }
    }

    /// 向指定 vCPU 递送中断。
    ///
    /// 内部流程：
    /// 1. upgrade `Weak<AxVM>` — VM 已销毁则返回 `NotFound`
    /// 2. 检查 VM status — 仅 Running/Paused 接受中断，否则返回 `BadState`
    /// 3. `vm.with_runtime(|rt| rt.dispatch_vcpu_interrupt(vcpu_id, interrupt))`
    ///    — runtime 尚未创建时 `with_runtime` 返回 `BadState`
    ///    — 目标 vCPU task 未注册（`enqueue` 内部）返回 `NotFound`
    ///
    /// 注意步骤 2→3 之间存在 TOCTOU（status 可能变化），与旧
    /// `vcpus::queue_interrupt` 行为一致，属于可接受的窗口。
    pub fn send(
        &self,
        vcpu_id: usize,
        interrupt: PendingVcpuInterrupt,
    ) -> AxVmResult;
}
```

关键语义：
- `Clone` — Router 可以持有 sender 的 clone，不引入额外引用计数开销（`Weak` 是单层间接）
- 每次 `send()` 从头查找 runtime — 不缓存 `VcpuIrqDispatcher`
- VM 已销毁、runtime 尚未创建、VM 状态不接受中断时返回明确错误

**PR 1b 内 `VmInterruptSender` 无调用者**，整个 struct + `new()` + `send()` 标记 `#[allow(dead_code)]`（Router 在模块 2+ 中创建 sender）。

### 2. VmRuntimeHandle 集成

修改 `virtualization/axvm/src/vm/mod.rs`：

```rust
use crate::irq::model::PendingVcpuInterrupt;

pub(crate) struct VmRuntimeHandle {
    wait_queue: crate::WaitQueue,
    vcpu_task_list: Mutex<BTreeMap<usize, crate::AxTaskRef>>,     // 旧字段，保留
    pending_interrupts: Mutex<BTreeMap<usize, Vec<PendingInterrupt>>>, // 旧字段，保留
    irq_dispatcher: VcpuIrqDispatcher,                              // 新增
    running_halting_vcpu_count: AtomicUsize,
}

impl VmRuntimeHandle {
    pub(crate) fn new() -> Self {
        Self {
            wait_queue: crate::WaitQueue::new(),
            vcpu_task_list: Mutex::new(BTreeMap::new()),
            pending_interrupts: Mutex::new(BTreeMap::new()),
            irq_dispatcher: VcpuIrqDispatcher::new(),               // 新增
            running_halting_vcpu_count: AtomicUsize::new(0),
        }
    }

    pub(crate) fn add_vcpu_task(&self, vcpu_id: usize, vcpu_task: crate::AxTaskRef) {
        self.irq_dispatcher.register_vcpu_task(vcpu_id, vcpu_task.clone());
        self.vcpu_task_list.lock().insert(vcpu_id, vcpu_task);
        self.pending_interrupts.lock().entry(vcpu_id).or_default();
    }

    /// 暴露 dispatcher 的只读访问（供 PR 1c vCPU run loop drain 使用）。
    #[allow(dead_code, reason = "wired in PR 1c")]
    pub(crate) fn irq_dispatcher(&self) -> &VcpuIrqDispatcher {
        &self.irq_dispatcher
    }

    /// 新递送路径：入队 → notify → host IPI。
    ///
    /// 在 `machine.lock()` 持有期间调用（通过 `with_runtime`）。
    /// `send_ipi` 是快速 MMIO/MSR 写，不睡眠、不获取其他锁，
    /// 在 SpinNoIrq 临界区内调用是安全的。
    /// 合并入队+notify+IPI 为单次锁获取，消除了旧代码两次
    /// `with_runtime` 之间的入队→notify 竞态窗口。
    #[allow(dead_code, reason = "wired in PR 1c / module 2+")]
    pub(crate) fn dispatch_vcpu_interrupt(
        &self,
        vcpu_id: usize,
        interrupt: PendingVcpuInterrupt,
    ) -> AxVmResult {
        let pcpu_id = self.irq_dispatcher.enqueue(vcpu_id, interrupt)?;
        self.notify_all();
        // send_ipi 在 machine.lock 内执行 — 见上方注释
        crate::host::task::send_ipi(pcpu_id);
        Ok(())
    }
}
```

关键约束：

1. **入队在 notify/IPI 之前**（保证 pending event 对目标 vCPU 可见后才唤醒）
2. **`notify_all()`** 持续使用第一阶段行为，不引入定向 wake API
3. **`add_vcpu_task`** 先注册到新 `irq_dispatcher`，再注册到旧 `vcpu_task_list`/`pending_interrupts`（防止 enqueue 时找不到 task 的窗口：如果旧字段先注册而 enqueue 恰好在 `register_vcpu_task` 之前到达新 dispatcher，会返回 NotFound）
4. **`send_ipi` 在 `machine.lock` 内调用**：与旧 `queue_interrupt`（两次 `with_runtime` 分别做 enqueue 和 notify，IPI 在锁外）不同。`machine` 锁是 `SpinNoIrq<Machine<...>>`，`send_ipi` 是快速 MMIO/MSR 写，不睡眠、不获取其他锁。单次锁获取消除了旧代码的 enqueue→notify 竞态窗口
5. **Task 迁移窗口**：`enqueue` 分两步获取锁（先 `vcpu_tasks` 读 `cpu_id()`，再 `queue.push`），task 可能在两步之间迁移到不同 pCPU。此时 `send_ipi` 发送到旧 pCPU，而 `notify_all()` 负责实际唤醒已迁移的目标 task。旧 pCPU 收到不相关的 IPI 无害（一次空的中断检查）

### 3. AxVM::prepare() 签名调整

**`prepare()` 定义在 `virtualization/axvm/src/vm/prepare.rs:36`**（不是 `mod.rs`）。修改该文件：

```rust
// 旧签名
pub fn prepare(&self) -> AxVmResult;
// 新签名
pub fn prepare(self: &Arc<Self>) -> AxVmResult;
```

当前 `prepare()` 的实现不需要内部修改：
- `self: &Arc<Self>` 在方法体内 auto-deref 为 `&AxVM`，所有现有代码（包括 `CurrentArch::init_vm(self, ...)`）不变
- 两个调用者 `start()`（`mod.rs:571`）和 `reset()`（`mod.rs:699`）已使用 `self: &Arc<Self>`，零适配成本
- Router 在后续模块（2-5）中创建，届时架构 `init_vm` 再按需改为接收 `&AxVMRef`

`prepare_with_factories()` 签名同样从 `&self` 改为 `&Arc<Self>`（`prepare.rs:41`）。

本 PR 中架构初始化文件不需要修改。

### 4. 移除和标注 dead_code

PR 1a 使用 `#[allow(dead_code)]`（非 `expect`），本次 PR 按以下策略处理：

| 位置 | 项 | 操作 | 原因 |
|------|-----|------|------|
| `model.rs` | `VirtualInterruptId` | 移除 `#[allow(dead_code)]` | 被 `PendingVcpuInterrupt` 的字段引用，PR 1b 中 `PendingVcpuInterrupt` 成为 live |
| `model.rs` | `PendingVcpuInterrupt` | 移除 `#[allow(dead_code)]` | 被 `dispatch_vcpu_interrupt` 和 `VmInterruptSender::send` 使用 |
| `dispatcher.rs` | `VcpuIrqDispatcher` (struct) | 移除 `#[allow(dead_code)]` | 被 `VmRuntimeHandle` 作为字段使用 |
| `dispatcher.rs` | `new()` | 移除 `#[allow(dead_code)]` | 被 `VmRuntimeHandle::new()` 调用 |
| `dispatcher.rs` | `register_vcpu_task()` | 移除 `#[allow(dead_code)]` | 被 `VmRuntimeHandle::add_vcpu_task()` 调用 |
| `dispatcher.rs` | `enqueue()` | 移除 `#[allow(dead_code)]` | 被 `dispatch_vcpu_interrupt()` 调用 |
| `dispatcher.rs` | `drain()` | **保留** `#[allow(dead_code)]` | PR 1c 才被 vCPU run loop 调用 |
| `sender.rs` | `VmInterruptSender`, `new()`, `send()` | 新增 `#[allow(dead_code)]` | Router 在模块 2+ 中创建 sender |
| `mod.rs` (vm) | `dispatch_vcpu_interrupt()` | 新增 `#[allow(dead_code)]` | 无调用者直到模块 2+ |
| `mod.rs` (vm) | `irq_dispatcher()` getter | 新增 `#[allow(dead_code)]` | PR 1c 才被 run loop 使用 |

## 文件变更清单

| 文件 | 操作 | 说明 |
|---|---|---|
| `virtualization/axvm/src/irq/sender.rs` | 新增 | `VmInterruptSender` 实现 |
| `virtualization/axvm/src/irq/mod.rs` | 修改 | 添加 `pub(crate) mod sender;` |
| `virtualization/axvm/src/vm/mod.rs` | 修改 | `VmRuntimeHandle` 集成 dispatcher、`dispatch_vcpu_interrupt`、`irq_dispatcher()` getter；新增 `use crate::irq::model::PendingVcpuInterrupt` |
| `virtualization/axvm/src/vm/prepare.rs` | 修改 | `prepare()` + `prepare_with_factories()` 签名从 `&self` 改为 `&Arc<Self>` |
| `virtualization/axvm/src/irq/model.rs` | 修改 | 移除 `VirtualInterruptId` 和 `PendingVcpuInterrupt` 的 `#[allow(dead_code)]` |
| `virtualization/axvm/src/runtime/dispatcher.rs` | 修改 | 移除 struct/`new`/`register_vcpu_task`/`enqueue` 的 `#[allow(dead_code)]`；保留 `drain` 的 |

## 测试

| 测试 | 覆盖 |
|---|---|
| `test_sender_returns_error_when_vm_destroyed` | Weak::upgrade 失败 → NotFound |
| `test_sender_returns_error_when_vm_not_running` | VM Created/Stopped/Crashed → BadState |
| `test_dispatch_vcpu_interrupt_enqueue_before_notify` | 在 `dispatch_vcpu_interrupt` 调用后，检查 dispatcher 内部 pending 非空（间实验证入队在 notify 之前） |
| `test_dispatch_vcpu_interrupt_round_trip` | sender.send → 检查 dispatcher 内部状态 → drain 验证 |
| `test_sender_does_not_cache_old_runtime` | VM reset 后 sender 找到新 runtime（新的 `VmRuntimeHandle` 实例） |

由于 `send_ipi` 和 `notify_all` 涉及 host 调用，测试策略：
- **host-test 环境**（`feature = "host-test"`）：可直接测试 `VcpuIrqDispatcher` 的 `enqueue` → `drain` 往返，验证队列语义
- **send 路径**：通过构造最小 VM（Running 状态）、注册 vCPU task、调用 `sender.send()`，然后 drain dispatcher 验证 middle-state

建议测试放在：
- `virtualization/axvm/src/runtime/dispatcher.rs` 的 `#[cfg(test)]` 模块（已有 1a 测试，扩展）
- `virtualization/test_crates/virtualization-tests/tests/axvm_irq.rs`（已有 test infra，用于集成测试）

## 退出条件

- [ ] 四种架构编译通过
- [ ] 现有测试全部通过（新增通道尚未被调用，不影响旧路径）
- [ ] `cargo fmt --all --check` 和 `cargo xtask clippy --package axvm` 通过
- [ ] `dispatch_vcpu_interrupt` 可以被调用但当前无调用者（router 尚未迁移）
- [ ] 编译通过时必须无 dead_code 警告（仅允许 sender.rs、`dispatch_vcpu_interrupt`、`irq_dispatcher` 的 `#[allow(dead_code)]`）
