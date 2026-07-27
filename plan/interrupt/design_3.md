# 模块 3 设计：AArch64 emulated GICv3 SPI 分层投递

## 1. 摘要

模块 3 完成 AArch64 emulated SPI 从设备线路到目标 vCPU GICv3 List
Register（LR）的完整分层投递：

```text
synthetic/real emulated device
  → IrqLine
  → InterruptFabric
  → ArmInterruptRouter
  → ArmVgicController（durable state）
  → VmInterruptSender（kick）
  → runtime dispatcher（queue + wake + host IPI）
  → target vCPU run loop
  → ArmVgicDeliveryPort
  → fold/refill target ICH LRs
```

本设计支持固定显式路由、edge、level、deactivate 后重投、guest
`EOImode=0/1`、LR refill、per-vCPU ICH context 和跨 CPU 唤醒。它不实现
guest-programmable `GICD_IROUTER`，也不补全 guest
pending/active/config MMIO，但 controller 的状态所有权和转换边界允许以后
在不重写 Router、sender 和 LR 路径的情况下增加这些输入。

## 2. 范围与假设

### 2.1 支持范围

- host：GICv3 virtualization interface；
- guest：emulated GICv3；
- interrupt type：wired SPI；
- INTID：32–1019；
- route：VM 配置提供的固定 `(INTID, target_vcpu_id)`；
- trigger：设备 `IrqLine` 声明的 edge 或 level；
- delivery：目标 vCPU 当前 pCPU 上的软件 LR；
- completion：ICH LR/EISR fold、TDIR trap 和 maintenance PPI；
- scheduling：vCPU 可换入换出，ICH state 属于 vCPU 而不是 pCPU；
- testing：test-only synthetic SPI device + emulated GICv3 SMP2 QEMU。

### 2.2 非目标

- GICv2；
- guest `GICD_IROUTER` 写；
- 完整 `ISPENDR/ICPENDR/ISACTIVER/ICACTIVER/ICFGR` MMIO；
- SGI、PPI、virtual timer 和 guest IPI；
- ITS、LPI、MSI/MSI-X；
- GPPT 和 passthrough GIC；
- host physical IRQ injection；
- runtime 修改固定 route；
- 同 vCPU direct-inject fast path。

“SGI/PPI 路径不迁移”不允许 TDIR trap 吞掉既有 private interrupt 的 DIR。
模块 3 只需为被全局 TDIR 捕获的非 module-owned INTID 保留兼容
deactivation，不改变其 producer、route 或 pending 模型。

### 2.3 完整性边界

“不实现完整 guest MMIO”不等于忽略控制器状态。本模块仍必须建模完成
设备 line 投递所需的：

- trigger；
- enabled；
- line asserted；
- pending latch；
- pending/active；
- LR inflight；
- fixed target。

现有最小 enable 行为接入这套状态。未实现的 guest register 访问继续按现有
兼容行为处理，但不能形成第二套 pending/active/route 状态。

## 3. 当前实现差距

### 3.1 INTID 截断

共享层的 `VirtualInterruptId` 是 `u32`，但
`virtualization/arm_vcpu/src/vcpu.rs` 中的 `ArmVcpu::inject_interrupt()`
调用 `ArmHostOps::inject_virtual_interrupt(u8)`。这一转换使合法 SPI
256–1019 静默截断，必须先修正底层边界，不能在 Router 中用范围限制掩盖。

### 3.2 LR 满载 panic 和事件丢失

当前 `virtualization/axvm/src/arch/aarch64/gic.rs` 的 GICv3 注入函数扫描
`ICH_ELRSR_EL2` 后选择一个空 LR，没有空 LR 时 panic。runtime dispatcher
会先 drain 全部事件；单项注入失败只记录日志，事件不会自动回到队列。

因此 dispatcher 不能成为 AArch64 pending 的状态真源。LR 容量是有限硬件
cache，VGIC 软件状态必须保留尚未装入 LR 的中断。

### 3.3 VGIC capability 不完整

当前 `virtualization/arm_vgic` 中的 emulated `Vgic/Vgicd` 主要提供有限
MMIO wrapper，没有 Router 可用的 typed SPI line/route/delivery 状态边界。
GPPT `v3::VGicD/VGicR` 是宿主 GIC MMIO partial-passthrough，不是本模块
需要的 per-VM emulated controller。

现有 `Vgicd` 还使用 512 项 `VgicInt` 数组，`ISENABLER` 直接根据寄存器
index 访问。即使 LR 编码扩为 `u32`，合法 INTID 512–1019 仍会越界；模块 3
必须同时移除这一隐含上限，不能只修 host injection 签名。

### 3.4 ICH state 不属于 vCPU

当前 `virtualization/arm_vcpu/src/vcpu.rs` 的 AArch64 `bind()`/`unbind()`
是空操作，`ICH_HCR_EL2`、`ICH_VMCR_EL2`、APR 和 LR 没有随 vCPU 保存
恢复。ICH registers 是 pCPU-local/banked 状态；如果不建立 per-vCPU
context，多 VM/vCPU 复用或迁移会串扰。

当前通用 `run_vcpu()` 还在一个 `with_current_cpu_set()` pin scope 之前调用
`bind()`，离开该 scope 后再调用 `unbind()`。当 vCPU affinity 允许多个
pCPU 时，三个阶段之间存在迁移窗口；ICH restore、guest run 和 save 必须
收敛到同一个 CPU pin/current-vCPU publication 事务。

### 3.5 deactivation 不可观测

当前 LR 只写 Pending/Group，没有软件 level interrupt 所需的 EOI
maintenance，也没有 fold EISR/LR state 的入口。guest 实际 deactivate 后
line 仍 asserted 时无法重新 pending。

## 4. 规范与 prior art

### 4.1 Arm GIC 语义

设计依据：

- Arm Generic Interrupt Controller Architecture Specification，
  IHI 0069G：
  <https://developer.arm.com/documentation/ihi0069/g>
- Arm CoreLink GICv3/v4 Overview，DAI0492 Version 3.0，
  2019-09-23：
  <https://developer.arm.com/-/media/Arm%20Developer%20Community/PDF/Learn%20the%20Architecture/GICv3_v4_overview.pdf?revision=65f91645-cd52-4795-952b-f01095ff5ef8>

与本模块直接相关的结论：

- SPI 的传统 INTID 范围是 32–1019；
- 每个 SPI 有 Inactive、Pending、Active、Active+Pending 状态；
- level interrupt 的 input level 与 pending latch 不是同一个事实；
- guest 写 EOIR 完成 priority drop；`EOImode=0` 时同时 deactivate，
  `EOImode=1` 时由后续 DIR 完成 deactivate；
- level source 只有在 deactivate 后仍 asserted 时才重新成为 pending；
- LR、HCR、VMCR 和 APR 是 virtual CPU interface state，不能作为 VM
  全局无所有者寄存器处理。

### 4.2 Linux KVM VGIC

参考 Linux v6.16：

- `arch/arm64/kvm/vgic/vgic.c`：
  <https://github.com/torvalds/linux/blob/v6.16/arch/arm64/kvm/vgic/vgic.c>
- `arch/arm64/kvm/vgic/vgic-v3.c`：
  <https://github.com/torvalds/linux/blob/v6.16/arch/arm64/kvm/vgic/vgic-v3.c>
- VGICv3 userspace ABI 文档：
  <https://www.kernel.org/doc/html/v6.16/virt/kvm/devices/arm-vgic-v3.html>

借鉴的语义：

- level 使用 line level，edge 使用 pending latch；
- pending/active 状态由 VGIC 保持，LR 只是当前 vCPU 的硬件驻留副本；
- LR 装载和 fold 成对出现；
- 软件 level LR 需要 EOI maintenance，以便在实际 deactivate 后折回状态；
- 软件 edge 在 active 期间再次 pending 时用 LR Active+Pending 表达；
- LR 不足时保留软件 pending，后续 refill。
- ICH HCR 的 UIE 只在“LR 外仍有 pending 且当前无空槽”时启用，以 underflow
  maintenance 驱动 refill；本模块不需要 NPIE。LRENPIE 只与
  active-outside-LR 相关，而本模块禁止该状态并保持其关闭。
- KVM 对 `EOImode=1` 使用 TDIR 捕获 DIR，既处理 active-outside-LR，
  也处理 SPI 在另一 vCPU 上被 deactivate；
- 本模块禁止 active-outside-LR，但仍需 TDIR 处理 cross-vCPU DIR，不能把
  “active 保持 LR-resident”误当成完整替代。

不照搬的部分：

- AxVisor 没有 KVM userspace irqchip API；
- 本模块没有完整 GICD/GICR register emulation；
- 本模块固定 route，不实现 KVM 的完整 affinity/routing；
- AxVisor 通过 `VmInterruptSender` 和自己的 vCPU run loop 唤醒/调度。

### 4.3 当前项目边界

当前 module 1 已提供持有 `Weak<AxVM>` 的 `VmInterruptSender`、runtime-owned
dispatcher 和 guest entry 前的目标 vCPU consumer。AArch64 现有 host 注入
边界仍把 IRQ 转成 `u8`，直接注入器在 LR 满时 panic，`ArmVcpu::bind/unbind`
也尚未保存恢复 ICH state。

因此本设计复用已成立的 sender/runtime 生命周期，只替换 AArch64 consumer
语义：dispatcher event 是 service kick，pending/active 状态不能保存在队列
或从源 CPU 直接写入目标 vCPU 的 banked LR。

## 5. 方案比较

| 方案 | 优点 | 缺点 | 结论 |
|------|------|------|------|
| Router 把每次 line 操作变成最终 FIFO event | 改动少 | lower/disable 后事件过期；LR 满时丢失；EOI 无状态源 | 不采用 |
| dispatcher 只保存 kick，VGIC 保存 pending 真相 | stale kick 可无害；支持 refill/EOI；边界稳定 | AArch64 注入 adapter 需要覆盖默认逐事件注入 | 采用 |
| 只在普通 VM exit fold LR | 不需要 maintenance IRQ | guest deactivate 后可能长期不退出，level 重投不及时 | 不采用 |
| deactivation + underflow maintenance，普通 entry/exit 也 fold | 语义完整且能恢复漏 kick | 需要 host maintenance PPI 和 ICH context | 采用 |
| 把 target 加入通用 `IrqLine` | Router 简单 | 设备层泄漏架构路由，未来 guest route 难以收敛 | 不采用 |
| 独立静态 SPI route 配置，controller 持 target | 显式校验；未来 IROUTER 更新同一状态 | 新增配置字段 | 采用 |

## 6. 标识与配置

### 6.1 命名空间

以下标识不能混用：

- `IrqLineId`：guest interrupt controller 输入 line；
- `ArmSpiIntId`：验证后的 GICv3 SPI INTID 32–1019；
- `VirtualInterruptId`：跨 runtime dispatcher 的架构无关 `u32`；
- `VCpuId`：AxVM 内逻辑 vCPU；
- `VgicVcpuId`：`arm_vgic` 自己的 typed target ID；
- `DeliveryEpoch`：VM 生命周期内不复用的单次 LR residency 标识；
- host `IrqId`：maintenance PPI 所在 host IRQ domain；
- pCPU ID/MPIDR：调度和物理 affinity。

转换只发生在明确边界：

```text
IrqLineId
  → ArmInterruptRouter::validate_spi()
  → ArmSpiIntId
  → VirtualInterruptId（仅用于 kick/诊断）

VCpuId
  → AArch64 adapter validation
  → VgicVcpuId
```

不得通过 `as u8`、host IRQ 算术或“line 默认等于 host IRQ”转换。
`arm_vgic` 不为 target ID 依赖 `AxVM`；未来 affinity-to-vCPU 解析仍属于
AArch64 adapter。

### 6.2 固定路由配置

VM device 配置增加：

```toml
[devices]
fixed_irq_routes = [
  { intid = 33, target_vcpu_id = 0 },
  { intid = 256, target_vcpu_id = 1 },
]
```

内部解析为被动配置数据：

```rust
struct FixedIrqRouteConfig {
    intid: u32,
    target_vcpu_id: usize,
}
```

进入 `arm_vgic` 前转换成验证后的领域类型：

```rust
struct ArmSpiRoute {
    intid: ArmSpiIntId,
    target: VgicVcpuId,
}
```

`fixed_irq_routes` 使用 `serde(default)`，保证旧 passthrough/no-irq 配置继续
反序列化。字段的完整传播链为：

```text
axvmconfig::VMDevicesConfig
  → os/axvisor config::build_axvm_config
  → axvm::AxVMConfigParams
  → axvm::AxVMConfig
  → AArch64 prepare validation
```

配置层只保存可序列化的 `Vec<FixedIrqRouteConfig>`；验证后的
`ArmSpiRoute` 才进入 architecture/controller。旧配置缺少字段、默认空表、
非 AArch64 build 和 `no_irq`/passthrough 分支都必须有反序列化或构建测试。

对于 emulated SPI：

1. 解析时拒绝重复 INTID；
2. VM prepare 时校验 target vCPU 存在；
3. 设备 bundle 构造后收集 `Resource::IrqLine`；
4. 每个实际 SPI line 必须匹配一条 route；
5. route 和 line 一并提交到 controller；
6. 缺 route、重复 route、无对应设备的 route 都使 prepare 原子失败；
7. 不允许 fallback 到 vCPU 0。

trigger 不放在 route 表中。它由设备资源声明，避免 route 和 line 保存两份
可能冲突的 trigger。

### 6.3 未来 `GICD_IROUTER`

本模块不公开 speculative route trait。controller 内部的 `InterruptRecord`
已经拥有 `target: VgicVcpuId`，固定配置只是在 prepare 阶段设置初值。

未来实现 `GICD_IROUTER` 时：

1. MMIO adapter 解码 affinity；
2. 架构层把 affinity 解析到 `VgicVcpuId`；
3. 调用 controller 内部 route 状态转换；
4. 如 pending SPI 的 target 改变，旧 target 的 stale kick 成为 no-op，
   新 target 获得 kick；
5. Router、Fabric、sender、delivery port 和 LR refill 接口保持不变。

这里不提前暴露未使用公共方法；只要求当前状态归属不阻塞上述演进。

## 7. VGIC controller 设计

### 7.1 所有权

每个 emulated-GICv3 VM 创建一个：

```text
Arc<ArmVgicController>
```

controller 在 prepare 时只为验证后的固定 route 创建
`ArmSpiIntId → InterruptRecord` 索引；lookup 在 line/LR 路径不分配。
实现可选适合 `no_std` 的有序 map 或验证后定长表，但不能沿用 512 上限，也
不能在 IRQ 路径按 INTID 扩容。

共享者：

- VGIC distributor/redistributor MMIO wrapper；
- `ArmInterruptRouter`；
- 每个 vCPU 的 `ArmVgicDeliveryPort`。

`ArmVgicController` 不持有：

- `AxVM`/`Weak<AxVM>`；
- runtime/dispatcher；
- task/wait queue；
- host IPI callback；
- pCPU-local ICH register reference。

具体组合边界为：

- `arm_vcpu` 保存原始 `IchVcpuContext`，提供 capability-safe 的本地
  sysreg save/restore/access，不依赖 `arm_vgic`；
- `arm_vgic` 保存纯 controller 状态，不依赖 AxVM、ICH 或 runtime；
- AxVM 私有 `AxvmArmVcpu` 组合 `ArmVcpu` 与可选
  `ArmVgicDeliveryPort`，delivery port 持有共享 controller 和
  `VmInterruptSender`，完成 target-side fold/reconcile/refill，并在
  cross-vCPU DIR 时于 controller lock 外 kick LR owner；
- passthrough vCPU 没有 emulated VGIC delivery port。

delivery port 不持有 Router、runtime、task 或 wait queue。它与 Router 复用
同一条规则：controller transition 返回 typed service hint，释放 controller
lock 后才调用 sender。

AxVM wrapper 使用私有 create config 组合两层，不把 `arm_vgic` 类型加入
`arm_vcpu` 公共 API：

```rust
struct AxvmArmVcpuCreateConfig {
    backend: ArmVcpuCreateConfig,
    delivery: Option<ArmVgicDeliveryPort>,
}
```

`VmArchVcpuOps::CreateConfig` 已是 associated type，因此该变化局限在 AArch64
AxVM adapter 和 `PreparedVcpus::create` 调用点。

因此依赖方向为：

```text
arm_vgic state
  ← AArch64 MMIO adapter
  ← AxVM AArch64 Router / delivery port
```

`arm_vgic` 不反向依赖 `axvm`。

### 7.2 SPI 状态

每个已绑定 SPI 至少保存：

```rust
struct InterruptRecord {
    intid: ArmSpiIntId,
    trigger: TriggerMode,
    enabled: bool,
    line_asserted: bool,
    pending_latch: bool,
    active: bool,
    inflight_epoch: Option<DeliveryEpoch>,
    deactivation_epoch: Option<DeliveryEpoch>,
    target: VgicVcpuId,
}
```

字段语义：

- `line_asserted`：设备 level input 当前状态；edge line 始终为 false；
- `pending_latch`：已经捕获但尚未消费的 edge，或以后 guest ISPENDR
  产生的软件 latch；
- `active`：最近一次 fold 已确认 guest acknowledge、尚未 deactivate 的实例；
- `inflight_epoch`：该记录当前驻留在目标 vCPU live/saved LR 集合中的
  instance；`None` 表示不驻留；
- `deactivation_epoch`：TDIR 捕获、尚未由 LR owner 应用的 instance；
- `enabled`：现有最小 enable 状态；
- `target`：controller 唯一的路由状态。

controller 使用 checked `u64` counter 分配 `DeliveryEpoch`，在同一 VM
生命周期内不复用，耗尽时返回明确错误。delivery commit 将 epoch 同时写入
record 和该 vCPU 的 LR slot map；TDIR request 只引用当时的
`inflight_epoch`。

是否使用一个 `InterruptState` enum 表达 pending/active 四态由实现选择，但
不得让 MMIO wrapper、Router 和 delivery port 各自保存一份持久状态。guest
运行期间 LR 是硬件执行副本，controller 在 fold 后保存 durable truth；
controller record 与该 vCPU 的 LR slot→INTID 驻留映射共同维持一致性。

### 7.3 deliverable 判定

概念上：

```text
source_pending =
  level ? line_asserted : pending_latch

deliverable =
  enabled
  && inflight_epoch.is_none()
  && !active
  && deactivation_epoch.is_none()
  && source_pending
```

本模块不增加 `redelivery_pending` 这一第二真源：edge 重触发由
`pending_latch` 表示，level 重投条件由 `line_asserted` 表示，是否可装载再
结合 `active/inflight` 推导。必须用测试证明 active 期间的新 edge 和
asserted level 不会丢失。

`deactivation_epoch` 不是新的 pending 真源，而是 EOImode=1 cross-vCPU
DIR 到 LR owner 的一次性 completion command。只有 owner delivery port
确认 epoch 与 resident slot 一致、清除 LR active bit 并 fold 后才能消费。
同 epoch 的重复 DIR 幂等合并；旧 epoch 的迟到 command 不能作用于随后同
INTID 的新 residency。

`deliverable` 只表示能占用一个空 LR，不等于“target 是否需要运行”。对
`inflight_epoch.is_some()` 的记录，line/pending 变化可能要求修正 resident
LR：

```text
target_service_needed =
  deliverable
  || inflight_record_may_need_reconcile
```

line producer 不读取远程 LR，因此对 inflight record 的 pulse/lower 可以
保守返回 target service hint。目标 vCPU 读取真实 LR state 后决定是原位更新、
撤销 Pending，还是 no-op。

### 7.4 edge

状态转换：

```text
pulse while inactive
  → pending_latch = true
  → target becomes kickable

commit to LR
  → pending_latch consumed
  → inflight = true

pulse while LR active/inflight
  → retain one pending instance
  → request target reconcile

target sees LR Active + retained pulse
  → rewrite resident LR as Active+Pending
  → consume retained latch

target sees LR Pending or Active+Pending + retained pulse
  → existing LR already represents one pending instance
  → coalesce retained latch

deactivate
  → Active+Pending becomes Pending in the virtual CPU interface
  → no pulse is lost
```

本模块只要求至少保留一个额外 edge，不定义无限 edge 计数队列。连续相同
电气 edge 在 GIC pending latch 已置位时可以合并，符合 GIC pending 语义。
edge 不依赖 level EOI maintenance；Active 期间的 retained pulse 必须通过
resident LR 的 Active+Pending 状态表达。

### 7.5 level

状态转换：

```text
raise
  → line_asserted = true
  → if enabled and not inflight: deliverable

commit to LR
  → inflight = true

guest acknowledge
  → active = true

lower while active
  → line_asserted = false
  → current active instance remains active

lower while resident LR is Pending
  → request target reconcile
  → target invalidates Pending LR before the next guest entry

deactivate while asserted
  → active/inflight = false
  → deliverable again

deactivate after lower
  → active/inflight = false
  → inactive
```

软件 level 不主动构造 `Active+Pending` LR。active 期间再次 raise 只保持
`line_asserted=true`；guest EOIR 仅 priority drop 而尚未 deactivate 时也
不能重投。只有观察到 active bit 清除后，controller 才依据当前 line level
重新 pending。

raise 后、LR commit 前发生 lower 时，不应留下待注入事件。LR commit 后、
guest acknowledge 前发生 lower 时，目标端必须 reconcile resident Pending
LR。所有 commit/reconcile 都在 controller lock 内重新检查当前状态，而不是
信任 dispatcher 中的旧 kick。

### 7.6 enable

本模块不补全 GICD enable/pending MMIO，但现有最小 `ISENABLER` 行为必须接入
同一个 `enabled`：

- module-owned SPI 的 `ISENABLER` read/write 委托 controller record，不再
  读写旧 `VgicInt` enable 副本；
- 合法但未绑定的 SPI read 返回 0，write 返回明确 unsupported/invalid
  policy；越界寄存器访问不得索引数组；
- disabled SPI 的 line 状态仍被记录；
- enable 时如果 level asserted 或 edge latch 已存在，则变为 deliverable；
- disable 不伪造 EOI，也不直接清 active；
- 固定配置绑定的 module-owned SPI 在 prepare 时初始化为 enabled；
- end-to-end guest 不依赖 `ISENABLER`，现有最小 enable 路径只由寄存器适配
  和确定性测试覆盖。

未来补全 `ICENABLER` 等寄存器时继续调用 controller 状态转换，不重建投递链。

### 7.7 最小 capability API

`arm_vgic` 对 `axvm` 暴露的是状态能力，不是寄存器能力。建议的最小边界为：

- route/trigger binding：prepare 阶段原子安装验证后的 SPI record；
- line input：`pulse`、`set_level`；
- guest control input：本模块只接入已有的 `set_enabled`；
- guest completion input：TDIR trap 提交 typed INTID deactivation request；
- resident reconcile：根据 LR observation 和 controller 最新 line/latch
  生成原位 LR 更新或撤销动作；
- delivery selection：按 target 和空槽预算选择 descriptor，并在本地 LR
  write 成功的同一短临界区 commit `inflight`；
- fold input：接收 delivery port 从 LR/EISR 读取的纯数据 observation，
  以 LR active 状态变化判定 deactivate，EISR 只作为 maintenance 原因；
- query/hint：返回需要 kick 的 target 和是否仍有 software pending。

delivery descriptor 只包含经过验证的 INTID、trigger、priority/group 和恢复
state，不包含 LR index、sysreg 地址、pCPU 或 sender。fold observation 只
描述 vCPU owner、INTID、LR state 和 deactivate 事实。这样：

- `arm_vgic` 无 ICH/sysreg、AxVM、runtime 或 host IRQ 依赖；
- `ArmVgicDeliveryPort` 决定 LR slot 并执行本地寄存器访问；
- 未来 IROUTER 和完整 MMIO 只是 controller 的新输入；
- 不需要让 Router、MMIO wrapper 或测试读取 controller 内部字段。

由于本地 sysreg write 在字段验证后是不可失败操作，首版可由 controller
在锁内调用一个 capability-bounded delivery closure 完成“写 LR + 状态
commit”。
若实现层把寄存器写建模为可失败操作，则必须改用 prepare/commit token，并在
commit 前重验；不得先清 pending，再把一个可能失败的 LR write 留在锁外。

## 8. Router 与 kick 语义

### 8.1 ArmInterruptRouter

```rust
struct ArmInterruptRouter {
    controller: Arc<ArmVgicController>,
    sender: VmInterruptSender,
}
```

它实现现有 `IrqSink`：

```rust
fn set_level(&self, line: IrqLineId, asserted: bool) -> IrqResult;
fn pulse(&self, line: IrqLineId) -> IrqResult;
```

步骤：

1. 把 line 校验/转换为 `ArmSpiIntId`；
2. 在 controller lock 内执行状态转换；
3. 取得需要 refill 或 resident reconcile 的 target 集合；
4. 释放 controller lock；
5. 对每个 target 调用 sender。

Router 不写 LR、不直接唤醒 task、不访问远程 vCPU。

### 8.2 为什么 dispatcher event 是 kick

`PendingVcpuInterrupt` 在其他架构可以代表最终注入事件。对 AArch64
emulated SPI，它只代表：

> target vCPU 的 VGIC 状态可能有新的可递送中断，请在目标上下文 fold/refill。

event 的 INTID 和 trigger 用于诊断、去重测试和定位来源，但目标端不能仅凭
event 直接写 LR。真实 pending/active/target 必须从 controller 重新读取。

因此：

- raise 后 lower：旧 kick 到达时 refill 发现无 deliverable，成功 no-op；
- 多个 pulse 合并：多个 kick 可以合并或重复，controller 决定实际次数；
- LR 满：kick 被消费，但 controller pending 保留；
- sender 失败：状态仍 pending，下一次 guest-entry refill 可以恢复；
- VM restart：旧 sender 查找当前 runtime，不缓存旧 dispatcher。即使并发的
  旧 Router 在 reset 后产生一个 stale kick，目标 delivery port 读取的是新
  controller，因而该 kick 成为 no-op；旧 controller 状态不会迁移。

### 8.3 kick 抑制

controller 可以在 target 从“无待处理工作”变为“需要 refill/reconcile”时
才请求 kick，避免每次重复 raise 都入队。以下情况即使没有新空槽
deliverable，也必须产生或保留 service hint：

- inflight edge 又收到 pulse；
- inflight level lower；
- TDIR 提交了本地或 cross-vCPU deactivation request；
- 未来 disable 或 route change 影响 resident LR。

该优化不能改变正确性：

- run loop 每次 guest entry 都检查/refill；
- maintenance underflow/EOI 会再次触发目标处理，fold 再确认是否已
  deactivate；
- stale/重复 kick 始终允许。

不需要在共享 dispatcher 增加取消、token 或 AArch64 专用 trait。

## 9. per-vCPU ICH context

### 9.1 状态归属

每个 `ArmVcpu` 保存 raw ICH context，AxVM 私有 `AxvmArmVcpu` 另持有可选
delivery port：

```rust
struct IchVcpuContext {
    hcr: u64,
    vmcr: u64,
    ap0r: [u64; MAX_APR_REGS],
    ap1r: [u64; MAX_APR_REGS],
    lrs: [u64; MAX_LRS],
    lr_capacity: usize,
}
```

实际数组形状可以按现有 no_std/const 限制调整。`ICH_VTR_EL2` 是 host CPU
能力，不作为可写 vCPU 状态保存。delivery port 另保存该 vCPU 的
`LR slot → (ArmSpiIntId, DeliveryEpoch)` 驻留映射；active record 在 guest
deactivate 前不得从 live/saved LR 集合驱逐。

### 9.2 ICH capability profile

`ICH_VTR_EL2` 只能在当前 pCPU 读取，VM prepare 不得跨 CPU 直接读 sysreg。
每个 `ArmPerCpu::hardware_enable()` 在本地采集并发布
`IchCapabilityProfile` 到 AArch64 capability registry；VM prepare 再按
vCPU placement 查询 registry。profile 至少记录：

- LR count；
- VINTID 可表示的 ID bits；
- priority/preemption bits；
- 因 priority/preemption 能力决定的 APR register count；
- TDS，即是否支持 `ICH_HCR_EL2.TDIR` 单独捕获 DIR。

首版兼容规则：

- 选择所有目标 pCPU 共同支持的最小 LR count；
- 每个允许的 pCPU 都必须能表示本模块全部 SPI INTID；
- priority/preemption/APR shape 必须一致；首版不做有损降级；
- 每个允许的 pCPU 都必须报告 TDS=1；不实现缺少 TDIR 时扩大
  `TALL0/TALL1/TC` trap 的兼容路径；
- host GICv3 probe 必须已经发布 typed maintenance PPI，且全局 per-CPU
  handler 覆盖每个允许的 pCPU；
- placement 中任一 pCPU 尚未发布 profile 时 prepare 失败，不猜测能力；
- 拒绝 LR count 为 0、不支持 GICv3 virtualization 或 profile 不兼容的
  placement；
- vCPU context 只保存/恢复验证后的共同 profile；
- bind 时复核当前 pCPU profile，变化时返回显式 capability error；
- 不把 QEMU 的 LR 数或同构 CPU 假设写成常量。

以后若需要支持异构 profile，可以在 capability negotiation 内增加安全交集，
不改变 controller、Router 或 delivery port 的职责。

### 9.3 bind/unbind 与 run 顺序

推荐顺序：

```text
one CPU pin + current-vCPU publication
  → bind on pinned pCPU
      → validate pCPU capability
      → restore ICH VMCR/APR/LR
      → restore/enable this vCPU ICH_HCR
  → before each guest entry
      → fold observable retired LR state
      → drain dispatcher kicks
      → reconcile resident LR against latest controller state
      → refill free LR from controller
      → configure EOI/underflow/TDIR controls
      → enter guest
  → after each VM exit
      → inspect MISR/EISR when applicable
      → fold LR state into controller
      → refill if run loop will immediately continue
  → unbind on the same pinned pCPU
      → fold latest LR state
      → save ICH context
      → disable/clear live ICH interface state owned by this vCPU
  → withdraw current-vCPU publication and release CPU pin
  → only then execute deferred work
```

不得在 restore 前把另一个 vCPU 的 LR fold 到当前 controller，也不得在
unbind 后留下有效 LR。实现应调整通用 run-loop 编排或提供等价的
architecture-scoped bound transaction；不能依赖任务 affinity 碰巧只有一个
pCPU。现有 nested `with_current_cpu_set()` 对同一 vCPU 可重入，允许在外层
pin 内复用现有 bind/run/unbind 状态转换。

模块 3 同时支持 guest `EOImode=0` 和 `EOImode=1`：

- active emulated SPI 的 LR slot 在 deactivate 前保留于该 vCPU 的 live
  或 saved context，不产生 active-outside-LR；
- `EOImode=0` 下 EOIR 可同时 priority drop 和 deactivate；
- `EOImode=1` 下 EOIR 只 priority drop，后续 DIR 才 deactivate；
- fold 以 LR active bit 的实际清除为完成条件，不能把任意 EOI
  maintenance observation 都当作 deactivate；
- emulated VGICv3 vCPU 的 guest entry 始终启用 TDIR，避免“另一个 vCPU
  在 owner 尚未 fold 出 active 状态前执行 DIR”的窗口；trap handler 把
  DIR 解码为 typed INTID，module-owned SPI 进入 controller
  deactivation request；
- DIR 在 LR owner vCPU 上发生时本地清 active；在另一 vCPU 上发生时只修改
  controller command 并 kick owner，由 owner delivery port 本地完成；
- TDIR 对 INTID 不可选择，因此未被 module 3 controller 拥有的 SGI/PPI/DIR
  必须委托给保持既有行为的 ICH deactivation compatibility helper，不能吞掉；
- 因不存在 active-outside-LR，本模块不需要 EOIcount 补偿，也不启用
  LRENPIE。

若实现不能维持 active-resident 约束，prepare/运行边界必须返回明确
unsupported/error，不能静默遗失 active state。以后支持 active-outside-LR
时，需在 ICH adapter 内增加 EOIcount/LRENPIE 等机制，但不改变
controller/Router/sender 主链路。

当前 AArch64 `ArmHostOps::fetch_pending_host_irq()` 会通过
`gic::fetch_irq()` 调用平台 IRQ dispatch，之后才返回
`ArmVmExit::ExternalInterrupt`；通用 deferred external-IRQ hook 目前还会
再次进入 host IRQ dispatch。模块 3 必须把 host IRQ transaction 收敛为唯一
owner，不能让 maintenance PPI 被 acknowledge/EOI 两次。

推荐保持平台 GIC dispatch 为 canonical owner：

```text
lower-EL maintenance IRQ exit
  → fetch_pending_host_irq/platform dispatch while target vCPU is bound
      → IRQ framework/GIC completes host PPI acknowledge/EOI transaction
      → registered maintenance action identifies this typed IrqId
      → records per-pCPU maintenance observation only
  → return ArmVmExit::ExternalInterrupt
  → unbind: save/clear this vCPU ICH context
      → read MISR/EISR and fold/reconcile LR before clearing live ICH
  → deferred phase: do not dispatch the already handled host IRQ again
  → scheduler/run loop iteration
  → bind target vCPU and refill before next guest entry
```

maintenance action 不直接进入 `ArmVgicController`，只使用 per-pCPU
“当前是否有 bound vCPU/是否观察到 maintenance”的最小架构状态。不得把
ICH fold 放进 deferred handler；届时 vCPU 已 unbind，banked ICH 不再有
可靠 owner。AArch64 应覆盖默认 `after_external_interrupt()`，只保留
`check_timer_events()`，不再调用第二次 `dispatch_host_irq(vector)`；其他架构
的默认 hook 不变。普通 host IRQ 的现有一次 dispatch 和 timer check 行为
必须回归。

### 9.4 并发边界

vCPU context 由当前运行该 vCPU 的线程独占。Router 不访问 context。

controller lock 与 ICH 操作的规则：

- fold：先在当前 CPU 读取短小寄存器快照，再在 controller lock 内合并；
- reconcile：在 controller lock 内根据最新 line/latch 复查 resident LR，
  再执行本地 LR 原位更新或撤销；
- refill：确定空 LR 后，在 controller lock 内重新检查并 commit delivery；
- 为避免 lower 与 LR commit 间的过期窗口，可以在同一短临界区完成本地 LR
  write；该临界区不得调用 sender/外部回调；
- 如果实现选择两阶段 commit，则必须使用可验证 token 并在写入前重验；
  不允许无保护的“先取 pending、稍后盲写 LR”。

优先使用短锁内本地 sysreg write，避免为当前单一实现引入公共 token API。

当前 `AxVM::pulse_interrupt()` 在 `with_resources()` 持有 machine lock 时直接
调用 Fabric。AArch64 Router 随后通过 sender 查询同一 VM runtime 会再次获取
machine lock，形成自锁。模块 3 必须让该 wrapper 只在锁内克隆内部
Fabric/sink capability，释放 machine lock 后再调用 `pulse()`；未来
`set_interrupt_level()` 遵循同一模式。AxVM 仍不向外暴露 Fabric 引用。

## 10. LR 编码、fold 与 refill

### 10.1 LR 编码

软件 emulated SPI LR 至少编码：

- VINTID：完整 `u32` INTID；
- State：Pending，edge 重触发所需的 Active+Pending，或 save/restore
  保留的 Active；
- Group：按当前 guest GIC 兼容设置；
- Priority：使用 controller 当前/默认优先级；
- HW：0；
- EOI：软件 level interrupt 置位。

当前 register wrapper 缺少 EOI bit，3A 必须补齐。所有字段编码使用 typed
helper 和范围校验，不直接拼接未经验证的裸 `usize`。

### 10.2 fold

fold 对每个有效 LR：

1. 读取 VINTID 和 state；
2. 通过该 vCPU 的 slot map 找到 controller record 和 delivery epoch；
3. 合并 Pending/Active/Pending+Active/Invalid；
4. 根据 LR active bit 从 1 到 0/Invalid 的变化判定 deactivate；EISR
   表示软件 EOI maintenance 原因，但不能替代 LR state 判定；
5. 对 resident LR 按最新 controller state 做 reconcile：
   - 有匹配该 slot epoch 的 deactivation request 且 LR 仍 Active：本地清
     active 后立即 fold；
   - request epoch 与 slot 不匹配：保留/诊断旧 command，不能修改 LR；
   - 有匹配 request 但 LR 已 inactive：消费已满足的 request；
   - level `Pending` 且已经 lower：写 `Invalid` 并清 inflight；
   - edge `Active` 且有 retained latch：写 `Active+Pending` 并消费 latch；
   - edge 已为 `Pending`/`Active+Pending`：合并多余 latch；
6. level deactivate 后仍 asserted 时重新 pending；只有 priority drop
   而 active 仍为 1 时不重投；
7. 释放已经 Invalid 的 LR；
8. 对未知、重复或 owner 不匹配的 LR 返回可诊断错误。

匹配 epoch 的 deactivation request 必须在释放旧 resident slot 和 refill 新
instance 之前消费。slot 释放后旧 epoch 永不复用；针对旧 active instance 的
迟到 command 因 epoch 不匹配只能被丢弃并诊断，不能应用到随后同 INTID 的
新 pending。

fold 不能仅通过 INTID 猜测 VM；当前 delivery port 已绑定唯一 controller 和
vCPU owner。reconcile 的 LR write 只能由当前 delivery port 在当前 pCPU
完成，远程 lower/pulse 只负责修改状态和发 service kick。

### 10.3 refill

refill：

1. 先完成 resident LR reconcile；
2. 扫描当前 vCPU 可用 LR；
3. controller 按确定性顺序选择该 target 的 deliverable SPI；
4. 在 commit 时标记 inflight；
5. 写入空 LR；
6. 直到没有空 LR 或没有 deliverable；
7. 若仍有软件 pending 且 LR 满，根据 outside-LR pending summary 启用
   `ICH_HCR_EL2.UIE`，否则关闭 UIE；
8. 本模块不使用 NPIE，并禁止 active-outside-LR，因此 LRENPIE 始终关闭；
9. emulated VGICv3 vCPU 始终保持 TDIR 开启；
10. 若没有 outside-LR pending，关闭不再需要的 maintenance enable。

本模块不要求复杂优先级调度。可以按 INTID 或稳定插入顺序选择，但顺序必须
确定、记录并测试；以后增加完整 priority 语义时替换 controller 内部选择策略，
不改变 Router/delivery port。

### 10.4 maintenance interrupt

maintenance PPI 必须通过 host platform IRQ 解析和注册获得 typed `IrqId`，
不能硬编码 QEMU 常见的 PPI 25。具体归属为：

1. `somehal` GICv3 FDT probe 解析 GIC 节点 `interrupts` 中的 maintenance
   specifier；
2. 在 GIC dynamic IRQ domain 分配后，将其翻译为该 domain 中的 typed
   `IrqId`，并经 `ax-plat`/`ax-hal` 的 typed AArch64 platform capability
   发布；AxVM 不直接依赖 `somehal`；
3. AxVM AArch64 的 `ArchOps::register_platform_irq_injector()` 在所有 pCPU
   已完成 virtualization enable 后只注册一次 `request_percpu_irq` action，
   CPU mask 覆盖 enabled virtualization CPUs；
4. host-lifetime owner 保存返回的 `IrqHandle`，重复初始化必须幂等或返回
   明确错误；
5. 现有 hook 返回 `()`，因此 capability registry 需保存
   registered/unavailable/error 状态；缺失 PPI 不阻止纯 passthrough host
   初始化；
6. VM prepare 只检查该 capability 和 CPU coverage，不按 VM 重复注册 PPI，
   emulated mode 遇到 unavailable/error 时返回原始上下文。

IRQ framework/GIC dispatch 是 host PPI transaction 的唯一 owner；注册的
maintenance action 只记录 per-pCPU maintenance observation，不修改 VGIC
controller。AArch64 VGIC delivery path 必须在 unbind 内、仍属于目标
vCPU 的上下文读取
`ICH_MISR_EL2`/`ICH_EISR_EL2` 并 fold；refill 在下一次 bind/guest entry
前完成。普通非 maintenance host IRQ 不触发 VGIC completion。

必须支持：

- EOI maintenance：提示检查软件 level LR；以 LR active bit 是否清除确认
  deactivate；
- underflow maintenance：通过 UIE 观察 LR 出现可用容量，且软件 pending
  尚存；
- NPIE 在本模块保持关闭；refill 不依赖“所有 LR 都不再 pending”才获得通知；
- LRENPIE 在本模块 active-resident 约束下保持关闭；
- 未知 MISR 原因：记录并按明确 unsupported/error 策略处理；
- 非 vCPU 上下文收到 maintenance：作为状态错误诊断，不能猜测 owner；
- handler 无需从 raw vector 推断 guest/INTID；由 unbind 内的 MISR/EISR
  快照识别 VGIC 原因；
- AArch64 external-IRQ deferred hook 不得再次 dispatch 已由
  `fetch_pending_host_irq()` 完成 transaction 的 IRQ。

TDIR 产生的是 trapped system-register exit，不是 maintenance PPI：

```text
guest DIR(INTID) with EOImode=1
  → arm_vcpu returns typed SysRegWrite exit
  → AArch64 adapter recognizes decoded ICC_DIR_EL1 write
  → module-owned SPI:
      controller snapshots current inflight epoch into deactivation request
      → same owner: delivery port clears local resident LR active bit
      → remote owner: sender kick/IPI; owner clears it before next entry
  → non-module-owned INTID:
      delegate to ICH compatibility deactivation helper
  → fold confirms active→inactive and applies level re-pend if asserted
```

trap handler 不直接写远程 banked LR，也不把 DIR 当成 guest active MMIO。
unknown INTID 或 owner mapping 不一致返回可诊断错误；同 epoch 重复 DIR
幂等合并，无当前 inflight instance 的 DIR 是带诊断的 no-op。旧 epoch
command 不能把新的 pending instance 误清除。

## 11. 初始化与生命周期

### 11.1 构造顺序

```text
1. 解析 VM config、vCPU placement 和 fixed_irq_routes
2. 验证 host GICv3 virtualization/TDS/maintenance capability 与共同 profile
3. 创建 ArmVgicController
4. 用 AxVMRef 创建 VmInterruptSender
5. 创建带 ArmVgicDeliveryPort 的 vCPU
6. 创建 ArmInterruptRouter
7. 创建 InterruptFabric
8. 用同一 controller 创建 VGIC MMIO wrapper
9. 构造其他设备并向其提供 IrqLine
10. 收集 Resource::IrqLine，绑定 trigger + fixed route
11. 校验 route 无遗漏/无多余
12. 原子提交 PreparedVm
```

当前 `AxVmDevices` 对 `InterruptController` 使用 legacy fallback 并在内部
执行 `Vgic::new()`。emulated GICv3 分支必须由 AArch64 prepare 认领这一
architecture-owned 配置：

- 从 common-device 输入中排除且只排除该 emulated interrupt-controller
  config，拒绝重复 controller；
- 用预先创建的 `Arc<ArmVgicController>` 构造 VGIC MMIO wrapper，并将
  wrapper 注册到同一个临时 `PreparedDevices`；
- 其余设备仍经原 `DeviceFactoryRegistry` 和 `DeviceBuildContext` 构造；
- 不向通用 `DeviceBuildContext` 塞入 `Any`/downcast 架构服务，也不允许
  legacy fallback 再创建第二个 `Vgic`；
- `prepare_with_factories()` 传入的任意 custom Fabric 无法证明与该
  controller 同源，emulated GICv3 模式显式拒绝这一组合，不能静默使用或
  忽略；synthetic factory 通过 test feature 注册到正常 AArch64 prepare；
- passthrough/GPPT 分支继续使用现有专用设备和初始化路径。

设备在 bundle 提交前不得开始异步发中断。若任一步失败，未提交 controller、
Router、device 和 line 随 prepared bundle 一并释放。

### 11.2 引用关系

```text
AxVM
  → Prepared/registered devices
      → IrqLine
          → Arc<dyn IrqSink>
              → ArmInterruptRouter
                  → Arc<ArmVgicController>
                  → VmInterruptSender
                      → Weak<AxVM>

AxvmArmVcpu
  → ArmVgicDeliveryPort
      → Arc<ArmVgicController>
      → VmInterruptSender
          → Weak<AxVM>

VGIC MMIO wrapper
  → Arc<ArmVgicController>
```

强引用没有回到 `AxVM`，不存在 `AxVM → device → Router → AxVM` 环。

### 11.3 reset

- reset 创建新的 controller、Router、Fabric、delivery port 和 runtime；
- 旧 line 只引用旧 Router/controller；
- sender 每次发送重新选择当前 runtime，不缓存旧 dispatcher；
- 并发旧 sender 可能向新 runtime 发出 stale kick，但新 delivery port
  只读取新 controller，必须将其作为 no-op；
- 旧 VM resource bundle 被释放后，旧 route/pending/LR context 一并消失；
- reset 后的新 vCPU 必须从空 ICH state 开始。

### 11.4 passthrough/GPPT

`interrupt_mode = "passthrough"` 和 GPPT device 继续走现有路径：

- 不要求 `fixed_irq_routes`；
- 不创建 emulated `ArmInterruptRouter`；
- 不把 host passthrough SPI 误送入软件 controller；
- 现有配置和构建必须回归通过。

## 12. synthetic SPI 测试设备

### 12.1 目的

当前没有 AArch64 emulated production device 使用 `IrqLine` 发 SPI。为验证
真实 device factory 和 MMIO 流程，增加仅测试配置启用的 synthetic device，
而不是顺带实现新的生产 PL011。

### 12.2 能力

基本语义配置至少创建两类实例：

- edge instance：固定 edge `IrqLine`，写 trigger register 调用 `pulse()`；
- level instance：固定 level `IrqLine`，写 assert register 调用 `raise()`，
  guest handler 写 acknowledge/clear register 调用 `lower()`。

另提供只用于测试报告的 MMIO register。guest 写入约定的 phase/result 编码，
device 在 host console 输出稳定、可匹配的 marker；它不改变 line、
pending/active 或 route，也不是中断注入后门。这样端到端用例不需要把 host
PL011 同时 passthrough 给 guest，避免 host/guest 争用同一串口。

配置/ABI 固定为：

- 一个 feature-gated、test-only 的
  `EmulatedDeviceType::SyntheticIrqTest`；该 variant、serde 映射、factory
  和 AxVisor feature 必须一起启用，不能复用已有 `Dummy`；
- `base_gpa`/`length` 指定独占 4 KiB MMIO range，`irq_id` 是唯一
  `Resource::IrqLine`，`cfg_list = [0]` 表示 edge、`[1]` 表示 level；
- offset `0x00`：edge `PULSE`，只接受 Dword 写 `1`；
- offset `0x04`：level `RAISE`，只接受 Dword 写 `1`；
- offset `0x08`：level `LOWER`，只接受 Dword 写 `1`；
- offset `0x10`：`REPORT`，接受测试协议的 Dword phase/result code 并输出
  host marker；
- offset `0x18`：只读 `STATUS`，返回设备 line 与触发/报告计数的测试快照。

edge instance 的 level register、level instance 的 pulse register、未知
`cfg_list`、offset、width 或 value 都返回 typed invalid/unsupported error。
VM config 和 guest crate 使用同一份文档化 test ABI 常量；运行时代码不从
guest 二进制反推配置。

MMIO `handle_write()` 必须把 `IrqError` 带上下文转换成 `DeviceError::Backend`
并返回，不能用日志替代错误或把 sender/runtime failure 当作成功。

LR pressure 配置复用同一 edge factory 创建 17 个不同 SPI 实例；GICv3
virtualization interface 最多有 16 个 LR，因此无需读取或硬编码当前 QEMU
具体 LR count，也能保证至少一个 SPI 留在 LR 外。每个实例仍只拥有正常
`Resource::IrqLine`，不增加测试专用批量注入后门。

可提供只读状态/计数器用于测试诊断，但设备不复制 VGIC pending/active 状态。
设备状态只表达自身电气 source 是否 asserted 和 guest 可见测试计数。

### 12.3 隔离

- 设备类型受专用 test feature/config 保护；
- test-only enum variant 使用保留编号，未启用 feature 时同一数值不能被
  解释为其他设备；
- 默认 AxVisor 产品配置不可创建；
- 普通配置引用未启用的测试设备时返回 unsupported；
- 测试代码仍通过正常 `DeviceFactory`、`Resource::IrqLine` 和
  `DeviceBuildContext::resolve_irq()`；
- 不为测试暴露 controller internals 或 raw LR 写入口。

## 13. 错误与可诊断性

至少区分：

- invalid SPI INTID；
- missing/duplicate/unused fixed route；
- target vCPU not found；
- trigger mismatch；
- host GICv3 virtualization unavailable；
- incompatible ICH profile（LR/ID bits/APR/TDS）；
- maintenance IRQ unavailable；
- unknown inflight LR/owner mismatch；
- sender/runtime unavailable；
- unsupported interrupt mode；
- emulated GICv3 with caller-provided custom Fabric。

可恢复配置和资源错误返回 typed error；不使用裸 `unwrap`、panic、`.ok()` 或
静默 vCPU 0 fallback。

日志至少包含 VM ID、vCPU ID、INTID、trigger、route target、当前 pCPU 和
操作阶段（route、kick、fold、refill、deactivate）。高频正常中断只使用
trace/debug，错误路径使用可定位但有限速意识的日志。

## 14. 锁与中断上下文

### 14.1 锁顺序

固定顺序：

```text
controller lock
  → release
  → VmInterruptSender
  → machine/runtime/dispatcher locks
  → release
  → notify/host IPI
```

禁止：

- controller lock 内调用 sender；
- machine/runtime lock 内回调 Router/controller；
- controller lock 内调用设备或 MMIO 外部回调；
- 源 pCPU 获取目标 vCPU backend lock 后写 ICH。

### 14.2 分配

`IrqSink` 本身不睡眠。模块 3 不新增 host physical hard-IRQ producer；
synthetic emulated device 从 vCPU MMIO exit 上下文调用。module 1 dispatcher
现有 queue 分配语义保持不变。

若以后 host hard-IRQ 接入，必须另行设计预分配/deferred path，不能假定本模块
允许在任意 hard-IRQ 中分配。

## 15. 测试设计

### 15.1 `arm_vgic` 单元测试

- INTID 32、255、256、1019；
- 拒绝 31、1020；
- `ISENABLER` 对绑定的 512/1019 无越界且更新同一 record，未绑定 bit 不产生
  隐式 record；
- 固定 route 成功、重复、缺失、非法 target；
- disabled level raise 后 enable；
- level raise→lower before commit；
- level lower after Pending LR commit, before acknowledge；
- level active→lower→deactivate 不重投；
- level active→EOIR（`EOImode=1`）仍 active 时不重投；
- level active→deactivate while asserted 重投；
- edge pulse；
- edge inflight 期间再次 pulse；
- edge Active + retained pulse 收敛为 LR Active+Pending；
- 多次 edge 合并语义；
- stale kick 不产生 delivery；
- route/trigger 不形成双重状态源。

### 15.2 ICH 纯逻辑/adapter 测试

- LR encode 保留 INTID 256；
- EOI bit 只为需要的软件 level LR 设置；
- LR state fold 四态；
- resident level Pending cancel 和 edge Active+Pending reconcile；
- EISR maintenance hint 与 LR active→inactive deactivation 判定；
- `EOImode=0` EOIR-deactivate 和 `EOImode=1` EOIR→DIR-deactivate；
- TDS capability 缺失时 emulated mode prepare 失败；
- TDIR local DIR 与 cross-vCPU DIR 均只由 LR owner 清 active；
- TDIR 捕获非 module-owned SGI/PPI 时委托 compatibility helper，不丢失；
- stale/重复 DIR 不清除随后产生的新 pending instance；
- active LR 不被 refill/折叠逻辑驱逐到 LR 外；
- host maintenance/普通 IRQ 各只 dispatch/acknowledge/EOI 一次，AArch64
  deferred hook 仍执行 timer check；
- 空 LR refill；
- LR 满时保留软件 pending；
- LR 释放后继续 refill；
- underflow enable/disable；
- UIE 与 outside-LR pending summary 的映射，NPIE/LRENPIE 保持关闭；
- owner mismatch/unknown LR 错误；
- save/restore round trip；
- 不同 LR capacity 的共同下限。

寄存器访问通过 crate-private adapter 或可测试纯函数隔离，不为测试增加公共
硬件 abstraction。

### 15.3 Router/runtime 测试

- Router 在 controller lock 释放后调用 sender；
- `AxVM::pulse_interrupt` 在进入 Router/sender 前已经释放 machine lock；
- 同一 target 重复 raise 不要求重复 kick；
- remote target 得到正确 vCPU ID；
- missing route 不 fallback；
- sender 错误传播；
- sender 暂时失败不清当前 controller pending；
- dispatcher 事件到达后 AArch64 adapter 调用 refill，而不是盲写 event INTID；
- stale kick 成功 no-op；
- reset 后旧 controller pending 不迁移，迟到 kick 对新 controller 无害。

### 15.4 QEMU SMP2

新增 case：
`test-suit/axvisor/normal/qemu/emulated-gicv3-spi-smp2/qemu-aarch64.toml`，
并在同目录增加专用 `build-aarch64-unknown-none-softfloat.toml`。专用 wrapper
沿用普通 AArch64 wrapper 的 host target/features，但引用本用例自己的 VM
config 和下述 guest build；它构成独立 build group，不把测试 feature、guest
产物或 emulated VM config 带入现有 smoke。QEMU machine 明确使用
`virt,virtualization=on,gic-version=3` 和至少 `-smp 2`。测试 VM 配置把
`phys_cpu_ids = [0, 1]` 用作 guest MPIDR，并以
`phys_cpu_sets = [1, 2]` 将 vCPU 0/1 分别固定到 host pCPU 0/1，避免
“remote target”退化成同 CPU wake；同时使用 virtualized address-space
policy，不 passthrough QEMU root device tree，避免 synthetic GPA 与 host
platform MMIO 形成第二条路径。

#### 15.4.1 guest 产物

guest 源码固定放在
`test-suit/axvisor/guests/emulated-gicv3-spi-smp2/`，是一个专用 `no_std`
AArch64 EL1 binary crate，包含自己的 linker script、异常向量和最小启动代码。
其 manifest 用独立 `[workspace]` 退出根 workspace，避免默认 host
workspace build/clippy 尝试为错误 target 构建该 EL1 binary。不复用 Alpine
userspace：EL0 程序既不能配置 GIC system registers，也不能直接安装 EL1 IRQ
vector。guest 提交自己的 `Cargo.lock`，runner 使用 `--locked` 和
workspace `tmp` 下的独立 target dir，不在源码目录产生构建文件。仓库不提交
生成的 ELF 或 raw binary。

AxVisor build wrapper 增加可选、typed 的 `guest_builds` 列表；每项至少明确
`manifest_path`、`bin`、`target`、`profile`、`locked`、`output_format`
和 workspace-relative `output`。本用例选择 pinned Rust 2024 nightly、
`aarch64-unknown-none-softfloat`、release profile 和 raw binary，输出到
`tmp/axbuild/axvisor-guests/emulated-gicv3-spi-smp2.bin`。不接受任意 shell
prebuild 字符串。

`cargo xtask axvisor test qemu` 的 build-group phase 按以下顺序执行：

1. 解析并校验 `guest_builds`，拒绝重复 output、越出 workspace/tmp 的 output
   和不受支持的 output format；
2. 用 pinned toolchain 构建 guest ELF，并使用同 toolchain 的 LLVM objcopy
   生成临时 raw binary；
3. 原子替换最终 output，保证失败构建不留下可被 host `include_bytes!` 误用的
   半成品；同一 build group 内按完整 build 描述去重；
4. 再加载/构建 AxVisor host artifact。

本用例 VM config 使用 `image_location = "memory"`，`kernel_path` 指向上述
workspace `tmp` 产物；`os/axvisor/build.rs` 的现有 `include_bytes!` 流程把它
嵌入 host image。缺失/越界的 guest manifest、编译/objcopy 失败或 output
不存在都在 host build 前报带 build-group 上下文的错误。该 runner 扩展只负责
可重复地产生测试 guest，不改变 AxVM boot/runtime API。

#### 15.4.2 guest 协议

测试 guest：

1. vCPU 0 作为 BSP，以 PSCI `CPU_ON`/HVC 启动 MPIDR 对应的 vCPU 1；两者
   从 `MPIDR_EL1` 选择角色，以共享内存原子量握手，不依赖 `CPU_ON`
   context 参数；
2. vCPU 1 安装 EL1 IRQ vector，配置 `ICC_SRE_EL1`、`ICC_PMR_EL1`、
   `ICC_IGRPEN1_EL1` 和选定 EOImode 后解除 IRQ mask，再发布 ready；
   module-owned SPI 已在 VM prepare 时按配置启用，guest 不需要
   `GICD_ISENABLER`，因而不把完整 guest enable MMIO 带入模块 3；
3. vCPU 1 执行 `WFI`，vCPU 0 写 edge device，vCPU 1 读取
   `ICC_IAR1_EL1`、校验 INTID、完成 EOI/deactivate 并确认计数恰好增加一次；
4. 使用 INTID 256 重复一次；
5. vCPU 0 raise level device；
6. vCPU 1 完成 deactivate 但暂不 lower，观察至少一次重投；
7. vCPU 1 写 device acknowledge/lower，再完成 deactivate，确认计数停止；
8. vCPU 1 屏蔽 guest IRQ，vCPU 0 pulse 17 个不同 edge SPI，再由 vCPU 1
   解除屏蔽并确认 17 个 INTID 全部且各一次完成，强制覆盖 LR refill；
9. vCPU 1 通过 synthetic report register 提交各阶段计数和最终 PASS，host
   device 输出明确 marker；错误路径提交 FAIL code 后停止。

测试 guest 至少覆盖其实际配置的一种 EOImode；另一种 EOImode 必须由
ICH adapter 确定性测试覆盖。host 侧同时确认 LR write/refill 日志只来自
target vCPU 当前 pCPU。

执行入口：

```bash
cargo xtask axvisor test qemu --arch aarch64 --test-group normal \
  --test-case emulated-gicv3-spi-smp2
```

### 15.5 回归

- 现有 QEMU AArch64 passthrough build；
- 至少一个现有 passthrough QEMU guest；
- 非 AArch64 架构编译；
- `axvmconfig` 旧配置反序列化；
- module 1 sender/dispatcher tests。

## 16. 子模块与合入顺序

### 3A：INTID/寄存器修正

只修无损 INTID、typed LR error 和缺失的 LR EOI、HCR TDIR/maintenance、
VTR TDS、MISR/EISR fields，并提供真实测试调用者。不切换 producer。

### 3B：VGIC state

加入 controller state、固定 route 和纯状态机测试。接口保持 crate 最小可见，
同一变更中由测试和下一层 adapter 消费。

### 3C：ICH lifecycle

实现 per-vCPU context、fold/refill、TDIR completion 和 EOI/underflow
maintenance。此阶段可用 fake controller/adapter 验证，不对设备开放半成品
路径。

### 3D：初始化

加入配置与两阶段构造，保持 passthrough 分支原样；失败不提交半初始化 VM。

### 3E：Router

接通 `IrqLine → controller → sender kick → target refill`，此时才启用新的
emulated SPI producer path。

### 3F：端到端

增加 synthetic device、专用 EL1 SMP2 guest、typed guest prebuild、
VM/QEMU config 和端到端验证，完成行为证据。

每个子模块必须保持可构建、可测试；不得用 `todo!()`、no-op 或 silent
fallback 维持中间状态。若一个新内部 API 在当前子模块没有真实消费者，应与
其首个消费者合并提交。

## 17. 验收映射

| 功能/风险 | 最低层证据 | 集成证据 | 通过条件 |
|-----------|------------|----------|----------|
| INTID 不截断 | LR encode unit test | QEMU INTID 256 | guest 收到 256 |
| 固定显式 route | config/controller test | SMP2 remote route | vCPU 1 收到，vCPU 0 不处理 |
| edge | state test | synthetic edge | 一次 pulse 一次可观察处理 |
| level deactivate 重投 | state + fold/EOImode/TDIR test | synthetic level | asserted 时重投，lower 后停止 |
| LR refill | fake LR capacity test | LR pressure QEMU | 无 panic，所有 SPI 最终完成 |
| ICH 隔离 | context round trip | SMP2/调度运行 | 无跨 vCPU LR 泄漏 |
| CPU owner | pinned run-loop test | SMP2/可迁移 affinity | bind/run/unbind 在同一 pCPU |
| maintenance PPI | FDT/domain/IRQ test | QEMU level | typed PPI 且 host transaction 仅一次 |
| 跨 CPU wake | sender/dispatcher test | SMP2 | target 从等待中被唤醒 |
| stale kick | Router/refill test | raise/lower sequence | 不注入已撤销 level |
| 兼容 | config tests | passthrough QEMU | 旧配置与现有路径不退化 |

## 18. 完成判定

模块 3 只有在以下条件全部满足后完成：

1. device 只持有 `IrqLine`，不知道 VGIC、VM runtime 或 vCPU；
2. controller 保存 line/pending/active/route 的 durable truth，LR 只作为
   当前 vCPU 的 live/saved 硬件执行副本；
3. dispatcher 只负责 kick/wake，不承担 AArch64 pending 真相；
4. LR 只在目标 vCPU 当前 pCPU 上访问；
5. ICH state 随 vCPU 保存恢复；
6. edge、level deactivate 重投、两种 EOImode、LR refill 和跨 CPU wake
   全部通过；
7. INTID 256 有端到端证据；
8. 固定 route 无隐式默认；
9. passthrough/GPPT、SGI/PPI/IPI 和非 AArch64 路径不退化；
10. 未来增加 `GICD_IROUTER` 或完整 MMIO 时不需要改变
    `IrqLine → Router → sender kick → target refill` 主链路。

## 19. 代码量与难度预估

以下是基于当前代码边界的净新增/修改量，不含自动生成文件和第三方代码：

| 子模块 | 生产代码 | 测试/配置 | 难度 | 主要风险 |
|--------|----------|-----------|------|----------|
| 3A | 100–220 行 | 100–180 行 | 中高 | host trait 扩宽、register/TDIR field 正确性 |
| 3B | 280–500 行 | 280–450 行 | 中高 | edge/level/deactivation command 状态机 |
| 3C | 520–900 行 | 380–650 行 | 高 | ICH owner、TDIR、maintenance、refill、platform PPI |
| 3D | 200–360 行 | 100–200 行 | 中高 | 配置兼容、构造顺序和失败回滚 |
| 3E | 150–280 行 | 150–260 行 | 中高 | kick 语义、machine lock、stale event |
| 3F | 150–300 行 | 500–900 行 | 中高 | guest build、EL1 handler、SMP2 和 LR pressure 证据 |

各项存在复用和重叠，整体预计：

- 生产 Rust：约 1,200–2,100 行；
- 单元/集成测试、guest、runner 支持与配置：约 1,300–2,400 行；
- 总变更：约 2,500–4,500 行。

整体难度评为高。3C 是关键路径，3B 和 3D 决定后续是否需要返工；3E 本身代码
不多，但必须等前述状态、生命周期和 capability 边界稳定后再接 producer。
