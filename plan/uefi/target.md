# Axvisor x86_64 Guest UEFI 目标

## 当前基线

- `dev@343d47f4e` 已有解析后设备图、x86 fw_cfg PIO、ACPI、PIC、CMOS、PIT、IOAPIC、ACPI PM timer、最小 PCI 配置机制和 REP INS/OUTS。
- `dev` 尚无固定上游 RELEASE OVMF 的嵌套诊断用例，也没有能区分外层与嵌套固件的自动成功判据。
- `feat/uefi-p0-r2` 只作为阶段 1 的参考实现；本设计不把该分支能力视为当前状态。

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

1. 固定固件与嵌套 fw_cfg 诊断：当前待实施阶段。
2. 在最新 resolved graph 上证明 ACPI 安装与 BDS：阶段 1 完成后的下一阶段。
3. 扩展现有 PCI 模型并加入可启动 guest virtio-blk：后续阶段。
4. 完成 INTx/MSI/MSI-X、INIT/SIPI/NMI 与 SMP：后续阶段。
5. Linux EFI stub、VMX/SVM 稳定门禁：最终阶段。

## 明确不重复实现

- 不重新添加 fw_cfg PIO、PIO memory-aware 写路径、x86 ACPI、PIC、CMOS 或 REP string I/O；这些已经在 `dev`。
- 不 cherry-pick 旧 `feat/uefi-p2` 作为整体实现；它基于 resolved graph 之前的架构，只能作为调试证据。
- 不在 TOML 暴露设备地址、IRQ、MSI 或 controller ID。
- 不在固件 composer、runtime 和测试中维护多份地址/中断事实。

详细边界、风险和验证矩阵见 [`design.md`](design.md)。
