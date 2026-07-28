# PR 3.3 设计：VGIC SPI 状态机、ICH completion 与 LR refill

## 1. PR 定位

- 建议标题：`feat(arm-vgic): implement emulated SPI delivery state`
- 依赖：PR 3.1、PR 3.2。
- 后继：PR 3.4 只负责配置、所有权、Router 和 producer 接线，不再发明
  edge/level/EOI 语义。
- 预计变更：生产代码 700–1,150 行，测试 650–1,050 行。
- 难度：最高。该 PR 集中解决 capability、active owner、TDIR、
  EOImode、maintenance 和 refill。

本 PR 建立可独立测试的完整底层功能，但不从 VM 配置创建设备 route，也不让
真实设备进入新路径。测试直接构造 route/controller/delivery port。

## 2. 设计不变量

1. `ArmVgicController` 是 line、pending、active 和 route 的 durable truth。
2. LR 是目标 vCPU 的 live/saved execution copy，不是无限 pending 队列。
3. active interrupt 始终驻留于其 owner vCPU 的 LR；本模块不支持
   active-outside-LR。
4. 源 CPU/Router 永不读取或写入目标 CPU 的 ICH。
5. 所有 LR 访问发生在 PR 3.2 的 bound/pinned target vCPU scope。
6. edge active 期间的新 pulse 至少保留一个 pending instance。
7. level 仅在实际 deactivate 后仍 asserted 时重投；priority drop 不等于
   deactivate。
8. LR full 保留软件 pending，不 panic、不丢失。
9. controller lock 内不调用 sender、wake、IPI、设备或外部日志 callback。
10. 同一 INTID 的旧 DIR/maintenance 事件不能清除新 residency。

## 3. crate 和模块边界

### 3.1 `arm_vgic`

新增：

```text
virtualization/arm_vgic/src/spi/
  id.rs
  state.rs
  controller.rs
  transition.rs
```

负责：

- typed SPI/target/epoch；
- fixed route 注册；
- edge/level/enable 状态；
- deliverable 选择；
- delivery commit/fold/deactivation command；
- service hint。

不依赖 AxVM、runtime、ICH register 或 host `IrqId`。

### 3.2 `axvm` AArch64 adapter

新增：

```text
virtualization/axvm/src/arch/aarch64/vgic/
  mod.rs
  delivery.rs
  maintenance.rs
  sysreg.rs
```

负责：

- controller descriptor 与 `IchLrEntry` 转换；
- per-vCPU slot map；
- fold/reconcile/refill；
- TDIR trap；
- maintenance service；
- module-owned SPI 与现有 SGI/PPI compatibility 分流。

本 PR 可把 `AxvmArmVcpu` 从 tuple struct 改为私有 named fields，并允许
测试/后续构造一个 `Option<ArmVgicDeliveryPort>`；PR 3.4 才在 production
prepare 中传入 controller。

### 3.3 platform capability

修改：

- `platforms/somehal/src/arch/aarch64/gic/v3.rs`
- `platforms/somehal/src/arch/aarch64/gic/mod.rs`
- `platforms/somehal/src/irq.rs`
- `platforms/axplat-dyn/src/irq.rs`
- `platforms/ax-plat/src/irq.rs`

发布 typed GIC maintenance PPI capability，不把裸 FDT cell 或 raw IRQ
算术泄漏到 AxVM。

## 4. controller 领域类型

```rust
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ArmSpiIntId(u32); // 32..=1019

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct VgicVcpuId(u32);

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DeliveryEpoch(u64); // 从 1 开始，VM 生命周期内不复用

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResidentInstance {
    owner: VgicVcpuId,
    epoch: DeliveryEpoch,
}

use axdevice_base::InterruptTriggerMode;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArmSpiRoute {
    pub intid: ArmSpiIntId,
    pub target: VgicVcpuId,
}
```

`ArmSpiIntId::new()` 拒绝 31、1020；`VgicVcpuId` 构造不依赖 AxVM，
`TryFrom<usize>` 对超过 `u32::MAX` 的值返回 typed range error，不截断。
`DeliveryEpoch` 的 checked counter 溢出返回 typed error，不能回绕为 1。
`arm_vgic` 已依赖 `axdevice_base`，因此直接复用其
`InterruptTriggerMode::{EdgeTriggered, LevelTriggered}`，不再维护一套同义
trigger enum和转换表。

## 5. controller 状态

每条已绑定 SPI 只有一份：

```rust
struct InterruptRecord {
    intid: ArmSpiIntId,
    route_target: VgicVcpuId,
    trigger: InterruptTriggerMode,
    enabled: bool,
    line_asserted: bool,
    pending_latch: bool, // edge 或未来 guest software-pending，独立于 trigger
    active: bool,
    inflight: Option<ResidentInstance>,
    deactivation: Option<ResidentInstance>,
}
```

当前固定路由下 `inflight.owner == route_target`。仍需从第一版就分开保存二者：
以后 guest 写 `GICD_IROUTER` 时，修改 `route_target` 不能把已 Active LR 的
owner 改掉；旧 instance 由 `inflight.owner` 完成，随后 pending instance 才
按新 route 投递。模块 3 不开放 route mutation API，但这一数据边界避免未来
重写 fold、TDIR 和 service hint。

未来 `ICPENDR` 清除已 resident的 Pending instance时，可增加携带同一
`ResidentInstance` 的 cancel command，并复用本 PR 的 owner reconcile；
未来 active MMIO若产生 active-outside-LR，则扩展 controller/ICH adapter的
active residency策略。二者都不要求 MMIO adapter、Router或源 CPU直接访问
远程 LR。

索引只为 prepare/test 显式注册的 route 创建，使用
`BTreeMap<ArmSpiIntId, InterruptRecord>`；builder阶段插入，sealed后 IRQ路径
只查询/更新既有节点。这样天然提供稳定 INTID顺序，且：

- IRQ 路径 lookup 不分配；
- 不沿用 512 上限；
- 未绑定 INTID 不隐式创建；
- 同一个 INTID 不允许重复 route；
- route 与 trigger 注册为一个原子操作。

module-owned SPI 在注册时 `enabled=true`，使 PR 3.5 guest 无需依赖完整
GICD enable MMIO。现有最小 `ISENABLER` read/write 必须代理同一字段：

- 只访问已注册 record；
- 未绑定 bit RAZ/WI；
- 不形成第二套 `VgicInt.enable`；
- 本 PR 不实现 ICENABLER。

### 5.1 最小 GICD MMIO wrapper

legacy `Vgic::new()` 当前硬编码 `0x0800_0000/64KiB`，并在自己的 512项
`Vgicd` 中保存 enable；它不能直接作为新 production wrapper。`arm_vgic`
增加以下显式构造器：

```rust
pub fn with_spi_controller(
    range: GuestPhysAddrRange,
    controller: Arc<ArmVgicController>,
) -> VgicResult<Self>;
```

该 wrapper：

- `BaseDeviceOps::address_range()` 返回构造时的 range，不再掩码到硬编码
  base；MMIO handler用 `addr.checked_sub(range.start)` 得到 offset并验证
  `offset + width <= length`，删除当前 `addr & 0xfff` alias；PR 3.4 首版要求
  base 64KiB对齐、length恰好64KiB、cfg_list为空；
- GICD_ISENABLER0 的 SGI/PPI bit继续走 legacy private state；
- GICD_ISENABLER1..31 对已注册 SPI逐 bit查询/设置同一 controller record，
  未注册 bit RAZ/WI，可覆盖 INTID 32–1019；
- 本模块代理的 ISENABLER只接受4-byte对齐 Dword访问；Byte/Word/Qword返回
  typed Unsupported，不能沿用当前未移位的 byte/word helper造成别名写；
- 其他本模块未实现寄存器保持现有兼容 RAZ/WI，不借机实现 IROUTER或完整
  pending/active；
- `Vgic::new()` 可暂留给未迁移 caller，但 AArch64 emulated factory禁止
  使用它。

因为 module-owned SPI初始已 enabled且本模块没有 ICENABLER，生产
ISENABLER write只能是幂等 set。若 controller意外返回非空 service hint，
wrapper返回明确 BadState而不能静默丢 hint；未来完整 enable MMIO再在
AxVM adapter层接入 sender。

## 6. controller API

`ServiceHint` 是值类型，不持有 callback：

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceHint {
    None,
    Target(VgicVcpuId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryOutcome {
    NoWork,
    Installed {
        intid: ArmSpiIntId,
        epoch: DeliveryEpoch,
    },
}
```

`None` 表示状态更新后当前无需运行 vCPU；`Target` 只表示该 vCPU需要
fold/reconcile/refill。它不等于一个 pending计数，也不携带 sender。

最小接口固定为：

```rust
pub struct ArmVgicController { /* private IRQ-safe lock + records */ }

impl ArmVgicController {
    pub fn new(
        routes: impl IntoIterator<Item = (ArmSpiRoute, InterruptTriggerMode)>,
    ) -> VgicResult<Self>;

    pub fn pulse(&self, intid: ArmSpiIntId) -> VgicResult<ServiceHint>;
    pub fn set_level(
        &self,
        intid: ArmSpiIntId,
        asserted: bool,
    ) -> VgicResult<ServiceHint>;
    pub fn set_enabled(
        &self,
        intid: ArmSpiIntId,
        enabled: bool,
    ) -> VgicResult<ServiceHint>;

    pub fn deliver_one<E>(
        &self,
        target: VgicVcpuId,
        install: impl FnOnce(DeliveryDescriptor) -> Result<(), E>,
    ) -> Result<DeliveryOutcome, DeliveryError<E>>;

    pub fn fold(
        &self,
        observation: ResidentObservation,
    ) -> VgicResult<FoldOutcome>;

    pub fn reconcile<E>(
        &self,
        observation: ResidentObservation,
        apply: impl FnOnce(ResidentUpdate) -> Result<(), E>,
    ) -> Result<ReconcileOutcome, ReconcileError<E>>;

    pub fn request_deactivation(
        &self,
        intid: ArmSpiIntId,
    ) -> VgicResult<ServiceHint>;
}
```

为解决“factory build 时需要真实 `IrqLine`，而 trigger 只有设备构造后从
`Device::resources()` 的 `Resource::IrqLine` 可确认”的初始化环，另提供
仅可配置一次的 control-plane builder：

```rust
pub struct ArmVgicControllerBuilder { /* owns Arc + unsealed phase */ }

impl ArmVgicControllerBuilder {
    pub fn new() -> Self;
    pub fn controller(&self) -> Arc<ArmVgicController>;
    pub fn register(
        &mut self,
        route: ArmSpiRoute,
        trigger: InterruptTriggerMode,
    ) -> VgicResult<()>;
    pub fn finish(self) -> VgicResult<Arc<ArmVgicController>>;
}
```

`controller()` 允许 PR 3.4 在 device build 前创建 Router/Fabric；controller
未 sealed 时所有 runtime line/MMIO/delivery 操作明确返回 NotReady。设备
bundle尚未对外发布，也不得在 constructor 中发中断。`finish()` 检查 route
完整性并以 Release 发布 sealed phase，此后禁止再创建 record。便利
`ArmVgicController::new(routes)` 可用 builder 实现。builder 和 NotReady
路径必须在本 PR 由测试真实消费。

`ServiceHint` 只表达“哪个 target 值得 service”，不是 pending 真相：

```rust
pub enum ServiceHint {
    None,
    Target(VgicVcpuId),
}
```

### 6.1 原子 delivery commit

`deliver_one()` 在 controller 短锁内：

1. 按稳定 INTID 顺序选择 `route_target == target` 的 deliverable record。
2. 分配 epoch。
3. 构造只读 `DeliveryDescriptor`。
4. 调用 capability-bounded `install()`，它只允许写当前 CPU 的一个已确认空
   LR，不得调用 sender/设备/runtime。
5. install 成功后消费一个 pending latch（若存在）并设置
   `inflight = Some(ResidentInstance { owner: target, epoch })`；level line
   asserted事实不被消费。
6. install 失败则不修改 record pending/active，返回错误。

这避免公开“prepare token → 锁外盲写 → commit”的过期窗口。closure 必须
文档化为 non-sleeping/local-register-only；controller lock 不允许进入任意
外部业务逻辑。

delivery port在进入 `deliver_one()` 前已选择并验证空 slot，descriptor
encode也必须先完成；closure的正常路径只做一次本地 sysreg write。若 fake/
backend仍报告 write失败，closure先 best-effort把该 slot恢复 Invalid，再把
错误返回，controller保持未 commit。禁止在 controller commit成功后才做可能
失败的 descriptor校验或 heap操作。

### 6.2 deliverable

```text
source_pending = pending_latch || (level && line_asserted)

deliverable =
  enabled
  && !active
  && inflight == None
  && deactivation == None
  && source_pending
```

选择顺序固定为 INTID 升序，便于 deterministic test；本模块不实现 priority
调度。虽然本 PR 不实现 ISPENDR/ICPENDR，trigger-independent latch必须由
纯状态测试覆盖，保证以后增加 guest software-pending输入无需改变 record、
deliverable或 LR commit边界。

## 7. edge 转换

- inactive pulse：`pending_latch=true`，返回 target hint。
- 已 pending 再 pulse：pending latch 合并。
- LR Pending 时 pulse：现有 pending 已代表一次，额外 latch可合并。
- LR Active 时 pulse：保留 `pending_latch=true`，返回 target reconcile hint。
- target reconcile 看到匹配 epoch 的 Active LR：
  - 原位写 ActivePending；
  - 成功后消费 latch。
- target reconcile 看到匹配 epoch 的 Pending 或 ActivePending LR且
  `pending_latch=true`：现有 LR 已代表 pending instance，不改 LR并消费
  latch；因此 Pending 阶段的重复 pulse 和 ActivePending 阶段的更多 pulse
  都按 GIC pending bit语义合并。
- deactivate ActivePending 后 LR 变 Pending；fold 保持该 residency，不创建
  第二 LR。

GIC pending latch 允许多个未观察 edge 合并为一个。本 PR 不承诺计数队列。

## 8. level 转换

- raise：`line_asserted=true`；若 deliverable 返回 target hint。
- 重复 raise：幂等；可不重复 kick。
- lower before LR commit：`line_asserted=false`，旧 kick 到达时 no-op。
- lower while LR Pending：返回 owner service hint；target 将 LR Invalid。
- lower while LR Active：active instance 保留，deactivate 后不重投。
- deactivate while asserted：清 active/inflight 后立即再次 deliverable。
- EOIR priority drop 但 LR 仍 Active：不得重投。

软件 level LR 设置 `EOI=1`，用于获得 EOI maintenance 机会；但“是否已经
deactivate”最终由 LR active bit/TDIR command 与 epoch 判定，EISR 不是唯一
真相。

## 9. delivery port 与 slot map

每个 emulated vCPU 创建：

```rust
struct ResidentSlot {
    intid: ArmSpiIntId,
    epoch: DeliveryEpoch,
}

pub(crate) struct ArmVgicDeliveryPort {
    target: VgicVcpuId,
    controller: Arc<ArmVgicController>,
    slots: [Option<ResidentSlot>; 16],
}
```

slot map 随 AxVM vCPU 保存，不放进 `arm_vcpu`。PR 3.2 已保存 raw LR，
因此 unbind 后 `slots[n]` 仍与 saved `lrs[n]` 对应。

进入 guest 前统一执行：

```text
fold → reconcile resident slots → refill empty slots → update HCR policy
```

每次 guest exit 在离开 bound scope 前执行至少 fold；maintenance 和 TDIR
exit 可请求立即再 reconcile/refill。

### 9.1 fold observation

adapter 先在本地读取每个 mapped LR 和 EISR，形成：

```rust
enum ResidentLrState {
    Invalid,
    Pending,
    Active,
    ActivePending,
}

struct ResidentObservation {
    target: VgicVcpuId,
    intid: ArmSpiIntId,
    epoch: DeliveryEpoch,
    state: ResidentLrState,
    eoi_maintenance: bool,
}
```

`ResidentLrState` 是 `arm_vgic` 的领域类型；AxVM adapter从
`arm_vcpu::IchLrEntry` checked转换，不能让 `arm_vgic` 反向依赖
`arm_vcpu`。controller 在锁内验证：

- `inflight == Some({ owner: observation.target, epoch: observation.epoch })`，
  不用当前 `route_target` 猜 resident owner；
- LR 非 Invalid 时其 VINTID 与 slot INTID匹配；Invalid LR 的 VINTID字段
  没有身份意义，只按 slot map 的 typed INTID/epoch fold；
- state 合法。

然后更新 durable `active/inflight/pending_latch`。unknown、duplicate、
owner mismatch 返回 typed error；不能仅按 LR 的裸 INTID 猜 owner。

### 9.2 resident reconcile

按最新状态处理：

- level Pending 且 line 已 lower：LR 写 Invalid，成功后清 inflight/slot。
- record disabled 且 LR Pending：LR 写 Invalid，成功后清 inflight/slot；
  Active/ActivePending instance不因 disable 被撤销。
- edge Active 且 latch=true：LR 写 ActivePending，成功后消费 latch。
- edge Pending/ActivePending 且 latch=true：不改 LR并消费 latch，和当前
  resident pending instance合并。
- matching deactivation command + LR Active/ActivePending：
  清 active state，重新读取/fold，再消费 command。
- matching command + LR 已非 active：消费 command。
- epoch 不匹配 command：诊断并丢弃旧 command，不修改 LR。
- Active LR 不因容量压力被驱逐。

所有“写 LR + controller commit”遵守与 `deliver_one()` 相同的短锁本地操作
规则；写失败不得先清 durable state。

### 9.3 refill

1. 只扫描 capability 的共同 `lr_count`。
2. 为每个空 slot 调用 `deliver_one(target, install)`。
3. install 把 descriptor 转为软件 LR：
   - VINTID 完整 u32；
   - Pending；
   - Group1；
   - 默认 priority；
   - HW=0；
   - level 设置 EOI，edge 不设置。
4. 成功后写入 slot map。
5. 直到无空 slot或无 deliverable。
6. 若 target 尚有 outside-LR deliverable 且 LR 满，开启 UIE。
7. 否则关闭 UIE。
8. NPIE/LRENPIE 始终关闭。
9. emulated mode TDIR 始终开启。
10. 从 PR 3.2 的 typed HCR policy 编码寄存器；EOICOUNT、NPIE/LRENPIE 和
    其他未拥有位始终为 0。

controller 应提供只读 `target_summary(target)`，至少区分
`deliverable_outside_lr` 和 `resident_needs_service`，避免 delivery port
遍历/泄露 records。

UIE 不是“任意空槽”watermark：架构在零个或一个 LR仍为 valid时报告
underflow。PR 3.4 因此要求共同 `lr_count >= 2`。maintenance到来后 fold
释放/确认 slots，再 refill；每次普通 entry/exit 也执行 service，所以 UIE
是 LR pressure 的最终进展通知而不是唯一 refill入口。

## 10. EOImode 与 TDIR

### 10.1 EOImode=0

guest EOIR 通常同时 priority drop/deactivate。软件 level 的 EOI
maintenance 使 host 获得 service；fold 观察 LR active bit清除：

- 清 active/inflight/slot；
- 若 line 仍 asserted，再次 pending/refill；
- 若已 lower，进入 Inactive。

### 10.2 EOImode=1

EOIR 只 priority drop，LR 仍 Active，不能重投。后续 guest 写
`ICC_DIR_EL1`：

- PR 3.1 的 TDIR bit使其 trap到现有 `ArmVmExit::SysRegWrite`；
- `sysreg.rs` 使用 `ArmSysRegAddr::new(0x32_3016)` 识别 `ICC_DIR_EL1`，并以
  `virtualization/aarch64_sysreg::SystemRegType::ICC_DIR_EL1` 的现有值作
  回归对照，禁止从 magic CRn/CRm 算术猜测；
- value checked 转换为 `ArmVirtualIntId`；
- module-owned SPI 调用 `request_deactivation()`；
- 若 owner 是当前 vCPU，立即 reconcile；
- 若 owner 是另一 vCPU，返回该 owner 的 service hint，由 PR 3.4 的 runtime
  adapter 在 controller lock 外调用 sender。

同 epoch 的重复 DIR 幂等；没有 inflight/active instance 时作为 stale
completion 诊断后成功 no-op。

### 10.3 SGI/PPI compatibility

TDIR 是 vCPU interface 全局策略，会捕获非 module-owned INTID。对：

- SGI/PPI；
- virtual timer；
- 既有 direct-inject private interrupt；

必须调用一个 AArch64 crate-private compatibility helper，按现有 direct LR
语义定位当前 vCPU 的匹配 LR并完成 deactivate。不得交给 SPI controller，
不得静默吞掉。未知/非法 INTID 返回 typed unsupported。

## 11. maintenance PPI capability

### 11.1 发现

GICv3 FDT binding 的 GIC node `interrupts` 属性提供 virtualization
maintenance interrupt。`somehal` 的 `probe_gic()` 当前忽略该属性；本 PR：

1. 在 `ProbeFdt::into_parts()` 后、注册 GIC device 前，从
   `FdtInfo::interrupts()` 复制唯一 maintenance specifier；不能把带 FDT
   lifetime 的引用保存到全局。
2. 按现有 `probe_gic()` 顺序初始化 GIC、分配 dynamic IRQ domain，并以
   `dev.register(rdif_intc::Intc::new(domain, gic))` 注册。
3. 通过该 GIC device/domain 对应的 `Intc::translate_fdt()`/`configure()`
   得到 typed `irq_framework::IrqId`；不得调用可能选中“任意一个 Intc”的
   `get_one()` helper。
4. 验证结果 `IrqId.domain == domain` 且 hwirq 是 private/per-CPU IRQ，不用
   `+32` 等裸算术。
5. 以 write-once capability 发布；只有 translate/configure/校验都成功才
   发布 `Available(IrqId)`。
6. 缺失/非法时 GIC 基础功能仍可启动，但 emulated GICv3 capability 查询
   返回 unavailable；PR 3.4 prepare 失败。

实现时在 `dev.register()` 前保存 `device_id`；注册后用
`rdrive::get::<rdif_intc::Intc>(device_id)` 重新取得确切实例并完成翻译。
post-registration 的 missing/malformed/translate/configure错误写入
`MaintenancePpiState::Unavailable` 或 `Error(context)` 后仍让 `probe_gic()`
返回 `Ok(())`，不能因可选 virtualization capability失败而撤销已经可用的
host GIC。write-once状态至少区分
`Uninitialized/Available/Unavailable/Error`，重复发布不同结果是
BadState。

`ax-plat` 增加最小查询，例如：

```rust
fn gic_maintenance_irq() -> Result<IrqId, IrqError>;
```

`axplat-dyn` 只转发 typed ID。不要暴露 FDT cells。

### 11.2 注册和生命周期

maintenance PPI handler 是 host-lifetime/per-CPU facility，不按 VM 重复
注册。`ax_plat::irq::request_percpu_irq(irq, enabled_cpu_mask, handler)` 是
现有最小注册 API；host-lifetime owner 保存返回的 `IrqHandle`。AArch64
`ArchOps::register_platform_irq_injector()` 在 host 等待各 pCPU 完成
virtualization enable 后执行该注册；VM prepare只查询
`Registered/Unavailable/Error` 状态，不触发第二次注册。handler：

- 由平台 common IRQ transaction完成 acknowledge/dispatch/EOI；
- 只检查当前是否有 published/bound emulated vCPU，并在 per-CPU 状态中记录
  typed maintenance observation；observation携带当前
  `(vm_id, vcpu_id, bind_generation)` owner key，不是一个可泄漏给下一 vCPU
  的裸 bool；
- 无当前 emulated vCPU 理论上意味着本地 ICH.EN已关闭；若仍收到 PPI，
  记录 orphan诊断并返回 Handled，避免 framework把已经完成 transaction 的
  专用 PPI再次报告为未处理；
- 不直接进入 delivery port/controller，不在 IRQ action 中读写 LR；
- 不做第二次 host IRQ fetch/EOI。

如果 IRQ framework 要求每个 CPU enable，boot/secondary CPU init 对 typed
PPI 分别 enable；全局 ownership 与 per-CPU action 必须分开。

`ArmVcpu::run()` 随后返回 `ExternalInterrupt`，此时 vCPU 仍处于 PR 3.2 的
bound/pinned scope。AArch64 exit/unbind 路径消费 observation，在清 ICH
之前读取 MISR/EISR/LR 并调用 delivery port fold/reconcile；下一次 guest
entry 前 refill。deferred `after_external_interrupt()` 时 vCPU 已 unbind，
禁止再访问 ICH。

每次成功 bind分配不复用的 `bind_generation`并随 current-vCPU publication
发布。消费 observation时必须与当前 owner key完全匹配；mismatch作为
BadState诊断并清除旧 observation，不能把前一 vCPU 的 maintenance原因应用
到后一 vCPU。多次相同 generation的 observation幂等合并为
`maintenance_seen=true`。

per-pCPU publication使用一个 CPU-local plain state，由本 CPU IRQ exclusion
而不是可自锁的 spinlock保护。bind/unbind侧用短
`NoPreemptIrqSave`/等价 current-CPU guard，handler已在本地 IRQ context：

```text
bind:
  generation = checked_global_next()
  local IRQ-off: publish { vm_id, vcpu_id, generation, reasons=0 }
  restore ICH, HCR.EN last

handler:
  read current CPU publication
  if generation != 0, set maintenance_seen in the same state

unbind:
  local IRQ-off:
    consume matching observation and fold LR
    clear HCR.EN, save/clear ICH
    withdraw publication only after hardware is quiet
```

该 IRQ-off区间只包含 CPU-local ICH snapshot/save、短 controller fold和
publication更新，不能发送 kick或调用设备。它保证 handler不可能看到撕裂
owner，也不会在 fold与 HCR clear之间追加新 observation。bind/restore失败
必须在 IRQ-off下撤销 publication；generation由 checked `AtomicU64`
全局分配，计数溢出返回 BadState，不回绕复用。

实现该平台 capability 时，按项目规范同步更新
`.claude/skills/arch-platform-porting/SKILL.md` 或其 reference，记录 GIC node
maintenance PPI 的发现和调试证据。

## 12. host IRQ 单事务

当前 AArch64 `ArmHostOps::fetch_pending_host_irq()` 已调用平台 handle，平台
`ActiveIrq` drop 完成 EOI/DIR。`ArmVmExit::ExternalInterrupt` 之后的
`after_external_interrupt()` 不得再次 dispatch 同一个 raw vector。

本 PR 在 AArch64 覆盖该 hook：

- 保留 `check_timer_events()`；
- 删除/跳过第二次 `dispatch_host_irq(vector)`；
- 其他架构默认 hook 不变。

测试用计数 fake 验证普通 IRQ、maintenance PPI 各只有一次
ack/dispatch/EOI。

## 13. 锁与错误

controller 使用 IRQ-safe/non-sleeping lock。固定顺序：

```text
读取当前 CPU ICH 小快照
  → controller lock
  → 本地 LR 原子更新/commit
  → release
  → 返回 ServiceHint
```

本 PR 底层不调用 sender。至少区分：

- invalid/unbound SPI；
- duplicate route；
- epoch exhausted；
- no deliverable；
- slot full；
- malformed/unknown LR；
- owner/epoch mismatch；
- unsupported TDS/ICH profile；
- maintenance PPI unavailable；
- unsupported DIR INTID。

正常 stale kick/DIR 是可诊断 no-op，不是 panic。

## 14. 测试

### 14.1 controller 状态机

- INTID 32、255、256、512、1019；拒绝 31、1020。
- duplicate route/非法 target。
- edge pulse、重复 pulse、active 期间 pulse、ActivePending。
- level raise/lower 的全部时序。
- disabled raise 后 enable。
- level priority drop 未 deactivate 不重投。
- asserted level deactivate 后重投。
- lower 后 deactivate 不重投。
- stale epoch/DIR 不影响新 instance。
- ISENABLER 绑定高 INTID读写同一 record，未绑定 RAZ/WI。

### 14.2 fake ICH adapter

- capacity 1 的 profile decode和 emulated policy拒绝；capacity 2/16 的
  fold/refill。
- 17 个 pending，16 个 LR，剩余 pending retained + UIE。
- LR 空出后继续 refill。
- Active LR 永不被驱逐。
- INTID 256/1019 encode。
- level EOI bit、edge EOI=false。
- EISR + LR state fold。
- EOImode0。
- EOImode1 local DIR。
- cross-vCPU DIR 只返回 owner hint。
- SGI/PPI DIR compatibility。
- UIE 开关；NPIE/LRENPIE 永远关闭。
- save/restore 后 slot map与 raw LR 一致。

### 14.3 platform/host

- GIC FDT maintenance specifier解析成 typed PPI。
- missing/malformed capability。
- boot/secondary CPU enable。
- handler 无 current vCPU 安全。
- maintenance 和普通 IRQ 单 transaction。
- AArch64 after-external仍执行 timer check。

建议验证：

```bash
cargo xtask clippy --package arm_vgic
cargo xtask clippy --package arm_vcpu
cargo xtask clippy --package axvm
cargo xtask clippy --package somehal
cargo test -p arm_vgic
cargo test -p arm_vcpu
cargo test -p axvm --features host-test
cargo fmt --all -- --check
```

## 15. 完成条件与交接

- controller 的纯状态机覆盖 edge/level/enable/active/epoch。
- delivery port 在 target vCPU 本地完成 fold/reconcile/refill。
- LR full 时软件 pending 保留；UIE 在 valid LR降到零个或一个时驱动后续
  refill，普通 entry/exit service同时兜底。
- TDIR 正确区分 module SPI 和 private interrupt。
- EOImode0/1 均有确定性测试。
- maintenance PPI 以 typed platform capability 提供，host IRQ 只处理一次。
- production 设备尚未接入新的 SPI producer path。

PR 3.4 可以只把配置 route、device `IrqLine`、controller、sender 和每个
delivery port 组合起来；不得在接线 PR 中重新定义本 PR 的状态转换。
