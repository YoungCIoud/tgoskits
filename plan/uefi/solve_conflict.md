# 解决 uefi/p0 与 dev 的冲突：方案与实施计划

> 状态：2026-08-04（经六轮独立 subagent 审查并迭代修正：初稿按 `dev@828538f64`；按新增 3 个提交复核至 `dev@4c7aa8577`，并核对了 axbuild 测试发现机制、VM 设备生命周期、串口链路与全部代码落点）。
> 本文档把 `summary.md` 后半部分的冲突分析固化为可执行的方案，补充按最新 dev 核实的接口事实和操作步骤。
>
> 范围：解决 `uefi/p0`（功能多但基线旧）与 `feat/uefi-phase1`（基线新、按 p0 复刻）合并到 dev 的冲突，并同步更新 `plan/uefi/` 设计与 `summary.md` 前半部分。**本次实现范围仅为阶段一（SEC 到达验证）**；阶段 2-5（ACPI/PCI/virtio/中断）保留为路线图，不在此方案内实现。
> 审查记录：2026-08-04 经六轮独立 subagent 审查并迭代修正（BLOCKING：trace 无对应提交、固定资产与测试 pflash 接线缺失、相对路径错误、测试命令不统一；IMPORTANT：分支拓扑、提交数、校验作用域、行号、ESP FAT 注入、design.md 同步；NIT：build 变体表述、dispatch 行号、函数名行号、GSI 行号、discovery.rs 路径等），第六轮结论"是"（无 BLOCKING/IMPORTANT），残余 NIT 已全部修正。本章后各节即修订结果。

---

## 1. 现状与结论

### 1.1 三条分支的关系

```text
dev (4c7aa8577)                          # 最新主线
   ▲ dev 自共同基点 024ecca10 前进 34 个提交（rev-list 计数，全部独立，无 merge）
   │
024ecca10  ◄── 共同基点 ◄── uefi/p0 (941134916, 1 个集中提交, +1707/-193, 24 文件)
   │
   ▲ dev 自 f1774c1f6 前进 20 个提交（rev-list 计数，全部独立，无 merge）
f1774c1f6  ◄── feat/uefi-phase1 基点 (2 个提交: 3782df379 + f0fdd421c)
```

- `uefi/p0` 和 `feat/uefi-phase1` 的共同基点是 `024ecca10`；phase1 的基点 `f1774c1f6` 比 p0（`941134916`）**新 14 个提交**（`git rev-list --count 941134916..f1774c1f6` = 14）。
- 注意：`d7050469f`（删除 NimbOS/脚本）**不在** phase1 基点之前（`f1774c1f6..d7050469f` = 15，即它晚于 phase1 基点 15 个提交）。因此 phase1 分支**并不包含** NimbOS 删除；它只是相对 p0 更接近 dev，但同样基于旧接口。`feat/uefi-phase1` 的"裁剪"体现在它只带了 2 个提交，而没有把 NimbOS/脚本的删除提交带进来。
- `summary.md` 前半部分记录的是 phase1 分支的端到端验证过程（基于已删除的旧测试框架）。

### 1.2 核心结论（与 summary.md 一致）

**不要对 `941134916` 做整体 rebase/cherry-pick 后逐个解冲突。** 只读合并模拟显示（逐文件核对，见 §1.4）：

- 16 个文件双方同时修改；
- 7 个文件 dev 已删除但 P0 仍在修改；
- 8 个文件 P0 新增（dev 中不存在）；
- 大量"文本上能合并、语义上已无效"的新增代码。

**原因**：dev 在共同基点后重构了 P0 依赖的核心边界，旧代码约一半以上已失效：

| dev 的变化 | 对 P0 的影响 |
|---|---|
| 串口变成 machine-owned 资源（`virtualization/axvm/src/machine.rs:335` x86 profile 固定 16550 COM1 PIO 0x3f8 + GSI 4；`docs/docs/architecture/axvisor-guest-machine.md:73` 起明确禁止 `emu_devices`/`serial` 等字段） | 必须删除 guest TOML 的 COM1 声明、`guest_serial.rs` 全局 matcher、`before_first_run`/`on_last_vcpu_exit` 挂钩、vlapic host ops 截获 |
| `d7050469f` 删除 setup_qemu.sh/quick-start.sh/NimbOS 配置/qemu-nimbos 用例/旧 CI 与脚本文档 | 不能保留 ovmf-profile.sh，不能恢复脚本或 NimbOS 用例 |
| VM 配置模型改为 `guest_type = "passthrough"` + `[devices] passthrough/disabled`（`os/axvisor/configs/vms/qemu/x86_64/arceos-uefi-smp1.toml:1,55-57`），`vm_type = 1`/`emu_devices = [...]` 已失效 | P0 的旧配置无法机械合并 |
| VM 创建顺序重构（`os/axvisor/src/config.rs:147-170`：`AxVM::new`(:148) → `prepare_memory_layout`(:152) → `load_images`(:158) → `prepare`(:162) → `register_vm`(:168)；串口 backend factory 在 `config.rs:241` 注入） | P0 的全局诊断状态（防 VM ID 重用、注册失败、reset 污染）已不需要；新的 per-generation serial backend（`axdevice/src/serial/backend.rs:11-28`，`SerialBackend` 在 :11、`SerialBackendFactory` 在 :23）已解决大部分问题 |
| `Device` trait 迁到 `axdevice_base/src/lib.rs:280`（`fn name()/resources()/access(&self, access, context)`），`DeviceAccess` 在 `:355`；`DeviceRuntime::dispatch` 在 `device.rs:1149` 返回 `Result<BusResponse, DeviceError>`；`BusAccess` 含 `kind/addr/width/is_read/data` | P0 在 device.rs 里新增的 trace 实现不能直接搬用，需按新 `Device`/`BusAccess` API 重写 |
| 测试规范禁止源码文本断言（`book/guideline/code-quality.md:1713`：不得用 `include_str!` 解析源码验证行为，必须编译并调用真实模块或公共 API） | P0 的 `ovmf_tests.rs`/`tests.rs` 大量读取源码/TOML 文本断言，必须重写 |
| `4c7aa8577` 新增 `virtualization/axvm/src/host/`（`host/mod.rs`、`host/arceos.rs`）host OS 适配边界，`os/axvisor/src/config.rs` 不再反向依赖 axvm 的 host 细节 | 阶段一若需要固件相关 host 服务，应放入 `axvm/src/host/` 或 axbuild，不直接在 `os/axvisor/src/config.rs` 扩展 |

### 1.3 建议

**推翻旧实现，保留设计目标。** 旧分支留作需求与边界条件参考；从最新 dev 新建实现分支重做；第一项工作先同步 `plan/uefi/design.md`，明确删除脚本、NimbOS、guest-owned COM1 和 arch-level matcher，再开始代码。

### 1.4 已核实的冲突文件清单

**双方同时修改（16）**：

```
.claude/skills/arch-platform-porting/references/boot-debugging.md
.github/workflows/ci.yml
docs/docs/build/ci.md
os/axvisor/configs/vms/qemu/x86_64/arceos-uefi-smp1.toml
os/axvisor/configs/vms/qemu/x86_64/linux-uefi-smp1.toml
os/axvisor/configs/vms/qemu/x86_64/nimbos-uefi-smp1.toml
os/axvisor/doc/qemu-quickstart.md
os/axvisor/doc/qemu-quickstart_cn.md
os/axvisor/scripts/quick-start.sh
os/axvisor/scripts/setup_qemu.sh
scripts/axbuild/src/axvisor/test/mod.rs
scripts/axbuild/src/axvisor/test/tests.rs
test-suit/axvisor/uefi/qemu-nimbos/qemu-x86_64.toml
virtualization/axdevice/src/device.rs
virtualization/axvm/src/arch/x86_64/boot/mod.rs
virtualization/axvm/src/arch/x86_64/mod.rs
```

**dev 已删除但 P0 仍在修改（7）**：

```
os/axvisor/configs/vms/qemu/x86_64/linux-uefi-smp1.toml
os/axvisor/configs/vms/qemu/x86_64/nimbos-uefi-smp1.toml
os/axvisor/doc/qemu-quickstart.md
os/axvisor/doc/qemu-quickstart_cn.md
os/axvisor/scripts/quick-start.sh
os/axvisor/scripts/setup_qemu.sh
test-suit/axvisor/uefi/qemu-nimbos/qemu-x86_64.toml
```

**P0 新增、dev 中不存在（8）**：

```
os/axvisor/doc/x86-uefi-profile.md
os/axvisor/scripts/ovmf-profile.sh
scripts/axbuild/src/axvisor/test/ovmf_tests.rs
test-suit/axvisor/uefi/ovmf-entry/build-x86_64-unknown-none-svm.toml
test-suit/axvisor/uefi/ovmf-entry/build-x86_64-unknown-none-vmx.toml
test-suit/axvisor/uefi/ovmf-entry/qemu-x86_64-svm.toml
test-suit/axvisor/uefi/ovmf-entry/qemu-x86_64-vmx.toml
virtualization/axvm/src/arch/x86_64/guest_serial.rs
```

---

## 2. 决策树

```text
Q1: 要保留 uefi/p0 的哪些功能？
    │
    ├─ 只需"SEC 到达验证"（阶段 1 诊断）── 本次实现范围
    │     └─ Q2
    ├─ 需要完整 UEFI 启动（ACPI/PCI/virtio-blk，设计文档阶段 2-5）
    │     └─ Q2（路线图，不在本次）
    └─ 两者都要
          └─ Q2

Q2: 这些功能在 dev 上怎么落地？
    │
    ├─ 直接改造现有 dev 配置/接口（guest COM1 已 machine-owned，
    │    SEC 校验挂在现有 boot/mod.rs 的 load 路径上）
    │     └─ 推荐：从 dev 新建分支重做（本方案）
    │
    └─ 必须保留旧接口/旧文件（如 ovmf-profile.sh、NimbOS 配置）
          └─ 不推荐：需要先把 dev 回滚到共同基点附近，损失 34+ 个提交，
             与主线背道而驰

Q3: 实施顺序？
    ├─ 先同步设计文档，再写代码（推荐，本方案 §4）
    └─ 直接写代码，文档最后补
          └─ 风险：实现与已评审设计脱节，评审来回返工
```

**分支决策**：从 dev 新建 `feat/uefi-p0-r2`（或复用/改名现有 `feat/uefi-phase1`），只在新分支上提交；`uefi/p0` 与 `feat/uefi-phase1` 均保留不合并、不删除。

---

## 3. 保留与重做清单

### 3.1 从 uefi/p0 保留（重做到最新边界）

| P0 内容 | 处理方式 | 重做落点 |
|---|---|---|
| 固定 edk2 tag/commit、构建开关、manifest 契约 | 保留 | 新分支 |
| CODE/VARS 大小、SHA-256、拼接关系验证 | 保留，改写为 Rust typed verifier | `scripts/axbuild`（新私有类型） |
| CODE GPA `0xffc84000`、大小 `0x37c000`、reset vector `0xfffffff0` | 保留，重新应用到最新 x86 loader | `virtualization/axvm/src/arch/x86_64/boot/mod.rs` |
| 只向 AxVisor 装载 CODE，不加载 VARS | 保留 | 同上 |
| 未验证固件必须显式 opt-in 并打印哈希 | 保留，放到 xtask CLI | `scripts/axbuild` xtask 参数 |
| PIO/MMIO trace 语义 | 保留，重写到新 `Device`/`BusAccess` API | `virtualization/axdevice/src/device.rs`（或 axdevice_base） |
| ovmf-entry VMX/SVM 诊断 | 保留，重新接入最新测试编排 | `test-suit/axvisor/uefi/ovmf-entry/` + `scripts/axbuild/src/axvisor/test/` |
| guest TOML 注册 COM1 | **删除**（machine-owned） | — |
| `guest_serial.rs` 全局 matcher | **删除** | — |
| `setup_qemu.sh`/`quick-start.sh`/`ovmf-profile.sh` | **删除，不恢复** | — |
| NimbOS 配置、qemu-nimbos | **删除** | — |
| 源码文本断言测试 | **删除并改写** | `scripts/axbuild`（编译并调用真实 API） |
| 旧 quick-start 文档路径 | **删除**，迁入现有 architecture/development 文档 | `docs/docs/architecture/` |

### 3.2 dev 上可直接复用的现成能力（不必重写）

| 能力 | 位置 |
|---|---|
| 16550 COM1 machine profile（PIO 0x3f8, GSI 4） | `virtualization/axvm/src/machine.rs:335`（`fn x86_64_profile()`，0x3f8 在 :339、`irq: 4` 在 :342）；`serial_device_config` 在 :292；`MachineSerialFactory` 在 :524-531，backend factory 在 `config.rs:241` 注入 |
| per-generation serial backend（VM reset 时重建设备图，回调不串代） | `virtualization/axdevice/src/serial/backend.rs:11-28`（`SerialBackend`/`SerialBackendFactory`） |
| guest console mux（输出统一经 `mux/mod.rs` 的 `ConsoleCore::write_guest_output`；公开入口 `serial_backend_factory` 在 `mux/mod.rs:412`、`route_host_byte` 在 `:420`；`-serial stdio` 用例另见 §5.3） | `os/axvisor/src/guest_console/mux/mod.rs`；`mux/output.rs:38` `format()` |
| UEFI 固件加载入口 | `virtualization/axvm/src/arch/x86_64/boot/mod.rs:259` `load_uefi_from_configured_path`（fs 路径经 `virtualization/axvm/src/boot/images/mod.rs:317` 起 `BootImageProvider::file_size`/`read_full_image`） |
| VM 配置模型 `guest_type = "passthrough"` | `os/axvisor/configs/vms/qemu/x86_64/arceos-uefi-smp1.toml:1,55-57` |
| VM 设备生命周期与 reset 清理 | `virtualization/axvm/src/vm/prepare.rs:36` `prepare()`、`complete_vm_init` 内 `reset_transient_resources()`；`machine.rs:514` `register_machine_device_factories` 从 config 取 `serial_profile`+`serial_backend_factory`（`:519-520`），`MachineSerialFactory` 在 `:524-531` |
| host OS 适配边界 | `virtualization/axvm/src/host/`（`host/mod.rs`、`host/arceos.rs`，`4c7aa8577` 新增） |

> 注：ovmf-entry 是单 VM 测试，不必重新实现 SEC matcher——直接把 `SecCoreStartupWithStack(` 作为 success_regex（见 §5.3 关于 `-serial stdio` 的说明）。

---

## 4. 实施顺序（五个提交，从 dev 依次建）

### 提交 1：`feat(axbuild): verify fixed OVMF debug bundles`

**目标**：在 axbuild 增加固定 OVMF 资产校验，替代旧 `ovmf-profile.sh` 的校验职责。

- 新增私有类型：`OvmfManifest`、`VerifiedOvmfBundle` 等；
- 使用 serde/TOML + `deny_unknown_fields`；
- 复用现有 image registry 下载与 archive SHA-256；
- 校验：单文件大小、哈希、CODE/VARS 拼接关系、profile、GPA；
- 用 `FirmwareSource` 枚举区分"固定资产"与"显式未验证本地固件"，避免布尔参数扩散。

**验证**：单元测试覆盖 manifest 正常 / 错误哈希 / 错误大小 / 错误 base / 未知字段 / 重复字段；未验证固件缺少显式 opt-in 时拒绝。

### 提交 2：`feat(axvm): enforce the fixed x86 OVMF CODE layout`

**目标**：在最新 x86 loader 上加小型、私有的 layout 校验。

- 落点：`virtualization/axvm/src/arch/x86_64/boot/mod.rs` 的 `load_uefi_from_configured_path`（`:259`，fs 路径）与 `load_boot_image_from_memory`（`:202`，内存镜像路径）两处都加校验；UEFI 分支调用点在 `:217-218`；
- 约束：CODE 大小 `0x37c000`、GPA `0xffc84000`、reset vector `0xfffffff0`；multiboot/direct boot 路径保持不变；
- **校验作用域（关键）**：dev 现存 `os/axvisor/configs/vms/qemu/x86_64/arceos-uefi-smp1.toml` 用 `bios_load_addr = 0xffc0_0000` 加载 OVMF_CODE.fd——若无条件拒绝非 `0xffc84000` 的装载，现有配置即失效。因此校验必须**只在固定 profile 启用时生效**（例如仅当 `uefi_firmware_path` 指向受管 bundle 路径、或 config 显式声明 profile 时），不校验普通 UEFI 装载；方案不要求修改/废弃现有 `arceos-uefi-smp1.toml`（该 TOML 未声明固定 profile 时不受影响）；
- 大小获取：fs 路径用 `BootImageProvider::file_size`（`virtualization/axvm/src/boot/images/mod.rs:317` 起的 impl 内）先查大小再加载；memory 路径对 `&[u8]` 直接取 `len()`；
- 成功日志打印 profile、路径、大小、GPA、reset vector。

**验证**：对错误 CODE 大小、GPA、entry point 的真实 API 测试（编译并调用真实模块，不是源码文本断言）。可参考现有 `boot/mod.rs` 的 `uefi_config_skips_multiboot_patch` 等单测风格。

### 提交 3：`feat(axdevice): trace guest device accesses in DeviceRuntime`

**目标**：在统一设备访问路径上实现可归因的 trace，替代 P0 在旧 `device.rs` 的 trace 实现。

- 落点：`virtualization/axdevice/src/device.rs:1149` `dispatch()`（各访问入口——含 `handle_mmio_write_with_memory` `:956`、`handle_mmio_read` `:887` 等——构造 `BusAccess` 后都经 `dispatch()` 统一路径）；`DeviceError::NotFound` 变体存在于 `axdevice_base/src/device.rs:186`；
- 输出格式：`device=<name> bus=PIO|MMIO direction=read|write addr=<addr> width=<width> value=<value> result=<ok|error>`；未注册资源返回 `DeviceError::NotFound`，不伪造返回值；
- 按 `axdevice_base` 的 `Device`（`lib.rs:280`，`fn name()/resources()/access(&self, access, context)`）与 `DeviceAccess`（`:355`）新 API 实现，不搬用旧 P0 实现；
- 保留/替换现有 `log_device_io`（`device.rs:86`）语义，不重复日志路径。

**验证**：host 单测覆盖注册设备 read/write/error、未注册设备、不同宽度（参考现有 `device.rs` 测试风格）；ovmf-entry 用例的 CI 日志可见 `device= bus= addr=` 行。

### 提交 4：`test(axvisor): add fixed OVMF entry diagnostics`

**目标**：ovmf-entry 用例接入最新测试编排，不绑定 NimbOS，不声明 COM1，且**必须使用提交 1 校验过的固定 bundle**（见 §5.3）。

- 新增 `test-suit/axvisor/uefi/` 分组（dev 自动发现：`discover_group_names` 枚举 `test-suit/<os>/` 下所有目录，见 `scripts/axbuild/src/test/suite.rs:33-49`；`--test-group uefi` 显式指定；`require_group_dir` 校验分组存在）；ovmf-entry 用例放 `test-suit/axvisor/uefi/ovmf-entry/`；
- 布局与现有 `test-suit/axvisor/normal/qemu/` 一致：`build-x86_64-unknown-none-svm.toml`、`build-x86_64-unknown-none-vmx.toml`（**两份内容相同的变体 build 配置**，各含 `vm_configs`；发现机制 `resolve_build_config_paths`（`scripts/axbuild/src/test/qemu/discovery.rs:243`）按 `build-<target>-<variant>.toml` 命名区分）为 build wrapper；`ovmf-entry/` 下放 `qemu-x86_64-svm.toml`、`qemu-x86_64-vmx.toml`（发现机制：build wrapper 目录下无 build 配置的子目录作为用例，`qemu_config_key_for_wrapper` 按 arch 选配置）；
- VMX/SVM 的 qemu 参数仅 `-cpu` 不同（参考现有 `normal/qemu/` 的 vmx/svm 差异）；
- success_regex 直接匹配 guest COM1 输出 `SecCoreStartupWithStack\(`；
- **固件接线（关键）**：ovmf-entry 的 qemu TOML **不能**沿用 P0 的 `uefi = true`——ostool 0.26 的 `prepare_uefi()`（`ostool/src/run/qemu.rs:605`）会从 `Source::LATEST`（`edk2-stable202508-r1`，`source_constants.rs:44`）下载 OVMF 并注入 pflash（`qemu_plan.rs:54-64`），且把待测 BIN（axvisor.efi）塞进 ESP 目录，验证的将不是固定资产。需在 axbuild 的 qemu 用例准备流程中：用提交 1 校验通过的 CODE/VARS 覆盖 ostool 的 pflash 参数（或在 qemu args 中显式传 `-drive if=pflash,format=raw,unit=0,readonly=on,file=<verified CODE>` + `unit=1,file=<verified VARS>`，并关闭/绕过 ostool 自带下载）；kernel 仍以 raw kernel（`-kernel`）方式加载 axvisor，OVMF 通过 pflash 进入；
- VM 配置：用 `guest_type = "passthrough"` 模型新建专用 config（或复用现有 `arceos-uefi-smp1.toml` 改 kernel_path），**不**声明 COM1/emu_devices；需确认固件加载走 fs 路径（`image_location = "fs"` + `uefi_firmware_path`）；
- 保持 non-gating，直到固定资产与两类硬件证据齐全（VMX + SVM 都验证过再升级 gating）。

**验证**：
- `cargo xtask axvisor test qemu --arch x86_64 --test-group uefi --test-case ovmf-entry-vmx`（及 `--test-case ovmf-entry-svm`）可运行并通过 SEC marker（CLI：`cargo xtask axvisor test qemu`，`ArgsTestQemu` 支持 `-g/--test-group`、`-c/--test-case`、`-l/--list`，见 `scripts/axbuild/src/axvisor/mod.rs:127-160`；`--test-group` 默认 `normal`；用例名含变体后缀，见下条）；
- SVM/VMX 变体选择：`resolve_axvisor_arch_and_target`（`scripts/axbuild/src/context/arch.rs:154`）在 `--arch x86_64` 时映射默认 target；发现机制按 `build-<target>-<variant>.toml` 变体拆分 build wrapper（`case_name_for_wrapper_variant` 生成 `ovmf-entry-vmx`/`ovmf-entry-svm`，`scripts/axbuild/src/test/qemu/discovery.rs:612-615`），`qemu_config_key_for_wrapper(arch, variant)` 生成 `qemu-x86_64-vmx`/`qemu-x86_64-svm` 配置键（`scripts/axbuild/src/test/qemu/discovery.rs:550-555`），用 `-c ovmf-entry-vmx` 或 `-c ovmf-entry-svm` 显式指定变体；
- 标准 QEMU q35 上同一固件可完成 SEC→PEI→DXE→BDS→EFI marker（排除固件自身问题，作为资产发布前验证）；
- 回归：现有 direct VMX/SVM smoke（`test-suit/axvisor/normal/qemu/`）与 dev 基线一致。

### 提交 5：`docs(axvisor): document the fixed OVMF profile`

- 更新现有架构文档与 `arch-platform-porting` reference（`.claude/skills/arch-platform-porting/references/boot-debugging.md` 在冲突清单中，需同步）；
- 不恢复已删除的 quick-start 文档与脚本；
- 同步更新 `plan/uefi/design.md`（见 §5.1 表格，为同步清单）：design.md 保留阶段 2-5 作为路线图（不删除），本次仅更新阶段 1 相关章节与基线；design.md 的同步即提交 5 的内容，其初稿在提交 1 之前先行评审通过（确保实现与已评审设计一致），随后按 1→5 顺序合入，提交 5 复检一致性。

---

## 5. plan/uefi/design.md 与 summary.md 的同步更新

### 5.1 design.md 需要改的点

| 章节 | 现状 | 改为 |
|---|---|---|
| 基线 | 写 `dev` 当前 HEAD（`47c044f7d`） | 写当前 `dev` HEAD（`4c7aa8577`），加"本设计随 dev 演进，实现前需复核"；声明本次执行范围为阶段一 |
| §1.1 证据 | 引用 `nimbos-uefi-smp1.toml` 的 `emu_devices = []`、`axdevice/src/lib.rs:78` 注释掉的 virtio_dev、`ci.yml:692-714` | 这些证据已失效：NimbOS 配置已删除，`axdevice/src/lib.rs` 结构已变，需改为引用 `machine.rs:335`（machine-owned COM1）、`arceos-uefi-smp1.toml:1`（新配置模型） |
| §2.1 内部检索 | 引用 `test-suit/axvisor/uefi/qemu-nimbos/`、`uefi/p0`（52b2edc）、`ci.yml:692-714` | qemu-nimbos 已删除；`uefi/p0` 标注为"基线与接口已过时，仅作需求参考"；CI 引用改为当前状态 |
| §4.1 已具备 | 引用 `load_uefi_from_configured_path()` 于 `boot/mod.rs:261`、`axdevice/src/fw_cfg.rs`（995 行）、`arch/x86_64/mod.rs:471` | 行号与文件结构需按 `dev` 重新核对（`boot/mod.rs:259` 已确认） |
| §4.2 缺口 1 | "当前 x86 仅有 SMP MP 表（`mptable.rs`），无 ACPI" | 保留，但补充 dev 已确认的现状 |
| §5.1 层次 | `axdevice/` 设备清单 | 补充 dev 新边界：`axdevice_base`（Device trait）、`axdevice/src/serial/`（backend）、`axvm/host/`、guest_console mux |
| §7 阶段 1 | COM1 observer、`ArchOps::before_first_run` 挂钩、`ovmf-entry` 配置 | 删除 observer 挂钩（改 success_regex 直配 guest COM1）；`ovmf-entry` 挂新测试编排；设备 trace 作为提交 3 保留 |
| §8 新增公共接口 | 阶段 2 起的新设备 | 补注：阶段 1 不新增公共接口，全部私有实现 |
| §9.1 测试矩阵 | `cargo xtask test axvisor --test uefi/ovmf-entry-vmx` | 按新发现机制改为 `cargo xtask axvisor test qemu --test-group uefi --test-case ovmf-entry-vmx`（`-g/-c` 参数见 `scripts/axbuild/src/axvisor/mod.rs:127-160`；用例名为 `ovmf-entry-vmx`/`ovmf-entry-svm`，`-c ovmf-entry` 无前缀匹配语义，必须带变体后缀） |
| §9.1 测试矩阵（trace 行） | "设备 trace 记录未支持访问 | 同上 | 同上" | 明确 trace 由 axdevice 提交（提交 3）提供，host 单测 + ovmf-entry CI 日志验证 |
| §9.2/§10 | 阶段 1 已交付 trace 基础设施（§10 数据依赖） | 保留（提交 3 实现后成立），可加"由提交 3 交付" |
| §12/§13 | 引用 setup/脚本 | 删除脚本引用，改为 xtask 流程 |

### 5.2 summary.md 需要改的点

| 部分 | 现状 | 改为 |
|---|---|---|
| 前半（phase1 验证记录） | 基于已删除的旧框架（手动 `cargo axvisor image pull`、`setup_qemu.sh`、NimbOS 等） | 保留为历史记录，但顶部加醒目说明："以下基于旧测试框架，相关脚本/用例已被 dev 删除，不可作为当前实现依据；当前实现方案见 solve_conflict.md"。或按 §4 重写为"r2 验证记录" |
| 后半（冲突分析） | 基于 `upstream/dev@828538f64f`，因执行额度限制未刷新远端 | 本方案已按本地 `dev@4c7aa8577` 核实（§1.4 清单），建议执行 §6.1 刷新后复核再合入 |

### 5.3 关于测试输出流与固定资产接线的说明（重要）

**输出流**：
- dev 的 guest COM1 输出经 `os/axvisor/src/guest_console/mux/mod.rs` 汇聚：`ConsoleCore::write_guest_output` 通过 `mux/output.rs:38` `format()` 处理。`format()` 只给续行加 `[VM n] ` 前缀，**从不改写 guest 输出的实际字节**（`mux/output.rs:90-94` 测试证实原始字节原样透传）。
- 但 ovmf-entry 的 QEMU 配置用 `-serial stdio` 时（P0 与现有 smoke 用例同款），guest COM1 输出直接走 QEMU stdio 流，**不经 mux**——此时输出即原始 guest 字节流。
- 因此两种情况下的测试输出流中，`SecCoreStartupWithStack(` 原始文本都**必然原样出现**，`success_regex = ["SecCoreStartupWithStack\\("]` 成立。
- 若将来用例改为经 mux 输出，前缀为 `[VM n] ` 且原始字节仍透传，正则依旧成立；不依赖 `VM[1] guest COM1 reached ...` 这种旧 P0 的宿主侧自造 marker。

**固定资产接线（提交 4 的关键约束）**：
- ostool 0.26（`~/.cargo/registry/src/index.crates.io-*/ostool-0.26.0/`）中，qemu 用例的 `uefi = true` 会触发 `prepare_uefi()`（`src/run/qemu.rs:605`）：
  1. `Prebuilt::fetch(Source::LATEST, ...)` 从 ostool 自带镜像站下载 **`edk2-stable202508-r1`**（`src/run/ovmf_prebuilt/source_constants.rs:44`），**不是**我们固定的 `edk2-stable202605`——两者是不同的固件版本，用 ostool 自带固件验证的就不是固定资产；
  2. `prepare_uefi_esp` 把待测 BIN（即 axvisor.efi）放进 ESP 目录——这与本用例"用 raw kernel 启动 axvisor + 验证 OVMF 进入 SEC"的意图不符；
  3. `uefi_pflash`（`src/run/qemu_plan.rs:30-67`）把 CODE/VARS 以 `-drive if=pflash,...` 注入，并**额外追加 `format=raw,file=fat:rw:<esp_dir>` FAT ESP 盘**（`qemu_plan.rs:64`），即 QEMU 会同时挂载 pflash CODE/VARS 与 FAT ESP（内含待测 BIN）——即使绕过 ostool 下载，该 FAT 注入也需要一并关闭。
- 因此**不能在 qemu TOML 里沿用 `uefi = true`**。正确的固件接线方案（三选一，推荐 A）：
  - **A（推荐）**：在 axbuild 的 qemu 用例准备流程中扩展——提交 1 校验通过的 `VerifiedOvmfBundle` 的 CODE/VARS 路径直接生成 qemu args（`-drive if=pflash,format=raw,unit=0,readonly=on,file=<CODE>`、`unit=1,file=<VARS>`），并保证 ostool 不触发自带 OVMF 下载（`uefi = false`）且不注入 FAT ESP；kernel 用 `-kernel` raw 启动；
  - B：qemu TOML 内显式写 pflash 参数，路径来自 xtask 准备的 bundle 目录；
  - C：扩展 ostool 调用，传入固定 bundle 的 CODE/VARS 覆盖默认下载（需改 axbuild 对 ostool 的调用层）。
- 无论哪种，最终验证的是**提交 1 校验过的固定资产**，且成功判定只来自 guest COM1 的 SEC marker。

---

## 6. 执行前检查与风险

### 6.1 必须做的检查

1. **刷新远端**：`git fetch origin && git fetch upstream`（方案基于本地 `dev@4c7aa8577`，远端若有更新需复核）；
2. **复核基线**：实现前用 §1.2 表格中的锚点文件快速核对 dev 是否再次变化；
3. **确认 qemu-nimbos 无残留依赖**：`git ls-tree -r dev --name-only | grep -i nimbos` 应为空（除设计/测试文档外）；
4. **确认测试发现机制**：`test-suit/axvisor/uefi/` 新分组可被 `discover_group_names` 发现（`scripts/axbuild/src/test/suite.rs:33`），用例名/配置命名遵循 §4 提交 4 的布局。**已核实**：`discover_group_names` 扫描 `test-suit/axvisor/` 下所有目录，`-g/--test-group uefi` 与 `-c/--test-case ovmf-entry-vmx`（大小写匹配 `case.name`）均可工作；`uefi` 目录不属默认 `normal`，天然 non-gating。

### 6.2 风险与缓解

| 风险 | 可能性 | 影响 | 缓解 |
|---|---|---|---|
| dev 继续演进，§1.2 锚点再次失效 | 中 | 实现返工 | 实施前先复核锚点；提交粒度小（5 个提交），每个独立可评审 |
| OVMF 资产（tgosimages 上的 bundle）不可达 | 低 | 阻塞提交 1/4 | 校验与下载分离：校验逻辑先行落地，资产下载失败时明确报错，不静默 fallback 到发行版 OVMF |
| 测试误用 ostool 自带 OVMF（`edk2-stable202508-r1`）而非固定资产 | 中 | 提交 4 验证失效 | §5.3 固件接线方案（推荐 A）：axbuild 生成 pflash 参数指向校验过的 CODE/VARS，`uefi = false` + `-kernel` raw 启动，阻断 ostool 下载 |
| 严格 layout 校验破坏现有 `arceos-uefi-smp1.toml`（`bios_load_addr = 0xffc0_0000`） | 中 | 提交 2 回归 | 校验只在固定 profile 显式启用时生效（§4 提交 2 作用域说明），不校验普通 UEFI 装载 |
| SEC marker 在 SVM/VMX 上表现不一致 | 中 | 阻塞提交 4 的 gating 升级 | 先保持 non-gating；VMX/SVM 都出证据再升级 |
| 新测试编排对 ovmf-entry 的发现路径与 QEMU 参数不兼容 | 中 | 提交 4 返工 | 先跑通一个最小用例确认发现机制，再补 VMX/SVM 参数 |
| 设计文档与实现脱节 | 低 | 评审阻塞 | design.md 同步即提交 5：初稿在提交 1 前先行评审通过（确保实现与已评审设计一致），随后按 1→5 顺序合入，提交 5 复检一致性；§5.1 表格即同步清单 |
| `-serial stdio` 与 guest console mux 并存，测试输出流可能与期望不符 | 低 | 提交 4 判定失效 | 见 §5.3 说明；`-serial stdio` 时输出即原始 guest 字节流，正则成立；经 mux 时原始字节仍透传 |

---

## 7. 最终验收标准

- [ ] `uefi/p0` 与 `feat/uefi-phase1` 保留未合并，仅作参考；
- [ ] 新分支（如 `feat/uefi-p0-r2`）从 dev 新建，仅含 §4 的 5 个提交；
- [ ] `plan/uefi/design.md` 已同步 dev 现状（§5.1 全项）；
- [ ] `summary.md` 已加前半部分作废说明或重写，后半分析已按最新 dev 复核；
- [ ] `cargo xtask axvisor test qemu --arch x86_64 --test-group uefi --test-case ovmf-entry-vmx` 与 `--test-case ovmf-entry-svm` 在本机可运行，qemu 输出含 SEC marker，success_regex 命中；
- [ ] ovmf-entry 用例使用的 OVMF 是提交 1 校验过的固定 bundle（§5.3 固件接线，`uefi = false` + pflash 指向校验过的 CODE/VARS），未触发 ostool `edk2-stable202508-r1` 下载；
- [ ] 设备 trace（提交 3）host 单测通过，ovmf-entry CI 日志含 `device= bus= addr=` 行；
- [ ] `cargo xtask clippy --package axbuild`、`--package axvm`、`--package axdevice` 通过，`cargo fmt` 已跑；
- [ ] 现有 direct VMX/SVM smoke 回归通过（与 dev 基线一致）；
- [ ] 标准 QEMU q35 上同一 OVMF 完成 SEC→BDS 的证据已记录；
- [ ] 无源码文本断言测试残留（全部改为编译并调用真实 API）；
- [ ] 未引入任何已删除的脚本/NimbOS 配置/guest-owned COM1 声明；
- [ ] 阶段 1 未触碰 ACPI/PCI/virtio/中断（路线图内容未混入）；现有 `arceos-uefi-smp1.toml` 未被破坏。
