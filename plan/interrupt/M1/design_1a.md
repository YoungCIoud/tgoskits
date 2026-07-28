# PR 1a：中断递送类型与 VcpuIrqDispatcher

## 范围

纯增量 PR：只往代码库添加新的数据结构定义 + 单元测试，**零行为变更**。不修改任何现有代码路径。

## 背景

见 `plan/interrupt/design.md` 的模块 1 —— 共享资源与 runtime 递送通道。PR 1a 只做其中的类型定义和 dispatcher 数据结构部分。

## 设计

### 1. VirtualInterruptId 与 PendingVcpuInterrupt

新文件 `virtualization/axvm/src/irq/model.rs`：

```rust
use axdevice_base::InterruptTriggerMode;

/// 架构无关的虚拟中断标识。
///
/// 使用 `u32` 避免 x86 `u8` vector 限制扩散到 GIC（INTID 可达 1020+）、
/// PLIC 和 LoongArch。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct VirtualInterruptId(pub u32);

/// 待递送到目标 vCPU 的中断事件。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingVcpuInterrupt {
    pub id: VirtualInterruptId,
    pub trigger: InterruptTriggerMode,
}
```

- `VirtualInterruptId` 派生 `Ord`/`PartialOrd` 以备未来 `BTreeMap` 排序需求
- `PendingVcpuInterrupt` 携带 trigger mode，供 Router（模块 2-5）和架构注入路径在需要时区分 edge/level 语义
- 放在 `axvm/src/irq/` 下是因为 Router（`IrqSink` 实现者）和 `VmInterruptSender` 都属于 axvm crate

### 2. VcpuIrqDispatcher

新文件 `virtualization/axvm/src/runtime/dispatcher.rs`：

```rust
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use alloc::sync::Arc;
use crate::ax_err_type;
use crate::{AxTaskRef, AxVmResult};
use crate::irq::model::{PendingVcpuInterrupt, VirtualInterruptId};

/// Runtime-owned vCPU 中断队列。
///
/// 生命周期与一次 Running/Paused/Stopping runtime 相同。
/// dispatcher 锁内只做 map/queue 更新，不调用 wake、IPI 或外部回调。
pub struct VcpuIrqDispatcher {
    pending: Mutex<BTreeMap<usize, Vec<PendingVcpuInterrupt>>>,
    vcpu_tasks: Mutex<BTreeMap<usize, AxTaskRef>>,
}

impl VcpuIrqDispatcher {
    pub fn new() -> Self;

    /// 注册 vCPU 任务，后续 `enqueue` 才能找到目标 pCPU。
    pub fn register_vcpu_task(&self, vcpu_id: usize, task: AxTaskRef);

    /// 入队一个待递送中断，返回目标 vCPU 当前所在的 pCPU ID。
    ///
    /// 两个内部锁（`vcpu_tasks` 和 `pending`）**顺序持有**而非同时持有：
    /// 1. 锁 `vcpu_tasks`，获取 `task.cpu_id()`，释放
    /// 2. 锁 `pending`，push interrupt，释放
    ///
    /// 步骤 1→2 之间存在 task 迁移窗口（pCPU 可能变化），
    /// 但 `notify_all()` + `send_ipi` 保证了最终递送，属于第一阶段可接受行为。
    /// 锁内不调用 wake、IPI 或外部回调。
    pub fn enqueue(
        &self,
        vcpu_id: usize,
        interrupt: PendingVcpuInterrupt,
    ) -> AxVmResult<usize>;

    /// 取出并清空指定 vCPU 的待递送中断队列。
    ///
    /// 调用者（vCPU run loop）在目标 pCPU 上消费这些中断。
    pub fn drain(&self, vcpu_id: usize) -> Vec<PendingVcpuInterrupt>;
}
```

**与 design.md 中签名的差异**：`register_vcpu_task` 严格按 design 中签名。`enqueue` 传入 `PendingVcpuInterrupt` 而非 `(VirtualInterruptId, InterruptTriggerMode)`，保持类型聚合。

**当前 `vmcpu_task_list` 的关系**：`VmRuntimeHandle` 中已有 `vcpu_task_list: Mutex<BTreeMap<usize, AxTaskRef>>`。`VcpuIrqDispatcher` 维护自己的 `vcpu_tasks`，独立于旧字段。在 PR 1b 集成时两处同时注册。迁移完成后（最终清理阶段）旧的 `vcpu_task_list` 随旧队列一起移除。

### 3. 模块注册

- `virtualization/axvm/src/irq/mod.rs`：添加 `pub(crate) mod model;`
- `virtualization/axvm/src/runtime/mod.rs`：添加 `mod dispatcher;`
  - re-export：`pub(crate) use dispatcher::VcpuIrqDispatcher;`
  - 或在 `VmRuntimeHandle` 中直接 `use super::dispatcher::VcpuIrqDispatcher;`
- 两个新模块标 `#[expect(dead_code, reason = "wired in PR 1b/1c")]` —— `model.rs` 的类型被 `dispatcher.rs` 消费，在本 PR 内不 dead；`dispatcher` 模块整体作为 dead_code 标注，因为 `VcpuIrqDispatcher` 在 PR 1b 才被 `VmRuntimeHandle` 使用

## 文件变更清单

| 文件 | 操作 | 说明 |
|---|---|---|
| `virtualization/axvm/src/irq/model.rs` | 新增 | `VirtualInterruptId`, `PendingVcpuInterrupt` |
| `virtualization/axvm/src/irq/mod.rs` | 修改 | 添加 `pub(crate) mod model;` |
| `virtualization/axvm/src/runtime/dispatcher.rs` | 新增 | `VcpuIrqDispatcher` 完整实现 |
| `virtualization/axvm/src/runtime/mod.rs` | 修改 | 注册 dispatcher 模块 |
| `virtualization/test_crates/virtualization-tests/tests/axvm_irq.rs` | 修改 | 添加 dispatcher 单元测试 |

## 测试

所有测试放在 `axvm_irq.rs` 中（或视长度拆出独立测试文件）。

| 测试 | 覆盖 |
|---|---|
| `test_dispatcher_enqueue_preserves_fifo_order` | 多个 interrupt 入队后 drain 顺序为 FIFO |
| `test_dispatcher_isolates_vcpus` | vCPU 0 和 vCPU 1 的队列互不干扰 |
| `test_dispatcher_drain_empties_queue` | drain 后队列为空 |
| `test_dispatcher_double_drain_returns_empty` | 连续两次 drain，第二次返回空 |
| `test_dispatcher_enqueue_unregistered_vcpu_returns_error` | 未注册 vCPU 调用 enqueue 返回 `NotFound` |
| `test_dispatcher_trigger_mode_round_trips` | Edge/Level trigger 在 enqueue→drain 后保持 |

## 退出条件

- [ ] 四种架构编译通过（`cargo xtask axvisor build --all-archs` 或等价命令）
- [ ] `cargo test` 通过，新测试全部 green
- [ ] `cargo fmt --all --check` 和 `cargo xtask clippy --package axvm` 通过
- [ ] 没有修改任何现有代码路径（零行为变更）
