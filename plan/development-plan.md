# AxVisor 中断路由与 Guest UEFI 总体开发计划

> 计划基线：2026-07-27
> 输入：[Discussion #1604](https://github.com/rcore-os/tgoskits/discussions/1604)、本目录中的中断/UEFI/probe 设计，以及 `intr/sender`、`uefi/p0`、`router/AArch64` 分支现状。

## 1. 总体目标

当前开发分为两条相互依赖的主线：

1. 完成 AxVisor 设备中断注入架构重构，使设备只表达 `IrqLine`，由各架构 Router 决定路由，runtime 负责排队、唤醒和 host IPI，目标 vCPU 在自己的 run loop 中完成最终注入。
2. 在中断边界稳定后，把 x86_64 Guest UEFI 从“能够进入 OVMF”推进到“能够通过标准 PC 平台发现路径从磁盘启动 Guest OS”，并补齐 LoongArch Guest UEFI 的自动化验收。

关键依赖关系如下：

```text
共享 IRQ 资源与 sender/dispatcher
  → AArch64 Router
  → LoongArch Router
  → x86 Router
  → 旧中断路径清理
  → x86 Guest UEFI 的 PCI/virtio-blk/INTx 闭环
```

UEFI P0、固件/flash、fw_cfg 和平台表可以在不改变依赖关系的前提下提前准备；真正的 virtio-blk INTx 启动链必须等 x86 Router 稳定。

## 2. 当前基线

| 工作项 | 状态 | 当前证据 | 下一出口 |
| --- | --- | --- | --- |
| IRQ line 资源与冲突检测 | 已完成 | `Resource::IrqLine`、冲突索引和失败回滚已有确定性测试 | 保持为各架构 Router 的设备能力入口 |
| 通用虚拟中断模型 | 已完成 | `VirtualInterruptId(u32)`、`PendingVcpuInterrupt` 和 per-vCPU 队列已落地 | 不再让 x86 `u8` vector 限制扩散到其他架构 |
| sender/dispatcher/run-loop 通道 | 代码已贯通，待最终合入 | `intr/sender` 已形成 `send → enqueue → notify/IPI → drain → inject`；旧队列仍保留 | 完成 review、生命周期/锁顺序回归并合入 |
| RISC-V 基线 | 已验证 | 继续使用现有 `RiscvPlicIrqSink`，vPLIC 与 IPI 行为不变 | 后续仅做回归 |
| AArch64 Router | 设计完成，未开始实现 | `interrupt/design_3.md` 与 `interrupt/issue_3.md` 已定义 3A–3F | 从无损 INTID 和 VGIC 状态机开始 |
| LoongArch Router | 未实现 | 已明确 PCH-PIC、EIOINTC、平台 IRQ 与 sender 边界 | 在 AArch64 主链验证后迁移 |
| x86 Router | 未实现 | 已拆为 5a–5f，覆盖 IOAPIC、PIT/串口、host IRQ、IPI、EOI | 为 UEFI INTx 闭环提供前置能力 |
| `probe.efi` | 原生 OVMF/QEMU 阶段完成 | `uefi-core`、`pc-platform`、`runtime-vars` profile 和稳定输出协议已有验证 | UEFI 平台可用后接入 AxVisor Guest |
| Guest UEFI P0 | 仓库内实现完成，外部验收待补 | `uefi/p0` 固定 OVMF DEBUG profile、manifest、入口诊断和测试编排 | 补真实固件资产、Q35 参考启动和 VMX/SVM Trace |
| Guest UEFI P1–P6 | 未实现 | 总体设计、阶段退出条件和验证矩阵已形成 | 按阶段推进，不提前宣告未实现设备 |

## 3. 实施里程碑

### M0：收尾共享 sender/dispatcher 通道

预计工作量：约 1 周。

工作内容：

- 完成 `intr/sender` 的最终 review 与合入，确保 sender 在释放 VM machine lock 后再进入 runtime 调度。
- 固化 `enqueue → notify → host IPI` 顺序，失败入队不得触发回调。
- 确认 stop/reset/start 后 sender 只选择当前 runtime，旧 dispatcher 不再接收事件。
- 保持旧 `pending_interrupts` 与新 dispatcher 双队列并存；在所有旧 producer 迁移前不删除兼容路径。
- 保持四架构 adapter 的 trigger 语义，避免 release 模式静默丢失 level 信息。

退出条件：

- sender、dispatcher、run-loop consumer 和生命周期错误路径均有可执行测试。
- `axvm-types`、`axvm` clippy 通过，四架构相关 AxVisor build 配置通过。
- 当前没有 Router 使用 sender 时，Guest 行为与迁移前一致。

### M1：实现 AArch64 emulated GICv3 SPI Router

预计工作量：约 3–4 周；整体难度高，3C 是关键路径。

按可独立审查的顺序实施：

1. **3A：INTID 与寄存器边界**
   - 将 host 侧 virtual interrupt 边界扩宽为无损 `u32`。
   - 校验 SPI INTID 32–1019，覆盖 255、256、1019 及非法边界。
   - 补齐 ICH LR/TDIR/maintenance 所需字段，把 LR 满载从 panic 改成可恢复结果。
2. **3B：VGIC SPI 状态**
   - 建立 VM-owned controller，保存 trigger、enabled、asserted、pending、active 和固定 route。
   - 用纯状态机测试覆盖 edge 合并、active+pending、level lower/deactivate/重投。
3. **3C：per-vCPU ICH 生命周期**
   - 保存恢复每个 vCPU 的 ICH context。
   - 在目标 vCPU/pCPU 上执行 fold/refill，处理两种 EOImode、TDIR、maintenance 和 LR pressure。
   - 明确 host maintenance PPI 的唯一 transaction owner，避免重复 acknowledge/EOI。
4. **3D：配置与初始化**
   - 增加 typed `fixed_irq_routes`，按 controller → sender → delivery port → MMIO wrapper → Router 顺序构造。
   - prepare 失败必须回滚，reset 不得复用旧 controller、Router 或 line。
5. **3E：Router 与 kick**
   - 接通 `IrqLine → ArmInterruptRouter → VGIC state → sender kick → target refill`。
   - dispatcher 事件只表示目标 vCPU 需要处理，pending/active 真相留在 VGIC。
   - stale kick 必须成为无害 no-op，Router 不直接访问远程 vCPU 或 ICH 寄存器。
6. **3F：端到端**
   - 增加 test-only synthetic SPI device、EL1 SMP2 Guest 和专用 QEMU case。
   - 覆盖跨 CPU 固定路由、INTID 256、edge、level 重投、lower 后停止和 17 路 LR pressure。

退出条件：

- LR 只在目标 vCPU 当前 pCPU 上更新，ICH state 不跨 vCPU 泄漏。
- edge、两种 EOImode 下的 level、LR refill、跨 CPU wake 和 INTID 256 均有确定性证据。
- emulated GICv3 SMP2 QEMU 通过，现有 passthrough/GPPT、SGI/PPI/IPI 和非 AArch64 路径不退化。

### M2：实现 LoongArch Router

预计工作量：约 1–2 周。

工作内容：

- 让 PCH-PIC MMIO wrapper 与 `LoongArchInterruptRouter` 共享同一 controller core。
- 保留 `physical_irq` 的 host bookkeeping 语义，把 `guest_vector` 作为 Guest line 输入。
- 平台 IRQ 和 PCH-PIC 输出统一经 sender 递送，不静默硬编码 vCPU 0。
- 迁移期保留 `after_mmio_write`，只把 drain 结果改走 sender。
- 所有 producer 迁移后再删除 `PendingInterrupt::External` 和旧 external queue。

退出条件：

- 平台 route 的 VM/vCPU/vector/physical IRQ 元数据不丢失。
- PCH-PIC MMIO 输出不会因后处理或 reset 丢失。
- LoongArch AxVisor build 与可用的 QEMU 回归通过。

### M3：实现 x86_64 Router

预计工作量：约 3–4 周；每个子阶段同时保持 VMX 与 SVM 可编译。

实施顺序：

- **5a**：共享 `EmulatedIoApic` 所有权，建立 `X86InterruptRouter` 和 APIC ID 映射。
- **5b**：实现 Fixed + Physical Destination、真实 edge/level line 语义及 unsupported route 错误。
- **5c**：让 PIT IRQ0 和串口通过 `IrqLine`，删除 runtime 的设备类型轮询。
- **5d**：host IRQ adapter 只向 Guest GSI 输入 line，分离 host trigger 与 Guest IOAPIC trigger。
- **5e**：vLAPIC IPI 经 x86 专用 sender hook 投递，不进入设备 Fabric。
- **5f**：收敛为唯一 EOI hook，remote-IRR 更新和重注入统一经 Router/sender。

退出条件：

- destination、edge pulse、level raise/lower、EOI 重投和错误路由均有单元测试。
- VMX/SVM 注入、IPI、EOI 与 direct-boot Guest 回归通过。
- x86 runtime 不再理解 PIT/串口 pending 状态，为 UEFI virtio INTx 提供 VM-local 中断闭环。

### M4：中断重构清理与总验收

预计工作量：约 1 周。

仅在搜索并证明所有旧 producer 已迁移后执行：

- 删除旧 `PendingInterrupt`、`pending_interrupts`、`queue_interrupt()` 和 `queue_external_interrupt()`。
- 删除无调用者的设备特定查询接口和 x86 轮询 helper。
- 保留仍承担明确架构职责的 EOI/MMIO hook，不为“统一接口”引入空操作方法。
- 完成 RISC-V、AArch64、LoongArch、x86 的构建与环境可用的 QEMU 回归。

总体验收以 [`interrupt/issue.md`](./interrupt/issue.md) 的完成标准为准。

### M5：完成 Guest UEFI P0 外部资格并推进 P1–P2

预计工作量：约 3–4 周，不含外部固件资产等待时间。

P0 补证：

- 发布并校验固定 `edk2-stable202605` OVMF DEBUG bundle。
- 在两个干净 workspace 验证可复现构建、CODE/VARS/combined 关系和 manifest。
- 完成 Q35 + TCG 的 SEC → PEI → DXE → BDS → EFI 应用参考启动。
- 在 Intel VMX 与 AMD SVM 上记录 Guest SEC 标记和第一个未实现设备访问 Trace。

P1–P2 实现：

- 在解析边界建立 typed firmware plan、boot source 和 profile。
- 实现只读 CODE、独立可写 VARS、pflash program/erase 与 reset/persistence 契约。
- 将 fw_cfg 拆成 per-VM store 与 transport profile；x86 使用 PIO，LoongArch 保持 MMIO。
- 建立只描述现有设备的 x86 `GuestPlatform`，生成 RSDP/XSDT/FADT/MADT/DSDT 等基础表。

退出条件：

- OVMF 能稳定进入 Shell，并能区分固件装载、平台发现、设备枚举和 OS 启动故障。
- CODE/VARS 生命周期、fw_cfg 端序和 ACPI 表均有字节级确定性测试。
- LoongArch fw_cfg/ACPI 现有行为不退化。

### M6：Guest UEFI P3–P4，形成 x86 MVP

预计工作量：约 4–6 周。

工作内容：

- 实现 VM-owned PCI root、ECAM/必要的 PIO config、静态 BDF、BAR 和 capability。
- 由同一平台描述生成 MCFG、PCI0 `_CRS`/`_PRT`，禁止 ACPI 宣告不存在的设备。
- 实现 modern virtio-pci block、guest-memory DMA 校验和文件 block backend。
- 使用 x86 Router 提供的 per-VM vIOAPIC sink 完成 INTx assert/deassert、remote-IRR 与 EOI 闭环。
- 准备确定性的 ESP/Guest 磁盘，依次验证 PCI 枚举、Block IO、`probe.efi`、EFI 应用和 Linux 用户态标记。

退出条件：

- x86_64 单 vCPU VM 不依赖宿主 PCI/IOAPIC 透传，从 virtio-blk 磁盘进入 Linux 用户态。
- VMX 与 SVM 使用同一份 backend-neutral Guest 配置通过。
- 成功判定来自 Guest，自身的 `VM boot success` 等宿主日志不得作为证据。

### M7：Guest UEFI P5–P6

作为 MVP 后续阶段推进：

- 补齐 RTC/CMOS、ACPI PM/SCI/reset、SMP、MSI/MSI-X、virtio-net 和 virtio-console。
- 收敛公开 TOML schema，旧 `enable_bios`、`bios_*` 和 kernel-level `disk_path` 仅保留兼容解析与弃用诊断。
- 增加 ArceOS、NimbOS 的明确 EFI/ESP/fw_cfg 启动契约。
- 为 LoongArch 建立 OVMF → Linux/rootfs 的自动化 Guest UEFI 门禁，并验证 virtio-blk 读写。

退出条件：

- x86_64 与 LoongArch 各至少有一条 Guest UEFI Linux 端到端用例。
- SMP Linux、时间、reset、磁盘 IRQ 和 MSI-X 具有独立回归证据。

## 4. 延后但保留的工作

以下工作不阻塞中断重构或 x86 Guest UEFI MVP：

- **虚拟设备饥饿**：Guest 长时间无 VM exit 时，需要 VMX preemption timer 等硬件虚拟化定时器强制退出。该问题独立设计，不与 Router 重构混合。
- **MSI/MSI-X**：放在 x86 INTx 与 Guest UEFI 磁盘启动稳定之后；不提前把 MSI address/data、Guest vector、GSI 和 host IRQ 混成一个编号。
- GICv2、guest-programmable `GICD_IROUTER`、完整 VGIC MMIO、ITS/LPI。
- Secure Boot、TPM、S3/S4、热插拔、图形和完整 q35 芯片组。

## 5. 通用质量门槛

每个可合入子阶段都必须满足：

- 先提供能够在旧实现上失败的确定性回归测试，再实现修复或新行为。
- 共享状态具有明确所有者；锁内只做状态更新，wake、IPI 和外部回调在锁外执行。
- 不使用静默默认 vCPU、裸截断、`.ok()`、不可恢复 panic 或无证据的兼容回退。
- Rust 改动后运行 `cargo fmt` 和受影响 crate 的 `cargo xtask clippy --package <crate>`。
- 按影响面执行 `cargo xtask axvisor` 构建/测试；物理板和 self-hosted 项在环境不可用时明确记录为外部证据缺口。
- 每个 PR 只跨越一个可独立解释和验证的边界，设计、实现、测试与 PR 描述保持同步。

## 6. 计划调整规则

- M1 的 3C、M3 的 EOI/host IRQ、M5 的真实 OVMF 资产和 M6 的 virtio DMA 是主要风险点。
- 任一阶段发现 machine contract、状态所有权或并发前提不成立时，先更新对应设计与验收映射，再继续实现。
- 外部资产或 KVM runner 不可用不等同于功能通过；仓库内测试和端到端证据分别记录。
- 每周总结按“完成、验证、未完成/阻塞、下周目标”更新，不把设计完成写成实现完成。
