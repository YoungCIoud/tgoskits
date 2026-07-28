# PR 1bc：完整 sender→dispatcher→run loop 中断投递通道

## 要解决的问题

PR #1661 提供了 `VirtualInterruptId`、`PendingVcpuInterrupt`、`VcpuIrqDispatcher`、`VcpuInterruptQueue` 等基础类型和数据结构。本 PR 将这些组件集成到运行时中：

1. 创建 Router→runtime 的稳定发送通道 `VmInterruptSender`
2. 将 `VcpuIrqDispatcher` 嵌入 `VmRuntimeHandle`
3. 新增 `ArchOps::inject_vcpu_interrupt` 并在 vCPU run loop 中 drain 新队列

此时整条路径已贯通：`send()` → `dispatch_vcpu_interrupt` → `enqueue` → `drain` → `inject_vcpu_interrupt` → `vcpu.inject_interrupt_with_trigger()`。新旧队列并存，当前无 Router 使用 sender，新队列始终为空，不改变现有中断生产路径。

## 改动内容

### 1. 新增 `VmInterruptSender`（`virtualization/axvm/src/irq/sender.rs`）

Router 到 runtime 的稳定发送通道，只持有 `Weak<AxVM>`，不缓存 runtime 或 dispatcher 引用：

- `new(vm: &Arc<AxVM>)` — 通过 `Arc::downgrade` 构造
- `send(vcpu_id, interrupt)` — 三步流程：
  1. `Weak::upgrade` 失败返回 `NotFound`（VM 已销毁）
  2. 在同一次 `machine` 锁内检查 VM 状态并 clone 当前 runtime，仅 `Running/Paused` 接受中断，否则返回 `BadState`
  3. 在释放 `machine` 锁后调用 `runtime.dispatch_vcpu_interrupt(vcpu_id, interrupt)`

sender 不缓存 runtime 或 dispatcher。stop/reset/start 后，同一个 sender 下一次发送会重新选择替换后的 runtime。

### 2. 集成 dispatcher 到 `VmRuntimeHandle`（`virtualization/axvm/src/vm/mod.rs`）

- 新增字段 `irq_dispatcher: VcpuIrqDispatcher`
- `new()` 初始化该字段
- `add_vcpu_task()` 先注册到新 `irq_dispatcher`，再注册旧字段。防止 enqueue 在注册完成前到达时返回 `NotFound`
- 新增 `dispatch_vcpu_interrupt()` — 严格执行 enqueue → notify_all → send_ipi
- 提取内部编排步骤，确保 enqueue 失败时不 notify、不发送 IPI；dispatcher 队列锁在两个回调前已经释放
- 新增 `irq_dispatcher()` getter 供 vCPU run loop drain 使用

### 3. `ArchOps::inject_vcpu_interrupt` + vCPU run loop 消费（`virtualization/axvm/src/architecture/ops.rs`）

- 在 `AxVCpu` 增加 trigger-aware 注入包装并统一 backend 错误映射
- 在 `ArchOps` trait 中新增 `inject_vcpu_interrupt(vcpu, interrupt: PendingVcpuInterrupt) -> AxVmResult`，默认实现同时传递 interrupt ID 和 trigger
- `VmArchVcpuOps::inject_interrupt_with_trigger` 不再提供 release 模式下静默丢弃 level trigger 的默认回退
- x86 把 edge/level 映射到 VMX/SVM backend 的 `level_triggered` 参数，维护 vLAPIC TMR/EOI 语义
- AArch64、RISC-V、LoongArch adapter 显式说明 trigger 由对应 Router/controller 消费，最终 vCPU 注入操作本身不区分模式
- 参数不包含 `vm`：Router 在入队前已完成所有 VM 级决策，`inject_vcpu_interrupt` 只做单 vCPU 注入
- vCPU run loop 在旧 `inject_pending_interrupts` 之后调用可单测的 dispatcher drain→逐项注入步骤：
  - drain 失败时 warn 日志 + 降级为空列表，不中断 run loop
  - 单个 interrupt 注入失败时 warn 日志 + 继续处理后续
  - 新旧队列 drain 顺序：先旧后新

### 4. `AxVM::prepare()` 边界（`virtualization/axvm/src/vm/prepare.rs`）

- `prepare(&self)` → `prepare(self: &Arc<Self>)`
- `prepare_with_factories(&self, ...)` → `prepare_with_factories(self: &Arc<Self>, ...)`
- 方法体内代码不变（`&Arc<Self>` 自动 deref 为 `&AxVM`）
- 两个调用者 `start()` / `reset()` 已使用 `self: &Arc<Self>`，零适配成本
- 本模块不提前修改架构 `init_vm`；AArch64、LoongArch、x86 在各自 Router 模块构造 sender 时再获取 `AxVMRef`，RISC-V 仅在确有 sender/Router 消费者时调整

### 5. dead_code 注解

| 位置 | 项 | 状态 |
|------|-----|------|
| `model.rs` | `VirtualInterruptId`, `PendingVcpuInterrupt` | 已移除（被 dispatcher/sender/ArchOps 使用） |
| `dispatcher.rs` | struct, `new()`, `register_vcpu_task()`, `enqueue()` | 已移除（被 `VmRuntimeHandle` 使用） |
| `dispatcher.rs` | `drain()` | 已移除（被 run loop 调用） |
| `vm/mod.rs` | `irq_dispatcher()` | 已移除（被 run loop 调用） |
| `vm/mod.rs` | `dispatch_vcpu_interrupt()` | 带阶段原因的 `#[expect(dead_code)]`（模块 2+ Router 使用） |
| `sender.rs` | struct, `new()`, `send()` | 带阶段原因的 `#[expect(dead_code)]`（模块 2+ Router 使用） |

### 6. 测试

**dispatcher 错误路径**：
- `enqueue_unregistered_vcpu_returns_error`
- `enqueue_multiple_unregistered_vcpus_all_return_error`
- `unregistered_vcpu_still_fails_when_others_registered`

**enqueue→drain 往返**（通过 `register_test_vcpu` 辅助方法绕过 ArceOS task 依赖）：
- `round_trip_enqueue_drain_preserves_fifo_order`
- `round_trip_enqueue_drain_isolates_vcpus`
- `round_trip_drain_empties_queue`
- `round_trip_double_drain_returns_empty`
- `round_trip_trigger_mode_preserved`
- `enqueue_returns_registered_cpu_id`

**run loop 与 trigger**：
- fake `VmArchVcpuOps` 验证 level trigger 经 dispatcher/drain/`ArchOps` 后仍为 level
- FIFO 中每项只注入一次；注入失败不阻断后续项，也不会在下一轮重复消费
- x86 edge/level 分别映射到 backend 的 false/true

**sender/runtime 生命周期**：
- Running、Paused 且 vCPU 已注册时选择 runtime 并成功入队
- Ready、Stopped、Destroyed 和 runtime 不存在时返回 `BadState`
- 未注册 vCPU 返回 `NotFound`
- Weak 对应目标释放后返回 `NotFound`
- stop/reset/start 后稳定选择逻辑只向新 runtime 入队，旧 dispatcher 不再变化

**调度顺序**：
- 事件记录器验证 enqueue 完成后才 notify，notify 后才发送 IPI
- notify 回调可立即 drain dispatcher，证明队列锁已经释放
- enqueue 失败时不得 notify 或发送 IPI

## 关键设计决策

1. **入队顺序**：`add_vcpu_task` 先注册新 dispatcher 再注册旧字段
2. **回调顺序**：runtime 在 dispatcher 锁外执行 notify 和 host IPI，失败入队不会产生回调
3. **不缓存 runtime**：每次 `send()` 通过 `Weak::upgrade` 并在单次 machine 锁内选择当前 runtime，VM stop/start/reset 后不会沿用旧 dispatcher
4. **新旧队列并存**：`VcpuIrqDispatcher` 和旧 `pending_interrupts` 独立维护各自的 task 映射，互不干扰
5. **run loop drain 顺序**：先旧后新，旧队列语义在迁移期保持不变
6. **模块边界**：模块 1 以共享类型、sender、dispatcher、consumer 和双队列证据为退出条件，不要求 Router 或架构 `init_vm` 提前迁移

## 本地验证

- `cargo fmt --all --check`：通过
- `cargo test -p axvm --features host-test --lib`：128 个测试通过
- `cargo xtask clippy --package axvm-types`：通过
- `cargo xtask clippy --package axvm`：6 组 feature 检查全部通过
- Axvisor x86_64 VMX、x86_64 SVM、AArch64、RISC-V 显式 test-suite build config：通过
- LoongArch build：本机仅有 Clang 15 且未安装 `loongarch64-linux-musl-cc`，在 `ax-posix-api` bindgen 阶段因 Clang 不识别 LoongArch target triple 而停止，尚未编译到 `axvm`
- x86 QEMU smoke：本机无 `/dev/kvm`，且当前宿主为 AMD，无法执行要求 KVM 的 VMX/SVM smoke；保留给对应 Intel/AMD self-hosted CI
- PR CI：31 项通过、30 项按矩阵条件跳过、0 失败；其中 x86 VMX/SVM self-hosted smoke 及 AArch64、RISC-V、LoongArch Axvisor QEMU 均通过
