# PR 标题

feat(axvm): add virtual interrupt model types and per-vCPU dispatch queue

## 要解决的问题

AxVisor 当前设备中断路径在各架构上分别演化，主要存在以下问题：

1. `IrqLine` 到 vCPU 注入的路径在不同架构上走的通道不一致（x86 运行时直接理解 IOAPIC、串口、PIT 细节；LoongArch 依赖架构特定队列变体）
2. 跨 vCPU 中断递送依赖 `VmRuntimeHandle::pending_interrupts`，设备中断、IPI 和直通 IRQ 的递送路径不统一
3. 没有一个清晰的通用边界：设备 → Router → runtime 队列 → 目标 vCPU 注入

本次变更是整体重构的第一步，**纯增量**，不修改任何现有代码路径，只添加日后接通整条通道所需的基础类型和数据结构。

## 改动内容

### 1. 新增 `irq::model` — 虚拟中断类型定义

`virtualization/axvm/src/irq/model.rs`：

- `VirtualInterruptId(u32)` — 架构无关的虚拟中断 ID，使用 `u32` 避免 x86 `u8` vector 限制扩散到 GIC（INTID 可达 1020+）、PLIC 和 LoongArch
- `PendingVcpuInterrupt` — 携带 `VirtualInterruptId` 和 `InterruptTriggerMode`，供架构 Router 和注入路径区分 edge/level 语义

### 2. 新增 `runtime::queue` — 可 host-test 的纯队列

`virtualization/axvm/src/runtime/queue.rs`：

- `VcpuInterruptQueue` — 从 `VcpuIrqDispatcher` 中拆出的纯 pending 队列，**不含 `AxTaskRef` 依赖**，因此可以在 host 上测试 FIFO、vCPU 隔离、drain 清空等队列语义

### 3. 新增 `runtime::dispatcher` — per-vCPU 中断调度器

`virtualization/axvm/src/runtime/dispatcher.rs`：

- `VcpuIrqDispatcher` — runtime 持有的 vCPU 中断队列，内部组合了 `VcpuInterruptQueue`（纯队列）和 `vcpu_tasks`（任务映射）。生命周期与一次 Running/Paused/Stopping runtime 绑定
- `new()` — 空初始化，将在 VM 进入 Running 状态时由 `VmRuntimeHandle::new` 调用
- `register_vcpu_task(vcpu_id, task)` — 注册 vCPU 任务，使 `enqueue` 能查到目标 pCPU ID
- `enqueue(vcpu_id, interrupt) -> AxVmResult<usize>` — 入队一个待递送中断，返回目标 pCPU ID。两个内部锁（`vcpu_tasks` → `queue`）顺序持有，锁内不调用 wake/IPI/外部回调。vCPU 未注册返回错误
- `drain(vcpu_id) -> Vec<PendingVcpuInterrupt>` — 取出并清空指定 vCPU 的队列，委托给 `VcpuInterruptQueue::drain`，由 vCPU run loop 在进入 guest 前消费

### 4. 新增 `host-test` feature

`virtualization/axvm/Cargo.toml`：

- 新增 `host-test = ["ax-kspin/host-test", "ax-kernel-guard/host-test"]` feature。启用后 `SpinNoIrq::lock()` 的 arch ops 变成 no-op，使队列测试可在 host 上安全执行。CI 使用 `--all-features` 运行 axvm 测试，此 feature 自动启用。

### 5. 模块注册

- `irq/mod.rs` — 添加 `pub(crate) mod model;`
- `runtime/mod.rs` — 添加 `mod queue;`、`mod dispatcher;` 和 `pub(crate) use dispatcher::VcpuIrqDispatcher;`

## 每步解决方案的逻辑

**类型选择：采用 `VirtualInterruptId(u32)` 而非 usize 或 u8**

GIC 的 INTID 区间可达 1020+，PLIC 和 LoongArch 使用较大编号空间。x86 使用 u8 vector（0-255），但不应让这个限制通过类型扩散到所有架构。u32 统一表示、各架构 Router 在注入前转为目标架构识别的编号。

**队列与任务分离：`VcpuInterruptQueue` vs `VcpuIrqDispatcher`**

`VcpuIrqDispatcher` 同时持有 pending 队列和 `AxTaskRef` 映射。`AxTaskRef`（`Arc<AxTask>`）会拉入 axtask → percpu → someboot 等 ArceOS 内核依赖链，host test 二进制无法链接。将纯队列逻辑拆为 `VcpuInterruptQueue`（只含 `PendingVcpuInterrupt`、`BTreeMap`、`SpinNoIrq`），配合 `host-test` feature，使其可在 host 上运行真实测试。

**队列设计：两个独立 Mutex，顺序持有而非同时持有**

`vcpu_tasks` 和 `queue` 分开加锁，每个锁内只做 BTreeMap/Vec 操作。这避免了：
1. 锁内调用 wake/IPI（可能死锁或优先级反转）
2. 锁粒度太粗影响并发

task 迁移窗口（enqueue 步骤 1→2 之间 pCPU 可能变化）是已知的，由调用者（`dispatch_vcpu_interrupt` 中的 `notify_all()` + `send_ipi`）保证最终递送正确。

**`drain()` 使用 `get_mut` + `core::mem::take` 而非 `remove`**

与 `VmRuntimeHandle::drain_pending_interrupts` 保持一致。保留空 Vec 避免后续 enqueue 时重新创建 entry，减少分配次数。

**零行为变更的设计原则**

所有新类型和方法当前为 dead code（标注 `#[allow(dead_code)]`）。vCPU run loop 继续只 drain 旧队列，新 `VcpuIrqDispatcher` 队列始终为空。后续 PR 将逐步：
1. 把 dispatcher 嵌入 `VmRuntimeHandle`，加入 `register_vcpu_task` 和 `dispatch_vcpu_interrupt`
2. 新增 `ArchOps::inject_vcpu_interrupt`，让 vCPU run loop 同时 drain 新旧队列
3. 各架构创建 Router 并通过新 sender/dispatcher 通道连线

## 测试

5 个测试在 `virtualization/axvm/src/runtime/queue.rs` 的 `#[cfg(test)]` 中，通过 `cargo test --all-features` 实际执行：

| 测试 | 覆盖 |
|---|---|
| `push_preserves_fifo_order` | 多个 interrupt push 后 drain 顺序为 FIFO |
| `isolates_vcpus` | vCPU 0 和 vCPU 1 的队列互不干扰 |
| `drain_empties_queue` | drain 后队列为空 |
| `double_drain_returns_empty` | 连续两次 drain，第二次返回空 |
| `trigger_mode_round_trips` | Edge/Level trigger 在 push→drain 后保持 |

`enqueue_unregistered_vcpu_returns_error` 依赖 `AxTaskRef` 注册，不在纯队列层测试。留到 `VcpuIrqDispatcher` 集成到 `VmRuntimeHandle` 时通过 VM 级 harness 覆盖。

## 验证

```bash
cargo fmt --all --check          # 通过
cargo xtask clippy --package axvm  # 5 个 feature 组合全部通过（含 host-test）
cargo test --manifest-path virtualization/axvm/Cargo.toml --all-features  # 107 测试通过（含 5 个新测试）
```
