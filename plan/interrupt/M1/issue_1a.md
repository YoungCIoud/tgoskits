# PR 1a issue

- [x] 新增 `virtualization/axvm/src/irq/model.rs`：`VirtualInterruptId(u32)`, `PendingVcpuInterrupt`
- [x] `virtualization/axvm/src/irq/mod.rs` 添加 `pub(crate) mod model;`
- [x] 新增 `virtualization/axvm/src/runtime/dispatcher.rs`：`VcpuIrqDispatcher` — `new`, `register_vcpu_task`, `enqueue`, `drain`
- [x] `virtualization/axvm/src/runtime/mod.rs` 注册 dispatcher 模块
- [x] `enqueue()` 仅更新队列并返回 pCPU ID，不在锁内调用 wake/IPI/外部回调
- [x] vCPU task 未注册时 `enqueue()` 返回 `NotFound`
- [x] 标记 `#[allow(dead_code)]`（实际实现用 `allow` 而非 design 中的 `expect`）
- [x] 单元测试：FIFO 顺序、vCPU 隔离、drain 清空、重复 drain 为空（`queue.rs`）、未注册 vCPU 错误（`dispatcher.rs`）、trigger mode 往返
- [x] 四种架构编译通过
- [x] `cargo fmt --all --check` 通过
- [x] `cargo xtask clippy --package axvm` 通过
- [x] 零行为变更（不修改任何现有代码路径）

**已合入 dev，PR #1661**。

**与设计差异**:
- 额外提取了 `VcpuInterruptQueue` 到 `runtime/queue.rs`，将 host-testable 的队列语义与依赖 ArceOS task 的 dispatcher 分离
- 使用 `#[allow(dead_code)]` 逐项标注而非 `#[expect(dead_code)]`
