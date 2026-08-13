# Axvisor x86_64 Guest UEFI 目标

## 当前基线

- `dev@56f8bfc82`（2026-08-13）已有解析后设备图、x86 fw_cfg PIO/DMA、ACPI、PIC、CMOS、PIT、IOAPIC、ACPI PM timer、最小 PCI 配置机制和 REP INS/OUTS。
- `dev` 已有固定 Ostool OVMF 输入的 VMX/SVM 对称用例，能够由嵌套 Linux marker 证明 OVMF 已完成 ACPI 安装和 kernel handoff；这条链路仍通过 fw_cfg 提供 kernel/initramfs，不证明 PCI 启动盘。
- PR #1935 已加入 `axvirtio-common` 的 checked split virtqueue、`axvirtio-blk` 的可复用块请求/MMIO 实现，以及 AArch64 virtio-mmio 块设备回归。x86 仍只有 host bridge/LPC 的最小 PCI config，没有 virtio PCI function、BAR、INTx 数据盘或 OVMF PCI 启动盘。
- 其他功能分支不作为现状或待搬运实现；设计只以最新 `dev`、本文目标和固定 commit 的参考实现为依据。

## 最终目标

让 x86_64 客户机沿标准 UEFI 路径完成：

```text
reset vector
  -> OVMF SEC/PEI/DXE/BDS
  -> fw_cfg table-loader 安装 ACPI
  -> PCI 枚举与启动盘发现
  -> EFI 应用 / Linux EFI stub
  -> guest shell
```

VMX 与 SVM 必须使用同一 backend-neutral guest baseline，并在结果可复现、可归因后进入 CI gating。

## 分阶段交付

1. 固定固件并证明嵌套 OVMF、ACPI 与 Linux handoff：已完成，继续作为回归基线。
2. 复用并按 transport/core 边界整理现有 `axvirtio-common`/`axvirtio-blk`，扩展 x86 PCI root bus，交付 direct-boot Linux 可读取的只读 virtio-blk PCI 数据盘：当前阶段。
3. 让 OVMF 从同一 PCI 设备的 ESP 自动加载 EFI 应用：后续阶段。
4. 从 guest 启动盘加载 Linux EFI stub，并定义 VARS 生命周期：后续阶段。
5. 完成 INTx/MSI/MSI-X、INIT/SIPI/NMI、SMP 和 VMX/SVM 稳定门禁：最终阶段。

## 明确不重复实现

- 不重新添加 fw_cfg PIO、PIO memory-aware 写路径、x86 ACPI、PIC、CMOS 或 REP string I/O；这些已经在 `dev`。
- 不为 x86 F2 重新实现一套 split virtqueue、块请求 core 或可写 file backend；优先复用或最小重构 `dev` 的 `axvirtio-common`/`axvirtio-blk`，并保持现有 virtio-mmio 调用方和 CI 行为。
- 不 cherry-pick 其他分支的现成实现；它们不属于本设计的事实来源。
- 不在 TOML 暴露设备地址、IRQ、MSI 或 controller ID。
- 不在固件 composer、runtime 和测试中维护多份地址/中断事实。

详细边界、风险和验证矩阵见 [`design.md`](design.md)。
