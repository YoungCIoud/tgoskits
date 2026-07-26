# AxVisor x86_64 Guest UEFI 支持完善 — 设计方案

> 基线：`130785326`（2026-07-21）。本文结合 `plan/uefi/design.md` 的产品层设计思路和代码实际验证结果。

## 1. 当前状态

### 1.1 已具备的能力

**启动协议与固件加载**：
- `VMBootProtocol` 枚举已定义 `Direct | Multiboot | Uefi`（`axvm-types/src/lib.rs`）
- x86_64 三个 UEFI 客户机配置（linux-uefi / arceos-uefi / nimbos-uefi）均已存在
- `load_uefi_from_configured_path()` 已实现：从文件系统加载 OVMF_CODE.fd 到 bios_load_addr，vCPU 以 `CS.base=0xFFFF0000, RIP=0xFFF0` 进入 reset vector（`axvm/src/arch/x86_64/boot/mod.rs`）
- `should_patch_multiboot_info()` 对 UEFI 协议正确返回 false
- `config.kernel.disk_path` 字段已在 schema 中预留（未使用）

**x86 虚拟设备** (`axdevice/src/x86.rs`)：
- `X86IoApicDevice`（MMIO, `0xFEC0_0000`）、`X86PitDevice`（PIO）、`X86SerialPortDevice`（PIO, `0x3F8`）
- PIO 设备框架已有两套独立端口类型层次：`Port`/`PortRange`（axdevice_base 框架层）和 `X86Port`/`X86PortRange`（x86_vlapic 设备核心层），适配器在 `handle()` 中做桥接

**中断架构**：
- `vIOAPIC`：24 RTE、level-triggered remote-IRR、EOI 去抖动（`x86_vlapic/src/vioapic.rs`）
- `vLAPIC`：virtual-APIC page、ISR/IRR/TMR 管理（`x86_vlapic/src/vlapic.rs`）
- `InterruptFabric`：统一中断线抽象，支持 `set_level`/`pulse`/`IrqResolver`（`axvm/src/irq/mod.rs`）
- PIT (GSI 0) 和 COM1 (GSI 4) 的注入链路已连通，但在 `VMInterruptMode::Passthrough` 门控下运行
- 注入触发点：`DeferredRunWork::PreemptionTimer`（轮询 PIT/串口）和 `DeferredRunWork::InterruptEnd`（EOI 后重注入）

**FwCfg 设备** (`axdevice/src/fw_cfg.rs`, ~1494 行)：
- 完整 QEMU fw_cfg 协议：selector/data/DMA
- 内置 ACPI 表生成框架 + QEMU table-loader + SMBIOS 终止结构
- `FwCfgPlatformConfig` 参数化平台配置
- **仅 DSDT/MADT/FADT builder 硬编码了 LoongArch 值**；`build_mcfg`/`build_srat`/`build_spcr` 纯参数化，架构无关
- 当前仅实现 `BaseDeviceOps<GuestPhysAddrRange>`（MMIO only）

**LoongArch UEFI 参考实现**：
- 完整链路：host ACPI 发现 → GuestPlatform → 固件 FDT → fw_cfg MMIO → guest OVMF → 直通 PCI → Linux
- `QemuVirtDefaults`（`probe.rs`）展示完整平台配置模式（flash、PCI、串口、中断拓扑、IRQ 路由）

**平台描述基础设施** (`axvm/src/boot/policy.rs`)：
- `GuestAcpiTables`：持有 ACPI 字节和 RSDP GPA，尚未接入任何架构
- `GuestBootDescription`：统一持有 DTB 或 ACPI 表（互斥）

### 1.2 缺失的部分

**平台设备骨架（PC Platform Skeleton）**：
1. x86_64 不调用 fw_cfg/ACPI 代码——`build_acpi()` 存在但 x86_64 路径不走它
2. x86 ACPI 表构建全未实现——DSDT/MADT/FADT 全 LoongArch 硬编码；`build_rsdp` 仅 ACPI 1.0
3. `FwCfgInterruptConfig` 全 LoongArch 专用（eiointc_irq、pch_msi_base 等 7 个字段）
4. FwCfg 仅 MMIO——OVMF x86 默认 PIO `0x510`/`0x511`
5. 无 PCI host bridge + ECAM 虚拟化
6. 无 virtio 设备仿真（`EmulatedDeviceType::VirtioBlk` 枚举值已预留，无实现）
7. 无 VM 自有中断路由：设备 IRQ 到 vIOAPIC INTx 的连接未建立，MSI/MSI-X 路由未实现
8. 无 OVMF_VARS 独立加载（CODE/VARS 双 flash 语义缺失）
9. 无 RTC/CMOS（PIO `0x70`/`0x71`）、无 PIIX4 PM 设备、无 PIRQ 路由寄存器（`0x60`-`0x63`）
10. 无 vPIC/i8259、IMCR 等 PC 兼容设备

**配置与测试**：
11. 配置模型复用了太多 `bios_*` 字段表达 UEFI flash 语义，`disk_path` 字段预留但无消费者
12. x86 UEFI CI 完全缺失：测试套件文件存在（`uefi/qemu-nimbos/`、`normal/qemu/qemu-x86_64-vmx.toml`）但 CI 矩阵未引用
13. LoongArch CI 也缺失：`normal/qemu/qemu-loongarch64.toml` 仅启动 AxVisor shell，`linux-rootfs-smp1.toml` 未接入测试套件

### 1.3 一句话总结

OVMF 加载到 reset vector 已打通，但 OVMF 进入后找不到任何硬件——缺少完整 PC 平台骨架（ACPI 表、fw_cfg、PCI 总线、virtio 磁盘、中断路由）。

---

## 2. 目标

### 2.1 最小可行目标（MVP）

x86_64 单 vCPU guest 在**不依赖宿主设备透传**的前提下完成：

1. 从只读 OVMF CODE 和独立可写 VARS flash 进入固件
2. 通过 x86 fw_cfg (PIO) 获得内存图、ACPI 表和启动策略
3. 枚举虚拟 PCI 根总线和一个 virtio-blk PCI function
4. 通过 INTx + vIOAPIC 完成块设备中断
5. 从带 ESP 的磁盘启动 Linux EFI stub 或 EFI 应用
6. 通过虚拟串口输出可判定的 guest 成功标记

### 2.2 "稳定可复用"的完成条件

- x86_64 VMX 和 SVM 都通过同一份 backend-neutral guest 配置
- LoongArch guest UEFI Linux 启动纳入自动化测试
- firmware、vars、disk、kernel/initrd 的来源和生命周期清晰，配置不再复用 `bios_*` 表达 UEFI flash
- UEFI、multiboot 和 direct boot 共享 VM/设备基础设施，但启动参数互不混用
- 不依赖外层 QEMU PCI BAR 地址、宿主 PCI 配置空间或宿主 IOAPIC 直通

### 2.3 非首阶段目标

- Secure Boot、TPM、S3/S4、热插拔
- 图形输出、USB、完整 q35 芯片组仿真
- virtio-net、virtio-console
- MSI/MSI-X、多队列、IOMMU
- 多 guest 共享物理设备或设备热迁移

---

## 3. 设计原则

1. **x86_64 优先攻坚，LoongArch 并行完善**。x86_64 从骨架补到完整 PC 平台；LoongArch 建立端到端门禁并抽出通用 fw_cfg 部分
2. **先 Boot 阶段，后 Runtime 阶段**。ExitBootServices 之前（ACPI、PCI、virtio-block、中断链路）是首要目标；Runtime Services 在 Boot 稳定后补齐
3. **复用 LoongArch 已验证的通用组件**。fw_cfg item 目录/DMA/table-loader、ACPI loader 框架、SMBIOS builder 直接复用；仅替换架构特定的 DSDT/MADT/FADT
4. **x86_64 不引入 FDT**。x86 UEFI 通过 ACPI (via fw_cfg) + SMBIOS (via fw_cfg) + UEFI Protocols 传递平台信息
5. **ACPI 必须由同一 PlatformDescription 生成**，不能与实际设备配置分别硬编码
6. **最小侵入 LoongArch**。用 `#[cfg(target_arch)]` 门控 DSDT/MADT/FADT，不改动 LoongArch 逻辑
7. **每阶段都有验证闭环**。从 OVMF 串口阶段标记开始，逐步到 EFI 应用和完整内核启动

---

## 4. 架构分层

```
axvmconfig
  启动协议、固件路径、启动源、磁盘路径、跨字段校验
          |
          v
axvm boot/policy
  FirmwarePlan（内存布局）+ PlatformDescription（ACPI/SMBIOS）
          |
          +-------------------+---------------------+
          v                   v                     v
axdevice firmware        axdevice pci         axdevice platform
  FwCfg (PIO+MMIO)        root bus/ECAM         vIOAPIC/PIT/RTC/COM1
  pflash core             virtio-pci transport  vPIC/PIRQ/PM reset
  table-loader + ACPI     virtio-blk backend
          |                   |                     |
          +-------------------+---------------------+
                              v
                   AxVM guest address space
                   + InterruptFabric + vLAPIC + DeferredRunWork
```

边界要求：
- `axvmconfig` 只表达意图，不包含固件路径探测逻辑
- `axdevice` 拥有可移植设备核心，不依赖 AxVisor OS 运行时
- AxVM 架构层负责 PIO/MMIO VM-exit、guest memory DMA 和 vCPU IRQ 注入适配
- `os/axvisor` 负责文件系统后端、镜像生命周期和宿主 IRQ/块设备能力绑定

---

## 5. 配置模型演进

### 5.1 当前问题

现有配置将 UEFI flash 语义塞进 `bios_*` 字段：`uefi_firmware_path` 实际指向 OVMF CODE，`bios_load_addr` 被复用为固件加载地址。CODE/VARS 只有一条路径和一个地址，没有角色区分、读写属性和独立地址范围。

### 5.2 目标配置（语义层面）

```toml
[boot]
protocol = "uefi"
source = "disk"          # "disk" | "fw_cfg"（兼容模式）

[boot.firmware]
code_path = "/guest/firmware/OVMF_CODE.fd"
vars_template_path = "/guest/firmware/OVMF_VARS.fd"
code_load_addr = 0xffc0_0000
vars_load_addr = 0xffe0_0000

[[devices.block]]
id = "root"
transport = "virtio-pci"
path = "/guest/linux/rootfs-esp.img"
boot_index = 0
```

### 5.3 过渡策略

MVP 阶段保持现有 `VMKernelConfig` 扁平结构，增加必要字段（`uefi_vars_path`、`disk_path` 消费者），避免大规模配置迁移阻塞功能实现。`enable_bios`、`bios_load_addr` 等旧字段在解析后转换成内部类型，给出弃用诊断。**阶段五（配置收敛）再引入新的 typed firmware/boot-source 配置**，同时保留旧字段兼容。

---

## 6. 分阶段实现路径

### P0：建立可诊断基线

**目标**：区分"进入固件"和"guest 启动成功"，停止用 `VM[1] boot success` 伪装通过。

- 保留现有 x86 UEFI 配置解析测试和 reset-vector 单元测试
- 增加 OVMF 串口阶段标记的测试说明：reset vector → SEC → PEI → DXE → BDS
- 将 UEFI smoke 的成功条件改为 guest 可观察标记（如 "UEFI Interactive Shell"）
- **在实现尚未达到前保持 job 非门禁**，不伪装通过
- 固定一组经过验证的 OVMF CODE/VARS 版本和校验值，测试不依赖发行版"当前最新"文件
- 为 direct/multiboot 现有配置保留回归用例

**退出条件**：能稳定判断故障发生在固件装载、平台发现、设备枚举还是 OS 启动。

---

### P1：fw_cfg(PIO) + x86 ACPI 表 → UEFI Shell

**目标**：OVMF 通过 fw_cfg 获取 ACPI + SMBIOS，进入 UEFI Shell。

#### 6.1.1 fw_cfg 从纯 MMIO 迁移到双总线

**现状**：`FwCfg` 实现 `BaseDeviceOps<GuestPhysAddrRange>`（旧式 MMIO trait）。OVMF x86 默认用 PIO `0x510`/`0x511`。

**方案**：将 FwCfg 迁移到统一 `Device` trait。`Device::resources()` 可返回多个 `Resource`，每个标记不同 `BusKind`。`handle()` 内部根据 `access.kind` 分发：
- `BusKind::Mmio` → offset=0x00 data, offset=0x08 selector, offset=0x10 DMA（现有逻辑）
- `BusKind::Port` → port `0x510`=selector (write, be16), port `0x511`=data (read, byte-stream)

同一个 `FwCfg` 实例同时响应 MMIO 和 PIO 访问，共享内部状态（selector、offset、DMA 地址）。这是 `DualBusDevice` 测试（`axdevice/src/device.rs:1443`）验证过的模式。

**与 `plan/uefi/design.md` 的"拆为三部分"方案（FwCfgStore + 两个 Transport）的关系**：语义上一致——item 目录、DMA、table-loader 核心是共享的；传输层分别处理 MMIO（LoongArch）和 PIO（x86）。但具体实现用 `Device` trait 的双资源声明替代三个独立类型的组合，避免 Rust 一致性规则冲突。

#### 6.1.2 重构 ACPI builder 架构分发

**经代码验证的函数签名和架构归属**：

| 函数 | 签名 | 架构归属 |
|------|------|---------|
| `build_acpi()` | `(cpu_num, &FwCfgPlatformConfig) -> AcpiBlobs` | 调用者中立，分派到架构特定子函数 |
| `build_dsdt()` | `(tables, platform)` → 调用 `build_loongarch_dsdt_aml()` | **全 LoongArch 硬编码** |
| `build_madt()` | `(tables, cpu_num, &FwCfgInterruptConfig)` → LoongArch type 17/20/21/22 | **全 LoongArch 硬编码** |
| `build_fadt()` | `(tables, _facs: u32, _dsdt: u32)` → 硬编码 `0x100e_001e` 等地址 | **全 LoongArch 硬编码** |
| `build_mcfg()` | `(tables, &FwCfgPciConfig)` → 纯参数化 | **架构无关** |
| `build_srat()` | `(tables, cpu_num, &[FwCfgRamRegion])` → 纯参数化 | **架构无关** |
| `build_spcr()` | `(tables, &FwCfgSerialConfig)` → 纯参数化 | **架构无关** |
| `build_rsdp()` | `(rsdp: &mut Vec<u8>)` → ACPI 1.0 RSDP (rev 0, 仅 RSDT) | **需升级到 ACPI 2.0** |

**改造方案**：用 `#[cfg(target_arch)]` 门控：

```rust
fn build_dsdt(tables: &mut Vec<u8>, platform: &FwCfgPlatformConfig) {
    let start = begin_acpi_table(tables, b"DSDT", 1);
    #[cfg(target_arch = "x86_64")]
    tables.extend_from_slice(&build_x86_dsdt_aml(platform));
    #[cfg(target_arch = "loongarch64")]
    tables.extend_from_slice(&build_loongarch_dsdt_aml(platform));
    end_acpi_table(tables, start);
}
```

`build_madt` 同理——x86 版从 `FwCfgPlatformConfig` 的新增 x86 字段读取参数（见 6.1.3），不使用 `FwCfgInterruptConfig`。

`build_fadt` 同理——x86 版使用标准 x86 FADT 值（见下方）。

`build_rsdp` 升级为 ACPI 2.0 RSDP（revision 2，含 XSDT 指针），新增 `build_xsdt`。

**生成的 ACPI 表通过 `GuestAcpiTables` 类型持有**（`axvm/src/boot/policy.rs:80`已有），写入 `GuestBootDescription` 后供运行时查询 RSDP GPA。

##### x86 FADT 关键字段

| 字段 | P1 值 | 说明 |
|------|-------|------|
| `SMI_CMD` | `0` | 设为 0 跳过 ACPI 模式切换；OVMF QEMU 路径的标准行为 |
| `PM1a_EVT_BLK` | `0x600` | P1 存根；P5 实现完整 PM 设备后激活 |
| `PM1a_CNT_BLK` | `0x604` | 同上 |
| `SCI_INT` | `9` | |
| `RESET_REG` (GAS) | IO, `0xCF9`, byte | |
| `RESET_VALUE` | `0x06` | |
| HW_REDUCED_ACPI | **不设置** | x86 使用传统 ACPI 硬件模型 |
| RESET_REG_SUP | 设置 | |

##### x86 MADT entry 结构

| Entry Type | 数量 | 内容 |
|-----------|------|------|
| LAPIC (type 0) | cpu_num 个 | Processor UID=vCPU index, LAPIC ID=vCPU index, Flags=1 |
| IOAPIC (type 1) | 1 个 | ID=0, MMIO base=`0xFEC0_0000`, GSI base=0 |
| ISO (type 2) | 2+ 个 | IRQ 0→GSI 2 (PIT), IRQ 9→GSI 9 (SCI)；后续加 RTC IRQ 8→GSI 8、COM1 IRQ 4→GSI 4 |
| NMI (type 4) | 1 个 | LINT1, Flags=5, GSI=0 (all CPUs) |

##### x86 DSDT 最小设备集（P1）

- `\_SB_.PCI0`：PCI host bridge（`_HID=PNP0A08`, `_CID=PNP0A03`）——先声明结构
- `\_SB_.COM1`：串口（`_HID=PNP0501`, `_CRS` PIO `0x3F8`-`0x3FF`, IRQ 4）
- `_S5_`：关机状态

#### 6.1.3 FwCfgPlatformConfig 的 x86 扩展

`FwCfgInterruptConfig` 的 7 个字段全 LoongArch 专用。在 `FwCfgPlatformConfig` 中增加 x86 专用字段：

```rust
// x86 专用（用 #[cfg] 或 Option 字段）
pub ioapic_base: u64,   // 默认 0xFEC0_0000
pub lapic_base: u64,    // 默认 0xFEE0_0000
pub gsi_base: u32,      // 默认 0
```

**x86 默认平台配置值**：

| 字段 | 默认值 | 说明 |
|------|--------|------|
| ram_regions | 从 VM config memory_regions 构建 | |
| serial.base / irq | `0x3F8` / 4 | COM1 |
| pci.ecam_base | `0xB000_0000` | < 2.75GB RAM 时不冲突 |
| pci.ecam_size | `0x1000_0000` (256MB) | |
| pci.mmio_base / size | `0xC000_0000` / `0x1000_0000` | 32-bit PCI MMIO 窗口 |
| pci.io_base / size | `0x6000` / `0x1000` | |
| pci.intx_base | 16 | PCI INTx GSI 起始 |
| ioapic_base | `0xFEC0_0000` | |
| lapic_base | `0xFEE0_0000` | |

#### 6.1.4 P1 的简化策略

通过 FADT `SMI_CMD=0` 告诉 OVMF ACPI 已启用，跳过 SMI 切换。**P1 不需要** PIIX4 PM 设备、PIRQ 路由、HPET。后续阶段（P4/P5）实现完整 PM 设备后将 `SMI_CMD` 设为 `0xB2`。

**涉及文件**：`axdevice/src/fw_cfg.rs`（Device trait 迁移、cfg 门控、FwCfgPlatformConfig 扩展、RSDP 升级）、`axvm/src/arch/x86_64/boot/mod.rs`（UEFI 路径调用 fw_cfg + ACPI 初始化）、`axvm/src/boot/policy.rs`（接入 `GuestAcpiTables`）

**退出条件**：OVMF 进入 UEFI Shell，能看到正确 CPU/内存/ACPI，不再因缺少 fw_cfg 或错误表卡在 DXE。

---

### P2：虚拟 PCI 根总线 + ECAM

**目标**：固件枚举由 AxVisor 拥有的 PCI function。

#### 6.2.1 PCI Host Bridge 虚拟化

- ECAM MMIO 区域：bus 0 = 1MB（32 dev × 8 func × 4KB），映射在 `pci.ecam_base`
- 配置空间存储：`BTreeMap<(bus, dev, func), Box<dyn PciDevice>>`
- `PciDevice` trait：`config_read`/`config_write`（8/16/32-bit）、`bar_read`/`bar_write`
- Host bridge (dev 0, func 0)：Vendor `0x8086`, Device `0x29C0`（QEMU q35 兼容）
- 如 OVMF 构建需要，可同时提供 `0xCF8`/`0xCFC` PIO config 访问（PCI 传统模式）

#### 6.2.2 PCI 中断路由（PIRQ）

x86 OVMF 可能通过 PIIX4 PIRQx 寄存器（PIO `0x60`-`0x63`）编程 INTx 路由。实现 4 个 8-bit 寄存器，Link Value 0-15 映射到 GSI 16-31，默认 `0x80`(disabled)。

#### 6.2.3 ACPI 描述

MCFG 已由 P1 生成。DSDT PCI0 包含：
- `_HID=PNP0A08`, `_CID=PNP0A03`, `_SEG=0`, `_BBN=0`
- `_CRS`：bus 0-0、IO range、MMIO range
- `_CBA`：ECAM base
- `_PRT`：32 slot × 4 pin → GSI 映射（GSI 16-19 for INTx A-D），通过 `PNP0C0F` link device

**LoongArch 的 `build_pci0_aml` 大部分可复用**——`_HID`/`_CID`/`_CBA`/`_CRS` 是标准 ACPI，仅 `_PRT` GSI 编号需调整。

#### 6.2.4 设备注册接口

```rust
trait PciDevice: Send + Sync {
    fn config_read(&self, offset: u16, width: AccessWidth) -> u32;
    fn config_write(&mut self, offset: u16, width: AccessWidth, value: u32);
}
fn register_pci_device(&mut self, dev: u8, func: u8, device: Box<dyn PciDevice>);
```

先用一个无 DMA 的测试 function 验证 OVMF `pci` 枚举、BAR 和 ACPI 路由。禁止 UEFI 配置使用外层 QEMU PCI BAR/PIO 透传作为替代。

**涉及文件**：新文件 `axdevice/src/pci.rs`、`axdevice/src/x86.rs`（PCI/PIRQ 初始化）、`axvm/src/arch/x86_64/boot/mod.rs`

**退出条件**：UEFI Shell 可稳定枚举 root bus 和测试 function，地址与 MCFG/DSDT 一致。

---

### P3：virtio-blk + 磁盘启动

**目标**：从 virtio-blk 磁盘完成第一条可用的 UEFI OS 启动链。

#### 6.3.1 virtio 1.0 PCI transport

- Vendor `0x1AF4`, Device `0x1042` (transitional block), Revision 1, Subsystem ID `0x0002`
- PCI capabilities（via cap chain）：Common (type 1)、Notifications (2)、ISR (3)、Device-specific (4)
- BAR 布局：Common @ BAR0+`0x0000`、ISR @ +`0x2000`、Notify @ +`0x3000`、DevCfg @ +`0x4000`

#### 6.3.2 Virtqueue 处理

一个 virtqueue (index 0, 256 descriptors)：
- Guest 填充描述符链 → 写 Available Index → 写 Notify 寄存器触发 kick
- Host 处理：读描述符链 → 磁盘 I/O → 写 Used Ring → 更新 Used Index → 触发中断（P4）
- **P3 可用轮询**：OVMF 在无中断时会轮询 Used Ring index

#### 6.3.3 磁盘后端

- `disk_path` 配置项（schema 已预留）指定 RAW 磁盘镜像
- 后端 trait 提供容量、读写、flush、只读属性
- 文件路径仅在 `os/axvisor` 转换为后端，不泄漏到设备核心

**涉及文件**：新文件 `axdevice/src/virtio/mod.rs` + `block.rs`、`axdevice/src/pci.rs`（集成）、`axvmconfig/`（disk 配置激活）

**退出条件**：UEFI Shell 可枚举 virtio-blk、读取 ESP 文件；EFI 应用能从磁盘启动。

---

### P4：中断链路（INTx + vIOAPIC）

**目标**：virtio 设备通过 INTx → vIOAPIC → VMCS 注入客户机中断。

#### 6.4.1 现有中断机制的精确描述

**已验证的注入路径**（`axvm/src/arch/x86_64/irq.rs`）：
- 所有 `inject_*` 函数首行都是 `if vm.interrupt_mode() != VMInterruptMode::Passthrough { return; }`
- PIT/COM1 注入：`devices.x86_ioapic_assert_gsi(gsi)` → `Option<IoApicInterrupt>` → `vcpu.get_arch_vcpu().inject_interrupt_with_trigger(vector, trigger)`
- 注入触发点：`DeferredRunWork::PreemptionTimer`（PIT/串口轮询）+ `DeferredRunWork::InterruptEnd { vector }`（EOI 后重注入）

**`InterruptFabric`**（`axvm/src/irq/mod.rs`）提供 `set_level(line, asserted)` / `pulse(line)` 抽象和 `IrqResolver` trait 实现。但当前 x86_64 UEFI 路径未通过 InterruptFabric 连接设备——注入仍走 `irq.rs` 全局函数。

#### 6.4.2 virtio INTx 注入方案

virtio kick 在 MMIO 分发中触发（`Device::handle()` 调用链），此时 vCPU run loop 在调用栈上方，无法直接产生 `DeferredRunWork`。采用 pending 标志 + `finish()` 入口轮询：

1. virtio 完成请求 → 写 Used Ring → `pending_interrupt.store(true, Release)`
2. `finish()` 入口调用 `inject_pending_virtio_irq(vm, vcpu)`（任何 `DeferredRunWork` 之前）
3. 注入函数：查 PCI `_PRT` 得 GSI → `x86_ioapic_assert_gsi(gsi)` → `vcpu.inject_interrupt_with_trigger()`
4. EOI 回环：guest LAPIC EOI → `DeferredRunWork::InterruptEnd { vector }` → 现有 `inject_pending_ioapic_irq_after_eoi()` 处理 level-triggered 重注入

**长期方向**：将 virtio 中断线通过 `InterruptFabric::set_level()` 接入，替代直接调用 `irq.rs` 全局函数，实现架构无关的中断路由抽象。

#### 6.4.3 MSI/MSI-X 支持（P5）

virtio 1.0 规范要求 MSI-X。P4 以 INTx 为 MVP；若 OVMF 构建拒绝无 MSI-X 设备，需提前实现 MSI-X capability 存根（Table BAR + PBA BAR）。MSI address/data 需由 vLAPIC/MSI router 解析，不把 guest vector 与宿主 `IrqId` 混为裸整数。

**涉及文件**：`axdevice/src/virtio/mod.rs`（pending 标志）、`axvm/src/arch/x86_64/irq.rs`（新增注入函数）、`axvm/src/arch/x86_64/exit.rs`（finish 入口调用）、`axdevice/src/pci.rs`（`_PRT` 查询）

**退出条件**：`/proc/interrupts` 显示 virtio 中断计数；Linux 内核通过 virtio-blk 无轮询完成 I/O。

---

### P5：PC 兼容 + 多核 + MSI-X + 配置收敛

**目标**：从 MVP 提升到可复用平台。

- 补 vPIC/i8259、IMCR、RTC/CMOS (`0x70`/`0x71`)、ACPI PM reset
- PIIX4 PM 设备：实现 PM1a_EVT/PM1a_CNT (SCI_EN 位)/PM_TMR 寄存器；FADT `SMI_CMD=0xB2`、`ACPI_ENABLE=0xF1`
- OVMF_VARS 独立加载：CODE @ `0xFFC0_0000` [2MB]、VARS @ `0xFFE0_0000` [2MB]；暂不实现 CFI flash 写回（OVMF 在空 VARS 上自动重建默认变量）
- vLAPIC timer、IPI、SIPI 和多 vCPU MADT
- MSI/MSI-X router，virtio-blk 切换到 MSI-X
- virtio-net + virtio-console
- 清理 UEFI guest 对宿主 IOAPIC、HPET 和 PCI 地址透传的依赖
- 引入新的 typed firmware/boot-source 配置类型（`axvmconfig`），`enable_bios`/`bios_load_addr` 保留为兼容输入，解析后给出弃用诊断
- NimbOS 和 ArceOS 产物作为 OVMF 可发现的 PE/COFF EFI 应用，各至少一个启动用例

**退出条件**：SMP Linux 稳定启动，时间、reset、磁盘 IRQ 和 MSI-X 有独立回归覆盖。

---

### P6：LoongArch 验证与配置收敛

**目标**：把已有 LoongArch 实现变成受支持门禁路径。

- 为 QEMU-LVZ 建立带 `linux-rootfs-smp1.toml` 的 guest test group
- 固定 guest OVMF、kernel、rootfs 和 QEMU-LVZ 版本
- 成功条件改为 Linux guest 用户态标记，覆盖 virtio-blk 读写
- 复用新的 boot-source/firmware 配置，同时保留 LoongArch MMIO fw_cfg 和平台发现
- 对比重构前后的 FDT、ACPI、IRQ route 和 rootfs 行为，防止 x86 抽象反向破坏 LoongArch

**退出条件**：x86_64 和 LoongArch 各至少有一条自动化 guest UEFI Linux 端到端用例。

---

## 7. 关键技术决策

| # | 决策 | 选择 | 理由 |
|---|------|------|------|
| 1 | **FwCfg 双总线实现** | 迁移到 `Device` trait，单实例同时声明 MMIO+PIO 资源 | Rust 一致性规则不允许同一类型实现两个 `BaseDeviceOps` 变体；`Device` trait 天然支持多资源；语义上等价于 plan/uefi 的"FwCfgStore+两个 Transport"分离 |
| 2 | **ACPI builder 拆分** | `#[cfg(target_arch)]` 门控 DSDT/MADT/FADT | 最小侵入 LoongArch；`build_mcfg`/`build_srat`/`build_spcr` 纯参数化无需改动 |
| 3 | **RSDP 版本** | ACPI 2.0 (rev 2) + XSDT | x86_64 UEFI 需要 64-bit 表指针；LoongArch 也受益于 XSDT |
| 4 | **x86 不引入 FDT** | fw_cfg (ACPI+SMBIOS) + UEFI Protocols | x86 UEFI 固件不使用设备树 |
| 5 | **PCI 范围** | bus 0 only, 32-bit MMIO | MVP 够用，可扩展到多 bus 和 64-bit |
| 6 | **VARS 持久化** | P5 做独立加载，暂不做 CFI 写入回写 | OVMF 在空 VARS 上自动重建默认变量，不阻塞启动 |
| 7 | **OVMF 固件** | 不含 SMM 的构建 | SMM 仿真（SMRAM、SMI handler）超出 MVP 范围 |
| 8 | **ACPI SMI_CMD** | P1 设为 0 跳过模式切换 | 无需 PM 设备即可验证 UEFI Shell；P5 实现 PM 后设为 `0xB2` |
| 9 | **中断注入方式** | `finish()` 预检查 pending 标志 | virtio kick 在 `Device::handle()` 中触发，`DeferredRunWork` 在 vCPU run loop 边界产生，两者有因果链断裂 |
| 10 | **中断模式** | 现有 `passthrough` 模式 | 符合现有 x86 UEFI 配置；长期通过 `InterruptFabric` 解耦 |
| 11 | **FwCfgInterruptConfig** | 保持 LoongArch 专用；x86 用 FwCfgPlatformConfig 新增字段 | 7 个 LoongArch 字段在 x86 全无意义 |
| 12 | **配置迁移策略** | P5 再做 typed config，P1-P4 增量扩展现有扁平字段 | 避免大规模 schema 迁移阻塞功能验证 |
| 13 | **先 ACPI 还是先 PCI** | 先 ACPI（P1）→ 再 PCI（P2）→ 再 virtio（P3） | 共享 `GuestPlatform`；先用测试 function 验证表和枚举，再引入 DMA |

---

## 8. 架构对比：LoongArch vs x86_64 UEFI

| 维度 | LoongArch (已有) | x86_64 (目标) |
|------|-----------------|---------------|
| **固件入口** | EDK2 @ `0x1C00_0000` | OVMF reset vector @ `0xFFFF_FFF0` |
| **平台信息** | FDT + fw_cfg (ACPI) | fw_cfg (ACPI + SMBIOS)，无 FDT |
| **MADT** | CPU(17)+EIOINTC(20)+PCH-MSI(21)+PCH-PIC(22) | LAPIC(0)+IOAPIC(1)+ISO(2)+NMI(4) |
| **FADT** | GAS @ `0x100E_001C`/`0x100E_001E`, 无 SMI_CMD | PM1a @ `0x600`/`0x604`, SMI_CMD=P1=0→P5=`0xB2`, RESET @ `0xCF9` |
| **RSDP** | ACPI 1.0 (RSDT only) | ACPI 2.0 (RSDT + XSDT) |
| **PCI** | ACPI 描述（宿主直通） | 虚拟 PCI host bridge + ECAM |
| **virtio** | 无（直通宿主设备） | virtio-pci 软件仿真 |
| **中断** | EIOINTC + PCH-PIC（直通） | vIOAPIC + vLAPIC（软件虚拟化）+ InterruptFabric |
| **Flash** | 2-bank @ `0x1C00_0000`/`0x1D00_0000` | 2-bank @ `0xFFC0_0000`/`0xFFE0_0000` |
| **fw_cfg** | MMIO @ `0x1E02_0000` | PIO `0x510`/`0x511`（Device trait 统一） |
| **串口** | MMIO @ `0x1FE0_01E0` | PIO `0x3F8` (COM1) |
| **RTC** | MMIO（直通） | PIO `0x70`/`0x71`（仿真） |
| **GED** | `0x100E_001C` | N/A（用 PIIX4 PM） |
| **PIRQ** | N/A | PIO `0x60`-`0x63` |

---

## 9. 文件变更清单

### 核心改动

| 文件 | 改动 | 阶段 | 说明 |
|------|------|------|------|
| `axdevice/src/fw_cfg.rs` | 重构 | P1 | 迁移到 Device trait（双总线 MMIO+PIO），cfg 门控 DSDT/MADT/FADT，FwCfgPlatformConfig 扩展，RSDP→ACPI 2.0 |
| `axdevice/src/pci.rs` | **新文件** | P2 | PCI host bridge + ECAM + PIRQ |
| `axdevice/src/virtio/mod.rs` | **新文件** | P3 | virtio 1.0 PCI transport |
| `axdevice/src/virtio/block.rs` | **新文件** | P3 | virtio-blk 后端 |
| `axvm/src/arch/x86_64/boot/mod.rs` | 扩展 | P1-P3 | UEFI 流程：fw_cfg→ACPI→PCI→virtio 初始化 |
| `axvm/src/arch/x86_64/irq.rs` | 扩展 | P4 | `inject_pending_virtio_irq()` |
| `axvm/src/arch/x86_64/exit.rs` | 扩展 | P4 | `finish()` 入口预检查 |
| `axvm/src/boot/policy.rs` | 扩展 | P1 | 接入 `GuestAcpiTables` |

### 辅助改动

| 文件 | 改动 | 说明 |
|------|------|------|
| `axdevice/src/x86.rs` | 扩展 | PCI/PIRQ 初始化，RTC/PM 设备（P5） |
| `axdevice/src/lib.rs` | 扩展 | 导出 pci、virtio 模块 |
| `axvmconfig/src/lib.rs` | 扩展 | disk_path 激活、uefi_vars_path、P5 typed config |
| `axvm-types/src/lib.rs` | 扩展 | FwCfgPlatformConfig x86 字段，typed firmware/boot-source（P5） |
| `axdevice/src/device.rs` | 轻量扩展 | virtio 设备注册方法 |
| 配置 TOML | 更新 | x86_64 UEFI 测试配置（fw_cfg 设备、VARS 区域、disk） |
| CI 配置 + test-suit | 更新 | 端到端 x86 UEFI + LoongArch UEFI 用例 |

### 不改动

- LoongArch 路径（仅 cfg 门控重命名，逻辑不变）
- Direct boot / Multiboot 路径
- ARM / RISC-V 路径
- `x86_vlapic` 核心（IOAPIC/LAPIC 寄存器模型不变）

---

## 10. 验证矩阵

| 层级 | 必测项 | 通过标准 |
|------|--------|---------|
| 配置 | 协议/固件/启动源组合、旧字段兼容 | 非法组合在 VM 创建前返回 typed error |
| fw_cfg | selector/data PIO/MMIO、DMA、目录边界 | 与 QEMU fw_cfg 契约一致，LoongArch 回归不变 |
| ACPI | checksum、长度、指针、CPU/APIC/PCI 资源 | 单元测试 + iasl 解析通过 |
| PCI | config width、BAR probe、越界 BDF | OVMF Shell 枚举结果稳定 |
| virtio-blk | feature negotiation、DMA 越界、flush、只读 | 单元测试 + guest 读写校验 |
| 中断 | INTx mask/assert/deassert、remote-IRR、EOI、重注入 | 不丢中断、不重复完成 |
| x86 VMX | OVMF → 磁盘 → Linux 用户态 | guest marker；不接受宿主 marker |
| x86 SVM | 与 VMX 相同 guest 配置 | 同一 marker 和磁盘校验 |
| x86 ArceOS/NimbOS | OVMF → EFI 应用 | guest marker；不接受裸 GPA 预装 |
| LoongArch LVZ | OVMF/fw_cfg → Linux rootfs | guest marker + virtio-blk 读写 |
| 回归 | x86 direct/multiboot、其他架构 direct boot | 现有 smoke 继续通过 |

推荐运行命令：

```bash
cargo test -p axvmconfig
cargo test -p axdevice
cargo test -p axvm
cargo xtask axvisor test qemu --arch x86_64 --test-group uefi --test-case <case>
```

---

## 11. 关键风险与缓解

| 风险 | 缓解 |
|------|------|
| OVMF 发行物差异（2M/4M、combined/split、内置 driver 不同） | 固定 firmware profile，加载地址由文件大小和 profile 校验 |
| virtio 1.0 要求 MSI-X，OVMF 可能拒绝 INTx-only 设备 | OVMF QEMU 路径接受 INTx；不兼容时 P4 提前加 MSI-X capability 存根 |
| PIO fw_cfg selector/data 偏移 vs MMIO 不同 | `Device::handle()` 内部按 `BusKind` 分支，PIO 端口逻辑独立 |
| Device trait 迁移影响 LoongArch fw_cfg 用户 | cfg 门控回归测试保护 |
| ECAM 地址与 RAM 扩展冲突 | 配置化 ECAM 地址，VM 创建时校验不重叠 |
| 透传基线的定位——UEFI 不能依赖宿主 BAR 地址 | 保留现有 x86 direct Linux 透传测试用于嵌套虚拟化回归，UEFI 平台不复用 |
| 多 guest 隔离——fw_cfg selector、PCI config、flash 状态必须 VM 实例所有 | 全局 x86 IRQ 转发状态不应成为最终模型；P5 迁移到 InterruptFabric |
| VARS 持久化安全 | 默认使用 VM 私有副本，异常退出不覆盖模板 |

---

## 12. 外部兼容契约

- [QEMU fw_cfg specification](https://qemu.readthedocs.io/en/v9.1.3/specs/fw_cfg.html)：selector/data/DMA 寄存器、x86 I/O 端口、MMIO endian、file directory、DMA 契约
- [TianoCore OVMF How-to](https://github.com/tianocore/tianocore.github.io/wiki/How-to-run-OVMF)：pflash 基本运行方式
- [TianoCore OVMF FAQ](https://github.com/tianocore/tianocore.github.io/wiki/OVMF-FAQ)：非易失变量依赖 flash 仿真的兼容性约束
- 实现时应对照固定 OVMF profile 对应的 EDK2 `OvmfPkg` 源码，而非仅依据发行版文件名推断固件布局

---

## 13. 自检结果

### 代码验证确认

- [x] `build_madt` 全 LoongArch type 17/20/21/22 硬编码，x86 需完全重写
- [x] `build_fadt` 硬编码 `0x100E_001E`/`0x100E_001C` LoongArch 地址
- [x] `FwCfgInterruptConfig` 7 个字段全 LoongArch 专用
- [x] `build_mcfg`/`build_srat`/`build_spcr` 纯参数化，无需改动
- [x] `build_rsdp` 仅 ACPI 1.0 rev 0，x86 需升级
- [x] FwCfg 仅旧式 `BaseDeviceOps<GuestPhysAddrRange>`，需迁移到 Device trait
- [x] `Port`/`PortRange`（框架）vs `X86Port`/`X86PortRange`（x86_vlapic）是两套独立类型体系
- [x] 所有 `inject_*` 函数被 `VMInterruptMode::Passthrough` 门控
- [x] `InterruptFabric` 存在于 `axvm/src/irq/mod.rs`，提供 `set_level`/`pulse`/`IrqResolver`
- [x] `GuestAcpiTables` 存在于 `axvm/src/boot/policy.rs`，尚未接入任何架构
- [x] `disk_path` 字段已在 `VMKernelConfig` schema 中预留，但无消费者
- [x] `EmulatedDeviceType::VirtioBlk` (`0xE1`) 枚举值已预留
- [x] x86 UEFI CI 完全缺失（测试套件文件存在但 CI 矩阵未引用）
- [x] LoongArch CI 只启动 shell，`linux-rootfs-smp1.toml` 未接入测试套件
- [x] `.github/workflows/qemu-x86_64-uefi.toml` 实际是 BIOS 配置（`uefi = false`），非真正 UEFI 测试
- [x] PCI ECAM `0xB000_0000` 不冲突于现有配置
- [x] INTx GSI 16-19 在 IOAPIC 24 RTE (0-23) 范围
- [x] `SMI_CMD=0` 是 OVMF QEMU 路径标准行为

### 内部一致性

- [x] P0-P6 阶段依赖链清晰：P0(基线)→P1(ACPI)→P2(PCI)→P3(virtio)→P4(中断)→P5(完善)→P6(LoongArch)
- [x] 配置迁移策略分两步：P1-P4 增量扩展现有字段，P5 引入 typed config
- [x] fw_cfg 的 `Device` trait 迁移方案语义上等价于 plan/uefi 的"拆为三部分"，实现上更务实
- [x] 中断方案从 P4 的 `finish()` 轮询到 P5 的 `InterruptFabric` 集成有明确演进路径
- [x] 每阶段都有独立可验证的退出条件，不依赖后续阶段的未实现功能
