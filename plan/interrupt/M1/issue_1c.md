# PR 1c issue

- [ ] 在 `ArchOps` trait 中新增 `inject_vcpu_interrupt(vcpu, interrupt: PendingVcpuInterrupt) -> AxVmResult`，默认实现调用 `vcpu.inject_interrupt_with_trigger(interrupt.id.0 as usize, interrupt.trigger)`
- [ ] `ops.rs` 添加 `use crate::irq::model::PendingVcpuInterrupt;`
- [ ] `inject_vcpu_interrupt` 不接收 `vm` 参数（Router 在入队前已完成所有 VM 级工作）
- [ ] x86 adapter 把 edge/level 映射到 backend 的 `level_triggered` 参数；AArch64、RISC-V、LoongArch adapter 显式说明 trigger 已由对应 Router/controller 消费
- [ ] vCPU run loop 在旧 `inject_pending_interrupts` 之后 drain 新 dispatcher（通过 PR 1b 暴露的 `irq_dispatcher()` getter）
- [ ] 新队列 drain 失败（runtime 不存在）时显式 match + warn 日志，降级为空列表，不中断 run loop
- [ ] 单个 interrupt 注入失败时记录 warn 日志（含 VM ID、vCPU ID、interrupt ID）并继续后续处理
- [ ] 提取可单测的 dispatcher drain→逐项注入步骤，验证 FIFO、每项只消费一次、失败项不重复
- [ ] 保持旧 `inject_pending_interrupts` / `pending_interrupts` drain 路径不变
- [ ] 移除 `dispatcher.rs` 中 `drain()` 的 `#[allow(dead_code)]`（被 run loop 调用）
- [ ] 移除 `vm/mod.rs` 中 `irq_dispatcher()` getter 的 `#[allow(dead_code)]`（被 run loop 调用）
- [ ] 将仍等待 Router 接入的 `dispatch_vcpu_interrupt()` 和 sender 入口改为带阶段原因的 `#[expect(dead_code)]`
- [ ] 单元测试：fake backend 验证 trigger 保留，x86 edge/level backend 映射正确
- [ ] 集成测试：新 dispatcher 为空时 guest 行为与 PR 之前完全一致
- [ ] 四种架构编译通过
- [ ] `cargo fmt --all --check` 通过
- [ ] `cargo xtask clippy --package axvm` 通过
