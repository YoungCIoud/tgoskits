# PR 2：基于最新 dev 证明 ACPI 安装与 BDS

## 1. 状态

本文取代旧 `feat/uefi-p2` 的提交清单。旧分支基于 `dev@4c7aa8577`，早于 PR #1718 的解析后设备图；其中 fw_cfg PIO、PIO guest-memory 写路径、x86 ACPI、PIC/CMOS、PCI config 和 REP string I/O 等内容已经以不同边界进入最新 `dev`，不能整体 rebase 或 cherry-pick。

旧分支在 AMD/SVM 上到达 `[BdsDxe]` 的运行记录仍有调查价值，但不是当前基线的自动通过证据。

## 2. 问题

阶段 1 计划使用的 `Nested OVMF fw_cfg accessed` 只能证明嵌套固件开始访问 fw_cfg。最新 `dev` 虽然已经生成 ACPI blob 并提供 table-loader，但尚无自动用例证明：

- table-loader allocate/pointer/checksum 操作成功；
- RSDP/XSDT/FADT/MADT/DSDT/SPCR 被嵌套固件安装并接受；
- RAM、固件窗口和 ACPI 分配没有互相覆盖；
- OVMF 所需 MSR/PIO 语义足够推进到 BDS；
- BDS 证据不会误命中 QEMU 外层 OVMF。

## 3. 范围

阶段 2 只处理“阶段 1 的固定 OVMF 在最新 resolved graph 上从首次 fw_cfg 访问推进到可归因 BDS”所需的最小缺口，并且只有阶段 1 验收后才开始。

不包含：

- guest virtio-blk 或其他启动盘；
- OVMF VARS 持久化；
- Linux EFI 启动；
- INIT/SIPI、NMI、完整 MSI/MSI-X；
- UEFI CI gating。

## 4. 调查顺序

1. 在完成阶段 1 的实施 head 上用固定 Ostool 固件运行 SVM 或 VMX；
2. 保存完整串口和 `DeviceRuntime` trace，而不是只看 success marker；
3. 跟踪 `fw-cfg` 节点的 resolved PIO slots：`selector-data` 与 `dma`；
4. 跟踪 `X86FirmwarePlan::from_graph` 生成的串口、IOAPIC、PM timer 和 fw_cfg 资源；
5. 核对 `build_fw_cfg_blobs()` 输出及 `etc/acpi/tables`、`etc/acpi/rsdp`、`etc/table-loader`；
6. 核对 `FwCfgPayloadSlot` 在设备 build 前完成填充；
7. 定位当前基线第一个不支持的 MSR/PIO/MMIO 或 DMA fault；
8. 只为已证实缺口添加必失败回归，再做最小修复。

旧 `feat/uefi-p2` 只用于对照候选缺口。MTRR、额外 fw_cfg 文件或内存分类只有在当前 trace 证明需要时才进入实现，不能因旧分支曾修改就直接复刻。

## 5. 架构约束

- 不新增第二个 fw_cfg 设备或 selector 状态；
- 不在 `os/axvisor` 手工拼 ACPI；
- direct ACPI 与 fw_cfg ACPI 必须继续读取同一 `X86FirmwarePlan`；
- 固件资源必须来自 `ResolvedDeviceGraph`，不能复制 `0x510`、IRQ 9、COM1 等为第二事实源；
- PIO/MMIO exit 只通过 sealed `DeviceRuntime` 单次 dispatch；
- table-loader、DMA 与 guest-memory fault 保持 PR #1918 的显式错误语义；
- 不用全零、全一或忽略写入等伪成功推动非规范路径；PCI 不存在 function 的规范全一返回除外。

## 6. BDS 证据设计

旧方案使用：

```text
(?s)VCpu\[0\] running\.\.\..*\[BdsDxe\]
```

该模式跨越的输出可能超过 runner 的流式匹配窗口，且外层 OVMF 也可能输出相同 BDS 文本，因此不能作为新门禁直接复用。

接受的证据优先级：

1. 嵌套设备模型或 guest console 边界产生的单行、VM 归属明确的 BDS 等价 marker；
2. runner 能独立消费的、无需跨长距离 `.*` 的嵌套串口 marker；
3. 在自动归因机制落地前，保存固定命令、固件摘要和完整人工转录，维持 non-gating。

不得把“延长 timeout”或“放宽 success regex”当作完成。

## 7. 退出条件

- [ ] 当前基线存在一个确定失败的回归或可重复 trace，能证明所修缺口；
- [ ] 每个代码改动都对应该缺口，没有重复实现 dev 能力；
- [ ] ACPI table-loader allocate/pointer/checksum 路径有最低层自动测试；
- [ ] ACPI 表指针闭包、checksum 和 resolved 资源一致；
- [ ] 固定 OVMF 到达 BDS 有嵌套归属明确的证据；
- [ ] `ovmf-entry-vmx`/`ovmf-entry-svm` 至少在可用宿主之一通过；
- [ ] normal 组 VMX/SVM smoke 不退化；
- [ ] 所有修改 crate 完成 `cargo fmt`、目标 clippy 和测试；
- [ ] 仍保持 non-gating，除非另有独立稳定性证据和 CI 设计。

## 8. 建议提交形状

提交数不预先固定。调查提交不合入；最终提交按真实边界拆分：

1. `test(<crate>): reproduce <exact missing semantic>`：先证明当前 dev 失败；
2. `fix(<crate>): <exact semantic fix>`：只修改职责所有者；
3. `test(axvisor): prove nested OVMF reaches BDS`：增加可靠、短窗口、嵌套归属明确的证据；
4. `docs(axvisor): document the BDS boundary`：同步用户文档、设计与验证命令。

如果调查表明最新 `dev` 已能到达 BDS，只缺可靠判据，则本 PR 不应包含无关固件或设备重构，只补证据和文档。

## 9. PR 标题占位

应在根因明确后确定 scope，例如：

```text
fix(axdevice): preserve <fw_cfg semantic> for nested OVMF
```

或仅补验证时：

```text
test(axvisor): verify nested OVMF reaches BDS
```

PR 正文必须用中文说明：当前失败证据、根因、为何修改职责所有者、回归为何会在旧实现失败，以及 VMX/SVM 实测结果。
