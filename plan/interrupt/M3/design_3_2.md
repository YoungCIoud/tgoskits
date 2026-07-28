# PR 3.2 设计：per-vCPU ICH context 与 CPU pin 生命周期

## 1. PR 定位

- 建议标题：`feat(arm-vcpu): preserve per-vCPU ICH execution state`
- 依赖：PR 3.1。
- 后继：PR 3.3 在本 PR 的 context 和 local access 边界内实现 fold/refill。
- 预计变更：生产代码 260–460 行，测试 220–380 行。
- 难度：高。主要风险是 save/restore 顺序、异常清理和 bind/run/unbind
  是否始终位于同一物理 CPU。

本 PR 解决 ICH execution state 的所有权和运行生命周期，不建立 SPI
pending/active 状态，不启用 maintenance/TDIR，不接 Router。

## 2. 不变量

1. `ICH_HCR_EL2`、`ICH_VMCR_EL2`、APR 和 LR 属于 vCPU。
2. ICH sysreg 只能由当前 pCPU 上已 bind 的 vCPU 访问。
3. restore、guest run、save 必须在同一个 CPU pin/current-vCPU publication
   scope 内。
4. unbind 后本地硬件不能残留前一 vCPU 的有效 LR 或 enabled interface。
5. 运行 emulated VGIC 的所有候选 pCPU 必须提供兼容 ICH capability。
6. passthrough/no-irq 路径仍可创建 context，但不得因此启用 emulated VGIC
   policy。

## 3. 当前缺口

- `ArmVcpu::bind()`/`unbind()` 当前是 no-op。
- ICH state 不在 `ArmVcpu` 中。
- `AxVCpu::run_vcpu()` 在 `with_current_cpu_set()` 之前 bind，在离开 pin
  scope 后 unbind，存在迁移窗口。
- `ArmPerCpu::hardware_enable()` 没有读取/发布 GIC virtualization interface
  capability。

## 4. 类型设计

### 4.1 capability profile

在 `arm_vcpu` 中定义：

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IchCapabilityProfile {
    lr_count: u8,
    id_bits: u8,
    priority_bits: u8,
    preemption_bits: u8,
    apr_count: u8,
    tds: bool,
}
```

公开只读 accessor，不公开可随意构造的字段。当前 CPU 通过
`IchCapabilityProfile::read_current()` 从 `ICH_VTR_EL2` 构造，并验证：

- `lr_count` 为 1–16；
- IDBITS raw 0/1分别解码为架构定义的 16/24-bit INTID能力，二者均足以
  表示 1019；不得把 raw字段简单解释为 `raw + 1`；
- PRIbits/PREbits按架构编码换算为实际 bit数量，再推导需要保存的 APR数量；
- APR 数量不超过 context 固定容量；
- 保留字段组合返回 typed `UnsupportedIchProfile`。

本 PR 记录 `tds`，但不因 `tds=false` 拒绝所有 AArch64 VM。PR 3.4 仅在
emulated GICv3 prepare 时要求 TDS。类似地，本 PR 接受架构合法的
`lr_count=1`，但 PR 3.4 的 emulated SPI 模式要求共同 `lr_count >= 2`：
UIE 在“零个或一个有效 LR”时断言，单 LR 实现无法把它作为无风暴的
outside-LR refill通知；本模块又明确不使用 NPIE，因此不能假装支持。

### 4.2 per-CPU capability registry

`ArmPerCpu::hardware_enable()` 成功建立 EL2 host hook 后读取 profile，并以
Release 发布到按逻辑 pCPU ID 索引的 registry。重复 enable：

- 相同 pCPU、相同 profile 幂等；
- 相同 pCPU、不同 profile 返回 BadState；
- 超出 registry 上限返回 typed error。

registry不引入 heap/全局锁。与 AxVM 现有 enabled CPU bitmap边界一致，使用
`MAX_TRACKED_CPUS = usize::BITS as usize` 个 `AtomicU64` slot；把完整
`IchCapabilityProfile` checked-pack到非零值，0表示 unpublished。发布用
compare-exchange Release/AcqRel，读取用 Acquire。packing/unpacking做纯测试，
不能用 relaxed且不能分别发布多个字段让 reader看到撕裂 profile。

profile读取、校验和 registry发布属于 `hardware_enable()` 成功事务的一部分。
若读取/校验/发布失败，必须撤销本次 current-EL hook user、恢复原
`VBAR_EL2`/`HCR_EL2`，并保持 slot unpublished；不得返回错误却留下半启用
virtualization环境。已发布 profile在 hardware disable后可作为不可变硬件
描述保留，但 VM placement仍只从 AxVM enabled CPU集合选择。

发布前先把本地 `ICH_HCR_EL2.EN` 清零，并将 profile范围内所有 LR写为
Invalid，消除固件/前一 host状态；首次 vCPU bind会覆盖 VMCR/APR和全部有效
LR后才最后启用 HCR。`hardware_disable()` 同样先清 ICH EN和 LR，再撤销
current-EL hook与 EL2 host状态。

提供：

```rust
pub fn ich_capability(cpu_id: usize) -> ArmVcpuResult<IchCapabilityProfile>;
pub fn common_ich_capability(cpu_ids: &[usize]) -> ArmVcpuResult<IchCapabilityProfile>;
```

`common_ich_capability()`：

- 要求每个 CPU 已发布；
- 取共同 `lr_count`/APR 下限；
- 校验 ID bits、priority/preemption/APR layout 兼容；
- `tds` 取所有 CPU 的逻辑与；该函数如实返回 false，PR 3.4 仅在 emulated
  GICv3 prepare 时将 false 作为 unsupported；
- 对不兼容返回包含 CPU ID 和 profile 的 typed error；
- 不静默选择 boot CPU profile。

该 API 是架构 capability 查询，不依赖 AxVM placement 类型。

### 4.3 context

```rust
const ICH_MAX_LRS: usize = 16;
const ICH_MAX_APRS: usize = 4;

#[derive(Debug)]
struct IchVcpuContext {
    hcr: IchHcrPolicy,
    vmcr: u64,
    ap0r: [u64; ICH_MAX_APRS],
    ap1r: [u64; ICH_MAX_APRS],
    lrs: [u64; ICH_MAX_LRS],
    lr_count: u8,
    apr_count: u8,
    initialized: bool,
    bound_cpu: Option<usize>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct IchHcrPolicy {
    enabled: bool,
    underflow_maintenance: bool,
    trap_dir: bool,
}
```

`IchVcpuContext` 由 `ArmVcpu` 私有持有，不加入 assembly 依赖的前两个字段
之前。若加入字段改变 layout，现有 compile-time offset assertions 必须继续
通过。

默认 policy：

- `enabled` 保留现有 direct inject 兼容所需状态；
- `underflow_maintenance` 和 `trap_dir` 为 0；
- NPIE/LRENPIE、group enable/disable maintenance、trap-all 和其他未由本模块
  拥有的 HCR 位始终写 0；
- 所有 LR Invalid；
- VMCR/APR 使用显式默认值，不从创建 vCPU 时恰好所在 pCPU 抄取。

`ICH_HCR_EL2` 不能作为 opaque `u64` 原样 round trip。`EN`、`UIE`、`TDIR`
是本模块拥有的配置 policy；`EOICOUNT` 是 active-outside-LR 协议的一部分，
不是可继承到下一次 bind 的配置。当前模块禁止 active-outside-LR，因此 entry
写 HCR 时显式令 EOICOUNT=0，save 时若观察到非零值则先完成安全 cleanup，
再返回带 CPU/vCPU 信息的 `BadState`。以后支持 active-outside-LR 时，应扩展
typed context 和 save/restore 协议，而不是开始保存未知 HCR 位。

首次 bind 时将 context 的 capacity 固定为当前 CPU profile。后续 bind 到
其他 CPU 时先验证该 CPU profile 兼容；不能为了迁移扩大已保存的有效范围。

### 4.4 local register access

内部提供只允许 bind scope 使用的 guard：

```rust
pub(crate) struct BoundIch<'a> {
    context: &'a mut IchVcpuContext,
    cpu_id: usize,
}
```

guard 提供 PR 3.3 所需的最小本地操作：

```rust
fn capability(&self) -> IchCapabilityProfile;
fn read_lr(&self, slot: usize) -> ArmVcpuResult<IchLrEntry>;
fn write_lr(&mut self, slot: usize, entry: IchLrEntry) -> ArmVcpuResult;
fn invalidate_lr(&mut self, slot: usize) -> ArmVcpuResult;
fn empty_lr_mask(&self) -> u16;
fn update_hcr(&mut self, policy: IchHcrPolicy) -> ArmVcpuResult;
```

`BoundIch` 不能跨 `ArmVcpu::run()` 长期借用，因为它可变借用了 backend
内部 context。`ArmVcpu` 提供 crate-private 的短时 closure 入口，例如：

```rust
pub(crate) fn with_bound_ich<T>(
    &mut self,
    operation: impl FnOnce(&mut BoundIch<'_>) -> ArmVcpuResult<T>,
) -> ArmVcpuResult<T>;
```

该入口只在 `bound_cpu == current_cpu` 时创建 guard。entry service、exit fold
和 unbind pre-save 各自取得一次短借用；guard 在 guest run 前释放，不存进
`AxvmArmVcpu` 或 delivery port。这样既保持本地访问能力边界，也不会让
delivery adapter 与 backend 的 `run(&mut self)` 形成不可实现的重叠借用。

本 PR 实现并测试上面列出的 guard方法。PR 3.3 在同一 crate-private guard上
增加其首个真实消费者所需的 `read_misr()`/`read_eisr()`；不得为测试把 raw
sysreg backend变成公共硬件抽象。真实访问和 host test访问通过
crate-private sealed adapter或纯 snapshot helper分离。

## 5. save/restore 协议

### 5.1 bind/restore

`ArmVcpu::bind()`：

1. 获取当前逻辑 pCPU ID。
2. 查询已发布 capability；未发布返回 BadState。
3. 检查 `bound_cpu.is_none()`，防止双重 bind。
4. 初始化或验证 context capacity。
5. 先确保本地 `ICH_HCR_EL2.EN=0`，避免写入中间状态时产生 maintenance。
6. 恢复 VMCR。
7. 恢复有效 APR。
8. 恢复 0..lr_count 的 LR；其余本地可见 LR 写 Invalid。
9. 从 typed policy 编码 HCR，显式将未拥有位和 EOICOUNT 写 0，并最后写
   `EN`。
10. 设置 `bound_cpu=Some(current_cpu)`。

restore 失败必须把本地 HCR.EN 清零并使所有可访问 LR Invalid，再返回错误。

### 5.2 unbind/save

`ArmVcpu::unbind()`：

1. 验证当前 CPU 等于 `bound_cpu`。
2. 先读取 HCR并记下 owned-policy/EOICOUNT 校验结果；无论校验是否成功都
   立即清本地 EN，不能从校验错误路径带着 enabled interface 返回。
3. 保存所有有效 LR。
4. 保存 APR、VMCR 和 typed HCR policy；不保存 reserved/unowned 位或
   EOICOUNT。
5. 清空本地 LR。
6. 保持本地 HCR.EN=0。
7. 清 `bound_cpu`。
8. 硬件和 publication 均已安全清理后，才把第 2 步记下的校验错误返回。

即使 guest run 返回错误，也必须执行 unbind。若 run error 和 unbind error
同时发生，返回可定位的主错误并记录 cleanup error；不得直接 `?` 跳过清理。

### 5.3 context 生命周期

- vCPU reset：恢复默认 VMCR/APR/HCR，所有 LR Invalid，清 owner。
- VM stop 后保留 vCPU object 时不得保留 live pCPU owner。
- drop 只允许 context 未绑定；debug assertion 可检查，运行时可恢复路径不能
  依赖 panic。

## 6. CPU pin/run-loop 重构

当前通用 `virtualization/axvm/src/vcpu.rs` 必须把：

```text
bind
with_current_cpu_set(run loop)
unbind
```

重构为单一 pin scope：

```text
with_current_cpu_set {
    bind_published
    run_published loop
    unbind_published
}
```

具体要求：

- 在 `AxVCpu` 增加 crate-private、只供已有 publication scope 调用的
  `bind_published()`、`run_published()`、`unbind_published()`；它们执行原
  state transition/backend call，但不再次进入
  `with_current_cpu_set()`；
- `ArchOps::run_vcpu()` 只建立一次 outer publication/pin，并在其中调用这些
  helper；不能把现有会各自 publication 的 `bind()/run()/unbind()` 简单嵌套
  进去；
- public/其他 caller需要的 standalone `bind()/run()/unbind()` 可继续用
  `with_current_cpu_set()` 包装同一内部 helper，避免复制状态机；
- `with_current_cpu_set()` publication 和 CPU pin 覆盖 bind 到 unbind 的完整
  生命周期；
- 进入 scope 后记录 CPU ID，bind 前、每次 guest entry 前和 unbind 前均可用
  debug assertion 验证未迁移；
- 所有 loop break/error 路径汇入一个 cleanup；
- bind 失败时不调用 unbind；
- bind 成功后，无论 `run()`、exit handler、shutdown 哪一步失败都调用
  unbind；
- 不持有 runtime/dispatcher/controller lock 跨 guest entry；
- 其他架构 bind/unbind 语义不变。

不新增通用 `run_bound` 抽象；直接在 `ArchOps::run_vcpu()` 的单一 outer
publication scope中保存 run result，并汇入一次显式
`unbind_published()` cleanup。host test通过现有 fake backend覆盖每个退出
分支。

## 7. 并发与内存顺序

- capability registry 是 write-once per CPU；发布使用 Release，读取使用
  Acquire。
- `IchVcpuContext` 不使用内部锁，由当前 vCPU execution owner 独占。
- `bound_cpu` 是普通字段，不用 atomic；并发运行同一 vCPU 本就是上层非法
  状态。
- current-vCPU publication 必须先于 restore 完成，并保持到本地 ICH 清理
  完成后。
- 不在 ICH enabled 状态下允许任务迁移或 preemption 到另一 pCPU。若
  `with_current_cpu_set()` 不能提供此保证，PR 必须先修正该 primitive 的 pin
  语义，而不是仅加日志。

## 8. 与后续 PR 的边界

本 PR 不知道：

- 哪些 LR 是 module-owned SPI；
- delivery epoch 或 slot map；
- edge/level；
- fixed target；
- sender/runtime。

因此 context 只保存 raw architectural execution state。PR 3.3 的
`ArmVgicDeliveryPort` 在 bind 后通过 `with_bound_ich()` 短时借用
`BoundIch`，并独立保存 LR slot 到 controller epoch 的软件映射。slot map
与 context 同属 vCPU，但不放进 `arm_vcpu`，避免架构 core 依赖
`arm_vgic`。

本 PR 不能启用 TDIR：在 PR 3.3 尚未处理 `ICC_DIR_EL1` trap 前启用会吞掉
guest deactivation。

## 9. 实施顺序

1. 为 run-loop error/cleanup 和 CPU pin 添加 failing host tests。
2. 添加 capability profile 的 raw VTR decode tests。
3. 添加 context snapshot round-trip tests。
4. 在 `ArmPerCpu::hardware_enable()` 发布 profile。
5. 把 context 加入 `ArmVcpu`，实现 bind/restore 和 unbind/save。
6. 重构 AxVM 通用 run-loop pin scope。
7. 添加 reset/cleanup 和多 profile compatibility tests。
8. 格式化、clippy、targeted tests。

## 10. 测试矩阵

### 10.1 capability

- LISTREGS 编码 0/15 分别得到 1/16。
- ID bits 足以覆盖 1019。
- TDS 被如实记录，不在本 PR 全局拒绝。
- 单 LR profile 可被读取，但留给 emulated mode 的上层 policy拒绝。
- 未发布 CPU 查询失败。
- 两 CPU 相同 profile 成功。
- LR 数不同取共同下限。
- ID/APR/layout 不兼容失败并指出 CPU。

### 10.2 context

- HCR/VMCR/AP0R/AP1R/LR round trip。
- HCR 只 round trip owned policy；构造 EOICOUNT 非零或 unowned bit 污染时
  返回可诊断错误，下一 vCPU 不继承该值。
- 只访问 profile 声明的 LR/APR。
- restore 时 HCR 最后启用。
- save 时 HCR 首先关闭。
- unbind 清空本地 LR。
- vCPU A → vCPU B → vCPU A 不泄漏 LR。
- bind CPU0、unbind CPU1 被拒绝。
- restore 中途失败执行本地清理。
- reset 清所有 transient ICH state。

### 10.3 run-loop

- 正常退出：bind/run/unbind 顺序。
- guest run error：仍 unbind。
- exit handler error：仍 unbind。
- bind error：不 unbind。
- pin scope 覆盖 bind 和 unbind。
- current-vCPU publication 在 unbind 完成后才撤销。

### 10.4 回归

- x86_64、RISC-V、LoongArch run-loop host tests。
- AArch64 direct inject 仍在 bound vCPU 上工作。
- passthrough/no-irq 不启用 TDIR/UIE。

建议至少执行：

```bash
cargo xtask clippy --package arm_vcpu
cargo xtask clippy --package axvm
cargo test -p arm_vcpu
cargo test -p axvm --features host-test
cargo fmt --all -- --check
```

## 11. 完成条件

- ICH execution state 有唯一 vCPU owner。
- bind/guest run/unbind 位于同一 CPU pin scope。
- 所有成功 bind 路径都执行 unbind cleanup。
- 不同 vCPU/VM 之间无 LR/APR/VMCR/HCR 泄漏。
- capability 按 pCPU 发布，可计算 placement 共同 profile。
- TDIR、maintenance、controller、Router 均未启用。

PR 3.3 可以假定：目标 vCPU entry/exit 时已在正确 pCPU 上拥有可安全读写的
`BoundIch`，且 context 会随 vCPU 保存恢复。
