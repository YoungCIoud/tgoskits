# Axvisor x86_64 Guest UEFI 支持设计

> 基线：`dev@343d47f4e`（2026-08-07，含 PR #1718 解析后设备图与 PR #1918 fw_cfg DMA fault 修复）。
>
> 风险等级：高风险功能。该路线涉及固件协议、客户机物理地址布局、PIO/DMA、ACPI、PCI、中断和 vCPU 启动契约。
>
> 本文只把 `dev` 视为当前事实。`feat/uefi-p0-r2` 可作为阶段 1 的参考实现，但其代码和运行结果不构成本文所述阶段已经完成的证据。`pr1.md`、`pr2.md` 分别描述待实施的阶段 1、阶段 2。

## 1. 问题、受众与目标

### 1.1 当前问题

最新 `dev` 已经具备 x86 客户机 UEFI 所需的大部分平台骨架，但缺少一条可复现、可归因、能逐级证明启动进度的验证链。没有固定固件和分层门禁时，以下状态容易被混为一谈：

- QEMU 外层 OVMF 成功加载 Axvisor 宿主；
- Axvisor 成功把 OVMF CODE 放入嵌套客户机；
- 嵌套 OVMF 从 reset vector 开始执行；
- 嵌套 OVMF 访问了 Axvisor 的 fw_cfg；
- table-loader 真正安装并校验了 ACPI；
- 固件到达 DXE/BDS；
- 固件发现启动设备并加载 EFI 应用或 Linux。

旧设计把 fw_cfg PIO、x86 ACPI、PIC、CMOS、PCI 配置端口和字符串 I/O 列为待实现能力。该判断已被 `dev` 的 PR #1718 淘汰；继续按旧阶段划分开发会重复现有能力，并绕过解析后设备图这一新的唯一事实来源。

### 1.2 受众

- Axvisor x86 VMX/SVM 维护者：需要能区分宿主固件与嵌套固件的诊断用例；
- 虚拟设备和固件维护者：需要从 resolved graph 跟踪到 ACPI 与 runtime 的同一资源事实；
- Guest OS 集成者：最终需要标准 UEFI 启动盘、EFI 应用和 Linux EFI stub 路径；
- CI 维护者：需要可复现、不会由宿主层日志误命中的门禁。

### 1.3 最终目标

让 x86_64 Axvisor 客户机通过标准固件路径启动：

1. 使用 Ostool 固定版本的上游 RELEASE OVMF；
2. 由 Axvisor 提供解析后设备图驱动的 fw_cfg 与 ACPI；
3. 由 OVMF 发现 PCI 启动设备并加载 EFI 应用；
4. Linux EFI stub 启动到可观察的 guest shell；
5. VMX 与 SVM 都有稳定门禁，SMP 客户机能通过 INIT/SIPI 启动辅助 vCPU。

### 1.4 阶段 1 成功标准

阶段 1 计划建立“固定固件到嵌套 fw_cfg 访问”的诊断闭环：

| 声明 | 可观察证据 |
| --- | --- |
| 运行使用固定上游 RELEASE OVMF 布局 | Ostool 提供 `code.fd`/`vars.fd`；axbuild 校验尺寸和地址布局并打印摘要 |
| Axvisor loader 使用同一固定 CODE 布局 | profile 强制 CODE GPA `0xffc8_4000`、size `0x37c000`、reset `0xfffffff0` |
| 嵌套固件实际执行并访问 Axvisor 设备模型 | 日志出现 `Nested OVMF fw_cfg accessed` |
| VMX/SVM 用例可分别选择 | `ovmf-entry-vmx` 与 `ovmf-entry-svm` 两个完整 case 名可被 runner 发现 |
| 不把未知本地固件当作验证证据 | 本地文件必须显式使用 `--allow-unverified-firmware`，且仅供诊断 |

该 marker **不能**证明 ACPI 已安装、BDS 已到达、PCI 已枚举或客户机 OS 已启动。

### 1.5 非目标

阶段 1 不实现：

- 嵌套 OVMF VARS/pflash 持久化；
- Secure Boot、SMM、TPM、PXE；
- 新的 PCI/virtio 设备；
- INIT/SIPI、NMI 和完整 MSI/MSI-X；
- 把 `uefi` 测试组加入 CI gating；
- LoongArch UEFI 路径重构。

### 1.6 不推进的代价

如果阶段 1 完成后只停留在该 marker，开发者仍只能证明嵌套 OVMF 开始访问 fw_cfg，无法证明固件平台描述、启动设备、中断和 OS 启动语义成立。x86 UEFI 仍然只是诊断路径，不能作为可支持的 guest 启动方式。

## 2. 最新 dev 的事实基线

### 2.1 已具备能力

| 能力 | 当前所有者与实现 | 设计结论 |
| --- | --- | --- |
| 开放式虚拟设备配置 | `axvmconfig` 的 `id + model + options`，`ConfiguredDeviceCatalog` 显式注册 | 不恢复中心设备 enum 或地址/IRQ TOML |
| 唯一设备事实来源 | `DeviceGraphBuilder -> ResolvedDeviceGraph -> ResourceClaimSet/lease -> DeviceRuntime` | 固件和 runtime 必须消费同一 resolved resources |
| x86 平台设备规划 | `virtualization/axvm/src/arch/x86_64/vm.rs` | IOAPIC、fw_cfg、PIT、PIC、CMOS、PCI config、ACPI PM timer 均由架构计划创建 |
| fw_cfg PIO | `axdevice::FwCfgPioDevice`，PIO `0x510..0x51b`，共用 fw_cfg selector/DMA 状态 | 不再新增第二套旁路 fw_cfg |
| PIO guest-memory 写入 | `DeviceRuntime::try_handle_port_write_with_memory` 和 x86 VM exit 接线 | fw_cfg DMA 已能经统一 runtime 访问 guest memory |
| fw_cfg DMA fault 语义 | `dev@343d47f4e`（PR #1918） | descriptor/data fault 必须回写 ERROR；不可回写时返回明确错误 |
| x86 ACPI | `X86FirmwarePlan::from_graph`，直接启动 ACPI image 与 fw_cfg table-loader 两条输出 | 两条路径必须继续读取同一 plan |
| 当前 ACPI 表 | RSDP/XSDT/FADT/FACS/DSDT/MADT/SPCR | 不声称已有 MCFG；PCI 当前通过配置机制 #1 |
| x86 固件设备 AML | PCI0、解析后的串口、`QEMU0002` fw_cfg | 地址和 IRQ 来自 resolved graph，不得复制常量事实源 |
| x86 传统平台设备 | 双 8259 PIC、CMOS、PIT、IOAPIC、ACPI PM timer | 旧设计中“新增 PIC/CMOS”已失效 |
| PCI 配置 | `X86PciConfigDevice`，PIO `0xcf8..0xcff`，当前仅 host bridge 与 LPC bridge | 这是最小枚举骨架，不等于已有可启动 PCI 磁盘 |
| REP INS/OUTS | `x86_vcpu::X86PortIoStringExit`，VMX/SVM 都通过逐元素事务完成 | 旧阶段 2 的字符串 I/O工作已被 dev 吸收 |
| 固件 payload | x86 loader 将 kernel/initrd/cmdline 与 ACPI blob 安装到 deferred fw_cfg payload | 不在 `os/axvisor` 重新拼第二份 payload |

### 2.2 尚未被证明或仍缺失

| 缺口 | 当前证据 | 后果 |
| --- | --- | --- |
| BDS 稳定到达 | `dev` 没有固定嵌套 OVMF 自动用例 | ACPI table-loader、MTRR 和后续固件行为仍需实测 |
| 嵌套 VARS 设备 | VM 配置只把 CODE 作为普通内存镜像加载；4 MiB window 不是持久化 flash 模型 | Boot Manager 变量和重启持久性不可承诺 |
| 可启动 PCI 设备 | PCI config 只暴露 host/LPC bridge；catalog 未注册 guest virtio-blk | 固件没有标准启动盘 |
| x86 MSI/MSI-X 设备链 | 通用资源模型已有 MSI 类型，但 x86 guest PCI 设备尚未接通完整路由 | 不能声明现代 PCI 中断可用 |
| SMP 固件启动 | `x86_vlapic::inject_nmi` 与 `process_init_sipi` 仍明确忽略请求 | 多 vCPU UEFI guest 不可用 |
| 稳定 BDS 自动判据 | 外层和嵌套 OVMF 共用串口输出，跨长距离 `.*` 还受 runner 流式匹配窗口影响 | 不能直接复用旧 `[BdsDxe]` regex 作为门禁 |
| CI gating | CI x86 行只跑 `normal` 组的 `smoke-vmx`/`smoke-svm` | `uefi` 组仍是显式诊断 |

### 2.3 最新 dev 带来的架构约束

`book/design/axvisor-resolved-device-graph.md` 是 PR #1718 的权威边界。后续 UEFI 工作必须遵守：

- 架构拥有设备初始化顺序；通用层不规定四架构统一序列；
- 图节点保存同一个 `Arc<dyn DeviceModel>`，其 `requirements()`、`firmware()` 与 `build()` 不得分叉；
- 固件 composer 只能把 `DeviceFirmwareSpec` 与 `ResolvedDeviceResources` 组合，不能再次分配资源；
- `DeviceRuntime` seal 前全部 claim 必须转为 lease；seal 后不动态补设备；
- MMIO/PIO exit 只做一次 runtime 区间查询，不能恢复地址特判、downcast 或二次 dispatch；
- 中断控制器仍是 pending/active/route/EOI 的唯一所有者，设备图不能保存第二份中断状态。

## 3. 阶段 1 增量设计

### 3.1 两层 OVMF 必须区分

```text
Ostool cache: code.fd + vars.fd
             |
             +--> QEMU pflash CODE/VARS --> 外层 OVMF --> 加载 Axvisor host PE32+
             |
             +--> runner 改写 VM config / include_bytes!
                         --> Axvisor loader --> 嵌套 guest CODE
                                             --> reset vector
                                             --> fw_cfg PIO
                                             --> Axvisor marker
```

- QEMU case 中 `uefi = false` 是有意配置：阻止 Ostool runner 自动注入另一套 FAT ESP/固件；Axvisor runner 自己注入经过检查的 pflash。
- QEMU 外层和嵌套层使用同一个 `code.fd`，但承担不同角色。
- 外层 OVMF 的 fw_cfg 访问发生在 QEMU 内部，不经过 Axvisor 设备模型；因此 `Nested OVMF fw_cfg accessed` 天然属于嵌套 guest。

### 3.2 固件来源与验证边界

- `cargo xtask ovmf --arch x86_64` 使用 Ostool 固定版本和 SHA-256 校验准备缓存；
- `scripts/axbuild/src/axvisor/ovmf.rs` 不下载固件，只检查目录中 CODE/VARS 的固定尺寸、组合布局、reset vector 覆盖关系，并输出实际摘要；
- 固定 profile 名计划使用 `qemu_x86_64_axvisor_ovmf_debug`。名字按参考实现兼容保留，计划选择的实际固件是 Ostool 上游 RELEASE OVMF（`edk2-stable202508-r1`）；
- 单个本地 CODE 文件只有显式 `--allow-unverified-firmware` 才能使用，仍需满足 CODE 尺寸，并打印 SHA-256。该路径不能作为通过 UEFI 功能门禁的证据。

### 3.3 配置与 loader 合同

阶段 1 拟新增可选的 `firmware_profile` 配置表面：

- 只有 `boot_protocol = "uefi"` 可声明 profile；
- profile 启用时 loader 强制 CODE size、load GPA 与 reset vector；
- 未声明 profile 的既有 direct/multiboot/UEFI 配置保持原行为；
- 固件缺失、尺寸错误、协议不匹配必须显式失败，不扫描发行版 `/usr/share`，不静默 fallback。

### 3.4 设备 trace 与成功 marker

统一 trace 应位于 `DeviceRuntime` 的真实访问路径，记录 bus、方向、地址、宽度、值和结果；它只提供诊断，不改变错误语义。

阶段 1 计划让 `FwCfgPioDevice` 在嵌套 guest 首次访问时输出一次 Info marker。marker 的语义严格限定为：

> 嵌套 OVMF 已执行到首次访问 Axvisor fw_cfg PIO 设备。

它不应被文档扩大解释为 SEC、PEI、DXE 或 BDS 完成。

## 4. prior art 与方案选择

### 4.1 参考范围

| 资料 | 标识 | 使用方式 |
| --- | --- | --- |
| UEFI Specification | 2.10 | 使用 SEC/PEI/DXE/BDS 生命周期语义，不复制实现 |
| ACPI Specification | 6.5 | 校验 RSDP/XSDT/FADT/MADT/DSDT 等表的指针、checksum 与资源语义 |
| QEMU fw_cfg specification | QEMU 9.x 文档语义 | 使用 selector/data/DMA 与 table-loader 协议 |
| QEMU q35 | `pc_q35`/ICH9 设备语义 | 参考 PIO、PCI config 与 INTx 约定；不实现完整 q35 芯片组 |
| EDK2/OvmfPkg | Ostool `edk2-stable202508-r1` | 固定实际运行的 RELEASE 固件资产 |
| 仓库旧 `feat/uefi-p2` | 基于 `dev@4c7aa8577` | 仅作为 BDS 调试证据；其实现早于 resolved graph，不直接搬运 |

### 4.2 方案比较

| 方案 | 优点 | 阻塞问题 | 结论 |
| --- | --- | --- | --- |
| 保持随机本地 OVMF + 串口文本匹配 | 改动小 | 不可复现，容易命中外层 OVMF | 拒绝 |
| 把旧 `feat/uefi-p2` 整体 rebase/cherry-pick | 看似已有 BDS 结果 | 重复 PR #1718，破坏 resolved graph 边界，冲突面大 | 拒绝 |
| 在调用方硬编码 fw_cfg/ACPI/PCI 地址 | 快速推进单例 | 制造第二事实源，固件与 runtime 可漂移 | 拒绝 |
| 基于最新 dev，先固定固件与证据，再按缺口扩展现有 model/plan | 每步可验证，保留统一边界 | 需要重新定位旧分支中仍真实存在的缺口 | 采用 |

## 5. 后续阶段

### 阶段 1：固定固件与嵌套 fw_cfg 诊断（待实施）

计划交付：

- 上游 RELEASE OVMF 缓存检查与显式未验证路径；
- 固定 loader profile；
- VMX/SVM `ovmf-entry-*` 用例；
- runner 的 pflash 与嵌套 CODE 接线；
- 统一设备 trace；
- 嵌套 fw_cfg 首次访问 marker。

退出条件见 §1.4，不追加 BDS 声明。参考分支中的实现形状可用于降低探索成本，但阶段完成必须以从 `dev` 实施后的当前 head 验证为准。

### 阶段 2：在最新设备图上证明 ACPI 安装与 BDS

阶段 2 依赖阶段 1 完成，但仍不是“重新实现 fw_cfg/ACPI”。执行顺序：

1. 使用阶段 1 交付的 `ovmf-entry-*` 和固定固件捕获完整 trace/串口转录；
2. 从首次 fw_cfg 访问沿 `ResolvedDeviceGraph -> X86FirmwarePlan -> FwCfgPayloadSlot -> FwCfgPioDevice` 核对 selector、DMA descriptor、table-loader 与 ACPI blob；
3. 对照旧 `feat/uefi-p2` 的 AMD/SVM BDS 记录，只移植经最新 dev 复核后仍缺失的最小语义；
4. 候选项必须由当前 trace 证明，重点检查 MTRR MSR、fw_cfg file set、RAM/firmware window 分类和 table-loader 操作；
5. 为每个真实缺口先添加能在当前 dev 失败的最低层回归，再实现修复；
6. 建立不依赖超长跨行 regex 的嵌套 BDS 证据。若只能人工确认，保持 non-gating 并保存完整命令与转录。

退出条件：

- ACPI table-loader 的 allocate/pointer/checksum 操作全部成功；
- RSDP/XSDT/FADT/MADT/DSDT/SPCR 指针闭包和 checksum 有自动测试；
- 嵌套 OVMF 到达 BDS 有可归因证据；
- direct Linux ACPI 与 fw_cfg ACPI 仍消费同一 `X86FirmwarePlan`；
- 没有重新引入设备地址特判或第二套设备描述。

### 阶段 3：PCI 启动设备与 EFI 应用

在现有 `X86PciConfigDevice` 和 resolved graph 上扩展，不新建旁路 PCI 总线：

- 为真实 guest 启动设备定义 `DeviceModel`，声明 PIO/MMIO、wired IRQ 或 MSI 等资源槽；
- PCI config、固件 `_PRT`/资源窗口和 runtime 使用同一 resolved node；
- 优先实现最小 virtio-blk PCI transport 与只读/可回滚测试镜像；
- DMA 只通过显式 guest-memory capability，错误和 reset 必须撤销未完成请求；
- BDS 发现启动项、读取 ESP 并加载 EFI 应用后才算完成。

退出条件：固件能枚举设备、读取 ESP、运行 EFI 应用；不存在 host NVMe 被误当成 guest 启动盘的路径。

### 阶段 4：中断与 SMP 完整性

- 补齐 x86 guest PCI INTx/MSI/MSI-X 的 controller-owned 路由；
- 实现并验证 INIT/SIPI，保持固件 APIC ID 与逻辑 vCPU ID 映射明确；
- 实现 NMI 或对不支持场景明确返回错误，不能继续静默忽略；
- level IRQ 的 pending/active/EOI 状态只由 PIC/IOAPIC/LAPIC 所有；
- 使用 ≥2 vCPU 的 UEFI guest 验证 AP 启动与中断。

### 阶段 5：Linux 与 CI 门禁

- Linux EFI stub 启动到 guest initramfs/shell marker；
- VMX 与 SVM 使用同一 backend-neutral guest baseline；
- `uefi` 用例只有在固件获取、结果归因和稳定性满足要求后才进入 CI gating；
- 不以扩大 timeout 或放宽 regex 代替修复。

## 6. 并发、错误与安全约束

- 固件和设备图在 VM 可运行前完成规划与 build；不在 vCPU 热路径动态注册设备；
- fw_cfg selector/DMA 状态由设备内部非睡眠锁保护，guest-memory fault 按 PR #1918 语义传播；
- 设备锁内不回调其他设备，不在持锁时唤醒或执行外部 I/O；
- PIO string I/O 逐元素提交，设备或 guest-memory 访问失败时不得提前更新 RIP/索引/计数；
- 未注册地址返回明确未命中/错误，不伪造“全零成功”来推动固件；PCI 不存在 function 的全一返回只用于 PCI 规范语义；
- 固件窗口、RAM、ACPI low-memory image 和设备 hole 必须有单一地址规划，所有加法检查溢出；
- `--allow-unverified-firmware` 不改变受支持固件声明，只提供开发者诊断入口。

## 7. 验证矩阵

| 能力或风险 | 层级 | 命令/测试 | 通过条件 |
| --- | --- | --- | --- |
| 固件目录布局 | axbuild 单测 | `cargo test -p axbuild --lib` | CODE/VARS 缺失、尺寸错误、本地文件未 opt-in 均失败 |
| 固定 loader profile | axvm 单测 | 对应 axvm package test | size/GPA/reset/profile 协议矩阵准确拒绝 |
| fw_cfg DMA fault | axdevice 单测 | `cargo test -p axdevice fw_cfg` | descriptor/data fault 与回写失败语义保持 PR #1918 行为 |
| resolved graph 一致性 | 集成测试 | `cargo test -p axdevice --test resource_planning` 及现有图测试 | 固件与 runtime 使用同一 resolved slots |
| 阶段 1 嵌套诊断 | QEMU/KVM | `cargo xtask ovmf --arch x86_64` 后运行 `ovmf-entry-vmx` 或 `ovmf-entry-svm` | 命中嵌套 fw_cfg marker，无 panic/fail regex |
| 阶段 2 BDS | QEMU/KVM | 后续专用验证 | ACPI 安装证据 + 可归因 BDS 证据 |
| BIOS/direct 回归 | QEMU/KVM | `normal` 组 `smoke-vmx`/`smoke-svm` | 现有路径不退化 |

本文只规定验证计划；实际代码阶段仍按修改 crate 运行 `cargo fmt`、目标 `cargo xtask clippy --package <crate>` 和对应测试。

## 8. 上线、回滚与可观测性

- UEFI 通过 `boot_protocol = "uefi"` opt-in；固定 profile 进一步选择受约束布局；
- 删除 `firmware_profile` 可回到原有非固定 UEFI 配置，改回 direct/multiboot 可完全绕过 UEFI；
- 阶段 1 用例计划放在 `uefi` 组并保持 non-gating，不影响现有 CI normal 组；
- 可观测信号按强度排序：固定资产摘要 < loader 日志 < fw_cfg marker < ACPI 安装证据 < BDS < EFI 应用 < Linux shell；
- 回滚时必须同步恢复测试配置、runner 接线、loader profile 和文档，不能留下两套互相矛盾的固件合同。

## 9. 文档一致性规则

- 本文只记录当前 `dev` 事实和仍有效的路线；
- `pr1.md` 记录阶段 1 的实施计划，不把参考分支的历史中间提交当作 `dev` 现状；
- `pr2.md` 描述阶段 1 完成后的下一阶段，不把旧分支提交列为待合入项；
- `solve_conflict.md` 记录参考实现适配最新 `dev` 时的边界选择，不声明阶段已经完成；
- 架构边界变化时同步更新 `.claude/skills/arch-platform-porting/SKILL.md` 或其 reference。当前变更只修正文档以匹配既有边界，不改变启动合同，因此无需再次修改该技能。
