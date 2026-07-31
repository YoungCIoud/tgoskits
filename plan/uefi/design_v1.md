# AxVisor x86_64 Guest UEFI 支持 — 设计方案

> 设计基线：`upstream/dev@024ecca10a4240a84b2c24bed2dc2361a6043d3e`
>（2026-07-31，`axvisor: virtualize AArch64 physical timer state (#1770)`）。
>
> P0 功能来源：`uefi/p0@52b2edc5ad829d2e7be1973cd237cfe709c4cca2`
>（2026-07-22，`feat(axvisor): add reproducible OVMF debug profile`）。
>
> 状态说明：P0 的功能契约和旧基线实现已经形成，但尚未重放到上述最新
> `dev`，也尚未取得固定 OVMF 资产发布、QEMU 参考启动以及 VMX/SVM 实机启动证据。
> 本文以最新 `dev` 的运行时接口为准；旧实现仅作为待迁移实现，不作为接口基线。

## 1. 问题与范围

### 1.1 要解决的问题

AxVisor 已能为 x86_64 guest 选择 `boot_protocol = "uefi"`、加载外部固件并从
`0xfffffff0` 进入，但当前 `dev` 仍会扫描发行版 OVMF，测试也仍可能使用宿主侧
`VM[1] boot success` 作为成功标记。这不能回答以下问题：

- 实际加载了哪一版、哪一组构建开关和哪一个布局的 OVMF；
- 固件是否真正进入 SEC，以及停在 SEC、PEI、DXE、BDS 还是 OS handoff；
- 失败来自固件入口、平台发现、设备枚举还是启动介质；
- 不同开发机和 CI 使用的固件是否可复现。

P0 因此只建立一条可复现、可归因的诊断链：

```text
固定 OVMF 资产
    -> manifest/文件校验
    -> CODE 固定 GPA 装载
    -> reset vector
    -> Guest COM1
    -> SEC/PEI/DXE/BDS/OS 阶段判定
```

### 1.2 P0 用户与成功标准

用户包括 AxVisor x86 平台开发者、固件/虚拟设备开发者和 UEFI CI 维护者。

P0 成功必须同时满足：

1. 默认路径只接受固定 EDK2 profile，不依赖发行版 `/usr/share/OVMF`。
2. setup 校验 profile、布局、大小和每个文件的 SHA-256。
3. AxVisor 只映射固定 CODE 窗口，并记录 profile、文件、大小、GPA 和 reset vector。
4. `ovmf-entry` 只能由目标 VM 的 Guest COM1 完整 SEC 标记判定成功。
5. 完整 guest 用例只能由 EFI 应用或 guest OS 自身标记判定成功。
6. Trace 能保留首个未实现平台访问，不伪造全 1 读取，也不静默丢弃写入。
7. 固定产物通过标准 QEMU q35 资格测试，并在 VMX、SVM 各取得一次 AxVisor
   SEC 入口证据。

### 1.3 P0 非目标

P0 不实现：

- pflash 设备、写时复制 VARS 或 CODE/VARS 生命周期；
- x86 fw_cfg PIO、ACPI 生成、完整 q35 平台；
- PCI ECAM/配置空间、PCI BAR 分配或热插拔；
- guest virtio-blk、virtio-pci、INTx、MSI/MSI-X；
- Secure Boot、SMM、TPM、网络或 PM 设备；
- 新的公开 VM 配置字段；
- LoongArch UEFI 功能。

这些能力按后续阶段独立设计和验证，不能通过放宽 P0 成功条件提前“模拟成功”。

## 2. 最新 dev 的现状

### 2.1 已具备的基础

最新 `dev` 已提供：

- `axvmconfig` 的 `VMBootProtocol::Uefi`、`uefi_firmware_path`、
  `bios_load_addr` 和架构相关配置校验；
- x86 外部 UEFI 固件装载与 reset vector 入口；
- 每 VM 的 `DeviceRuntime`、`DeviceFactoryRegistry`、`DeviceBundle` 和静态设备拓扑
  sealing；
- 统一的 `Device::access(&BusAccess, &mut dyn DeviceAccess)` 访问协议；
- width-aware 的 PIO/MMIO/SysReg 路由；
- DMA、Timer、Wake、Stop 的显式 grant 和 access-scoped runtime port；
- typed `DeviceServices`，包括 x86 serial、PIT、IOAPIC 和 VM 本地
  `X86InterruptDomainKey`；
- `VmInterruptSender`、每 vCPU 待处理中断队列和运行时 dispatcher；
- `ArchOps::before_first_run(&AxVMRef, ...)` 与
  `ArchOps::on_last_vcpu_exit(&AxVMRef)`；
- 已迁移到统一设备框架的 MMIO fw_cfg transport、DMA grant、payload factory 和
  ACPI blob 容器；当前架构接入主要服务于 LoongArch，不能等同于 x86 q35 fw_cfg；
- x86 Guest COM1 输出通路；
- x86 UEFI VM 配置和 `qemu-nimbos` test-suit 框架。

### 2.2 仍存在的缺口

最新 `dev` 仍有以下 P0 缺口：

- `setup_qemu.sh` 和 `quick-start.sh` 会扫描发行版 OVMF 路径；
- UEFI 固件版本、构建开关、文件布局和哈希未形成固定契约；
- 默认配置以 `bios_load_addr = 0xffc00000` 接受任意固件，未固定 CODE 的真实
  `0xffc84000..0xffffffff` 布局；
- 没有与 VM 实例绑定的 SEC marker observer；
- `qemu-nimbos` 仍以宿主日志 `VM[1] boot success` 判定成功；
- UEFI AxVisor CI job 仍处于注释状态，不是门禁；
- 现有设备 trace 不完整覆盖“设备名、方向、地址、宽度和值”的 P0 契约；
- 固定 tgosimages 资产、参考 QEMU 资格结果和 VMX/SVM 入口证据尚不存在。

### 2.3 dev 更新对旧 P0 实现的影响

| dev 更新 | 对旧 P0 的影响 | 同步原则 |
| --- | --- | --- |
| 统一设备框架（#1722） | 旧实现修改 `AxVmDevices`/`Device::handle` 的部分不能直接保留 | 在 `DeviceRuntime` 的统一访问路径上重放 trace |
| access-scoped DMA/grant | MMIO DMA write 存在独立入口 | `dispatch()` 与 `handle_mmio_write_with_memory()` 都必须覆盖 trace |
| VM 本地 x86 中断域 | 旧的全局 IRQ 假设失效 | 后续设备只通过 typed service 和 VM 本地 domain 注入 |
| `VmInterruptSender` 与 vCPU 队列 | 轮询式或按全局 VM ID 注入的设计过时 | 后续 IRQ 使用稳定 sender 和 runtime dispatcher |
| lifecycle callback 接收 `AxVMRef` | SEC observer 不再需要仅凭 VM ID 回查 | observer 的启停绑定精确 VM 实例 |
| `with_current_vcpu` | 旧 `get_current_vcpu` 调用失效 | COM1 输出路径按新 CPU-local API 适配 |
| AxVisor host rootfs 迁移到 NVMe（#1784） | P0 新增的 `ovmf-entry` 配置仍使用 virtio-blk | rebase 后 host 构建与 QEMU 参数统一改为 NVMe |
| CI router 与 axloader OVMF 重试 | 与 P0 策略无直接冲突 | 保留这些 dev 修复，不覆盖相关 workflow |

这里的“host NVMe”是承载 AxVisor 自身 rootfs 的 QEMU 设备。后续“guest
virtio-blk”是 AxVisor 向 guest 模拟的启动设备，两者属于不同层，不能互相替代。

## 3. P0 固定 OVMF 契约

### 3.1 固件 profile

固定资产名：

```text
qemu_x86_64_axvisor_ovmf_debug
```

固定源码和构建参数：

```text
tag: edk2-stable202605
commit: b03a21a63e3bd001f52c527e5a57feddb53a690b
architecture: X64
target: DEBUG
toolchain: GCC
platform: OvmfPkg/OvmfPkgX64.dsc
defines:
  FD_SIZE_4MB
  DEBUG_ON_SERIAL_PORT
  BUILD_SHELL=TRUE
  SMM_REQUIRE=FALSE
  SECURE_BOOT_ENABLE=FALSE
  TPM2_ENABLE=FALSE
  NETWORK_ENABLE=FALSE
  SDCARD_ENABLE=FALSE
  CC_MEASUREMENT_ENABLE=FALSE
```

选择标准 `OvmfPkgX64` 而非 MicroVM 变体，以保持后续 q35/PCI/virtio 路线一致。
选择 COM1 而非默认 `0x402` debugcon，因为 AxVisor 已有 Guest COM1 模拟设备。
关闭 SMM 和 Secure Boot，因为 SMM profile 依赖 q35 SMM 与 pflash varstore，超出 P0。

### 3.2 资产与地址布局

| 文件 | 大小 | GPA/用途 |
| --- | ---: | --- |
| `OVMF_CODE.fd` | `0x37c000` | `0xffc84000..0xffffffff`，P0 唯一映射的固件 |
| `OVMF_VARS.fd` | `0x84000` | `0xffc00000..0xffc83fff`，仅模板和参考 QEMU 使用 |
| `OVMF.fd` | `0x400000` | 必须等于 `VARS + CODE`，仅结构和参考验证使用 |
| `manifest.toml` | 不定 | 来源、构建环境、布局、哈希、开关和阶段标记 |

reset vector 固定为 `0xfffffff0`，必须落在 CODE 最后 16 字节覆盖范围内。

P0 不把 VARS 映射给 AxVisor guest。把 VARS 当普通 RAM 装载会错误承诺变量服务的
flash 写入/擦除语义，因此 VARS 必须留到 pflash 阶段。

### 3.3 manifest 格式和校验

manifest 使用 setup 可无额外 TOML 工具解析的受限 flat TOML：

- 每个非注释行只能包含一个唯一 bare key；
- value 只能是单行字符串、布尔值或无符号整数；
- 拒绝 table、quoted key、array、inline table、多行值和重复 key。

必需字段包括：

- 身份与来源：`schema_version`、`profile`、`edk2_tag`、`edk2_commit`、
  `architecture`、`target`、`toolchain`、`platform`；
- 可复现信息：`build_command`、`build_container_digest`、`tool_versions`、
  `submodule_commits`；
- 布局：`code_base`、`code_size`、`vars_base`、`vars_size`、
  `combined_size`、`reset_vector`；
- 文件：三个文件名及各自 SHA-256；
- 所有固定构建开关；
- SEC、PEI、DXE IPL、DXE CORE 和 BDS marker。

tgosimages registry 校验归档 SHA-256；tgoskits 仍需校验 manifest、固定字段、
单文件大小/哈希、`VARS + CODE == OVMF.fd` 和 reset vector 覆盖关系。registry
校验不能替代消费端校验。

### 3.4 verified 与 unverified 路径

默认路径只从 tgosimages 获取并接受固定 bundle，不扫描发行版固件。

开发者使用本地固件时必须同时设置：

```bash
AXVISOR_X86_64_UEFI_FIRMWARE=/absolute/path/to/OVMF_CODE.fd
AXVISOR_X86_64_UEFI_ALLOW_UNVERIFIED=1
```

unverified 固件仍必须满足固定 CODE 大小和布局。setup 必须输出 `UNVERIFIED` 和
实际 SHA-256，并生成独立的 unverified VM config。test-suit 只引用 verified
config；unverified 运行结果不得用于 UEFI 测试判定。

`quick-start.sh setup` 用每 guest `.selected` 文件记录本次实际生成的 config。
后续 `run` 读取该记录，不重新解释另一进程的环境变量。setup 开始时先清除旧记录，
只有成功后才发布新记录，避免失败后误启动 stale config。

## 4. P0 运行时设计

### 4.1 固件装载

setup 生成的 VM 配置继续使用现有字段，不增加公开配置面：

```toml
[kernel]
enable_bios = true
boot_protocol = "uefi"
entry_point = 0xffff_fff0
uefi_firmware_path = ".../OVMF_CODE.fd"
bios_load_addr = 0xffc8_4000
```

内存窗口严格来自固定 profile，而不是依据输入文件长度反推布局。加载成功后至少记录：

```text
profile=<name>
verification=VERIFIED|UNVERIFIED
path=<actual path>
sha256=<actual hash>
size=0x37c000
gpa=[0xffc84000, 0x100000000)
reset_vector=0xfffffff0
```

只有通过固定 profile 校验并成功映射的 AxVM 实例，才能持有
`fixed_ovmf_profile_loaded` 状态。普通 UEFI 配置、任意同大小文件或仅 VM ID 相同的
另一个实例不能激活 observer。

### 4.2 Guest COM1 与阶段 observer

所有 x86 UEFI VM 配置注册 `x86-com1`，资源为 `0x3f8..0x3ff`。

observer 生命周期：

1. firmware loader 在精确 AxVM 实例上记录 fixed profile 已加载；
2. VM 完成 prepare、进入 Running 后，
   `ArchOps::before_first_run(&AxVMRef, ...)` 激活该实例 observer；
3. 重复 vCPU 激活必须幂等，不能清空已经匹配的 marker 前缀；
4. reset 后在下一次 first-run 重新激活；
5. `ArchOps::on_last_vcpu_exit(&AxVMRef)` 清理该实例 observer；
6. prepare/注册失败、普通 guest COM1 输出和同 ID 的旧实例均不能创建成功状态。

Guest COM1 字节仍通过 x86 serial service 输出到宿主 console，但成功判定发生在字节
离开目标 VM 的模拟 COM1 时，而不是扫描混合后的 QEMU transcript。

完整 SEC marker：

```text
SecCoreStartupWithStack(
```

匹配后输出 VM 限定边界：

```text
VM[<id>] guest COM1 reached OVMF SEC: SecCoreStartupWithStack(
```

`ovmf-entry` 只匹配这条边界，不匹配原始串口片段。

### 4.3 Trace 设计

最新 dev 的统一入口是 `DeviceRuntime`，不再存在可继续扩展的旧
`AxVmDevices::handle` 路线。Trace 至少记录：

```text
device=<name>
bus=PIO|MMIO|SYSREG
direction=read|write
addr=<address>
width=<width>
value=<write value or read result>
result=<ok or typed error>
```

实现时需要覆盖两条路径：

1. 普通 `BusRouter::dispatch()`；
2. 带 guest-memory capability 的 `handle_mmio_write_with_memory()`。

日志不能绕过 width-aware lookup，也不能为未知资源生成兼容值。未注册资源继续返回
`DeviceError::NotFound` 或上层对应 typed error。

### 4.4 阶段标记与故障分类

阶段顺序：

1. SEC：`SecCoreStartupWithStack(`
2. PEI：`Platform PEIM Loaded`
3. DXE：`DXE IPL Entry` 或 `Loading DXE CORE at`
4. BDS：`[BdsDxe]`
5. OS：EFI 应用或 guest OS 的专有成功标记

超时分类：

| 最后观测 | 分类 |
| --- | --- |
| 无 SEC | CODE 映射、reset vector、vCPU 入口或早期串口 |
| SEC/PEI | 平台发现或内存初始化 |
| DXE | 固件设备枚举 |
| BDS | 启动源、EFI 应用或 OS handoff |

## 5. 测试与 CI 语义

### 5.1 用例拆分

`ovmf-entry-vmx` 和 `ovmf-entry-svm`：

- 目标是证明固定 CODE 进入 SEC；
- 成功只来自目标 VM Guest COM1 的 VM-qualified SEC 边界；
- 当前为 non-gating diagnostic；
- 保存完整 Trace，记录 SEC 后首个未支持访问。

`qemu-nimbos`：

- 目标是证明 guest/OS 完整启动；
- 最终成功标记为 NimbOS 自身的 `usertests passed!`；
- 禁止接受 `VM[1] boot success`；
- 在实际启动链完成前保持非门禁。

direct VMX/SVM smoke 和现有 multiboot 单元回归保持其原有语义，不因 UEFI P0
放宽或替换。

### 5.2 与最新 dev CI 的合并规则

- 保留 dev 的 workflow router、`since_ref` 和自托管 runner 修复；
- 保留 axloader OVMF 查找/安装与重试，因为它不属于 AxVisor guest 固件契约；
- AxVisor UEFI job 不再扫描 `/usr/share/OVMF`，改为固定 tgosimages bundle；
- self-hosted matrix 的 `cache_key` 保持空字符串；
- host rootfs 构建 feature 使用 `ax-driver/nvme`；
- QEMU host 磁盘参数使用 dev 当前 NVMe 形式：

  ```text
  -device nvme,drive=disk0,serial=tgoskits,max_ioqpairs=64,msix_qsize=65
  ```

- `ovmf-entry` 新增配置也必须跟随 host NVMe 基线，不能保留旧 virtio-blk host 参数。

## 6. 分阶段路线

### P0-A：在最新 dev 上重放实现

目标是让旧 P0 功能在 `upstream/dev@024ecca10` 的接口上编译并保持语义不变。

工作项：

- 解决 `DeviceRuntime`、x86 arch lifecycle 和 `AxVMResources` 的冲突；
- 将 trace 适配到统一访问路径和 DMA-capable MMIO write 路径；
- 将 SEC observer 适配到 `AxVMRef` lifecycle 与 `with_current_vcpu`；
- 在 dev 新增字段旁加入 fixed-profile 状态，不覆盖
  `pending_fw_cfg_payload`、device runtime ports 或中断 runtime；
- 将全部 P0 test-suit host 存储配置迁移到 NVMe；
- 保留 dev 的 VM 本地 interrupt domain、capability grant 和 CI 改进。

退出条件：

- rebase 后无旧 `AxVmDevices`/`Device::handle` 残留；
- P0 单元测试、shell syntax、axbuild test discovery 通过；
- 相关 crate fmt 和 targeted clippy 通过；
- direct VMX/SVM smoke 无回归。

### P0-B：固定资产资格与入口证据

目标是补齐当前缺少的外部证据。

固件构建验证：

- 在同一不可变容器 digest 的两个干净 workspace 构建；
- CODE、VARS、合并镜像分别逐字节一致；
- 大小分别为 `0x37c000`、`0x84000`、`0x400000`；
- `OVMF_VARS.fd + OVMF_CODE.fd` 与 `OVMF.fd` 完全一致；
- reset vector 落在 CODE 最后 16 字节；
- 报告确认 Shell、`VirtioPciDeviceDxe`、`Virtio10Dxe`、`VirtioBlkDxe`。

标准 QEMU 资格测试：

- q35 + TCG + SMM off；
- CODE 为只读 pflash，VARS 使用工作副本；
- virtio-blk FAT ESP 的 `startup.nsh` 输出
  `AXVISOR_OVMF_REFERENCE_BOOT_OK`；
- 日志依次出现 SEC、PEI、DXE、BDS 和 reference marker；
- 第二次启动允许工作 VARS 改变，但发布 VARS template 哈希不变。

AxVisor 入口验证：

- setup 拒绝错误 hash、size、CODE base 和未显式允许的外部固件；
- VMX、SVM 各运行 `ovmf-entry` 并看到 loader 日志和 SEC 边界；
- 归档完整 Trace 和 SEC 后首个未支持访问。

### P1：pflash 与 CODE/VARS 生命周期

P1 才把只读 CODE 和每 VM 可写 VARS 建模为 flash，而不是普通 RAM：

- 定义 read-only CODE 与 writable/eraseable VARS 的设备边界；
- 每 VM 从已验证 template 创建工作副本；
- reset 保留变量、销毁释放工作副本；
- 明确并发、持久化、异常中断和恢复语义；
- 参考 QEMU 的 CODE/VARS 行为建立确定性测试。

P1 属于新的硬件能力和持久化语义，需要单独设计审阅。不能只增加第二个 firmware
路径字段后直接映射。

### P2：x86 平台发现

在 P0 Trace 证据确认需求后，补齐固件从 PEI/DXE 前进所需的最小平台：

- 复用现有 `FwCfg` payload、ACPI blob 和 DMA grant 逻辑；
- 新增 x86 q35 所需的 PIO transport/adapter，而不是复制 fw_cfg 核心；
- ACPI/AML 内容由 x86 平台层生成，fw_cfg 只负责传输；
- 通过 `DeviceFactory`、`DeviceBundle` 和 typed services 注册；
- 未支持 selector、宽度或 DMA 操作返回明确错误。

退出条件是阶段 marker 从 SEC/PEI 稳定推进到 DXE/BDS，并由 Trace 证明没有依赖
伪造访问。

### P3：虚拟 PCI 与 guest 启动盘

- 实现最小 PCI host bridge/ECAM 或经证据确认的 q35 配置访问；
- virtio-pci 与 virtio-blk 通过统一设备框架注册；
- BAR、capability、queue、DMA 和 reset 分层；
- guest memory 访问只通过显式 DMA grant；
- guest 启动盘后端与 host NVMe rootfs transport 分离。

退出条件是 OVMF 能枚举块设备、读取 ESP 并进入 EFI 应用。

### P4：中断、SMP 与性能路径

- INTx 通过 VM 本地 `X86InterruptDomainKey`；
- vCPU 投递使用 `VmInterruptSender` 和 runtime dispatcher；
- guest GSI/vector 与 host `IrqId` 保持不同命名空间；
- MSI/MSI-X 在 PCI/virtio 基础稳定后增加；
- 不恢复按 VM ID 的全局 forwarding state 或轮询式注入。

退出条件包括 SMP guest、重复 start/stop/reset、路由 teardown 和中断风暴回归。

### P5：平台完整性与 LoongArch 收敛

- 按实际固件访问补 PM/reset/RTC 等设备，不预先伪造 q35；
- Secure Boot、SMM 和 TPM 作为独立高风险能力；
- 在 x86 契约稳定后抽取可复用的固件资产、阶段诊断和测试编排边界；
- LoongArch 保留其 MMIO fw_cfg、FDT/ACPI 和中断平台差异，不强行共享地址布局。

## 7. 关键设计决策

1. **最新 dev 是运行时接口唯一基线。** P0 旧提交只提供行为意图，不能覆盖
   `DeviceRuntime`、capability grant 或 interrupt runtime。
2. **P0 固定 4 MiB OVMF profile，但只映射 CODE。** 发布 VARS 不等于实现
   VARS。
3. **不新增 P0 公开 VM 配置字段。** 固定布局由 setup/profile 生成到现有字段。
4. **默认不接受发行版固件。** 本地固件必须显式 unverified opt-in，且不能进入
   test-suit 判定。
5. **成功标记归 guest 所有。** loader 成功和 VM 进入 Running 都不是 guest boot
   成功。
6. **诊断不改变设备语义。** 未知访问继续失败；Trace 只增加证据。
7. **后续设备遵守统一框架。** factory、bundle、typed service、grant、sealed
   topology 和 typed error 是必需边界。
8. **中断状态按 VM 所有。** 不引入全局 guest IRQ 路由或混淆 host/guest IRQ
   namespace。
9. **host NVMe 与 guest virtio 分层。** CI 基础设施变化不能决定 guest 平台模型。
10. **P1 以后按高风险功能重新审阅。** pflash、PCI、DMA 和 IRQ 均需要各自的设计与
    最低层确定性回归。

## 8. 变更落点

P0 已有实现涉及：

- `os/axvisor/scripts/ovmf-profile.sh`
- `os/axvisor/scripts/setup_qemu.sh`
- `os/axvisor/scripts/quick-start.sh`
- `os/axvisor/configs/vms/qemu/x86_64/*-uefi-smp1.toml`
- `os/axvisor/doc/x86-uefi-profile.md`
- `scripts/axbuild/src/axvisor/test/ovmf_tests.rs`
- `test-suit/axvisor/uefi/ovmf-entry/`
- `test-suit/axvisor/uefi/qemu-nimbos/`
- `virtualization/axvm/src/arch/x86_64/boot/mod.rs`
- `virtualization/axvm/src/arch/x86_64/guest_serial.rs`
- `virtualization/axvm/src/arch/x86_64/mod.rs`
- `virtualization/axvm/src/vm/mod.rs`
- `virtualization/axdevice/src/device.rs`
- `.github/workflows/ci.yml`
- `docs/docs/build/ci.md`
- `.claude/skills/arch-platform-porting/references/boot-debugging.md`

rebase 时优先围绕最新 dev 的以下边界适配：

- `virtualization/axdevice/src/device.rs`：`DeviceRuntime` 路由与 capability context；
- `virtualization/axvm/src/vm/mod.rs`：VM 固定 profile 状态；
- `virtualization/axvm/src/arch/x86_64/mod.rs`：lifecycle 与 Guest COM1 hook；
- `virtualization/axvm/src/arch/x86_64/boot/mod.rs`：严格 CODE layout；
- `test-suit/axvisor/uefi/`：host NVMe 和 marker 语义。

不应恢复或新建旧式 `AxVmDevices`、`BaseDeviceOps`、全局 IRQ forwarding map。

## 9. 验证矩阵

| 层次 | 验证 | P0 gate |
| --- | --- | --- |
| manifest parser | 合法 bundle；重复 key/table/quoted key/错误 hash/size/base | 是 |
| setup | verified pull；unverified 双开关；stale `.selected` 隔离 | 是 |
| loader | 固定 CODE window；reset vector；实例状态 | 是 |
| observer | 跨写入 marker；重复 vCPU；reset；同 ID 不同实例；普通 COM1 | 是 |
| device trace | PIO/MMIO/SysReg；read/write；DMA write；typed error | 是 |
| Rust 静态检查 | fmt、targeted clippy、axbuild lib tests | 是 |
| 回归 | direct VMX/SVM smoke、multiboot tests | 是 |
| 标准 QEMU | SEC→PEI→DXE→BDS→reference marker；二次启动 | 发布 gate |
| AxVisor VMX/SVM | 两种后端各进入 SEC 并保存 Trace | P0 完成 gate |
| NimbOS UEFI | `usertests passed!` | 后续 gate |

文档或 shell-only 变更不需要无关 crate clippy；一旦修改 Rust 逻辑，必须对每个受影响
crate 执行 `cargo fmt` 和 `cargo xtask clippy --package <crate>`，并运行最低层回归。

## 10. 风险与证据缺口

### 10.1 当前阻塞

- 固定 `qemu_x86_64_axvisor_ovmf_debug` bundle 尚未确认已在 tgosimages 发布；
- 尚无双 workspace 可复现构建报告；
- 尚无标准 QEMU q35 全阶段参考日志；
- 尚无基于最新 dev 的 VMX/SVM SEC 入口日志；
- `uefi/p0` 比最新 dev 落后 57 个提交，存在 4 个文本冲突和额外语义冲突；
- 新增 `ovmf-entry` 的 host 存储配置尚未迁移到 dev 的 NVMe 基线。

在这些证据补齐前，只能说“P0 设计和旧基线实现已完成”，不能说“最新 dev 上 P0
验收完成”。

### 10.2 版本降级规则

首先只验证 `edk2-stable202605`：

- 若标准 QEMU q35 失败，先判定并修复构建或测试环境，不降级；
- 若标准 QEMU 通过而 AxVisor 无法进入 SEC，用相同参数构建
  `edk2-stable202602` 对照；
- 两版都失败：判为 AxVisor 平台问题，仍固定 202605；
- 仅 202602 成功：确认上游回归或首次不兼容访问后，P0 可临时固定 202602，并在
  manifest 记录阻塞 202605 的访问/提交；
- 两版都成功：固定 202605。

### 10.3 安全与正确性风险

- 把 VARS 映射成 RAM 会造成错误的持久化和写保护语义；
- 将宿主 console 文本直接作为成功条件会被宿主日志或其他 VM 污染；
- 在统一设备框架外增加旁路会绕过 resource overlap、width 和 capability 校验；
- 全局 IRQ 状态会导致同 ID VM 重建、并发 VM 和 teardown 污染；
- 为推进阶段而返回全 1 或吞写入会把首个真实平台缺口隐藏到更晚阶段；
- 混用 host NVMe 和 guest virtio 会让 CI 环境偶然决定 guest 平台设计。

## 11. 完成定义

P0 只有在以下项目全部完成后才关闭：

- [ ] P0 实现在最新 dev 上完成 rebase，且无旧设备/IRQ API 残留；
- [ ] 固定 OVMF bundle 发布并通过消费端完整校验；
- [ ] 两个干净 workspace 构建逐字节一致；
- [ ] 标准 QEMU q35 依次到达 SEC、PEI、DXE、BDS 和 reference marker；
- [ ] VMX `ovmf-entry` 到达 VM-qualified SEC marker；
- [ ] SVM `ovmf-entry` 到达 VM-qualified SEC marker；
- [ ] Trace 保存 SEC 后首个未支持平台访问；
- [ ] `qemu-nimbos` 不再接受 `VM[1] boot success`；
- [ ] direct VMX/SVM、multiboot、axbuild tests 和 targeted clippy 无回归；
- [ ] UEFI 调试文档、CI 文档和 `arch-platform-porting` reference 与实现一致。

P0 完成不代表 guest OS 已经通过 UEFI 启动。完整启动必须在后续 pflash、平台发现、
PCI/块设备和中断能力逐阶段具备后，由 guest 自身成功标记证明。
