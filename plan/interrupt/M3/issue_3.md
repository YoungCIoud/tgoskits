# 模块 3 issue：AArch64 emulated SPI 分层投递

## 问题

模块 1 已建立 `VmInterruptSender`、runtime dispatcher 和目标 vCPU run loop
消费边界，但 AArch64 还不能把 emulated device 的 SPI 通过这条路径可靠地送入
目标 vCPU 的 GICv3 List Register（LR）。

当前实现至少存在以下阻塞：

1. `VirtualInterruptId` 虽然使用 `u32`，`arm_vcpu` 的 host 注入边界仍把
   `usize` 转成 `u8`，INTID 大于 255 时会静默截断。
2. LR 没有空槽时直接 panic；dispatcher 已 drain 的事件也没有可靠的
   pending/refill 状态源。
3. 当前 `arm_vgic` 没有可供 Router 使用的 SPI line、固定路由、pending、
   active 和 inflight 状态边界。
4. level line 的 asserted 状态、guest deactivate、maintenance interrupt
   和 deactivate 后重投没有闭环，且必须区分 EOIR priority drop 与
   `EOImode=1` 下后续 DIR deactivation。
5. `ICH_HCR_EL2`、`ICH_VMCR_EL2`、APR 和 LR 等虚拟 CPU interface 状态没有
   按 vCPU 保存恢复，vCPU 换入换出或迁移时可能串扰。
6. AArch64 初始化路径在设备内部创建 VGIC wrapper，Router、MMIO wrapper 和
   vCPU delivery path 无法共享同一个 controller core。
7. 当前 AArch64 配置没有 emulated GICv3 SMP2 测试，也没有通过 `IrqLine`
   发出 SPI 的可执行测试设备。

继续只在 Router 中把 line 转成一个最终 `PendingVcpuInterrupt`，会留下
raise 后 lower、LR 满载、deactivate 重投和过期事件仍被注入等错误。本模块
必须先补齐 VGIC/ICH 底层语义，再接通 Router。

## 使用者与场景

直接调用方是 AArch64 emulated device factory、`InterruptFabric`、
`ArmInterruptRouter` 和目标 vCPU run loop。

验收场景是：

1. vCPU 0 上运行的测试 guest 写测试设备 MMIO；
2. 测试设备通过 `IrqLine` pulse 或 raise 一个 SPI；
3. 固定静态路由把该 SPI 指向 vCPU 1；
4. Router 更新 VGIC 状态并通过 `VmInterruptSender` 唤醒 vCPU 1；
5. 只有 vCPU 1 所在的 pCPU 把该 SPI 写入自己的 GICv3 LR；
6. edge SPI 递送一次；level SPI 在 line 仍 asserted 时于 deactivate 后重投，
   guest 清除设备状态并 lower 后停止重投；
7. LR 满载时中断保留在 VGIC 软件 pending 中，LR 可用后 refill，不 panic、
   不丢失。

## 范围

本模块只完成：

- AArch64 host GICv3 virtualization interface；
- guest emulated GICv3；
- wired SPI，INTID 32–1019；
- host 配置提供的固定显式 `(INTID, target_vcpu_id)` 路由；
- device `IrqLine` 提供的 edge/level 语义；
- VGIC 保存 pending/active/line level 的 durable truth，LR 只作为
  vCPU 的 live/saved 硬件执行副本；
- runtime dispatcher 作为 kick/wake 通道；
- 目标 vCPU 上的 ICH context 保存恢复、LR fold、refill；
- GIC maintenance PPI 驱动的 deactivation 与 underflow/refill 处理；
- guest `EOImode=0/1` 在 active SPI 保持 LR-resident 条件下的正确语义；
- 跨 CPU 唤醒；
- 测试专用 synthetic SPI device 和 AArch64 GICv3 SMP2 QEMU 用例。

## 非目标

本模块不实现：

- guest-programmable `GICD_IROUTER`；
- 完整的 guest `GICD_ISPENDR`、`ICPENDR`、`ISACTIVER`、
  `ICACTIVER`、`ICFGR` 等 pending/active/config MMIO 语义；
- GICv2 guest 或 GICv2 host virtualization interface；
- SGI、PPI、virtual timer、guest IPI 路径迁移；
- ITS、LPI、MSI/MSI-X；
- GPPT/passthrough GIC 路径重构；
- host physical IRQ 转发；
- 同 vCPU 不经过 dispatcher 的直接注入优化；
- 可运行时修改的 host route 或 hotplug route。

SGI/PPI/IPI “不迁移”不表示可以吞掉 TDIR 捕获的既有 DIR。模块 3 只增加
保持既有 private interrupt deactivation 的 ICH compatibility helper，不改变
其 producer、route 或 pending 模型。

以上非目标不能通过短期结构实现。VGIC controller 必须保存唯一 durable
truth，固定路由和现有最小 enable 行为也必须通过 controller 的内部状态转换
完成。以后实现
`GICD_IROUTER` 或完整 guest MMIO 时，应只增加新的状态输入入口和寄存器适配，
不能重写 Router、sender、LR refill 或 deactivation 路径。

## 配置契约

在 VM device 配置中增加默认空的固定路由表：

```toml
[devices]
fixed_irq_routes = [
  { intid = 33, target_vcpu_id = 0 },
  { intid = 256, target_vcpu_id = 1 },
]
```

每项显式给出 SPI `intid` 和 `target_vcpu_id`。trigger 不进入路由表，由设备
`Resource::IrqLine` / `IrqLine` 声明。

配置必须在 VM prepare 阶段验证：

- INTID 必须在 32–1019；
- target vCPU 必须存在；
- 同一 INTID 不得重复配置；
- 每个实际解析的 emulated SPI line 必须有且只有一条固定路由；
- 未被任何设备使用的路由作为配置错误拒绝；
- 不允许缺省到 vCPU 0；
- passthrough/no-irq VM 保持现有配置行为，不要求该表。

## 功能工作包与 5 个 PR 映射

下面的 3A–3F 是功能工作包，不再等同于 PR。实际按五个可独立审查、逐步启用
的 PR 合入：

| PR | 功能工作包 | 详细设计 | producer 状态 |
|----|------------|----------|---------------|
| 3.1 | 3A：无损 INTID、ICH 字段和可恢复 LR error | [`design_3_1.md`](./design_3_1.md) | 不切换 |
| 3.2 | 3C 前半：per-vCPU ICH context、capability 和 CPU pin | [`design_3_2.md`](./design_3_2.md) | 不切换 |
| 3.3 | 3B + 3C 后半：VGIC state、TDIR/maintenance、fold/refill | [`design_3_3.md`](./design_3_3.md) | 仅直接构造测试 |
| 3.4 | 3D + 3E：配置、初始化、Router、sender 和 target consumer | [`design_3_4.md`](./design_3_4.md) | 在此切换 |
| 3.5 | 3F：synthetic device、EL1 guest 和 SMP2 QEMU | [`design_3_5.md`](./design_3_5.md) | 增加端到端证据 |

依赖顺序为 `3.1 → 3.2 → 3.3 → 3.4 → 3.5`。3.1–3.3 不允许把真实设备
接入半成品路径；3.4 是唯一 production 接线 PR；3.5 不应承担底层语义修复。

### 3A：INTID 与 ICH 寄存器边界

- [ ] 将 AArch64 virtual interrupt host boundary 从 `u8` 扩宽为无损
  `u32`/typed INTID。
- [ ] 在进入 ICH LR 编码前使用无损、checked virtual INTID；direct
  SGI/PPI 继续接受 0–31，SPI 专用 `ArmSpiIntId` 在 PR 3.3 校验 32–1019。
- [ ] 修正 `arm-gic-driver` ICH wrapper：增加 LR 软件 EOI overlay，把当前
  与 PREBITS 重叠的 VTR.IDBITS 修正为 1 bit；复用并测试已有
  UIE/LRENPIE/TDIR、MISR/EISR、VTR.TDS 字段，不重复定义。
- [ ] 将“无空闲 LR”从 panic 改成内部可恢复结果。
- [ ] 测试 virtual INTID 31、255、256、1019 和非法 1020，确认没有截断；
  SPI boundary 另测试 31 非法、32 合法。
- [ ] 消除现有 `Vgicd` 512 项数组/未校验 `ISENABLER` 索引对合法
  INTID 512–1019 的越界风险；非法或未绑定 SPI 访问返回确定结果。
- [ ] 本阶段不把现有 producer 切换到未完成的新路径。

### 3B：VGIC SPI 状态与固定路由

- [ ] 在 `arm_vgic` 中增加独立于 `AxVM`/runtime 的 SPI controller state。
- [ ] controller 只按 prepare 验证后的 route 创建 SPI record；module-owned
  SPI 的最小 enable MMIO 委托这些 record，不能保留旧 `VgicInt` enable
  作为第二真源。
- [ ] controller 保存 trigger、enabled、line asserted、pending latch、
  active、带 delivery epoch 的 deactivation request/inflight 和固定 target；
  epoch 在 VM 生命周期内不复用。
- [ ] edge pulse、level raise/lower、delivery commit、LR fold 和 deactivate
  都通过 controller 的命名状态转换完成。
- [ ] controller 除了“有新 deliverable”还返回 target service hint：
  inflight edge 再 pulse、inflight level lower 时必须让目标 vCPU reconcile
  resident LR。
- [ ] 路由配置使用 typed INTID 和 typed vCPU ID，不使用裸 `usize` 猜测。
- [ ] sender、wake、IPI 和 LR system register 访问不进入 `arm_vgic`。
- [ ] 保留现有最小 enable 行为并接入同一状态；固定配置的 module-owned SPI
  在 prepare 时初始化为 enabled，端到端验收不依赖 guest enable MMIO，也
  不扩展完整 guest MMIO。
- [ ] 覆盖 edge 合并/重触发、edge active+pending、level deassert/
  deactivate、错误 target 和重复 route 的确定性单元测试。

### 3C：per-vCPU ICH context、deactivation 与 refill

- [ ] 为每个 vCPU 保存 `ICH_HCR_EL2`、`ICH_VMCR_EL2`、实现支持的 APR 和
  LR 状态。
- [ ] 根据所有允许承载该 VM vCPU 的 pCPU 建立 ICH capability profile：
  取共同 LR capacity，校验 ID bits/TDS，并要求首版
  priority/preemption/APR shape 兼容；不假设每个 pCPU 的
  `ICH_VTR_EL2` 完全相同。
- [ ] `ArmPerCpu::hardware_enable()` 在本地采集/publish profile；VM prepare
  查询 registry，不跨 CPU 直接读 ICH sysreg，缺少 profile 时显式失败。
- [ ] emulated SPI prepare 要求共同 LR capacity 至少为 2；UIE 在零个或
  一个 valid LR 时断言，单 LR profile 在本模块不使用 NPIE 的 policy 下
  明确 Unsupported，而不是进入 maintenance storm。
- [ ] bind/guest entry 前恢复当前 vCPU ICH state，退出/unbind 时 fold 并
  保存，离开后不得把该 vCPU 的 LR 留给其他 vCPU。
- [ ] 将 bind→guest run→unbind 收敛到同一个 CPU pin/current-vCPU
  publication 事务，deferred work 在 unbind 和撤销 publication 后执行；
  不依赖单 pCPU affinity 掩盖迁移窗口。
- [ ] target-side refill 只在当前 vCPU/pCPU 上访问 banked ICH 寄存器。
- [ ] 软件 level LR 设置 EOI maintenance；仍有软件 pending 且 LR 已满时
  按 outside-LR pending summary 配置 UIE；本模块不使用 NPIE，禁止
  active-outside-LR 并保持 LRENPIE 关闭。
- [ ] `somehal` GICv3 FDT probe 从 GIC 节点 `interrupts` 解析 maintenance
  PPI，在 dynamic GIC domain 中经 `ax-plat`/`ax-hal` capability 发布
  typed `IrqId`；AxVM 不直接依赖 `somehal`，也不硬编码 PPI 25。
- [ ] AxVM 在所有 pCPU 完成 virtualization enable 后通过现有架构 hook
  全局注册一次 per-CPU maintenance action；VM prepare 只校验 capability
  和 CPU coverage，不按 VM 重复注册，host-lifetime owner 保存
  `IrqHandle`。hook 的注册状态/错误保存在 registry；缺失 PPI 不阻止纯
  passthrough host 初始化，但 emulated VM prepare 必须返回原始错误上下文。
- [ ] 因新增 AArch64 platform capability，按仓库规范同步更新
  `.claude/skills/arch-platform-porting/` 中受影响的动态平台契约说明。
- [ ] 明确 AArch64 host IRQ 的唯一 transaction owner：当前
  `fetch_pending_host_irq()` 已执行平台 dispatch，deferred hook 不得再次
  acknowledge/EOI 同一个 maintenance PPI。
- [ ] AArch64 覆盖默认 `after_external_interrupt()`，只保留 timer event
  check，不再第二次 `dispatch_host_irq(vector)`；其他架构默认行为不变。
- [ ] IRQ framework/GIC dispatch 是 host PPI transaction 的唯一 owner；
  maintenance action 只记录带 `(vm_id, vcpu_id, bind_generation)` 的
  per-pCPU observation，不能用泄漏给下一 vCPU 的裸 bool。target vCPU
  `unbind` 重验 owner并在清 ICH 前 fold MISR/EISR/LR，下一次 bind/entry
  refill。
- [ ] level 在 Pending LR 中 lower 时撤销 resident Pending；软件 level
  active 期间不主动构造 Active+Pending。
- [ ] edge 在 Active 期间再次 pulse 时将 resident LR 收敛为
  Active+Pending，不依赖 level EOI maintenance。
- [ ] LR 满载只保留软件 pending，不 panic、不丢失。
- [ ] active emulated SPI 在 deactivate 前不得从该 vCPU 的 live/saved LR
  集合驱逐；否则返回明确 unsupported/error。
- [ ] 同时覆盖 `EOImode=0` EOIR-deactivate 和 `EOImode=1`
  EOIR priority drop→DIR-deactivate；fold 只在 LR active bit 实际清除后
  完成 active/inflight。
- [ ] emulated mode 要求所有允许 pCPU 的 `ICH_VTR_EL2.TDS=1`；所有
  emulated VGICv3 vCPU guest entry 始终开启 TDIR，缺少 capability 时
  prepare 失败。
- [ ] TDIR 捕获的 module-owned SPI DIR 写入 controller deactivation
  command；只有 LR owner delivery port 本地清 active，cross-vCPU DIR
  通过 sender kick/IPI 通知 owner。
- [ ] LR slot map 与 deactivation command 携带同一个 delivery epoch；
  owner 在释放旧 LR slot/refill 同 INTID 新 instance 前消费或确认 command，
  迟到 command 因 epoch 不匹配不得清除新 instance，同 epoch 重复 DIR
  幂等合并。
- [ ] TDIR 捕获非 module-owned SGI/PPI 时委托 ICH compatibility helper，
  不迁移其 producer 路径，也不能吞掉既有 deactivation。
- [ ] level line 在 deactivate 后仍 asserted 时重新 pending；lower 后停止重投。

### 3D：配置、所有权与两阶段初始化

- [ ] 将 `fixed_irq_routes` 从 `axvmconfig::VMDevicesConfig` 经
  `os/axvisor/src/config.rs::build_axvm_config`、`AxVMConfigParams` 完整传播
  到 `AxVMConfig`，增加 `serde(default)`、旧配置兼容和完整校验。
- [ ] AArch64 `init_vm` 获取 `AxVMRef`，用于构造 `VmInterruptSender`。
- [ ] 初始化顺序改为 controller core → sender → per-vCPU delivery port →
  Router/Fabric → VGIC MMIO wrapper → 其他设备。
- [ ] `AxvmArmVcpu` 使用 AxVM-private create config 组合
  `ArmVcpuCreateConfig` 与可选 delivery port；delivery port 持共享
  controller，`VmInterruptSender` 作为同一 AxVM-private vCPU 组合中的兄弟
  字段，只在 delivery 返回 target service hint 后调用；二者都不进入
  `arm_vcpu`，`arm_vcpu` 也不依赖 `arm_vgic`。
- [ ] MMIO wrapper、Router 和 vCPU delivery port 共享同一 controller core。
- [ ] AArch64 emulated prepare 认领 `InterruptController` 配置，从 common
  legacy fallback 中排除该项，用预创建 controller 构造并注册 wrapper；
  拒绝重复 controller，不向通用 build context 增加 `Any`/downcast 服务。
- [ ] emulated GICv3 显式拒绝 `prepare_with_factories()` 提供的任意 custom
  Fabric，不能绕过/替换 architecture-owned Router；synthetic factory 通过
  test feature 进入正常 prepare。其他模式和架构的 provided 流程不变。
- [ ] 设备 bundle 构造完成、对外可见前，根据 `Resource::IrqLine` 将
  trigger 与固定 route 绑定到 controller。
- [ ] prepare 失败必须回滚整个未提交设备/controller bundle。
- [ ] VM reset 后旧 Router、旧 line 和旧 controller 不进入新 controller；
  并发迟到的旧 sender kick 在新 delivery port 上成为 no-op。
- [ ] passthrough/GPPT 初始化路径保持原行为。

### 3E：ArmInterruptRouter 与 runtime kick

- [ ] 新建 `ArmInterruptRouter` 并实现 `IrqSink`。
- [ ] `set_level`/`pulse` 只更新 VGIC controller 状态。
- [ ] controller 从“无待处理工作”变为“目标 vCPU 需要 refill 或 resident
  reconcile”时，Router 在释放 controller 锁后调用 `VmInterruptSender`。
- [ ] dispatcher 中的 AArch64 `PendingVcpuInterrupt` 是 kick/hint，不是
  pending/active 的状态真源。
- [ ] AArch64 `ArchOps::inject_vcpu_interrupt` 不直接按 event 写一个 LR，
  而是要求当前 vCPU delivery port fold/refill controller 当前状态。
- [ ] 通用 `ArchOps` 增加默认 no-op 的 `before_vcpu_entry`，两个 queue
  drain 后、guest run 前只调用一次；AArch64 在该 hook 即使没有新 kick 也
  执行一次轻量 fold/refill，保证
  暂时 sender 失败或合并 kick 不会永久饿死当前 controller pending；
  reset 后旧 controller pending 不迁移。
- [ ] stale kick 在 controller 中没有可递送中断时成为成功 no-op。
- [ ] Router 不直接访问远程 vCPU、task、wait queue 或 ICH 寄存器。
- [x] `AxVM::pulse_interrupt()` 的 machine-lock 重入已在 PR #1679
  （commit `5155111ef`）修复：锁内只快照内部 Fabric capability，释放后再
  调用 sink；AxVM 不公开 Fabric。
- [x] 已有
  `interrupt_pulse_runs_after_snapshot_lock_is_released` 确定性测试，并在
  当前 plan 基线验证通过。PR 3.4 只需保留该回归；新增 level wrapper 必须
  复用相同 snapshot 模式。

### 3F：synthetic device 与端到端验证

- [ ] 增加仅测试配置可用的 synthetic SPI MMIO device，不进入默认产品配置。
- [ ] 为 synthetic device 分配数值稳定的 test-only
  `EmulatedDeviceType::SyntheticIrq = 0xDF`；variant identity无条件保留，
  factory实现由 `test-synthetic-irq` feature逐层传播。未启用时构造明确
  Unsupported，不能复用 `Dummy`、fallback或占用生产设备语义。
- [ ] device factory 通过正常 `DeviceBuildContext::resolve_irq()` 获取
  `IrqLine`。
- [ ] 严格校验 synthetic config：`irq_id` 是唯一 line，`cfg_list` 只编码
  edge/level kind，未知 kind、长度、访问宽度或寄存器返回 typed error。
- [ ] edge 实例支持 MMIO pulse；level 实例支持 raise，并由 guest
  acknowledge/clear MMIO 写执行 lower。
- [ ] synthetic device 提供只用于测试报告的 MMIO register；guest 以结构化
  phase/result 值报告进度，device 在 host console 输出稳定 marker。测试不依赖
  guest UART，也不把报告 register 作为中断注入后门。
- [ ] 增加 emulated GICv3 SMP2 guest/VM 配置，SPI 固定路由到 vCPU 1。
- [ ] 新增
  `test-suit/axvisor/normal/emulated-gicv3-spi-smp2/qemu-aarch64.toml`，
  以及同目录专用 `build-aarch64-unknown-none-softfloat.toml`；后者沿用普通
  AArch64 wrapper 的 host target/features，但使用本用例自己的 VM config
  和 guest build 描述，避免影响 smoke build group；该目录与
  `normal/qemu/` 平级，以符合当前不发现嵌套 build wrapper 的 discovery
  规则。
- [ ] 增加
  `test-suit/axvisor/guests/emulated-gicv3-spi-smp2/` 下的专用 `no_std`
  AArch64 EL1 guest；manifest 用独立 `[workspace]` 退出根 workspace，
  提交独立 lockfile 并以 `--locked` 构建，不提交生成的 ELF/raw binary。
- [ ] 为 AxVisor QEMU build wrapper 增加窄且 typed 的 `guest_builds`
  描述。runner 必须先用 pinned toolchain 构建 guest、objcopy 为 raw binary
  到 `tmp/axbuild/axvisor-guests/`，再构建通过 `image_location = "memory"`
  嵌入该产物的 AxVisor；构建失败或产物缺失立即失败。
- [ ] QEMU 显式使用 GICv3 与至少 2 个 pCPU；VM 的 vCPU 0/1 固定到不同
  pCPU（`phys_cpu_ids = [0, 1]`，`phys_cpu_sets = [1, 2]`）；测试 VM 使用
  virtualized address-space policy，不 passthrough QEMU root device tree。
- [ ] BSP guest 通过 PSCI `CPU_ON` 启动 vCPU 1，以共享内存握手确认其 IRQ
  handler 与 GIC CPU interface 已就绪；模块内 SPI 在 VM prepare 时按固定
  配置启用，不以 guest GICD enable MMIO 作为本模块验收前提。
- [ ] guest 在 vCPU 0 触发设备，在 vCPU 1 观察中断；vCPU 1 等待阶段使用
  `WFI`，使跨 CPU wake 成为必经路径。
- [ ] 覆盖 INTID 256，证明全链路无 `u8` 截断。
- [ ] 覆盖 edge 一次递送、level deactivate 重投、lower 后停止和 LR refill。
- [ ] LR pressure 使用 17 个不同 edge SPI 实例；目标 vCPU 屏蔽 guest IRQ
  后由源 vCPU 全部 pulse，再解除屏蔽并确认每个 INTID 各处理一次。
- [ ] 保留并运行现有 AArch64 passthrough 构建/测试回归。

## 并发与错误要求

- Router 和 controller 方法可能运行在 vCPU exit 或设备上下文，不得睡眠。
- controller 使用 IRQ-safe/non-sleeping lock。delivery port 仅可在该锁内完成
  状态转换和短小的本地 ICH 事务；controller crate 本身不依赖 ICH sysreg，
  且锁内不调用 sender、wake、IPI、日志回调或设备代码。
- 固定锁顺序为：controller state → 释放 → sender/runtime。不得反向持有
  runtime/machine lock 后进入 controller。
- target refill 在持有当前 vCPU 独占访问权时执行；远程 CPU 不访问该 vCPU
  的 ICH state。
- sender 失败不清除电气 line/pending 真相；调用方得到显式错误，后续
  guest-entry polling/refill 仍能恢复投递。
- 无 route、非法 INTID、非法 target、LR capacity/TDS 不兼容和 maintenance
  IRQ 不可用都必须在 prepare 或明确操作边界返回错误，不能静默 fallback。

## 验证

每个子模块运行受影响 crate 的最小单元测试，并在有 Rust 改动时执行：

```bash
cargo fmt --all
cargo xtask clippy --package arm_vgic
cargo xtask clippy --package arm_vcpu
cargo xtask clippy --package axdevice
cargo xtask clippy --package axvm
```

按实际 xtask 参数完成 AArch64 AxVisor build，并在 3F 完成：

```bash
cargo xtask axvisor test qemu --arch aarch64 --test-group normal \
  --test-case emulated-gicv3-spi-smp2
```

最终运行证据必须来自当前实现 head，并记录：

- edge/level 测试 guest 的成功标志；
- 中断处理所在 guest vCPU；
- host 日志中的目标 vCPU/pCPU 与 INTID；
- INTID 256 无截断；
- level 重投次数及 lower 后停止；
- LR 压力用例完成且无 panic/丢失；
- passthrough 回归结果。

## 代码量与难度

预计生产 Rust 约 1,680–2,870 行，单元/集成测试、测试 guest、runner
支持和配置约 1,940–3,270 行，总变更约 3,620–6,140 行。该区间是五个 PR
分项估算之和，并包含为了
可审查性补齐的 capability、host IRQ 和 typed guest-build 边界，整体难度为高：

- PR 3.1 中高：INTID/寄存器字段正确性；
- PR 3.2 高：ICH save/restore 与 CPU pin；
- PR 3.3 最高：状态机、epoch、EOImode、maintenance 和 refill；
- PR 3.4 中高：构造原子性、Router/runtime 接线；
- PR 3.5 中高：可重复的 EL1 SMP2 与 LR pressure 证据。

详细分项估算见 [`design_3.md`](./design_3.md#19-代码量与难度预估)。

## 完成标准

- [ ] AArch64 emulated GICv3 SPI 只经
  `IrqLine → ArmInterruptRouter → VGIC state → sender kick →
  target vCPU refill → LR` 投递。
- [ ] 固定路由显式、可校验，不存在隐式 vCPU 0。
- [ ] edge、level deactivate 重投、两种 EOImode、LR refill 和跨 CPU
  唤醒全部有确定性证据。
- [ ] 大于 255 的 INTID 不截断。
- [ ] LR 满载不 panic、不丢中断。
- [ ] ICH state 按 vCPU 隔离并支持调度换入换出。
- [ ] guest-programmable `GICD_IROUTER` 和完整 MMIO 状态语义虽未实现，
  但以后只需增加 controller 状态输入/MMIO adapter，不需要重写投递链路。
- [ ] GICv2、SGI/PPI/IPI、ITS/LPI、GPPT/passthrough 均未被意外扩入范围。
