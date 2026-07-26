# probe.efi 修改复审与当前成果

## 1. 复审结论

本轮修改已经达到上一轮复审的主要目的：`axbuild` 发布边界、Block IO 零 block size、
报告 render/parse 闭包、完整 envelope 顺序以及 ACPI 未知表和重复地址契约均已落实，相关
设计文档、README 和回归测试也已同步。

本轮没有发现新的功能性阻塞问题。就第一阶段“独立 probe.efi + 原生 OVMF/QEMU 验证链”
而言，实现已经满足 `plan/issue.md` 和 `plan/design_probe.md` 的目标，可以进入提交准备阶段。

提交前仍有一项必须完成的工作：将所有新增但尚未跟踪的源文件和资源纳入 commit。另有两项
不阻塞当前功能、但建议在提交前处理或记录为后续改进的维护性问题。

## 2. 上一轮问题复核

### 2.1 `axbuild` 发布依赖边界：已解决

`axbuild` 已移除对 `publish = false` 的 `axvisor-uefi-probe` crate 的依赖：

- 根 `[workspace.dependencies]` 中的 probe path dependency 已删除；
- `scripts/axbuild/Cargo.toml` 中的 probe dependency 已删除；
- `Cargo.lock` 中 `axbuild` 对 probe 的依赖关系已删除；
- version 1 host parser 移入 `scripts/axbuild/src/uefi_probe/report.rs`，不进入 probe 的发布
  边界。

`cargo package -p axbuild --allow-dirty --list` 确认发布包包含新的 parser 和三份
agent-review-bench 契约资源。根据 `plan/feedback.md`，完整 `cargo package` 已从生成的 crate
tarball 独立编译通过，`cargo publish -p axbuild --dry-run --allow-dirty` 也完成验证。

因此，上一轮“已发布 axbuild 依赖不可发布测试 crate”的阻塞问题已经消除。

### 2.2 Block IO 零 block size：已解决

firmware adapter 现在只跳过 `media_present = false` 的 handle。介质存在时，包括
`block_size == 0` 在内的描述都会进入 `allocate_media_block_buffer()` 和统一 geometry
校验，零 block size 会成为 `invalid-field`，不会再被误报为设备缺失。

新增 host 回归覆盖了以下不变量：

- absent media 可以安全跳过；
- present media 的零 block size 必须失败；
- 固件几何值在分配前完成大小、对齐和溢出校验；
- 分配失败通过结构化错误返回。

### 2.3 报告 renderer/parser 闭包和 envelope：已解决

renderer 现在省略空 `detail`，因此公开的 `with_detail("")` 不再生成 parser 拒绝的
`detail=` 字段。新增 round-trip 测试已覆盖该情况。

probe parser 和 axbuild host parser 都以状态机校验首次逻辑记录顺序：

1. BEGIN；
2. 固定顺序的 16 个 CHECK；
3. SUMMARY；
4. terminal marker。

相同的 stdout/Serial IO 物理副本仍可重复，冲突副本、非 canonical percent encoding、
未知 version 1 record、非法字段、summary 不一致、冲突 terminal marker、panic 和
`internal-error` 都会被拒绝。该行为与设计文档和 README 当前描述一致。

### 2.4 ACPI 未知表与重复地址：已解决

version 1 契约已明确为：

- 未知 signature 仍校验公共 SDT header、length 和 checksum；
- 有效未知表不增加固定逻辑检查；
- 损坏未知表使 `acpi-root` 失败；
- 根表重复物理地址使 `acpi-root` 以 `invalid-field` 失败；
- 重复地址只遍历第一次出现的位置；
- FADT、MADT、MCFG 继续独立保留 validated、missing 或 failed 状态。

实现、`plan/design_probe.md`、`plan/issue.md` 和 probe README 已采用同一策略。新增测试覆盖
未知表 checksum 损坏和重复未知表物理地址，上一轮的文档/实现矛盾已经消除。

## 3. 剩余审阅意见

### 3.1 提交阻塞：新增文件仍处于 untracked 状态

当前 `git status --short` 中至少包含以下关键未跟踪内容：

```text
?? scripts/axbuild/src/uefi_probe/report.rs
?? scripts/axbuild/assets/
?? plan/
```

其中 `report.rs` 是 axbuild 编译所需源文件，`assets/` 是 axbuild 发布包独立编译所需资源。
当前测试和 `cargo package --allow-dirty` 会读取工作区里的未跟踪文件，所以测试通过不能保证
提交后的分支仍然完整。如果提交时遗漏这些文件，CI 或发布包构建会直接失败。

提交前必须确认这些文件全部纳入 Git，并在 staged/committed 状态下重新执行至少：

```text
cargo test -p axbuild --lib uefi_probe
cargo package -p axbuild --allow-dirty
```

### 3.2 维护性：agent-review-bench 契约资源存在两套来源

以下三份文件目前同时存在于 `scripts/agent-review-bench/` 和
`scripts/axbuild/assets/agent-review-bench/`，本轮比较确认内容相同：

- `reviewer.md`；
- `schemas/review.schema.json`；
- `schemas/grade.schema.json`。

axbuild 代码只编译包内 `assets/` 副本。后续如果维护者只修改原目录中的文件，发布版
axbuild 会继续嵌入旧契约，普通编译和测试不一定能发现漂移。

该问题不阻塞 probe.efi 功能。建议将包内资源设为唯一来源并让其他流程引用它，或者增加
确定性测试，逐字节比较两处资源，确保修改任一契约时不会遗漏同步。

### 3.3 维护性：独立 host parser 的反例覆盖仍偏少

将 parser 独立放入 axbuild 解决了发布边界问题，但也形成了两个协议实现。probe parser
已有较完整的反例测试；实际 QEMU runner 使用的 axbuild parser 当前只有一个集中测试，直接
覆盖了合法报告、CHECK-before-BEGIN 和非 canonical 编码，其他约束主要依赖实现代码与
probe 侧测试保持人工同步。

当前两份实现的关键规则一致，原生 QEMU matrix 也验证了真实 producer/consumer 正向链路，
因此这不是当前正确性阻塞。为降低后续 version 1 漂移风险，建议建立一组不依赖 probe crate
的共享文本 fixture/corpus，让两侧分别验证同一批合法和非法 transcript，至少覆盖：

- SUMMARY-before-CHECK、terminal-before-SUMMARY；
- 缺失或乱序 CHECK；
- 冲突重复副本；
- 超长行和非法 `truncated`；
- summary/terminal 不一致；
- panic、`internal-error` 和未知 protocol record。

## 4. 本轮验证结果

本轮实际执行并确认：

```text
cargo fmt --all -- --check
  passed

CARGO_TARGET_DIR=/tmp/tgoskits-probe-review-20260720 \
  cargo test -p axvisor-uefi-probe --all-targets
  44 passed, 0 failed

CARGO_TARGET_DIR=/tmp/tgoskits-axbuild-review-20260720 \
  cargo test -p axbuild --lib uefi_probe
  5 passed, 0 failed

CARGO_TARGET_DIR=/tmp/tgoskits-probe-clippy-20260720 \
  cargo xtask clippy --package axvisor-uefi-probe
  passed

CARGO_TARGET_DIR=/tmp/tgoskits-probe-clippy-20260720 \
  cargo xtask clippy --package axbuild
  passed

cargo package -p axbuild --allow-dirty --list
  probe parser 和三份包内契约资源均在 package 文件列表中

git diff --check
  passed
```

直接使用默认 `target/` 运行 clippy 时仍会被既有
`target/debug/.fingerprint/tg-xtask-*` 权限问题阻断；改用 `/tmp` 独立 target 后通过。这是
本地构建缓存权限问题，不是本轮实现失败。

按 `update-std-tests` 的项目流程重新审计 workspace 后，没有发现遗漏在
`scripts/test/std_crates.csv` 之外的新候选；`axvisor-uefi-probe` 已正确位于白名单中。

本轮没有重新执行耗时较长的完整原生 OVMF/QEMU matrix、UEFI target clippy、axloader
HTTP smoke 和 publish dry-run；这些结果采用 `plan/feedback.md` 中本轮修改后的实际记录：

- core 双跑、pc、runtime、negative QEMU case 全部通过；
- negative case 只有 `pci-expected/expectation-not-met` 一个逻辑失败；
- `x86_64-unknown-uefi` target check/clippy 通过；
- axloader HTTP smoke 通过；
- axbuild package 独立编译和 publish dry-run 通过；
- 48 个 std 白名单 package 全部通过。

完整 axbuild lib tests 中记录的一个并行时序型 agent-review-bench 偶发失败不涉及 probe、
UEFI parser 或发布资源路径；该用例单独运行通过。本轮未扩大范围修改该既有问题。

## 5. probe.efi 当前开发成果

### 5.1 独立 UEFI 探测应用

已经实现独立的 `x86_64-unknown-uefi`、`no_std` UEFI application：

- 产物为 `probe.efi`；
- 只依赖标准 UEFI table、Configuration Table 和 protocol；
- 不依赖 AxVisor、AxVM 或 axdevice 内部 API；
- 默认不调用 `ExitBootServices`，不承担 bootloader 或 Guest OS 启动职责；
- 默认只读，不写磁盘、PCI 配置、BAR、MMIO 或系统时间。

### 5.2 配置和 profile

已经实现固定路径 `\EFI\BOOT\probe.cfg` 与 Loaded Image options 两层配置输入：

- 配置文件使用带 `version=1` 的 UTF-8 `key=value` 格式；
- Load Options 使用 UTF-16，同名字段覆盖配置文件；
- 文件在复制前限制为 4 KiB，Load Options 在复制前限制为 2048 UTF-16 units；
- 重复 key、未知 key、空值、非法编码和非法数值均返回结构化配置错误；
- 支持累计能力 profile：`uefi-core`、`pc-platform`、`runtime-vars`；
- 支持 `expect-pci`、`expect-block-magic` 和显式 `variable-mode=roundtrip`。

### 5.3 固定的 16 项平台契约检查

probe 按稳定顺序产生以下检查：

1. config；
2. system-table；
3. loaded-image；
4. memory-map；
5. acpi-rsdp；
6. acpi-root；
7. acpi-fadt；
8. acpi-madt；
9. acpi-mcfg；
10. pci-root-bridge；
11. pci-expected；
12. block-io；
13. block-magic；
14. runtime-time；
15. runtime-variable-info；
16. runtime-variable-roundtrip。

每项检查根据 profile 和配置生成 PASS、FAIL 或 SKIP，并使用固定 reason code 区分能力缺失、
协议失败、签名/长度/checksum 错误、整数溢出、期望不匹配、清理失败、配置错误和依赖失败。

### 5.4 防御性固件解析和只读设备探测

当前实现已经具备：

- System Table、Boot/Runtime Services、Loaded Image 和 Memory Map 基础验证；
- ACPI 1.0/2.0 RSDP 选择与分段读取；
- 有界 RSDT/XSDT、公共 SDT、FADT、MADT 和 MCFG 解析；
- ACPI checksum、长度、条目推进、bus range、重复 signature 和重复物理地址校验；
- 通过 UEFI `PciRootBridgeIo` 进行只读 PCI 枚举、排序、去重和目标设备匹配；
- 通过 UEFI Block IO 对 present media 执行有界、对齐、只读的 LBA 0 读取和 magic 匹配；
- 区分设备缺失、全部 protocol open 失败、无效 geometry 和读取失败；
- Runtime Services `GetTime` 和变量信息查询；
- 默认关闭的变量 round-trip，以及读取失败、内容不一致等失败路径上的删除清理；
- 清理失败优先报告，避免把残留固件变量隐藏在业务失败之后。

所有复杂解析和策略逻辑均可在 host 上测试；裸指针和 UEFI protocol 类型限制在 firmware
adapter 边界。

### 5.5 稳定的 version 1 报告协议

已经实现机器可解析的 `UEFI_PROBE_*` version 1 协议：

- 固定 BEGIN/CHECK/SUMMARY/terminal envelope；
- 固定 16 项检查及其顺序；
- 最大 512-byte protocol line；
- UTF-8 detail 的 canonical percent encoding；
- 有界 detail 截断和 `truncated=1`；
- stdout 与 Serial IO 重复物理记录折叠；
- 冲突副本、未知记录、panic、internal-error、summary/terminal 不一致均导致 runner 失败；
- 动态 detail 不参与重复运行的稳定结果比较。

### 5.6 axbuild 原生验证链

axbuild 已提供：

```text
cargo xtask uefi-probe build
cargo xtask uefi-probe test qemu
```

build 命令会生成 release/debug EFI image，并用 `object` 校验 AMD64 PE32+、EFI application
subsystem 和非零入口。QEMU runner 会：

- 查找或接收显式 OVMF CODE/VARS；
- 只读使用 CODE，并为每个 case 复制临时 VARS；
- 为每次执行创建独立临时 ESP 和测试盘；
- 使用 Q35 + TCG 启动原生 OVMF；
- 执行 core、pc、runtime 和预期失败 negative matrix；
- 为每个 case 设置超时；
- 在 terminal marker 后有界排空输出；
- core case 独立运行两次并比较稳定结果；
- 失败时保留完整 transcript 和 QEMU 命令行。

### 5.7 测试与 CI 接入

目前已经形成以下验证层次：

- probe host unit tests，当前 44 项；
- axbuild runner/parser 定向 tests，当前 5 项；
- probe 和 axbuild 定向 clippy；
- `x86_64-unknown-uefi` target check/clippy；
- `cargo xtask uefi-probe test qemu` 原生 OVMF matrix；
- `cargo axloader test qemu --target x86_64-unknown-uefi` 兼容性 smoke；
- `axvisor-uefi-probe` 已加入 `scripts/test/std_crates.csv`；
- CI 已接入 host tests、clippy、UEFI target clippy 和原生 QEMU matrix。

### 5.8 当前阶段边界

第一阶段已经交付的是“独立 probe + 原生固件基线”。以下能力仍属于明确的后续工作，不应
被误认为本阶段遗漏：

- 将 `probe.efi` 放入 AxVisor Guest ESP 并建立嵌套 Guest 测试；
- 验证 AxVisor 提供的 Guest OVMF、ACPI、PCI 和 Block IO 契约；
- 验证中断注入、LAPIC/IOAPIC、INTx、MSI/MSI-X 和 EOI；
- 提交 VirtIO queue/DMA 请求；
- 调用 `ExitBootServices` 或验证 runtime virtual mapping；
- 加载 Linux EFI stub 或其他 Guest OS；
- 扩展到 x86_64 以外架构。

这些工作应在 PR #1612 合入并稳定相关接口后，通过新的 AxVisor 集成 issue 独立推进。
