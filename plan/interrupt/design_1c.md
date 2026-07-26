# PR 1c：ArchOps::inject_vcpu_interrupt + vCPU run loop 消费新队列

## 范围

vCPU run loop 开始 drain 新 `VcpuIrqDispatcher`，新旧队列并存消费。新增 `ArchOps::inject_vcpu_interrupt()` trait 方法，提供从 `PendingVcpuInterrupt` 到架构注入的统一入口。

## 前置依赖

- PR 1a（#1661）+ PR 1b 已合入（类型、dispatcher、sender、dispatch_vcpu_interrupt、`irq_dispatcher()` getter 均已就绪）
- PR 1b 保留的 `#[allow(dead_code)]`：`drain()`（dispatcher.rs）、`irq_dispatcher()`（mod.rs）、`dispatch_vcpu_interrupt()`（mod.rs）、`VmInterruptSender`（sender.rs）

## 设计

### 1. ArchOps::inject_vcpu_interrupt

修改 `virtualization/axvm/src/architecture/ops.rs`，在 `ArchOps` trait 中新增：

```rust
/// 将已排队的 `PendingVcpuInterrupt` 注入到目标 vCPU。
///
/// 此方法在**目标 vCPU 的 run loop** 中调用，保证对 banked system register
/// （GIC LR、x86 vLAPIC 等）的访问发生在正确的 pCPU 上。
///
/// 默认实现调用
/// `vcpu.inject_interrupt_with_trigger(interrupt.id.0 as usize, interrupt.trigger)`，
/// 把 trigger 元数据保留到架构 adapter。
fn inject_vcpu_interrupt(
    vcpu: &AxVCpuRef<Self::VCpu>,
    interrupt: PendingVcpuInterrupt,
) -> AxVmResult {
    vcpu.inject_interrupt_with_trigger(interrupt.id.0 as usize, interrupt.trigger)
}
```

注意：
- 参数**没有** `vm: &AxVMRef`。Router 在入队前已完成所有 VM 级决策（路由、校验）。`inject_vcpu_interrupt` 只做单 vCPU 注入，不查 VM 状态。
- 与旧 `inject_pending_interrupt(vm, vcpu, interrupt)` 的签名差异：
  - 旧方法需要 `vm` 来访问 x86 IOAPIC 轮询（本次重构要移除的路径）
  - 新方法不需要 `vm`，因为所有 VM 级工作已在 Router + sender + dispatcher 中完成
- `interrupt.id.0 as usize`：`VirtualInterruptId` 内部是 `u32`，`vcpu.inject_interrupt()` 接受 `usize`。在 64 位平台上无损。

- 需要在 `ops.rs` 文件顶部添加 `use crate::irq::model::PendingVcpuInterrupt;`

### 2. 架构 adapter 的 trigger 策略

- x86 的 `VmArchVcpuOps::inject_interrupt_with_trigger` 把 `EdgeTriggered`/`LevelTriggered` 映射为 backend 的 `level_triggered: bool`，使 VMX 与 SVM 都能在注入时更新 vLAPIC TMR/EOI 语义。
- AArch64、RISC-V、LoongArch 的 adapter 显式实现 trigger-aware 方法。对应 Router/controller 在产生待注入 INTID/vector 前消费 line trigger 语义，最终的 GIC LR、虚拟 pending bit 或 guest vector 注入本身不再区分模式。
- `VmArchVcpuOps` 不保留 release 模式下静默退化到普通 `inject_interrupt` 的默认实现；新增 adapter 必须明确选择策略。

### 3. vCPU run loop 消费新队列

修改 `virtualization/axvm/src/architecture/ops.rs` 中的 `run_vcpu` 方法（当前约第 121 行 `vcpu.with_current_cpu_set` 内部）：

```rust
fn run_vcpu(
    vm: &crate::AxVMRef,
    vcpu: &crate::vm::AxVCpuRef<Self::VCpu>,
) -> AxVmResult<VcpuRunAction>
where
    Self: Sized,
{
    // ... 现有 bind 逻辑不变 ...

    let run_result = vcpu.with_current_cpu_set(|| -> AxVmResult<_> {
        loop {
            // 旧路径：drain 旧队列（保留直到所有生产者迁移）
            crate::runtime::vcpus::inject_pending_interrupts::<Self>(vm.id(), vcpu_id, vcpu);

            // 新路径：通过可单测步骤 drain 新 dispatcher 并逐项注入
            drain_and_inject_dispatched_interrupts::<Self>(vm, vcpu_id, vcpu);

            let exit = vcpu.run()?;
            trace!("{exit:#x?}");
            match Self::handle_vcpu_exit_bound(vm, vcpu, exit)? {
                BoundVcpuExit::Continue => continue,
                action => break Ok(action),
            }
        }
    });
    // ... 现有 unbind 逻辑不变 ...
}
```

关键约束：
- 新旧队列 drain 顺序：先旧后新（旧队列的语义在迁移期保持不变）
- 新队列 drain 失败（runtime 不存在）时返回空列表，不中断 run loop
- 单个 interrupt 注入失败时记录 warn 日志（含 VM ID、vCPU ID、interrupt ID），继续处理后续 interrupt
- dispatcher 在逐项注入前一次性 drain；即使某项注入失败，该项也不会在下一轮重复消费
- `irq_dispatcher()` getter 已在 PR 1b 暴露；`drain()` 在 `VcpuIrqDispatcher` 上

### 4. 当前行为验证

在 PR 1c 合入时，新的 `irq_dispatcher` 队列始终为空（尚无 Router 使用 `VmInterruptSender`），因此：
- drain 返回 `Vec::new()`
- 循环不执行任何 `inject_vcpu_interrupt` 调用
- guest 行为与 PR 之前完全一致

### 5. dead_code 标注调整

| 位置 | 项 | 操作 | 原因 |
|------|-----|------|------|
| `dispatcher.rs` | `drain()` | 移除 `#[allow(dead_code)]` | 被 run loop 调用 |
| `mod.rs` (vm) | `irq_dispatcher()` getter | 移除 `#[allow(dead_code)]` | 被 run loop 调用 |
| `mod.rs` (vm) | `dispatch_vcpu_interrupt()` | **保留** `#[allow(dead_code)]` | 模块 2+ 才被 Router 调用 |
| `sender.rs` | `VmInterruptSender`, `new()`, `send()` | **保留** `#[allow(dead_code)]` | 模块 2+ 才被 Router 使用 |

## 文件变更清单

| 文件 | 操作 | 说明 |
|---|---|---|
| `virtualization/axvm/src/architecture/ops.rs` | 修改 | 新增 `inject_vcpu_interrupt` 默认实现（含 `use PendingVcpuInterrupt`）；run loop drain 新队列 |
| `virtualization/axvm/src/runtime/dispatcher.rs` | 修改 | 移除 `drain()` 的 `#[allow(dead_code)]` |
| `virtualization/axvm/src/vm/mod.rs` | 修改 | 移除 `irq_dispatcher()` getter 的 `#[allow(dead_code)]` |

## 测试

| 测试 | 覆盖 |
|---|---|
| `inject_vcpu_interrupt_preserves_level_trigger_at_backend_boundary` | fake backend 验证 dispatcher→drain→`ArchOps` 后 level trigger 不退化 |
| `dispatcher_drain_injects_fifo_once_and_consumes_failed_entries` | FIFO、每项仅注入一次、失败项不重试、二次 drain 为空 |
| `maps_edge_and_level_triggers_to_x86_backend_modes` | x86 edge/level 到 backend bool 的映射 |
| 既有 dispatcher/queue 测试 | FIFO、vCPU 隔离、trigger 往返、drain 清空 |

## 退出条件

- [ ] 四种架构编译通过
- [ ] 现有测试全部通过（新旧队列并存，新队列为空，guest 行为不变）
- [ ] `cargo fmt --all --check` 和 `cargo xtask clippy --package axvm` 通过
- [ ] 新 dispatcher 队列在 drain 期间行为正确（空队列 drain 返回空，runtime 不存在时安全降级）
- [ ] 模块 1 整体完成：共享类型、sender、runtime dispatcher、run-loop consumer 与迁移期双队列都有可执行证据；Router 和架构初始化入口留在模块 2-5
