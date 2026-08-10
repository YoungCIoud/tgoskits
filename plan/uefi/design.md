# Axvisor x86_64 Guest UEFI 设计

> 事实基线：`dev@e04a3ca28`（2026-08-09，包含 PR #1718 的解析后设备图、PR #1917 的 Ostool OVMF 复用、PR #1918 的 fw_cfg DMA fault 修复和 PR #1931 的 VMX/SVM OVMF/ACPI 验证）。
>
> 风险等级：高风险。后续功能阶段涉及固件协议、客户机物理地址布局、PIO/DMA、ACPI、PCI、中断和 vCPU 启动契约。

## 0. 文档权威性

本文是 Axvisor x86_64 guest UEFI 路线的**唯一开发依据**，必须能够脱离 `target.md`、`pr1.md`、`pr2.md`、历史分支和实现 diff 独立评审。

- `target.md` 是目标摘要；本文完整重述并约束该目标，不依赖读者同时打开它。
- `pr*.md`、`summary.md`、`solve_conflict.md` 只能从本文派生，用于某次实施、评审或冲突处理。
- 派生文档与本文冲突时，以本文为准；不得用派生文档反向修改阶段边界。
- `dev` 的代码、测试和当前 head 运行记录决定“现状”；历史分支只能提供调查线索，不能证明当前能力。
- 目标、现状或证据发生实质变化时，先更新本文，再同步派生文档。

本文分为两大部分：第一部分回答“要达到什么、现在有什么、证据能证明什么”；第二部分回答“先验证什么、再逐阶段增加哪些 feature”。

## 第一部分：目标、现状与证据

### 1. 目标

#### 1.1 问题

Axvisor x86_64 已经具备 guest UEFI 路径的大部分基础构件。PR #1931 已在 `dev` 建立 VMX/SVM 对称的 OVMF -> Linux initramfs/ACPI 验证，并记录实际固件输入和最终 guest image 摘要；当前开发重点已经从“证明既有链路”推进到“增加标准 guest PCI 启动设备”。仍需持续明确区分：

- 仓库中已经实现的机制；
- 当前测试实际证明的行为；
- 只在单一 backend 或单次人工运行中观察到的结果；
- 仍需新增的 guest 平台 feature；
- 只有达到稳定性要求后才能建立的 CI 门禁。

如果不先建立这组边界，后续工作容易重复实现 `dev` 已有能力，或把“存在代码”“看到固件日志”“guest OS 真正启动”误认为同一强度的证据。

#### 1.2 受众

- Axvisor x86 VMX/SVM 维护者：需要同一 backend-neutral guest baseline 和可归因的运行结果；
- 虚拟设备、固件与 ACPI 维护者：需要确认 firmware 与 runtime 消费同一 resolved resources；
- guest OS 集成者：需要标准 PCI 启动设备、EFI 应用和 Linux EFI stub 路径；
- CI 维护者：需要可复现、可重复且不会误命中外层 QEMU OVMF 的门禁。

#### 1.3 最终目标

让 x86_64 Axvisor guest 沿标准 UEFI 路径完成：

```text
reset vector
  -> OVMF SEC/PEI/DXE/BDS
  -> fw_cfg table-loader 安装 ACPI
  -> PCI 枚举与 guest 启动盘发现
  -> EFI 应用 / Linux EFI stub
  -> guest initramfs 或 shell
```

最终状态还必须满足：

1. VMX 与 SVM 使用同一 backend-neutral guest 配置，只由外层 QEMU CPU 能力暴露区分宿主 backend；
2. guest 固件、ACPI、PCI、DMA 和中断资源来自同一解析后设备图，不维护第二份地址或 IRQ 事实；
3. 单 vCPU 启动路径稳定后，支持由 INIT/SIPI 启动辅助 vCPU；
4. VMX/SVM 结果可复现、可归因且达到稳定性门槛后，才进入 CI gating。

#### 1.4 最终成功标准

| 声明 | 必须具备的证据 |
| --- | --- |
| 固件输入可复现 | 记录 Ostool 来源、实际 CODE/VARS 路径、尺寸和 digest；运行使用的镜像与摘要一致 |
| 嵌套 OVMF 确实执行 | 证据产生于 Axvisor guest 设备或 guest console 边界，不能由 QEMU 外层 OVMF 产生 |
| ACPI 被固件安装并被 OS 接受 | table-loader 操作测试通过；guest 能读取要求的 ACPI 表并初始化 IOAPIC/串口 |
| 固件发现标准 guest 启动设备 | PCI 枚举和块设备访问来自 guest device model，不把宿主 rootfs 磁盘误认为 guest 启动盘 |
| EFI/Linux 启动成立 | EFI 应用 marker 和 Linux initramfs/shell marker 分别来自 guest 内部 |
| SMP guest 成立 | 至少两个 vCPU 在线，INIT/SIPI、APIC ID 映射和中断路径有自动验证 |
| 可以建立门禁 | VMX/SVM 分别在对应宿主重复通过，资产获取和结果归因不依赖人工判断 |

#### 1.5 路线非目标

以下能力不属于本路线的完成条件，除非另有独立设计：

- Secure Boot、SMM、TPM、PXE；
- 完整 q35 芯片组仿真；
- 在 TOML 中暴露设备地址、IRQ、MSI vector 或 controller ID；
- 为 x86 UEFI 重写 resolved device graph、fw_cfg 或 ACPI 的平行实现；
- LoongArch、AArch64 或 RISC-V guest UEFI 路径重构。

### 2. 当前 `dev` 事实

#### 2.1 已有实现

| 范围 | `dev@e04a3ca28` 的所有者与实现 | 能够确认的事实 |
| --- | --- | --- |
| 设备资源 | `DeviceGraphBuilder -> ResolvedDeviceGraph -> ResourceClaimSet/lease -> DeviceRuntime` | graph 是固件与 runtime 的唯一资源事实来源 |
| x86 平台计划 | `virtualization/axvm/src/arch/x86_64/vm.rs` | 已规划 IOAPIC、fw_cfg、PIT、PIC、CMOS、PCI config 和 ACPI PM timer |
| fw_cfg PIO/DMA | `FwCfgPioDevice`、共享 selector/DMA 状态和 memory-aware PIO 写路径 | 已有 selector/data/DMA 访问及显式 guest-memory fault 语义 |
| x86 ACPI | `X86FirmwarePlan::from_graph`、direct ACPI image 和 fw_cfg table-loader 输出 | 两条输出消费同一个 firmware plan |
| ACPI 表 | RSDP/XSDT/FADT/FACS/DSDT/MADT/SPCR | 已有单 vCPU 平台所需的当前表集；不声称已有 MCFG |
| x86 legacy | IOAPIC、双 8259 PIC、PIT、CMOS、ACPI PM timer | 不需要作为 UEFI feature 重复实现 |
| PCI 配置 | `X86PciConfigDevice` 的配置机制 #1，当前暴露 host bridge 与 LPC bridge | 只证明最小枚举骨架，不等于已有 guest 启动盘 |
| PIO string I/O | VMX/SVM 的 `X86PortIoStringExit` 逐元素事务 | 不需要重新增加 REP INS/OUTS 支持 |
| 固件 payload | `FwCfgPayloadSlot` 在设备 build 前填充 kernel/initrd/cmdline 与 ACPI blob | 不应在 OS 配置层重建第二份 payload |
| OVMF 资产 | `support::ovmf::OvmfFirmware` 复用 Ostool 资产，`axvisor/test/ovmf.rs` 组装 4 MiB x86 镜像 | 构建记录实际 CODE/VARS 和最终 guest image 的路径、尺寸、SHA-256 及 split/monolithic 布局，并校验最终镜像尺寸 |
| 嵌套 OVMF ACPI 用例 | `test-suit/axvisor/normal/qemu-acpi-ovmf` | 已有 VMX/SVM 对称用例，共用 guest baseline，只改变外层 QEMU CPU capability；两者均通过 OVMF 启动 Linux initramfs 并执行 guest-side ACPI 断言 |
| table-loader 证据 | `x86_64/boot/acpi/fw_cfg.rs`、`boot/acpi/loader.rs` 最低层测试 | 解码并验证 allocate、pointer、checksum 的文件、偏移、范围、宽度和顺序，不只检查命令数量 |
| x86 SMP 固件启动 | `x86_vlapic::inject_nmi` 和 `process_init_sipi` | NMI、INIT/SIPI 仍被明确忽略，不能声明多 vCPU UEFI guest |

#### 2.2 当前自动证据与运行记录

现有 `ovmf-acpi-vmx` 和 `ovmf-acpi-svm` 用例比“首次 fw_cfg 访问”更强。它们使用同一 build config、guest TOML、BusyBox initramfs、OVMF 镜像和成功断言，仅由外层 QEMU 分别暴露 VMX/SVM capability，并完成以下链路：

```text
Ostool OVMF
  -> 组装 4 MiB guest firmware image
  -> Axvisor loader 放置固件与 Linux kernel/initramfs payload
  -> 嵌套 OVMF 启动 Linux
  -> guest init 检查 ACPI 与平台状态
  -> AXVISOR_X86_OVMF_ACPI_PASSED
```

guest 内的 `run_x86_acpi_check` 只有在以下条件全部满足时才输出成功 marker：

- `DSDT`、`APIC`、`FACP`、`SPCR` 可读；
- online CPU 集合为 `0`；
- `ttyS0` 存在；
- Linux 已初始化 IOAPIC。

该 marker 来自嵌套 Linux initramfs，不可能由 QEMU 外层 OVMF 直接产生，因此已经能够分别证明 VMX/SVM 路径上的嵌套固件启动、Linux 进入早期用户态以及 ACPI 被 Linux 接受。它也意味着固件已经越过 BDS 并完成 kernel handoff，但当前没有独立、短窗口的 BDS marker。

PR #1931 还让构建日志输出实际 Ostool CODE/VARS 和最终 4 MiB guest image 的路径、尺寸与 SHA-256，并区分 split/monolithic 布局；guest TOML 使用的 `uefi_firmware_path` 指向该最终镜像。当前 head 已人工完成 `smoke-vmx`、`smoke-svm`、`ovmf-acpi-vmx` 和 `ovmf-acpi-svm` 各一次成功运行。该结果完成 V 阶段，但一次成功轮次仍属于 E5，不足以替代 F6 要求的重复运行和 CI 记录 E6。

#### 2.3 证据强度

证据必须按强度解释，较弱证据不能替代较强声明：

| 等级 | 证据 | 可以证明 | 不能单独证明 |
| --- | --- | --- | --- |
| E0 | 代码、配置或静态检查存在 | 实现形状和配置可解析 | 运行路径被执行 |
| E1 | 最低层单元/集成测试 | 局部协议、布局、错误和资源不变量 | 完整固件启动 |
| E2 | runner 能发现、构建并启动 case | 测试接线有效 | guest 已到达目标状态 |
| E3 | Axvisor guest 设备边界 marker/trace | 嵌套 guest 已访问对应设备 | ACPI、BDS 或 OS 启动完成 |
| E4 | guest 固件或 guest OS 内部断言 | 断言覆盖的固件/平台行为成立 | 未覆盖设备和跨 backend 稳定性 |
| E5 | EFI 应用或 Linux guest marker | guest 启动链已到达对应 payload | SMP、完整中断和长期稳定性 |
| E6 | VMX/SVM 重复运行与 CI 记录 | backend 对称性和门禁稳定性 | 未纳入用例的 feature |

`AXVISOR_X86_OVMF_ACPI_PASSED` 属于 E5，并包含一组 E4 的 ACPI 断言；VMX/SVM 对称用例和证据来源摘要已经补齐。当前证据缺口是跨宿主重复性和 CI，而不是重新发明 E3 marker。

#### 2.4 当前可以声明

- x86 guest 资源由 resolved graph 统一规划；
- x86 已有 fw_cfg PIO/DMA、ACPI、legacy 平台设备、最小 PCI 配置和 REP string I/O；
- Ostool OVMF 可以被组装为固定 4 MiB 嵌套 guest 固件镜像，实际输入、输出、布局和 SHA-256 可从构建日志复核；
- `ovmf-acpi-vmx` 与 `ovmf-acpi-svm` 共用 backend-neutral guest baseline，并均能通过 OVMF 启动 Linux initramfs；
- 两种 backend 的 guest 均能读取 DSDT/APIC/FACP/SPCR，并观察到 ttyS0、IOAPIC 和单 online CPU；
- table-loader 的 allocate、pointer 和 checksum 命令布局已有完整最低层测试；
- normal `smoke-vmx`/`smoke-svm` 与两条 OVMF/ACPI 用例已经在当前 head 完成一轮人工验证。

#### 2.5 当前不能声明

- 独立 BDS marker 已建立；现有 Linux marker 只能作为越过 BDS 的更强端到端证据；
- OVMF VARS 在嵌套 guest 重启后持久化；
- OVMF 已枚举 Axvisor 提供的标准 PCI guest 启动盘；
- EFI 应用从 guest ESP 被加载；
- Linux 是从 guest PCI 启动盘沿 EFI stub 启动，而不是当前 fw_cfg kernel/initrd payload；
- guest PCI INTx/MSI/MSI-X 完整可用；
- INIT/SIPI、NMI 或多 vCPU UEFI guest 可用；
- VMX/SVM 已具备跨宿主、多轮重复的 E6 稳定性证据；
- UEFI 路径已经进入 CI gating。

#### 2.6 未验证与确认缺失必须分开

| 类别 | 项目 | 后续处理 |
| --- | --- | --- |
| 已完成验证 | OVMF 资产摘要、VMX/SVM 对称验证、table-loader 命令、BDS/guest marker 归因 | V 阶段已由 PR #1931 和当前 head 运行记录完成；继续作为后续阶段回归基线 |
| 已确认缺失 | guest PCI 启动设备、ESP/EFI 应用加载、VARS 持久化、INIT/SIPI/NMI、完整 guest MSI 链 | 进入后续 feature 阶段 |
| 已知但未阻塞 | vPIC `INTR/LAPIC/NULL` wire-mode transition 尚未实现 | 当前四个用例通过，不进入 F1；保留到 F5，除非 F2 trace 证明其阻塞数据盘 INTx |
| 仍需补证据 | VMX/SVM 跨宿主、多轮重复性与 CI 记录 | 进入 F6；不能阻塞功能开发，也不能提前声明 gating |

### 3. 证据与开发约束

#### 3.1 验证不等于 feature

验证阶段只能证明 `dev` 现有行为，不得为了让验证通过而改变产品语义。

验证阶段允许：

- 新增或调整 test-suit case、runner 接线和测试资产准备；
- 输出实际使用的固件路径、尺寸、digest 和命令摘要；
- 增加只观察既有状态、且归属边界明确的测试断言；
- 在现有日志不足以归因时增加最窄范围的诊断 marker；
- 更新设计、测试说明和运行记录。

验证阶段禁止：

- 新增公开 `firmware_profile`、用户配置字段或面向产品的 CLI；
- 改变 loader、fw_cfg、ACPI、MSR、PIO/MMIO、PCI 或 vCPU 的运行语义；
- 返回伪成功、忽略错误、放宽 regex 或只延长 timeout；
- 把验证中发现的功能修复混入验证提交；
- 用历史分支的运行结果替代当前 `dev` head 证据。

若现有行为不能达到验证目标，验证阶段的正确结果是记录第一个确定失败、完整复现命令和候选职责所有者；修复进入后续独立阶段。

#### 3.2 架构不变量

- firmware composer 与 runtime 必须消费同一 `ResolvedDeviceGraph` 和 resolved resources；
- 不新增第二个 fw_cfg 设备、selector 状态、ACPI composer 或旁路 PCI 总线；
- MMIO/PIO exit 只经过 sealed `DeviceRuntime` 单次 dispatch，不恢复地址特判或 downcast；
- direct ACPI 与 fw_cfg ACPI 继续读取同一 `X86FirmwarePlan`；
- table-loader、DMA 和 guest-memory fault 保持显式错误语义；
- PCI config、固件资源描述和 runtime device 必须来自同一个 resolved node；
- 中断控制器是 pending/active/route/EOI 的唯一所有者；
- backend-neutral guest 配置不得选择 Cargo `vmx`/`svm` feature；差异只位于外层 QEMU CPU 能力暴露；
- 不把宿主 NVMe rootfs 或 QEMU 外层 pflash 当作 guest 启动设备证据。

#### 3.3 外部语义与 prior art

| 资料 | 版本/标识 | 本设计使用的语义 |
| --- | --- | --- |
| [UEFI Specification](https://uefi.org/specs/UEFI/2.10/) | 2.10 | SEC/PEI/DXE/BDS、EFI image 与 Boot Services 生命周期 |
| [UEFI media access](https://uefi.org/specs/UEFI/2.10/13_Protocols_Media_Access.html) | UEFI 2.10 chapters 3/13 | Block I/O、partition、Simple File System、FAT 和 `\\EFI\\BOOT\\BOOTX64.EFI` fallback |
| ACPI Specification | 6.5 | RSDP/XSDT/FADT/MADT/DSDT/SPCR 指针和 checksum 语义 |
| [VirtIO Specification](https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html) | OASIS VirtIO 1.2 | non-transitional PCI discovery/capabilities、split virtqueue、block、只读 feature 和 INTx ISR 语义 |
| QEMU fw_cfg specification | QEMU 9.x 文档语义 | selector/data/DMA 和 table-loader 协议 |
| QEMU q35 | `pc_q35`/ICH9 设备语义 | PCI config、INTx 和固件设备约定；不复制完整芯片组 |
| [EDK2/OvmfPkg](https://github.com/tianocore/edk2/tree/master/OvmfPkg) | Ostool 管理的 x86_64 OVMF 对应源码版本 | 实际验证固件及 VirtioPci/Block、partition、FAT driver；每次证据记录具体资产摘要 |
| resolved device graph | `book/design/axvisor-resolved-device-graph.md` | 资源规划、claim/lease、firmware/runtime 一致性 |

#### 3.4 方案选择

| 方案 | 结论 | 原因 |
| --- | --- | --- |
| 把旧“阶段一/阶段二”分别实现 marker 和 BDS | 拒绝 | `dev` 已有更强的 guest Linux/ACPI 证据，重复增加验证 feature |
| 直接搬运历史 UEFI 分支 | 拒绝 | 早于 resolved graph，容易重复现有能力并制造第二事实源 |
| 验证与缺口修复放在同一阶段 | 拒绝 | 无法区分现状与新增 feature，也无法证明回归在修复前存在 |
| 先统一验证当前 `dev`，再按已确认缺口逐阶段加 feature | 采用，V 已完成 | 证据与能力边界清晰，每个 feature 可独立设计、回归和审核 |

## 第二部分：分阶段开发与验证

### 4. 阶段总览

| 阶段 | 性质 | 目标 | 状态 | 是否增加产品 feature |
| --- | --- | --- | --- | --- |
| V：现状验证 | 验证 | 固定并复核 `dev` 已有 OVMF -> Linux/ACPI 链路，补齐 VMX/SVM 证据 | 已完成：PR #1931，`dev@e04a3ca28` | 否 |
| F1：固件平台缺口 | 条件 feature/fix | 修复 V 阶段确认且阻塞现有链路的最小语义缺口 | 已跳过：V 未发现阻塞语义缺口 | 是；无缺口则跳过 |
| F2：PCI 只读数据盘 | feature | 让 direct-boot Linux 枚举标准 PCI virtio-blk、挂载只读数据盘并读取 identity | 当前阶段：先固定 PCI/virtio/数据盘契约 | 是 |
| F3：PCI 启动盘与 EFI 应用 | 集成 feature/交付 | 让 OVMF 从 F2 的同一标准设备读取 ESP 并运行 EFI 应用 | 未开始 | 条件；F2 已满足全部运行语义时不新增产品代码 |
| F4：Linux EFI 启动与 VARS | feature | 从 guest 启动盘启动 Linux EFI stub，并定义 VARS 生命周期 | 未开始 | 是 |
| F5：中断完整性与 SMP | feature | 完成 guest INTx/MSI/MSI-X、INIT/SIPI/NMI 和多 vCPU UEFI | 未开始 | 是 |
| F6：稳定性与 CI 门禁 | 交付 | VMX/SVM 稳定重复并进入 CI gating | 未开始 | 不新增启动语义 |

阶段必须按顺序推进。前一阶段的退出证据是后一阶段的输入；不得以“后续会补”绕过当前阶段的核心失败路径。

feature/交付阶段必须以独立可用结果结束：F2 交付 direct-boot Linux 可使用的标准只读数据盘，F3 交付可自动运行 EFI payload 的标准启动盘，F4 交付从该盘启动的 Linux 和明确的 VARS 生命周期。PCI root-bus 重构、virtqueue core、x86 planner/image-provider 接线或测试资产准备可以独立提交和审核，但不能单独升级为 feature 阶段，也不能在对应可用结果完成前暴露不可工作的用户配置。

### 5. 阶段 V：验证当前实现

> 状态：已完成。实现与自动验证由 PR #1931 合入 `dev@e04a3ca28`；当前 head 的四个目标 case 已完成一轮人工运行。

#### 5.1 目标

在不改变产品运行语义的前提下，复核当前 VMX `qemu-acpi-ovmf`，建立同一 guest baseline 的 SVM 证据，并把固件来源、嵌套归因、ACPI 断言和运行限制记录成可重复验证链。

原设计中的“固定固件与 fw_cfg marker”和“证明 ACPI/BDS”合并在本阶段。fw_cfg 访问只是调查检查点，不能单独成为阶段完成条件；现有 guest Linux/ACPI marker 是更强的最终证据。

#### 5.2 已完成的实施顺序

1. 从最新 `dev` 运行并列出 `normal` 组，确认 `ovmf-acpi-vmx` 和 `ovmf-acpi-svm` 均被 runner 发现；
2. 使用现有 Ostool 获取路径准备 x86_64 OVMF，记录实际 CODE/VARS 路径、尺寸和 digest；
3. 证明 runner 生成的 4 MiB 镜像就是 Axvisor guest loader 实际使用的镜像；
4. 运行 VMX `ovmf-acpi-vmx`，保存完整命令、固件摘要和 guest success marker；
5. 增加仅改变外层 QEMU CPU 暴露的 SVM case，复用同一 build config、guest TOML、固件和成功断言；
6. 在对应 Intel/AMD KVM 宿主分别运行 VMX/SVM；
7. 复核 table-loader 的 allocate/pointer/checksum 最低层测试，以及 guest 对 DSDT/APIC/FACP/SPCR、ttyS0、IOAPIC 的断言；
8. 如果需要定位首个失败，临时启用设备 trace；只有能稳定表达归属且确有长期诊断价值的最小 marker 才能保留；
9. 确认 `normal` 组既有 `smoke-vmx`/`smoke-svm` 不退化。

#### 5.3 推荐命令

```bash
cargo xtask ovmf --arch x86_64
cargo xtask axvisor test qemu --arch x86_64 --test-group normal --list
cargo xtask axvisor test qemu --arch x86_64 --test-group normal \
  --test-case ovmf-acpi-vmx
cargo xtask axvisor test qemu --arch x86_64 --test-group normal \
  --test-case ovmf-acpi-svm
```

两条 case 共用 backend-neutral guest 配置，不能通过 Cargo feature 选择 backend；VMX 应在 Intel/VMX KVM 宿主运行，SVM 应在 AMD/SVM KVM 宿主运行。

#### 5.4 退出条件

- [x] 当前 head 的 VMX case 命中 `AXVISOR_X86_OVMF_ACPI_PASSED`；
- [x] 当前 head 的 SVM 对称 case 命中同一 guest marker；
- [x] 两次运行记录实际 OVMF 路径、尺寸和 digest；
- [x] guest 使用的固件镜像与记录摘要一致；
- [x] DSDT/APIC/FACP/SPCR、CPU、ttyS0 和 IOAPIC 断言全部由 guest 执行；
- [x] table-loader 和 fw_cfg DMA 的最低层成功/错误测试通过；
- [x] normal VMX/SVM smoke 不退化；
- [x] diff 中没有新增产品配置、公共 API 或固件/设备运行语义；
- [x] 用例保持 non-gating，直到 F6 的稳定性标准满足。

完成 V 只表示当前 baseline 已被证明，不表示这些用例可以停止回归。F2 及以后阶段必须继续运行四个 case；重复性和 CI gating 仍由 F6 负责。

#### 5.5 后续回归失败处理

若后续 `dev` 的 VMX 或 SVM 无法再达到 guest marker：

1. 保存固定资产摘要、完整命令、最后可靠状态和第一个错误；
2. 形成最低层、能稳定复现该错误的测试，并定位首次引入回归的阶段或提交；
3. 若回归由 F2 及以后 feature 引入，在对应 feature/fix 中恢复 V baseline，不重新定义 V 的完成标准；
4. 只有证据证明 V 当时漏掉了 baseline 自身的确定语义缺口时，才先修订本文并重新打开 F1；
5. 修复后重新执行 V 阶段四个用例和对应阶段的新增验证。

### 6. 阶段 F1：修复验证确认的固件平台缺口

> 状态：已跳过。V 阶段未发现阻塞现有 OVMF/Linux/ACPI 链路的产品语义缺口；PR #1931 的 Rust 改动均为测试和测试编排，不改变设备运行语义。

#### 6.1 进入条件

只有 V 阶段在固定输入下证明现有实现存在确定语义缺口时才进入本阶段。如果 VMX/SVM 均通过 V 阶段，本阶段直接标记为不需要，不为历史分支中的候选改动寻找理由。

当前观察到的 vPIC wire-mode warning 不满足 F1 进入条件：它们没有阻止 VMX/SVM 到达 guest marker。除非 F2 的设备 trace 证明这些 transition 是数据盘 INTx 的第一个确定阻塞点，否则不得重新打开 F1 或把相关修复混入 PCI/virtio 提交。

#### 6.2 范围

- 每个改动必须对应 V 阶段的确定失败和修复前必失败回归；
- 只修改拥有该语义的现有边界，例如 fw_cfg、ACPI plan、x86 loader、MSR/PIO 或 guest-memory transaction；
- MTRR、额外 fw_cfg 文件、RAM/firmware window 分类等只能在当前 trace 证明需要时进入；
- 不新增旁路设备、第二套 ACPI、地址特判、伪成功或静默 fallback；
- 一个独立缺口原则上对应一个可单独审核的 fix。

#### 6.3 退出条件

- 修复前回归确定失败，修复后通过；
- V 阶段 VMX/SVM 全部退出条件重新通过；
- direct/BIOS 路径和 normal smoke 不退化；
- 设计、实现、测试和用户文档对该语义的声明一致。

### 7. 阶段 F2：PCI 只读数据盘

> 状态：当前阶段。V 已完成且 F1 已跳过；下一项工作是固定 PCI/virtio/数据盘契约，并确认现有 direct-boot Linux 包含所需驱动，尚未开始产品代码实现。

#### 7.1 目标

为 Axvisor x86 guest 增加标准、只读的 modern virtio-blk PCI 数据盘。沿用当前 fw_cfg direct-boot Linux 路径，让 Linux 枚举该设备、挂载磁盘并读取固定 identity 文件。

本阶段完成后，用户已经可以给 direct-boot Linux guest 配置确定性的只读 PCI 数据盘；它不是只能枚举的框架半成品，也不依赖尚未实现的 UEFI 启动链。F3 再复用完全相同的产品设备，把可用性推进到 OVMF 启动盘。

本阶段的最小垂直链路是：

```text
guest TOML 中的只读磁盘后端
  -> Axvisor 在 VM build 前读取并冻结镜像
  -> resolved graph 中组合唯一 function 的 PCI root bus
  -> 4 KiB BAR aperture 和 resolved INTx route
  -> modern virtio-blk PCI transport
  -> direct-boot Linux virtio-pci / virtio-blk
  -> raw ext4 /dev/vda -> /axvisor/F2-ID.txt
  -> AXVISOR_X86_PCI_BLOCK_PASSED identity=axvisor-f2-readonly-data-v1
```

成功 marker 必须由 guest Linux 在核对实际 PCI identity、块设备只读属性和 identity 文件内容后写入 guest console。仅看到 PCI vendor/device、BAR 访问、virtqueue notify 或设备节点均不能完成 F2。

> marker 输出格式：guest 先单独输出一行裸 marker `AXVISOR_X86_PCI_BLOCK_PASSED`，再输出 identity 键值行（如 `identity=axvisor-f2-readonly-data-v1`）。成功 marker 与 identity 是两行输出：success_regex 以 `(?m)^AXVISOR_X86_PCI_BLOCK_PASSED\s*$` 行尾锚定匹配裸 marker 行，identity 由 runner 单独核对。此格式沿用现有 `ovmf-acpi` case 的 `echo $success_marker` 模式，避免 regex 与带 identity 的单行输出互相矛盾。

#### 7.2 进入条件和兼容性确认

开始修改产品代码前必须满足：

1. **已满足**：V 阶段的 `ovmf-acpi-vmx`、`ovmf-acpi-svm`、`smoke-vmx` 和 `smoke-svm` 均通过，F1 保持跳过；
2. 固定 VirtIO 1.2 和 PCI conventional configuration 为协议依据；
3. 检查现有 direct-boot Linux kernel/initramfs 是否包含 `virtio_pci`、`virtio_blk` 和 ext4 支持；F2 使用无分区 raw ext4，不依赖 GPT/MBR 扫描；缺少测试侧驱动时先修复测试资产，不能用产品私有协议代替；
4. 用参考 QEMU 的 `virtio-blk-pci` 把候选镜像作为 `/dev/vda` 直接挂载，证明 raw ext4 和 identity 文件本身有效；
5. 确认 guest PCI MMIO aperture 不与 RAM、firmware、IOAPIC/LAPIC、fw_cfg 或其他 resolved resources 重叠。

#### 7.3 方案选择和非目标

| 方案 | 结论 | 原因 |
| --- | --- | --- |
| modern non-transitional virtio-blk PCI | 采用 | 标准接口，Linux 和目标 OVMF 均有现成驱动；F2 先以 direct-boot Linux 建立独立可用性 |
| 单个 root-bus node 组合唯一 disk function | 采用 | 当前只有一个真实 endpoint；避免给 `DeviceBuildContext` 增加无当前调用方的跨节点 service/seal 协议 |
| root node 与 endpoint node 通过通用 registry 组装 | 推迟 | 只有出现第二种 PCI endpoint 后，才能确定 registry 的真实调用方、生命周期和扩展形状 |
| transitional/legacy virtio-blk | 拒绝 | 会同时引入 legacy PIO、旧 feature 协商和两套 transport 语义 |
| fw_cfg 私有块设备或直接传文件 | 拒绝 | 不能形成标准 PCI 块设备，也不能被后续 OVMF 启动阶段复用 |
| AHCI、NVMe 或宿主块设备直通 | 拒绝 | 控制器和安全边界远大于本阶段；宿主 NVMe rootfs 不是 guest 设备 |
| 可写或宿主文件实时后端 | 推迟 | F2 只需要确定性读取；写入、一致性、flush 和故障恢复进入独立设计 |

F2 明确只支持每个 VM 一块只读数据盘，并且不包含：OVMF 从盘启动、EFI 应用、Linux EFI stub、VARS 持久化、多盘、多 endpoint registry、多队列、packed virtqueue、indirect descriptor、event index、MSI/MSI-X、热插拔、IOMMU、磁盘写入和多 vCPU。这些能力不得以未声明 feature bit、空实现或伪成功形式出现。

#### 7.4 PCI root bus 与资源所有权

当前 `X86PciConfigDevice` 只有 bus 0 上固定的 host bridge 和 LPC bridge，不能直接承载真实 endpoint；尤其不能把 resolved graph 中的固定 MMIO range 当成可由 guest PCI driver 任意重编程的 BAR。F2 把它扩展为一个组合唯一 virtio-blk function 的 PCI root-bus owner：

- 保留配置机制 #1 的 `0xcf8..=0xcff` 唯一入口，不增加 ECAM/MCFG 或第二条 config 路径；
- machine planner 固定唯一 endpoint 为 BDF `0000:00:03.0`、INTA 和一个 4 KiB、32-bit、non-prefetchable BAR0；`00:00.0` host bridge 与 `00:1f.0` LPC bridge 保持不变，用户不能配置 BDF、pin 或 BAR 数值；
- root-bus graph node 同时拥有 config PIO、BAR0 对应的 4 KiB MMIO aperture、唯一 level-triggered exclusive INTx claim、guest-memory 使用声明和只读 block backend；endpoint 不再成为第二个 graph/runtime node；
- 4 KiB aperture 和 INTx GSI 由现有 resource plan 分配并检查冲突，不硬编码 MMIO base 或 GSI19；root bus 是 aperture 的唯一 `DeviceRuntime` claim 和 dispatch owner；
- BAR0 reset value 使用 resolved aperture base。guest 写全一执行 size probe；普通赋值按 BAR mask 保存，只有完整落在该 aperture 且 PCI command memory-space enable 打开时才参与 decode。aperture 外赋值不得改变 sealed runtime 路由，也不得命中其他设备；
- root bus 对 BAR0 的访问继续使用 bus device 的 `DeviceAccess`，并显式持有 guest-memory grant；function 只持有从同一 root-node claim 创建的 `IrqLine`，不能保存裸 GSI、目标 vector 或全局回调；
- x86 machine plan 保存一份 typed PCI plan，包含 node id、BDF、BAR specification、INTx pin 和资源 slot identity。runtime model 与 ACPI composer 都使用该 plan，并从 resolved graph 查询最终 aperture/GSI；不为 F2 扩展通用 `DeviceFirmwareBinding`；
- ACPI `PCI0._CRS` 只发布 bus 0 和实际 4 KiB non-prefetchable memory window；不再发布没有 runtime owner 的 PCI I/O/VGA window。`_PRT` 只发布 `00:03.0 INTA` 到 resolved GSI；未配置磁盘时不发布 endpoint route；
- 不存在的 BDF/config offset 按 PCI 规范返回全一；非法 BAR assignment、aperture 外访问和未开启 memory-space decode 返回明确未命中或保持规范定义的寄存器状态，不能路由到其他设备。

PCI config 最低要求包括 conventional header、command/status、capability list、BAR size probe/assignment、interrupt pin/line 和 reset。BAR 当前值是 PCI function 的运行状态；aperture、BAR 约束和中断路由来自同一 x86 machine/resolved plan，不允许 AML、config space 和 runtime 各自硬编码副本。

#### 7.5 modern virtio-blk PCI 协议面

设备使用 VirtIO 1.2 non-transitional PCI 身份：vendor `0x1af4`、virtio block device ID `2` 对应 PCI device `0x1042`，revision 不使用 legacy 值。4 KiB BAR0 固定承载：

- `0x000`：common configuration；
- `0x100`：queue notification，`notify_off_multiplier = 4`；
- `0x200`：ISR status；
- `0x300`：block device-specific configuration，至少包含 64-bit capacity。

`VIRTIO_PCI_CAP_PCI_CFG` 只是访问上述 BAR region 的可选替代通道，不是 modern virtio PCI device 的必需 capability；F2 不实现也不发布它。只有后续出现独立、可复现且无法通过标准 BAR access 解决的兼容性需求时，才能先修订设计再增加。

本阶段只提供 queue 0，一个 size 为 128 的 split virtqueue。设备只发布 `VIRTIO_F_VERSION_1` 和 `VIRTIO_BLK_F_RO`；不发布 `VIRTIO_BLK_F_BLK_SIZE`、MSI-X、packed ring、ring reset、indirect descriptor、event index、flush、discard、write-zeroes、multi-queue 或 topology feature。capacity 严格等于冻结镜像字节数除以 512。

transport 必须完整实现本阶段可达的状态：

- device/driver feature select、`ACKNOWLEDGE -> DRIVER -> FEATURES_OK -> DRIVER_OK` 和 `FAILED`；
- 未提供 feature 的协商不能保留 `FEATURES_OK`；`device_status = 0` 同步恢复 transport、queue、ISR 和 block request 状态；
- queue select/size、descriptor/available/used 地址、enable、notify 和 used-ring 更新；queue 未 enable 或 device 未 `DRIVER_OK` 时不得读取 descriptor；
- common、notify、ISR、device config 的访问宽度、对齐和越界检查；
- 未提供 MSI-X 时使用单个 PCI INTx：完成请求先设置 ISR queue bit 和 PCI interrupt-status，再按 resolved route 断言 level IRQ；读取 ISR 清零并撤销断言；
- PCI interrupt disable、queue notification suppression 和 reset 必须阻止不应产生的中断。

F2 的 INTx 是单个只读块设备所需的窄而完整路径，不代表已经完成任意 PCI endpoint、中断切换、MSI 或 MSI-X；这些泛化仍属于 F5。已观察到的 vPIC wire-mode warning 只有在 trace 证明其阻塞上述 INTx/Linux 数据盘链路时才前移为独立 fix，不能顺带塞入 virtio 提交。

#### 7.6 virtqueue、DMA 与块请求语义

virtqueue 和 virtio-blk core 放在可复用的 `no_std` 设备层，不依赖 Axvisor 文件系统。所有 guest 地址访问都经过显式 guest-memory capability，并满足：

- descriptor table、available ring、used ring、header、data 和 status byte 分别进行 checked range/overflow 校验；
- 拒绝 descriptor 环、越界 next、错误读写方向、重叠导致的别名风险、缺失 header/status、非 512 字节倍数和越过 capacity 的请求；
- F2 只接受 `VIRTIO_BLK_T_IN`；写请求在发布 `VIRTIO_BLK_F_RO` 时返回 `VIRTIO_BLK_S_IOERR`，未知请求返回 `VIRTIO_BLK_S_UNSUPP`；
- status/used ring 只有在请求结果确定后提交；DMA fault 不得报告成功，只有能够安全写入 status/used 时才提交 error completion，否则保持 queue 可 reset 并记录明确 fault；
- used length、sector 到 byte offset 的换算和多 descriptor 总长度使用 checked arithmetic；
- 初版同步处理 notify，不在持锁状态调用外部 I/O、调度器或其他设备；因此 reset 完成时不存在后台请求，queue/ISR/IRQ 能原子回到初始状态；
- 如果后续引入异步 backend，必须先独立设计 request ownership、取消、reset barrier 和 completion lifetime，不能扩展 F2 的同步实现假装支持。

#### 7.7 镜像后端和配置契约

用户只描述设备意图和后端，不提供资源数值。每个 VM 最多出现一次该 model，目标配置形状为：

```toml
[[devices.virtual]]
id = "readonly-data-disk"
model = "virtio-blk-pci"
backend = { type = "file", path = "/guest/axvisor-f2-data.ext4", read_only = true }
```

最终字段拼写沿用 `axvmconfig` 的 open typed options，不为 F2 建立专用顶层配置；options 使用 `deny_unknown_fields`。`type = "file"`、非空 `path` 和显式 `read_only = true` 都是必需项；`bdf`、`bar`、`mmio_base`、`irq`、`msi_vector` 等数值字段必须被拒绝。第二个 `virtio-blk-pci` 请求在规划期报错，并列出两个 device id。

x86 device planner 在调用通用 configured-device append 前消费这一个内建 machine model，并把它组合进 `pci-config` root node；它不能同时作为独立 catalog node 再构造一次。其他 configured devices 继续走现有 catalog。在非 x86 架构上，该 model 没有 machine owner，必须按 unsupported/unknown model 拒绝，不能创建一个无 transport 的通用 block node。

AxVM 只接收一个由 integration layer 提供的窄 read-only image-provider capability；provider 以 VM id、device id 和 path 为输入，返回冻结的 `Arc<[u8]>`。编译期 VM 配置由 `os/axvisor/build.rs` 解析 backend path、嵌入镜像并按 VM/device identity 提供；运行期配置只在启用 `fs` 时读取 Axvisor 文件系统中的绝对路径并一次性快照。静态镜像优先于 runtime filesystem；两者都不存在时返回 unsupported，不得静默忽略设备或退化为实时文件访问。

provider 在 VM resource planning 前检查存在性、非零长度、512 字节对齐和只读要求，再把不可变快照作为窄 `ReadOnlyBlockBackend` 交给设备。设备热路径不得持有 host path 或依赖 `ax_std::fs`；原文件在 VM 运行期间变化也不得改变 guest 看到的内容。缺失、空、未对齐、路径不合法或 `read_only = false` 时，VM 创建明确失败并带 VM id、device id 和 path 上下文。没有配置该 model 时不创建 image provider 请求，现有 fw_cfg/OVMF 行为保持不变。

#### 7.8 数据盘与 Linux 证据

测试资产固定为 8 MiB raw ext4 镜像，没有 MBR/GPT 或分区表，guest 直接挂载 `/dev/vda`。host 工具以固定 UUID、固定 `E2FSPROGS_FAKE_TIME`、固定文件 mode/mtime，并关闭 lazy inode/journal 初始化的 `mke2fs` 参数生成镜像；生成流程先验证所需 e2fsprogs 能力，再连续生成两次并要求 SHA-256 相同。生成物放在 `tmp/axbuild`，不提交二进制镜像。镜像只包含本测试需要的最小目录和 identity：

```text
/axvisor/F2-ID.txt = axvisor-f2-readonly-data-v1\n
```

参考 QEMU 使用 `disable-legacy=on` 的 `virtio-blk-pci` 和 read-only raw drive，把同一镜像作为 `/dev/vda` 挂载并读取 identity；这只证明资产和 Linux driver 兼容，不替代 Axvisor E2E。

direct-boot Linux initramfs 必须完成以下检查后，才向 guest console 先输出一行裸 marker `AXVISOR_X86_PCI_BLOCK_PASSED`、再输出 identity 键值行 `identity=axvisor-f2-readonly-data-v1`：

1. `/sys/bus/pci/devices/0000:00:03.0` 报告 `1af4:1042`，BAR0 已分配且 memory-space decode 已启用；
2. Linux 已绑定 `virtio_pci`/`virtio_blk`，从该 PCI function 的 sysfs `irq` 取得 Linux IRQ number，并在挂载前记录 `/proc/interrupts` completion count；
3. `/sys/block/vda/ro` 为 `1`，以 `dd` 对 `/dev/vda` 发起的 512-byte 写入确定失败；设备层另以最低层测试证明 `VIRTIO_BLK_T_OUT -> VIRTIO_BLK_S_IOERR`，不能把 Linux block-layer 的提前拒绝当作设备收到 OUT 的证据；
4. 以 `mount -t ext4 -o ro /dev/vda` 直接挂载 raw filesystem，读取并严格匹配 `F2-ID.txt`；
5. 挂载和读取后的同一 IRQ completion count 大于初始值，证明请求经过真实 INTx completion；
6. runner 在 VMX/SVM 运行前后计算源镜像 SHA-256，摘要必须不变，VMX/SVM backend 使用同一 digest。

内核、initramfs 和 cmdline 仍由已有 fw_cfg direct-boot 路径提供，因此该 marker 只声明“标准 PCI 只读数据盘可用”，不声明 OVMF 或 EFI 启动。F3 将复用同一个 `virtio-blk-pci` 产品模型和 backend 契约，不再实现第二套磁盘设备。

#### 7.9 实施单元与提交顺序

F2 是第一个可用 feature 阶段。PCI root-bus 重构、virtqueue core、x86 planner/image-provider 接线和测试资产都只是阶段内实施单元，单独完成时不能声明一个新 feature。以下顺序把行为保持重构、协议 core、设备 transport、machine plan、应用后端和 E2E 分开；每个提交都必须可构建、通过对应最低层测试，并在修改 Rust crate 时通过该 crate 的 targeted clippy：

1. **已完成** `docs(uefi): finalize the F2 read-only PCI disk contract`：只修改本文，固定单节点、单盘、raw ext4、资源、配置和证据；实现分支不重复携带 `plan/` 文档；
2. `refactor(axdevice): make x86 PCI functions table-driven`：只把当前 host bridge/LPC 的 BDF、config lookup 和 absent-function 行为改为 typed、表驱动表示，不增加 BAR aperture、INTx 或用户可见 endpoint；现有 config-space 行为测试前后等价；
3. `feat(axdevice): add one MMIO PCI function slot`：在 root bus 内加入唯一可选 function、4 KiB BAR probe/assignment/decode、PCI command/status 和 reset；使用 synthetic function 覆盖正常/非法访问，但不加入 VirtIO、guest-memory 或用户配置；
4. `feat(axdevice): add a read-only virtio block PCI function`：在同一提交中加入 queue 0 所需的 checked split virtqueue，并用它组成一个真实的只读 modern virtio-blk PCI function。queue core 实现 descriptor/ring 状态、checked guest-memory walker、单 outstanding chain、used completion 和 reset，覆盖环、bad next、indirect descriptor、错方向、metadata/data overlap、地址/长度溢出及 descriptor/available/used DMA fault；PCI function 实现 capability chain、feature/status 状态机、同步 `VIRTIO_BLK_T_IN`、`T_OUT=IOERR`、未知请求 `UNSUPP`、ISR 和 level INTx，并以 synthetic guest memory、immutable backend 和 IRQ sink 完成一次真实 read completion、notification suppression、reset 与 assert-read-deassert。guest-memory、backend 和 IRQ 调用必须发生在设备状态锁外。至此设备协议完整，但尚不接受用户 TOML；
5. `feat(axvm): plan an x86 virtio block PCI disk`：x86 planner 消费至多一个 `virtio-blk-pci` 请求，建立 typed BDF plan、4 KiB aperture、guest-memory grant 和 wired IRQ claim，并让 ACPI `_CRS`/`_PRT` 与 runtime 同源；使用 fake immutable image provider 做 graph/firmware/config 集成测试，不接 `ax_std::fs`；
6. `feat(axvisor): load immutable virtio block images`：实现编译期静态镜像和启用 `fs` 时的 runtime 文件快照，校验 path、非空、512 字节对齐和 `read_only=true`，把 provider 接入 VM 配置。该提交结束后用户配置形成可用产品 feature，缺失 provider 或非法镜像明确失败；
7. `test(axbuild): prepare a deterministic Axvisor block image`：生成 8 MiB raw ext4、固定 identity/metadata，验证连续两次生成 SHA-256 相同，并完成 reference-QEMU 资产兼容性检查；不修改产品设备语义；
8. `test(axvisor): read the PCI data disk from Linux`：加入 initramfs 检查、VMX/SVM case、INTx completion、只读写失败、identity 和前后 digest 证据。E2E 的 qemu TOML 沿用现有 x86_64 case 的 `uefi = true` + `to_bin = true`（x86_64 Axvisor hypervisor 是 PE/COFF EFI 应用，外层 QEMU 必须经 ostool UEFI pflash+ESP 流程启动，`uefi = false` 会让 ostool 回退 `-kernel` 直启而无法进入 long-mode）；“不依赖嵌套 OVMF”的落点是 guest 配置 `enable_bios = false`、`boot_protocol = direct`，以及 build TOML 不设置 `AXVISOR_TEST_X86_OVMF_OUTPUT`，而非关闭外层 UEFI 标志；
9. `docs(uefi): record F2 validation evidence`：在实现合入后回到 `plan` 分支，记录最终 head、数据盘 digest、命令、VMX/SVM backend 结果和已知限制，同步派生 PR 文档。

第 2 项可以作为独立、行为保持的前置 PR；第 3～6 项原则上组成同一个 feature PR，但保留上述提交边界供逐层审核和定位回归。checked split virtqueue 不再单独形成提交：在没有非测试产品调用方时，单独提交只能提前暴露闲置公共 API、留下 `dead_code` 或依靠 lint suppression；把它与第 4 项的首个真实 PCI/block 调用方合并，才能保持 crate 内最小可见性并让该提交独立通过 build、test 和 targeted clippy。第 6 项是首次暴露用户配置的提交，此前不得注册空 model、接受但忽略 backend，或声明磁盘可用。

第 4 项必须按 `virtio/split_queue`、`virtio/block` 和 `x86/virtio_block_pci` 的职责拆分文件，生产代码以约 1,500 行为上限；若实现前估算或实际改动超过该上限，应先简化到 F2 已声明的单 queue、同步、只读协议面，而不能重新拆出没有真实产品调用方的 queue-only 提交。其他提交若同时修改两个以上状态所有者，或必须依赖后续提交才能解释错误/资源生命周期，也应在写入代码前继续拆分并先修订本文。第 7、8 项只能增加资产和交付证据，不能借 E2E 之名补产品主语义；若 E2E 暴露标准协议缺口，必须回到拥有该语义的第 3～6 项增加最低层回归和修复。

#### 7.10 分层验证

| 层级 | 必须覆盖的成功路径 | 必须覆盖的失败/边界 |
| --- | --- | --- |
| PCI config/root bus | `00:03.0` 枚举、header/capability chain、BAR probe/assignment、command decode、reset | absent function 全一、越界 config、错位 BAR、aperture 外 assignment、第二个 disk function |
| virtio PCI transport | feature/status 顺序、queue 配置/enable/notify、ISR/INTx assert-read-deassert | 不支持 feature、错宽度/越界 capability、未 ready notify、reset 清理 |
| split virtqueue/DMA | 单 descriptor 和 scatter read、used-ring completion | descriptor 环、bad next、错方向、guest-memory fault、地址/长度溢出、缺 status |
| virtio block | capacity、512-byte sector、只读 IN 请求 | 越界 sector、非整扇区、OUT=IOERR、未知类型=UNSUPP、空/未对齐 backend |
| graph/firmware/config | 固定 typed BDF、自动 aperture/INTx、root `_CRS` 与 `_PRT` 同源、静态/runtime image provider | 用户数值资源、资源冲突、缺失/空/未对齐镜像、`read_only=false`、重复 model |
| 资产 | 两次生成 digest 相同，reference QEMU 直接挂载 raw ext4 并读取 identity | e2fsprogs 能力不符、digest 不一致、损坏 filesystem/identity、只读属性丢失 |
| Axvisor E2E | VMX/SVM 同一 guest TOML 命中 Linux 数据盘 marker | 未命中 marker 时保留首个失败、设备 trace 和资产摘要 |

Rust 改动执行 `cargo fmt`、受影响 crate 的 `cargo xtask clippy --package <crate>` 和最低层测试；Axvisor case 使用 `cargo xtask axvisor test`。E2E case 在 F6 前保持 non-gating，并继续回归 V 阶段的 `smoke-vmx`、`smoke-svm`、`ovmf-acpi-vmx` 和 `ovmf-acpi-svm`。

#### 7.11 退出条件

- [ ] 现有 direct-boot Linux 的 virtio PCI/block 和 ext4 驱动兼容性有可复核证据；
- [ ] Linux 枚举到 `1af4:1042` function，BAR 已分配并启用 memory decode；
- [ ] PCI aperture、typed BDF plan、INTx route、ACPI `_CRS`/`_PRT` 和 runtime 均能追溯到同一 machine/resolved plan；
- [ ] guest 通过只读 virtio-blk 直接挂载 raw ext4，并读取 identity 文件；
- [ ] guest 验证只读属性、写入失败和 INTx completion 增长，runner 证明镜像摘要不变，并输出含正确 identity 的专属 marker；
- [ ] PCI、transport、virtqueue、DMA、block、backend 和 reset 的最低层成功/错误测试全部通过；
- [ ] VMX/SVM 使用同一 guest 配置和同一数据盘 digest 通过；
- [ ] V 阶段四个 case 不退化，F2 case 保持 non-gating；
- [ ] 没有新增 legacy transport、磁盘写入、MSI/MSI-X、OVMF 启动、VARS 或 SMP 的隐式支持声明。

F2 尚不以 OVMF/EFI 启动、多 vCPU、VARS 持久化、多盘或任意 PCI 设备通用性为完成条件。任何一项核心退出条件失败时，不得以延长 timeout、放宽 regex、测试侧直接读取 host image 或私有 fw_cfg 块协议代替。

### 8. 阶段 F3：PCI 启动盘与 EFI 应用

#### 8.1 目标

复用 F2 已交付的 `virtio-blk-pci` 产品设备，让 OVMF 从 partitioned FAT ESP 加载一个确定性的 EFI 应用。F3 不再修改 PCI、virtqueue 或 block 协议来制造第二条启动专用路径；若 OVMF 暴露 F2 未覆盖的标准访问序列，应先为同一设备补最低层回归。

```text
F2 的只读 virtio-blk-pci
  -> OVMF VirtioPciDeviceDxe / VirtioBlkDxe
  -> Block I/O -> partition -> FAT
  -> \\EFI\\BOOT\\BOOTX64.EFI
  -> 应用读取同一 ESP 的 \\AXVISOR\\F3-ID.TXT
  -> AXVISOR_X86_UEFI_PCI_APP_PASSED
```

完成 F3 后，用户可以配置标准只读 guest PCI 启动盘，并通过 removable-media fallback 启动 EFI payload；不需要 Boot#### 持久化、Linux EFI stub 或手工进入 OVMF 菜单。

#### 8.2 进入条件和兼容性确认

- F2 的 VMX/SVM 数据盘用例和 V 阶段四个基线用例全部通过；
- 固定本阶段使用的 Ostool OVMF 路径、布局和 digest，并从对应 EDK2 build/module 信息确认包含 `VirtioPciDeviceDxe`、`VirtioBlkDxe`、partition、FAT 和 removable-media boot 支持；
- 用同一 OVMF 在参考 QEMU 上启动候选 ESP，证明分区、FAT、`BOOTX64.EFI` 和 identity 文件自身有效；
- 固定 UEFI 2.10 为固件接口依据，并记录 OVMF 的实际 PCI/virtio 访问序列；该序列只能发现标准兼容性缺口，不能成为偏离 VirtIO 1.2 的理由。

若固定 OVMF 不包含所需驱动，不得静默退回 legacy virtio、私有 fw_cfg 磁盘或把额外块驱动塞入 ESP。必须先修订本文并重新评审“更换受管 OVMF”或“选择另一种标准块设备”。

#### 8.3 范围和证据

- ESP 使用可重复生成的 GPT 或 MBR partitioned FAT；记录生成工具、输入文件清单、镜像尺寸和 digest；
- ESP 至少包含 `\\EFI\\BOOT\\BOOTX64.EFI` 和 `\\AXVISOR\\F3-ID.TXT`；
- EFI 应用通过 `LoadedImage` device handle 打开同一 Simple File System，读取并校验 identity，再向 guest console 输出包含该 identity 的 marker；
- VMX/SVM 共用同一 guest TOML、OVMF 和 ESP digest；
- 保留 F2 direct-boot Linux 数据盘用例，固件启动不能成为设备协议的唯一证据。

F3 不包含 Linux EFI stub、VARS 持久化、磁盘写入、MSI/MSI-X、多队列或多 vCPU。除修复 OVMF 首次暴露且符合规范的同设备缺口外，F3 的增量应主要位于 boot policy、测试资产、用户配置示例和 runner 接线；不得重新实现 PCI root bus 或 virtio block core。若 F2 的标准设备在固定 OVMF 下无需产品代码修改即可完成本阶段链路，F3 应如实标为集成交付，不得为了把它包装成“新增 feature”而制造启动开关、专用协议或空公共接口。

#### 8.4 退出条件

- [ ] 固定 OVMF 的 virtio PCI/block、partition、FAT 和 removable-media boot 兼容性有可复核证据；
- [ ] OVMF 枚举 F2 的 `1af4:1042` function，并从同一只读设备读取 ESP；
- [ ] EFI 应用由 `\\EFI\\BOOT\\BOOTX64.EFI` 自动加载，不依赖持久化 Boot#### 或人工菜单；
- [ ] EFI 应用从自身启动设备读取正确 identity，并输出 `AXVISOR_X86_UEFI_PCI_APP_PASSED`；
- [ ] VMX/SVM 使用同一 guest 配置和同一 OVMF/ESP digest 通过；
- [ ] F2 数据盘用例和 V 阶段四个 case 不退化，F3 case 保持 non-gating；
- [ ] 没有引入启动专用设备旁路、legacy transport、Linux、VARS、磁盘写入或 SMP 的隐式支持声明。

### 9. 阶段 F4：Linux EFI 启动与 VARS 生命周期

#### 9.1 目标

把当前通过 fw_cfg 提供 kernel/initrd 的 Linux 证据推进为标准 guest 启动盘路径：OVMF 从 ESP 加载 Linux EFI stub，Linux 进入 initramfs 或 shell。同时明确嵌套 OVMF VARS 的创建、隔离、重启和清理语义。

#### 9.2 设计边界

- Linux kernel、initrd 和 cmdline 的来源必须在测试配置中显式；
- 每个 case 使用隔离的 VARS，不得修改共享缓存模板；
- 若声明持久化，必须定义 VM 重启、测试重跑和失败清理后的状态；
- EFI stub 与 initramfs marker 必须来自 guest console；
- 保留 F2 的 direct-boot Linux 数据盘用例和 F3 的独立 EFI 应用用例，Linux EFI 启动不能成为唯一的设备协议或固件证据。

#### 9.3 退出条件

- OVMF 从 guest ESP 加载 Linux EFI stub；
- Linux 到达确定的 initramfs/shell marker；
- guest ACPI、PCI block 和中断断言通过；
- VARS 隔离和约定的持久化/非持久化行为有自动测试；
- VMX/SVM 使用同一 guest baseline 通过。

### 10. 阶段 F5：中断完整性与 SMP

#### 10.1 范围

- 在 F2 数据盘和 F3 启动盘的单设备 INTx 基线上，完成任意 PCI endpoint 的 controller-owned INTx 路由并增加 MSI/MSI-X；
- 实现 INIT/SIPI，保持 firmware APIC ID 与逻辑 vCPU ID 映射明确；
- 实现 NMI，或对尚不支持的调用返回明确错误，不能继续静默忽略；
- level IRQ 的 pending/active/route/EOI 状态只由 PIC/IOAPIC/LAPIC 所有；
- 使用至少两个 vCPU 的 UEFI guest 验证 AP 启动、timer、IPI 和设备中断。

#### 10.2 退出条件

- 两个及以上 vCPU 被 Linux 报告为 online；
- INIT/SIPI 顺序、重复 SIPI、无效 APIC ID 和 reset 有确定回归；
- 启动设备的 INTx 回归保持通过，并至少用一个实际设备覆盖 MSI/MSI-X 新路径；
- VMX/SVM 的 SMP guest 均通过；
- 单 vCPU F2/F3/F4 用例不退化。

### 11. 阶段 F6：稳定性与 CI 门禁

#### 11.1 进入条件

- F2/F3/F4 的 VMX/SVM 单 vCPU 用例稳定；
- F5 的多 vCPU 用例达到约定稳定性；
- 固件获取、guest 资产准备和结果归因完全自动化；
- 不依赖人工日志判断、超长跨行 regex 或扩大 timeout 掩盖波动。

#### 11.2 交付

- 为 Intel/VMX 与 AMD/SVM 保留独立 self-hosted case；
- 两者使用同一 backend-neutral guest baseline；
- 固件 digest、guest 镜像标识和成功 marker 可从 CI 日志复核；
- `cache_key` 在 self-hosted CI matrix 中保持空字符串；
- 记录重复运行次数、失败率和已知宿主限制后再启用 gating。

### 12. 跨阶段验证矩阵

| 声明或风险 | 最低层验证 | 运行验证 | 首次要求阶段 |
| --- | --- | --- | --- |
| OVMF 输入与实际加载镜像一致 | 资产路径、尺寸、digest 和组装测试 | VMX/SVM 运行摘要 | V |
| fw_cfg/table-loader 错误显式 | axdevice 成功、descriptor/data fault 与回写失败测试 | guest ACPI 断言 | V |
| resolved graph 一致性 | resource planning/firmware plan 测试 | guest tty/IOAPIC/ACPI | V |
| 当前 OVMF/Linux 链路可复现 | runner discovery/build 测试 | `AXVISOR_X86_OVMF_ACPI_PASSED` | V |
| 已确认平台缺口被正确修复 | 修复前失败回归 | V 阶段全量复跑 | F1（条件） |
| PCI 只读数据盘协议完整 | config/BAR/DMA/reset/error 测试 | direct-boot Linux 数据盘 marker | F2 |
| OVMF 从 PCI ESP 启动 EFI 应用 | OVMF/ESP 资产与同设备回归 | EFI 应用 marker | F3 |
| Linux 从 guest ESP 启动 | EFI/磁盘/VARS 测试 | Linux initramfs/shell marker | F4 |
| 中断与 SMP 完整 | APIC/INIT/SIPI/IRQ 状态测试 | 多 vCPU Linux marker | F5 |
| backend 与稳定性 | 同一 guest config 校验 | VMX/SVM 重复与 CI | F6 |

每次修改 Rust 逻辑仍须运行 `cargo fmt`、目标 `cargo xtask clippy --package <crate>` 和对应最低层测试；Axvisor 构建与运行优先使用 `cargo xtask`。

### 13. 错误、资源与回滚规则

- 固件和设备图在 VM 可运行前完成规划与 build，不在 vCPU 热路径动态注册设备；
- fw_cfg selector/DMA 状态使用既有非睡眠同步，锁内不执行外部 I/O 或回调其他设备；
- PIO string I/O 逐元素提交，失败时不得提前更新 RIP、索引或计数；
- 未注册设备访问返回明确未命中/错误；只有 PCI 不存在 function 使用规范要求的全一返回；
- 固件窗口、RAM、ACPI image、PCI BAR 和 device hole 由单一规划负责并检查溢出/重叠；
- 测试资产必须隔离、可清理；共享 OVMF VARS 模板不得被原地修改；
- 每个 feature 阶段可以独立回滚，回滚时同步移除配置、runner 接线、测试和文档声明；
- V/F2/F3/F4/F5 的新增用例保持 non-gating，F6 之前不改变现有 CI 默认行为。

### 14. 派生文档规则

后续维护 `pr1.md`、`pr2.md` 或新的 PR 计划时：

1. 先声明它对应本文的 V、F1、F2、F3、F4、F5 或 F6；
2. 只展开该阶段的提交拆分、负责人、运行记录和 PR 文案；
3. 不重新定义目标、现状、证据强度、架构不变量或跨阶段边界；
4. 如果实施发现本文事实错误，先以当前 `dev` 证据修订本文，再更新派生文档；
5. 历史分支提交只能列为调查线索，不能列为“待搬运实现”或当前能力证据。
