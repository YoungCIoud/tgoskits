# x86_64 Guest UEFI 同步摘要

## 结论

旧设计的主要偏差不是少了几个新提交，而是架构基线已经改变。`dev@343d47f4e` 包含 PR #1718 的解析后设备图和 PR #1918 的 fw_cfg DMA fault 修复；旧文档仍把其中多项能力写成未来工作。文档现已改为：

- 把 `ResolvedDeviceGraph` 设为固件与 runtime 的唯一资源事实来源；
- 把 fw_cfg PIO、x86 ACPI、PIC、CMOS、PCI config 和 REP string I/O 标记为 `dev` 已有；
- 把“固定固件与嵌套 fw_cfg 诊断”定义为阶段 1 待实施目标；
- 把下一阶段改为“基于当前设备图证明 ACPI 安装与 BDS”，而不是重做旧 stage 2；
- 把旧 `feat/uefi-p2` 降级为调查证据，不作为可直接合入的补丁序列。

## 事实基线与参考实现

```text
dev@343d47f4e
  -> 当前设计事实

feat/uefi-p0-r2
  -> 阶段 1 参考实现
  -> 不计入当前能力或完成状态
```

## `dev` 已有与阶段 1 拟新增

| 范围 | `dev` 已有 | 阶段 1 计划 |
| --- | --- | --- |
| 设备架构 | resolved graph、claim/lease、sealed runtime | 保持不变 |
| 固件平台 | x86 fw_cfg PIO、ACPI direct/fw_cfg 两路、SPCR、PM timer | 固定实际运行的 OVMF 资产和 profile |
| x86 legacy | IOAPIC、PIC、PIT、CMOS、PCI config | 不重复实现 |
| vCPU I/O | 单次 PIO、REP INS/OUTS、guest-memory 事务 | 保持不变 |
| 测试 | normal VMX/SVM smoke | 新增 non-gating `ovmf-entry-vmx`/`ovmf-entry-svm` |
| 成功证据 | 无嵌套固定固件门禁 | 新增 `Nested OVMF fw_cfg accessed` |

## `dev` 当前能够声明的能力

- x86 VM 资源由 resolved graph 统一规划；
- x86 已有 fw_cfg PIO、ACPI、传统平台设备和最小 PCI 配置骨架；
- direct ACPI 与 fw_cfg ACPI 读取同一个 `X86FirmwarePlan`；
- normal VMX/SVM smoke 已存在。

`dev` 当前不能声明：

- 固定上游 OVMF 诊断资产和 profile 已接入；
- VMX/SVM `ovmf-entry-*` 用例可用；
- 嵌套固件访问 fw_cfg 有可归因自动证据；
- ACPI table-loader 已完整执行；
- OVMF 稳定到达 BDS；
- PCI 启动盘、EFI 应用或 Linux 可启动；
- OVMF VARS 在嵌套 guest 中持久化；
- SMP、INIT/SIPI、NMI 或完整 MSI/MSI-X 可用；
- UEFI 用例为 CI gating。

## 下一步

1. 先按 `pr1.md` 从 `dev` 实施固定固件、profile、trace 和 `ovmf-entry-*`；
2. 以嵌套 fw_cfg marker 证明阶段 1；
3. 再沿 resolved graph、`X86FirmwarePlan`、fw_cfg payload 与 table-loader 核对 ACPI 安装；
4. 只对阶段 2 当前运行证据证明的缺口添加失败回归和最小修复；
5. BDS 成立后，再设计 resolved-graph 原生的 guest PCI 启动设备。

详细设计见 [`design.md`](design.md)，阶段 1 计划见 [`pr1.md`](pr1.md)，阶段 2 计划见 [`pr2.md`](pr2.md)。
