# PR 3.5 设计：synthetic SPI device 与 AArch64 SMP2 端到端验证

## 0. 同步基线

- 面向 `dev@31f341abca46e2c8920c607c2e63316ea374a290`，依赖 PR 3.1–3.4。
- 当前 dev 的 AxVisor QEMU case 以 `test-suit/axvisor/normal/<wrapper>/` 为
  build wrapper，`normal/qemu/` 已拥有 AArch64 smoke；独立平级 wrapper 的
  选择仍与 discovery 结构一致。
- 当前 `scripts/axbuild` 尚无 `guest_builds` schema；本 PR 的 typed build
  支持是显式新增能力，不能依赖 flatten 后被忽略的未知字段或 shell prebuild。
- 该 PR 是验证闭环，不得用 synthetic device 绕过正常 `DeviceFactory →
  Resource::IrqLine → InterruptFabric` production boundary。

## 1. PR 定位

- 建议标题：`test(axvisor): cover AArch64 emulated SPI delivery`
- 依赖：PR 3.1–3.4。
- 预计变更：production/test-support 180–320 行，guest/runner/config/tests
  650–1,100 行。
- 难度：中高。主要成本是稳定构建 EL1 guest、SMP2 同步和让 LR pressure
  证据真正覆盖 refill。

该 PR 不修正前四个 PR 未完成的语义。如果端到端测试暴露状态机问题，应先在
对应低层补确定性回归和修复，再更新本 PR。

## 2. 交付物

```text
test-suit/axvisor/normal/emulated-gicv3-spi-smp2/
  build-aarch64-unknown-none-softfloat.toml
  qemu-aarch64.toml
  vm.toml（或当前 test runner 约定的 VM config）

test-suit/axvisor/guests/emulated-gicv3-spi-smp2/
  Cargo.toml
  Cargo.lock
  rust-toolchain.toml（若仓库 runner 不统一注入）
  linker.ld
  src/main.rs
  src/start.S
  src/vector.S

virtualization/axdevice/src/synthetic_irq.rs
  feature-gated synthetic SPI device/factory

scripts/axbuild/.../
  typed guest_builds schema、校验、构建和测试
```

该目录是与现有 `normal/qemu/` 平级的独立 build wrapper。当前 discovery在
遇到 `normal/qemu/build-*.toml` 后不会把其内另一个 `build-*.toml` 目录注册
为嵌套 wrapper，因此不能放到 `normal/qemu/emulated-.../`。平级目录可使
case 名固定为 `emulated-gicv3-spi-smp2`，且不污染现有 smoke build group。

## 3. synthetic device

### 3.1 隔离

新增 test-only emulated device type和 factory。shared enum 的 numeric
identity不能随 feature消失，因此采用：

- `axvm-types::EmulatedDeviceType::SyntheticIrq = 0xDF` 无条件保留；
  `Display`、`ALL` 和 `from_usize()` 同步更新，保证配置数值稳定；
- `0xDF` 当前位于明确保留段，只用于仓库测试协议，不复用 `Dummy` 或现有
  production device；
- device/factory实现由 `axdevice/test-synthetic-irq` 保护；
- `axvm/test-synthetic-irq = ["axdevice/test-synthetic-irq"]`，
  `axvisor/test-synthetic-irq = ["axvm/test-synthetic-irq"]` 逐层传播；
- AArch64 test build wrapper只启用顶层 `test-synthetic-irq`；
- 默认 AxVisor product feature不包含；
- 未启用时 numeric config仍能识别其身份，但 factory registry没有实现，
  `build_with_factories()` 返回包含 device name/type 的 Unsupported；不得
  fallback到其他设备；
- 仍通过 `DeviceFactory`、`Device::resources()`、
  `DeviceBuildContext::resolve_irq()` 获取正常 `IrqLine`；
- 不持有 controller、VM、sender、vCPU或 raw LR接口。

factory放在 `axdevice/src/synthetic_irq.rs`，由
`register_builtin_factories()` 在 feature开启时注册。设备使用
`MmioDeviceAdapter`/统一 `Device` trait，并通过 `Device::resources()` 声明
资源；`DeviceBundle` 只承载注册项，本身没有 resources字段。

### 3.2 配置

每个实例：

```toml
{
  name = "spi-edge-256",
  base_ipa = ...,
  length = 4096,
  irq_id = 256,
  emu_type = <reserved>,
  cfg_list = [0]
}
```

`cfg_list`：

- `[0]`：edge；
- `[1]`：level；
- 其他长度/值：InvalidConfig。

factory声明：

```rust
Resource::MmioRange { base, size: 0x1000 }
Resource::IrqLine {
    line: irq_id,
    trigger: EdgeTriggered | LevelTriggered,
}
```

要求：

- length必须恰好 0x1000；
- base 4K对齐；
- irq_id由 PR 3.4 route校验为 SPI；
- 设备名在 VM 内唯一；
- 不在 factory 中 hardcode target vCPU。

### 3.3 MMIO ABI

所有寄存器为 little-endian 32-bit，仅允许 aligned Dword：

| offset | 名称 | 方向 | 语义 |
|--------|------|------|------|
| `0x00` | `PULSE` | WO | 写 1 调用 edge `IrqLine::pulse()` |
| `0x04` | `RAISE` | WO | 写 1 调用 level `set_level(true)` |
| `0x08` | `LOWER` | WO | 写 1 调用 level `set_level(false)` |
| `0x10` | `REPORT` | WO | guest上报阶段/结果，host输出测试 marker |
| `0x18` | `STATUS` | RO | trigger、line asserted、调用/报告计数快照 |

约束：

- edge设备只接受 PULSE，level设备只接受 RAISE/LOWER；
- 写值必须为 1，REPORT使用下面的编码；
- 未知 offset、错误 width、读 WO、写 RO 返回 typed
  Invalid/Unsupported；
- `IrqError` 转换成包含 device name、line、operation 的
  `DeviceError::Backend`，不可日志后返回成功；
- STATUS只保存设备电气状态和测试计数，不复制 VGIC pending/active。

REPORT 编码：

```text
bits 31..28: kind
  0x1 = phase passed
  0xE = failure
  0xF = final pass
bits 27..16: phase
bits 15..0 : value/error code
```

host marker固定：

```text
AXVISOR_EMULATED_SPI PHASE=<n> VALUE=<v>
AXVISOR_EMULATED_SPI FAIL=<code> PHASE=<n>
AXVISOR_EMULATED_SPI PASS
```

仅 final pass进入 success regex；任何 FAIL进入 fail regex。

## 4. EL1 guest

### 4.1 crate 隔离

guest是独立 `#![no_std]`、`#![no_main]` binary crate：

- manifest包含空 `[workspace]`，退出根 workspace；
- 使用 pinned Rust 2024 nightly；
- target `aarch64-unknown-none-softfloat`；
- 提交自己的 `Cargo.lock`；
- 不依赖 std、Alpine或外部预编译镜像；
- 构建输出到 workspace `tmp`，源码目录不生成 target；
- 仓库不提交 ELF/raw binary。

EL0程序不满足要求，因为它不能安装 EL1 vector或配置 ICC system
register。因此测试必须在 EL1直接运行。

### 4.2 启动和链接

linker script至少定义：

- image load/entry地址，与 VM `entry_point`/kernel load配置一致；
- `.text.boot`；
- 对齐的 exception vector；
- `.text/.rodata/.data/.bss`；
- 共享 SMP2状态页和每 CPU stack；
- image end不能越出配置内存。

启动代码：

1. 根据 MPIDR选择 stack。
2. BSP清 BSS；secondary不能重复清。
3. 使用 acquire/release原子量发布 BSS ready。
4. 设置 `VBAR_EL1`。
5. 跳入 Rust `guest_main(mpidr)`。

### 4.3 vCPU 1 启动

vCPU0通过 PSCI 1.0 `CPU_ON` 的 HVC64 calling convention：

```text
x0 = 0xC400_0003
x1 = target MPIDR
x2 = secondary entry
x3 = context（guest不依赖其正确传递）
```

guest根据 `MPIDR_EL1`选择角色，以共享原子状态握手。必须设置超时计数和
FAIL code，不能永久无诊断 spin。

VM config：

- guest逻辑 vCPU 0/1的 `phys_cpu_ids=[0,1]` 用作 MPIDR；
- `phys_cpu_sets=[1,2]` 是位图 `0b01`/`0b10`，使二者分别固定到 host
  pCPU 0/1；
- host QEMU至少 `-smp 2`；
- 不能只看 guest MPIDR就声称跨 CPU wake成立。

### 4.4 GICv3初始化

vCPU1：

1. 设置 `VBAR_EL1`。
2. 确认/启用 `ICC_SRE_EL1.SRE`。
3. 设置 `ICC_PMR_EL1=0xff`。
4. 设置 `ICC_BPR1_EL1`为测试默认。
5. 设置 `ICC_CTLR_EL1.EOImode`为本 case选择的模式。
6. 设置 `ICC_IGRPEN1_EL1=1`。
7. `isb` 后解除 DAIF.I。
8. 发布 IRQ_READY，然后执行 WFI loop。

module-owned SPI由 controller prepare启用，guest不访问 GICD_ISENABLER。
这避免把完整 guest enable MMIO加入模块范围。

IRQ handler：

1. 读 `ICC_IAR1_EL1`。
2. 对 1020–1023 special INTID不记为设备中断。
3. 在无锁/最小临界区按 INTID增加共享计数和 seen bitmap。
4. 写 `ICC_EOIR1_EL1`。
5. EOImode=1时写 `ICC_DIR_EL1`。
6. `dsb sy; isb` 后返回。

handler不得在 IRQ context写 host REPORT（避免嵌套复杂性）；main loop观察
计数后报告。

## 5. 端到端阶段

### 阶段 1：remote edge

- vCPU1 IRQ ready后 WFI。
- vCPU0写 edge device PULSE。
- route target为 vCPU1。
- vCPU1恰好处理一次，vCPU0计数保持0。
- 报告 phase 1。

### 阶段 2：INTID 256

- pulse INTID 256。
- handler读到完整 256，不是0。
- 恰好一次，报告 phase 2。

### 阶段 3：level deactivate重投

- vCPU0写 level RAISE。
- vCPU1处理并完成 EOI/deactivate，但暂不 LOWER。
- 等待计数至少达到2，证明 asserted level在 deactivate后重投。
- vCPU1通过共享命令通知 vCPU0写 LOWER，避免中断 handler直接访问对端设备。
- 完成当前 instance deactivate，等待稳定窗口，计数不再增加。
- 报告 phase 3。

若 case选 EOImode=1，此阶段同时给出 TDIR/DIR证据；EOImode=0由低层测试。
若 case选0则相反。两种模式都必须有 PR 3.3确定性测试。

### 阶段 4：LR pressure/refill

- 配置 17 个不同 edge device和17条固定 route，都指向 vCPU1。
- 选择一组不与 phase 1–3冲突的 SPI INTID。
- vCPU1暂时 mask IRQ，但保持 vCPU可被调度/service。
- vCPU0逐个 pulse 17条 line。
- GICv3 virtualization interface架构上最多16个 LR，所以至少一个保留在软件
  pending。
- vCPU1 unmask。
- seen bitmap最终包含17个 INTID且每个计数恰好1。
- host debug counters/trace证明至少发生一次：
  - LR full/outside pending；
  - UIE underflow或后续 entry触发 refill；
  - 从软件 pending装入释放后的 LR。
- 报告 phase 4和 final PASS。

不能通过对同一 INTID pulse 17次测试 LR pressure，因为 GIC pending latch会
合并，无法占满17个 LR。

## 6. typed guest build

### 6.1 schema

扩展 AxVisor build group：

```toml
[[guest_builds]]
manifest_path = "test-suit/axvisor/guests/emulated-gicv3-spi-smp2/Cargo.toml"
bin = "emulated-gicv3-spi-smp2"
target = "aarch64-unknown-none-softfloat"
profile = "release"
locked = true
output_format = "binary"
output = "tmp/axbuild/axvisor-guests/emulated-gicv3-spi-smp2.bin"
```

使用 typed struct/enum：

```rust
struct GuestBuildConfig {
    manifest_path: PathBuf,
    bin: String,
    target: String,
    profile: BuildProfile,
    locked: bool,
    output_format: GuestOutputFormat,
    output: PathBuf,
}
```

不接受任意 shell prebuild字符串。

当前配置实际由
`scripts/axbuild/src/axvisor/build/config.rs::AxvisorBoardConfig` 以
`BuildInfo` flatten后加载，`qemu_group_build_context()` 最终只保留
`ostool::build::config::Cargo`。`guest_builds` 不能塞进 flatten的 BuildInfo
后依赖 unknown-field行为，否则会被静默丢弃。实现必须：

1. 将 `guest_builds: Vec<GuestBuildConfig>` 作为
   `AxvisorBoardConfig`/`LoadedAxvisorBuildConfig` 的显式字段；
2. 增加保留该字段的 AxVisor build-wrapper loader；
3. 定义私有
   `AxvisorQemuBuildRequest { request: ResolvedAxvisorRequest, guest_builds }`，
   让 `qemu_group_build_context()` 返回
   `(AxvisorQemuBuildRequest, Cargo)`；`prepare_case_build_groups()` 的现有
   closure契约正是 `(R, Cargo)`，`QemuCaseBuildGroup<T, R>` 会把二者分别
   放入 `request` 和 `cargo`，因此不能再把 Cargo重复塞进 `R`；
4. Phase 1 先对 `build_group.request.guest_builds` 执行事务构建，再用
   `build_group.request.request` 和 `build_group.cargo` 调用现有 host build；
   Phase 2 同样从内层 request取得运行参数；
5. `qemu_case_cargo_config()` 的缓存仍以 build config path为 key，host
   Cargo必须与 Phase 1使用同一次解析结果或有等价性测试。

非 QEMU普通 `cargo xtask axvisor build` 若读取到 `guest_builds`，同样先调用
共用的 typed guest-build transaction再构建 host；空列表保持原行为。不能让
同一 build wrapper在 `build` 与 `test qemu` 下产生不同产物。

### 6.2 校验

在启动构建前拒绝：

- manifest越出 workspace或不存在；
- output不在 workspace `tmp/`；
- duplicate output；
- 空 bin/unsupported target/profile/format；
- `locked=false`（本 case必须 true）；
- manifest package/bin不匹配；
- source与output指向同一路径。

路径 canonicalization必须防止 `..` 和 symlink逃逸。

### 6.3 构建事务

每个 build group：

1. 解析并验证全部 guest builds。
2. 按完整 build descriptor去重。
3. 用仓库 pinned toolchain执行 `cargo build --locked --release --target ...`
   和独立 `CARGO_TARGET_DIR`。
4. 定位 ELF。
5. 使用同 toolchain的 `rust-objcopy -O binary` 写同目录临时文件。
6. 校验临时文件非空且大小在 guest memory范围内。
7. 原子 rename到最终 output。
8. 所有 guest成功后才开始 AxVisor host build；`os/axvisor/build.rs` 的
   `include_bytes!` 读取最终文件。

失败不得留下会被下一次 host build误用的半成品。已有旧final output时，
本次开始前应以可恢复方式标记/替换，不能在 guest build失败后继续使用旧文件
并报告成功。

仓库现有 host tool约定使用 `rust-objcopy`（由 pinned toolchain的
`llvm-tools`/cargo-binutils提供），因此实现命令应为
`rust-objcopy -O binary <elf> <temp>`，并复用/抽取现有 axbuild command
availability诊断；不要另选系统 `objcopy` 或在测试过程中静默下载工具。

## 7. VM 和 QEMU 配置

VM：

- `interrupt_mode="emulated"`；
- 一个唯一 InterruptController；
- `fixed_irq_routes`覆盖所有 synthetic lines；
- virtualized address-space policy，不 passthrough host root DT；
- kernel path指向 typed guest output；
- image location/entry/load GPA与 linker一致；
- 2个 vCPU，明确 placement；
- synthetic MMIO不与 RAM/GIC/其他 device重叠。

QEMU：

```text
-machine virt,virtualization=on,gic-version=3
-cpu <支持EL2与GICv3的现有AArch64测试CPU>
-smp <满足host和两个不同pCPU placement>
```

该独立 build wrapper 的顶层 `features` 必须显式包含
`test-synthetic-irq`；普通 AArch64 wrapper不包含它。

success regex只匹配：

```text
AXVISOR_EMULATED_SPI PASS
```

fail regex至少匹配：

- `AXVISOR_EMULATED_SPI FAIL=`
- panic/fatal；
- VM prepare失败；
- timeout由runner判失败。

case设置足够但有限的 timeout；每阶段guest还有自己的较短超时/FAIL code。

## 8. 验证与证据

### 8.1 runner单元测试

- schema round trip。
- duplicate/escape/missing path拒绝。
- build去重。
- guest build失败阻止host build。
- objcopy失败不发布final。
- atomic replacement。

### 8.2 device单元测试

- edge/level config。
- width/offset/value/kind严格错误。
- IrqLine backend error传播。
- STATUS/REPORT。
- feature disabled拒绝创建。

### 8.3 guest可测试逻辑

将 report encoding、phase state、seen bitmap和timeout状态机放在可 host-test
的纯模块；汇编/vector和真实 ICC访问由 QEMU验证。

### 8.4 系统测试

```bash
cargo xtask axvisor test qemu --arch aarch64 --test-group normal \
  --test-case emulated-gicv3-spi-smp2
```

保存/检查日志证据：

- vCPU1 route，vCPU0未处理；
- INTID 256；
- asserted level重投、lower后停止；
- 17个唯一 SPI全部完成；
- target LR写发生在目标 vCPU当前 pCPU；
- 跨 CPU notify/IPI；
- final PASS。

### 8.5 回归

- 现有 AArch64 passthrough AxVisor QEMU case。
- 现有 AArch64普通 smoke。
- module 1 sender/dispatcher host tests。
- axvmconfig旧配置测试。
- test feature关闭的默认 AxVisor build。
- 至少一个非 AArch64 AxVisor build。

代码变更后执行相关 `cargo xtask clippy` 和 `cargo fmt`；guest独立 crate也必须
按其 target执行 fmt和可用 clippy。

## 9. 完成条件

- synthetic device只能经正常 IrqLine触发中断。
- EL1 SMP2 guest可重复构建，不提交生成二进制。
- remote edge、INTID 256、level EOI重投、lower停止、17-line refill均有
  自动化 PASS/FAIL证据。
- placement日志证明跨 CPU，而不只是跨 vCPU。
- 默认产品未启用 test device。
- passthrough和非 AArch64回归通过。

该 PR 完成后，模块 3 的功能和证据同时闭环；测试支持不能成为 production
绕过 controller/Router/delivery分层的后门。

