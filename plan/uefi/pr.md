# AxVisor UEFI P0 PR 草案

## PR 标题

```text
feat(axvisor): add reproducible OVMF debug profile
```

## PR 状态

建议先以 Draft PR 提交。

仓库内实现已经基于最新 `dev` 完成 rebase，并通过单元测试、契约测试、格式检查和
targeted clippy。固定 OVMF 资产发布、标准 QEMU q35 资格测试以及 VMX/SVM 实机入口
证据尚未取得，因此当前不能宣称 P0 已完成端到端验收。

## PR 正文

### 要解决的问题

AxVisor 已能为 x86_64 guest 选择 UEFI 启动、装载外部固件并从 reset vector `0xfffffff0` 开始执行，但原有流程存在以下问题：

- setup 会扫描宿主发行版的 `/usr/share/OVMF`，不同开发机和 CI 可能使用不同版本、构建参数或布局的固件；
- 固件装载地址会随输入文件变化，无法形成稳定的平台契约；
- `VM[1] boot success` 只表示宿主启动了 VM，不能证明 guest 固件进入 SEC，更不能证明 guest OS 启动；
- 原始串口日志会混合宿主和多个 guest 的输出，不能可靠归属于目标 VM；
- 固件停在 SEC、PEI、DXE 或 BDS 时，缺少统一的阶段标记和设备访问证据。

本 PR 实现 P0 固件入口诊断能力：固定并验证一套 OVMF DEBUG profile，严格装载 CODE窗口，通过目标 VM 的 Guest COM1 判断是否进入 SEC，并保留后续平台缺口所需的 Trace。

### 主要变更

#### 1. 固定 OVMF DEBUG profile

- 固定资产名为 `qemu_x86_64_axvisor_ovmf_debug`。
- 固定 EDK2 `edk2-stable202605`，commit`b03a21a63e3bd001f52c527e5a57feddb53a690b`。
- 使用标准 `OvmfPkg/OvmfPkgX64.dsc`、X64、DEBUG、GCC、4 MiB FD 和 COM1 debug输出。
- 关闭 SMM、Secure Boot、TPM、网络、SD card 和 confidential-computing measurement。
- 固定产物布局：

  | 文件 | 大小 | P0 用途 |
  | --- | ---: | --- |
  | `OVMF_CODE.fd` | `0x37c000` | 映射到 `0xffc84000..0xffffffff` |
  | `OVMF_VARS.fd` | `0x84000` | 仅作为发布模板和参考 QEMU 输入 |
  | `OVMF.fd` | `0x400000` | 仅用于结构及参考验证 |
  | `manifest.toml` | 不定 | 保存来源、布局、构建参数和文件哈希 |

P0 只把 CODE 装载到 guest，不把 VARS 当作普通 RAM 映射，避免承诺尚未实现的 flash
写入、擦除和持久化语义。

#### 2. 严格校验固件资产

- 默认 setup 只从 tgosimages 获取固定 bundle，不再扫描发行版 OVMF 路径。
- 增加受限 flat TOML manifest parser，拒绝重复 key、table、quoted key、array、inline table 和多行值。
- 消费端校验 profile 身份、EDK2 revision、构建开关、布局、单文件大小和 SHA-256。
- 校验 `OVMF_VARS.fd + OVMF_CODE.fd == OVMF.fd`，并确认 reset vector 位于 CODE 覆盖范围。
- 本地固件必须同时设置固件路径和`AXVISOR_X86_64_UEFI_ALLOW_UNVERIFIED=1`；setup 会输出 `UNVERIFIED` 和实际SHA-256，并生成与 verified 配置隔离的文件。
- `quick-start.sh setup` 原子记录本次生成的配置；后续 `run` 使用该记录，不根据新进程的环境变量重新推导配置。失败的 setup 不会继续使用陈旧选择。

#### 3. 固定 CODE 装载契约

- 沿用现有 VM 配置字段，不增加公开配置接口。
- 所有 x86 UEFI 配置保持 `entry_point = 0xfffffff0`，并注册 `x86-com1`。
- AxVM 只接受 `bios_load_addr = 0xffc84000`、大小 `0x37c000` 的固定 CODE 布局，不再根据任意输入文件推测地址窗口。
- 装载成功时记录 profile、verification、路径、实际哈希、大小、GPA 区间和 reset vector。
- 固件成功复制后，以弱引用记录精确的 AxVM 实例资格，不延长 VM 生命周期，也不让相同 VM ID 的其他实例继承资格。

#### 4. 用 Guest COM1 建立 VM 限定的 SEC 判定

- 只有已经装载固定 profile、完成注册并进入首次 vCPU 运行的精确 AxVM 实例才会激活 SEC matcher。
- matcher 支持 marker 跨多次串口写入；多 vCPU 重复激活保持幂等，不会清空已匹配前缀。
- reset 后由同一实例资格重新激活，最后一个 vCPU 退出时清理 matcher。
- prepare/register 失败、普通 Guest COM1 输出和相同 ID 的未注册实例不会产生成功状态。
- 只有完整观察到 `SecCoreStartupWithStack(` 后才输出：

  ```text
  VM[<id>] guest COM1 reached OVMF SEC: SecCoreStartupWithStack(
  ```

`ovmf-entry` 只匹配这条 VM-qualified 边界，不匹配原始串口片段或宿主 VM 启动日志。

#### 5. 补齐统一设备访问 Trace

- 在最新 `DeviceRuntime`/`Device::access` 统一路径记录设备名、总线类型、读写方向、地址、宽度、值和结果。
- 同时覆盖普通 PIO/MMIO/SysReg dispatch 和带 guest-memory capability 的 MMIO write 路径。
- Trace 不绕过 width-aware resource lookup。
- 未注册或不支持的资源继续返回 typed error；不会伪造全 1 读取或静默忽略写入。

#### 6. 拆分 UEFI 测试语义

- 新增 `ovmf-entry-vmx` 和 `ovmf-entry-svm` 诊断用例，其成功条件只能来自目标 VM 的 Guest COM1 SEC 边界。
- 两个用例使用与最新 `dev` 一致的 host NVMe rootfs 配置；VMX/SVM 后端仍由运行时 CPUID 选择，build config 保持 backend-neutral。
- `qemu-nimbos` 不再接受 `VM[1] boot success`，只保留 NimbOS 自有 `usertests passed!` 作为未来完整启动成功条件。
- UEFI 用例在完整平台能力和真实资产证据闭合前保持 non-gating。
- direct VMX/SVM smoke 和既有 multiboot 回归语义不变。

#### 7. 文档与调试流程

- 增加 x86 guest OVMF profile、verified/unverified 用法、阶段 marker、超时分类和参考资格测试说明。
- 同步 quick-start、CI 文档和 `arch-platform-porting` UEFI 调试参考。
- 固定阶段标记为：
  - SEC：`SecCoreStartupWithStack(`
  - PEI：`Platform PEIM Loaded`
  - DXE：`DXE IPL Entry` 或 `Loading DXE CORE at`
  - BDS：`[BdsDxe]`
  - OS：EFI 应用或 guest OS 自有成功标记

### 实现逻辑

1. registry 的归档校验不能替代消费端校验，因此 setup 仍逐项验证 manifest、布局和文件内容。
2. P0 只证明固定 CODE 能进入 SEC；VARS、pflash 和完整 q35 平台属于新的设备语义，不通过 RAM 映射或兼容返回值提前模拟。
3. 成功状态归 guest 所有。固件 loader 成功和 VM 进入 Running 都不是 guest bootsuccess。
4. SEC observer 绑定精确 AxVM 实例和 lifecycle，避免初始化失败、VM ID 重用、reset 和多 vCPU 场景污染全局诊断状态。
5. Trace 保持现有设备框架的 lookup、capability 和 typed-error 语义，只增加诊断证据。
6. host NVMe 只承载 AxVisor rootfs；后续 guest virtio-blk 是独立的虚拟设备能力，两者不混用。

### 范围与兼容性

本 PR：

- 不增加公开 VM 配置字段；
- 不加载或模拟 VARS；
- 不实现 pflash、x86 fw_cfg、ACPI/q35、PCI、guest virtio-blk、PM、SMM、Secure
  Boot 或 TPM；
- 不把 `ovmf-entry` 解释为 guest OS 已经启动；
- 不要求 `qemu-nimbos` 在 P0 通过；
- 不修改 direct VMX/SVM smoke 的成功语义。

用户可见的行为变化是：AxVisor x86 guest UEFI setup 默认不再搜索宿主发行版 OVMF，
而是要求固定 tgosimages bundle；本地同布局固件必须显式进入 unverified 模式。

### 已完成验证

当前实现基于：

```text
upstream/dev@024ecca10a4240a84b2c24bed2dc2361a6043d3e
uefi/p0@941134916
```

已执行并通过：

```text
cargo fmt
git diff --check
bash -n os/axvisor/scripts/ovmf-profile.sh \
  os/axvisor/scripts/setup_qemu.sh \
  os/axvisor/scripts/quick-start.sh

cargo test -p axbuild --lib
# 836 passed

cargo test -p axdevice --no-default-features --features host-test
# unit: 41 passed
# error contract: 2 passed

cargo test -p axvm --no-default-features --features host-test
# unit: 160 passed
# architecture boundary: 19 passed
# error contract: 4 passed

cargo xtask clippy --package axbuild
cargo xtask clippy --package axdevice
cargo xtask clippy --package axvm
```

targeted clippy 覆盖：

- `axbuild`：1 个配置；
- `axdevice`：base、host-test，共 2 个配置；
- `axvm`：base、fs、host-fs、host-test、sstc、tls，共 6 个配置。

测试发现验证：

```text
cargo xtask axvisor test qemu --list --arch x86_64
# normal/smoke-svm
# normal/smoke-vmx

cargo xtask axvisor test qemu --list --arch x86_64 --test-group uefi
# uefi/ovmf-entry-svm
# uefi/ovmf-entry-vmx
# uefi/qemu-nimbos
```

### 尚缺的外部及端到端证据

以下项目尚未完成，不能由合成 bundle 或单元测试替代：

- 确认 `qemu_x86_64_axvisor_ovmf_debug` 已在 tgosimages 发布并可由 setup 拉取；
- 在同一不可变容器 digest 的两个干净 workspace 构建，证明 CODE、VARS 和合并镜像逐字节一致；
- 检查固定大小、拼接关系、reset vector，并确认构建报告包含 Shell、`VirtioPciDeviceDxe`、`Virtio10Dxe` 和 `VirtioBlkDxe`；
- 标准 QEMU q35 + TCG 下依次取得 SEC、PEI、DXE、BDS 和`AXVISOR_OVMF_REFERENCE_BOOT_OK`；
- 完成参考 QEMU 二次启动，确认工作 VARS 可变化而发布模板哈希不变；
- 在 Intel/VMX 和 AMD/SVM 上分别运行 direct smoke；
- 在 Intel/VMX 和 AMD/SVM 上分别运行 `ovmf-entry`，取得 VM-qualified SECmarker；
- 保存完整 Trace，并记录 SEC 后首个未支持的平台访问。

完成上述证据前，本 PR 应保持 Draft；这些证据齐备后才能宣称 P0 验收完成。

### 建议审阅重点

- manifest parser 是否严格拒绝歧义或未支持的 TOML 结构；
- verified/unverified 配置和跨进程 `.selected` 状态是否可能串用；
- 固定 CODE 布局是否在 setup、VM config 和 loader 三层保持一致；
- SEC matcher 是否只绑定已注册的精确 AxVM 实例，并正确处理多 vCPU、reset、失败
  初始化和 VM ID 重用；
- Trace 是否覆盖普通 dispatch 与 DMA-capable MMIO write，且没有改变设备错误语义；
- `ovmf-entry`、`qemu-nimbos` 和 direct VMX/SVM smoke 的成功条件是否保持隔离；
- host NVMe 与未来 guest virtio 设备是否保持分层。
