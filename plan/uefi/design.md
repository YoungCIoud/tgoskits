# Axvisor x86_64 Guest UEFI 设计

> 事实基线：`dev@343d47f4e`（2026-08-07，包含 PR #1718 的解析后设备图、PR #1917 的 Ostool OVMF 复用和 PR #1918 的 fw_cfg DMA fault 修复）。
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

Axvisor x86_64 已经具备 guest UEFI 路径的大部分基础构件，也已经存在一条 OVMF 启动 Linux initramfs 并检查 ACPI 的 VMX 用例，但尚未形成与最终支持声明相匹配的完整证据链。当前需要明确区分：

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

| 范围 | `dev@343d47f4e` 的所有者与实现 | 能够确认的事实 |
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
| OVMF 资产 | `support::ovmf::OvmfFirmware` 复用 Ostool 资产，`axvisor/test/ovmf.rs` 组装 4 MiB x86 镜像 | 已有受管理 OVMF 获取与嵌套镜像准备路径 |
| 嵌套 OVMF ACPI 用例 | `test-suit/axvisor/normal/qemu-acpi-ovmf` | 已有 VMX 用例通过 OVMF 启动 Linux initramfs 并执行 guest-side ACPI 断言 |
| x86 SMP 固件启动 | `x86_vlapic::inject_nmi` 和 `process_init_sipi` | NMI、INIT/SIPI 仍被明确忽略，不能声明多 vCPU UEFI guest |

#### 2.2 当前自动证据

现有 `qemu-acpi-ovmf` 用例比“首次 fw_cfg 访问”更强。它完成以下链路：

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

该 marker 来自嵌套 Linux initramfs，不可能由 QEMU 外层 OVMF 直接产生，因此已经能够证明 VMX 路径上的嵌套固件启动、Linux 进入早期用户态以及 ACPI 被 Linux 接受。它也意味着固件已经越过 BDS 并完成 kernel handoff，但当前没有独立、短窗口的 BDS marker。

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

`AXVISOR_X86_OVMF_ACPI_PASSED` 属于 E5，并包含一组 E4 的 ACPI 断言；当前缺口主要是证据来源摘要、SVM 对称用例、重复性和 CI，而不是重新发明 E3 marker。

#### 2.4 当前可以声明

- x86 guest 资源由 resolved graph 统一规划；
- x86 已有 fw_cfg PIO/DMA、ACPI、legacy 平台设备、最小 PCI 配置和 REP string I/O；
- Ostool OVMF 可以被组装为嵌套 guest 固件镜像；
- 现有 VMX `qemu-acpi-ovmf` 用例能够通过 OVMF 启动 Linux initramfs；
- 该 VMX guest 能读取 DSDT/APIC/FACP/SPCR，并观察到 ttyS0 与 IOAPIC；
- normal `smoke-vmx`/`smoke-svm` 分别验证宿主 VMX/SVM 基线，但不等同于两种 backend 都完成嵌套 UEFI 验证。

#### 2.5 当前不能声明

- SVM 已达到与 VMX `qemu-acpi-ovmf` 相同的 guest-side 证据；
- 每次运行都记录了可复核的 OVMF digest，并证明实际加载镜像与摘要一致；
- 独立 BDS marker 已建立；现有 Linux marker 只能作为越过 BDS 的更强端到端证据；
- OVMF VARS 在嵌套 guest 重启后持久化；
- OVMF 已枚举 Axvisor 提供的标准 PCI guest 启动盘；
- EFI 应用从 guest ESP 被加载；
- Linux 是从 guest PCI 启动盘沿 EFI stub 启动，而不是当前 fw_cfg kernel/initrd payload；
- guest PCI INTx/MSI/MSI-X 完整可用；
- INIT/SIPI、NMI 或多 vCPU UEFI guest 可用；
- UEFI 路径已经进入 CI gating。

#### 2.6 未验证与确认缺失必须分开

| 类别 | 项目 | 后续处理 |
| --- | --- | --- |
| 已有但需补证据 | OVMF 资产摘要、VMX 当前用例复现、SVM 对称验证、BDS/guest 归因说明 | 进入“验证阶段”，不增加产品 feature |
| 已确认缺失 | guest PCI 启动设备、ESP/EFI 应用加载、VARS 持久化、INIT/SIPI/NMI、完整 guest MSI 链 | 进入后续 feature 阶段 |
| 可能缺失 | SVM 或重复运行中暴露的 MSR/PIO/MMIO/table-loader 语义 | 先由验证阶段定位；确认后进入独立 feature/fix 阶段 |

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
| 先统一验证当前 `dev`，再按已确认缺口逐阶段加 feature | 采用 | 证据与能力边界清晰，每个 feature 可独立设计、回归和审核 |

## 第二部分：分阶段开发与验证

### 4. 阶段总览

| 阶段 | 性质 | 目标 | 是否增加产品 feature |
| --- | --- | --- | --- |
| V：现状验证 | 验证 | 固定并复核 `dev` 已有 OVMF -> Linux/ACPI 链路，补齐 VMX/SVM 证据 | 否 |
| F1：固件平台缺口 | 条件 feature/fix | 修复 V 阶段确认且阻塞现有链路的最小语义缺口 | 是；无缺口则跳过 |
| F2：PCI 启动设备与 EFI 应用 | feature | 让 OVMF 从 Axvisor guest PCI 设备读取 ESP 并运行 EFI 应用 | 是 |
| F3：Linux EFI 启动与 VARS | feature | 从 guest 启动盘启动 Linux EFI stub，并定义 VARS 生命周期 | 是 |
| F4：中断完整性与 SMP | feature | 完成 guest INTx/MSI/MSI-X、INIT/SIPI/NMI 和多 vCPU UEFI | 是 |
| F5：稳定性与 CI 门禁 | 交付 | VMX/SVM 稳定重复并进入 CI gating | 不新增启动语义 |

阶段必须按顺序推进。前一阶段的退出证据是后一阶段的输入；不得以“后续会补”绕过当前阶段的核心失败路径。

### 5. 阶段 V：验证当前实现

#### 5.1 目标

在不改变产品运行语义的前提下，复核当前 VMX `qemu-acpi-ovmf`，建立同一 guest baseline 的 SVM 证据，并把固件来源、嵌套归因、ACPI 断言和运行限制记录成可重复验证链。

原设计中的“固定固件与 fw_cfg marker”和“证明 ACPI/BDS”合并在本阶段。fw_cfg 访问只是调查检查点，不能单独成为阶段完成条件；现有 guest Linux/ACPI marker 是更强的最终证据。

#### 5.2 实施顺序

1. 从最新 `dev` 运行并列出 `normal` 组，确认 `qemu-acpi-ovmf` 的 VMX case 被 runner 发现；
2. 使用现有 Ostool 获取路径准备 x86_64 OVMF，记录实际 CODE/VARS 路径、尺寸和 digest；
3. 证明 runner 生成的 4 MiB 镜像就是 Axvisor guest loader 实际使用的镜像；
4. 运行现有 VMX `ovmf-acpi`，保存完整命令、固件摘要和 guest success marker；
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
  --test-case ovmf-acpi
```

SVM case 的最终名称由 test-suit 结构确定，但必须与 VMX 共用 backend-neutral guest 配置，不能通过 Cargo feature 选择 backend。

#### 5.4 退出条件

- [ ] 当前 head 的 VMX case 命中 `AXVISOR_X86_OVMF_ACPI_PASSED`；
- [ ] 当前 head 的 SVM 对称 case 在 AMD KVM 宿主命中同一 guest marker；
- [ ] 两次运行记录实际 OVMF 路径、尺寸和 digest；
- [ ] guest 使用的固件镜像与记录摘要一致；
- [ ] DSDT/APIC/FACP/SPCR、CPU、ttyS0 和 IOAPIC 断言全部由 guest 执行；
- [ ] table-loader 和 fw_cfg DMA 的最低层成功/错误测试通过；
- [ ] normal VMX/SVM smoke 不退化；
- [ ] diff 中没有新增产品配置、公共 API 或固件/设备运行语义；
- [ ] 用例保持 non-gating，直到 F5 的稳定性标准满足。

#### 5.5 失败处理

若 VMX 或 SVM 在当前 `dev` 无法达到 guest marker：

1. 保存固定资产摘要、完整命令、最后可靠状态和第一个错误；
2. 形成最低层、能稳定复现该错误的测试草案，在 F1 的首个提交中先证明修复前失败；
3. 明确缺口的职责所有者和外部规范依据；
4. 把修复作为 F1 的独立设计与提交；
5. F1 完成后重新执行 V 阶段全部验证。

### 6. 阶段 F1：修复验证确认的固件平台缺口

#### 6.1 进入条件

只有 V 阶段在固定输入下证明现有实现存在确定语义缺口时才进入本阶段。如果 VMX/SVM 均通过 V 阶段，本阶段直接标记为不需要，不为历史分支中的候选改动寻找理由。

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

### 7. 阶段 F2：PCI 启动设备与 EFI 应用

#### 7.1 目标

让 OVMF 枚举由 Axvisor 提供的标准 guest PCI 启动设备，从 guest ESP 读取并运行一个确定性的 EFI 应用。该阶段首次增加面向 guest 的启动设备 feature。

本阶段的最小垂直链路是：

```text
guest TOML 中的只读磁盘后端
  -> Axvisor 在 VM build 前读取并冻结镜像
  -> resolved graph 中的 PCI root bus、function、BAR aperture 和 INTx route
  -> modern virtio-blk PCI transport
  -> OVMF VirtioPciDeviceDxe / VirtioBlkDxe
  -> Block I/O -> FAT -> \\EFI\\BOOT\\BOOTX64.EFI
  -> AXVISOR_X86_UEFI_PCI_APP_PASSED
```

成功 marker 必须由 ESP 中的 EFI 应用写入 guest console；仅看到 PCI vendor/device、BAR 访问或 OVMF 菜单均不能完成 F2。

#### 7.2 进入条件和兼容性确认

开始修改产品代码前必须满足：

1. V 阶段的 VMX/SVM `ovmf-acpi` 和 normal smoke 均通过；F1 若无确定缺口则保持跳过；
2. 固定本阶段使用的 Ostool OVMF 路径和 digest，并从对应 EDK2 build/module 信息确认包含 `VirtioPciDeviceDxe`、`VirtioBlkDxe`、partition、FAT 和 removable-media boot 支持；
3. 用同一 OVMF 在参考 QEMU 上启动候选 ESP，证明镜像、FAT 和 `BOOTX64.EFI` 自身有效；
4. 固定 VirtIO 1.2 和 UEFI 2.10 为协议依据，把 OVMF 的实际访问序列作为兼容性证据，而不是改变规范语义的理由；
5. 确认 guest PCI MMIO aperture 不与 RAM、firmware、IOAPIC/LAPIC、fw_cfg 或其他 resolved resources 重叠。

若固定 OVMF 不包含上述驱动，不得静默退回私有 fw_cfg 磁盘、legacy virtio 或把驱动塞入 ESP。应先更新本文，重新评审“更换受管 OVMF”或“选择另一种标准块设备”两种方案。

#### 7.3 方案选择和非目标

| 方案 | 结论 | 原因 |
| --- | --- | --- |
| modern non-transitional virtio-blk PCI | 采用 | 标准接口、OVMF 有现成驱动；只需实现本阶段实际使用的最小协议面 |
| transitional/legacy virtio-blk | 拒绝 | 会同时引入 legacy PIO、旧 feature 协商和两套 transport 语义 |
| fw_cfg 私有块设备或直接传 EFI 文件 | 拒绝 | 不能证明标准 PCI 枚举、Block I/O、partition 和 FAT 启动链 |
| AHCI、NVMe 或宿主块设备直通 | 拒绝 | 控制器和安全边界远大于本阶段；宿主 NVMe rootfs 不是 guest 设备 |
| 可写或宿主文件实时后端 | 推迟 | F2 只需要确定性读取；写入、一致性、flush 和故障恢复进入独立设计 |

F2 明确不包含：Linux EFI stub、VARS 持久化、多队列、packed virtqueue、indirect descriptor、event index、MSI/MSI-X、热插拔、IOMMU、磁盘写入和多 vCPU。这些能力不得以未声明 feature bit、空实现或伪成功形式出现。

#### 7.4 PCI root bus 与资源所有权

当前 `X86PciConfigDevice` 只有 bus 0 上固定的 host bridge 和 LPC bridge，不能直接承载真实 endpoint；尤其不能把 resolved graph 中的固定 MMIO range 当成可由 OVMF 任意重编程的 BAR。F2 必须在现有设备上扩展为一个 PCI root-bus owner：

- 保留配置机制 #1 的 `0xcf8..=0xcff` 唯一入口，不增加 ECAM/MCFG 或第二条 config 路径；
- root bus 从 resolved graph 领取一个专用 PCI MMIO aperture，并作为该 aperture 在 `DeviceRuntime` 中的唯一 claim owner；
- endpoint 在规划期声明 BAR 类型、大小和对齐，root bus 在运行时根据 PCI command/BAR 状态在 aperture 内完成 decode；因此 OVMF 的 size probe、disable、assignment 和 relocation 不需要修改 sealed runtime 路由；
- aperture 基址/范围同时生成 ACPI `PCI0._CRS`；不得继续让 AML 使用与 runtime 无关的硬编码窗口；
- endpoint 的 BDF 由 machine planner 分配为 typed identity，避开 `00:00.0` 和 `00:1f.0`，不允许用户在 TOML 指定 bus/device/function；
- function、BAR specification、INTx pin/route 和 firmware binding 属于同一个 resolved device node；ACPI `_PRT` 从该 node 和既有 swizzle 规则生成；
- runtime build 依赖 root-bus node，所有 function 在 VM 进入 runnable 状态前注册并 seal；vCPU 热路径不得注册、移除或替换 function；
- 不存在的 BDF/config offset 按 PCI 规范返回全一；非法 BAR assignment、aperture 外访问和未开启 memory-space decode 返回明确未命中或保持规范定义的寄存器状态，不能路由到其他设备。

PCI config 最低要求包括 conventional header、command/status、capability list、BAR size probe/assignment、interrupt pin/line 和 reset。BAR 的当前值是 PCI function 的运行状态；aperture、BDF、BAR 约束和中断路由仍来自 resolved graph，二者不能维护重复地址事实。

#### 7.5 modern virtio-blk PCI 协议面

设备使用 VirtIO 1.2 non-transitional PCI 身份：vendor `0x1af4`、virtio block device ID `2` 对应 PCI device `0x1042`，revision 不使用 legacy 值。一个 MMIO BAR 至少承载：

- common configuration capability；
- notification capability；
- ISR status capability；
- block device-specific configuration capability；
- 规范要求的 PCI configuration access capability。

本阶段只提供一个 split virtqueue。设备只发布已经完整实现的 feature：`VIRTIO_F_VERSION_1`、`VIRTIO_BLK_F_RO`，以及实现和 OVMF 证明确有需要时的 `VIRTIO_BLK_F_BLK_SIZE`。不发布 MSI-X、packed ring、ring reset、indirect descriptor、event index、flush、discard、write-zeroes、multi-queue 或 topology feature。

transport 必须完整实现本阶段可达的状态：

- device/driver feature select、`ACKNOWLEDGE -> DRIVER -> FEATURES_OK -> DRIVER_OK` 和 `FAILED`；
- 未提供 feature 的协商不能保留 `FEATURES_OK`；`device_status = 0` 同步恢复 transport、queue、ISR 和 block request 状态；
- queue select/size、descriptor/available/used 地址、enable、notify 和 used-ring 更新；queue 未 enable 或 device 未 `DRIVER_OK` 时不得读取 descriptor；
- common、notify、ISR、device config 的访问宽度、对齐和越界检查；
- 未提供 MSI-X 时使用单个 PCI INTx：完成请求先设置 ISR queue bit 和 PCI interrupt-status，再按 resolved route 断言 level IRQ；读取 ISR 清零并撤销断言；
- PCI interrupt disable、queue notification suppression 和 reset 必须阻止不应产生的中断。

F2 的 INTx 是启动盘所需的窄而完整路径，不代表已经完成任意 PCI endpoint、中断切换、MSI 或 MSI-X；这些泛化仍属于 F4。已观察到的 vPIC wire-mode warning 只有在 trace 证明其阻塞上述 INTx/OVMF 链路时才前移为独立 fix，不能顺带塞入 virtio 提交。

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

用户只描述设备意图和后端，不提供资源数值。目标配置形状为：

```toml
[[devices.virtual]]
id = "uefi-boot-disk"
model = "virtio-blk-pci"
backend = { type = "file", path = "/images/uefi-esp.img", read_only = true }
```

最终字段拼写必须沿用 `axvmconfig` 的 open typed options；不得为了 F2 再建立专用顶层配置。`bdf`、`bar`、`mmio_base`、`irq`、`msi_vector` 等数值字段必须被拒绝。

Axvisor integration layer 在 VM build 前解析并读取文件，检查存在性、非零长度、512 字节对齐和只读要求，然后把不可变内存快照作为窄 `BlockBackend` capability 交给设备。设备热路径不得访问 host path 或 `ax_std::fs`；原文件在 VM 运行期间变化也不得改变 guest 看到的内容。配置了缺失、空、未对齐或 `read_only = false` 的镜像时，VM build 明确失败并带 device id/path 上下文。没有配置该 device 时，现有 fw_cfg/OVMF case 保持原行为。

#### 7.8 ESP 与 EFI 应用证据

测试资产使用可重复生成的 GPT 或 MBR partitioned FAT ESP；分区方案一经选择必须记录生成工具、输入文件清单、镜像尺寸和 digest。ESP 中至少包含：

```text
\\EFI\\BOOT\\BOOTX64.EFI
\\AXVISOR\\F2-ID.TXT
```

EFI 应用通过其 `LoadedImage` device handle 打开同一 Simple File System，读取固定 identity 文件并校验内容，然后向 guest console 输出包含该 identity 的 `AXVISOR_X86_UEFI_PCI_APP_PASSED`。这样成功同时证明：

1. OVMF 枚举 PCI function 并绑定 virtio PCI/block 驱动；
2. Block I/O、partition 和 FAT 路径可用；
3. 应用来自 guest ESP，而不是 fw_cfg kernel/initrd 或外层 QEMU OVMF；
4. 应用还能从自己的启动设备继续读取数据。

使用 UEFI removable-media fallback `\\EFI\\BOOT\\BOOTX64.EFI`，F2 不依赖可持久化 Boot#### 或共享 VARS。EFI 应用保持最小，不引入 shell、网络、Linux、ACPI 全量探测或历史 `uefi_probe` 的大范围代码。

#### 7.9 实施单元与提交顺序

F2 是一个 feature 阶段，可以拆成多个可独立审核的 PR，但只有完成最后一个垂直链路 PR 后才能声明 F2 完成。推荐按以下顺序提交，每个提交都必须编译并通过当时适用的测试：

1. `docs(uefi): define the F2 PCI boot-device contract`：只修改本文，固定选择、非目标、接口和证据；
2. `refactor(axdevice): generalize the x86 PCI root bus`：在不增加用户可见设备的前提下，把固定 config state 重整为 sealed function registry、BAR aperture/decode 抽象和 typed BDF；新接口先保持 crate-private，现有 machine plan 暂不启用 endpoint/aperture，并以 host/LPC 与 ACPI 行为测试证明等价；
3. `feat(axdevice): add a read-only virtio block PCI device`：加入 modern transport、单 split queue、同步只读 block core、INTx 和最低层成功/错误测试；不接 host 文件路径；
4. `feat(axvisor): register the virtio block PCI model`：接入 catalog、resolved graph、ACPI root aperture/`_PRT` 和 VM-build-time immutable file backend，并增加配置/资源集成测试；
5. `test(axvisor): boot an EFI app from the guest PCI disk`：加入可重复 ESP/EFI app 资产、VMX/SVM case、reference-QEMU 资产检查和 guest marker；
6. `docs(uefi): record F2 validation evidence`：记录最终 head、OVMF/ESP digest、命令、两种 backend 结果和已知限制，同步派生 PR 文档。

第 2 项可作为独立、行为保持的前置 PR；第 3、4 项不得分别合入为对用户可配置但不可启动的半成品，原则上放在同一个 feature PR 中按 commit 审核。不得先暴露空模型、未使用公共 trait 或声称支持但只能枚举的设备。

#### 7.10 分层验证

| 层级 | 必须覆盖的成功路径 | 必须覆盖的失败/边界 |
| --- | --- | --- |
| PCI config/root bus | BDF 枚举、header/capability chain、BAR probe/assignment、command decode、reset | absent function 全一、越界 config、错位 BAR、aperture 外 assignment、重叠 function |
| virtio PCI transport | feature/status 顺序、queue 配置/enable/notify、ISR/INTx assert-read-deassert | 不支持 feature、错宽度/越界 capability、未 ready notify、reset 清理 |
| split virtqueue/DMA | 单 descriptor 和 scatter read、used-ring completion | descriptor 环、bad next、错方向、guest-memory fault、地址/长度溢出、缺 status |
| virtio block | capacity、512-byte sector、只读 IN 请求 | 越界 sector、非整扇区、OUT=IOERR、未知类型=UNSUPP、空/未对齐 backend |
| graph/firmware/config | 自动 BDF/aperture/INTx、root `_CRS` 与 `_PRT` 同源、catalog build | 用户数值资源、资源冲突、缺失镜像、重复 id/model |
| 资产 | reference QEMU 加载 `BOOTX64.EFI` 并读取 identity | digest 不一致、缺 fallback 文件、损坏 FAT/identity |
| Axvisor E2E | VMX/SVM 同一 guest TOML 命中 EFI marker | 未命中 marker 时保留首个失败、设备 trace 和资产摘要 |

Rust 改动执行 `cargo fmt`、受影响 crate 的 `cargo xtask clippy --package <crate>` 和最低层测试；Axvisor case 使用 `cargo xtask axvisor test`。E2E case 在 F5 前保持 non-gating，并继续回归 V 阶段的 `smoke-vmx`、`smoke-svm`、`ovmf-acpi-vmx` 和 `ovmf-acpi-svm`。

#### 7.11 退出条件

- [ ] 固定 OVMF 的 virtio PCI/block、partition、FAT 驱动兼容性有可复核证据；
- [ ] OVMF 枚举到 `1af4:1042` function，完成 BAR probe/assignment 并启用 memory decode；
- [ ] PCI aperture、BDF、INTx route、ACPI `_CRS`/`_PRT` 和 runtime 均能追溯到同一 resolved plan；
- [ ] guest 通过只读 virtio-blk 读取 partition、FAT、EFI 应用和 identity 文件；
- [ ] EFI 应用输出含正确 identity 的 guest 专属 marker；
- [ ] PCI、transport、virtqueue、DMA、block、backend 和 reset 的最低层成功/错误测试全部通过；
- [ ] VMX/SVM 使用同一 guest 配置和同一 OVMF/ESP digest 通过；
- [ ] V 阶段四个 case 不退化，F2 case 保持 non-gating；
- [ ] 没有新增 legacy transport、磁盘写入、MSI/MSI-X、Linux、VARS 或 SMP 的隐式支持声明。

F2 尚不以 Linux shell、多 vCPU、VARS 持久化或任意 PCI 设备通用性为完成条件。任何一项核心退出条件失败时，不得以延长 timeout、放宽 regex、手工选择 OVMF 菜单或改用 fw_cfg payload 代替。

### 8. 阶段 F3：Linux EFI 启动与 VARS 生命周期

#### 8.1 目标

把当前通过 fw_cfg 提供 kernel/initrd 的 Linux 证据推进为标准 guest 启动盘路径：OVMF 从 ESP 加载 Linux EFI stub，Linux 进入 initramfs 或 shell。同时明确嵌套 OVMF VARS 的创建、隔离、重启和清理语义。

#### 8.2 设计边界

- Linux kernel、initrd 和 cmdline 的来源必须在测试配置中显式；
- 每个 case 使用隔离的 VARS，不得修改共享缓存模板；
- 若声明持久化，必须定义 VM 重启、测试重跑和失败清理后的状态；
- EFI stub 与 initramfs marker 必须来自 guest console；
- 保留 F2 的独立 EFI 应用用例，Linux 不能成为唯一的设备协议测试。

#### 8.3 退出条件

- OVMF 从 guest ESP 加载 Linux EFI stub；
- Linux 到达确定的 initramfs/shell marker；
- guest ACPI、PCI block 和中断断言通过；
- VARS 隔离和约定的持久化/非持久化行为有自动测试；
- VMX/SVM 使用同一 guest baseline 通过。

### 9. 阶段 F4：中断完整性与 SMP

#### 9.1 范围

- 在 F2 启动盘的单设备 INTx 基线上，完成任意 PCI endpoint 的 controller-owned INTx 路由并增加 MSI/MSI-X；
- 实现 INIT/SIPI，保持 firmware APIC ID 与逻辑 vCPU ID 映射明确；
- 实现 NMI，或对尚不支持的调用返回明确错误，不能继续静默忽略；
- level IRQ 的 pending/active/route/EOI 状态只由 PIC/IOAPIC/LAPIC 所有；
- 使用至少两个 vCPU 的 UEFI guest 验证 AP 启动、timer、IPI 和设备中断。

#### 9.2 退出条件

- 两个及以上 vCPU 被 Linux 报告为 online；
- INIT/SIPI 顺序、重复 SIPI、无效 APIC ID 和 reset 有确定回归；
- 启动设备的 INTx 回归保持通过，并至少用一个实际设备覆盖 MSI/MSI-X 新路径；
- VMX/SVM 的 SMP guest 均通过；
- 单 vCPU F2/F3 用例不退化。

### 10. 阶段 F5：稳定性与 CI 门禁

#### 10.1 进入条件

- F2/F3 的 VMX/SVM 单 vCPU 用例稳定；
- F4 的多 vCPU 用例达到约定稳定性；
- 固件获取、guest 资产准备和结果归因完全自动化；
- 不依赖人工日志判断、超长跨行 regex 或扩大 timeout 掩盖波动。

#### 10.2 交付

- 为 Intel/VMX 与 AMD/SVM 保留独立 self-hosted case；
- 两者使用同一 backend-neutral guest baseline；
- 固件 digest、guest 镜像标识和成功 marker 可从 CI 日志复核；
- `cache_key` 在 self-hosted CI matrix 中保持空字符串；
- 记录重复运行次数、失败率和已知宿主限制后再启用 gating。

### 11. 跨阶段验证矩阵

| 声明或风险 | 最低层验证 | 运行验证 | 首次要求阶段 |
| --- | --- | --- | --- |
| OVMF 输入与实际加载镜像一致 | 资产路径、尺寸、digest 和组装测试 | VMX/SVM 运行摘要 | V |
| fw_cfg/table-loader 错误显式 | axdevice 成功、descriptor/data fault 与回写失败测试 | guest ACPI 断言 | V |
| resolved graph 一致性 | resource planning/firmware plan 测试 | guest tty/IOAPIC/ACPI | V |
| 当前 OVMF/Linux 链路可复现 | runner discovery/build 测试 | `AXVISOR_X86_OVMF_ACPI_PASSED` | V |
| 已确认平台缺口被正确修复 | 修复前失败回归 | V 阶段全量复跑 | F1（条件） |
| PCI 启动设备协议完整 | config/BAR/DMA/reset/error 测试 | EFI 应用 marker | F2 |
| Linux 从 guest ESP 启动 | EFI/磁盘/VARS 测试 | Linux initramfs/shell marker | F3 |
| 中断与 SMP 完整 | APIC/INIT/SIPI/IRQ 状态测试 | 多 vCPU Linux marker | F4 |
| backend 与稳定性 | 同一 guest config 校验 | VMX/SVM 重复与 CI | F5 |

每次修改 Rust 逻辑仍须运行 `cargo fmt`、目标 `cargo xtask clippy --package <crate>` 和对应最低层测试；Axvisor 构建与运行优先使用 `cargo xtask`。

### 12. 错误、资源与回滚规则

- 固件和设备图在 VM 可运行前完成规划与 build，不在 vCPU 热路径动态注册设备；
- fw_cfg selector/DMA 状态使用既有非睡眠同步，锁内不执行外部 I/O 或回调其他设备；
- PIO string I/O 逐元素提交，失败时不得提前更新 RIP、索引或计数；
- 未注册设备访问返回明确未命中/错误；只有 PCI 不存在 function 使用规范要求的全一返回；
- 固件窗口、RAM、ACPI image、PCI BAR 和 device hole 由单一规划负责并检查溢出/重叠；
- 测试资产必须隔离、可清理；共享 OVMF VARS 模板不得被原地修改；
- 每个 feature 阶段可以独立回滚，回滚时同步移除配置、runner 接线、测试和文档声明；
- V 阶段保持 non-gating，F5 之前不改变现有 CI 默认行为。

### 13. 派生文档规则

后续维护 `pr1.md`、`pr2.md` 或新的 PR 计划时：

1. 先声明它对应本文的 V、F1、F2、F3、F4 或 F5；
2. 只展开该阶段的提交拆分、负责人、运行记录和 PR 文案；
3. 不重新定义目标、现状、证据强度、架构不变量或跨阶段边界；
4. 如果实施发现本文事实错误，先以当前 `dev` 证据修订本文，再更新派生文档；
5. 历史分支提交只能列为调查线索，不能列为“待搬运实现”或当前能力证据。
