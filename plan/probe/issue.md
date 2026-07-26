# AxVisor：新增可确定性验证 UEFI 平台契约的 probe.efi

## 问题概述

AxVisor 的 Guest UEFI 支持正在补齐固件加载、ACPI、PCI、块设备和 Runtime Services 等平台能力，但当前缺少一个独立的 Guest 侧验证程序。现有启动测试通常只能判断 Guest 是否到达某条日志，不能稳定回答以下问题：

1. UEFI System Table、Boot Services 和 Runtime Services 是否可访问；
2. Memory Map 是否能够被正确获取和遍历；
3. RSDP、XSDT/RSDT、FADT、MADT、MCFG 等 ACPI 表是否存在且结构有效；
4. UEFI 是否能通过标准协议发现 PCI Root Bridge、目标 PCI 设备和 Block IO 设备；
5. Runtime Services 的时间和变量接口是否可用；
6. 测试失败究竟来自 UEFI ABI、固件表、设备发现、测试运行器，还是 AxVisor 自身。

因此需要新增一个最小、只读、输出稳定的 `probe.efi`，把 Guest 可见的 UEFI 平台能力转换为可由 CI 精确判断的检查结果。

本 issue 按 [design_probe.md](./design_probe.md) 实施。

## 目标

- 新增独立的 `x86_64-unknown-uefi` 应用 `probe.efi`。
- 仅通过标准 UEFI 表和协议观察 Guest 可见能力，不依赖 AxVisor、AxVM 或 axdevice 内部 API。
- 分离“探测到的事实”和“当前 profile 的验收要求”，同一份程序可验证不同能力阶段。
- 提供稳定、版本化、适合 CI 匹配的 `UEFI_PROBE_*` 输出协议。
- 用 host 单元测试验证解析器、策略和报告器，用原生 OVMF/QEMU 验证 EFI 应用及测试运行器。
- 为后续将 `probe.efi` 作为 AxVisor UEFI Guest 运行提供稳定测试载荷和判定协议。

## 与 PR #1612 的边界

本 issue 的交付内容不依赖 PR #1612：

- 新增独立的 probe crate；
- 实现 host 可测试的固件表解析和检查策略；
- 构建 `probe.efi`；
- 在原生 OVMF/QEMU 中运行 smoke test；
- 增加 probe 专用的 xtask 构建和测试入口；
- 文档化 profile、输出协议和验证方法。

以下内容不在本 issue 中实现，应在 PR #1612 合入并稳定 AxVisor 配置及设备接口后另开 issue：

- 把 `probe.efi` 接入 AxVisor VM 配置；
- 新增 AxVisor 嵌套 UEFI Guest test-suit case；
- 根据 probe 结果修改 AxVisor ACPI、PCI、设备模型或中断实现；
- 让 AxVisor CI 以 `UEFI_PROBE_PASS` 作为 Guest 启动验收条件。

## 能力范围

### UEFI Core

- [x] 初始化 UEFI helper，并读取固件 vendor、revision 和 UEFI revision。
- [x] 验证 System Table、Boot Services、Runtime Services 和 Loaded Image 的基本结构。
- [x] 获取 Memory Map，验证 descriptor size、version、数量、区间和整数溢出。
- [x] 通过 UEFI stdout 和 Serial IO 协议输出稳定日志；至少一个通道完整写入才返回成功。

### ACPI

- [x] 从 UEFI Configuration Table 查找 ACPI 2.0 RSDP，缺失时回退 ACPI 1.0 GUID。
- [x] 验证 RSDP signature、长度和 checksum，并按 20/36 字节版本边界读取。
- [x] 验证 RSDT/XSDT header、长度、checksum 和表项边界。
- [x] 枚举 FADT、MADT、MCFG，并校验未知子表的公共 header、length 和 checksum；损坏未知表归到 `acpi-root`。
- [x] 对截断、超长、溢出、错误 checksum、重复必需表和重复物理地址返回分阶段结构化失败；重复地址仅遍历首次出现。

### PCI 与 Block IO

- [x] 通过 `PciRootBridgeIo` 枚举 segment、BDF、vendor/device ID、class code 和 header type。
- [x] 支持按 profile 要求目标 PCI vendor/device，例如 VirtIO vendor `0x1af4`。
- [x] 枚举全部可读 Block IO media，在分配前限制 block size、alignment 和总 buffer。
- [x] 使用正确对齐的 buffer 只读 LBA 0，并可验证测试盘中的固定 magic。
- [x] 不依赖 handle 枚举顺序选择目标磁盘。
- [x] 全部 Block IO protocol open 失败时报告协议错误；部分失败时继续聚合成功设备。

### Runtime Services

- [x] 调用 `GetTime` 并验证返回字段范围。
- [x] 查询变量存储能力，并支持只读变量检查。
- [x] 提供显式启用的变量 round-trip：写入、读取、比较、删除和确认清理。
- [x] 使用项目专属 vendor GUID 和唯一变量名；写入后的所有退出路径都尝试清理。

### Profile 与报告

- [x] 支持 `uefi-core`、`pc-platform` 和 `runtime-vars` profile。
- [x] 每项检查输出 `PASS`、`FAIL` 或 `SKIP`，并使用稳定的检查名和失败原因。
- [x] 输出汇总计数和唯一的最终 `UEFI_PROBE_PASS` 或 `UEFI_PROBE_FAIL` marker。
- [x] profile 参数非法、检查重复或内部状态不一致时显式失败。

## 非目标

本 issue 不包含以下工作：

- 不把 `probe.efi` 实现为 bootloader，也不加载或启动内核；
- 不调用 `ExitBootServices`；
- 不写 PCI configuration space、BAR、MMIO 或 I/O port；
- 不启用设备 bus mastering，不驱动 VirtIO queue；
- 默认不写 Block IO、UEFI 变量、系统时间或复位状态；
- 不验证 LAPIC、IOAPIC、EOI、INTx、MSI/MSI-X 等中断递送语义；
- 不把某一版本 OVMF 的地址、表顺序、handle 顺序或时间值写成固定期望；
- 不依赖 AxVisor 私有结构，也不在本 issue 中增加 AxVisor 嵌套运行配置。

## 实施计划

### 模块 1：crate、profile 和输出协议

- [x] 在 `test-suit/axvisor/uefi/probe/` 新增 `axvisor-uefi-probe` package 和 `probe` binary。
- [x] `main.rs` 只负责 UEFI 初始化、profile 解析、运行检查和返回最终状态。
- [x] 将检查状态、profile、结果聚合和 marker 格式放入 host 可测试的 library。
- [x] 定义版本 1 输出协议，并限制单行长度和字段字符集。
- [x] 增加 profile 解析、必需/可选策略、汇总计数和报告格式单元测试。

配置来源固定为内置默认值、`\EFI\BOOT\probe.cfg`、Loaded Image Load
Options，按此顺序合并且后者覆盖前者。配置文件存在时必须是最大 4 KiB 的
UTF-8 文本并包含 `version=1`；Load Options 使用相同字段的 UTF-16
`key=value` 语法。文件 metadata 和借用的 Load Options 长度必须在复制前检查。

### 模块 2：UEFI Core 和 ACPI

- [x] 实现 System Table、Loaded Image、Memory Map 探测。
- [x] 实现有界的 RSDP、RSDT/XSDT 和 ACPI SDT 解析器。
- [x] 将裸指针访问限制在固件边界模块，在每个 `unsafe` 位置说明有效性前提。
- [x] 增加合法、截断、错误 checksum、异常长度和整数溢出 fixture。

### 模块 3：PCI、Block IO 和 Runtime Services

- [x] 通过 UEFI protocol API 枚举 PCI Root Bridge 和 Block IO。
- [x] 增加目标 PCI 设备和测试盘 magic 的 profile 条件。
- [x] 实现 `GetTime` 和变量只读检查。
- [x] 实现默认关闭、可完整清理的变量 round-trip。
- [x] 增加与 handle 顺序无关的选择和结果聚合测试。

### 模块 4：OVMF/QEMU 测试运行器

- [x] 增加 `cargo xtask uefi-probe build`。
- [x] 增加 `cargo xtask uefi-probe test qemu`。
- [x] 复用或抽取 axloader 已有的 OVMF 查找、临时 ESP、QEMU 子进程、超时和 transcript 处理能力。
- [x] 使用临时 OVMF VARS 副本，禁止修改系统固件模板。
- [x] 增加 `uefi-core` 原生 OVMF 用例。
- [x] 增加 Q35 + VirtIO Block 的 `pc-platform` 用例。
- [x] 增加一个故意要求不存在设备的负向用例，并要求它是唯一逻辑失败。

## 测试与验证

### Host 测试

- [x] profile required/optional 规则。
- [x] PASS、FAIL、SKIP 聚合和最终状态。
- [x] parser 验证字段转义、严格 BEGIN/CHECK/SUMMARY/terminal envelope、固定检查顺序、最大长度、截断标记、完整副本和 marker 唯一性。
- [x] ACPI 合法表、错误 signature、错误 checksum、截断和溢出。
- [x] 测试盘 magic 匹配和损坏输入。

### Target 检查

- [x] `cargo fmt --all -- --check`
- [x] `cargo test -p axvisor-uefi-probe --all-targets`
- [x] `cargo xtask clippy --package axvisor-uefi-probe`
- [x] 对 `x86_64-unknown-uefi` target 执行 clippy/check。
- [x] release 构建生成 `probe.efi`。
- [x] 使用 axbuild 的 `object` 解析确认产物为 AMD64 EFI application，且存在有效入口。

### QEMU 测试

- [x] 所有用例都有明确超时。
- [x] runner 解析固定检查集合、summary、最终 marker 和 panic/internal-error，不把 QEMU 进程状态误当成 EFI application 返回状态。
- [x] 失败时保留完整串口 transcript 和 QEMU 命令行。
- [x] core 输入重复运行两次，profile、check status/reason 及最终结果一致。
- [x] 正向用例必须通过，负向用例必须且只能产生预期的结构化失败。

## 完成标准

- [x] `probe.efi` 不依赖 AxVisor、AxVM 或 axdevice 内部 API。
- [x] 默认执行路径不修改固件、磁盘、PCI 配置或设备状态。
- [x] 所有来自固件的长度、数量和地址运算都有边界或溢出检查。
- [x] parser 和策略的错误路径具有确定性 host 测试。
- [x] `cargo xtask uefi-probe test qemu` 能在原生 OVMF/QEMU 中完成正向和负向验证。
- [x] 输出协议具有版本，CI 不依赖易变地址、时间、表顺序或 handle 顺序。
- [x] 文档说明能力边界、profile、输出格式、验证方法和后续 AxVisor 集成方式。
- [x] 没有引入“非目标”中列出的额外功能。

## 后续工作

本 issue 完成后另开 AxVisor 集成 issue，依赖本 issue 和 PR #1612 合入后的接口：

1. 将 `probe.efi` 放入 AxVisor Guest ESP；
2. 使用 AxVisor 提供的 OVMF、ACPI、PCI 和 Block IO 能力运行 `pc-platform`；
3. 在 `test-suit/axvisor/uefi/` 中匹配 `UEFI_PROBE_PASS`；
4. 中断、VirtIO queue 和 Linux EFI stub 启动继续使用独立 Guest workload 验证。
