# 阶段 1 适配最新 dev 的冲突处理原则

## 状态

本文以 `dev@343d47f4e` 为唯一事实基线，说明如何把阶段 1 方案落到最新架构。`feat/uefi-p0-r2` 只作为参考实现，用来了解固件资产、runner 和诊断 marker 的可行形状；本文不声明该阶段已经实现或验证。

## 主要架构变化

旧 P0/P2 方案形成后，`dev` 已通过 PR #1718 引入解析后设备图，并通过 PR #1918 修正 fw_cfg DMA fault 语义。实施阶段 1 时必须先接受这些新边界：

| 旧方案假设 | 最新 `dev` 事实 | 适配方式 |
| --- | --- | --- |
| fw_cfg/ACPI 仍需新建 | x86 fw_cfg PIO 与 ACPI 已存在 | 阶段 1 只加诊断和固定资产，不重做协议 |
| machine/factory 可直接追加设备 | 设备必须经 resolved graph 规划并 seal | marker 放在既有 `FwCfgPioDevice` 真实访问路径 |
| 设备地址可在测试/OS 层复制 | 固件与 runtime 必须消费 resolved resources | 测试只选择用例，不维护第二份平台资源表 |
| SEC 串口文本可直接判定嵌套固件 | 外层 OVMF 会输出相同阶段文本 | 使用只经过 Axvisor 设备模型的嵌套 fw_cfg marker |
| 旧 P2 提交可继续 rebase | 多数能力已以新边界进入 dev | 旧分支只作为运行证据，逐项重新判断真实缺口 |

## 阶段 1 应保留的功能意图

- 固定 OVMF CODE/VARS 布局和 reset vector；
- 显式区分 Ostool 上游固件目录与未验证本地 CODE；
- VMX/SVM 分离测试 case，但共享 backend-neutral guest baseline；
- 记录真实设备访问，不用假成功推动固件；
- 固件缺失时 fail-fast，不扫描发行版路径；
- 测试先保持 non-gating，直到 BDS/OS 证据稳定。

## 实施时不应执行

- 不恢复 PR #1718 删除的中心设备 enum、旧 factory registry 或旧 config 字段；
- 不手工合并旧单文件 `virtualization/axdevice/src/fw_cfg.rs`；最新实现位于 `fw_cfg/` 模块；
- 不重新实现 PIO guest-memory 写路径、PIC、CMOS、PCI config 或 REP string I/O；
- 不把参考分支提交号当作新的提交计划；
- 不用更宽松 regex 或更长 timeout 代替归因问题；
- 不修改 `Cargo.lock` 冲突内容；若未来 rebase 产生 lockfile 冲突，应先解决其他文件，再由 Cargo 重新生成。

## 从 dev 实施的建议顺序

1. 先加入固件来源和布局检查的 host 单测；
2. 再加入可选 `firmware_profile` 及 loader 失败测试；
3. 在既有 `DeviceRuntime`/`FwCfgPioDevice` 路径增加不改变语义的诊断；
4. 新增 backend-neutral VM 配置和 VMX/SVM case；
5. 最后接入 runner 的外层 pflash与嵌套 CODE，并运行真实 KVM 验证；
6. 同步用户文档，但只声明实际达到的 fw_cfg 访问边界。

## 阶段 1 验收清单

- [ ] 实施分支以目标 `dev` 为 merge-base；
- [ ] 未重复 `dev` 已有的 fw_cfg/ACPI/platform device 能力；
- [ ] 用例名称为 `ovmf-entry-vmx` / `ovmf-entry-svm`；
- [ ] 成功判据为嵌套专属的 `Nested OVMF fw_cfg accessed`；
- [ ] 外层 QEMU pflash 与嵌套 guest 使用同一 CODE；
- [ ] `uefi = false` 的 runner 角色有文档说明；
- [ ] 未验证本地固件只能显式 opt-in，且不作为功能通过证据；
- [ ] 旧 stage 2 只作为调查材料，没有被整体移植；
- [ ] 所有代码验证来自实施后的当前 head。

未来 `dev` 再次前进时，应重新检查 resolved graph、固件 plan、fw_cfg fault 语义和测试 runner，不能机械复用本文的文件级判断。
