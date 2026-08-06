# AxVisor x86_64 Guest UEFI 支持 — 设计方案

> 基线：`dev` 分支当前 HEAD（`4ade9d191`，2026-08-05 复核）。
> 风险等级：**高风险功能**（涉及硬件协议、MMIO/DMA/IRQ、页表、启动/虚拟化契约、跨 vCPU 并发、新 crate 边界和公共 API）。
> 本文为独立可评审的设计材料，按 `book/guideline/feature-development.md` 高风险功能要求编写。
> 历史说明：本设计最初编写于 `dev@47c044f7d`，2026-08-04 按最新 dev 复核并同步；2026-08-05 按 `4ade9d191` 复核阶段 2 章节并同步（修正 fw_cfg PIO 接入方式、MADT override 条目、阶段 2 退出条件格式，并按 `virtualization/` 前缀统一 dev 代码路径引用）。当前执行范围为**阶段 1（SEC 到达验证）**，阶段 2-5 保留为后续路线图，不在本次实现范围内。

## 1. 目标

### 1.1 问题

AxVisor 当前只能以传统 BIOS/multiboot 方式启动 x86_64 guest：

- 依赖自带的 `axvm-bios.bin` 固件，无法使用行业标准 OVMF/UEFI 固件；
- 没有 ACPI 表、PCI 总线和 virtio-pci 设备模型，guest 无法通过标准 PC 发现路径找到启动盘；
- vCPU 初始状态为实模式 trampoline，不区分传统 BIOS 和 UEFI 的启动语义；
- Linux 等主流 guest OS 的 UEFI 启动路径完全不可用。

**证据**：

- `virtualization/axvm/src/machine.rs:335`：x86 machine profile 固定提供 16550 COM1（PIO `0x3f8`，GSI 4）、vIOAPIC、PIT，且配置明确禁止 `emu_devices`/`serial` 等字段（`docs/docs/architecture/axvisor-guest-machine.md:70`）——UEFI 固件在 AxVisor 上首次启动只能落在 SEC 之前，尚无 ACPI/PCI/virtio 平台发现路径；
- `virtualization/axvm/src/arch/x86_64/boot/mptable.rs`：x86 仅有 SMP MP 表（legacy），无 ACPI 表生成；
- `virtualization/axvm-types/src/lib.rs`：`VirtioBlk/VirtioNet/VirtioConsole` 类型已定义，但无 DeviceFactory 实现（`virtualization/axdevice/src/lib.rs` 未导出 virtio 设备）；
- `.github/workflows/ci.yml`：x86 UEFI 相关 CI 目前仅保留 axloader 的 UEFI smoke（`ci.yml:705-743`，下载发行版 OVMF 后执行 `cargo axloader test qemu --target x86_64-unknown-uefi`）；AxVisor 侧无 UEFI 测试门禁（阶段 1 计划为 non-gating，阶段 5 升级为 gating）；
- LoongArch 已有完整 ACPI + fw_cfg + PCI + virtio 的 UEFI 启动参考实现（`virtualization/axvm/src/arch/loongarch64/boot/`），x86 侧完全缺失。

**受众**：

- AxVisor x86 平台开发者：需要可诊断的 UEFI 启动路径来开发和调试虚拟设备；
- Guest OS 集成者：需要标准 UEFI 环境来启动 Linux 和其他 UEFI 应用（NimbOS 用例及其配置已被 `d7050469f` 从 dev 删除，仅作历史背景）；
- CI 维护者：需要可复现的 UEFI 测试门禁。

### 1.2 使用场景

开发者构建包含固定 OVMF 固件和 rootfs 的 AxVisor 镜像后，执行：

```bash
cargo xtask axvisor test qemu --arch x86_64 --test-group uefi --test-case ovmf-entry-svm
```

期望结果：AxVisor 启动指定 VM，OVMF 固件从 reset vector 进入 SEC 阶段，Guest COM1 输出 `SecCoreStartupWithStack(`，测试通过。

最终场景（阶段 3 完成后）：

```bash
cargo xtask axvisor test qemu --arch x86_64 --test-group uefi --test-case linux-uefi
```

期望结果：OVMF 通过 ACPI 发现平台硬件、通过 PCI 枚举发现 virtio-blk 启动盘、加载 Linux EFI 应用（原 NimbOS 用例及其测试框架已被 `d7050469f` 从 dev 删除，阶段 3 的目标用例按现有 `arceos-uefi-smp1.toml` 的配置模型新建对应的 Linux EFI 启动配置）。

### 1.3 成功标准

1. 固定版本的 OVMF 固件在 AxVisor VMX 和 SVM 后端上稳定进入 SEC，Guest COM1 输出 SEC marker（阶段 1 目标）；
2. OVMF 通过 fw_cfg 获取 ACPI 表，推进到 DXE/BDS；
3. OVMF 通过 PCI 枚举发现 virtio-blk 设备，读取 ESP 并进入 UEFI Shell 或 EFI 应用（阶段 3 起）；
4. Linux EFI stub 通过 virtio-blk 成功启动到 initramfs shell；
5. SMP guest（≥2 vCPU）通过 INIT/SIPI 成功启动；
6. 以上所有场景的测试在 CI 中为 gating（阶段 1 保持 non-gating，证据齐全后升级）。

### 1.4 非目标

- pflash 设备及 OVMF VARS 持久化；
- Secure Boot、SMM、TPM、网络启动（PXE）；
- virtio-net、virtio-console 的完整实现（virtio-blk 优先）；
- LoongArch UEFI 功能（LoongArch 已有独立实现路径）；
- 热插拔、NUMA、SRAT。

### 1.5 不实现的代价

- x86_64 guest 只能通过非标准 BIOS/multiboot 路径启动，无法启动主流 Linux 发行版的 UEFI 安装镜像；
- 无法复用 OVMF 的 DXE 驱动生态（PCI 枚举、文件系统驱动、UEFI Shell 诊断）；
- 虚拟设备开发缺少标准固件的反馈循环：新设备是否正确暴露、中断是否到达、ACPI 表是否被解析，都没有标准 OVMF 作为"探针"；
- 与 LoongArch UEFI 路径的差距持续扩大，跨架构公共抽象无法收敛。

---

## 2. 资料检索与 prior art

### 2.1 项目内部检索

按 `book/guideline/feature-development.md` §4.1 要求，检查了以下项目内部资源：

| 检索范围 | 发现 | 结论 |
|----------|------|------|
| `axvmconfig` / `axvm-types` 的 `VMBootProtocol`、`EmulatedDeviceType` | `VMBootProtocol::Uefi`、`FwCfg`/`VirtioBlk`/`VirtioNet`/`VirtioConsole` 类型已存在 | 复用现有枚举，新增工厂实现和平台接入 |
| `axdevice` 的 `FwCfg`、`DeviceFactory`、`DeviceRuntime` | fw_cfg 核心传输层已完成（MMIO + DMA，`FwCfgDmaDevice::access` 仅接受 `BusKind::Mmio`），DeviceFactory 注册机制已就绪 | 复用 fw_cfg 核心，按 §7 阶段 2 扩展 PIO transport（或新增 Port 变体设备） |
| `x86_vlapic` 的 `EmulatedLocalApic`、`EmulatedIoApic`、`EmulatedPit`、`EmulatedSerialPort` | vLAPIC（EOI/IPI/Timer/LVT）、vIOAPIC（24 GSI）、PIT、COM1 均已实现 | 复用现有仿真，补全 INIT/SIPI/NMI/PIC |
| `x86_vcpu` 的 VMX/SVM backend、EPT/NPT、reset vector | `0xfffffff0` → CS:F000/IP:FFF0 已正确映射 | 复用现有 vCPU 入口 |
| `virtualization/axvm/src/arch/x86_64/boot/mptable.rs` | 已有 SMP MP 表生成（LAPIC + IOAPIC + PCI INTx routing） | ACPI 表是 MP 表的替代/升级，INTx 路由值可复用 |
| `virtualization/axvm/src/arch/x86_64/irq.rs` | host IRQ forwarding 框架（GSI lease、mask/re-arm、drain-before-run）已就绪 | 复用 `X86InterruptDomain`，新增 PIC/MSI 路由 |
| `virtualization/axvm/src/arch/loongarch64/boot/acpi.rs`（878 行） | 完整 ACPI 表生成（RSDP/RSDT/FADT/MADT/SRAT/SPCR/MCFG + DSDT AML + QEMU loader），入口 `build()`（`acpi.rs:133-141`）返回 `FwCfgAcpiBlobs`，经 `fw_cfg_platform_config` 注入 `FwCfgPlatformConfig`（`boot/mod.rs:283`，位于 `add_uefi_fw_cfg` 的 vm setup/loader 阶段，顶层函数 `boot/mod.rs:183`） | 作为 x86 实现的**参考模板**，但表内容（MADT entry 类型、DSDT AML 设备树）为 arch-specific，不能直接复用；x86 写独立的 `build_x86_acpi()` 入口 |
| `virtualization/axvm/src/arch/loongarch64/boot/` 的 `GuestPlatform`、`probe.rs`、`fdt/` | 完整的平台发现→ACPI 构建→fw_cfg 注入→FDT 生成链路 | 整体架构可借鉴，x86 不需要 FDT 分支 |
| `os/axvisor/configs/vms/qemu/x86_64/arceos-uefi-smp1.toml` | 现存 UEFI guest 配置（`guest_type = "passthrough"`、`boot_protocol = "uefi"`、`uefi_firmware_path`、`bios_load_addr`，机器设备由 machine profile 提供） | 阶段 1 复用该配置模型，新建专用 ovmf-entry 配置 |
| `uefi/p0` 分支（`941134916`） | 旧 P0 实现（固定 OVMF profile、SEC observer、trace），涉及 24 文件 +1707/-193 | 功能意图可参考，但基线过旧：COM1 已 machine-owned、配置模型已改、脚本/NimbOS 用例已删除。不直接复用代码，按 `solve_conflict.md` 重做 |
| `feat/uefi-phase1` 分支（`3782df379` + `f0fdd421c`） | 基于 `f1774c1f6` 的复刻：固定 OVMF 校验、SEC observer、设备 trace、ovmf-entry 配置 | 同上，代码不直接搬用；其中的 SEC 到达验证经验保留为需求输入 |
| `.github/workflows/ci.yml` | x86 UEFI 相关 CI 仅保留 axloader 的 UEFI smoke（`ci.yml:705-743`，下载发行版 OVMF 后执行 `cargo axloader test qemu --target x86_64-unknown-uefi`） | AxVisor 侧无 UEFI 测试门禁；阶段 1 恢复为 non-gating，阶段 5 升级为 gating |

### 2.2 外部规范与参考

按 `book/guideline/feature-development.md` §4.2 证据优先级检索：

| 规范 / 资料 | 版本 / 标识 | 可获取位置 | 与本方案相关的结论 |
|------------|------------|-----------|-------------------|
| UEFI Specification | 2.10 (§7.2, §7.5, §8, §10) | uefi.org/specifications | 固件从 reset vector 进入 SEC，依次通过 PEI、DXE、BDS；DXE 阶段通过 ACPI/PCI 发现硬件；BDS 通过 UEFI Boot Manager 选择启动项 |
| ACPI Specification | 6.5 (§5.2.5, §5.2.8, §5.2.9, §5.2.11, §5.2.12) | uefi.org/specifications | RSDP 含 XSDT 指针；XSDT 指向 FADT/MADT/MCFG 等表；MADT 含 LAPIC（type 0）、IOAPIC（type 1）、Interrupt Source Override（type 2）entry |
| PCI Firmware Specification | 3.3 (§4.1) | pcisig.com/specifications | ECAM 基址和总线范围在 MCFG 中声明；每个 bus 的配置空间映射为 MMIO 窗口 |
| VirtIO Specification | 1.2 (§4.1, §4.2) | docs.oasis-open.org/virtio/ | PCI transport 通过 PCI capability 链暴露 Common/Notify/ISR/Device CFG 结构；transport 与 device type 分层 |
| Intel SDM | Vol.3 §10.4, §10.12, §11.9 | intel.com/sdm | LAPIC 在 `0xFEE00000`，xAPIC/x2APIC 模式；IOAPIC 在 `0xFEC00000`；MSI address `0xFEE00000-0xFEEFFFFF`，data 含 vector + delivery mode + trigger mode |
| Intel SDM | Vol.3 §9.1.4 | intel.com/sdm | x86 reset vector `0xFFFFFFF0`，CS=0xF000、CS.base=0xFFFF0000、RIP=0xFFF0 |
| EDK2 / OvmfPkg | `edk2-stable202605`，`OvmfPkg/OvmfPkgX64.dsc` | github.com/tianocore/edk2 | 4MB FD 布局：VARS 在 `0x0-0x84000`，CODE 在 `0x84000-0x400000`；CODE 映射到 `0xFFC84000-0xFFFFFFFF`，reset vector 落在 CODE 最后 16 字节 |
| QEMU q35 机型 | QEMU 9.2，`hw/i386/pc_q35.c`，`hw/pci-host/q35.c` | gitlab.com/qemu-project/qemu | MCH 的 PCI 主桥在 bus 0 slot 0 function 0；PIRQA→GSI16, PIRQB→GSI17, PIRQC→GSI18, PIRQD→GSI19；ECAM 默认 `0xB0000000`（可重定位）；fw_cfg PIO 在 `0x510`/`0x511` |
| QEMU fw_cfg | `docs/specs/fw_cfg.rst`，`hw/nvram/fw_cfg.c` | gitlab.com/qemu-project/qemu | PIO selector `0x510`、data `0x511`；DMA 通过 `0x514` 的 8 字节寄存器触发；file directory 结构为 4 字节 count + 每项 64 字节（size + selector + reserved + name） |
| tgoskits CI | `ci.yml:705-743`（axloader UEFI smoke：下载发行版 OVMF 后执行 `cargo axloader test qemu --target x86_64-unknown-uefi`） | 本仓库 `.github/workflows/ci.yml` | 展示 x86 UEFI 测试的基础命令与配置格式；AxVisor 侧 UEFI 门禁阶段 1 为 non-gating，阶段 5 升级为 gating |

---

## 3. 方案比较

按 `book/guideline/feature-development.md` §5 要求，比较以下方案：

| 维度 | 方案 A：保持现状 | 方案 B：扩展 MP 表路径 | 方案 C：最小 q35 ACPI（本方案） | 方案 D：全功能 q35 仿真 |
|------|:---:|:---:|:---:|:---:|
| **语义正确性** | — 无法启动 UEFI guest | 部分（MP 表仅覆盖中断，无 PCI/ACPI 发现） | 满足最小可行目标 | 完整 q35 兼容 |
| **架构适配** | 无变化 | 与现有 mptable.rs 一致，但不解决 ACPI 需求 | 每层只增加必要能力，复用 fw_cfg/DeviceFactory/vLAPIC | 需要完整的南桥/北桥/LPC/PM 仿真 |
| **接口代价** | 无 | `mptable.rs` 扩展为 ACPI 生成器（职责混乱） | 新增 `virtualization/axvm/src/arch/x86_64/boot/acpi.rs`、`virtualization/axdevice/src/pci/`、`virtualization/axdevice/src/virtio/` | 新增大量 q35 设备模型和持久化接口 |
| **实现复杂度** | — | 低（但功能不完整） | 中等，分 5 阶段递进 | 极高，远超当前需求 |
| **兼容性** | 不变 | 不影响现有 BIOS 路径 | 不影响现有 BIOS 路径，UEFI 在新配置中 opt-in | 现有配置需要大量适配 |
| **资源与性能** | 无额外开销 | 与现状相同 | 新增 ACPI blob 内存（~16 KiB）、PCI ECAM MMIO 窗口（128 MiB guest 地址空间）、virtio virtqueue DMA（开销与现有 passthrough 路径相当） | 需大量常驻内存和 MMIO 窗口 |
| **可测试性** | — | 仅能验证 MP 表生成 | 每阶段有独立测试和退出条件 | 需要 q35 兼容性测试套件 |
| **演进成本** | 积累技术债务 | 后续仍需 ACPI，MP 表路径会成为废弃代码 | 阶段 5 抽取跨架构公共部分 | 过度投入，多数设备 guest 不使用 |

**选择方案 C**。理由：

- 方案 A 阻塞所有 UEFI 相关工作，代价不可接受；
- 方案 B 只解决中断路由，不解决设备发现（ACPI、PCI），UEFI guest 仍然无法找到启动盘；
- 方案 D 虽然有长期价值，但当前 AxVisor 没有需要全功能 q35 的 guest 场景。多数 OVMF 构建可以通过最小 ACPI 表集合 + 精简 PCI 主桥 + virtio-blk 进入 UEFI Shell。先实现最小集，后续按真实需求逐步加入更多设备能力；
- 方案 C 的每个阶段都产出可独立验证的能力增量（SEC 入口 → DXE → BDS → EFI 应用 → Linux），而不是等到全部完成后才能看到启动成功。

**放弃的**：全功能 q35 设备模型（SMBus、LPC、SATA、USB）。这些在 guest OS 通过 virtio-blk 启动后可以逐步加入。

**付出的代价**：阶段 2 生成的 ACPI DSDT AML 只包含 PCI0 和 COM1，后续新增设备（HPET 虚拟化、RTC）需要更新 DSDT；但 OVMF 在 DXE 阶段可以接受部分设备缺失，不影响核心启动路径。

---

## 4. dev 现状

### 4.1 已具备

**启动配置模型**
- `VMBootProtocol::Uefi` 枚举，TOML 解析 `boot_protocol = "uefi"`（`virtualization/axvmconfig/src/lib.rs`）
- `uefi_firmware_path` / `bios_load_addr` 配置字段，arch 校验矩阵
- `load_uefi_from_configured_path()` 从文件系统加载固件（`virtualization/axvm/src/arch/x86_64/boot/mod.rs:259`，含大小查询与 layout 校验挂点）
- x86 vCPU reset vector `0xfffffff0` → `CS:F000, IP:FFF0` 正确映射（`virtualization/x86_vcpu/src/lib.rs`）

**machine-owned 串口**
- x86 machine profile 固定提供 16550 COM1（PIO `0x3f8`，GSI 4，`virtualization/axvm/src/machine.rs:335`，`GuestSerialProfile` 在 `machine.rs:337-339`），配置不能覆盖；输出统一经 `os/axvisor/src/guest_console/mux/mod.rs:310` 汇聚，每次设备 generation 创建新 backend（`virtualization/axdevice/src/serial/backend.rs:19`）
- 因此阶段 1 的 SEC 判定直接以 guest COM1 输出为准（success_regex），无需在 axvm 侧再实现串口截获或 observer 注册

**host 依赖边界（`4c7aa8577` 新增，`refactor(axvisor): clean up host dependencies (#1861)`）**
- `virtualization/axvm/src/host/`：host OS 适配边界（`host/mod.rs`、`host/arceos.rs`），`os/axvisor` 不再反向依赖 axvm 的 host 细节
- 阶段 1 若需要固件相关 host 服务（如镜像 registry 访问、IO 路径），应放入 `axvm/src/host/` 或 axbuild，不直接在 `os/axvisor/src/config.rs` 扩展

**fw_cfg 传输层**
- 完整的 QEMU 兼容 **MMIO** fw_cfg 设备：选择器/数据寄存器、DMA 传输、QEMU loader 协议（`virtualization/axdevice/src/fw_cfg.rs`，994 行）。注意 `FwCfgDmaDevice::access` 仅接受 `BusKind::Mmio`（`fw_cfg.rs:651-652`），非 MMIO 访问直接返回 `DeviceError::OutOfRange`；`FwCfgBuildConfig.base` 为 `GuestPhysAddr`（MMIO 语义，`fw_cfg.rs:517`）——x86 PIO 接入需按 §7 阶段 2 扩展核心（§6 决策 2）
- `FwCfgAcpiBlobs` / `FwCfgPlatformConfig` 类型，ACPI/SMBIOS/memmap 文件目录
- `DeviceFactory` 注册、DMA grant、payload factory 均就绪

**已模拟的 x86 平台设备**
- COM1 串口（`x86_vlapic::EmulatedSerialPort`），PIO `0x3f8-0x3ff`（由 machine profile 注册，GSI 4）
- PIT 定时器（`x86_vlapic::EmulatedPit`），PIO `0x40-0x61`
- vIOAPIC（`x86_vlapic::EmulatedIoApic`），MMIO `0xfec00000`
- 以上设备均由 machine profile 通过 `DeviceFactory` 注册（`machine.rs:335`），配置不能覆盖

**guest 串口输出路径**
- guest COM1 输出经 `os/axvisor/src/guest_console/mux/` 汇聚（`mux/mod.rs:310` 起，`create_serial_backend`，generation 递增在 `mux/mod.rs:308-313`），每次设备 generation 创建新 backend（`virtualization/axdevice/src/serial/backend.rs:19`）
- 阶段 1 的 SEC 判定直接以该路径的 guest 输出为准（success_regex），**不需要**在 axvm 侧重新实现串口截获、VM ID 全局表或 `before_first_run`/`on_last_vcpu_exit` 挂钩（旧 P0 方案已过时）

**中断链路**
- vLAPIC：xAPIC MMIO（`0xfee00000`）、x2APIC MSR、EOI 处理、IPI Fixed 模式、Timer（OneShot/Periodic/TSCDeadline）、全部 LVT 条目（`virtualization/x86_vlapic/src/vlapic.rs`）
- vIOAPIC：24 个重定向表项、GSI 断言/向量查询/EOI 广播、level-triggered 中断（`virtualization/x86_vlapic/src/vioapic.rs`）
- 完整的 host IRQ forwarding 框架：GSI lease、mask/re-arm、drain-before-run（`virtualization/axvm/src/arch/x86_64/irq.rs`）
- `VmInterruptSender` 和每 vCPU 中断队列（`virtualization/axvm/src/runtime/vcpus.rs`）

**LoongArch 参考实现**

完整的 x86 可参考链路：`GuestPlatform` 发现 → ACPI 表构建（878 行）→ fw_cfg 注入 → UEFI FDT 生成（`virtualization/axvm/src/arch/loongarch64/boot/`）。ACPI 表生成器包含 RSDP/RSDT/FADT/MADT/SRAT/SPCR/MCFG + DSDT AML（PCI0 设备含 `_PRT` 路由表、COM1 设备、PCI 资源描述）+ QEMU loader 协议。x86 实现可以复用相同的 fw_cfg 传输层、QEMU loader 协议和 ACPI blob 容器，只需替换 arch-specific 的表内容。

### 4.2 当前缺口

**阻塞性缺失（固件无法完成平台发现）**

| # | 缺口 | 影响 |
|---|------|------|
| 1 | **x86 ACPI 表生成** | OVMF 在 DXE 阶段需要 ACPI 表来发现 LAPIC、IOAPIC、PCI 总线。当前 x86 仅有 SMP MP 表（`mptable.rs`），无 ACPI |
| 2 | **fw_cfg x86 接入** | fw_cfg 核心已实现（MMIO），`FwCfgDmaDevice::access` 只接受 `BusKind::Mmio`（`fw_cfg.rs:651-652`）；x86 UEFI 启动路径需要 PIO transport（`0x510`/`0x511`/`0x514`），尚未注册。OVMF 需要通过 fw_cfg 获取 ACPI 表、kernel/cmdline/initrd |
| 3 | **PCI 主桥 + ECAM** | 无 PCI bus 0 设备模型、无配置空间处理。Guest 无法枚举 PCI 设备 |
| 4 | **virtio-pci 传输层** | `EmulatedDeviceType::VirtioBlk/Net/Console` 已定义，但无 DeviceFactory 实现和 virtio-pci capability 结构 |

**高优先级缺失（中断与多核）**

| # | 缺口 | 影响 |
|---|------|------|
| 5 | **i8259 PIC 仿真** | DXE/BDS 阶段的中断路由需要 PIC 作为基础。当前无 PIC 设备 |
| 6 | **vLAPIC INIT/SIPI** | `process_init_sipi` 为 stub，多 vCPU 启动不可用 |
| 7 | **vLAPIC NMI 注入** | `inject_nmi` 为 `unimplemented!()` |
| 8 | **MSI/MSI-X 拦截** | Guest MSI 写（`0xFEE00000-0xFEEFFFFF`）会触发 `ApicRegOffset::from(0)` 的 panic |

**后续完善**

| # | 缺口 | 影响 |
|---|------|------|
| 9 | **HPET 虚拟化** | 当前仅穿透，需虚拟 HPET 暴露给 ACPI |
| 10 | **RTC/CMOS 仿真** | UEFI 固件在早期可能访问 CMOS 获取内存大小 |
| 11 | **OVMF_VARS 支持** | 当前只加载 OVMF_CODE.fd。VARS 存储需在后续阶段建模为 flash |

---

## 5. 架构分析

### 5.1 层次与边界

本方案新增的能力分布在以下层次：

```text
axdevice_base/     （已存在——OS-neutral 基元）
└── lib.rs         Device trait（name/resources/access）、DeviceAccess、BusAccess

axdevice/          （可复用设备模型——OS-neutral）
├── fw_cfg.rs      已存在：fw_cfg MMIO core + DMA + ACPI blob 容器
├── serial/        已存在：per-generation serial backend（machine-owned）
├── pci/           新增：PCI 主桥 + ECAM 配置空间（不依赖 OS）
├── virtio/        新增：virtio-pci transport + virtio-blk device
└── x86/pic.rs     新增：i8259 PIC（x86-only，PIO 设备）

axvm/arch/x86_64/  （x86 平台接入——依赖 axdevice；即 `virtualization/axvm/src/arch/x86_64/`）
├── boot/acpi.rs   新增：x86 ACPI 表生成器（含 DSDT AML）
├── boot/mod.rs    修改：UEFI layout 校验（load_uefi_from_configured_path）
├── mod.rs         修改：设备工厂注册
├── msi.rs         新增：MSI/MSI-X 地址拦截与翻译
└── irq.rs         修改：PIC IRQ 注入、中断风暴保护

axvm/host/         （已存在——host OS 适配边界）
└── arceos.rs      阶段 1 的固件相关 host 服务放入此处，不扩展 os/axvisor

os/axvisor/        （OS 层）
├── guest_console/mux/  已存在：guest COM1 输出汇聚（阶段 1 SEC 判定数据源）
└── config.rs            不改动：VM 创建顺序已固定（new → prepare_memory_layout
                         → load_images → prepare → register_vm）

x86_vlapic/        （已存在——vLAPIC/vIOAPIC/PIT/COM1 core）
└── vlapic.rs      修改：INIT/SIPI/NMI 补全
```

层次原则：

- `axdevice_base`/`axdevice/` 中的设备模型不依赖任何 OS 或平台类型，只通过 `DeviceFactory`、`DeviceBundle` 和 DMA grant 接入；trace 实现基于 `Device`/`BusAccess` 新 API；
- `axvm/arch/x86_64/`（`virtualization/axvm/src/arch/x86_64/`）负责 x86 平台特有的 ACPI 内容生成、固件 layout 校验和中断路由，是架构适配层；
- `axvm/host/` 承接 host OS 适配，`os/axvisor` 不反向依赖 axvm 内部细节；
- `x86_vlapic/` 保持 OS-neutral，只通过 `X86VlapicHostOps` trait 与宿主交互；
- 阶段 1 不新增 guest 串口截获代码：SEC 判定直接以 `os/axvisor/src/guest_console/mux/` 汇聚的 guest COM1 输出为准。

### 5.2 关键数据流

**固件加载与平台发现（阶段 1-2）**

```text
OVMF_CODE.fd (文件系统)
    → load_uefi_from_configured_path()（含 layout 校验：大小/GPA/reset vector）
    → 写入 guest 物理内存 [0xffc84000, 0x100000000)
    → vCPU reset vector 0xfffffff0 落在该窗口
    → OVMF SEC → PEI → DXE
    → （阶段 1 止步于此：SEC marker 经 guest COM1 → guest_console/mux 输出）
    → DXE 阶段通过 fw_cfg PIO (0x510/0x511/0x514) 获取 ACPI 表（阶段 2）
    → FwCfg 设备返回 ACPI blob（RSDP → XSDT → FADT/MADT/MCFG/DSDT；blob 由 build_x86_acpi() 生成并经 fw_cfg_platform_config 注入——注入发生在 vm setup/loader 阶段（对应 LoongArch `add_uefi_fw_cfg` 的注入模式，`virtualization/axvm/src/arch/loongarch64/boot/mod.rs:283`），而非顶层函数入口）
    → OVMF 解析 ACPI 表，发现 LAPIC/IOAPIC/PCI 总线
```

**PCI 枚举与 virtio 启动盘（阶段 3）**

```text
OVMF DXE → PCI 枚举
    → ECAM MMIO 读 (bus 0, device N, function 0, offset 0)
    → PciHostBridge 设备解码 ECAM 地址
    → 返回 VendorID/DeviceID（virtio-blk: 0x1AF4/0x1042）
    → OVMF 读 PCI capability 链
    → VirtioPciDevice 返回 Common/Notify/ISR/Device CFG 结构
    → OVMF 初始化 virtqueue，读 ESP，加载 EFI 应用
```

**中断投递路径（阶段 4）**

```text
virtio-blk 完成 IO 请求
    → (MSI 路径) Guest 写 MSI address/data 到 0xFEE00000-0xFEEFFFFF
    → VM exit (EPT violation 或 APIC-access)
    → msi.rs 解析 address (dest_id) 和 data (vector, delivery_mode)
    → vLAPIC accept_interrupt(vector, edge)
    → vCPU 注入中断
    → (INTx 路径) virtio-pci 拉低 INTx#
    → vIOAPIC assert_gsi(gsi) → IoApicInterrupt
    → vLAPIC LINT0/LINT1 或直接注入
    → vCPU 注入中断
```

### 5.3 并发与锁

本方案引入的主要并发状态：

| 状态 | 所有者 | 共享方式 | 锁策略 |
|------|--------|---------|--------|
| PCI ECAM 配置空间 | `PciHostBridge`（VM 级） | 多 vCPU 可能同时访问不同 bus/device | `SpinNoIrq`，每个 access 持锁时间仅限于单次 MMIO 读写 |
| virtio-blk virtqueue | `VirtioBlk`（VM 级） | vCPU 提交请求，设备侧处理完成 | virtqueue 的 avail/used ring 通过内存序同步；设备侧不持锁等待 IO |
| i8259 PIC 状态（IMR/IRR/ISR） | `Pic`（VM 级） | vCPU PIO 访问 | `SpinNoIrq`。PIO handler 在 vCPU VM-exit 上下文中执行，不能睡眠；PIC handler 不回调其他 Device 锁，避免重入 |
| MSI 路由表 | `MsiRouter`（VM 级） | vCPU MSI 写（VM-exit 上下文，高频）+ 设备中断路由注册（低频） | `SpinNoIrq` 读写锁。MSI 写路径在 VM-exit 上下文中运行，必须使用非睡眠锁；读侧（MSI 拦截）高频，写侧（PCI 设备 MSI capability 初始化）低频 |
| vLAPIC SIPI pending | per-vCPU | 发起 vCPU 写 ICR → 目标 vCPU 读 | 通过 `VmInterruptSender` 消息队列，无共享锁 |

**锁顺序**（从外到内）：

```text
DeviceRuntime::dispatch()
    → 可能持 Device 内部锁（PciHostBridge、Pic、VirtioBlk）
    → 中断注入 → vLAPIC accept_interrupt
    → VmInterruptSender → vCPU pending queue
    → 不回调其他 Device 锁（避免 ABBA）
```

关键约束：Device handler 内部持锁时不应触发其他 Device 的 bus access 或 interrupt injection（避免重入和死锁）。virtio-blk 的 IO 完成后通过异步 callback 注入中断，不在 virtqueue handler 内部直接调用 `VmInterruptSender`。

### 5.4 错误路径

| 场景 | 行为 |
|------|------|
| OVMF 访问未注册的 PIO/MMIO 地址 | `DeviceRuntime` 返回 `DeviceError::NotFound`，不伪造返回值。VM exit 记录到 trace |
| fw_cfg 收到未支持的 selector | `FwCfg` 返回空 entry（`FwCfgEntry::Bytes(&[])`），不 panic |
| PCI ECAM 读不存在的 device/function | 返回 `0xffffffff`，PCI 标准语义 |
| virtio-blk 读超出磁盘镜像边界 | 返回 virtio error status（`VIRTIO_BLK_S_IOERR`），不 panic |
| guest 写 MSI address 但未命中任何已注册路由 | Trace 记录，静默丢弃（与物理硬件行为一致） |
| 固件访问未实现的 ACPI 方法（DSDT AML） | AML interpreter 返回 0，不阻塞其他方法执行 |
| virtio DMA 传输中 guest reset | DMA grant 标记为 invalid，后续访问被拒绝 |

---

## 6. 关键设计决策

1. **LoongArch `boot/acpi.rs` 作为 x86 的模板，但独立实现**。两架构共享 fw_cfg 传输层、QEMU loader 协议和 ACPI blob 容器（已存在于 `axdevice`），但 ACPI 表内容为 arch-specific：x86 需要 LAPIC/IOAPIC/Override entry（MADT）、IO 端口资源描述（DSDT）、PIC 相关声明，LoongArch 需要 EIOINTC/PCH-PIC/MSI entry 和 FDT 生成。共享传输层、独立表内容。

2. **fw_cfg PIO transport 需扩展核心，不能靠薄 adapter**。dev 中 `FwCfgDmaDevice::access` 仅接受 `BusKind::Mmio`（非 MMIO 返回 `OutOfRange`），`FwCfgBuildConfig.base` 是 `GuestPhysAddr`（MMIO 语义），x86 不能仅注册一个 PIO 到 MMIO 的薄 adapter 完事。两种方案：**(a)** 在 `virtualization/axdevice/src/fw_cfg.rs` 核心内扩展 Port transport——selector/data/DMA 偏移（`0x510`/`0x511`/`0x514`）映射到 PIO 端口，注册为 `BusKind::Port` 设备，x86 私有配置提供 PIO 端口基址（非 `GuestPhysAddr` 语义），LoongArch 继续走 MMIO（`0x1e02_0000`，`virtualization/axvm/src/machine.rs:471`）；**(b)** 新增 Port 变体设备（`FwCfgPioDevice`），复用核心的 selector/DMA/文件目录逻辑。方案 (a) 更收敛，两架构共用同一实现，推荐；方案 (b) 不触碰 LoongArch 已稳定的 MMIO 设备，改动面更小，作为备选。LoongArch 的 MMIO 基址与 PIO 端口语义不同，不可照搬。

3. **PCI 和 virtio 通过统一设备框架接入**。PCI 主桥、virtio-pci transport、virtio-blk 各为一个 `DeviceFactory`，通过 `DeviceBundle` 声明资源（MMIO/PIO range、DMA grant、typed service），不新增旁路或绕过 `DeviceRuntime` 的访问路径。

4. **中断状态按 VM 所有，不引入全局状态**。i8259 PIC、MSI/MSI-X 路由均通过 `X86InterruptDomainKey` 和 `VmInterruptSender` 注入，不恢复旧式按 VM ID 查全局 forwarding state 的轮询模式。

5. **Guest COM1 作为阶段判定的可信数据源**。成功判定始终来自目标 VM 的 Guest COM1 输出（VM-qualified marker），不依赖宿主日志混合匹配。每个 OVMF 阶段 marker 的首次出现即为该阶段的进入证据。阶段 1 中 COM1 已由 machine profile 提供，直接以测试 success_regex 匹配 guest 输出，不新增串口截获代码。阶段 1 实际落地的 success_regex 为 `(?s)VCpu\[0\] running\.\.\..*SecCoreStartupWithStack`（见 `plan/uefi/pr1.md` 提交 6 `245e038c7`）：`VCpu[0] running...` 仅嵌套 VM 输出，避免匹配 QEMU 层为引导宿主注入的 pflash OVMF 所输出的相同 SEC 行（真实 QEMU 验证中裸 regex 847ms 即误匹配宿主层）。阶段 2 的 `[BdsDxe]` 判定沿用同一前缀锚定。

6. **每个阶段不能"跳跃"**。如果 SEC marker 出现但 PEI marker 缺失，不能说"PEI 大概过了"。Trace 中的第一个未支持访问是下一阶段的优先级输入，不伪造返回值来模拟通过。

7. **模块边界按 §5.1 分层**。`axdevice/pci/ecam.rs` 隔离 ECAM 地址解码（纯数学映射），`axdevice/pci/mod.rs` 管理 bus/device 拓扑和 BAR 分配，`axdevice/virtio/` 实现 transport 和 device type 两层。新增文件均满足 guideline §8 的抽象条件：隔离平台差异（x86 PCI 寄存器布局）、存在确定的多实现边界（virtio-blk vs virtio-net vs 未来的 virtio-console）、隔离外部依赖（disk 后端）。

---

## 7. 实现路径

### 阶段 1：诊断基础 — 固件入口验证（本阶段实现范围）

**目标**：建立可复现、可归因的诊断链，让每次 UEFI 启动都能确定 OVMF 走到了哪一步。

**为什么是第一步**：后续 ACPI、PCI、virtio 等设备实现时，需要精确知道"OVMF 接下来访问了什么"，而不是靠猜。

**范围界定**：本阶段**不实现**完整 UEFI 启动（fw_cfg/ACPI/PCI/virtio 属阶段 2-5），只建立"固定 OVMF 资产 + 固件装载校验 + SEC 到达判定"的最小闭环。COM1 已由 machine profile 提供，SEC 判定直接以 guest COM1 输出为准。

**工作项**：

1. **固定 OVMF profile**：选定 `edk2-stable202605`、`OvmfPkg/OvmfPkgX64.dsc`、DEBUG、GCC、4MB FD、`DEBUG_ON_SERIAL_PORT`、`BUILD_SHELL=TRUE`。记录 CODE 布局（`0xffc84000-0xffffffff`）和 SHA-256，不扫描发行版 `/usr/share/OVMF`。

2. **OVMF 资产校验（axbuild）**：新增私有类型 `OvmfManifest`/`VerifiedOvmfBundle` 等（serde/TOML + `deny_unknown_fields`）。校验 CODE/VARS 大小、SHA-256、拼接关系、profile、GPA。未验证的本地固件必须显式 opt-in 并打印哈希（用 `FirmwareSource` 枚举区分固定资产与未验证固件，避免布尔参数扩散）。替代旧 `ovmf-profile.sh` 脚本——脚本不恢复，校验职责迁入 Rust 编排层。

3. **x86 loader layout 校验（axvm）**：在 `virtualization/axvm/src/arch/x86_64/boot/mod.rs:259` 的 `load_uefi_from_configured_path`（及内存镜像路径）上增加小型私有校验：CODE 大小 `0x37c000`、GPA `0xffc84000`、reset vector `0xfffffff0`；multiboot/direct boot 路径保持不变。成功日志打印 profile、路径、大小、GPA、reset vector。

4. **设备访问 trace（axdevice）**：在 `DeviceRuntime` 的统一访问路径（`virtualization/axdevice/src/device.rs:1149` `dispatch()`，含 `virtualization/axdevice/src/device.rs:956` `handle_mmio_write_with_memory`）上记录 `device=<name> bus=PIO|MMIO direction=read|write addr=<addr> width=<width> value=<value> result=<ok|error>`。对未注册资源返回 `DeviceError::NotFound`，不伪造返回值。按 `axdevice_base` 的 `Device`/`BusAccess` 新 API 实现（`Device::name/resources/access`，`BusAccess` 含 `kind/addr/width/is_read/data`），不搬用旧 P0 在 device.rs 的 trace 实现。

5. **ovmf-entry 测试用例**：新增 `test-suit/axvisor/uefi/ovmf-entry/`，接入现有测试编排（`scripts/axbuild/src/axvisor/test/` 的 discovery/qemu 机制）。VMX/SVM 使用同一个 backend-neutral build 配置，分别用对应 QEMU/KVM 参数。success_regex 直接匹配 guest COM1 输出 `SecCoreStartupWithStack\(`。初始为 non-gating diagnostic。

**退出条件**：

| 条件 | 可观察证据 |
|------|-----------|
| 固定 OVMF 在 VMX/SVM 后端进入 SEC | Guest COM1 输出 `SecCoreStartupWithStack(`，测试 success_regex 命中 |
| Trace 记录首个未支持的平台访问 | CI 日志中可定位到具体的 `device= bus= addr=` 行 |
| 固件装载日志完整 | 日志含 profile、路径、SHA-256、大小、GPA、reset vector |
| ovmf-entry 测试可运行 | `cargo xtask axvisor test qemu --arch x86_64 --test-group uefi --test-case ovmf-entry-svm`（及 `--test-case ovmf-entry-vmx`）执行并报告结果 |

### 阶段 2：平台发现 — fw_cfg 与 ACPI

**目标**：OVMF 在 DXE 阶段发现平台硬件拓扑，推进到 BDS。

**工作项**：

1. **x86 fw_cfg PIO 接入**：q35 标准 fw_cfg 在 PIO `0x510`（selector）、`0x511`（data word）、`0x514`（DMA address，8 字节 big-endian）。dev 的 `FwCfgDmaDevice::access` 只接受 `BusKind::Mmio`（非 MMIO 返回 `OutOfRange`），`FwCfgBuildConfig.base` 为 MMIO 语义的 `GuestPhysAddr`，因此按 §6 决策 2 扩展 `FwCfg` 核心支持 Port transport（selector/data/DMA 偏移映射到 PIO `0x510`/`0x511`/`0x514`，注册为 `BusKind::Port` 设备，x86 私有配置提供 PIO 端口基址），或新增 Port 变体设备；LoongArch 保持 MMIO（`0x1e02_0000`，`virtualization/axvm/src/machine.rs:471`）不变，不可照搬其基址语义。

2. **x86 ACPI 表生成**：新建 `virtualization/axvm/src/arch/x86_64/boot/acpi.rs`，以 LoongArch `boot/acpi.rs` 为模板。参照其入口 `build()`（`acpi.rs:133-141`，返回 `FwCfgAcpiBlobs`，经 `fw_cfg_platform_config` 注入 `FwCfgPlatformConfig`，注入发生在 `add_uefi_fw_cfg` 的 vm setup/loader 阶段，`boot/mod.rs:283`，顶层函数在 `boot/mod.rs:183`），x86 写 `build_x86_acpi()` -> `FwCfgAcpiBlobs`（arch private）。生成表集合：

   | 表 | 关键字段 |
   |---|---------|
   | RSDP | 含 XSDT 指针（64 位地址），checksum |
   | XSDT | 指向 FADT/MADT/MCFG/DSDT 的指针数组 |
   | FADT | SMI_CMD=0, ACPI_ENABLE/DISABLE=0, SCI_INT=9, PM1a_CNT_BLK=0, x86 固件控制字段 |
   | MADT | 每 vCPU 一个 LAPIC entry（ACPI ID=vcpu_id, APIC ID=vcpu_id, flags=1 BSP/0 AP）；一个 IOAPIC entry（addr=0xfec00000, GSI base=0）；一个 IRQ0 override（source=0, GSI=2, flags=0）；按 q35 真实行为补 IRQ5/9/10/11 override（source=i, GSI=i, flags=0xd，与 QEMU `pci_irq_mask = (1<<5)|(1<<9)|(1<<10)|(1<<11)` 一致，见 §2.2 QEMU 源与 2026-08-05 复核）；FADT 的 SCI_INT=9 需与 IRQ9 的 level-active-low override 语义保持一致（IRQ9 override flags=0xd 即 level-active-high，为 SCI 保留） |
   | MCFG | ECAM base=`0x20000000`, segment=0, bus start=0, bus end=255 |
   | DSDT | PCI0 设备（_HID PNP0A08, _CRS, _PRT, _CBA）+ COM1 设备（_HID PNP0501, _CRS） |

   所有表通过 QEMU table-loader 协议注册到 fw_cfg（`etc/acpi/tables`、`etc/acpi/rsdp`、`etc/table-loader`），checksum 正确计算。

3. **DSDT AML**：参考 `build_loongarch_dsdt_aml` 的 `aml_device("PCI0", ...)` 模式（`acpi.rs:502-519`，`aml_device("PCI0", ...)` 在 `acpi.rs:508`），x86 DSDT 声明 PCI0（`_HID` PNP0A08、`_CRS`、`_PRT`、`_CBA`）与 COM1（`_HID` PNP0501、`_CRS`）。PCI0 INTx 路由表按 q35 标准：PIRQA→GSI16, PIRQB→GSI17, PIRQC→GSI18, PIRQD→GSI19。PCI0 `_CRS` 含 bus number resource（0-255）、IO resource（`0x6000-0xffff`）、32-bit MMIO resource（`0x80000000-0xbfffffff`）。

**退出条件**：

| 条件 | 可观察证据 |
|------|-----------|
| OVMF DXE 阶段加载 ACPI 表 | Trace 中可见 `fw_cfg` 设备和 `FW_CFG_ACPI_TABLES`/`FW_CFG_ACPI_RSDP` selector 的 DMA 读操作，且返回的 blob 不含全零填充 |
| OVMF 推进到 BDS | Guest COM1 输出裸 `[BdsDxe]`（`-serial stdio` 原始字节流，无 `VM[1] guest COM1:` 前缀——该格式是旧 P0 observer 的输出，阶段 1 已确认不可用）。测试 success_regex 沿用阶段 1 实际落地的嵌套 VM 锚定思路（`(?s)VCpu\[0\] running\.\.\..*\[BdsDxe\]`，前缀锚定同阶段 1 的 `(?s)VCpu\[0\] running\.\.\..*SecCoreStartupWithStack`：`VCpu[0] running...` 仅嵌套 VM 输出，避免匹配 QEMU 层宿主 OVMF 的同名输出，见 §6 决策 5 与阶段 1 章节） |
| ACPI 表的 MCFG 和 MADT 被解析 | OVMF 日志出现 `MCFG base=0x20000000` 或 PCI 枚举相关消息（阶段 1 设备访问 trace 扩展）；MADT 含 IRQ0→GSI2 与 IRQ5/9/10/11 override（flags=0xd）时 OVMF/guest 中断语义（SCI=9 等）才与 q35 一致 |

### 阶段 3：PCI 与启动盘 — virtio-pci

**目标**：OVMF 通过 PCI 枚举发现 virtio-blk，读取 ESP 并进入 UEFI Shell。

**工作项**：

1. **PCI 主桥（`virtualization/axdevice/src/pci/`）**：ECAM 地址解码 `addr = ecam_base + (bus << 20) + (device << 15) + (function << 12) + offset`。Vendor ID/Device ID 查表返回，不存在设备返回 `0xffff`。Command/Status/BAR/Capability Pointer 等基本寄存器读写。

2. **virtio-pci transport（`virtualization/axdevice/src/virtio/pci.rs`）**：遵循 VirtIO 1.2 §4.1。PCI capability 链中暴露 Common CFG、Notify CFG、ISR CFG、Device CFG 四个结构。Queue Select/Size/Enable/MSIX Vector 等寄存器映射到对应 PCI BAR。

3. **virtio-blk（`virtualization/axdevice/src/virtio/blk.rs`）**：一个 virtqueue（请求队列），capacity/block_size 等 device config。读取 host 磁盘镜像，guest memory 通过显式 DMA grant 访问。

4. **PCI INTx**：利用已有 vIOAPIC，按 `_PRT` 声明将 INTx 路由到 GSI。

**退出条件**：

| 条件 | 可观察证据 |
|------|-----------|
| OVMF 枚举到 virtio-blk | Guest COM1 或 trace 中出现 `VirtioBlk` 或 `0x1AF4/0x1042` 的 PCI config 读 |
| OVMF 能读 ESP 文件 | Guest COM1 出现 UEFI Shell 提示符或 EFI 应用加载消息 |
| Linux EFI 启动到 guest 自身标记 | Linux guest 输出 kernel banner（不再是宿主侧 `VM[1] boot success`） |

### 阶段 4：中断完善

**目标**：完整中断链路，包括 PIC、MSI/MSI-X、SMP。

**工作项**：

1. **i8259 PIC（`virtualization/axdevice/src/x86/pic.rs`）**：双片级联（master `0x20-0x21`、slave `0xa0-0xa1`），ICW/OCW 初始化、IRQ 断言、优先级、masking、EOI。PIC slave 的 INT 输出连接到 PIC master 的 IRQ2。通过 `DeviceFactory` 注册为 PIO 设备。

2. **MSI/MSI-X 拦截（`virtualization/axvm/src/arch/x86_64/msi.rs`）**：拦截 `0xFEE00000-0xFEEFFFFF` MMIO 写。解析 MSI address（`0xFEE00000 | (dest_id << 12)`）和 data（vector + delivery mode + trigger mode），转换为 vLAPIC 中断注入。PCI MSI-X 需要 MSI-X capability 和 BAR 在配置空间中声明。

3. **vLAPIC INIT/SIPI 补全**：`process_init_sipi` 通过 `VmInterruptSender` 发送 SIPI vector 到目标 vCPU。

4. **vLAPIC NMI 补全**：`inject_nmi` 将 NMI 注入目标 vCPU。

5. **中断风暴保护**：level-triggered 中断 EOI 后重新检查源状态，连续 3 次 re-assert 后插入退避延迟。

**退出条件**：

| 条件 | 可观察证据 |
|------|-----------|
| virtio-blk INTx/MSI 中断可投递 | virtio 请求完成，中断向量被 vCPU 接受（trace 中可见 `inject_interrupt`） |
| SMP 启动（≥2 vCPU） | Guest COM1 显示 BSP 和 AP 均已进入 OS |
| 重复 start/stop/reset 无状态泄漏 | 连续 5 次 VM reset 后，中断投递延迟不增长 |
| MSI 写不再 panic | 阶段 2 中先加 offset 0 的显式处理 |

### 阶段 5：测试与跨架构收敛

**目标**：完整测试覆盖，x86 与 LoongArch 公共部分抽取。

**工作项**：

1. **Linux EFI stub 启动测试**：新增 x86_64 Linux guest UEFI 启动测试，success_regex 为 guest initramfs shell prompt。

2. **HPET 虚拟化**：新建 `virtualization/axdevice/src/x86/hpet.rs`，MMIO `0xfed00000`，至少一个 timer comparator。通过 `DeviceFactory` 注册。ACPI DSDT 中声明。

3. **RTC/CMOS 仿真**：最小 CMOS RTC（PIO `0x70-0x71`），支持内存大小扩展 CMOS 寄存器。

4. **公共部分抽取**：识别 `fw_cfg` 管线、ACPI blob 组装、QEMU loader 协议的两架构共用部分，抽取到 `axdevice/` 公共层。LoongArch 保留 FDT 路径。

5. **CI 门禁**：ovmf-entry 和 Linux UEFI 用例从 non-gating 升级为 gating，在 `.github/workflows/ci.yml` 中新增 x86 UEFI gating job（现有 `ci.yml:705-743` 仅为 axloader 的 UEFI smoke，下载发行版 OVMF）。

**退出条件**：Linux 启动到 initramfs；HPET/RTC UEFI 可用；x86 UEFI CI gating。

---

## 8. 新增公共接口

每个阶段新增的公共接口汇总：

| 阶段 | 新增公共类型 / 接口 | 所属 crate | 说明 |
|------|-------------------|-----------|------|
| 2 | `FwCfg` Port transport（扩展核心；或新增 Port 变体设备） | `axdevice` | fw_cfg Port transport 扩展，实现 `Device` trait，注册为 `BusKind::Port` 设备（§6 决策 2，方案 (a)） |
| 2 | `build_x86_acpi()` free function -> `FwCfgAcpiBlobs` | `axvm` (arch private) | x86 ACPI 表生成入口，参照 LoongArch `acpi::build`（`virtualization/axvm/src/arch/loongarch64/boot/acpi.rs:133-141`），经 `fw_cfg_platform_config` 注入 `FwCfgPlatformConfig` |
| 3 | `PciHostBridge` struct + `Device` impl | `axdevice` | PCI 主桥设备 |
| 3 | `VirtioPciDevice` struct + `Device` impl | `axdevice` | virtio-pci transport |
| 3 | `VirtioBlk` struct + `Device` impl | `axdevice` | virtio-blk device type |
| 4 | `Pic` struct + `Device` impl | `axdevice` | i8259 PIC 设备 |
| 4 | `MsiRouter` struct（arch private） | `axvm` | MSI/MSI-X 地址拦截与路由 |

> 阶段 1 不新增公共接口：axbuild 校验类型（`OvmfManifest`/`VerifiedOvmfBundle`/`FirmwareSource`）为私有实现，x86 loader 的 layout 校验为 `virtualization/axvm/src/arch/x86_64/boot/mod.rs` 内的私有函数，trace 仅改 `axdevice` 内部日志路径。

**接口设计原则**：

- 所有新设备通过现有 `DeviceFactory` trait 注册，不新增 trait 方法；
- `EmulatedDeviceType` 枚举可能新增 `X86Pic` variant，但现有 variant 不修改；
- TOML VM 配置不新增公开字段，UEFI guest 配置沿用现有 `boot_protocol = "uefi"` + `uefi_firmware_path` + `bios_load_addr`；
- 错误类型：`axdevice` 中的 Device 返回 `DeviceError`（已有）；`virtualization/axvm/src/arch/x86_64/` 中的平台代码返回 `AxVmError`（已有）。

---

## 9. 验证与测试

### 9.1 测试矩阵

| 功能声明 | 验证层级 | 命令/用例 | 可观察通过条件 | 阶段 |
|----------|---------|-----------|---------------|------|
| OVMF 固定 profile 进入 SEC | QEMU VMX/SVM | `cargo xtask axvisor test qemu --arch x86_64 --test-group uefi --test-case ovmf-entry-vmx`（同理 SVM） | Guest COM1 输出 `SecCoreStartupWithStack(`，success_regex 命中 | 1 |
| OVMF 资产校验（manifest/哈希/大小/拼接） | host 单元测试 | `cargo xtask clippy --package axbuild`（含 `cargo test -p axbuild`） | 错误哈希/大小/base/未知字段/重复字段均被拒绝；未验证固件缺 opt-in 时拒绝 | 1 |
| x86 loader layout 校验 | host 单元测试 | axvm 单测 | 错误 CODE 大小/GPA/entry point 被拒绝（真实 API 调用，非源码文本断言） | 1 |
| 设备 trace 记录未支持访问 | 同上 | 同上 | CI 日志含 `device= bus= addr=` trace 行，首个 `result=NotFound` 后至少有一个后续 trace 行 | 1 |
| OVMF 通过 fw_cfg 获取 ACPI 表 | QEMU VMX | ovmf-entry 测试（扩展） | trace 含 `fw_cfg DMA read selector=0x23`（ACPI_TABLES），返回非零长度 | 2 |
| OVMF 进入 BDS | QEMU VMX | ovmf-entry 测试 | Guest COM1 输出 `[BdsDxe]` | 2 |
| OVMF PCI 枚举发现 virtio-blk | QEMU VMX | Linux UEFI 测试 | trace 含 `pci` device 的 config read，VendorID=0x1AF4 | 3 |
| Linux EFI stub 启动 | QEMU VMX | `cargo xtask axvisor test qemu --arch x86_64 --test-group uefi --test-case linux-uefi` | Guest COM1 输出 Linux kernel banner + initramfs shell prompt | 3/5 |
| virtio-blk MSI 中断投递 | QEMU VMX | Linux UEFI + trace | `inject_interrupt` 日志含 virtio-blk MSI vector | 4 |
| SMP UEFI 启动 | QEMU VMX | 新增 `linux-uefi-smp2` 用例 | Linux 启动，且至少 2 个 vCPU 的 COM1 有活跃输出 | 4 |
| 重复 VM reset 无中断泄漏 | QEMU VMX | 新增压力测试：连续 reset 5 次 | 每次 reset 后的首次 MSI 延迟与前次无统计显著差异 | 4 |
| 回归：BIOS/multiboot 不受影响 | QEMU VMX | 现有 direct VMX/SVM smoke | 与 dev 基线一致 | 1-5 |

### 9.2 测试失败模式

每个测试必须在对应行为损坏时确实失败：

- ovmf-entry：若 OVMF CODE 映射错误或 reset vector 未命中，Guest COM1 将无 SEC marker 输出，测试超时；
- ACPI 表缺失：OVMF DXE 阶段无法完成平台发现，不会输出 `[BdsDxe]` 或输出后立即挂起，测试超时；
- virtio-blk 缺失：OVMF BDS 无法找到 boot option，UEFI Shell 无输出或 Linux EFI 应用不启动；
- 中断缺失：virtio-blk IO 请求完成后 guest 永远收不到中断，测试超时。

---

## 10. 阶段依赖关系

```mermaid
graph TD
    P1[阶段1: 诊断基础（本次实现）] --> P2[阶段2: fw_cfg + ACPI]
    P2 --> P3[阶段3: PCI + virtio-blk]
    P3 --> P4[阶段4: 中断完善]
    P2 --> P4
    P4 --> P5[阶段5: 测试与收敛]
    P3 --> P5
```

**数据依赖**：

- 阶段 2 依赖阶段 1 的 trace 输出确定 fw_cfg/ACPI 相关访问（阶段 1 已交付 trace 基础设施）；
- 阶段 3 依赖阶段 2 的 MCFG/DSDT PCI0 声明（ECAM base、_PRT）；
- 阶段 4 MSI/MSI-X 依赖阶段 3 的 virtio-pci capability 结构和 PCI BAR 布局；
- 阶段 4 PIC 不依赖阶段 3，可在阶段 2 后并行开始；
- 阶段 5 的公共抽取依赖阶段 3-4 的接口稳定。

---

## 11. 风险

| 风险 | 可能性 | 影响 | 缓解 |
|------|:---:|------|------|
| 固定 OVMF 在 AxVisor 上无法进入 SEC | 低 | 阻塞阶段 1 | 先用标准 QEMU q35 验证同一固件可完成 SEC→BDS，排除固件自身问题；若 QEMU 通过而 AxVisor 不通过，是 vCPU/NPT 问题 |
| OVMF 资产（tgosimages bundle）不可达 | 低 | 阻塞阶段 1 的测试 | 校验与下载分离：资产校验逻辑先行落地；下载失败时明确报错，不静默 fallback 到发行版 OVMF |
| test-suit 新分组（`test-suit/axvisor/uefi/`）未被 discovery 发现 | 中 | 阻塞阶段 1 用例 | 先跑通一个最小用例确认发现机制（`scripts/axbuild/src/axvisor/test/`），再补 VMX/SVM 参数 |
| MSI 写触发 panic（`ApicRegOffset::from(0)` 无对应 arm） | 高 | 阻塞阶段 3+ | 阶段 2 先加 offset 0 显式处理（返回 0 + trace），避免 PCI 枚举时的意外 panic |
| vLAPIC SIPI 跨 vCPU 同步问题 | 中 | 阻塞阶段 4 SMP | 利用已有 `VmInterruptSender` 和 vCPU 消息队列，避免引入新同步原语；单 vCPU 场景不受影响 |
| OVMF 依赖 VARS store 查找 boot option | 中 | 可能阻塞阶段 3 UEFI Shell 入口 | OVMF 在 shell 编译开关开启时，即使无 VARS 也会进入 shell；若标准构建不进入 shell，可通过 fw_cfg 传递 `bootorder` 文件引导特定条目 |
| LoongArch DSDT AML 与 x86 q35 差异大 | 低 | 阶段 2 需独立验证 AML | 使用 `iasl` 反编译参考 QEMU 生成的 DSDT，验证 AML 结构 |
| ACPI 表 checksum 错误 | 中 | OVMF 在 DXE 阶段拒绝加载表 | 阶段 2 编写 `iasl` 验证脚本，CI 中自动校验生成的表 |

---

## 12. 上线、回滚与可观测性

- **启用**：通过 VM 配置中 `boot_protocol = "uefi"` opt-in，不影响默认 BIOS/multiboot 路径；
- **禁用**：将 `boot_protocol` 改回 `"multiboot"` 或 `"direct"`，无需重新编译 AxVisor；
- **确认功能在使用**：Guest COM1 输出 SEC marker、loader 日志中的 `boot_protocol=uefi` 行与固件装载信息（profile、路径、SHA-256、大小、GPA、reset vector）；
- **回滚**：UEFI 路径完全 opt-in，回滚只影响特定 VM 配置，不影响其他 VM 或 AxVisor 自身运行；
- **可观测信号**：Guest COM1 阶段 marker（SEC/PEI/DXE/BDS/OS）、device access trace、fw_cfg DMA log、MSI 拦截 log；
- **回滚触发条件**：OVMF 无法进入 SEC、PCI 枚举失败导致 BDS 超时、中断风暴；
- **外部固件依赖不可用**：若 tgosimages 上的 OVMF bundle 不可达，xtask/axbuild 报告明确错误，不静默 fallback 到发行版 OVMF。

---

## 13. 变更落点总览

| 阶段 | 新增 | 修改 |
|------|------|------|
| 1 | `test-suit/axvisor/uefi/ovmf-entry/`（build + qemu 配置）；axbuild 私有校验类型（`OvmfManifest`/`VerifiedOvmfBundle`/`FirmwareSource`） | `virtualization/axvm/src/arch/x86_64/boot/mod.rs`（layout 校验挂点）、`virtualization/axdevice/src/device.rs`（trace 按 `axdevice_base` 新 API）、`scripts/axbuild/src/axvisor/test/`（用例接入）、`.github/workflows/ci.yml`（non-gating job） |
| 2 | `virtualization/axvm/src/arch/x86_64/boot/acpi.rs`、`virtualization/axvm/src/arch/x86_64/boot/acpi/dsdt.rs` | `virtualization/axvm/src/arch/x86_64/boot/mod.rs`、`virtualization/axvm/src/arch/x86_64/mod.rs`、`virtualization/axdevice/src/fw_cfg.rs`（Port transport 扩展） |
| 3 | `virtualization/axdevice/src/pci/mod.rs`、`virtualization/axdevice/src/pci/ecam.rs`、`virtualization/axdevice/src/virtio/pci.rs`、`virtualization/axdevice/src/virtio/blk.rs` | `virtualization/axvm/src/arch/x86_64/boot/acpi/`（DSDT 更新）、VM 配置 toml |
| 4 | `virtualization/axdevice/src/x86/pic.rs`、`virtualization/axvm/src/arch/x86_64/msi.rs` | `virtualization/x86_vlapic/src/vlapic.rs`、`virtualization/x86_vcpu/`（EPT MSI range）、`virtualization/axvm/src/arch/x86_64/irq.rs` |
| 5 | Linux EFI 测试、HPET/RTC 设备 | CI workflow、`virtualization/axdevice/src/x86/hpet.rs`、公共层抽取 |
