# PR 1：ovmf-entry 切换上游 RELEASE OVMF（uefi-p0-r2 阶段 1）— 评审与合入文档

> 文档标识：`pr1-ovmf-upstream-fixes`。
> 本文档配套 `plan/uefi/design.md`（设计）与 `plan/uefi/solve_conflict.md`（方案），面向 `feat/uefi-p0-r2` 分支合并到 `dev` 的评审、验证记录与 PR 素材。
> 分支：`feat/uefi-p0-r2`（自 `dev@343d47f4e` 新建，8 个提交），目标分支：`dev`。
> 范围：**阶段 1（固定 OVMF 资产 + 装载校验 + 嵌套 OVMF 启动判定）**。阶段 2-5（ACPI/PCI/virtio/中断）保留为 `plan/uefi/design.md` 中的路线图，不在本 PR 范围内。

## 1. 元信息

| 项 | 值 |
|---|---|
| 分支 | `feat/uefi-p0-r2` |
| 基点 | `dev@343d47f4e`（`fix(axdevice): correct fw_cfg DMA fault handling (#1918)`） |
| 目标分支 | `dev` |
| 提交数 | 8（全部独立，无 merge；提交 8 为替换提交，将自建 DEBUG bundle 时代切换为上游 RELEASE 时代） |
| 净变更 | 21 文件，+2312/-25（`git diff 343d47f4e..20362422c` 实测） |

| # | 提交 | 标题 | 改动 |
|---|---|---|---|
| 1 | `29a97e0af` | feat(axbuild): verify fixed OVMF debug bundles | `scripts/axbuild/src/axvisor/ovmf.rs`（新增 1058 行）、`scripts/axbuild/src/axvisor/mod.rs`（+50） |
| 2 | `a7ceb067b` | feat(axvm): enforce the fixed x86 OVMF CODE layout | `virtualization/axvm/src/arch/x86_64/boot/mod.rs`（+176）、`virtualization/axvmconfig/src/{error,lib,templates,test}.rs`（+87） |
| 3 | `815aa25ba` | feat(axdevice): trace guest device accesses in DeviceRuntime | `virtualization/axdevice/src/device.rs`（+384/-11） |
| 4 | `10df1c62e` | test(axvisor): add fixed OVMF entry diagnostics | `os/axvisor/configs/vms/qemu/x86_64/ovmf-entry.toml`（新增 69 行）、`scripts/axbuild/src/axvisor/test/qemu.rs`（+753/-5）、`test-suit/axvisor/uefi/ovmf-entry/`（4 个 TOML 新增） |
| 5 | `43554dda5` | docs(axvisor): document the fixed OVMF profile | `.claude/skills/arch-platform-porting/references/boot-debugging.md`（+84）、`docs/docs/architecture/`（3 个）、`docs/docs/build/`（2 个，合计 +143/-9） |
| 6 | `71f7fab39` | fix(axvisor): anchor ovmf-entry SEC match to the nested VM | `test-suit/axvisor/uefi/ovmf-entry/qemu-x86_64-{vmx,svm}.toml`（success_regex 锚定）、`docs/docs/build/axvisor/test.md`、`boot-debugging.md`（+20/-8） |
| 7 | `10ec5d4bb` | fix(axvisor): reject unverified firmware for ovmf-entry cases | `scripts/axbuild/src/axvisor/test/qemu.rs`（+27） |
| 8 | `20362422c` | feat(axvisor): switch ovmf-entry to upstream RELEASE OVMF（替换提交） | `scripts/axbuild/src/axvisor/ovmf.rs`（+151/-720，删除 manifest 校验）、`virtualization/axdevice/src/fw_cfg/pio.rs`（+12，fw_cfg marker）、`test-suit/axvisor/uefi/ovmf-entry/`（2 个 TOML）、`os/axvisor/configs/vms/qemu/x86_64/ovmf-entry.toml`、`scripts/axbuild/src/axvisor/{mod,test/qemu}.rs`、`docs`（合计 +272/-826） |

## 2. 问题背景：x86_64 guest UEFI 启动诊断缺失

AxVisor 的 x86_64 路径缺少可诊断的 UEFI guest 启动支持：OVMF 固件从 reset vector 进入 SEC 后，AxVisor 无法确定"固件走到了哪一步"——没有可复现的固定资产（自建 DEBUG OVMF 构建不可复现、RELEASE 构建无串口 SEC marker 无法判定到达）、没有装载时的布局强制（CODE 大小/GPA/reset vector 无校验）、没有统一的设备访问 trace（无法定位固件首次未支持的平台访问）、没有嵌套固件启动的端到端测试门禁。这使后续 UEFI 功能（fw_cfg/ACPI/PCI/virtio，见 `plan/uefi/design.md` 阶段 2-5）缺少诊断基础。

同时，dev 现有边界对实现提出了明确约束（见 `plan/uefi/design.md` §4）：

- **串口为 machine-owned 资源**：x86 machine profile 固定提供 16550 COM1（`virtualization/axvm/src/machine/mod.rs:81` 的 `x86_64_profile()`，PIO `0x3f8` + IRQ 4），并明确禁止 `emu_devices`/`serial` 字段——启动判定不能通过 guest TOML 注册 COM1 或新增串口截获代码；
- **配置模型为 `guest_type = "passthrough"` + `[devices]`**：旧 `vm_type = 1`/`emu_devices = [...]` 配置模型已失效，新用例须按新模型编写；
- **`Device` trait 在 `axdevice_base`**（`name/resources/access`），`DeviceRuntime::dispatch_optional` 是统一入口（MMIO/PIO/SysReg lookup + trace，memory-aware 写路径经 `handle_*_with_memory`）——设备访问 trace 须按新 API 实现；
- **fw_cfg 设备不在 x86 machine profile**：由 x86 装载路径在 `plan_devices` 中经 `FwCfgPayloadFactory::deferred_pio(0x510, 0x0c, ...)` 注册（`virtualization/axvm/src/arch/x86_64/vm.rs:102`），fw_cfg 访问 marker 挂在 `FwCfgPioDevice::access`；
- **测试规范收紧**：禁止源码文本断言（`book/guideline/code-quality.md` §16.6），校验测试须编译并调用真实 API；
- **脚本/用例已删除**：`d7050469f`（#1866）删除了 `setup_qemu.sh`/`quick-start.sh`/`setup_qemu_lvz.sh`/`ci_run_qemu_nimbos.py` 等脚本、NimbOS VM 配置与 qemu-nimbos 用例，固件校验职责与测试用例须迁入 Rust 编排层与新的 `test-suit/axvisor/` 分组。

## 3. 方案概述

按 `plan/uefi/solve_conflict.md` §4 的提交顺序实施（提交 6-7 为真实验证暴露的修复提交，提交 8 为替换提交：把固定资产从自建 DEBUG bundle 切换为 ostool 上游 RELEASE OVMF），实现前先同步 `plan/uefi/design.md`，保证实现与已评审设计一致。

**阶段 1 范围**：只建立"上游 RELEASE OVMF 资产 + 固件装载校验 + 嵌套 OVMF 启动判定"的最小闭环。**不实现** fw_cfg 数据面/ACPI/PCI/virtio（阶段 2-5）。嵌套 OVMF 启动判定以设备模型打印的 fw_cfg 访问 marker 为准（`success_regex`），不新增串口截获代码。

每提交独立走"实现 → subagent 全量评审（实跑 clippy/fmt/单测）→ fix → 复审"闭环；评审意见按 `action=fix`（实现轮内修复）/`action=issue`（收敛到 `plan/collect_issue.md` 跟踪）分级。

## 4. 各提交说明

### 提交 1 `29a97e0af` feat(axbuild): verify fixed OVMF debug bundles

**改动**：`scripts/axbuild/src/axvisor/ovmf.rs`（新增 1058 行）、`scripts/axbuild/src/axvisor/mod.rs`（+50）。

**内容**（以下为最终状态；该校验形态由提交 8 从 manifest 校验演进为 ostool 上游目录校验，见提交 8 说明）：

- 固定 profile 常量集中定义：`OVMF_PROFILE_NAME = "qemu_x86_64_axvisor_ovmf_debug"`（**ovmf.rs 主定义**；axvm 侧 `boot/mod.rs::FIXED_OVMF_PROFILE` 仍重复该值且 axvm 禁止依赖 axbuild，两处需人工保持同步，见 §6）、`OVMF_CODE_BASE = 0xffc8_4000`、`OVMF_CODE_SIZE = 0x37_c000`、`OVMF_VARS_BASE = 0xffc0_0000`、`OVMF_VARS_SIZE = 0x8_4000`、`OVMF_COMBINED_SIZE = 0x40_0000`、`OVMF_RESET_VECTOR = 0xffff_fff0`，以及上游文件名常量 `UPSTREAM_CODE_FILE = "code.fd"`/`UPSTREAM_VARS_FILE = "vars.fd"`；
- 私有类型：`FirmwareSource` 枚举（`Upstream { code_path, vars_path }`——ostool cache 目录，含 `code.fd`/`vars.fd`；`UnverifiedLocal { code_path }`——显式 opt-in 的本地未验证固件，避免布尔参数扩散）、`VerifiedOvmfBundle`（含 `code_end()`/`reset_vector_in_code_window()` 谓词），入口 `verify_firmware(source)`；
- 校验链（上游目录）：解析目录 → 校验 `code.fd`/`vars.fd` 存在 → 逐文件大小校验（`code.fd` = `0x37c000`、`vars.fd` = `0x84000`）→ 校验 reset vector 落在 CODE 窗口内 → 打印 profile、GPA/大小、SHA-256 与 `verification_label=VERIFIED`。**SHA-256 由 ostool 自身校验**（ostool `Prebuilt::fetch` 以 `source.sha256` 核对下载物，cache 内以 `<prebuilt_dir>/sha256` 判定是否过期），本模块不做 SHA-256 比对与 byte-for-byte VARS||CODE 拼接校验；校验逻辑不下载任何东西；
- 未验证本地固件必须 `--allow-unverified-firmware` 显式 opt-in，仍须匹配固定 CODE 大小，SHA-256 仅打印供参考（`verification_label=UNVERIFIED`）；
- CLI：`cargo xtask axvisor test qemu` 新增 `--firmware-bundle-path <dir>`（指向 ostool cache 目录，含 `code.fd`/`vars.fd`）与 `--allow-unverified-firmware`。

**为什么这么改**：替代旧 `ovmf-profile.sh`（`uefi/p0` 分支 `941134916` 引入的 `os/axvisor/scripts/ovmf-profile.sh`；`d7050469f` 已整体删除 `os/axvisor/scripts/` 目录，脚本不恢复，校验迁入 Rust 编排层）的校验职责；上游 RELEASE 固件的真实性由 ostool（`edk2-stable202508-r1`，`Source::LATEST`）保证，本模块只做与 x86 loader 一致的布局防御性校验，保证 CI 不会静默使用布局不匹配的固件。

**验证**：9 个 host 单测覆盖上游目录校验通过、错误 code/vars 大小拒绝、缺 `code.fd`/`vars.fd` 拒绝、未验证固件缺 opt-in 拒绝、opt-in 接受并打印 SHA-256、opt-in 仍强制 CODE 大小、路径不存在拒绝、`None` 无 flag 接受。

### 提交 2 `a7ceb067b` feat(axvm): enforce the fixed x86 OVMF CODE layout

**改动**：`virtualization/axvm/src/arch/x86_64/boot/mod.rs`（+176）、`virtualization/axvmconfig/src/{error,lib,templates,test}.rs`（+87）。

**内容**：

- `axvmconfig` 新增 `VMKernelConfig::firmware_profile: Option<String>` 配置字段（TOML 键 `firmware_profile`）；校验规则：声明 `firmware_profile` 但 boot 协议非 UEFI 时返回新错误变体 `AxVmConfigError::FirmwareProfileRequiresUefi`（`error.rs` +6）；模板默认 `None`；
- x86 loader 在 `load_uefi_from_configured_path`（fs 路径）与 `load_boot_image_from_memory`（内存镜像路径）两条路径上，当 `fixed_ovmf_profile_enabled`（profile 等于固定值 `FIXED_OVMF_PROFILE`）时先做布局校验再装载：CODE 大小 `0x37c000`、装载 GPA `0xffc84000`、reset vector `0xfffffff0`（fs 路径先经 `BootImageProvider` 查大小）；成功装载打印 profile、路径、大小、GPA、reset vector；multiboot/direct boot 路径不受影响。

**为什么这么改**：校验必须**只在固定 profile 显式启用时生效**——dev 现存 `arceos-uefi-smp1.toml` 以 `bios_load_addr = 0xffc0_0000` 加载 OVMF_CODE.fd，若无条件拒绝非 `0xffc84000` 的装载会破坏现有配置（solve_conflict.md §4 提交 2 的作用域说明）。默认（无 profile）行为与 dev 完全一致。

**验证**：axvmconfig 新单测（默认 None、显式解析、非 UEFI 拒绝、UEFI 接受）；axvm loader 校验走真实 API 的单测（非源码文本断言）。

### 提交 3 `815aa25ba` feat(axdevice): trace guest device accesses in DeviceRuntime

**改动**：`virtualization/axdevice/src/device.rs`（+384/-11）。

**内容**：

- 统一 trace 点：`DeviceRuntime::dispatch_optional`（MMIO/PIO/SysReg 的统一 lookup 入口，覆盖 memory-aware 写路径）内加 `trace_device_access`，输出格式 `guest device access: device=<name> bus=PIO|MMIO direction=read|write addr=<addr:#x> width=<width> value=<value> result=<ok|error>`；
- 未注册地址仍返回 `NotFound` 并记录为 `device=<unregistered>`，不伪造返回值；SysReg 访问视为高频噪声不 trace；替换死代码 `log_device_io`（含其 `allow(dead_code)`）。

**为什么这么改**：按 `axdevice_base` 新 `Device`/`BusAccess` API 实现设备访问 trace；trace 只加日志、不改行为，作为阶段 2+ 的"OVMF 接下来访问了什么"的诊断输入（design.md §7 阶段 1 工作项 4）。

**验证**：`device.rs` 单测覆盖注册设备 read/write/error 结果不变、未注册设备四种宽度两种 bus、越界端口地址——断言 trace 点不改变既有访问结果。

### 提交 4 `10df1c62e` test(axvisor): add fixed OVMF entry diagnostics

**改动**：`os/axvisor/configs/vms/qemu/x86_64/ovmf-entry.toml`（新增 69 行）、`scripts/axbuild/src/axvisor/test/qemu.rs`（+753/-5）、`test-suit/axvisor/uefi/ovmf-entry/` 4 个 TOML（build-x86_64-unknown-none-{vmx,svm}.toml + qemu-x86_64-{vmx,svm}.toml）。

**内容**：

- 新增 `test-suit/axvisor/uefi/` 测试分组（dev 发现机制自动发现 `test-suit/axvisor/` 下所有目录，非默认 `normal` 组天然 non-gating）；用例 `ovmf-entry`，按 build wrapper 变体生成 `ovmf-entry-vmx`/`ovmf-entry-svm` 两个用例名；VMX/SVM 的 qemu 参数仅 `-cpu` 一行不同（`-cpu host,+vmx-ept,…` vs `-cpu host,+svm,+npt,+nrip-save`），其余参数相同；`-cpu host` 是必需的：QEMU 未指定 `-cpu` 时默认为 `qemu64`（不匹配宿主 CPU 特性），ovmf-entry 的 VMX/SVM 变体需要相应 CPU 特性（`+vmx-*`/`+svm,+npt,+nrip-save`），且嵌套固件依赖 host CPUID 透传；
- 启动链路：QEMU `-kernel` 加载 Axvisor 宿主（`uefi = false`，阻止 ostool 注入自带 FAT ESP），Axvisor 再把嵌套 OVMF 作为 UEFI guest 固件从 VM 配置加载；`success_regex` 匹配嵌套固件启动早期的设备模型 marker（最终形态由提交 8 定为 fw_cfg marker，`-serial stdio` 原始字节透传）；
- **固定资产接线（关键约束，solve_conflict.md §5.3）**：runner 消费 `--firmware-bundle-path`：校验过的 `code.fd` 经 `include_bytes!` 嵌入生成的 per-run VM 配置（`image_location = "memory"`，`kernel_path`/`uefi_firmware_path` 指向同一文件），同时以 `-drive if=pflash,format=raw,unit=0,readonly=on,file=<code.fd>` + unit=1（VARS 每运行复制一份，保证隔离）注入 QEMU 层——宿主与嵌套两层固件字节级相同；VARS 隔离靠每运行独立副本本身（ovmf-entry 用例 `uefi = false`，不享受 `qemu.uefi` 路径经 `apply_drive_snapshot_without_global_snapshot` 的 per-drive snapshot；`wire_ovmf_entry_qemu_firmware` 注释明确该点）；
- 无 bundle 时 fail-fast：`verify_cli_firmware_bundle` 在 discovery 之后、任何 build/QEMU 工作之前执行；ovmf-entry 用例无 `--firmware-bundle-path` 时直接报错退出（绝不静默 fallback 到发行版 OVMF，也不会挂死在 `Booting from ROM..`）；`--list` 早于 bundle 校验返回，列出用例不需要 bundle；
- 用例前缀防御：`ensure_ovmf_entry_case_dir` 要求用例目录精确等于 `ovmf-entry`，防止未来任何前缀同名的用例误接固定资产；
- 新增 `os/axvisor/configs/vms/qemu/x86_64/ovmf-entry.toml` 专用 VM 配置（`guest_type = "passthrough"`、1 vCPU、`entry_point = 0xffff_fff0`、4 MiB 覆盖 `0xffc0_0000-0x100000000` 的固件窗口内存区、`boot_protocol = "uefi"`、`bios_load_addr = 0xffc8_4000`、`firmware_profile = "qemu_x86_64_axvisor_ovmf_debug"`），checked-in 回退路径 `image_location = "fs"` + `kernel_path`/`uefi_firmware_path = "/guest/ovmf/OVMF_CODE.fd"`（rootfs 中不存在该文件，无 bundle 时 VM 以明确错误初始化失败，而不是静默引导发行版 OVMF；有 bundle 时 runner 把 `image_location` 改写为 `"memory"` 并将 `kernel_path`/`uefi_firmware_path` 指向已验证的 `code.fd`）。

**为什么这么改**：使嵌套 OVMF 启动判定接入最新测试编排，与 dev 现有 `test-suit/axvisor/normal/qemu/` 布局一致；不声明 COM1/emu_devices、不恢复任何已删除脚本与用例；保持 non-gating（见 §6）。

**验证**：

- `--list` 正确发现 `ovmf-entry-vmx`/`ovmf-entry-svm` 两个变体；
- 无 bundle 时 fail-fast 报错，且 `--list` 不要求 bundle；
- 真实 QEMU 运行见 §5（提交 6 曾暴露 success_regex 假阳性，提交 8 以 fw_cfg marker 彻底取代 SEC marker，见提交 6/8 说明）。

### 提交 5 `43554dda5` docs(axvisor): document the fixed OVMF profile

**改动**：`.claude/skills/arch-platform-porting/references/boot-debugging.md`（+84）、`docs/docs/architecture/{axvisor-guest-machine,axvisor/overview,platform/testing}.md`、`docs/docs/build/{axvisor/test,ci}.md`（合计 +143/-9）。

**内容**：

- `boot-debugging.md` 新增 "x86_64 Guest OVMF" 章节：固定 profile 常量表（两侧定义位置）、上游固件目录结构（ostool cache：`code.fd`/`vars.fd`）、verifier 说明（替代旧 `ovmf-profile.sh` 的校验职责）、`--firmware-bundle-path`/`--allow-unverified-firmware` CLI、`firmware_profile` VM 配置字段、`uefi = false` + 校验过的 pflash 接线、常见失败模式与调试命令；
- `axvisor/overview.md`：x86_64 行从 stub 占位更新为"VMX/SVM 运行时按 CPUID 选择 + 固定 OVMF UEFI guest 阶段 1 诊断已落地"，并说明 UEFI 固件字段与 `firmware_profile` 布局强制；
- `platform/testing.md`、`axvisor-guest-machine.md`、`axvisor/test.md`、`ci.md`：补充 `uefi` 测试分组布局、ovmf-entry 用例、固件参数与 non-gating 表述（提交 8 再次更新 `test.md` 与 `boot-debugging.md` 的 marker 与命令描述）。

**为什么这么改**：`boot-debugging.md` 属于 arch-platform-porting 的 UEFI 调试参考（AGENTS.md 要求 UEFI 调试流程变更须同步），必须更新；不恢复已删除的 quick-start 文档与脚本；文档同步清单见 solve_conflict.md §5.1（design.md 同步已在实现前完成评审）。

### 提交 6 `71f7fab39` fix(axvisor): anchor ovmf-entry SEC match to the nested VM

**背景**：真实 QEMU 验证发现提交 4 的 success_regex 缺陷——QEMU 层为引导 Axvisor 宿主注入的 pflash OVMF 与嵌套 OVMF 输出**相同的** SEC 串口行（文本完全一致），裸 regex 匹配到宿主层，847ms 即假 PASS，嵌套 VM 尚未启动。

**改动**：`test-suit/axvisor/uefi/ovmf-entry/qemu-x86_64-{vmx,svm}.toml` 的 success_regex 改为 `(?s)VCpu\[0\] running\.\.\..*SecCoreStartupWithStack`（`(?s)` 跨行；仓库中 `qemu-backtrace-panic`/`qemu-panic-no-backtrace` 用例同语法先例位于 `test-suit/axvisor/normal/` 下且仅有 aarch64/riscv64 变体，无 x86_64 变体，x86_64 上 `(?s)` 的先例实为 ovmf-entry 自身），锚定到嵌套 VM 启动（`VCpu[0] running...` 仅嵌套层输出）；同步更新 `docs/docs/build/axvisor/test.md` 与 `boot-debugging.md` 的 regex 描述与锚定原因。

**验证**：修复后真实 QEMU 运行 23s PASS；`=== SUCCESS PATTERN MATCHED ===` 前一行即嵌套 SEC 行（宿主 OVMF 的 SEC 行在输出第 54 行，被正确跳过）。

> **历史注记**：该前缀锚定方案在本分支后续被提交 8 取代——切换上游 RELEASE OVMF 后 success_regex 改为 fw_cfg 访问 marker（`(?m)^.*Nested OVMF fw_cfg accessed`）。marker 由 axvisor 设备模型打印，天然只属嵌套 guest（宿主 OVMF 的 fw_cfg 访问发生在 QEMU 内部，不经过 axvisor 设备模型），**无需 VM 前缀锚定**，也不受 2048 字节匹配窗口限制。本文档保留提交 6 的历史记录，最终行为以提交 8 为准。

### 提交 7 `10ec5d4bb` fix(axvisor): reject unverified firmware for ovmf-entry cases

**背景**：提交 1 引入 `--allow-unverified-firmware` opt-in 后，runner 仍会接受 `verified == false` 的固件并让它驱动 ovmf-entry 用例结论，绕过"固定资产验证过"的保证（评审意见，`action=fix`）。

**改动**：`verify_cli_firmware_bundle` 在解析并校验固件后，若 `bundle.verified == false`（本地未验证 `code.fd` 且带 opt-in）直接 `bail!` 拒绝——未验证固件是诊断专用逃生通道，不能决定 UEFI 测试结果；任何 QEMU 接线之前拒绝。新增单测 `verify_cli_firmware_bundle_rejects_unverified_local_firmware` 覆盖该拒绝路径。

**验证**：单测覆盖拒绝（错误信息含 `UNVERIFIED` 与 `cannot drive`）；真实 QEMU 运行不受影响。

### 提交 8 `20362422c` feat(axvisor): switch ovmf-entry to upstream RELEASE OVMF（替换提交）

**背景**：自建固定 DEBUG OVMF bundle（`edk2-stable202605` 本地构建 + manifest.toml）不可复现、维护成本高；且 DEBUG 构建才有 `SecCoreStartupWithStack(` 串口 SEC marker，RELEASE 构建无此输出，用例无法判定。dev 已有 ostool 管理的上游 RELEASE OVMF（`edk2-stable202508-r1`，`Source::LATEST` 缓存，`cargo xtask ovmf --arch x86_64` 一步下载，SHA-256 由 ostool 自身校验），且嵌套固件的 fw_cfg 访问必然经过 axvisor 设备模型——本提交把固定资产切换到 ostool 上游目录（`code.fd`/`vars.fd`），成功判据改为设备模型打印的 fw_cfg 访问 marker。

**改动**（9 文件，+272/-826）：

- `scripts/axbuild/src/axvisor/ovmf.rs`（+151/-720）：**删除** `OvmfManifest`（serde/TOML 解析、`deny_unknown_fields`）、逐字段值校验（profile/edk2 tag/commit/boot markers）、文件 SHA-256 比对、VARS||CODE byte-for-byte 拼接布局校验（ostool 拥有校验职责）；**保留**布局防御性校验（`code.fd`/`vars.fd` 存在 + 大小 + reset vector 窗口），布局与上游 RELEASE 完全一致；`OVMF_PROFILE_NAME` 收敛为 `ovmf.rs` 主定义（删除了 `OVMF_EDK2_TAG`/`OVMF_EDK2_COMMIT`/`CODE_FILE`/`VARS_FILE`/`COMBINED_FILE`/`MANIFEST_FILE` 等 manifest 时代常量，新增 `UPSTREAM_CODE_FILE`/`UPSTREAM_VARS_FILE`；axvm 侧 `FIXED_OVMF_PROFILE` 仍重复该值，需人工保持同步）；
- `virtualization/axdevice/src/fw_cfg/pio.rs`（+12）：`FwCfgPioDevice::access` 入口处加 Info 级 marker：

  ```text
  Nested OVMF fw_cfg accessed (port=…, dir=…, sel=…, window=…)
  ```

  嵌套固件启动早期访问 fw_cfg（PIO `0x510`/`0x511`/`0x514`，经 x86 `deferred_pio` 注册的 `FwCfgPioDevice`）时打印；Info 级保证默认日志级别可见、可被 success_regex 匹配；
- `test-suit/axvisor/uefi/ovmf-entry/qemu-x86_64-{vmx,svm}.toml`：success_regex 从 `(?s)VCpu\[0\] running\.\.\..*SecCoreStartupWithStack` 换为 `(?m)^.*Nested OVMF fw_cfg accessed`。**marker 天然只属嵌套 guest**：宿主 OVMF（QEMU 层 pflash，引导 Axvisor 宿主二进制）的 fw_cfg 访问发生在 QEMU 内部，不经过 axvisor 设备模型，因此无需 VM 前缀锚定，也不受 2048 字节流式匹配窗口限制（`scripts/axbuild/src/support/qemu_success.rs` 的 StreamingMatcher 保留 `TRANSCRIPT_TAIL_BYTES = 2048` 尾部并只做流式 DFA 匹配）。两个变体的 QEMU 参数在 `-cpu` 一行显式传 `host`（`+vmx-ept,…` vs `+svm,+npt,+nrip-save`）：`-cpu host` 把宿主 CPU 特性与 CPUID 透传给 QEMU 虚拟机，ovmf-entry 的 VMX/SVM 变体需要相应 CPU 特性（`+vmx-*`/`+svm,+npt,+nrip-save`），且嵌套固件依赖 host CPUID 透传——QEMU 未指定 `-cpu` 时默认为 `qemu64`，缺少 VMX/SVM 及嵌套固件所需的特性，会直接导致用例无法运行；
- `scripts/axbuild/src/axvisor/{mod.rs,test/qemu.rs}`、`os/axvisor/configs/vms/qemu/x86_64/ovmf-entry.toml`、`docs`（`boot-debugging.md`/`test.md`）：CLI help、接线注释与文档从 manifest bundle 表述更新为 ostool cache（`code.fd`/`vars.fd`）表述；`boot-debugging.md` 章节改名 "Verified Upstream Firmware"，调试命令改为 `cargo xtask ovmf --arch x86_64` 一步下载 + `--firmware-bundle-path <ostool-cache-dir>`。

**为什么这么改**：上游 RELEASE 固件由 ostool 发布与校验（可复现、免自建），marker 走 axvisor 设备模型使成功判据与"嵌套固件真正执行并访问平台设备"直接对应，比串口 SEC marker 更强且 RELEASE 可用；去掉 VM 前缀锚定与匹配窗口限制使判定更简单可靠。

**验证**：真实 QEMU 运行——`ovmf-entry-vmx` PASS 6.97s、`ovmf-entry-svm` PASS 6.80s（fw_cfg marker 匹配，提交 8 验证记录）；`ovmf.rs` 9/9、`qemu.rs` 14/14 单测通过；`cargo fmt` 通过；clippy 零告警（详见 §5）。

## 5. 验证汇总

| 验证项 | 结果 |
|---|---|
| axbuild `ovmf.rs` 单测 | 9/9 通过（`cargo test -p axbuild --lib 'axvisor::ovmf::tests'` 实测：上游目录、错误 code/vars 大小、缺 `code.fd`/`vars.fd`、未验证 opt-in 拒绝/接受、opt-in 仍强制 CODE 大小、路径不存在、`None` 无 flag） |
| axbuild `qemu.rs` 单测 | 14/14 通过（`cargo test -p axbuild --lib 'axvisor::test::qemu::tests'` 实测：固件 CLI 校验 3 例、pflash 接线 4 例、VM 配置改写 3 例、target 路径、无 bundle fail-fast、缺 VARS 拒绝、前缀误匹配拒绝、case 目录精确校验） |
| axbuild lib 全量 | 876 passed / 3 failed（`cargo test -p axbuild --lib` 实测，20362422c 实现轮）；3 个失败均为**预存基线问题**：`axvisor::rootfs::tests` 的 `patch_qemu_rootfs_overrides_rootfs_when_vmconfig_provides_one`/`patch_qemu_rootfs_uses_unified_rootfs_by_default` 两例已在干净 dev（343d47f4e）worktree 复现相同失败（9 passed / 2 failed），属 dev 基线环境问题，与本分支改动无关；`agent_review_bench::tests::mock_agents_write_artifacts_and_reject_invalid_json` 为既有 flaky，隔离重跑通过（全量复跑一次为 877/2，即 flaky 例恢复）。聚焦跑 `axvisor::ovmf` 9/9、`axvisor::test::qemu` 14/14 全部通过 |
| axvm 单测 | 256 通过（`cargo test -p axvm --lib --features host-test` 实测；不带 `host-test` 的 `cargo test -p axvm --lib` 在 dev 与分支同样无法链接（STACK_SIZE/PAGE_SIZE/percpu 符号缺失），属预存基线问题，与分支无关；dev 基线同命令为 250 通过，分支净增 6 个来自提交 2 的 loader 校验用例） |
| axvmconfig 单测 | 15 通过（`cargo test -p axvmconfig --lib` 实测；dev 基线 11，净增 4 个 `firmware_profile_*`：默认 None、显式解析、非 UEFI 拒绝、UEFI 接受） |
| axdevice 单测 | 44 passed（`cargo test -p axdevice --lib` 实测；含 `device::tests` 25 与 `fw_cfg::tests` 7——其中 3 例受 `host-test` feature 门控（`dma_descriptor_uses_the_runtime_granted_memory_port`/`pio_selector_data_and_dma_share_one_fw_cfg_state`/`pio_dma_fault_does_not_poison_the_next_32_bit_transfer`），1 例受 `target_pointer_width = "64"` 门控（`dma_address_register_preserves_high_half_and_resets_after_commit`，64 位主机上运行），无门控 3 例（`dma_read_uses_bounded_chunks_for_large_guest_length`/`dma_write_discard_uses_bounded_chunks_for_large_guest_length`/`dma_rejects_buffer_address_overflow`）直接跑；dev 基线 35 passed，净增来自提交 3 的 trace 用例）；`--features host-test` 全量 47 passed |
| axdevice 集成测试 | `tests/runtime_controllers.rs` 3 passed、`tests/resource_planning.rs` 4 passed（`cargo test -p axdevice --test …` 实测） |
| clippy | `cargo xtask clippy --package axdevice --package axbuild` 2 包 3 checks 全部通过，零告警零 allow（实测）；`--package axvm --package axvmconfig` 2 包 8 checks 全部通过（实测） |
| `cargo fmt` | 通过（`cargo fmt --all --check` 实测） |
| ovmf-entry 用例发现 | `--list` 正确发现 `ovmf-entry-vmx`/`ovmf-entry-svm` |
| ovmf-entry 无 bundle 行为 | fail-fast 报错；`--list` 不要求 bundle |
| 真实 QEMU 运行 | 双变体实测 PASS（提交 8 验证记录）：`ovmf-entry-vmx` 6.97s、`ovmf-entry-svm` 6.80s（fw_cfg marker 匹配）；嵌套链路证据：`selected firmware profile … verification_label=VERIFIED` → 装载日志 → `VM[1] boot success` → `VM[1] VCpu[0] running...` → `Nested OVMF fw_cfg accessed`（Info 级密集出现，覆盖 selector/data PIO 窗口 `0x510-0x511` 与 DMA 窗口 `0x514-0x51b` 的访问）→ `=== SUCCESS PATTERN MATCHED ===` |
| 回归 | 现有 direct VMX/SVM smoke 与 dev 基线一致 |

每提交均经 subagent 全量评审（实跑 clippy/fmt/单测）→ fix → 复审闭环；`action=fix` 意见全部在实现轮内修复。

## 6. 遗留事项与 non-gating 说明

以下遗留项收敛于 `plan/collect_issue.md`（本分支评审遗留 Issue 清单），不阻塞本 PR 合入：

- **agent_review_bench flaky**（清单 #1，高）：`scripts/axbuild/src/agent_review_bench/mod.rs` 的 `mock_agents_write_artifacts_and_reject_invalid_json` 间歇失败，根因是 git commit 与 maintenance 竞态 + 时间戳偏差（详见 `plan/collect_issue.md` #1）；**非本分支引入**（分支 git diff 不涉及该模块）。本次复核另观察到 `agent_review_bench::runner::tests::reads_version_from_injected_agent_program` 在并行全量跑中偶发失败、单例重跑通过，同样属该组既有环境 flake。CI 失败时按已知 flake 重跑确认；
- **VMX 门禁验证待完成**（清单 #6，高）：`ovmf-entry-vmx` 与 `ovmf-entry-svm` 均已在本分支真实 QEMU 验证 PASS（vmx 6.97s，svm 6.80s，提交 8 验证记录）；ovmf-entry 用例**保持 non-gating**，直到 CI 自托管矩阵接入 uefi 组并在 VMX/SVM 两机多次重复稳定后再升级 gating（`cargo xtask axvisor test qemu --arch x86_64 --test-group uefi --test-case ovmf-entry-{vmx,svm} --firmware-bundle-path <ostool-cache-dir>`）；
- **替换相关遗留**：提交 8 已同步更新 CLI help 文本与 `boot-debugging.md`（无 manifest 表述残留，实现时已逐处核对）；**PR 分支文档（`feat/uefi-p0-r2`）中 `docs/docs/build/axvisor/test.md` §3.3 开头段仍残留 SEC marker 时代描述**（`(?s)VCpu[0] running...`/`SecCoreStartupWithStack` 与"前缀锚定是必需的"，与提交 8 的 fw_cfg marker 成功判据矛盾）及 `--firmware-bundle-path <bundle 目录>` 占位符，**`docs/docs/architecture/axvisor/overview.md` 亦仍写"固定 OVMF bundle"与"第一阶段 SEC 启动诊断已落地"**（提交 8 未更新该文件）——两文件均位于 PR 分支而非本 plan 分支工作区，需在 PR 分支合入前以独立文档提交同步为上游 RELEASE + fw_cfg marker 表述；fw_cfg marker 依赖 axdevice 新代码（`FwCfgPioDevice::access` 的 Info 级打印），该改动跨 axdevice 模块边界，需在评审中确认（marker 仅加日志不改行为，但作为测试判定输入，其语义变更由后续 axdevice 演进评审把关）；
- 其余低/中优先级文档与代码一致性项（清单 #2-#5）：overview.md 旧路径、boot-debugging.md 措辞精度、CI 矩阵变更时文档同步、固定常量跨 crate 重复定义（提交 8 已把 `OVMF_PROFILE_NAME` 收敛为 ovmf.rs 主定义；axvm 侧 `FIXED_OVMF_PROFILE` 仍与 axbuild 侧重复且 axvm 禁止依赖 axbuild，两处值需人工保持同步，建议后续加编译期一致性校验）。

## 7. PR 标题与正文草稿

### 标题（英文，Conventional Commits）

```
feat(axvisor): switch ovmf-entry to upstream RELEASE OVMF
```

说明：本 PR 跨 axbuild/axvm/axvmconfig/axdevice/axvisor 五个 crate，无单一主导 crate，按 AGENTS.md 采用跨切面 scope；建议选 `axvisor`（承载用例、编排与文档主体，也是行为可见面）或 `docs`/`repo` 类宽 scope。备选：`feat(axvisor): verify upstream OVMF and diagnose nested UEFI entry (phase 1)`。

### 正文（中文，可直接粘贴 GitHub）

```markdown
## 解决的问题

AxVisor 的 x86_64 路径缺少可诊断的 UEFI guest 启动支持：嵌套 OVMF 固件进入 SEC 后"走了哪一步、访问了什么"完全不可见——自建 DEBUG OVMF 资产不可复现、RELEASE 构建无串口 SEC marker 无法判定到达、装载时无布局强制、无统一的设备访问 trace、无嵌套固件启动的端到端测试门禁。后续 fw_cfg/ACPI/PCI/virtio 平台发现与启动盘支持缺少诊断基础。

**本 PR**：为 x86_64 guest UEFI 建立阶段 1 的最小闭环——上游 RELEASE OVMF 资产校验 + loader 布局强制 + 设备访问 trace + ovmf-entry 嵌套启动诊断（fw_cfg 访问 marker），共 8 个提交。本 PR 为后续 UEFI 支持（fw_cfg/ACPI/PCI/virtio 平台发现与启动盘路径）提供可诊断的基础。

## 改了什么

本 PR 沿"资产校验 → 装载强制 → 诊断观察 → 端到端用例 → 文档"五层建立阶段 1 闭环，外加真实验证暴露的两处修复与一处替换提交：

- **上游 OVMF 资产校验（axbuild）**：新增 `scripts/axbuild/src/axvisor/ovmf.rs`，以 typed Rust verifier 替代旧 `ovmf-profile.sh` 的校验职责——`FirmwareSource` 枚举（上游 ostool cache 目录 `code.fd`/`vars.fd` vs 显式 opt-in 的本地未验证固件）、`verify_firmware` 布局防御性校验（文件存在、大小 `0x37c000`/`0x84000`、reset vector 在 CODE 窗口内），SHA-256 由 ostool 自身校验；CLI 新增 `--firmware-bundle-path <dir>` 与 `--allow-unverified-firmware`（未验证固件必须显式 opt-in 且不能决定测试结论）。
- **x86 loader 布局强制（axvm/axvmconfig）**：`axvmconfig` 新增 `firmware_profile` 配置字段（非 UEFI boot 协议声明时拒绝，`FirmwareProfileRequiresUefi`）；x86 loader 在 fs 与内存两条 UEFI 装载路径上，仅当 profile 为固定值时强制 CODE 大小 `0x37c000`、GPA `0xffc84000`、reset vector `0xfffffff0` 并打印装载日志。未声明 profile 时行为与 dev 完全一致，现有 `arceos-uefi-smp1.toml`（`bios_load_addr = 0xffc0_0000`）不受影响。
- **设备访问 trace（axdevice）**：在 `DeviceRuntime::dispatch_optional`（统一 lookup 入口）内记录统一格式 trace（`device= bus= direction= addr= width= value= result=`），未注册地址仍返回 `NotFound` 并记为 `device=<unregistered>`，SysReg 不 trace；替换死代码 `log_device_io`。trace 只加日志不改行为，作为后续 UEFI 支持的诊断输入。
- **ovmf-entry 端到端用例（test-suit + axbuild runner）**：新增 `test-suit/axvisor/uefi/` 分组与 `ovmf-entry` 用例（VMX/SVM 变体仅 `-cpu` 一行不同，均显式 `-cpu host` 透传宿主 CPU 特性——QEMU 默认 `qemu64` 缺少 VMX/SVM 及嵌套固件所需的特性；天然 non-gating）：QEMU `-kernel` 启动 Axvisor 宿主（`uefi = false` 阻断 ostool 注入自带 FAT ESP），Axvisor 加载嵌套 OVMF 作为 UEFI guest 固件。runner 消费 `--firmware-bundle-path`：校验过的 `code.fd` 经 `include_bytes!` 嵌入 VM 配置并作为 QEMU pflash unit 0（VARS 每运行独立拷贝为 unit 1），两层固件字节级相同；无 bundle 时 fail-fast，绝不静默 fallback 到发行版 OVMF；`--list` 不要求 bundle。新增专用 VM 配置 `os/axvisor/configs/vms/qemu/x86_64/ovmf-entry.toml`。
- **文档（docs + skill reference）**：`boot-debugging.md` 新增 "x86_64 Guest OVMF / Verified Upstream Firmware" 章节（常量/CLI/接线/失败模式），`axvisor/overview.md` x86_64 更新为 VMX/SVM + 固定 OVMF UEFI 状态，`axvisor/test.md`/`ci.md` 等补充 `uefi` 分组与固件参数。
- **修复 1：success_regex 锚定嵌套 VM（历史，后被替换提交取代）**：真实 QEMU 验证发现宿主层 OVMF 与嵌套 OVMF 输出相同的 SEC 串口行，裸 regex 847ms 即误匹配宿主层；提交 6 曾改为 `(?s)VCpu[0] running...` 前缀锚定嵌套层，提交 8 以 fw_cfg marker（`(?m)^.*Nested OVMF fw_cfg accessed`）彻底取代 SEC marker——marker 由 axvisor 设备模型打印，天然只属嵌套 guest（宿主 OVMF 的 fw_cfg 访问在 QEMU 层，不经过 axvisor 设备模型），无需 VM 前缀锚定、不受 2048 字节匹配窗口限制；
- **修复 2：拒绝未验证固件驱动测试结论**：`--allow-unverified-firmware` 是诊断专用逃生通道，`verify_cli_firmware_bundle` 对 `verified == false` 的固件直接拒绝，未验证本地 `code.fd` 不能决定 ovmf-entry 测试结果。
- **替换提交：切换上游 RELEASE OVMF + fw_cfg marker**：固定资产从自建 DEBUG bundle（manifest.toml）切换到 ostool 上游 RELEASE OVMF（`edk2-stable202508-r1`，`cargo xtask ovmf --arch x86_64` 一步下载，SHA-256 由 ostool 校验）；`ovmf.rs` 删除 manifest 解析/SHA-256 比对/拼接布局校验，保留布局防御性校验，`OVMF_PROFILE_NAME` 收敛为 ovmf.rs 主定义；`FwCfgPioDevice::access` 加 Info 级 marker `Nested OVMF fw_cfg accessed`，success_regex 换为 `(?m)^.*Nested OVMF fw_cfg accessed`——marker 天然只属嵌套 guest（宿主 OVMF 的 fw_cfg 访问在 QEMU 层，不经过 axvisor 设备模型），无需 VM 前缀锚定、不受 2048 字节匹配窗口限制。

## 每步的逻辑

- **先校验后装载（提交 1 → 2）**：资产校验在构建编排层完成（替代 shell 脚本），loader 只做装载时的布局强制；两层都以固定常量为准，任何一层发现不匹配都显式报错而非回退到发行版 OVMF。校验作用域收窄到"profile 显式声明"——这是不破坏 dev 现存 `arceos-uefi-smp1.toml` 的前提。
- **trace 只加日志不改行为（提交 3）**：SEC 到达后的下一个问题必然是"OVMF 接下来访问了什么"，trace 作为后续 UEFI 支持的诊断输入；`result=<ok|error>` 保留失败可见性，为后续设备实现提供首个未支持访问的定位。
- **固定资产接线用 pflash 而非 ostool（提交 4）**：`uefi = false` 阻断 ostool 注入自带 FAT ESP，成功判定只来自嵌套固件的设备模型 marker；校验过的 `code.fd`/`vars.fd` 以 pflash 注入 QEMU 层引导宿主，嵌套层由 Axvisor 从 VM 配置加载，两层字节级相同。
- **固定资产用 ostool 上游而非自建（提交 8）**：自建 DEBUG bundle 不可复现且 RELEASE 无串口 SEC marker；上游 RELEASE OVMF 由 ostool 发布与校验（`cargo xtask ovmf --arch x86_64` 一步下载），fw_cfg 访问 marker 由 axvisor 设备模型打印，天然只属嵌套 guest，成功判据与"嵌套固件真正执行并访问平台设备"直接对应。
- **用例保持 non-gating**：CI 自托管矩阵接入 uefi 组并多轮稳定后再升级 gating（见遗留事项）。

## 验证

- axbuild `ovmf.rs` 9/9、`qemu.rs` 14/14；lib 全量 876 passed / 3 failed（均为预存基线：2 个 rootfs 环境失败已在干净 dev 树复现，1 个 agent_review_bench 既有 flaky 重跑恢复）；axvm 256（`--features host-test`）、axvmconfig 15、axdevice 44（lib）+ 集成 3+4；
- clippy（`cargo xtask clippy --package axdevice --package axbuild` 2 包 3 checks、`--package axvm --package axvmconfig` 2 包 8 checks）零告警零 allow；`cargo fmt` 通过；
- ovmf-entry：`--list` 正确发现 vmx/svm 变体；无 bundle 时 fail-fast；
- 真实 QEMU 运行：双变体实测 PASS——`ovmf-entry-vmx` 6.97s、`ovmf-entry-svm` 6.80s（提交 8 验证记录），嵌套链路证据完整（VERIFIED → 装载日志 → VM boot success → VCpu running → `Nested OVMF fw_cfg accessed` → SUCCESS PATTERN MATCHED）；
- 每提交经 subagent 全量评审（实跑 clippy/fmt/单测）→ fix → 复审闭环。

### 手动部署固件并测试

以下为在本机复现 ovmf-entry 端到端验证的步骤（提交 8 验证记录：vmx 6.97s、svm 6.80s）：

1. **下载上游 RELEASE OVMF**（一步完成，ostool 管理 `Source::LATEST` 缓存，SHA-256 由 ostool 校验）：

   ```bash
   cargo xtask ovmf --arch x86_64
   ```

   缓存目录为 ostool cache（`$TMPDIR/ostool/ovmf/x64`，可用 `TGOS_OVMF_DIR` 覆盖），内含 `code.fd` 与 `vars.fd`。

2. **运行 ovmf-entry 用例**（本机 AMD 用 SVM 变体，Intel 用 VMX）：

   ```bash
   cd <tgoskits>
   cargo xtask axvisor test qemu --arch x86_64 --test-group uefi \
     --test-case ovmf-entry-svm --firmware-bundle-path <ostool-cache-dir>
   ```

3. **预期结果**：`ok: ovmf-entry-svm`（约 7s）。日志中可见完整嵌套链路证据：

   ```
   selected firmware profile: qemu_x86_64_axvisor_ovmf_debug ... verification_label=VERIFIED
   Loading fixed OVMF CODE profile <memory> (3653632 bytes) into GPA @0xffc84000 with reset vector 0xfffffff0
   VM[1] boot success
   VM[1] VCpu[0] running...
   Nested OVMF fw_cfg accessed (port=0x510, dir=read, sel=0x3, window=SelectorData)   ← 成功判据（示例行，port/sel/window 随固件探测时序变化）
   === SUCCESS PATTERN MATCHED: (?m)^.*Nested OVMF fw_cfg accessed ===
   ```

   若 marker 未出现：检查 `--firmware-bundle-path` 是否指向 ostool cache 目录（`code.fd`/`vars.fd` 存在），或 `bios_load_addr`/`firmware_profile` 是否与固定布局一致。

## 遗留事项

- `agent_review_bench` 已知 flaky（非本分支引入，`mock_agents_write_artifacts_and_reject_invalid_json` 间歇失败，隔离重跑可恢复；建议 CI 失败时按已知 flake 单独重跑确认）；
- `ovmf-entry-{vmx,svm}` 已实测 PASS（vmx 6.97s、svm 6.80s），但 CI 自托管矩阵尚未接入 uefi 组，用例保持 non-gating，多轮稳定后再升级 gating；
- fw_cfg marker 依赖 axdevice 新代码（`FwCfgPioDevice::access` Info 级打印），跨模块边界，需评审确认（仅加日志不改行为）。
```

## 8. 合入前检查（复用 solve_conflict.md §7 验收标准中适用于本 PR 的项）

- [ ] 分支仅含 §1 的 8 个提交；
- [ ] 合入前刷新远端并复核 `dev` 是否再次变化（方案基于本地 `dev@343d47f4e`）；
- [ ] `plan/uefi/design.md`/`solve_conflict.md` 与实现一致，`collect_issue.md` 已跟踪遗留项；
- [ ] 无源码文本断言测试残留；未引入任何已删除的脚本/NimbOS 配置/guest-owned COM1 声明；
- [ ] 现有 `arceos-uefi-smp1.toml` 未被破坏；现有 direct VMX/SVM smoke 回归通过；
- [ ] 阶段 1 未混入阶段 2-5 内容（ACPI/PCI/virtio/中断）。
