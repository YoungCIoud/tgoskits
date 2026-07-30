# PR #1718 与模块 3 的冲突及后续开发注意事项

## 1. 文档目的和快照

本文记录模块 3 设计与
[`rcore-os/tgoskits#1718`](https://github.com/rcore-os/tgoskits/pull/1718)
之间可能影响实现、合入顺序和验收的差异。

分析基于 2026-07-29 的 PR head
`8eef0ca419ffa8c9a6812cb14ee42057424e087b`。PR 仍可能继续修改，因此：

- 本文描述的 PR 文件、CI 和 merge 状态只是该 head 的审查快照；
- 模块 3 的功能不变量和验收条件不因 PR 后续变化自动放宽；
- 开发或合入前必须重新比较最新 PR head，不能只按本文解决旧冲突。

## 2. 总体判断

两套方案的长期原则大体一致：

- device 只持有中断线路能力；
- controller 是 pending、active、route、enable 和 EOI 语义的唯一状态源；
- LR 是有限硬件执行缓存，不是 durable pending 队列；
- 每个 vCPU 保存和恢复虚拟 CPU interface 状态；
- controller 锁内只更新状态，wake、IPI、host deactivate 等动作在锁外执行；
- 初始化和资源 claim 需要确定性、原子提交和失败回滚。

但 PR #1718 不是模块 3 的普通前置补丁。它同时重建：

- `axdevice_base` 的中断类型和线路；
- `axdevice` 的全架构资源 planner、claim 和 controller registry；
- `arm_vgic` 的 GICv2、GICv3、ITS/LPI、软件和物理中断状态；
- AArch64 guest FDT、VGIC、timer、physical SPI 和 vCPU 接线；
- RISC-V、x86_64、LoongArch 的设备资源初始化。

因此两者当前属于“部分原则相同、实现边界相互竞争”。不能把模块 3 的五个 PR
直接堆叠到 PR #1718，也不能在不重新确定状态和 capability 所有权的情况下把
两边代码机械合并。

## 3. 冲突总表

| 领域 | 模块 3 | PR #1718 快照 | 影响 |
|------|--------|---------------|------|
| 范围 | 仅 AArch64 emulated GICv3 wired SPI | GICv2/v3、ITS/LPI、物理 SPI/MSI、全架构资源规划 | PR 规模和风险显著超过模块 3 |
| 软件 SPI route | host 配置固定、显式、无默认 target | GICv3 主要由 guest `GICD_IROUTER` 写入；GICv2 无配置时使用 CPU0 mask | 不满足模块 3 固定显式路由 |
| line 到 runtime | Router 产生 target service kick，经 `VmInterruptSender`/dispatcher | `VgicCore` 直接调用 per-vCPU wake callback，`notify_all()` 后可选 target IPI | Router、dispatcher 和 wake 边界不同 |
| pending 真相 | controller；dispatcher event 只是可重复/过期的 kick | controller；VGIC 路径不向 dispatcher pending queue 入队 | 状态原则一致，runtime 接线不同 |
| ICH capability | per-pCPU 发布 profile，按 placement 求共同能力 | 一个全局 `Once<HostCpuInterface>` 缓存首次发现的能力 | 迁移和异构 pCPU 能力未按模块 3 边界验证 |
| ICH state 所有权 | raw context 在 `arm_vcpu`，领域 slot/epoch 在 `arm_vgic`/AxVM adapter | 完整 `CpuInterfaceState` 在 `arm_vgic` Redistributor，AxVM 实现寄存器 backend | crate 边界互斥，不能同时保留 |
| LR overflow | active 必须 resident；outside-LR 只允许 pending；UIE 推进 refill | 支持 active-outside-LR，并使用 NPIE、LRENPIE、EOICOUNT | HCR policy、fold 和完成模型不兼容 |
| level EOI | 软件 level LR 设置 EOI maintenance；typed maintenance PPI 保证进展 | GICv3 软件 LR 未设置 EOI 位；未见 maintenance PPI 注册/消费链路 | asserted level 的及时重投尚无系统证据 |
| 完整 guest MMIO | 当前非目标，只预留状态输入边界 | 已实现大量 enable/pending/active/priority/route MMIO | 不是语义错误，但扩大首轮证明面 |
| resource/config | AArch64 fixed route + architecture-owned两阶段构造 | 通用 planner、one-shot claim、immutable AArch64 plan | 配置类型和 factory 假设需要重做 |
| 当前 PR 3.1 | 扩宽 `arm_vcpu` virtual INTID 和 direct LR API | 删除该边界，把 typed INTID/LR 管理迁入 `arm_vgic` | 代码和领域所有权直接冲突 |

## 4. 需要保留的模块 3 不变量

不论最终复用 PR #1718 的多少代码，以下要求不能因“已有类似实现”而跳过。

### 4.1 固定显式路由

模块 3 的软件 SPI 必须在 VM prepare 时得到唯一
`(INTID, target_vcpu_id)`：

- 不依赖 guest 写 `GICD_IROUTER` 后才可投递；
- 不允许 GICv2-style 隐式 vCPU 0 或任意 fallback；
- route 与 device line/trigger 一一匹配；
- missing、duplicate、unused、非法 target 都在发布设备前失败；
- future `GICD_IROUTER` 只增加 route mutation 输入，不改变线路、pending、
  wake 和 target refill 主链路。

PR #1718 快照中软件 SPI 的 `claim_software_spi()` 不安装 fixed route；
GICv3 测试通过写 `GICD_IROUTER` 选择 Redistributor。这部分不能直接作为模块
3 的 route 完成证据。

### 4.2 capability 必须按实际执行 CPU 验证

不能只在某个 CPU 首次读取一次 ICH capability 后假设所有 placement CPU
相同。至少需要：

- 每个已启用 pCPU 发布不可变 ICH profile；
- 校验 LR count、virtual INTID bits、priority/preemption/APR layout 和 TDS；
- VM prepare 对实际 placement CPU 求共同 profile；
- vCPU 换入新 pCPU 前再次验证兼容性；
- capability 发现失败不得留下半启用 ICH interface；
- load/save 只能访问当前已 pin 的本地 ICH bank。

如果最终采用 PR #1718 的 backend trait，可以把 per-CPU registry 放在 AxVM
AArch64 backend，而不必强制沿用模块 3 当前的具体类型位置；但上述语义和错误
边界必须保留。

### 4.3 level EOI 后必须有独立进展来源

controller 状态机能在调用 `synchronize()` 后重投，不等于真实 guest EOIR
一定会让 hypervisor 获得执行机会。开发中必须同时证明：

1. software level LR 的 EOI/deactivation 完成可被 host 观察；
2. guest `EOImode=0` 的 EOIR 和 `EOImode=1` 的 DIR 都有正确路径；
3. line 仍 asserted 时完成后重新 pending/refill；
4. line 已 lower 时 active instance 完成但不重投；
5. 没有 timer、设备或其他偶然 VM exit 时也能推进；
6. maintenance IRQ 只经过一次平台 acknowledge/dispatch/EOI transaction。

首选仍是模块 3 的方案：

- software level LR 设置 EOI maintenance；
- 解析并注册 typed GIC maintenance PPI；
- handler 只记录当前 `(VM, vCPU, bind generation)` observation；
- target vCPU 的 pinned exit/entry 路径完成 fold/refill。

若改用其他机制，必须提供同等的无偶然外部事件进展证明，不能用周期 timer
exit 或测试 backend 主动 `synchronize()` 代替。

### 4.4 LR refill 和 HCR policy 只能选择一套模型

两套设计不能混用：

- 模块 3：active 保持 LR-resident，outside-LR 只保存 pending，使用 UIE，
  NPIE/LRENPIE 关闭，EOICOUNT 为 0；
- PR #1718：允许 active delivery spill 到软件队列，使用 UIE、NPIE、
  LRENPIE 和 EOICOUNT 完成 active-outside-LR。

在开发 PR 3.3 前必须明确选择。若保留模块 3 范围，继续采用第一套，因为：

- 状态空间更小；
- level EOI 和 epoch owner 更容易审计；
- 不需要在本模块证明 active-outside-LR 的优先级、EOICOUNT 和抢占语义；
- future 完整 VGIC 可以在独立设计中扩展，而不污染首轮固定 SPI 链路。

不得只从 PR #1718 复制 HCR bit 设置，却仍按模块 3 的 active-resident
controller 不变量 fold；这会产生两套互不匹配的完成协议。

### 4.5 wake 必须针对 target 且不成为状态源

允许最终不保留名为 `ArmInterruptRouter` 的独立结构，但边界必须等价：

- controller 锁内只产生 target service hint；
- 锁释放后才查询 runtime/task/pCPU；
- wake 失败不清 controller pending；
- 源 CPU不能获取目标 vCPU backend lock或写目标 ICH；
- target 迁移竞争下仍保证最终被唤醒；
- stale/duplicate wake 在 target refill 时是成功 no-op；
- 正常单 target SPI 不以 `notify_all()` 作为最终精度边界。

如果复用 PR #1718 的 `GicV3VcpuWake`，应把它适配到
`VmInterruptSender` 或等价的 target-specific runtime capability，而不是让
`arm_vgic` 知道 AxVM runtime。

## 5. 对五个模块 3 PR 的影响

### 5.1 PR 3.1

当前 `fix/armINTID` 与 PR #1718 在 `arm_vcpu`、AArch64 AxVM adapter 和旧
`arm_vgic` 文件上直接冲突。

可独立保留：

- `arm-gic-driver` 的 `ICH_VTR_EL2.IDbits` 字段修正；
- INTID 256 和边界值测试意图；
- 所有窄化转换必须 checked 的错误契约。

需要在状态所有权确定后再决定：

- `ArmVirtualIntId` 是否继续属于 `arm_vcpu`；
- direct LR encode/write API 是否存在；
- legacy `Vgicd` 修复是否仍有对象可改。

若采用 PR #1718 的 `arm_vgic`-owned ICH state，PR 3.1 不能原样合入，应拆出
driver capability 修正，并把无损 INTID 测试迁到新的 backend encode/decode
边界。

### 5.2 PR 3.2

per-vCPU save/restore 的功能目标与 PR #1718 一致，但所有权不同。开发前
必须先形成一个独立决策：

- 方案 A：raw ICH context 属于 `arm_vcpu`，`arm_vgic` 只拥有领域状态；
- 方案 B：context 属于 `arm_vgic` per-vCPU binding，AxVM 提供本地寄存器
  backend。

两种都可以满足分层，但不能同时保存同一 HCR/VMCR/APR/LR。无论选择哪种，
仍需补 per-pCPU capability、placement common profile、CPU pin 和错误清理。

### 5.3 PR 3.3

PR #1718 的 canonical interrupt record、software overflow、priority refill、
callback-after-unlock 和 physical retirement 可作为 prior art，但不能直接视为
PR 3.3 已完成。必须先解决：

- fixed software SPI route；
- active-resident 与 active-outside-LR 二选一；
- software level LR EOI；
- maintenance PPI；
- EOImode 0/1 的真实完成路径；
- owner/epoch 或等价的 stale completion 防护；
- 不依赖偶然 VM exit 的 LR refill。

### 5.4 PR 3.4

这是冲突最集中的 production cutover：

- PR #1718 的通用 planner 会替换当前 `InterruptFabric`/factory/config 假设；
- PR #1718 的 direct wake callback 会绕过模块 3 的 sender kick；
- AArch64 immutable plan 与模块 3 两阶段构造目标相近，可复用思想；
- 不应为了使用 planner 把 ITS、physical MSI 和其他架构迁移一起带入本 PR。

若要吸收 planner，先拆成独立、跨架构可审查的前置 PR；模块 3.4 只消费稳定
能力，不在 production cutover 中同时重写公共配置和四个架构。

### 5.5 PR 3.5

PR #1718 的 QEMU GICv2/GICv3+ITS case 在该快照中主要证明 Linux guest
启动，guest shell 直接输出成功 marker。它不能代替模块 3.5 的功能证据。

模块 3.5 仍必须使用 synthetic line producer 和专用 EL1 SMP2 guest覆盖：

- fixed remote route；
- INTID 256；
- edge exactly-once observable delivery；
- level 在 asserted 时 deactivate 后重投、lower 后停止；
- 17 个 SPI 的 LR pressure/refill；
- vCPU 1 `WFI` 下的跨 pCPU 定向 wake；
- host 日志确认 LR 只由 target vCPU 所在 pCPU 操作。

## 6. 推荐的复用与拆分顺序

如果 PR #1718 后续继续推进，推荐按以下顺序协调，而不是互相整 PR rebase：

1. **先确定唯一 ICH/VGIC 状态所有权。**
   这是 PR 3.1、3.2、3.3 能否复用代码的决策门。
2. **独立吸收 typed interrupt namespace 和 wired line。**
   `ControllerInputId`、`WiredIrqInput`、`IrqLine`、shared-level wired-OR
   可以成为公共前置，但要保持 device core 与 AxVM runtime 解耦。
3. **独立吸收资源 planner 时只提交 planner。**
   不与 VGIC、ITS、物理 SPI和四架构 production cutover捆绑。
4. **在 `arm_vgic` 最低层统一状态机。**
   加入模块 3 fixed route、capability、EOI/maintenance约束后再接 AxVM。
5. **最后进行唯一一次 production cutover。**
   旧 controller、旧 fabric和新 core不能同时接收同一 device line。
6. **ITS/LPI、物理 SPI/MSI和完整 MMIO另立后续模块。**
   它们可以复用同一 canonical core，但不作为模块 3 完成条件。

如果 PR #1718 不拆分并以当前整体方案合入，则模块 3 的五份详细设计必须重新
基于合入后的代码编写，不能只更新文件路径；尤其要重新定义 PR 3.1–3.4 的
交付物和状态所有权。

## 7. 开发前和合入前检查

每个模块 3 PR 开发前：

- [ ] 获取 PR #1718 最新 head，重新比较 changed files 和公开 API；
- [ ] 确认本 PR 不会创建第二个 pending/active/route/ICH 状态源；
- [ ] 确认是否依赖尚未合入的 typed line、planner 或 VGIC core；
- [ ] 在 PR 描述中记录采用、替代或拒绝 PR #1718 对应设计的理由；
- [ ] 若共同修改公共接口，先确定合入顺序，禁止两边各自增加兼容 shim。

production cutover 前：

- [ ] 软件 SPI 在 guest 未写 IROUTER 时已按 host fixed route 可投递；
- [ ] 不存在隐式 vCPU 0；
- [ ] capability 覆盖所有 placement CPU；
- [ ] GICv3 level EOI 在没有其他 VM exit 时仍会重投；
- [ ] LR overflow 有真实 maintenance/refill 进展；
- [ ] wake 精确指向 target，迁移竞争下不丢；
- [ ] passthrough、SGI/PPI/IPI 和非 AArch64 路径仍走各自既有模型；
- [ ] 功能 QEMU case 检查中断结果，不只检查 guest 启动 marker；
- [ ] CI 证据来自最终 rebase 后的 head。

## 8. 当前结论

PR #1718 可以作为重要 prior art 和代码来源，但在解决以下三项前，不能作为
模块 3 已完成或可直接依赖的实现：

1. software SPI fixed explicit route；
2. per-placement ICH capability；
3. GICv3 level EOI/maintenance 驱动的确定性重投。

此外必须先选择唯一的 ICH state 所有权和 LR overflow 模型。完成这些决策后，
再判断复用 PR #1718 的 controller/backend，还是继续模块 3 当前
`arm_vcpu context + ArmVgicDeliveryPort` 方案。
