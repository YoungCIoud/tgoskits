# PR 1：固定 OVMF SEC 到达诊断（uefi-p0-r2 阶段 1）— 评审与合入文档

> 本文档配套 `plan/uefi/design.md`（设计）与 `plan/uefi/solve_conflict.md`（方案），面向 `feat/uefi-p0-r2` 分支合并到 `dev` 的评审、验证记录与 PR 素材。
> 分支：`feat/uefi-p0-r2`（自 `dev@4c7aa8577` 新建，6 个提交），目标分支：`dev`。
> 范围：**阶段 1（固定 OVMF SEC 到达诊断）**。阶段 2-5（ACPI/PCI/virtio/中断）保留为 `plan/uefi/design.md` 中的路线图，不在本 PR 范围内。

## 1. 元信息

| 项 | 值 |
|---|---|
| 分支 | `feat/uefi-p0-r2` |
| 基点 | `dev@4c7aa8577`（`refactor(axvisor): clean up host dependencies (#1861)`） |
| 目标分支 | `dev` |
| 提交数 | 6（全部独立，无 merge） |
| 净变更 | 13 文件，+2707/-45 |

| # | 提交 | 标题 | 改动 |
|---|---|---|---|
| 1 | `8fe00af47` | feat(axbuild): verify fixed OVMF debug bundles | `scripts/axbuild/src/axvisor/mod.rs`(+50)、`scripts/axbuild/src/axvisor/ovmf.rs`(新增 1058 行) |
| 2 | `36b4d5b85` | feat(axvm): enforce the fixed x86 OVMF CODE layout | `virtualization/axvm/src/arch/x86_64/boot/mod.rs`(+176)、`virtualization/axvmconfig/{error,lib,templates,test}.rs` |
| 3 | `2a67bf5b9` | feat(axdevice): trace guest device accesses in DeviceRuntime | `virtualization/axdevice/src/device.rs`(+407/-31) |
| 4 | `9881374a6` | test(axvisor): add fixed OVMF entry diagnostics | `os/axvisor/configs/vms/qemu/x86_64/ovmf-entry.toml`(新增)、`scripts/axbuild/src/axvisor/test/qemu.rs`(+753)、`test-suit/axvisor/uefi/ovmf-entry/`(4 个 TOML 新增) |
| 5 | `093e0c1fa` | docs(axvisor): document the fixed OVMF profile | `.claude/skills/arch-platform-porting/references/boot-debugging.md`(+84)、`docs/docs/architecture/`(3 个)、`docs/docs/build/`(2 个) |
| 6 | `245e038c7` | fix(axvisor): anchor ovmf-entry SEC match to the nested VM | `test-suit/axvisor/uefi/ovmf-entry/qemu-x86_64-{vmx,svm}.toml`(success_regex 锚定)、`docs/docs/build/axvisor/test.md`、`boot-debugging.md` |

## 2. 问题背景：x86_64 guest UEFI 启动诊断缺失

AxVisor 的 x86_64 路径缺少可诊断的 UEFI guest 启动支持：OVMF 固件从 reset vector 进入 SEC 后，AxVisor 无法确定"固件走到了哪一步"——没有可复现、可归因的固定资产（发行版 OVMF 版本漂移、不可复现）、没有装载时的布局强制（CODE 大小/GPA/reset vector 无校验）、没有统一的设备访问 trace（无法定位固件首次未支持的平台访问）、没有 SEC 到达的端到端测试门禁。这使后续 UEFI 功能（fw_cfg/ACPI/PCI/virtio，见 `plan/uefi/design.md` 阶段 2-5）缺少诊断基础。

同时，dev 现有边界对实现提出了明确约束（见 `plan/uefi/design.md` §4）：

- **串口为 machine-owned 资源**：x86 machine profile 固定提供 16550 COM1（`virtualization/axvm/src/machine.rs:335`，PIO `0x3f8` + GSI 4），并明确禁止 `emu_devices`/`serial` 字段——SEC 判定不能通过 guest TOML 注册 COM1 或新增串口截获代码；
- **配置模型为 `guest_type = "passthrough"` + `[devices]`**：旧 `vm_type = 1`/`emu_devices = [...]` 配置模型已失效，新用例须按新模型编写；
- **`Device` trait 在 `axdevice_base`**（`name/resources/access`），`DeviceRuntime::dispatch` 返回 `Result<BusResponse, DeviceError>`——设备访问 trace 须按新 API 实现；
- **测试规范收紧**：禁止源码文本断言（`book/guideline/code-quality.md:1713`），校验测试须编译并调用真实 API；
- **脚本/用例已删除**：`d7050469f` 删除了 `setup_qemu.sh`/`quick-start.sh`/`ovmf-profile.sh`/NimbOS 配置/qemu-nimbos 用例，固件校验职责与测试用例须迁入 Rust 编排层与新的 `test-suit/axvisor/` 分组。

## 3. 方案概述

按 `plan/uefi/solve_conflict.md` §4 的五个提交顺序实施（提交 6 为真实验证暴露的修复提交），实现前先同步 `plan/uefi/design.md`（基线 `4c7aa8577`、阶段 1 范围声明、machine-owned COM1 等），保证实现与已评审设计一致。

**阶段 1 范围**：只建立"固定 OVMF 资产 + 固件装载校验 + SEC 到达判定"的最小闭环。**不实现** fw_cfg/ACPI/PCI/virtio（阶段 2-5）。COM1 已由 machine profile 提供，SEC 判定直接以 guest COM1 输出为准（`success_regex`），不新增串口截获代码。

每提交独立走"实现 → subagent 全量评审（实跑 clippy/fmt/单测）→ fix → 复审"闭环；评审意见按 `action=fix`（实现轮内修复）/`action=issue`（收敛到 `plan/collect_issue.md` 跟踪）分级。

## 4. 各提交说明

### 提交 1 `8fe00af47` feat(axbuild): verify fixed OVMF debug bundles

**改动**：`scripts/axbuild/src/axvisor/ovmf.rs`（新增 1058 行）、`scripts/axbuild/src/axvisor/mod.rs`（+50）。

**内容**：

- 固定 profile 常量集中定义：`OVMF_PROFILE_NAME = "qemu_x86_64_axvisor_ovmf_debug"`、`OVMF_EDK2_TAG = "edk2-stable202605"`（commit `b03a21a6…`）、`OVMF_CODE_BASE = 0xffc8_4000`、`OVMF_CODE_SIZE = 0x37_c000`、`OVMF_VARS_BASE = 0xffc0_0000`、`OVMF_VARS_SIZE = 0x8_4000`、`OVMF_COMBINED_SIZE = 0x40_0000`、`OVMF_RESET_VECTOR = 0xffff_fff0`；
- 私有类型：`OvmfManifest`（serde/TOML 解析，`deny_unknown_fields`）、`VerifiedOvmfBundle`（含 `code_end()`/`reset_vector_in_code_window()` 谓词）、`FirmwareSource` 枚举（区分"受管固定 bundle"与"显式 opt-in 的未验证本地固件"，避免布尔参数扩散），入口 `verify_firmware(source)`；
- 校验链：解析 bundle 目录的平铺 `manifest.toml` → 逐字段值校验（profile、edk2 tag/commit、GPA/大小常量、reset vector、boot markers）→ 逐文件大小与 SHA-256（复用现有 `support::download::file_sha256`）→ 校验 VARS||CODE 逐字节拼接的 `OVMF.fd` 组合布局。未验证本地固件必须 `--allow-unverified-firmware` 显式 opt-in 且打印 SHA-256，同时仍须匹配固定 CODE 大小；校验逻辑不下载任何东西；
- CLI：`cargo xtask axvisor test qemu` 新增 `--firmware-bundle-path <dir|file>` 与 `--allow-unverified-firmware`。

**为什么这么改**：替代已删除的 `ovmf-profile.sh` 的校验职责（脚本不恢复，校验迁入 Rust 编排层）；`deny_unknown_fields` + 逐字段值校验保证资产被篡改/换版本时编译层即可拒绝，避免 CI 静默使用发行版 OVMF。

**验证**：17 个 host 单测覆盖正常 bundle、错误 SHA-256/大小/base/profile、未知字段、重复字段、错误拼接布局、错误 reset vector、reset vector 窗口谓词、缺失字段、未验证固件缺 opt-in 拒绝。

### 提交 2 `36b4d5b85` feat(axvm): enforce the fixed x86 OVMF CODE layout

**改动**：`virtualization/axvm/src/arch/x86_64/boot/mod.rs`（+176）、`virtualization/axvmconfig/src/{error,lib,templates,test}.rs`（+87）。

**内容**：

- `axvmconfig` 新增 `VMKernelConfig::firmware_profile: Option<String>` 配置字段（TOML 键 `firmware_profile`）；校验规则：声明 `firmware_profile` 但 boot 协议非 UEFI 时返回新错误变体 `AxVmConfigError::FirmwareProfileRequiresUefi`；模板默认 `None`；
- x86 loader 在 `load_uefi_from_configured_path`（fs 路径，`boot/mod.rs:270`）与 `load_boot_image_from_memory`（内存镜像路径）两条路径上，当 `fixed_ovmf_profile_enabled`（profile 等于固定值）时先做布局校验再装载：CODE 大小 `0x37c000`、装载 GPA `0xffc84000`、reset vector `0xfffffff0`（fs 路径先经 `BootImageProvider` 查大小）；成功装载打印 profile、路径、大小、GPA、reset vector；multiboot/direct boot 路径不受影响。

**为什么这么改**：校验必须**只在固定 profile 显式启用时生效**——dev 现存 `arceos-uefi-smp1.toml` 以 `bios_load_addr = 0xffc0_0000` 加载 OVMF_CODE.fd，若无条件拒绝非 `0xffc84000` 的装载会破坏现有配置（solve_conflict.md §4 提交 2 的作用域说明）。默认（无 profile）行为与 dev 完全一致。

**验证**：axvmconfig 4 个新单测（默认 None、显式解析、非 UEFI 拒绝、UEFI 接受）；axvm loader 校验走真实 API 的单测（非源码文本断言）。

### 提交 3 `2a67bf5b9` feat(axdevice): trace guest device accesses in DeviceRuntime

**改动**：`virtualization/axdevice/src/device.rs`（+407/-31）。

**内容**：

- 统一 trace 点：`DeviceRuntime::dispatch` 路由路径与绕过 dispatch 的 `handle_mmio_write_with_memory` 路径各加一处 `trace_device_access`，输出格式 `guest device access: device=<name> bus=PIO|MMIO direction=read|write addr=<addr:#x> width=<width> value=<value> result=<ok|error>`；
- 未注册地址仍返回 `DeviceError::NotFound` 并记录为 `device=<unregistered>`，不伪造返回值；SysReg 访问视为高频噪声不 trace；替换死代码 `log_device_io`（含其 `allow(dead_code)`）。

**为什么这么改**：按 `axdevice_base` 新 `Device`/`BusAccess` API 实现设备访问 trace；trace 只加日志、不改行为，作为阶段 2+ 的"OVMF 接下来访问了什么"的诊断输入（design.md §7 阶段 1 工作项 4）。

**验证**：9 个 host 单测覆盖注册设备 read/write/error 结果不变、未注册设备四种宽度两种 bus、越界端口地址——断言 trace 点不改变既有访问结果。

### 提交 4 `9881374a6` test(axvisor): add fixed OVMF entry diagnostics

**改动**：`os/axvisor/configs/vms/qemu/x86_64/ovmf-entry.toml`（新增 69 行）、`scripts/axbuild/src/axvisor/test/qemu.rs`（+753/-5）、`test-suit/axvisor/uefi/ovmf-entry/` 4 个 TOML（build-x86_64-unknown-none-{vmx,svm}.toml + qemu-x86_64-{vmx,svm}.toml）。

**内容**：

- 新增 `test-suit/axvisor/uefi/` 测试分组（dev 发现机制 `discover_group_names` 自动发现 `test-suit/axvisor/` 下所有目录，非默认 `normal` 组天然 non-gating）；用例 `ovmf-entry`，按 build wrapper 变体生成 `ovmf-entry-vmx`/`ovmf-entry-svm` 两个用例名；VMX/SVM 的 qemu 参数仅 `-cpu` 一行不同（`+vmx-ept,…` vs `+svm,+npt,+nrip-save`），其余参数相同；
- 启动链路：QEMU `-kernel` 加载 Axvisor 宿主（`uefi = false`，阻止 ostool 下载自带 OVMF），Axvisor 再把嵌套 OVMF 作为 UEFI guest 固件从 VM 配置加载；`success_regex = ["(?s)VCpu\\[0\\] running\\.\\.\\..*SecCoreStartupWithStack"]` 锚定嵌套 VM 启动后匹配 guest COM1 的 SEC marker（`-serial stdio`，原始字节透传）。**前缀锚定是必需的**：QEMU 层为引导 Axvisor 宿主而注入的 pflash OVMF 会输出相同的 `SecCoreStartupWithStack(` 行，裸 `SecCoreStartupWithStack\(` 会误匹配宿主固件导致假阳性（该缺陷在真实 QEMU 验证中发现，见提交 6）；
- **固定资产接线（关键约束，solve_conflict.md §5.3）**：不能用 ostool 的 `uefi = true`（会从 `Source::LATEST` 下载 `edk2-stable202508-r1` 并注入 FAT ESP，验证的不是固定资产）。runner 消费 `--firmware-bundle-path`：manifest 校验过的 CODE 经 `include_bytes!` 嵌入生成的 per-run VM 配置（`image_location = "memory"`），同时以 `-drive if=pflash,format=raw,unit=0,readonly=on,file=<CODE>` + unit=1（VARS 每运行复制一份，保证隔离）注入 QEMU 层——宿主与嵌套两层固件字节级相同；VARS 拷贝复用现有 `apply_drive_snapshot_without_global_snapshot` 机制；
- 无 bundle 时 fail-fast：`verify_cli_firmware_bundle` 在 discovery 之后、任何 build/QEMU 工作之前执行；ovmf-entry 用例无 `--firmware-bundle-path` 时直接报错退出（绝不静默 fallback 到发行版 OVMF，也不会挂死在 `Booting from ROM..`）；`--list` 早于 bundle 校验返回，列出用例不需要 bundle；
- 用例前缀防御：`ensure_ovmf_entry_case_dir` 要求用例目录精确等于 `ovmf-entry`，防止未来任何前缀同名的用例误接固定资产；
- 新增 `os/axvisor/configs/vms/qemu/x86_64/ovmf-entry.toml` 专用 VM 配置（`guest_type = "passthrough"`、1 vCPU、4 MiB 覆盖 `0xffc0_0000-0x100000000` 的内存区），checked-in 回退路径 `image_location = "fs"` + `/guest/ovmf/OVMF_CODE.fd`（rootfs 中不存在该文件，无 bundle 时 VM 以明确错误初始化失败，而不是静默引导发行版 OVMF）。

**为什么这么改**：使 SEC 到达诊断接入最新测试编排，与 dev 现有 `test-suit/axvisor/normal/qemu/` 布局一致；不声明 COM1/emu_devices、不恢复任何已删除脚本与用例；保持 non-gating（见 §6）。

**验证**：

- `--list` 正确发现 `ovmf-entry-vmx`/`ovmf-entry-svm` 两个变体；
- 无 bundle 时 fail-fast 报错，且 `--list` 不要求 bundle；
- 真实 QEMU 运行（SVM，本机 AMD）：本地构建的固定 DEBUG bundle（`edk2-stable202605`，`/home/young/code/edk2/Build/OvmfX64/DEBUG_GCC/FV`）经 `verify_firmware` 校验通过（VERIFIED）；首次运行暴露 success_regex 假阳性（847ms 即 PASS，匹配到宿主 OVMF 的 SEC），修复锚定后 23s PASS，嵌套链路完整证据：`Loading fixed OVMF CODE profile <memory> (3653632 bytes) into GPA @0xffc84000` → `VM[1] boot success` → `VM[1] VCpu[0] running...` → 嵌套 `SecCoreStartupWithStack(` → PEI → fw_cfg 探测；
- 现有 direct VMX/SVM smoke 回归与 dev 基线一致。

### 提交 6 `245e038c7` fix(axvisor): anchor ovmf-entry SEC match to the nested VM

**背景**：真实 QEMU 验证发现提交 4 的 success_regex 缺陷——QEMU 层为引导 Axvisor 宿主注入的 pflash OVMF 与嵌套 OVMF 输出**相同的** `SecCoreStartupWithStack(` 行（文本完全一致），裸 regex 匹配到宿主层，847ms 即假 PASS，嵌套 VM 尚未启动。

**改动**：`test-suit/axvisor/uefi/ovmf-entry/qemu-x86_64-{vmx,svm}.toml` 的 success_regex 改为 `(?s)VCpu\[0\] running\.\.\..*SecCoreStartupWithStack`（`(?s)` 跨行，仓库已有 `qemu-panic-no-backtrace` 用例同语法先例），锚定到嵌套 VM 启动（`VCpu[0] running...` 仅嵌套层输出）；同步更新 `docs/docs/build/axvisor/test.md` 与 `boot-debugging.md` 的 regex 描述与锚定原因。

**验证**：修复后真实 QEMU 运行 23s PASS；`=== SUCCESS PATTERN MATCHED ===` 前一行即嵌套 `SecCoreStartupWithStack(`（宿主 OVMF 的 SEC 在输出第 54 行，被正确跳过）。

### 提交 5 `093e0c1fa` docs(axvisor): document the fixed OVMF profile

**改动**：`.claude/skills/arch-platform-porting/references/boot-debugging.md`（+84）、`docs/docs/architecture/{axvisor-guest-machine,axvisor/overview,platform/testing}.md`、`docs/docs/build/{axvisor/test,ci}.md`（合计 +143/-9）。

**内容**：

- `boot-debugging.md` 新增 "x86_64 Guest OVMF / Fixed OVMF Bundle" 章节：固定 profile 常量表（两侧定义位置）、bundle 目录结构、verifier 说明（替代已删除的 `ovmf-profile.sh`）、`--firmware-bundle-path`/`--allow-unverified-firmware` CLI、`firmware_profile` VM 配置字段、`uefi = false` + 校验过的 pflash 接线、常见失败模式与调试命令；
- `axvisor/overview.md`：x86_64 行从 stub 占位更新为"VMX/SVM 运行时按 CPUID 选择 + 固定 OVMF UEFI guest 阶段 1 SEC 诊断已落地"，并说明 UEFI 固件字段与 `firmware_profile` 布局强制；
- `platform/testing.md`、`axvisor-guest-machine.md`、`axvisor/test.md`、`ci.md`：补充 `uefi` 测试分组布局、ovmf-entry 用例、固件参数与 non-gating 表述。

**为什么这么改**：`boot-debugging.md` 属于 arch-platform-porting 的 UEFI 调试参考（AGENTS.md 要求 UEFI 调试流程变更须同步），必须更新；不恢复已删除的 quick-start 文档与脚本；文档同步清单见 solve_conflict.md §5.1（design.md 同步已在实现前完成评审）。

## 5. 验证汇总

| 验证项 | 结果 |
|---|---|
| 分支 std 套件 `cargo xtask test` | 49 包全部通过：1184 passed / 0 failed / 7 ignored |
| axvm 单测 | 242 通过 |
| axdevice 单测 | 47 通过 |
| axvmconfig 单测 | 14 通过 |
| clippy（`cargo xtask clippy --package axbuild/axvm/axvmconfig/axdevice`） | 全部通过，零告警零 allow |
| `cargo fmt` | 通过 |
| ovmf-entry 用例发现 | `--list` 正确发现 `ovmf-entry-vmx`/`ovmf-entry-svm` |
| ovmf-entry 无 bundle 行为 | fail-fast 报错；`--list` 不要求 bundle |
| 真实 QEMU 运行 | 本机 AMD（SVM）：固定 DEBUG bundle 校验通过后 23s PASS，嵌套 SEC 匹配（含任务 2 装载日志 → VM boot → VCpu running → 嵌套 SEC 完整证据链）；首次运行暴露 success_regex 假阳性并已修复（提交 6） |
| 回归 | 现有 direct VMX/SVM smoke 与 dev 基线一致 |

每提交均经 subagent 全量评审（实跑 clippy/fmt/单测）→ fix → 复审闭环；`action=fix` 意见全部在实现轮内修复。

## 6. 遗留事项与 non-gating 说明

以下遗留项收敛于 `plan/collect_issue.md`（本分支评审遗留 Issue 清单），不阻塞本 PR 合入：

- **agent_review_bench flaky**（清单 #1，高）：`scripts/axbuild/src/agent_review_bench/sandbox.rs` 的 `mock_agents_write_artifacts_and_reject_invalid_json` 间歇失败，根因是 git maintenance 竞态 + 时间戳偏差；**非本分支引入**（分支 git diff 不涉及该模块）。CI 失败时按已知 flake 重跑确认；
- **VMX 硬件验证待完成**（清单 #6，高，提交 4/6 后更新）：SVM（AMD）已在本机真实 QEMU 验证通过（23s PASS，嵌套 SEC 匹配）；VMX（Intel）尚未验证。ovmf-entry 用例**保持 non-gating**，直到固定 DEBUG bundle 在 VMX（Intel）机器验证一次 SEC marker 后再升级 gating（`cargo xtask axvisor test qemu --arch x86_64 --test-group uefi --test-case ovmf-entry-vmx --firmware-bundle-path <bundle>`）；
- 其余低/中优先级文档与代码一致性项（清单 #2-#5）：overview.md 旧路径、boot-debugging.md 措辞精度、CI 矩阵变更时文档同步、固定常量跨 crate 重复定义（axvm 禁止依赖 axbuild，两处常量需人工保持同步，建议后续收敛单一来源或加编译期一致性校验）。

## 7. PR 标题与正文草稿

### 标题（英文，Conventional Commits）

```
feat(axvisor): add fixed OVMF SEC entry diagnostics for x86_64 UEFI guests
```

说明：本 PR 跨 axbuild/axvm/axvmconfig/axdevice/axvisor 五个 crate，无单一主导 crate，按 AGENTS.md 采用跨切面 scope；建议选 `axvisor`（承载用例、编排与文档主体，也是行为可见面）或 `docs`/`repo` 类宽 scope。备选：`feat(axvisor): verify fixed OVMF bundle and diagnose SEC entry (phase 1)`。

### 正文（中文，可直接粘贴 GitHub）

```markdown
## 解决的问题

AxVisor 的 x86_64 路径缺少可诊断的 UEFI guest 启动支持：没有可复现的固定资产（发行版 OVMF 版本漂移）、没有装载时的布局强制、没有统一的设备访问 trace、没有 SEC 到达的端到端测试门禁。UEFI 固件进入 SEC 后"走了哪一步、访问了什么"完全不可见，后续 fw_cfg/ACPI/PCI/virtio 平台发现与启动盘支持缺少诊断基础。

**本 PR**：为 x86_64 guest UEFI 建立阶段 1 的最小闭环——固定 OVMF 资产校验 + loader 布局强制 + 设备访问 trace + ovmf-entry SEC 到达诊断 + 真实验证暴露的 regex 锚定修复，共 6 个提交。本 PR 为后续 UEFI 支持（fw_cfg/ACPI/PCI/virtio 平台发现与启动盘路径）提供可诊断的基础。

## 改了什么

本 PR 沿"资产校验 → 装载强制 → 诊断观察 → 端到端用例 → 文档"五层建立阶段 1 闭环，外加一处真实验证暴露的修复：

- **固定 OVMF 资产校验（axbuild）**：新增 `scripts/axbuild/src/axvisor/ovmf.rs`，以 typed Rust verifier 替代已删除的 `ovmf-profile.sh`——`OvmfManifest`（`deny_unknown_fields` 解析）/`VerifiedOvmfBundle`/`FirmwareSource` 私有类型，`verify_firmware` 校验 manifest 逐字段值、文件大小与 SHA-256、VARS||CODE 拼接布局；CLI 新增 `--firmware-bundle-path` 与 `--allow-unverified-firmware`（未验证固件必须显式 opt-in 并打印哈希）。
- **x86 loader 布局强制（axvm/axvmconfig）**：`axvmconfig` 新增 `firmware_profile` 配置字段（非 UEFI boot 协议声明时拒绝）；x86 loader 在 fs 与内存两条 UEFI 装载路径上，仅当 profile 为固定值时强制 CODE 大小 `0x37c000`、GPA `0xffc84000`、reset vector `0xfffffff0` 并打印装载日志。未声明 profile 时行为与 dev 完全一致，现有 `arceos-uefi-smp1.toml`（`bios_load_addr = 0xffc0_0000`）不受影响。
- **设备访问 trace（axdevice）**：在 `DeviceRuntime::dispatch` 与 `handle_mmio_write_with_memory` 路径记录统一格式 trace（`device= bus= direction= addr= width= value= result=`），未注册地址仍返回 `NotFound` 并记为 `device=<unregistered>`，SysReg 不 trace；替换死代码 `log_device_io`。trace 只加日志不改行为，作为后续 UEFI 支持的诊断输入。
- **ovmf-entry 端到端用例（test-suit + axbuild runner）**：新增 `test-suit/axvisor/uefi/` 分组与 `ovmf-entry` 用例（VMX/SVM 变体仅 `-cpu` 不同，天然 non-gating）：QEMU `-kernel` 启动 Axvisor 宿主（`uefi = false` 阻断 ostool 下载自带 OVMF），Axvisor 加载嵌套 OVMF 作为 UEFI guest 固件，success_regex 匹配嵌套 VM 启动后的 `SecCoreStartupWithStack(`。runner 消费 `--firmware-bundle-path`：校验过的 CODE 经 `include_bytes!` 嵌入 VM 配置并作为 QEMU pflash unit 0（VARS 每运行独立拷贝），两层固件字节级相同；无 bundle 时 fail-fast，绝不静默 fallback 到发行版 OVMF；`--list` 不要求 bundle。新增专用 VM 配置 `os/axvisor/configs/vms/qemu/x86_64/ovmf-entry.toml`。
- **文档（docs + skill reference）**：`boot-debugging.md` 新增固定 OVMF 章节（常量/CLI/接线/失败模式），`axvisor/overview.md` x86_64 更新为 VMX/SVM + 固定 OVMF UEFI 状态，`axvisor/test.md`/`ci.md` 等补充 `uefi` 分组与固件参数。
- **修复：success_regex 锚定嵌套 VM**：真实 QEMU 验证发现 QEMU 层宿主 OVMF（为引导 axvisor 注入的 pflash）与嵌套 OVMF 输出相同的 SEC 行，裸 regex 847ms 即误匹配宿主层；改为 `(?s)VCpu[0] running...` 前缀锚定嵌套层，修复后真实 QEMU 23s PASS。

## 每步的逻辑

- **先校验后装载（提交 1 → 2）**：资产校验在构建编排层完成（替代 shell 脚本），loader 只做装载时的布局强制；两层都以固定常量为准，任何一层发现不匹配都显式报错而非回退到发行版 OVMF。校验作用域收窄到"profile 显式声明"——这是不破坏 dev 现存 `arceos-uefi-smp1.toml` 的前提。
- **trace 只加日志不改行为（提交 3）**：SEC 到达后的下一个问题必然是"OVMF 接下来访问了什么"，trace 作为后续 UEFI 支持的诊断输入；`result=<ok|error>` 保留失败可见性，为后续设备实现提供首个未支持访问的定位。
- **固定资产接线用 pflash 而非 ostool（提交 4）**：ostool 的 `uefi = true` 会下载 `edk2-stable202508-r1` 并注入 FAT ESP，验证的不是本 PR 固定的 `edk2-stable202605` 资产；因此 `uefi = false` + 校验过的 CODE/VARS 以 pflash 注入，成功判定只来自 guest COM1 的 SEC marker。
- **用例保持 non-gating**：SEC marker 的 VMX/SVM 两机真实硬件证据尚未齐备，合入后继续跟踪（见遗留事项），证据齐全后再升级 gating。

## 验证

- 分支 std 套件 `cargo xtask test`：49 包 1184 passed / 0 failed / 7 ignored；axvm 242、axdevice 47、axvmconfig 14 全过；
- `cargo xtask clippy --package axbuild/axvm/axvmconfig/axdevice` 零告警零 allow；`cargo fmt` 通过；
- ovmf-entry：`--list` 正确发现 vmx/svm 变体；无 bundle 时 fail-fast；
- 真实 QEMU（SVM，本机 AMD）：固定 DEBUG bundle 校验通过后 23s PASS，嵌套 SEC 匹配（提交 6 修复后）；
- 每提交经 subagent 全量评审（实跑 clippy/fmt/单测）→ fix → 复审闭环。

### 手动部署固件并测试

以下为在本机复现 ovmf-entry 端到端验证的步骤（已在 AMD/SVM 环境实测通过）：

1. **构建固定 DEBUG OVMF**（需要 edk2 源码，`edk2-stable202605` tag）：

   ```bash
   cd <edk2>
   git checkout edk2-stable202605
   # 构建 OvmfPkgX64 DEBUG/GCC/4MB/DEBUG_ON_SERIAL_PORT，产物在 Build/OvmfX64/DEBUG_GCC/FV/
   # 关键产物: OVMF_CODE.fd (0x37c000) / OVMF_VARS.fd (0x84000) / OVMF.fd (0x400000)
   ```

2. **组装 bundle 目录**（verifier 要求的格式：三个文件 + manifest.toml）：

   ```bash
   mkdir -p <bundle-dir>
   cp <edk2>/Build/OvmfX64/DEBUG_GCC/FV/{OVMF_CODE.fd,OVMF_VARS.fd,OVMF.fd} <bundle-dir>/
   # manifest.toml 需按 scripts/axbuild/src/axvisor/ovmf.rs 的字段契约填写:
   #   profile = "qemu_x86_64_axvisor_ovmf_debug", edk2_tag = "edk2-stable202605",
   #   edk2_commit = <实际 commit>, architecture/target/toolchain/platform, 全部 GPA/大小常量,
   #   code/vars/combined 的 _sha256(用 sha256sum 计算), 布尔开关, 5 个 boot marker。
   # 用 sha256sum <bundle-dir>/*.fd 计算哈希填入。
   ```

3. **运行 ovmf-entry 用例**（本机 AMD 用 SVM 变体，Intel 用 VMX）：

   ```bash
   cd <tgoskits>
   cargo xtask axvisor test qemu --arch x86_64 --test-group uefi \
     --test-case ovmf-entry-svm --firmware-bundle-path <bundle-dir>
   ```

4. **预期结果**：`ok: ovmf-entry-svm`（约 20-30s）。日志中可见完整嵌套链路证据：

   ```
   selected firmware profile: qemu_x86_64_axvisor_ovmf_debug ... verification_label=VERIFIED
   Loading fixed OVMF CODE profile <memory> (3653632 bytes) into GPA @0xffc84000 with reset vector 0xfffffff0
   VM[1] boot success
   VM[1] VCpu[0] running...
   SecCoreStartupWithStack(          ← 嵌套 OVMF SEC, success_regex 锚定点
   === SUCCESS PATTERN MATCHED: (?s)VCpu\[0\] running\.\.\..*SecCoreStartupWithStack ===
   ```

   若 `success_regex` 在 1 秒内即匹配（约 847ms），说明匹配到了宿主 OVMF 的 SEC 而非嵌套层——检查用例 TOML 的 regex 是否带 `VCpu[0] running...` 前缀锚定。

   **注意**：OVMF 必须为 DEBUG 构建（含 `SecCoreStartupWithStack(` 输出）；RELEASE 构建无此 SEC marker，用例会超时失败。

## 遗留事项

- `agent_review_bench` 已知 flaky（非本分支引入，建议 CI 失败时按已知 flake 单独重跑确认）；
- VMX（Intel）硬件验证未完成（SVM 已在本机 AMD 验证通过），ovmf-entry 保持 non-gating，VMX 证据齐全后再升级 gating。
```

## 8. 合入前检查（复用 solve_conflict.md §7 验收标准中适用于本 PR 的项）

- [ ] 分支仅含 §1 的 6 个提交；
- [ ] 合入前刷新远端并复核 `dev` 是否再次变化（方案基于本地 `dev@4c7aa8577`）；
- [ ] `plan/uefi/design.md`/`solve_conflict.md` 与实现一致，`collect_issue.md` 已跟踪遗留项；
- [ ] 无源码文本断言测试残留；未引入任何已删除的脚本/NimbOS 配置/guest-owned COM1 声明；
- [ ] 现有 `arceos-uefi-smp1.toml` 未被破坏；现有 direct VMX/SVM smoke 回归通过；
- [ ] 阶段 1 未混入阶段 2-5 内容（ACPI/PCI/virtio/中断）。
