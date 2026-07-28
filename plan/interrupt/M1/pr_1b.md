## 要解决的问题

合入的 PR #1661 提供了 `VirtualInterruptId`、`PendingVcpuInterrupt`、`VcpuIrqDispatcher`、`VcpuInterruptQueue` 等基础类型和数据结构。本 PR 将这些组件集成到运行时中，并创建 Router→runtime 的稳定发送通道。

此时发送通道存在但尚无消费者（vCPU run loop 尚未 drain 新队列，等待下一个 PR），新旧队列并存，零行为变更。

## 改动内容

### 1. 新增 `VmInterruptSender`（`virtualization/axvm/src/irq/sender.rs`）

Router 到 runtime 的稳定发送通道，只持有 `Weak<AxVM>`，不缓存 runtime 或 dispatcher 引用：

- `new(vm: &Arc<AxVM>)` — 通过 `Arc::downgrade` 构造
- `send(vcpu_id, interrupt)` — 三步流程：
  1. `Weak::upgrade` 失败返回 `NotFound`（VM 已销毁）
  2. 检查 VM 状态，仅 `Running/Paused` 接受中断，否则返回 `BadState`
  3. `vm.with_runtime(|rt| rt.dispatch_vcpu_interrupt(vcpu_id, interrupt))`
- 整体标记 `#[allow(dead_code)]`，后续实现各个架构的 Router 时在 Router 中创建 sender

### 2. 集成 dispatcher 到 `VmRuntimeHandle`（`virtualization/axvm/src/vm/mod.rs`）

- 新增字段 `irq_dispatcher: VcpuIrqDispatcher`
- `new()` 初始化该字段
- `add_vcpu_task()` 先调用 `self.irq_dispatcher.register_vcpu_task(vcpu_id, vcpu_task.clone())`，再注册旧字段。关键：新 dispatcher 先准备好，防止 enqueue 在注册完成前到达新 dispatcher 时返回 `NotFound`
- 新增 `dispatch_vcpu_interrupt()` — enqueue → notify_all → send_ipi，三合一在单次 `machine.lock` 获取中完成，消除旧代码两次 `with_runtime` 的入队→notify 竞态窗口。`send_ipi` 是快速 MMIO/MSR 写，在 `SpinNoIrq` 临界区内调用安全
- 新增 `irq_dispatcher()` getter 供下一个 PR vCPU run loop drain 使用

### 3. `AxVM::prepare()` 签名调整（`virtualization/axvm/src/vm/prepare.rs`）

- `prepare(&self)` → `prepare(self: &Arc<Self>)`
- `prepare_with_factories(&self, ...)` → `prepare_with_factories(self: &Arc<Self>, ...)`
- 方法体内代码不变（`&Arc<Self>` 自动 deref 为 `&AxVM`）
- 两个调用者 `start()` / `reset()` 已使用 `self: &Arc<Self>`，零适配成本
- Router 在后续模块中创建时，架构 `init_vm` 可通过 `Arc` 构造 `VmInterruptSender`

### 4. dead_code 注解更新

| 位置 | 项 | 操作 |
|------|-----|------|
| `model.rs` | `VirtualInterruptId`, `PendingVcpuInterrupt` | 移除 `#[allow(dead_code)]` |
| `dispatcher.rs` | struct, `new()`, `register_vcpu_task()`, `enqueue()` | 移除 `#[allow(dead_code)]` |
| `dispatcher.rs` | `drain()` | 保留 `#[allow(dead_code)]` |
| `sender.rs` | struct, `new()`, `send()` | 新增 `#[allow(dead_code)]` |
| `mod.rs` (vm) | `dispatch_vcpu_interrupt()`, `irq_dispatcher()` | 新增 `#[allow(dead_code)]` |

### 5. 测试

扩展了 `dispatcher.rs` 的 `#[cfg(all(test, feature = "host-test"))]` 测试模块：

**错误路径**：
- `enqueue_unregistered_vcpu_returns_error`
- `enqueue_multiple_unregistered_vcpus_all_return_error`
- `unregistered_vcpu_still_fails_when_others_registered`

**enqueue→drain 往返**（通过 `register_test_vcpu` 辅助方法绕过 ArceOS task 依赖）：
- `round_trip_enqueue_drain_preserves_fifo_order` — 验证 FIFO 入队/出队
- `round_trip_enqueue_drain_isolates_vcpus` — 验证不同 vCPU 队列独立
- `round_trip_drain_empties_queue` — 验证 drain 后队列为空
- `round_trip_double_drain_returns_empty` — 验证二次 drain 返回空
- `round_trip_trigger_mode_preserved` — 验证 edge/level trigger 在往返中保留
- `enqueue_returns_registered_cpu_id` — 验证 enqueue 返回正确的 pCPU ID

`register_test_vcpu` 通过 `#[cfg(all(test, feature = "host-test"))]` 在 host-test 编译时在 `VcpuIrqDispatcher` 内部创建一个独立的 `test_vcpu_cpu_ids` 映射，不污染生产代码路径（`enqueue` 优先查询此映射，未命中时走正常 `vcpu_tasks` 查找）。

## 关键设计决策

1. **入队顺序**：`add_vcpu_task` 先注册新 dispatcher 再注册旧字段，确保新路径对任何 enqueue 窗口都是就绪的
2. **锁内 send_ipi**：与旧代码不同，`dispatch_vcpu_interrupt` 在 `machine.lock` 内调用 `send_ipi`。该函数是快速 MMIO/MSR 写，不睡眠、不获取其他锁，在 `SpinNoIrq` 临界区内安全
3. **不缓存 runtime**：每次 `send()` 通过 `Weak::upgrade` + `with_runtime` 查找当前 runtime，VM stop/start/reset 后不会引用旧 dispatcher
4. **新旧队列并存**：新旧 `VcpuIrqDispatcher` 和旧 `pending_interrupts` 独立维护各自的 `vcpu_tasks` 映射，互不干扰
