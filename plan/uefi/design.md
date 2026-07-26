# AxVisor Guest UEFI 支持完善设计方案

> 仓库基线：`130785326`（2026-07-21）。本文只讨论 **AxVisor 客户机（guest）UEFI**。
> `qemu-*.toml` 中的 `uefi = true` 表示 AxVisor 宿主镜像由 QEMU/OVMF 启动，不能作为
> guest UEFI 已可用的证据。

## 1. 结论

现有实现已跨过“能否从 OVMF reset vector 执行”的最初阶段，但还没有形成一个由
AxVisor 自己拥有、可复用的 x86_64 UEFI 虚拟平台：

- x86_64 已有显式 `direct` / `multiboot` / `uefi` 启动协议、外部固件加载、正确的
  reset-vector vCPU 初始状态，以及 vLAPIC、vIOAPIC、PIT、串口和部分宿主 IRQ 转发能力。
- LoongArch 已有 guest UEFI、固件 FDT、fw_cfg MMIO、ACPI、PCI/中断拓扑发现和 Linux
  kernel/initrd/cmdline 传递链路，适合作为“固件数据通道和平台描述”的参考实现。
- x86_64 仍缺少 OVMF 实际依赖的 fw_cfg I/O 端口前端、x86 ACPI/SMBIOS 平台描述、
  VM 自有的 PCI 根总线、virtio-pci 设备、guest 磁盘后端和完整的 PC 兼容设备。
- 现有 x86 direct-boot Linux 冒烟测试依赖外层 QEMU 设备的地址/端口/IRQ 透传。这条路径
  能验证嵌套虚拟化和直接启动，但不是可迁移的 guest UEFI 设备模型。
- x86 guest UEFI CI 已被临时禁用，且现有成功条件只是 `VM[1] boot success`。该日志只说明
  vCPU 任务已启动，不证明 OVMF、EFI 应用或 guest OS 已运行成功。
- LoongArch 的 guest UEFI 实现存在，但当前常规 QEMU 用例只验证 AxVisor shell；没有把
  `linux-rootfs-smp1.toml` 接入自动化 guest 启动门禁。

因此，实现主线应是：

1. 固定一个经过验证的 OVMF/guest machine profile，再固化启动协议、flash 和启动源边界；
2. 将 fw_cfg 拆成架构无关数据核心，并通过统一 `Device` 资源模型提供 x86 PIO 或
   LoongArch MMIO 前端；
3. 建立 x86 guest platform 描述，按已经注册的设备分阶段生成 ACPI，禁止表先于设备出现；
4. 实现 VM 自有 PCI 根总线、modern virtio-pci block 和 VM-local INTx 路由；
5. 用 OVMF Shell、PCI/块设备探测、EFI 应用、Linux 磁盘启动逐级形成真实门禁；
6. 再补齐公共配置 schema 收敛、RTC/PM/reset、MSI/MSI-X、SMP 和更多 virtio 设备。

## 2. 已核实的仓库现状

### 2.1 启动配置与固件装载

| 能力 | 当前状态 | 依据与限制 |
| --- | --- | --- |
| 启动协议 | 已实现 | `axvm-types` 定义 `Direct`、`Multiboot`、`Uefi`；`axvmconfig` 对 x86_64 和 LoongArch UEFI 做架构、固件路径和加载地址校验。 |
| 兼容配置 | 已实现 | `uefi_firmware_path` 优先，旧 `bios_path` 可回退；`enable_bios` 仍是必须配合的历史开关。 |
| x86 OVMF 装载 | 部分实现 | 固件被当作普通只读镜像加载到 `bios_load_addr`，vCPU 以 `CS.base = 0xffff0000, RIP = 0xfff0` 进入 `0xfffffff0`。 |
| LoongArch OVMF 装载 | 已实现 | 固件、FDT、fw_cfg payload 和运行时平台资源已串联。 |
| CODE/VARS 双 flash | 未实现 | 配置只有一个 firmware path 和一个 load address，没有代码区/变量区角色、读写属性和独立地址范围。 |
| flash 命令与持久化 | 未实现 | 当前只是预填 `0xff` 后复制固件字节，没有 CFI/pflash 写入协议，也没有 NVRAM 后端。 |
| 启动源语义 | 不完整 | x86 UEFI 配置仍要求 `kernel_path` 并把镜像预装到 `kernel_load_addr`，但没有把该镜像通过 fw_cfg、磁盘或 EFI 文件系统交给 OVMF；这不是一条闭合启动路径。 |

相关实现：

- `virtualization/axvmconfig/src/lib.rs`
- `virtualization/axvm/src/arch/x86_64/boot/mod.rs`
- `virtualization/x86_vcpu/src/lib.rs`
- `virtualization/axvm/src/arch/loongarch64/boot/mod.rs`
- `os/axvisor/configs/vms/qemu/x86_64/*-uefi-smp1.toml`

### 2.2 固件数据与平台描述

`virtualization/axdevice/src/fw_cfg.rs` 已实现：

- fw_cfg selector/data 状态机；
- MMIO 寄存器和 DMA 访问；
- kernel、initrd、cmdline、内存图和文件目录；
- QEMU table-loader 格式；
- ACPI 表和最小 SMBIOS 终止结构。

但它当前并非架构通用实现：

- 只有 MMIO 传输，x86 OVMF 所需的 `0x510` selector、`0x511` data、`0x514` DMA I/O
  端口尚未实现；
- MMIO selector 是 big-endian，x86 PIO selector 是 little-endian；data register 是
  string-preserving，DMA address/control structure 则保持 QEMU 规定的 big-endian。迁移时不能
  用一套端序分支覆盖所有寄存器；
- `FwCfgPlatformConfig`、DSDT、MADT、FADT、PCI 窗口和中断拓扑均按 LoongArch 编码；
- ACPI 只生成 RSDT，没有 x86 所需的 LAPIC/IOAPIC MADT 项和对应 DSDT 设备；
- `GuestAcpiTables` 所有权类型已经存在于 `axvm/src/boot/policy.rs`，但尚未接入任何平台；
- SMBIOS 当前只有 Type 127 终止结构，可作为占位，不应描述成完整 SMBIOS 支持。

结论：应复用 fw_cfg 的 item 目录、DMA 和 table-loader 核心，但不能直接把现有 MMIO 设备
挂到 x86，也不能让 x86 复用 LoongArch ACPI 字节布局。

### 2.3 x86 CPU、中断和设备模型

已经具备：

- VMX/SVM 在运行时按 CPUID 选择，二者都能建立真实模式 guest 初始状态；
- `x86_vlapic` 提供 vLAPIC、vIOAPIC、PIT、串口核心；
- AxVM 能配置 x86 串口、vIOAPIC、PIT，处理端口 I/O 和 MMIO VM exit；
- vIOAPIC 支持重定向表、level-triggered remote-IRR、EOI 后重注入；
- 宿主 PCI INTx 可解析为 host `IrqId` 并转发到指定 guest GSI；
- 通用设备框架的 `Device::resources()` 已允许同一个设备声明多个 MMIO/PIO 资源，
  `InterruptFabric`/`IrqResolver`/`IrqLine` 也已提供 VM-local IRQ capability 边界；
- Linux direct boot 会生成 boot params 和 MP table，可启动当前嵌套测试用 Linux。

仍然缺少：

- x86 UEFI 配置没有启用已有的串口、vIOAPIC 和 PIT，仍直通宿主 IOAPIC/HPET；
- 没有 vPIC/i8259、IMCR、RTC/CMOS、ACPI PM/reset 寄存器；
- 没有 VM 自有 PCI host bridge、ECAM/PIO 配置空间、BAR 分配和 PCI capability；
- `EmulatedDeviceType` 虽预留 `VirtioBlk` / `VirtioNet` / `VirtioConsole`，但 `axdevice`
  没有对应 factory 或设备实现；
- `VMKernelConfig::disk_path` 只存在于 schema/template，没有消费者；
- x86 尚未给 `InterruptFabric` 安装 vIOAPIC `IrqSink`；PIT、COM1 和现有 vIOAPIC 注入 helper
  都受 `VMInterruptMode::Passthrough` 门控，且宿主 IRQ 转发状态仍是全局单 VM 模型；
- 没有把 VM 设备的 IRQ capability 接到 x86 vIOAPIC INTx，更没有 MSI/MSI-X 路由。

当前 `linux-smp1.toml` 通过 identity RAM、外层 QEMU PCI BAR MMIO 窗口、`0x6000` I/O
端口和宿主 INTx 转发复用外层 virtio-blk。这是一个有意保留的 direct-boot 回归基线，但
UEFI 标准路径不得继续扩大这套硬编码透传范围。

### 2.4 LoongArch guest UEFI

LoongArch 已具备以下实现链：

```text
host firmware/ACPI discovery
  -> GuestPlatform
  -> guest firmware FDT
  -> fw_cfg MMIO (kernel/initrd/cmdline + ACPI/table-loader)
  -> guest OVMF
  -> passthrough PCI/virtio block + PCH-PIC IRQ route
  -> Linux guest
```

这条链路的不足主要是交付和验证，而不是从零实现：

- `linux-rootfs-smp1.toml` 已存在，但 `test-suit/axvisor/normal/qemu` 的 LoongArch build
  config 仍是 `vm_configs = []`；
- quick-start 明确拒绝 `qemu-loongarch64 --linux`；
- 当前 smoke 只以 `Welcome to AxVisor Shell!` 为成功条件；
- QEMU-LVZ、guest OVMF、kernel 和 rootfs 的来源尚未整理成可重复的测试资产契约。

LoongArch 后续应优先建立端到端门禁，并把经过验证的通用 fw_cfg 部分抽出；不应为 x86
改造而破坏其现有 MMIO、FDT、ACPI 和 IRQ 路由行为。

### 2.5 自动化测试的真实覆盖

| 用例 | 实际验证内容 | 是否证明 guest UEFI |
| --- | --- | --- |
| x86 VMX/SVM normal smoke | AxVisor 宿主通过动态 UEFI 启动；guest Linux 走 direct boot 和设备透传 | 否 |
| x86 `uefi/qemu-nimbos` | 配置发现、宿主 UEFI 参数；运行成功条件仅为 AxVM vCPU 任务启动 | 否，且 CI job 当前注释禁用 |
| LoongArch normal smoke | AxVisor 宿主通过 LoongArch OVMF 启动并进入 AxVisor shell | 否 |
| `axloader` UEFI smoke | AxLoader 自身的 UEFI handoff | 与 AxVisor guest UEFI 无关 |

后续任何“guest UEFI 已支持”的结论都必须由 guest 串口输出或 guest 内部行为证明，不能再用
宿主启动标记或 `VM boot success` 代替。

## 3. 目标边界

### 3.1 最小可用目标（MVP）

x86_64 单 vCPU guest 在不依赖宿主设备透传的前提下完成：

1. 从只读 OVMF CODE 和独立可写 VARS flash 进入固件；
2. 通过 x86 fw_cfg 获得内存图、ACPI 和启动策略；
3. 枚举虚拟 PCI 根总线和一个 virtio-blk PCI function；
4. 通过 INTx 完成块设备中断；
5. 从带 ESP 的磁盘启动 EFI 应用或 Linux EFI bootloader/kernel；
6. 通过虚拟串口输出可判定的 guest 成功标记。

### 3.2 “稳定可复用”的完成条件

- x86_64 VMX 和 SVM 都通过同一份 backend-neutral guest 配置；
- LoongArch guest UEFI Linux 启动纳入自动化测试；
- Linux、NimbOS、ArceOS 的 UEFI 可执行物采用明确的磁盘 EFI 文件或 fw_cfg payload
  契约，不再依赖“把裸镜像预装到某个 GPA 后期待固件发现”；
- firmware、vars、disk、kernel/initrd 的来源和生命周期清晰，配置不再复用 `bios_*` 表达
  UEFI flash；
- UEFI、multiboot 和 direct boot 共享 VM/设备基础设施，但启动参数互不混用；
- reset 后 VARS 行为确定，持久化模式有原子写回和损坏恢复策略；
- 不依赖外层 QEMU PCI BAR 地址、宿主 PCI 配置空间或宿主 IOAPIC 直通。

### 3.3 非首阶段目标

以下能力不阻塞第一条 Linux UEFI 磁盘启动链，可在 MVP 后实现：

- Secure Boot、TPM、S3/S4、热插拔；
- 图形输出、USB 输入和完整 q35 芯片组仿真；
- virtio-net、virtio-console；
- MSI/MSI-X、多队列、IOMMU；
- 多 guest 共享同一物理设备或设备热迁移。

### 3.4 设计约束

- **先闭合 x86 纵向链路，再抽象跨架构公共部分**：LoongArch 是已运行的参考路径，不应为了
  x86 的结构整洁而先重写其工作实现。
- **Boot 优先于 Runtime**：先达到可诊断的 Shell、PCI 枚举和单盘启动；复杂电源管理、
  Secure Boot、热插拔和完整芯片组兼容后置。
- **固件表只描述真实设备**：某阶段未实现 PCI、RTC、PM、reset 或 SCI，就不得在 MCFG、
  DSDT、FADT、MADT 中提前宣告相应资源。
- **平台身份必须一致**：固定的 OVMF profile、PCI host bridge、southbridge、PIRQ/PM 模型和
  ACPI 内容必须属于同一 machine contract；不得混用 q35 host bridge 与 PIIX4 寄存器语义。
- **VM 自有资源不走宿主透传捷径**：UEFI 路径中的 fw_cfg、PCI config、BAR、block queue、
  flash 和 IRQ line 都由 VM 实例拥有。
- **每阶段有独立观测点**：测试从固件阶段标记逐步升级到 Shell 命令、EFI 应用 marker 和
  guest 用户态 marker，不能把后续未实现能力当作前一阶段的隐含前提。

## 4. 总体设计

### 4.1 分层

```text
axvmconfig
  FirmwareConfig + BootSource + GuestPlatformProfile
          |
          v
axvm boot/guest_platform
  校验并生成 FirmwarePlan / MemoryLayout / PlatformDescription
          |
          +--------------------+---------------------+
          v                    v                     v
axdevice firmware         axdevice pci          axdevice platform
  pflash core               root bus/config       vIOAPIC/PIT/RTC
  fw_cfg item store         virtio-pci transport  serial/reset
  x86 PIO / LA MMIO         virtio-blk backend
          |                    |                     |
          +--------------------+---------------------+
                               v
                    AxVM guest address space
                    + InterruptFabric / vLAPIC
```

边界要求：

- `axvmconfig` 只表达意图，不包含 OVMF 或 QEMU 路径探测逻辑；
- `axdevice` 拥有可移植设备核心，不依赖 AxVisor OS 运行时；
- AxVM 架构层负责端口/MMIO VM exit、guest memory DMA 和 vCPU IRQ 注入适配；
- `os/axvisor` 负责文件系统后端、镜像生命周期和宿主 IRQ/块设备能力绑定；
- ACPI 必须由同一个 `PlatformDescription` 生成，不能与实际设备配置分别硬编码。

### 4.2 配置模型

目标配置应把启动协议、固件、启动源和设备分开。示意：

```toml
[boot]
protocol = "uefi"
source = "disk"

[boot.firmware]
code_path = "/guest/firmware/OVMF_CODE.fd"
vars_template_path = "/guest/firmware/OVMF_VARS.fd"
vars_path = "/var/lib/axvisor/vm-1-vars.fd"
code_load_addr = 0xffc0_0000
vars_load_addr = 0xff80_0000

[[devices.block]]
id = "root"
transport = "virtio-pci"
path = "/guest/linux/rootfs-esp.img"
read_only = false
boot_index = 0
```

最终字段名可在实现阶段调整，但语义必须满足：

- `protocol = "direct"`：`kernel_path`/`ramdisk_path`/`cmdline` 由 AxVM 直接消费；
- `protocol = "multiboot"`：只保留 x86 兼容路径，消费 axvm-bios 和 multiboot info；
- `protocol = "uefi", source = "disk"`：固件从 PCI block 设备发现 ESP，不预装裸 kernel；
- `protocol = "uefi", source = "fw_cfg"`：可选兼容模式，显式把 kernel/initrd/cmdline
  放入 fw_cfg；
- CODE 必须只读，VARS 必须独立可写；二者大小、对齐、地址范围和 reset vector 覆盖关系
  在 VM 创建前一次性校验；
- `enable_bios`、`uefi_firmware_path`、`bios_load_addr` 只作为旧配置兼容输入，解析后转换成
  新类型，并给出弃用诊断，不再向运行时传播布尔组合。

配置迁移分成两层，避免 schema 大改阻塞第一条启动链，又不让历史布尔组合继续污染运行时：

1. P1 先在解析边界把现有扁平字段和最少新增字段转换为内部 typed `FirmwarePlan`、
   `BootSource`、`FirmwareProfile`；地址和组合校验只依赖这些内部类型。
2. x86 磁盘启动稳定后再收敛公开 TOML schema 为上述目标形态；旧字段只留在兼容 parser，
   并有确定的优先级、弃用诊断和删除条件。

### 4.3 fw_cfg 与平台表

将当前 `FwCfg` 拆成数据核心和 bus 前端，但接入现有统一 `Device` 资源模型：

1. `FwCfgStore`：每 VM 一份 selector、offset、item 目录、DMA 请求和读写权限状态；
2. `FwCfgDevice`：持有同一 store，并根据显式 `FwCfgTransportProfile` 声明资源和解释寄存器；
3. LoongArch profile 声明一个 MMIO range，保留 `base + 0/8/0x10` 和现有 endian 契约；
4. x86 profile 声明 `0x510..0x511` 与 `0x514..0x51b` 两段 PIO resource：selector 只接受
   16-bit little-endian write，data 使用 8-bit string-preserving access，DMA address 为
   64-bit big-endian，并支持规范允许的分段写触发。

`Device` 已支持单实例声明多个不重叠 bus resource，因此无需继续受旧
`BaseDeviceOps<GuestPhysAddrRange>` 限制，也无需在一个 guest profile 中同时暴露 MMIO 和
PIO。store/transport 分层是状态所有权边界，`Device` 是注册和分发边界，两者并不冲突。

平台数据改为 provider/builder：

- 通用：RAM map、CPU 数、kernel/initrd/cmdline、boot order、UUID；
- LoongArch：现有 RSDT/FADT/MADT/SRAT/SPCR/MCFG/DSDT 布局；
- x86：RSDP + XSDT（必要时同时给 RSDT）、FADT、MADT、MCFG、DSDT；
- SMBIOS：MVP 可保留最小合法集合，但必须明确版本、入口和 checksum；后续补 Type 0/1/4/16/17/32。

不要用 `#[cfg(target_arch)]` 或大量 `Option` 字段继续扩张当前 LoongArch 形态的
`FwCfgPlatformConfig`。应由显式的 `X86GuestPlatform` / `LoongArchGuestPlatform` 实现公共
platform description/provider 契约，再复用纯参数化的 table-loader、MCFG、SRAT、SPCR 等
builder。这样单元测试可以在同一 host 构建中检查不同平台输入，也不会把 host 编译架构误当成
guest 表类型。

x86 MADT 至少描述：

- 每个可用 vCPU 的 Local APIC/x2APIC ID；
- 一个 vIOAPIC 及其 GSI base；
- 只为 ISA source 与 GSI 不同或 polarity/trigger 非默认的线路生成 interrupt source override，
  例如兼容 PC 的 PIT IRQ0 -> GSI2；COM1 identity route 不需要 ISO，PCI INTx 由 DSDT `_PRT`
  描述而不是塞入 MADT ISO。

x86 DSDT 至少描述：

- P2 Shell 基线只描述已经存在的 CPU/APIC、COM1 和 fw_cfg inventory；
- P3 注册 PCI root 后才增加 PCI0 的 `_HID`、`_SEG`、`_BBN`、`_CRS`、`_PRT` 和 MCFG；
- PCI INTx direct route 或 link device 与实际 vIOAPIC GSI 一致；
- RTC/CMOS、ACPI PM/SCI 和 reset register 只在相应设备实现后加入；
- 所有 MMIO/PIO window 与 PCI allocator 使用同一平台常量或生成结果。

FADT 同样遵守逐阶段发布：早期未实现的 PM1 event/control、PM timer、SMI command、SCI 和
reset GAS 保持未声明/零值，不能用“占位地址”欺骗 OVMF 或 guest OS。P2 使用 ACPI 2.0
RSDP + XSDT；是否同时生成 RSDT 由固定 OVMF profile 和兼容测试决定。

### 4.4 PCI 与 virtio

MVP 使用单 segment、单 bus、静态 function 集合：

- P0 必须先确定 machine profile：优先使用能够接受 AxVisor 最小平台的固定 OVMF 构建；若选
  stock q35 OVMF，则 host bridge、LPC/PM/PIRQ 和 ACPI 都按同一 q35 子集实现，不能只伪造
  `0x8086:0x29c0` 身份；
- root complex 提供 ECAM；如固定 OVMF profile 需要，再提供同一 config store 的
  `0xcf8/0xcfc` PIO 前端；
- config space 支持 vendor/device/class/header、BAR probe/assign、command/status、capability list；
- BAR 由 guest platform allocator 在低 MMIO window 内确定，ACPI `_CRS` 和 MCFG 同源；
- MVP 的 `_PRT` 可直接路由到 GSI；只有 machine profile 确实包含 southbridge/link device 时，
  才实现 PIRQ route register。PIIX3/PIIX4 的 `0x60..0x63` 是 PCI config offset，不是 I/O port；
- 首个设备为 non-transitional modern virtio-blk PCI function（vendor `0x1af4`、device
  `0x1042`、revision >= 1）；INTx pin 固定但路由由 PCI swizzle/route
  模型计算，不在设备内硬编码 GSI；
- virtio PCI capability 至少包含 common、notify、ISR、device-specific 和 PCI config access；
  BAR offset/length 来自一个布局对象，PCI config capability 与实际 BAR dispatch 同源；
- virtio queue DMA 只能通过 AxVM guest-memory capability 访问，禁止直接假设 GPA=HPA；
- queue size 是设备上限与 driver 协商结果，不能把 256 当成固定 guest 值；descriptor walker
  必须拒绝环、越界、错误方向、长度溢出和跨越未授权 guest region 的链；
- 块后端通过明确 trait 提供容量、读写、flush 和只读属性；文件路径只在 `os/axvisor`
  转换为后端，不能泄漏到设备核心；
- 无 MSI-X 时，queue completion 设置 virtio ISR queue bit 和 PCI Interrupt Status，并 assert
  INTx；guest 读取 ISR 后清零并 deassert。不能把“固件可能轮询 used index”当成正确性前提。

MSI/MSI-X 在 INTx 稳定后实现。届时 MSI address/data 要由 vLAPIC/MSI router 解析，不能把
guest vector 或 GSI 与宿主 `IrqId` 混成同一个裸整数。

### 4.5 VM-local 中断闭环

目标路径是：

```text
virtio-blk completion
  -> device-owned IrqLine.raise()/lower()
  -> per-VM x86 IrqSink
  -> vIOAPIC GSI state / remote-IRR
  -> per-VM pending delivery + vCPU wakeup
  -> before-vCPU-run injection
  -> guest LAPIC EOI / virtio ISR read
  -> re-inject or deassert
```

- x86 VM 初始化时用 VM 自有 vIOAPIC 和 pending-delivery capability 构造 `InterruptFabric`，
  不能继续使用当前无 backend 的 `InterruptFabric::new()`；
- virtio factory 通过 `DeviceBuildContext::resolve_irq()` 获得 level-triggered `IrqLine`，设备
  不引用 `AxVM`、vCPU 或 x86 注入函数；
- `IrqSink` 负责改变 vIOAPIC line state，并把可注入的 vector 排入该 VM 的 pending queue、
  唤醒目标 vCPU；运行循环已有 before-run pending drain，应扩展这条通用边界，而不是新增
  virtio 专用 `finish()` 轮询；
- EOI 只更新 vIOAPIC remote-IRR；只要设备 line 仍 asserted 就允许重新投递，读取 virtio ISR
  后 lower line 才结束 level interrupt；
- 当前 `irq.rs` 的静态 passthrough bitmap、PIT/COM1 polling helper 保留作 direct-boot 回归，
  不作为 VM 自有设备的最终路径，也不能扩展到多 guest。

### 4.6 UEFI flash、变量和 reset

- CODE bank 映射为只读、可执行 firmware region；
- VARS bank 映射为不可执行、可写 flash region；
- pflash/CFI 状态机处理 OVMF 实际使用的读、查询、块擦除和编程命令；
- 默认从只读 VARS template 创建 VM 私有 working copy；
- `ephemeral` 模式只在测试中使用，VM 重建后丢弃；`persistent` 模式在显式 flush/stop 时
  原子写回临时文件后 rename，不能原地部分覆盖模板；
- warm reset 保留 VARS，cold recreate 按所选策略恢复；
- 实现 ACPI reset register 或 x86 reset port 后，将 guest reset 转换为 AxVM 生命周期 reset，
  不能让 guest 写操作触达宿主复位设备。

## 5. 分阶段实现路径

### P0：建立可诊断基线

目标：先区分“进入固件”和“guest 启动成功”。

- 保留现有 x86 UEFI 配置解析测试和 reset-vector 单元测试；
- 增加 OVMF DEBUG/串口阶段标记的测试说明：reset vector、SEC、PEI、DXE、BDS；
- 将 UEFI smoke 的成功条件改为 guest 可观察标记；在实现尚未达到前保持 job 非门禁，不能用
  `VM[1] boot success` 伪装通过；
- 固定一组经过验证的 OVMF CODE/VARS 版本、构建选项和校验值，并记录其对应的 machine
  contract、内置 virtio driver 与 SMM 要求；测试不得依赖发行版“当前最新”文件布局；
- 用 trace 验证 OVMF 实际访问过的 fw_cfg/flash/PCI/PM 资源，以此决定最小平台，而不是根据
  QEMU 某一 machine 的设备名猜测；
- 为 direct/multiboot 现有配置保留回归用例。

退出条件：能稳定判断故障发生在固件装载、平台发现、设备枚举还是 OS 启动。

### P1：配置与 flash 生命周期

目标：把单一 BIOS 镜像模型升级为有角色的 UEFI firmware plan。

- 在 `axvmconfig` 解析边界生成内部 typed firmware/boot-source/profile，并增加最少的
  CODE/VARS/disk 输入字段和交叉字段校验；公开 TOML schema 的最终重组后置到 P5；
- 在 `axvm` 生成不重叠的 CODE/VARS/RAM/MMIO 布局；
- 在 `axdevice` 实现 pflash core，并接入 x86 MMIO VM exit；
- 保留旧 `uefi_firmware_path + bios_load_addr` 的只读单镜像兼容路径；
- 增加 CODE 写拒绝、VARS erase/program、越界/重叠和 reset 保留测试。

退出条件：OVMF 能从 CODE bank 启动并识别独立 VARS bank，重启行为可重复。

### P2：通用 fw_cfg 核心与 x86 平台表

目标：让 OVMF 获得与 VM 实际拓扑一致的固件数据。

- 拆分 fw_cfg store 和 transport profile，将 `FwCfg` 迁移到统一 `Device` trait；x86 profile
  声明两段 PIO resource，LoongArch profile 保留 MMIO resource；
- 保持 LoongArch 行为和 DMA endian 契约不变；
- 新建 x86 `GuestPlatform`，此阶段只集中描述已实现的 RAM、CPU、APIC、串口和 fw_cfg；
- 实现 x86 基础 ACPI/SMBIOS builder 和 table-loader 文件；此阶段不发布尚未实现的
  MCFG/PCI0、RTC、PM、SCI 或 reset GAS；
- 对所有表做长度、checksum、交叉指针和地址范围单元测试；
- 使用 ACPICA/iasl（若测试环境提供）反编译 DSDT，并校验 RSDP/XSDT/MADT；MCFG 在 P3
  随 PCI root 一起加入验证。

退出条件：OVMF 进入 Shell，能看到正确 CPU/内存/ACPI，不再因缺少 fw_cfg 或错误表卡在 DXE。

### P3：虚拟 PCI 根总线

目标：固件能枚举由 AxVisor 拥有的 PCI function。

- 实现 ECAM 和必要的 PIO config 前端；
- 实现静态 BDF 分配、BAR probe/assign、command register 和 capability list；
- 从同一 `GuestPlatform` 增加 MCFG、PCI0 `_CRS`/`_PRT`；MVP 优先 direct-GSI `_PRT`，
  只有固定 machine profile 要求时才增加匹配的 southbridge/PIRQ link；
- 用一个无 DMA 的测试 function 验证 OVMF `pci` 枚举、BAR 和 ACPI 路由；
- 禁止 UEFI 配置使用外层 QEMU PCI BAR/PIO 透传作为替代。

退出条件：UEFI Shell 可稳定枚举 root bus 和测试 PCI function，地址与 MCFG/DSDT 一致。

### P4：virtio-blk + INTx + 磁盘启动

目标：完成第一条真正可用的 UEFI OS 启动链。

- 实现 non-transitional modern virtio-pci transport、完整 capability chain、virtio-blk queue
  和 block backend；
- 把 `disk_path` 迁移为设备配置并实现镜像打开、容量、flush、只读策略；
- 补齐最小 vPIC/i8259 与 IMCR，明确固件 legacy PIC 阶段、APIC 切换和 vIOAPIC 路由关系；
- 为 x86 `InterruptFabric` 增加 per-VM vIOAPIC sink；virtio 只持有 resolver 返回的 `IrqLine`；
- 按 ISR read-to-clear、PCI Interrupt Status、INTx assert/deassert、vIOAPIC remote-IRR/EOI
  验证 mask、重复完成和并发完成；
- 准备包含 ESP、EFI 应用/bootloader 和 rootfs 的确定性磁盘镜像；
- 依次验证 `map`/`pci`、读文件、EFI 应用、Linux EFI 启动和用户态成功标记。

退出条件：x86_64 单 vCPU VM 在无设备透传条件下从 virtio-blk 磁盘进入 Linux 用户态。

### P5：PC 兼容性与多核

目标：从 MVP 提升到可复用平台。

- 补 RTC/CMOS、ACPI PM/SCI/reset，并只在设备接入后更新 FADT/DSDT；
- 验证 vLAPIC timer、IPI、SIPI 和多 vCPU MADT；
- 实现 MSI/MSI-X router，再让 virtio-blk 切换到 MSI-X；
- 加入 virtio-net 和 virtio-console；
- 将公开 TOML schema 收敛为 firmware/boot source/device 分层结构，旧 `enable_bios`、
  `bios_*` 和 kernel-level `disk_path` 只保留兼容解析与弃用诊断；
- 将 NimbOS 和 ArceOS 产物打包为 OVMF 可发现的 PE/COFF EFI 应用、ESP 文件或经明确
  约定的 fw_cfg payload，并为二者分别增加至少一个启动用例；
- 清理 UEFI guest 对宿主 IOAPIC、HPET 和 PCI 地址透传的依赖。

退出条件：SMP Linux 稳定启动，时间、reset、磁盘 IRQ 和 MSI-X 有独立回归覆盖。

### P6：LoongArch 验证与配置收敛

目标：把已有 LoongArch 实现变成受支持路径。

- 为 QEMU-LVZ 建立带 `linux-rootfs-smp1.toml` 的 guest test group；
- 固定 guest OVMF、kernel、rootfs 和 QEMU-LVZ 版本/获取方式；
- 成功条件改为 Linux guest 用户态标记，并覆盖 virtio-blk 读写；
- 为 quick-start 增加受支持的 `qemu-loongarch64 --linux` 路径；
- 复用新的 boot-source/firmware 配置，同时保留 LoongArch MMIO fw_cfg 和平台发现；
- 对比重构前后的 FDT、ACPI、IRQ route 和 rootfs 行为，防止 x86 抽象反向破坏 LoongArch。

退出条件：x86_64 和 LoongArch 各至少有一条自动化 guest UEFI Linux 端到端用例。

## 6. 验证矩阵

| 层级 | 必测项 | 通过标准 |
| --- | --- | --- |
| 配置 | 协议/固件/启动源组合、旧字段兼容、地址重叠 | 非法组合在 VM 创建前返回 typed error |
| pflash | CODE 只读、VARS program/erase、reset、持久化失败 | 字节级确定性测试，无静默截断或回退 |
| fw_cfg | PIO selector LE、MMIO selector BE、data 字节流、DMA BE、目录边界 | 与 QEMU fw_cfg 契约一致，LoongArch 回归不变 |
| ACPI/SMBIOS | checksum、长度、指针、CPU/APIC/已注册设备资源 | 单元测试 + iasl/解析器通过；不含虚构设备 |
| PCI | config width、BAR probe、越界 BDF、command/capability、ECAM/PIO 一致性 | OVMF Shell 枚举结果稳定，machine identity 自洽 |
| virtio-blk | modern ID/capability、feature/queue negotiation、descriptor/DMA 越界、flush、ISR | 单元测试 + guest 读写校验 |
| 中断 | `IrqLine`、INTx mask/assert/deassert、remote-IRR、EOI、重注入 | 不丢/重复完成，不走全局 passthrough 状态 |
| x86 VMX | OVMF -> 磁盘 -> Linux 用户态 | guest marker；不接受宿主 marker |
| x86 SVM | 与 VMX 相同 guest 配置 | 同一 marker 和磁盘校验 |
| x86 ArceOS/NimbOS | OVMF -> EFI 文件或显式 fw_cfg source | 各自的 guest marker；不接受裸 GPA 预装 |
| LoongArch LVZ | OVMF/fw_cfg -> Linux/rootfs | guest marker + virtio-blk 读写 |
| 回归 | x86 direct/multiboot、其他架构 direct boot | 现有 normal smoke 继续通过 |

推荐的逐级运行命令：

```bash
cargo test -p axvmconfig
cargo test -p axdevice
cargo test -p axvm
cargo xtask axvisor test qemu --list --arch x86_64
cargo xtask axvisor test qemu --arch x86_64 --test-group uefi --test-case <case>
```

Rust 逻辑变更后按涉及 crate 运行 `cargo fmt` 和针对性的
`cargo xtask clippy --package <crate>`。端到端 x86 需要对应 Intel/AMD KVM runner；LoongArch
需要仓库约定的 QEMU-LVZ 环境。

## 7. 关键风险与决策点

1. **OVMF 发行物差异**：2M/4M、combined/split、内置 virtio driver 和 Secure Boot 配置不同。
   测试必须固定 firmware profile，加载地址由文件大小和 profile 校验，不硬猜。
2. **先 ACPI 还是先 PCI**：两者必须共享 `GuestPlatform`，但可以先用 test function 验证表和
   枚举，再引入 virtio DMA；P2 的基础表不包含 PCI，P3 注册 root bus 后才同步加入
   MCFG/PCI0，保证每个阶段都不描述虚构设备。
3. **fw_cfg 注入与直接 ACPI 放置**：OVMF 主路径使用 fw_cfg table-loader；
   `GuestAcpiTables` 可服务非固件或未来静态表路径，但不能绕过 OVMF 的安装流程造成两套真相。
4. **fw_cfg 的代码形态**：使用 store + transport profile 保持状态/协议分层，同时以统一
   `Device` trait 注册多段资源；不为规避旧泛型 trait 而复制两份 selector/offset 状态。
5. **ACPI 架构分发**：使用显式 guest platform provider，而不是 `#[cfg(target_arch)]` 选择
   table builder。编译 target 是 hypervisor host 架构，不应成为平台描述数据模型的一部分。
6. **machine identity**：先固定 OVMF profile，再选择最小自有平台或一致的 q35 子集。禁止
   q35 host bridge ID、PIIX4 PIRQ、任意 PM port 三者拼装成一个不存在的机器。
7. **INTx 与 MSI-X**：Virtio 规范允许 MSI-X disabled 时通过 ISR + INTx 工作。MVP 固定 INTx，
   先验证 `IrqLine`、vIOAPIC level/EOI 和 read-to-clear；MSI-X 后置，避免同时引入 MSI router。
8. **中断注入位置**：扩展 per-VM `InterruptFabric` 和 before-run pending drain，不增加
   virtio 专用 `finish()` pending flag；后者会绕过设备 capability 边界并延续单 VM 假设。
9. **透传基线的定位**：保留现有 x86 direct Linux 透传测试用于嵌套虚拟化回归，但不继续向
   UEFI 平台复制宿主 BAR 地址和 IRQ 假设。
10. **持久化安全**：VARS 和可写 guest disk 都是外部状态。默认使用 VM 私有副本，测试使用
   临时文件，异常退出不能覆盖模板或留下半写镜像。
11. **多 guest 隔离**：fw_cfg selector、PCI config、flash 状态、block queue 和 IRQ line 必须是
   VM 实例所有，不能继续使用只适合单 VM 的全局 x86 IRQ 转发状态作为最终模型。

## 8. 架构复用边界

| 维度 | LoongArch（保留现有行为） | x86_64（目标行为） | 可复用部分 |
| --- | --- | --- | --- |
| 固件入口 | EDK2 固件 GPA 入口 | OVMF reset vector `0xfffffff0` | firmware plan、镜像校验 |
| 平台发现 | FDT + fw_cfg MMIO | fw_cfg PIO + ACPI/SMBIOS，无 x86 FDT | fw_cfg store、file directory、table-loader |
| ACPI 中断 | EIOINTC/PCH-MSI/PCH-PIC | Local APIC/IOAPIC/ISO | table header/checksum 与 provider 接口 |
| PCI | 当前宿主资源发现/透传 | VM 自有 root/config/BAR | MCFG/PCI `_CRS` builder 的参数化部分 |
| 块设备 | 当前透传链路 | modern virtio-pci 软件设备 | block backend trait；不复用宿主 IRQ |
| 中断 | 现有 PCH route | VM-local `IrqLine` -> vIOAPIC -> vLAPIC | `InterruptFabric`/`IrqResolver` capability |
| flash | 现有固件映射 | OVMF CODE/VARS pflash | 生命周期和持久化策略 |

复用以协议核心和 capability 边界为单位，不以“让两种架构共享同一个含大量 cfg/Option 字段的
platform struct”为目标。

## 9. 预计改动位置

| 模块 | 阶段 | 主要职责 |
| --- | --- | --- |
| `virtualization/axvm-types` | P1/P5 | 内部 typed firmware/boot source、PCI/IRQ 边界值；后续公开配置类型 |
| `virtualization/axvmconfig` | P1/P5 | 兼容转换、交叉字段校验；磁盘启动稳定后收敛公开 schema |
| `virtualization/axdevice/src/fw_cfg.rs`（重构时可拆模块） | P2 | store、transport profile、x86 PIO、LA MMIO、table-loader |
| `virtualization/axdevice` 的 PCI/virtio 模块 | P3/P4 | PCI root/config/BAR、modern virtio-pci/block、factory 与 IRQ capability |
| `virtualization/axvm/src/boot` | P1-P3 | firmware plan、guest platform、表/镜像/磁盘启动策略 |
| `virtualization/axvm/src/arch/x86_64` | P2-P5 | PIO/MMIO exit 适配、x86 platform、vIOAPIC sink、vCPU pending delivery |
| `virtualization/x86_vlapic` | P4/P5 | vIOAPIC line/EOI，后续 vPIC/MSI 等可移植 x86 中断核心 |
| `os/axvisor` | P1/P4 | 文件 block backend、VARS 持久化、镜像资产和 VM 生命周期集成 |
| `scripts/axbuild` | P0-P6 | 固件/磁盘资产准备和 QEMU 测试编排，不推断 guest boot 协议 |
| `test-suit/axvisor` | P0-P6 | x86 VMX/SVM guest UEFI 和 LoongArch LVZ 端到端用例 |

具体文件拆分在实现阶段遵守现有 crate 边界，不把设计中的建议文件名视为预先确定的 API。

## 10. 外部兼容契约

- [QEMU fw_cfg specification](https://qemu.readthedocs.io/en/v9.1.3/specs/fw_cfg.html)：
  selector/data/DMA 寄存器、x86 I/O 端口、MMIO endian、file directory 和 DMA 契约。
- [TianoCore: How to run OVMF](https://github.com/tianocore/tianocore.github.io/wiki/How-to-run-OVMF)：
  OVMF 作为虚拟机 UEFI 固件和 pflash 的基本运行方式。
- [TianoCore OVMF FAQ](https://github.com/tianocore/tianocore.github.io/wiki/OVMF-FAQ)：
  非易失变量依赖 flash 仿真的兼容性约束。
- [OASIS Virtio 1.2](https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html)：
  modern PCI ID、capability、virtqueue、ISR/INTx 和 MSI-X 契约。

实现时还应对照当前固定 OVMF profile 对应的 EDK2 `OvmfPkg` 源码，而不是仅依据某个发行版
文件名推断固件布局。
