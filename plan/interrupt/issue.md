# AxVisor：重构设备中断路由与 vCPU 递送边界

## 问题概述

AxVisor 当前的设备中断路径由各架构独立演化，已经出现以下结构性问题：

1. `IrqLine` → `IrqSink` 只在 RISC-V 上完整接通，其他架构仍依赖运行时或设备类型特判。
2. x86 runtime 直接理解 IOAPIC、PIT、串口和 vCPU 注入细节，设备、虚拟中断控制器与调度逻辑耦合。
3. 跨 vCPU IPI、设备中断和直通 IRQ 使用不同的排队与唤醒路径，生命周期和并发语义不一致。
4. AArch64 的远程注入必须在目标 vCPU 上下文写入 GIC LR，现有共享路径没有表达这一约束。
5. LoongArch 的平台 IRQ 与 PCH-PIC MMIO 输出仍依赖架构特定队列和后处理逻辑。
6. 设备资源模型无法声明 IRQ line，也不能在设备注册时发现 line 冲突。

本 issue 按 [`design.md`](./design.md) 实施。目标是整理现有设备线中断的
边界和递送路径。为使既有中断类型正确投递而必须补齐的底层状态、错误和
生命周期属于范围；新的 guest-programmable 路由模式和中断类型不属于范围。

## 目标

- 设备只通过 `IrqLine::raise()`、`lower()`、`pulse()` 表达线中断语义。
- 每种架构使用自己的具体 Router，把 IRQ line 操作转换为虚拟中断控制器输入及待递送中断。
- Router 输出先进入 VM runtime 队列，最终由目标 vCPU run loop 调用架构注入接口。
- runtime 负责 task wake、等待队列通知和 host IPI；Router 不依赖调度器。
- IRQ line 成为可声明、可校验、可回滚的设备资源。
- 各架构可以独立迁移，迁移期间保留旧路径兼容性。

## 非目标

本 issue 不包含以下工作：

- 新建通用虚拟时钟后端，或改变 PIT 的时间源和推进方式；
- 新增 HPET、MSI/MSI-X、ITS、IMSIC 等能力；
- 新增 IOAPIC logical destination、lowest-priority delivery 或 x2APIC 扩展；
- 改变 guest 可见中断控制器寄存器 ABI；
- 新增共享 IRQ 注册或共享策略；
- 重构 host IRQ framework、任务调度器或等待队列；
- 增加同 vCPU 直接注入的快速路径；
- 统一不同架构的 EOI 协议；
- 实现 AArch64 guest `GICD_IROUTER` 或完整 pending/active/config MMIO；
- 增加 AArch64 GICv2、SGI/PPI、ITS/LPI，或重构 GPPT/passthrough；
- 解决 guest 长时间无 VM exit 时 PIT 可能得不到推进的问题。

## 目标路径与边界

```text
Device Core
  → IrqLine
  → InterruptFabric
  → Architecture Router
  → VmInterruptSender
  → VmRuntimeHandle / VcpuIrqDispatcher
  → target vCPU run loop
  → ArchOps::inject_vcpu_interrupt
```

边界约束：

- `InterruptFabric` 只解析和转发 line 操作，不负责 EOI、MMIO side effect、vCPU 注入、downcast、wake 或 host IPI。
- 不新增通用 `InterruptRouter` trait。各架构 Router 实现现有 `IrqSink`，Fabric 继续保存 `Arc<dyn IrqSink>`。
- IPI 不是设备线中断，不通过 `InterruptFabric` 或 `IrqLine`。
- 架构特有 EOI 和 MMIO 后处理保留在架构层，不向共享 trait 添加无意义方法。
- Router 不直接修改远程 vCPU，也不持有 runtime 或 vCPU task。
- 架构可以把 dispatcher event 解释为 target kick。AArch64 emulated SPI
  的 pending/active durable truth 保存在 VGIC，目标 vCPU 收到 kick 后重新
  fold/refill LR，不能盲信旧 event。

## 实施计划

### 模块 1：共享资源与 runtime 递送通道

模块一是纯增量基础设施，不改变任何架构现有中断生产者。

#### IRQ line 资源

- [x] 为 `Resource` 增加 `IrqLine { line: u32, trigger: InterruptTriggerMode }`。
- [x] 使用 `BTreeMap<u32, DeviceId>` 建立独占 IRQ line 索引，不使用 `u64` 位图。
- [x] 检测同一设备重复声明和不同设备占用同一 line。
- [x] bundle 注册失败时回滚已经登记的 IRQ line。
- [x] IRQ line 不参与 MMIO、PIO 或 SysReg dispatch 查找。
- [x] 资源层不设置架构无关的 line 上限；Router 在解析时校验自身范围。

#### runtime 递送模型

- [x] 新增 `VirtualInterruptId(u32)`，避免把 x86 `u8` vector 限制扩散到其他架构。（PR #1661 / `model.rs`）
- [x] 新增携带 interrupt ID 和 trigger mode 的 `PendingVcpuInterrupt`。（PR #1661 / `model.rs`）
- [x] 新增 runtime-owned `VcpuIrqDispatcher`，维护 vCPU task 注册、pending queue、`enqueue()` 和 `drain()`。（PR #1661 / `dispatcher.rs`）
- [x] `enqueue()` 仅更新队列并返回目标 pCPU ID，不在锁内调用 wake、IPI 或外部回调。（PR #1661）
- [x] 新增持有 `Weak<AxVM>` 的 `VmInterruptSender`；每次发送时查找当前
  runtime，不缓存 dispatcher。
- [x] 新增 `VmRuntimeHandle::dispatch_vcpu_interrupt()`，按“入队 → notify → host IPI”顺序递送。
- [x] runtime 不存在、VM 状态不接受中断、vCPU 不存在或 task 未注册时返回明确错误。
- [x] vCPU run loop 在进入 guest 前 drain 新 dispatcher 并调用 `ArchOps::inject_vcpu_interrupt()`。
- [x] 迁移期继续 drain 旧 `pending_interrupts`，不得提前删除旧生产者所依赖的路径。

#### Fabric 与生命周期

- [x] `InterruptFabric` 继续只保存 `IrqSink`，按需提供 `with_sink()`（design 中称为 `with_router()`）、`set_level()` 和 `pulse()`。
- [x] AxVM 只暴露有状态检查的 `pulse_interrupt()`，不公开 Fabric 引用或克隆。（`set_interrupt_level` 通过 Fabric 的 `set_level()` 可调用，但 AxVM 上尚未包装。）
- [x] `AxVM::prepare(self: &Arc<Self>)` 为后续 Router 安全构造
  `Weak<AxVM>` 提供条件；模块一不提前修改尚无 Router 消费者的架构
  `init_vm`。
- [x] VM stop/start/reset 后，sender 只能发现当前 runtime，不能向旧
  dispatcher 入队。
- [x] 不增加 `inject_to_vcpu`、`drain_side_effects`、`as_any` 或 `router_ref` 等共享接口。

#### 模块一退出条件

- [x] 四种架构仍能沿原路径编译，模块一没有要求任何架构 Router 同步完成。
- [x] 共享类型、sender、runtime dispatcher、run-loop consumer 和迁移期双队列均有可执行证据；不要求 Router 或架构初始化入口提前迁移。
- [x] 共享资源（IRQ line 冲突检测、回滚、InterruptFabric）行为具有确定性单元测试。
- [x] 模块一没有修改 PIT、串口、VGIC、PCH-PIC、IOAPIC、vLAPIC 或平台 IRQ 的生产路径。

### 模块 2：RISC-V 基线验证

RISC-V 已有可用的 `RiscvPlicIrqSink`，本模块只验证共享层没有造成回归。

- [x] 保持 `RiscvPlicIrqSink` 作为 Fabric sink。
- [x] 如构造 API 发生变化，只做必要适配。
- [x] 不为 RISC-V 增加空操作 `inject_to_vcpu()`。
- [x] 保持 `SendIPI` exit 的现有语义；IPI 不改走设备 Fabric。
- [x] 仅在后续确实创建 sender/Router 时，才把 RISC-V `init_vm` 调整为
  获取 `AxVMRef`。
- [x] 验证 vPLIC pending、claim、complete 和 secondary vCPU IPI。

### 模块 3：AArch64 emulated SPI 分层投递

详细范围、拆分和验收见 [`issue_3.md`](./issue_3.md)，完整设计见
[`design_3.md`](./design_3.md)。

- [ ] **3A：**修正 INTID 截断、ICH/TDIR register fields 和 LR 可恢复错误。
- [ ] **3B：**实现 VGIC SPI line/pending/active 状态与固定显式 route。
- [ ] **3C：**实现 per-vCPU ICH context、TDIR/maintenance completion 和
  LR refill。
- [ ] **3D：**增加 `fixed_irq_routes` 配置、共享 controller 和两阶段初始化。
- [ ] **3E：**接通 `ArmInterruptRouter → sender kick → target refill`，并
  修正 `AxVM::pulse_interrupt()` 持 machine lock 进入 sender 回查 runtime
  的重入风险。
- [ ] **3F：**增加 synthetic SPI device 和 emulated GICv3 SMP2 QEMU。
- [ ] 验证 edge、两种 EOImode 下的 level deactivate 重投、LR 满载、
  跨 CPU wake 和 INTID 256。
- [ ] 保持 GICv2、SGI/PPI/IPI、ITS/LPI 和 GPPT/passthrough 在模块范围外。

### 模块 4：LoongArch Router

- [ ] 创建共享的 PCH-PIC controller core 和 `LoongArchInterruptRouter`。
- [ ] Router 把 PCH-PIC 输入 line 转换为 EIOINTC 输出 vector，并经 sender 递送。
- [ ] 平台 IRQ glue 保留 `vm_id`、`vcpu_id`、`guest_vector`、`physical_irq` 的原始含义。
- [ ] `physical_irq` 仅用于 host route/mask bookkeeping，`guest_vector` 作为 guest line 输入。
- [ ] 不无条件硬编码 vCPU 0；仅在现有硬件模型明确路由到 BSP 时使用它。
- [ ] 第一阶段保留 `ArchOps::after_mmio_write`，其 drain 结果改走 sender。
- [ ] 不向共享 Fabric 增加 LoongArch MMIO drain 方法。
- [ ] 在构造 `LoongArchInterruptRouter` 时把 LoongArch `init_vm` 调整为
  获取 `AxVMRef`。
- [ ] 平台 IRQ 和 MMIO 输出均迁移后，再移除 `PendingInterrupt::External` 和 `queue_external_interrupt()`。
- [ ] 验证平台 route 元数据和 PCH-PIC MMIO 输出均不丢失。

### 模块 5：x86_64 Router

该模块应拆成可独立验证的小改动，同时保持 VMX 和 SVM 可编译。

#### 5a：IOAPIC 共享所有权与 Router 骨架

- [ ] IOAPIC core 使用 `Arc<EmulatedIoApic>`，由 Router 和 MMIO wrapper 共享同一实例。
- [ ] 新建 `X86InterruptRouter`，只持有 IOAPIC core、APIC ID 到 vCPU 的映射和 sender。
- [ ] 初始化顺序遵循“controller core → Router/Fabric → MMIO wrapper → 其他设备”。
- [ ] 在构造 `X86InterruptRouter` 时把 x86 `init_vm` 调整为获取
  `AxVMRef`。

#### 5b：IOAPIC route 与 line-level 语义

- [ ] `IoApicInterrupt` 携带 vector、destination APIC ID 和 level trigger 信息。
- [ ] 只支持当前已有的 Fixed + Physical Destination。
- [ ] logical destination、unsupported delivery mode 和未知 APIC ID 返回明确错误。
- [ ] 增加最小的 `set_gsi_level()` / `pulse_gsi()` 能力。
- [ ] `raise()`、`lower()`、`pulse()` 具有真实且可测试的 line 语义。
- [ ] level line 在 EOI 后仅在仍 asserted 或出现新 pending 事件时重新递送。

#### 5c：PIT 与串口事件化

- [ ] PIT 保持现有时间推进方式，`tick(now)` 到期后通过 IRQ0 `pulse()`。
- [ ] 串口在自身状态进入 interrupt pending 时操作 `IrqLine`。
- [ ] 移除 runtime 对 PIT/串口 pending 状态的轮询。
- [ ] `IrqLine` 错误必须传播或记录，不得使用 `.ok()` 静默忽略。

#### 5d：host IRQ 转发

- [ ] host IRQ adapter 通过 VM line API 输入 guest GSI，不直接操作 vCPU。
- [ ] host physical IRQ 的 edge/level 属性与 guest IOAPIC redirection trigger 分开保存。
- [ ] hard-IRQ 上下文不能安全分配时，使用现有预分配状态或 deferred path；本 issue 不新增硬 IRQ 分配机制。

#### 5e：vLAPIC IPI

- [ ] ICR 解码继续属于 `x86_vlapic`。
- [ ] vLAPIC 通过 x86 专用 hook 调用 `VmInterruptSender`。
- [ ] IPI 不经过 `IrqLine` 或 `InterruptFabric`。
- [ ] 保持当前 VMX/SVM IPI 语义和 trigger 信息。

#### 5f：EOI 收敛

- [ ] VMX/SVM 的现有 EOI 来源统一进入一个 canonical x86 EOI hook。
- [ ] hook 捕获 `Weak<X86InterruptRouter>`，不从 Fabric downcast Router。
- [ ] 同一次 EOI 不同时经过 `eoi_notify` 与 `after_interrupt_end` 重复处理。
- [ ] IOAPIC 更新 remote IRR 后，如需重注入则通过 sender 排队。
- [ ] host IRQ glue 独立决定 host unmask，不能只依据 guest trigger mode。
- [ ] 移除已被 Router 取代的 x86 轮询和 EOI 注入函数。

### 最终清理

只有确认所有旧队列生产者已经迁移后，才能执行以下清理：

- [ ] 移除旧 `PendingInterrupt` 和 `pending_interrupts`。
- [ ] 移除 `queue_interrupt()` / `queue_external_interrupt()`。
- [ ] 移除已无调用者的设备类型特定查询 trait。
- [ ] 移除被 Router 取代的 x86 runtime 轮询注入函数。
- [ ] 保留仍承担明确架构职责的 hook；例如 LoongArch MMIO 后处理只能在已有安全替代后删除。

## 并发与错误处理要求

- Router 的 `IrqSink` 方法可能从设备任务、vCPU exit 或 host IRQ adapter 调用，不得睡眠。
- Router 不直接锁目标 vCPU，不从远程 pCPU 写 banked system register。
- controller core 的锁内只更新控制器状态；释放锁后再调用 sender。
- dispatcher 锁内只更新 task map 和 pending queue。
- pending event 必须先发布到队列，再执行 notify/IPI。
- `Weak` 升级失败、目标不存在、line 越界和不支持的路由模式必须返回显式错误。
- 中断路径不得使用裸 `unwrap()`、`.ok()` 或静默选择默认 vCPU。

## 测试与验证

### 共享层测试

- [x] IRQ line 0、63、64 和大于 255 的合法声明。（`test_irq_line_resource_rejects_duplicate_declarations_and_cross_device_conflicts` 覆盖了 line 10、20 等场景；`test_bundle_irq_conflict_rolls_back_all_resources_from_prior_devices` 覆盖了 line 40、41。）
- [x] 同设备重复 line 和跨设备 line 冲突。
- [x] bundle 注册中途失败后的 IRQ index 回滚。
- [x] dispatcher 保持入队顺序、trigger mode 和 vCPU 隔离。（PR #1661 / `queue.rs` 测试）
- [x] drain 后队列为空。（PR #1661 / `queue.rs` 测试）
- [x] runtime 不存在、VM 状态错误、vCPU 不存在和 task 未注册。
- [x] VM reset 后旧 runtime 队列不再接收新事件。
- [x] 入队发生在 notify/IPI 之前，且回调发生在 dispatcher 队列锁释放之后。

### 架构测试

- [x] RISC-V：vPLIC pending/claim/complete 和 secondary vCPU IPI 回归。
- [ ] AArch64：固定 route、INTID 256、edge、两种 EOImode 下的 level
  deactivate 重投、LR refill、per-vCPU ICH 隔离和跨 CPU wake；LR 只在
  目标 vCPU 上更新。
- [ ] LoongArch：平台 route 参数得到保留，PCH-PIC MMIO 输出不丢失。
- [ ] x86：destination 解析、edge pulse、level raise/lower、EOI 重注入和不支持路由错误。
- [ ] x86：host trigger 与 guest redirection trigger 分开处理。
- [ ] VMX 和 SVM 分别验证受影响的注入与 EOI 路径。

### 本地检查

每个模块至少执行：

```bash
cargo fmt --all --check
cargo xtask clippy --package axdevice_base
cargo xtask clippy --package axdevice
cargo xtask clippy --package axvm
```

模块 3 还检查 `arm_vgic` 和 `arm_vcpu`。按受影响架构执行对应的
`cargo xtask axvisor build`，最终合并前完成环境可用的 QEMU 架构验证。
模块 3 必须完成 emulated GICv3 SMP2 QEMU；物理板和 self-hosted 流程不要求
在每个模块执行。

## 完成标准

- [ ] 所有模拟设备通过 `IrqLine` 发出设备中断。
- [ ] 设备代码不依赖架构中断控制器或 VM runtime API。
- [ ] Router 不承担 task wake、等待队列通知或 host IPI。
- [ ] 远程 vCPU 注入只在目标 vCPU run loop 落地。
- [ ] RISC-V、AArch64、LoongArch 和 x86 的现有 IPI 行为不退化。
- [ ] x86 runtime 不再轮询 PIT/串口 IRQ pending 状态。
- [ ] 旧 VM pending interrupt 队列已经没有生产者并被移除。
- [ ] x86 EOI 和 LoongArch MMIO 后处理没有污染共享 Fabric 接口。
- [ ] AArch64 emulated SPI 由 VGIC 保存 pending/active durable truth，
  具备固定 route、两种 EOImode 下的 level deactivate 重投和 LR refill。
- [ ] 新增资源、队列和 Router 行为均有确定性回归测试。
- [ ] 没有引入“非目标”中列出的额外功能。
