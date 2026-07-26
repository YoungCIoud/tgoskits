## 当前状态

现在 Axvisor 里的 x86_64 客户机只能以传统 BIOS 方式启动。具体来说：

- 配置中使用 entry_point 和 bios_load_addr 将启动代码固定加载到 0x8000 这样的低地址位置。
- 启动依赖我们自己的 rvm-bios.bin 或 axvm-bios.bin 固件，而不是行业标准的 OVMF/UEFI 固件。
- 镜像加载器只认识“按物理地址直接放置内核、BIOS、内存盘、设备树”这一种方式，缺少对 OVMF_CODE（UEFI 代码）、OVMF_VARS（UEFI 变量存储）、pflash、fw_cfg 以及 UEFI 变量区域等语义的支持。
- vCPU 初始状态也被设定为实模式 / trampoline 风格，依靠“不受限客户机”能力直接在低地址执行启动代码。
- 设备模型非常单薄：x86 的 emu_devices 列为空，IOAPIC/LAPIC/HPET 基本直接穿透给客户机，但客户机一侧看不到 ACPI 表、PCI 根桥、virtio-pci 设备，也缺少虚拟 IOAPIC 和 MSI 中断路由。

一句话总结：当前缺的不是某个小功能，而是一整套能让标准 UEFI 客户机找到硬件并启动的“PC 平台骨架”。

## 目标

让 Axvisor 能够启动标准的 x86_64 UEFI 客户机：

- 使用 OVMF / EDK2 作为客户机固件，从固件入口自然进入 UEFI 环境。
- 通过 ACPI 表、PCI 总线、virtio 设备等标准 PC 发现路径，逐步启动 Linux EFI stub、UEFI 应用或其他 UEFI 感知的操作系统。

最小可行目标包括：

- OVMF_CODE 与 OVMF_VARS 的加载和地址映射
- fw_cfg（QEMU 风格的固件配置接口）
- 基本 ACPI 表（最少要包含 RSDP、XSDT/RSDT、FADT、MADT、MCFG）
- 一个最小可用的 PCI 主桥和配置空间（ECAM 或 PIO 方式）
- virtio-block 设备（通过 virtio-pci 暴露），能用来枚举磁盘并启动 OS
- 后续接入 virtio-net 和 virtio-console
- 基本中断链路：vLAPIC、vIOAPIC、i8259、INTx，后续再完善 MSI/MSI-X

## 技术实现路线

1. 扩展启动配置模型 将启动方式从单一的 bios_path / bios_load_addr 扩展为可配置的 boot = "trampoline"（传统 BIOS 方式）或 boot = "uefi"。配置项需要显式描述 OVMF 的代码、变量存储、pflash 属性，以及固件在客户机物理地址空间中的位置。这部分改动会落在 axvmconfig、axvm::config 以及镜像加载器上。
2. 补齐 UEFI 固件所需的“平台设备” 先实现一个 QEMU 兼容的 fw_cfg 设备，用以向固件传递启动信息。接着生成并暴露最小 ACPI 表集合：RSDP、XSDT/RSDT、FADT、MADT、MCFG。之后根据需要逐步加入 HPET、SPCR 以及 DSDT/SSDT 等更完整的表。
3. 补齐 PC 风格的客户机设备模型 实现最小 PCI 主桥和配置空间，优先支持 virtio-pci block 设备，让 UEFI 固件能发现磁盘并从中启动。随后再加入 virtio-net 和 virtio-console 支持，构成一套基本可用的输入输出环境。
4. 完善 x86 中断链路 短期：利用 vIOAPIC + INTx 让 virtio-pci 的基本中断跑通。 中期：完善 vLAPIC 的 EOI、IPI、timer 以及 MSI/MSI-X 支持。目前 x86_vcpu 中已经有 pending event 和 vLAPIC 雏形，但还未能串成完整的 IRQ 数据通路，需要把它们连起来并填补缺失逻辑。
5. 建立验证闭环 新建一个专门的 x86_64 UEFI 客户机配置，并编写对应的 QEMU 冒烟测试。验证路径从 OVMF 的 UEFI Shell 开始，逐步到通过 Linux EFI stub 和 virtio-block 启动完整内核。目前 qemu-x86_64.toml 中仍标记为 uefi = false，我们需要将其升级为真正的 UEFI 启动测试。


## 另一个目标描述： Axvisor Guest UEFI 支持完善

把 guest UEFI 从“局部能跑”推进到“可作为标准启动路径”。

工作内容：
- 完善 UEFI firmware 加载、guest 内存布局、启动参数和磁盘/rootfs 处理。
- 支持 Linux、NimbOS 或 ArceOS 等 guest 通过 UEFI 启动。
- 增加 x86_64 和 LoongArch 的 UEFI 启动自动化测试。
- 梳理 UEFI 与 direct boot 两套路径的边界，减少配置复杂度。

交付结果：
- guest UEFI 启动路径稳定可复用。
- Axvisor 更接近真实虚拟化平台使用方式。
- 为后续 ACPI、PCI、磁盘和多 guest 管理打基础。