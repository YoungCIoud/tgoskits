# PR 3.4 设计：固定路由配置、Router 与 AArch64 runtime 接通

## 1. PR 定位

- 建议标题：`feat(axvm): route AArch64 emulated SPIs through the VGIC`
- 依赖：PR 3.1、3.2、3.3。
- 后继：PR 3.5 只增加 synthetic producer 和端到端证据。
- 预计变更：生产代码 420–700 行，测试/配置 300–520 行。
- 难度：中高。风险在构造顺序、失败回滚、custom Fabric 兼容和把 dispatcher
  event 正确解释成 service kick。

本 PR 是唯一启用新 production producer path 的 PR。它不新增底层 GIC
状态语义，也不实现 guest-programmable IROUTER。

## 2. 已完成前置：machine-lock 修复

当前基线已经完成 `AxVM::pulse_interrupt()` 的重入锁修复：

- `interrupt_fabric_snapshot()` 在 machine lock 内验证 lifecycle 并克隆
  `InterruptFabric`；
- 释放 machine lock 后才调用 `fabric.pulse()`；
- `InterruptFabric` 是 cloneable capability；
- 已有
  `interrupt_pulse_runs_after_snapshot_lock_is_released` 回归测试；
- 修复来自 PR #1679 / commit `5155111ef`，当前 plan 基线已包含。

因此本 PR 不再修改 `pulse_interrupt()` 锁逻辑，只运行该测试。新增
`AxVM::set_interrupt_level()` 时必须复用同一个 snapshot helper，禁止重新
使用 `with_resources()` 在 machine lock 内调用 Router。

## 3. 配置模型

### 3.1 序列化类型

在 `axvmconfig` 增加：

```rust
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FixedIrqRouteConfig {
    pub intid: u32,
    pub target_vcpu_id: usize,
}

pub struct VMDevicesConfig {
    // existing fields
    #[serde(default)]
    pub fixed_irq_routes: Vec<FixedIrqRouteConfig>,
}
```

具体同步点：

- `virtualization/axvmconfig/src/lib.rs`：类型、field、serde default和
  `std`下的 schemars schema；
- `virtualization/axvmconfig/src/templates.rs` 及测试 fixture：显式空
  `fixed_irq_routes` 或依赖 default；
- `virtualization/axvm/src/config.rs`：`AxVMConfigParams`/`AxVMConfig`持有
  `Vec<FixedIrqRouteConfig>` 并提供 slice accessor；
- `os/axvisor/src/config.rs::build_axvm_config()`：从
  `cfg.devices.fixed_irq_routes` clone传播；
- 仓库内所有直接构造 `VMDevicesConfig`/`AxVMConfigParams` 的测试使用
  `..Default::default()` 或补空值，不能用 `cfg`条件掩盖缺字段。

这是一项 user-visible config扩展；同步更新配置示例/生成 schema。旧 TOML
缺字段必须成功并得到空表，未知/错误字段仍按当前 parser policy处理。

传播链必须完整：

```text
axvmconfig::VMDevicesConfig
  → os/axvisor/src/config.rs
  → axvm::AxVMConfigParams
  → axvm::AxVMConfig
  → AArch64 prepare
```

`AxVMConfig` 提供只读 slice accessor，不向 runtime 暴露可变 route。

配置含义：

- `intid` 同时是设备 `Resource::IrqLine.line` 和 guest SPI INTID；
- target 是 VM 内逻辑 vCPU ID，不是 pCPU/MPIDR；
- trigger 不放 route 表，由 device resource 唯一声明；
- route 在本模块生命周期内固定。

### 3.2 验证

仅当 `target_arch=aarch64` 且
`interrupt_mode=emulated` 时消费 fixed routes：

- INTID 必须在 32–1019；
- target vCPU 存在；
- target 可无损转换为 `VgicVcpuId`，不能 `as u32`截断；
- 每个 INTID 唯一；
- 不接受隐式 vCPU 0；
- 不接受 wildcard/Any；
- 所有实际 module SPI device line 恰好有一条 route；
- 每条 route 恰好由一个 device line 使用；
- route 的 line 与设备 trigger 一起注册到 controller；
- 未使用 route、missing route、duplicate line 均使 prepare 失败。

`passthrough`/`no_irq` 若配置非空 fixed routes 应返回 invalid config，而不是
静默忽略，避免用户误以为生效。旧配置缺字段时默认为空并继续工作。

## 4. Router

新增：

```rust
struct ArmInterruptRouter {
    controller: Arc<ArmVgicController>,
    sender: VmInterruptSender,
}
```

实现 `axdevice_base::IrqSink`：

```rust
fn pulse(&self, line: IrqLineId) -> IrqResult<()>;
fn set_level(&self, line: IrqLineId, asserted: bool) -> IrqResult<()>;
```

### 4.1 pulse

1. checked `IrqLineId → u32 → ArmSpiIntId`。
2. 调用 `controller.pulse(intid)`。
3. controller lock 已释放后检查 `ServiceHint`。
4. 若有 target，调用：

```rust
sender.send(
    target.as_usize(),
    PendingVcpuInterrupt {
        id: VirtualInterruptId(intid.as_u32()),
        trigger: EdgeTriggered,
    },
)
```

5. sender error 转成带 line/target/operation 的 `IrqError::Backend`。

### 4.2 level

流程相同，但调用 `set_level()`，event trigger 为 LevelTriggered。lower 也可能
需要 service resident Pending/Active LR，因此不能假定
`asserted=false` 永远不 kick；是否返回 hint完全由 controller 决定。

### 4.3 event 语义

`PendingVcpuInterrupt` 只是：

- 定位/诊断用 INTID；
- 唤醒 target vCPU 的 service kick。

它不是 pending instance。sender 失败时 controller durable state保留；
后续 producer 操作、maintenance 或 guest-entry refill 可恢复。重复/stale
event 被 target drain 后可以成功 no-op。

Router 不：

- 持有 AxVM；
- 查 target task/pCPU；
- 锁 target vCPU；
- 写 ICH；
- 保存 pending/active；
- 在 controller lock 内调用 sender。

## 5. AArch64 初始化所有权

### 5.1 默认 prepare

`Aarch64Arch::init_vm()`/`init_vm_with()` 需要接收 `&Arc<AxVM>`，因为
`VmInterruptSender::new()` 需要 VM weak reference。不得把 AxVM reference
放进 controller/Fabric/device。

emulated GICv3 的构造顺序固定：

1. 校验 interrupt mode、placement 共同 ICH profile、TDS、maintenance PPI
   以及 host-lifetime maintenance handler 已注册且覆盖 placement CPU。
2. 扫描 emulated device configs，找出唯一
   `EmulatedDeviceType::InterruptController` 配置。
3. 创建 `ArmVgicControllerBuilder` 并取得尚未 sealed 的唯一
   `Arc<ArmVgicController>`。
4. 创建 `VmInterruptSender`。
5. 创建 Router/Fabric；未 sealed controller 的 runtime操作只会返回
   NotReady，而 prepared bundle 尚未发布。
6. 注册 architecture-owned InterruptController factory并构建临时 devices；
   device factory通过该真实 Fabric取得最终 `IrqLine`。
7. 在 `PreparedDevices` 尚未提交时，经现有
   `prepared.devices().devices().flat_map(Device::resources)` 只读迭代全部
   `Resource::IrqLine`，收集 SPI line/trigger，与 `fixed_irq_routes` 做
   一一校验并注册到 builder；不穿透 `DeviceBundle` 的私有字段。
8. `builder.finish()` seal controller；任何 missing/unused/duplicate route
   在此之前失败并丢弃临时 bundle。
9. 为每个 vCPU 创建绑定同一 sealed controller 的
   `ArmVgicDeliveryPort`。
10. 构建/设置 vCPU。
11. 完成 address-space mapping。
12. 由 `complete_vm_init()` 一次提交 vCPU、devices、Fabric。

任何步骤失败都通过现有 `reset_transient_resources()` 回滚，不把 controller、
route、device、delivery port 或 Fabric 的半成品发布给 VM。

步骤 1 的 emulated capability policy 是显式边界：

- 共同 `lr_count >= 2`；单 LR 时 UIE始终满足“零个或一个 valid LR”的
  underflow条件，本模块未启用 NPIE替代方案，因此返回 Unsupported；
- 16-bit或24-bit virtual INTID能力都足以覆盖 0–1019；
- TDS 必须为 true；
- priority/preemption/APR layout必须满足 PR 3.2 的共同 profile规则；
- maintenance PPI 与 handler CPU mask必须覆盖 placement中的每个 pCPU。

### 5.2 InterruptController ownership

当前 `AxVmDevices::init()` 对 `InterruptController` 有 legacy fallback，会
调用 `Vgic::new()`，产生第二个无共享状态的 controller。emulated AArch64
必须：

- 在 architecture factory registry 中预注册 authoritative controller
  factory；
- factory 产出的 MMIO wrapper引用同一个 `Arc<ArmVgicController>`；
- 因 factory 已存在，`build_with_factories()` 不进入 fallback；
- duplicate InterruptController config在注册 factory 前被拒绝；
- config 的 base/length/cfg_list 必须与 factory fingerprint相符；
- MMIO wrapper 的最小 ISENABLER代理共享 controller。

首版 InterruptController fingerprint固定为：base 64KiB对齐、length
`0x1_0000`、`cfg_list=[]`。factory调用 PR 3.3 的
`Vgic::with_spi_controller(configured_range, controller.clone())`，不能调用
仍使用硬编码 range/legacy 512项 SPI state的 `Vgic::new()`。

不能在 `AxVmDevices` 中增加 AArch64-specific downcast accessor来事后偷取
controller。

### 5.3 caller-provided Fabric

`prepare_with_factories()` 允许 caller 提供 Fabric。对 AArch64
`interrupt_mode=emulated`：

- architecture 必须拥有 `ArmInterruptRouter/controller/delivery` 的一致
  组合；
- arbitrary custom Fabric 无法证明与 delivery controller 相同，必须明确
  返回 unsupported；
- 不可一边让 device 使用 custom sink，一边让 vCPU refill architecture
  controller。

passthrough/no-irq 的 provided Fabric 规则保持现状。未来若需要可组合 custom
backend，应设计 typed architecture backend capability，不在本 PR 加 downcast
或 magic identity。

## 6. `AxvmArmVcpu` 组合

从 tuple struct改为：

```rust
pub(crate) struct AxvmArmVcpu {
    backend: ArmVcpu<AxvmArmHostOps>,
    delivery: Option<ArmVgicDeliveryPort>,
    sender: Option<VmInterruptSender>,
}
```

- passthrough/no-irq 为 None；
- emulated 的 delivery target 与该 AxVM vCPU ID一致；
- controller 与 Router/MMIO wrapper 是同一 Arc；
- sender 只在 delivery 返回 cross-vCPU service hint 后调用；
- sender 不放入 `arm_vcpu` 或 `arm_vgic`。

private create config：

```rust
struct AxvmArmVcpuCreateConfig {
    backend: ArmVcpuCreateConfig,
    vgic: Option<ArmVgicVcpuConfig>,
}
```

利用 `VmArchVcpuOps::CreateConfig` associated type把变化限制在 AArch64。

## 7. target vCPU consumer

当前 runtime 在 guest entry 前 drain queue并逐个调用
`A::inject_pending_interrupt()`。AArch64 emulated mode覆盖为：

1. `ArchOps` 增加默认 no-op 的
   `before_vcpu_entry(vm, vcpu) -> AxVmResult` hook；通用 run loop在两个
   pending queue都 drain 完成后、`vcpu.run()` 前恰好调用一次。
2. AArch64 `inject_vcpu_interrupt()` 对 emulated controller-owned SPI只验证
   event target/INTID并 trace，随后成功返回；不逐项写 LR。passthrough或
   现有 private direct event保持原 inject行为。
3. AArch64 `before_vcpu_entry()` 即使 queue 为空，也执行一次
   `delivery.service(bound_ich)`：
   - fold
   - reconcile
   - refill
4. stale event时统一 service返回 no-op。

该 hook 对其他架构默认 no-op。测试必须证明 17 个已 drain event只触发一次
entry service，而不是 17 次 blind LR write。

### 7.1 TDIR cross-vCPU hint

PR 3.3 的 sysreg handler返回 owner `ServiceHint`：

- 当前 vCPU owner：同一 bound scope立即 service；
- 远程 owner：释放 controller lock后通过该 vCPU持有的 sender发送 kick；
- sender failure传播为 exit handling error，但 deactivation command保留，
  后续 owner service仍可完成。

## 8. AxVM line wrapper

增加：

```rust
pub fn set_interrupt_level(&self, irq_id: usize, asserted: bool) -> AxVmResult {
    let fabric = self.interrupt_fabric_snapshot()?;
    fabric.set_level(irq_id, asserted)
}
```

与 `pulse_interrupt()` 共用 lifecycle/snapshot规则：

- Running/Paused可接受；
- 其他状态返回 BadState；
- machine lock只保护 snapshot；
- Router/sender在锁外调用；
- 不公开 `&InterruptFabric` 或 public clone accessor。

## 9. reset/stop/迟到事件

- VM reset销毁旧 transient vCPUs/controller/Fabric并重新 prepare。
- 旧 device/IrqLine随旧 devices释放；不得保留到新 controller。
- sender持 `Weak<AxVM>`，runtime generation变化后按当前 module 1 lifecycle
  返回错误。
- 旧 queue的迟到 kick若到达新 runtime，新的 controller无对应状态，
  target service为 no-op；不得凭 event重新创建 pending。
- stop期间 controller可保留到 resources reset，但不得访问已 unbind ICH。

## 10. passthrough 和非 AArch64

- `interrupt_mode=passthrough` 的 `register_arch_devices()`、GPPT、physical IRQ
  路径保持原样。
- no_irq不创建 Router/controller/maintenance。
- RISC-V/LoongArch/x86 的 default prepare、provided Fabric、run-loop hook不变。
- SGI/PPI/IPI仍走既有路径，只有全局 TDIR trap按 PR 3.3 compatibility完成。

## 11. 测试

### 11.1 配置

- 旧 TOML 缺字段反序列化为空。
- 新 route完整传播到 `AxVMConfig`。
- 31/1020、duplicate、missing target、missing route、unused route失败。
- no_irq/passthrough + nonempty routes失败。
- non-AArch64 build不消费 route。

### 11.2 初始化

- controller/MMIO/Router/delivery共享同一 identity。
- duplicate InterruptController拒绝。
- authoritative factory不进入 fallback。
- custom Fabric + emulated拒绝。
- 单 LR、missing TDS/maintenance/profile或 handler CPU coverage不足拒绝。
- device build、vCPU setup、mapping任一点失败均未提交半初始化 resources。
- reset重建干净 controller。

### 11.3 Router/runtime

- pulse/raise/lower调用正确 controller transition。
- sender仅在 lock释放后调用。
- sender target是固定 route vCPU。
- sender失败保留 pending。
- stale kick no-op。
- remote event唤醒正确 vCPU。
- drain 17 events不逐项 blind inject，统一 refill。
- guest entry无 event也会 fold/refill。
- cross-vCPU DIR在 lock外 kick owner。

### 11.4 锁回归

必须保留并运行：

```bash
cargo test -p axvm --features host-test \
  interrupt_pulse_runs_after_snapshot_lock_is_released
```

为新 level wrapper增加同构测试，证明 `set_level()` 在 snapshot lock释放后
运行。

### 11.5 回归命令

```bash
cargo xtask clippy --package axvmconfig
cargo xtask clippy --package axvm
cargo xtask clippy --package axdevice
cargo test -p axvmconfig
cargo test -p axvm --features host-test
cargo fmt --all -- --check
```

同时执行可用的 AArch64 AxVisor build、至少一个现有 passthrough config build
和非 AArch64 package checks。

## 12. 完成条件

- 设备只持有正常 `IrqLine`。
- fixed route显式且与 device line一一对应。
- Router只更新 controller并发送 service kick。
- target vCPU每次 entry前按 controller truth fold/refill。
- controller、MMIO wrapper、Router、delivery port共享同一实例。
- init失败原子回滚，reset不继承旧 pending。
- `pulse_interrupt` 锁修复保持已完成，level wrapper遵循同一模式。
- passthrough、no_irq、private interrupt和非 AArch64不退化。

完成本 PR 后，模块 3 的 production 链路已经可用；PR 3.5 负责提供可重复的
SMP2、INTID 256、level EOI重投和 LR pressure 行为证据。
