# PR 3.1 设计：无损 INTID 与 ICH 寄存器边界

## 1. PR 定位

- 建议标题：`fix(arm-vcpu): preserve full virtual interrupt IDs`
- 依赖：模块 1、模块 2 已合入；不依赖后续模块 3 PR。
- 后继：PR 3.2 依赖本 PR 提供的 ICH capability/寄存器基础；PR 3.3
  依赖本 PR 的 LR 编解码和可恢复错误。
- 预计变更：生产代码 120–240 行，测试 120–220 行。
- 难度：中高。风险集中在架构字段编码和保持现有 SGI/PPI/direct inject
  兼容，而不是代码量。

本 PR 只修复底层表示能力和失败语义，不引入 VGIC SPI controller，不切换
设备 producer，也不启用 TDIR、UIE 或 maintenance。

## 2. 要解决的问题

当前路径存在三个独立缺陷：

1. `ArmVcpu::inject_interrupt(usize)` 在调用
   `ArmHostOps::inject_virtual_interrupt(u8)` 时截断 INTID，256–1019 会静默
   变成 0–255。
2. `virtualization/axvm/src/arch/aarch64/gic.rs` 在 LR 满时 panic。
3. `drivers/intc/arm-gic-driver/src/sys_reg/ich.rs` 已定义 HCR 的
   UIE/LRENPIE/NPIE/TDIR、MISR/EISR 和 VTR.TDS，但 LR 缺少软件中断 EOI
   overlay，VTR.IDBITS 当前还以 4 bit 定义并与 PREBITS 重叠。必须修正字段
   并为后续提供 checked access。

此外，`arm_vgic::Vgicd` 使用 512 项数组，`ISENABLER` 和 `fetch_irq()` 对
寄存器推导出的下标直接索引。这个 PR 不建立最终 sparse controller，但必须
先消除越界和 panic；合法但尚未绑定的高 INTID 应得到确定结果，而不是访问
数组。

## 3. 范围

### 3.1 必须完成

- 端到端保留 32 位 virtual INTID，不再经过 `u8`。
- 提供验证后的 AArch64 virtual INTID 类型，编码 LR 前拒绝超范围值。
- 把“没有空 LR”改为可恢复错误。
- 将现有 direct inject 路径迁移到 typed LR 编码 helper。
- 修正 `arm-gic-driver` 的 LR EOI/VTR.IDBITS 字段，并为 PR 3.2/3.3 增加
  checked access 和纯编解码 helper。
- 修复 legacy `Vgicd` 的所有外部下标访问，使 512–1019 和非法地址都不会
  越界。
- 更新 `arm_vcpu` README、host API test 和所有 host 实现。

### 3.2 明确不做

- 不创建 `ArmVgicController`、固定 route 或 durable pending/active 状态。
- 不保存恢复 ICH context。
- 不处理 guest EOI/DIR。
- 不注册 maintenance PPI。
- 不改变 dispatcher 的事件语义。
- 不把 LR full 事件重新排队；本 PR 只返回错误。PR 3.3 才以软件 pending
  真相消除 LR full 丢失。
- 不把 legacy 512 项 `Vgicd` 扩成 1020 项作为最终方案。

## 4. 类型和 API

### 4.1 virtual INTID

在 `virtualization/arm_vcpu/src/interrupt.rs` 新建：

```rust
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ArmVirtualIntId(u32);

impl ArmVirtualIntId {
    pub const MAX: u32 = 1_019;

    pub const fn new(value: u32) -> ArmVcpuResult<Self>;
    pub const fn as_u32(self) -> u32;
}
```

该类型表示可编码到本模块所支持 GIC traditional INTID 字段的值
`0..=1019`，不是 SPI 专用类型。原因是现有 direct inject 还服务 SGI/PPI。
SPI 的 `32..=1019` 验证由 PR 3.3 的 `ArmSpiIntId` 完成。

禁止提供 `From<usize>` 或会截断的 `as u8` 转换。可提供：

```rust
impl TryFrom<usize> for ArmVirtualIntId;
impl From<ArmVirtualIntId> for u32;
```

越界错误加入 `ArmVcpuError` 的可匹配 variant，例如：

```rust
InvalidVirtualInterruptId { value: usize }
```

当前 `ArmVcpuError` 只有 `InvalidInput/Unsupported/BadState`。本 PR 保留这些
variant 兼容既有 caller，并为 INTID/LR 增加带上下文的 variant。按项目错误
规范在 `arm_vcpu/Cargo.toml` 增加 `thiserror.workspace = true`，让
`ArmVcpuError` derive `thiserror::Error` 并为每个 variant提供
`#[error(...)]`；该依赖支持 no_std，不在实现 PR 中重新讨论手写
`Display`。AxVM `arm_error_to_backend()` 必须穷尽映射新增 variant，不能用
wildcard把 LR full误报成 InvalidInput。

### 4.2 host 边界

修改：

```rust
pub trait ArmHostOps {
    fn inject_virtual_interrupt(intid: ArmVirtualIntId) -> ArmVcpuResult;
    // ...
}
```

`ArmVcpu::inject_interrupt()` 保留架构无关 caller 所需的 `usize` 参数，但先
执行 `ArmVirtualIntId::try_from(vector)?`，再调用 host。以下实现必须同步：

- `virtualization/axvm/src/arch/aarch64/mod.rs`
- `virtualization/arm_vcpu/src/vcpu.rs` 中 assembly layout host
- `virtualization/arm_vcpu/tests/os_neutral_api_test.rs`
- `virtualization/arm_vcpu/README.md`
- `virtualization/arm_vcpu/README_CN.md`

`arm_vgic` virtual timer 的现有 `hardware_inject_virtual_interrupt(30)` 可暂时
保持自己的 private-interrupt API；其 AxVM host adapter 转换到
`ArmVirtualIntId` 时必须使用 checked constructor。

### 4.3 LR 描述符

在 `arm_vcpu` 的 AArch64 ICH 模块定义 plain data：

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IchLrState {
    Pending,
    Active,
    ActivePending,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IchLrEntry {
    Invalid,
    Software {
        intid: ArmVirtualIntId,
        state: IchLrState,
        priority: u8,
        group1: bool,
        eoi: bool,
    },
}
```

`IchLrEntry::encode()` 和 `decode()` 必须是可在 host test 上运行的纯函数。
decode 必须先判断 STATE：Invalid LR 直接得到 `IchLrEntry::Invalid`，不能
读取或校验其无身份意义的 VINTID/HW/PINTID残值；其他三种 state才要求
`HW=0`、VINTID有效并构造 `Software`。若有效 LR 的 `HW=1`、state保留或
VINTID超范围，返回 typed unsupported/malformed错误，不伪装成软件 LR。
encode `Invalid` 生成确定的全零值。

不要让 `arm_vgic` 依赖寄存器 crate。后续 AxVM delivery adapter 负责把
`arm_vgic` 的 delivery descriptor 转成 `IchLrEntry`。

### 4.4 LR 操作错误

直接注入函数改为：

```rust
pub(crate) fn inject_interrupt(intid: ArmVirtualIntId) -> ArmVcpuResult;
```

至少区分：

```rust
NoFreeListRegister { intid: ArmVirtualIntId }
MalformedListRegister { slot: usize }
UnsupportedListRegister { slot: usize }
```

LR 满时不得 panic、覆盖 Active LR 或假装成功。现有上层
`VmArchVcpuOps::inject_interrupt()` 按当前 error translation 返回
`BackendResult`。

## 5. ICH 字段清单

字段定义的实际所有者是
`drivers/intc/arm-gic-driver/src/sys_reg/ich.rs`，不是 `aarch64-cpu` 或
`arm_vcpu`。本 PR 在该文件修正字段，再在 `arm_vcpu` 提供 typed
plain-data/access helper。用 compile-time/纯值测试核对：

`arm_vcpu` 的 raw ICH adapter使用 target-specific workspace dependency：

```toml
[target.'cfg(target_arch = "aarch64")'.dependencies]
arm-gic-driver.workspace = true
```

纯 `IchLrEntry`/error 类型保持非 AArch64 host test可编译；实际 sysreg访问用
`cfg(target_arch = "aarch64")` 隔离，不在 `arm_vcpu` 复制汇编寄存器定义。

- `ICH_LR_EL2`
  - VINTID
  - STATE
  - GROUP
  - PRIORITY
  - HW
  - EOI
- `ICH_HCR_EL2`
  - EN
  - UIE
  - LRENPIE
  - NPIE
  - TDIR
- `ICH_MISR_EL2`
  - U
  - EOI
  - LRENP（只用于诊断；模块 3 不启用 LRENPIE）
- `ICH_EISR_EL2`
  - 对应 LR slot 的 EOI status
- `ICH_ELRSR_EL2`
  - 空 LR status
- `ICH_VTR_EL2`
  - LISTREGS
  - IDbits
  - PREbits/PRIbits
  - A3V
  - TDS

当前 wrapper 中除 LR.EOI 外的大部分字段已经存在，不能重复定义。必须：

- 增加 LR 的软件 EOI overlay（与 `HW=0` 语义配套）；
- 将 `ICH_VTR_EL2.IDBITS` 修正为架构规定的 3 bit `[25:23]`，避免与从
  bit 26 开始的 PREBITS 重叠；
- 保留 PINTID/EOI 的条件 overlay，由 `IchLrEntry` 根据 HW 语义选择；
- 把 raw slot helper 的 0–15 panic封装在事先 checked 的 safe access 内。

测试必须根据 Arm GICv3 architecture 的明确 bit position构造 raw value，
不能只做 encode/decode 自洽测试。

本 PR 仅使字段可用：

- `TDIR`、`UIE`、`LRENPIE` 和 `NPIE` 保持关闭；
- direct inject 继续只按需要设置 `EN`；
- EOI bit 默认 false，PR 3.3 的软件 level LR 才设置 true。

## 6. direct inject 行为

迁移后的 direct inject 算法：

1. 读取当前 CPU 的 `ICH_VTR_EL2.LISTREGS + 1`。
2. 拒绝 0、超过架构最大 16 或超过 wrapper 可访问 slot 数的异常 capability。
3. 在有效 slot 范围内检查是否已有相同 INTID 的 Pending/Active/
   ActivePending software LR；若有则按现有兼容语义成功返回。遇到非空
   `HW=1` LR 时只视为已占用并跳过，不能因 direct inject不理解其 payload
   而覆盖它或使 passthrough路径失败。
4. 从 `ICH_ELRSR_EL2` 和 LR state 的交集选择空 slot。
5. 无空 slot 返回 `NoFreeListRegister`。
6. 使用 `IchLrEntry` 写 Pending、Group1、默认 priority、EOI=false。
7. 必要时开启 `ICH_HCR_EL2.EN`。

此路径只用于尚未迁移的 private/direct injection。不能把“同 INTID 已存在”
的合并逻辑复用于 PR 3.3 的 SPI 状态机；edge active 时再次 pulse 需要
Active+Pending，由 controller 决定。

## 7. legacy `Vgicd` 安全修复

`Vgicd` 的最终状态所有权将在 PR 3.3 重构。本 PR 采取最小兼容修复：

- 所有 INTID/index 访问先用 `get()`/`get_mut()` 或 checked range；
- `ISENABLER<n>` read：
  - 数组内已有项返回当前 bit；
  - 合法但 legacy 未建模的项返回 0；
  - 超出 GICD register 范围返回现有 RAZ 行为；
- write：
  - 数组内已有项维持现有 enable；
  - 合法但未建模的项 WI；
  - 不创建隐式 route/record；
- `fetch_irq()` 改为返回 `VgicResult<VgicInt>` 或 `Option<VgicInt>`，不得
  裸索引和 panic；更新唯一 caller。

这不是对 512–1019 的功能支持。PR 3.3 会让 module-owned 高 INTID 通过 sparse
controller 得到真实状态；本 PR 的验收点是“无越界、结果确定”。

## 8. 实施顺序

1. 添加 failing tests：255/256/1019 保留，1020 拒绝，LR full 返回错误，
   legacy ISENABLER 高 index 不 panic。
2. 添加 `ArmVirtualIntId` 和错误 variant。
3. 扩宽 `ArmHostOps`，更新所有实现、README 和 test double。
4. 添加 LR plain-data 编解码 helper 和字段定义。
5. 迁移 AArch64 direct inject，删除 `unwrap_or_else(panic!)`。
6. 为 `Vgicd` 外部索引加 checked access。
7. 格式化、clippy、targeted tests。

## 9. 测试

### 9.1 必须先失败再修复

- `ArmVcpu::inject_interrupt(256)` 的 fake host 观察值必须是 256；旧实现会
  观察到 0。
- LR capacity 已满时返回 `NoFreeListRegister`；旧实现会 panic。
- `GICD_ISENABLER16`/`GICD_ISENABLER31` 的高 INTID bit 访问不得越界；
  旧实现会 panic。

### 9.2 纯逻辑测试

- INTID 0、31、32、255、256、1019 round trip。
- 1020、`usize::MAX` 被拒绝。
- Invalid entry以及三种 software state编解码；Invalid raw中的 VINTID/HW
  残值不参与身份校验。
- EOI、Group、priority、VINTID 字段使用预期 bit。
- INTID 256 编码不等于 0。
- `HW=1` decode 返回 unsupported。

### 9.3 回归

- SGI/PPI direct injection 使用原 INTID。
- virtual timer INTID 30 构建通过。
- AArch64 AxVM adapter error translation 保留错误。
- 非 AArch64 host test 构建通过。

建议验证命令以实际 package feature 为准，至少包括：

```bash
cargo xtask clippy --package arm_vcpu
cargo xtask clippy --package arm_vgic
cargo xtask clippy --package axvm
cargo test -p arm_vcpu
cargo test -p arm_vgic
cargo fmt --all -- --check
```

如果 `xtask clippy` 不支持 host-only feature，先检查 xtask 参数，再使用与 CI
相同 target/features 的 Cargo 命令。

## 10. 完成条件与交接

- 代码中不存在 virtual INTID 到 `u8` 的转换。
- direct inject 对 1019 可编码，对 1020 返回 typed error。
- LR full 不 panic。
- legacy Vgicd 所有外部索引均 checked。
- TDIR/UIE 等字段存在但未启用。
- 未引入 controller、route、maintenance 或 producer 行为。

交给 PR 3.2 的稳定契约是：`ArmVirtualIntId`、ICH capability 字段和 LR
plain-data 编解码已经可用。交给 PR 3.3 的稳定契约是：本地 LR 操作会明确
报告满载/非法状态，不再截断或 panic。
