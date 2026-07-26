# AxVisor 设备中断路由重构设计（修订版）

## 1. 背景

当前 AxVisor 的设备中断路径按架构分别演化，主要存在以下问题：

1. `IrqLine` → `IrqSink` 路径只有 RISC-V 已完整接通，其他架构仍依赖运行时特判。
2. x86 运行时直接理解 IOAPIC、PIT、串口和 vCPU 注入细节，设备层与架构层边界不清晰。
3. 跨 vCPU 中断递送依赖 `VmRuntimeHandle::pending_interrupts`，设备中断、IPI 和直通 IRQ 的递送路径不一致。
4. LoongArch 的外部中断和 MMIO 后处理仍通过架构特定队列变体及设备管理器特判完成。
5. 当前资源模型不能声明和校验设备占用的 IRQ line。

本次重构只整理设备线中断到虚拟中断控制器、以及虚拟中断控制器到目标 vCPU 的递送边界，不扩展新的中断控制器功能。

## 2. 设计目标

### 2.1 必须实现的目标

1. **设备只表达线中断语义。**
   设备通过 `IrqLine::raise()`、`lower()`、`pulse()` 发出事件，不调用 IOAPIC、VGIC、vPLIC、PCH-PIC 或 vCPU API。

2. **每种架构拥有独立的 Router。**
   Router 负责把架构无关的 IRQ line 操作转换成该架构虚拟中断控制器的输入和输出。

3. **vCPU 递送发生在目标 vCPU 的运行上下文。**
   远程 vCPU 不被其他 pCPU 直接修改。Router 输出先进入 VM runtime 队列，由目标 vCPU run loop 消费并调用架构注入接口。

4. **运行时负责唤醒，Router 不依赖调度器实现。**
   Router 可以请求“向 vCPU N 递送中断”，但任务唤醒、等待队列和 host IPI 属于 `VmRuntimeHandle`。

5. **迁移按架构独立进行。**
   任一架构迁移期间，尚未迁移的架构继续使用旧路径；共享层变更不得要求所有 Router 同时完成。

6. **保持现有中断控制器能力范围。**
   当前不支持的 IOAPIC logical destination、x2APIC destination、共享 IRQ 等能力不在本次重构中新增。

### 2.2 非目标

以下内容明确不属于本次重构：

- 新建通用虚拟时钟后端或改变 PIT 的时间源；
- 增加 HPET、MSI/MSI-X、ITS、IMSIC 等新设备能力；
- 实现 IOAPIC logical destination、lowest-priority delivery 或 x2APIC 扩展；
- 改变 guest 可见中断控制器寄存器 ABI；
- 新增共享 IRQ 注册能力；
- 重构 host IRQ framework、任务调度器或等待队列；
- 优化同 vCPU 注入的快速路径；
- 统一不同架构的 EOI 协议。

PIT 在本次重构后仍由现有 vCPU exit/timer hook 调用 `tick()`。本次只把“到期后的 IRQ 传播”改成事件路径，不解决 guest 长时间无 VM exit 时的定时器饥饿问题。

## 3. 核心边界

重构后的路径分为三个独立边界：

```text
Device Core
  │ raise / lower / pulse
  ▼
IrqLine → InterruptFabric → Architecture Router
                                  │ routed interrupt
                                  ▼
                         VmInterruptSender
                                  │
                                  ▼
                    VmRuntimeHandle / Dispatcher
                                  │ queue + wake + host IPI
                                  ▼
                         target vCPU run loop
                                  │
                                  ▼
                    ArchOps::inject_vcpu_interrupt
```

### 3.1 Device Core

设备只持有在构造时解析好的 `IrqLine`。设备不知道 VM ID、vCPU ID、虚拟中断控制器类型和 host 调度 API。

### 3.2 InterruptFabric

`InterruptFabric` 只负责：

- 为设备解析 `IrqLine`；
- 把 `set_level` / `pulse` 转发给当前架构 Router；
- 为平台 IRQ 适配层提供 VM 级 `set_interrupt_level` / `pulse_interrupt` 操作。

`InterruptFabric` 不负责：

- 直接向 vCPU 注入中断；
- MMIO 写后的架构副作用 drain；
- EOI；
- downcast 到具体 Router；
- 任务唤醒或 host IPI。

### 3.3 Architecture Router

Router 是架构适配对象，实现现有的 `IrqSink`：

```rust
pub trait IrqSink: Send + Sync {
    fn set_level(&self, line: IrqLineId, asserted: bool) -> AxResult;
    fn pulse(&self, line: IrqLineId) -> AxResult;
}
```

不新增跨架构的 `InterruptRouter` 虚方法表。`X86InterruptRouter`、`ArmInterruptRouter`、`RiscvPlicIrqSink` 和 `LoongArchInterruptRouter` 是具体类型；Fabric 只保存它们 coercion 后的 `Arc<dyn IrqSink>`。

这避免为了 x86 EOI、LoongArch MMIO drain 或 IPI 注入向所有 Router 增加无意义方法。

### 3.4 VmInterruptSender

Router 需要递送虚拟中断时，通过稳定的 VM 级 sender 请求 runtime 处理：

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VirtualInterruptId(u32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingVcpuInterrupt {
    pub id: VirtualInterruptId,
    pub trigger: InterruptTriggerMode,
}

#[derive(Clone)]
pub struct VmInterruptSender {
    vm: Weak<AxVM>,
}

impl VmInterruptSender {
    pub fn send(
        &self,
        vcpu_id: usize,
        interrupt: PendingVcpuInterrupt,
    ) -> AxVmResult;
}
```

`VirtualInterruptId` 使用 `u32`，避免把 x86 的 `u8` vector 限制扩散到 GIC、PLIC 和 LoongArch。

`VmInterruptSender` 不缓存 `Weak<VcpuIrqDispatcher>`。它每次通过 `Weak<AxVM>` 查找当前 `VmRuntimeHandle`，因此 VM stop/start/reset 后不会引用旧 runtime。

VM 已销毁、runtime 尚未创建、VM 状态不接受中断、目标 vCPU task 未注册时，`send()` 必须返回明确错误，不得返回成功并静默丢弃中断。

## 4. VM runtime 递送模型

### 4.1 VcpuIrqDispatcher

`VcpuIrqDispatcher` 属于 `VmRuntimeHandle`，其生命周期与一次 Running/Paused/Stopping runtime 相同：

```rust
pub struct VcpuIrqDispatcher {
    pending: Mutex<BTreeMap<usize, Vec<PendingVcpuInterrupt>>>,
    vcpu_tasks: Mutex<BTreeMap<usize, AxTaskRef>>,
}

impl VcpuIrqDispatcher {
    pub fn register_vcpu_task(&self, vcpu_id: usize, task: AxTaskRef);

    pub fn enqueue(
        &self,
        vcpu_id: usize,
        interrupt: PendingVcpuInterrupt,
    ) -> AxVmResult<usize>;

    pub fn drain(&self, vcpu_id: usize) -> Vec<PendingVcpuInterrupt>;
}
```

`enqueue()` 只维护队列并返回目标 task 当前所在的 pCPU ID。它不调用 host API。

### 4.2 VmRuntimeHandle 编排

运行时入口负责完成整个递送动作：

```rust
impl VmRuntimeHandle {
    fn dispatch_vcpu_interrupt(
        &self,
        vcpu_id: usize,
        interrupt: PendingVcpuInterrupt,
    ) -> AxVmResult {
        let pcpu_id = self.irq_dispatcher.enqueue(vcpu_id, interrupt)?;
        self.notify_all();
        crate::host::task::send_ipi(pcpu_id);
        Ok(())
    }
}
```

第一阶段保持当前 `notify_all()` 行为，不在本次重构中增加定向 task wake API。后续若 runtime 提供可靠的定向唤醒能力，可单独优化。

### 4.3 目标 vCPU 消费

每次 vCPU run loop 进入 guest 前：

1. drain 当前 vCPU 的 `PendingVcpuInterrupt`；
2. 逐个调用架构注入边界；
3. 注入失败时记录包含 VM ID、vCPU ID 和 interrupt ID 的错误。

```rust
trait ArchOps {
    fn inject_vcpu_interrupt(
        vcpu: &AxVCpuRef<Self::VCpu>,
        interrupt: PendingVcpuInterrupt,
    ) -> AxVmResult;
}
```

共享实现调用 `vcpu.inject_interrupt_with_trigger(id, trigger)`，不得在默认路径丢弃 trigger。x86 adapter 把 edge/level 映射到 vLAPIC 注入参数，以维护 TMR/EOI 语义；AArch64、RISC-V 和 LoongArch adapter 也必须显式说明 trigger 在对应 Router/controller 中消费后，vCPU 注入操作为何不再区分模式。AArch64 的 GIC List Register、RISC-V CSR 和其他只能在当前 pCPU 安全访问的寄存器，都在目标 vCPU run loop 中更新。

同 vCPU 直接注入只是性能优化，不是第一阶段要求。第一阶段所有 Router 输出统一排队，先保证所有权和并发语义一致。

## 5. InterruptFabric API

```rust
pub struct InterruptFabric {
    mode: VMInterruptMode,
    sink: Option<Arc<dyn IrqSink>>,
}

impl InterruptFabric {
    pub const fn new(mode: VMInterruptMode) -> Self;

    pub fn with_router<R>(mode: VMInterruptMode, router: Arc<R>) -> AxVmResult<Self>
    where
        R: IrqSink + 'static;

    pub fn set_level(&self, line: usize, asserted: bool) -> AxVmResult;
    pub fn pulse(&self, line: usize) -> AxVmResult;
}
```

设备仍通过 `IrqResolver` 获取 `IrqLine`。架构 Router 的强类型 `Arc<R>` 由创建它的架构初始化代码或闭包持有；Fabric 不提供 `as_any()` 和 `router_ref()`。

AxVM 对外只暴露有状态校验的操作：

```rust
impl AxVM {
    pub fn set_interrupt_level(&self, line: usize, asserted: bool) -> AxVmResult;
    pub fn pulse_interrupt(&self, line: usize) -> AxVmResult;
}
```

不新增返回 `&InterruptFabric` 或克隆 Fabric 的公共访问器，避免泄漏 `Machine` 锁内引用或让调用者跨 reset 持有旧 Router。

## 6. IRQ line 资源声明

### 6.1 Resource

```rust
pub enum Resource {
    MmioRange { base: u64, size: u64 },
    PortRange { base: u16, size: u16 },
    SysReg { addr: u32, count: u32 },
    IrqLine {
        line: u32,
        trigger: InterruptTriggerMode,
    },
}
```

本次只支持独占 IRQ line。共享 IRQ 属于非目标，后续如果确有设备需要，再扩展 sharing policy。

### 6.2 冲突索引

不使用固定 `u64` 位图。GIC、PLIC 及现有 LoongArch 配置都可能使用大于 63 的 line。

```rust
irq_line_index: BTreeMap<u32, DeviceId>
```

注册规则：

1. 同一设备重复声明同一 line，返回 `InvalidResource`；
2. 不同设备声明同一 line，返回明确的资源冲突错误，并包含已有 `DeviceId`；
3. bundle 注册失败时回滚已经插入的 IRQ line；
4. IRQ line 不进入 MMIO/Port/SysReg dispatch 查找；
5. 不人为限制 line 上界，具体 Router 在实际解析时按架构能力返回 `OutOfRange`。

## 7. 初始化与所有权

### 7.1 必要的 API 调整

Router 需要持有 `Weak<AxVM>` 封装成的 `VmInterruptSender`。模块 1 只要求 `prepare` 以 `Arc` receiver 为后续 Router 构造 sender 提供条件：

```rust
impl AxVM {
    pub fn prepare(self: &Arc<Self>) -> AxVmResult;
}
```

架构 `init_vm` 获取 `&AxVMRef` 的调整随实际 Router 一起完成：AArch64、LoongArch 和 x86 分别在模块 3、4、5 修改；RISC-V 仅在后续确实需要创建 sender/Router 时修改。模块 1 不提前迁移尚无消费者的架构初始化接口。

### 7.2 两阶段设备初始化

Router 通常需要和 MMIO 总线共享同一个中断控制器 core。初始化顺序统一为：

```text
1. 创建 vCPU 元数据和 placement 信息
2. 创建架构虚拟中断控制器 core
3. 创建 VmInterruptSender
4. 创建具体 Router
5. 用 Router 创建 InterruptFabric
6. 将同一个控制器 core 包装成 MMIO/PIO Device 并注册
7. 构造其余设备；设备通过 Fabric 解析 IrqLine
8. 安装架构所需 hooks
9. 完成 vCPU setup 和地址空间映射
```

中断控制器不能再由通用设备构建流程内部临时创建后只留下 `dyn Device`。各架构初始化代码先创建 controller core，再把 wrapper 交给 DeviceManager。

### 7.3 引用关系

```text
AxVM resources ── owns ──> devices
AxVM resources ── owns ──> InterruptFabric ── Arc ──> Router
device ── owns ──> IrqLine ── Arc ──> Router
Router ── owns ──> controller core
Router ── owns ──> VmInterruptSender ── Weak ──> AxVM
VmRuntimeHandle ── owns ──> VcpuIrqDispatcher
```

Router 到 AxVM 只有弱引用，不形成强引用环。Router 不持有 runtime 或 vCPU task。

## 8. 各架构设计

### 8.1 RISC-V

RISC-V 已有完整的 `RiscvPlicIrqSink`，继续作为 Fabric sink 使用：

```text
device IrqLine
  → RiscvPlicIrqSink
  → vPLIC pending state
```

不为它增加空操作 `inject_to_vcpu()`。IPI 保持现有 `SendIPI` exit → runtime queue → 目标 vCPU 注入路径。

模块只需验证：

- 现有 `raise/pulse` 行为不变；
- vPLIC claim/complete 行为不变；
- secondary vCPU IPI 行为不变。

### 8.2 AArch64

`ArmInterruptRouter` 负责：

1. 校验输入 line/INTID；
2. 根据当前已支持的 VGIC 路由状态确定目标 vCPU；
3. 将 `PendingVcpuInterrupt` 交给 `VmInterruptSender`；
4. 保存 level line 必需的 asserted 状态，或委托给 VGIC core。

```text
device IrqLine
  → ArmInterruptRouter
  → VGIC route decision
  → VmInterruptSender
  → target vCPU run loop
  → write GIC LR on target pCPU
```

当前 `VGicD` 没有可直接使用的 `set_pending/clear_pending` 公共 API，因此实现前必须在 `arm_vgic` 中提供最小能力边界，例如“更新 SPI line 状态并返回目标 vCPU/待递送 INTID”。不得在 Router 中假设不存在的 API，也不得从远程 pCPU 直接写目标 vCPU 的 banked List Register。

本次不新增 GIC 路由模式，只遵循当前 VGIC 已支持的路由语义。

### 8.3 LoongArch

`LoongArchInterruptRouter` 负责 PCH-PIC 输入 line 到 EIOINTC 输出 vector 的转换，并把最终待递送中断交给 `VmInterruptSender`。

平台 IRQ 回调继续保留 `vm_id`、`vcpu_id`、`guest_vector`、`physical_irq` 参数。边界划分为：

- platform glue 使用 `physical_irq` 做 host route/mask bookkeeping；
- `guest_vector` 作为 guest PCH-PIC 输入 line；
- Router 根据 PCH-PIC/EIOINTC 当前状态决定最终 vector 和目标 vCPU；
- 只有已有硬件模型明确证明固定到 BSP 的路径才使用 vCPU 0，否则保留配置提供的 `vcpu_id`。

第一阶段保留 `ArchOps::after_mmio_write`，继续 drain PCH-PIC MMIO 写产生的输出事件。该 hook 是 LoongArch 架构适配，不进入共享 Fabric。是否让 PCH-PIC 在 `handle_write` 中直接发布事件可在后续单独重构，本次不扩展 `Device::handle` 返回值。

当平台 IRQ 和 MMIO 输出事件都已改为 Router → sender 路径后，才移除 `PendingInterrupt::External` 和旧 `queue_external_interrupt()`。

### 8.4 x86_64

#### 8.4.1 Router 状态

```rust
pub struct X86InterruptRouter {
    ioapic: Arc<EmulatedIoApic>,
    apic_id_to_vcpu: BTreeMap<u32, usize>,
    sender: VmInterruptSender,
}
```

Router 不持有 vCPU、runtime 或 host IRQ 全局状态。

#### 8.4.2 IOAPIC 输出

`IoApicInterrupt` 扩展为包含：

```rust
pub struct IoApicInterrupt {
    pub vector: u8,
    pub dest_apic_id: u8,
    pub level_triggered: bool,
}
```

本次继续只支持当前已有的 Fixed + Physical Destination。遇到 logical destination、unsupported delivery mode 或不存在的 APIC ID 时返回明确错误，不猜测目标 vCPU。

#### 8.4.3 raise/lower/pulse

IOAPIC core 增加最小 line-level API，使 `IrqSink` 语义真实成立：

```rust
fn set_gsi_level(&self, gsi: usize, asserted: bool) -> X86VlapicResult<Option<IoApicInterrupt>>;
fn pulse_gsi(&self, gsi: usize) -> X86VlapicResult<Option<IoApicInterrupt>>;
```

- `raise()` 设置 asserted level；
- `lower()` 清除 asserted level；
- EOI 后只有 line 仍 asserted 或存在新的 pending pulse 时才重新递送；
- `pulse()` 产生一次边沿语义，不把 line 永久保持为 asserted。

设备模型如果只会产生一次通知，使用 `pulse()`。真正的 level source 必须在 guest 清除设备状态时调用 `lower()`；不允许 Router 静默忽略 deassert。

#### 8.4.4 IPI

vLAPIC ICR 解码仍属于 `x86_vlapic`。它通过一个 x86 专用注入 hook 调用 `VmInterruptSender`，不经过设备 `IrqLine`，也不要求 `InterruptFabric` 提供 `inject_to_vcpu()`。

#### 8.4.5 EOI

EOI 保持 x86 专属，不进入共享 trait。三个现有 VMX/SVM EOI 来源统一调用同一个 vLAPIC/架构 hook，该 hook 捕获 `Weak<X86InterruptRouter>`：

```text
guest EOI exit
  → one canonical x86 EOI hook
  → X86InterruptRouter::handle_eoi(vector)
  → IOAPIC updates remote IRR
  → optional reinjection through VmInterruptSender
  → x86 host IRQ glue decides host unmask
```

只保留一个 EOI 入口，不能同时通过 `eoi_notify` 和 `after_interrupt_end` 重复调用。

host IRQ 的 edge/level 属性由 x86 host IRQ glue 保存。是否解除 host mask 不能只根据 guest IOAPIC redirection entry 的 trigger mode 推断。

#### 8.4.6 PIT 与串口

- PIT 的现有时间推进方式不变；`tick(now)` 到期后调用 IRQ0 `pulse()`；
- 串口在自身状态转为 interrupt pending 时调用 `IrqLine`；
- 删除运行时对 PIT/串口“是否有 IRQ”的查询；
- `IrqLine` 失败必须传播或记录，不能使用 `.ok()` 静默忽略。

## 9. 迁移计划

### 模块 1：共享资源与 runtime 通道

1. 新增 `Resource::IrqLine`。
2. 使用 `BTreeMap<u32, DeviceId>` 实现冲突检测和回滚。
3. 新增 `VirtualInterruptId`、`PendingVcpuInterrupt`。
4. 新增 runtime-owned `VcpuIrqDispatcher`。
5. 新增 `VmInterruptSender` 和 `VmRuntimeHandle::dispatch_vcpu_interrupt()`。
6. vCPU run loop 增量 drain 新 dispatcher，同时保留旧 `pending_interrupts` drain。
7. `InterruptFabric` 继续只保存 `IrqSink`，不新增 `inject_to_vcpu`、`drain_side_effects` 或 `as_any`。

模块 1 不改变任何架构现有中断生产者。
模块 1 的退出证据只覆盖共享类型、sender、runtime dispatcher、run-loop consumer 和迁移期双队列；不要求任何 Router 或架构 `init_vm` 入口提前迁移。

### 模块 2：RISC-V 基线验证

1. 保持现有 `RiscvPlicIrqSink` 接线。
2. 适配新的 Fabric 构造接口（如有必要）。
3. 验证 vPLIC line IRQ 和 IPI 路径。

该模块不新增空操作 Router 方法。

### 模块 3：AArch64 Router

1. 为 VGIC 增加 Router 所需的最小 line/route capability。
2. 创建 `ArmInterruptRouter`。
3. 通过 Fabric 向设备提供 `IrqLine`。
4. Router 输出经 sender/dispatcher，在目标 vCPU run loop 写 LR。
5. 保留现有 IPI 入口，只把其 runtime 递送收敛到 sender（若需要）。
6. 在构造 `ArmInterruptRouter` 时把架构 `init_vm` 调整为获取 `AxVMRef`。

### 模块 4：LoongArch Router

1. 创建共享的 `Arc<PchPicDevice>` 和 `LoongArchInterruptRouter`。
2. 平台 IRQ 回调改为调用 VM line IRQ API。
3. 保留 LoongArch `after_mmio_write`，但其输出改走 sender。
4. 确认 `physical_irq` 的实际功能后再移除旧 External 队列变体。
5. 不硬编码目标 vCPU，除非当前硬件模型的路由结果就是 BSP。
6. 在构造 `LoongArchInterruptRouter` 时把架构 `init_vm` 调整为获取 `AxVMRef`。

### 模块 5：x86_64 Router

建议继续拆分：

- **5a：初始化与 IOAPIC 共享所有权**；
- **5b：IOAPIC destination 和 line-level API**；
- **5c：PIT/串口使用 IrqLine**；
- **5d：host IRQ 转发使用 VM line API**；
- **5e：vLAPIC IPI hook 使用 sender**；
- **5f：统一唯一 EOI hook 并移除旧运行时查询接口**。

每一步必须保持 VMX 和 SVM 可编译；涉及其中一方特有注入语义时分别验证。
在 5a 创建 `X86InterruptRouter` 时，把 x86 架构 `init_vm` 调整为获取 `AxVMRef`。

### 最终清理

仅在所有旧生产者消失后：

1. 移除旧 `PendingInterrupt` 和 `pending_interrupts`；
2. 移除 `queue_interrupt()` / `queue_external_interrupt()`；
3. 移除已无调用者的设备类型特定查询 trait；
4. 移除被 Router 取代的 x86 轮询注入函数；
5. 保留仍有明确架构职责的 hook，例如 LoongArch MMIO 后处理，除非已有独立改动安全替代它。

## 10. 并发与错误处理约束

1. Router 的 `IrqSink` 方法可以在设备任务、vCPU exit 或 host IRQ adapter 中调用，不能睡眠。
2. Router 不直接锁目标 vCPU，不从远程 pCPU 写 banked system register。
3. dispatcher 锁内只做 map/queue 更新，不调用 wake、IPI 或外部回调。
4. queue 完成入队后才能执行 wake/IPI，目标 vCPU drain 后才能进入 guest。
5. controller core 的寄存器锁保持短小，释放后再调用 sender。
6. `Weak` 升级失败、目标不存在、line 越界和不支持的路由模式都返回显式错误。
7. host IRQ hard-IRQ 路径若不能安全分配，应使用已有预分配 pending 状态或 deferred path；本次不引入新的硬 IRQ 分配机制。
8. 不在中断路径使用 `.ok()`、裸 `unwrap()` 或静默默认目标。

## 11. 测试与验收

### 11.1 共享层

- IRQ line 0、63、64 和大于 255 的合法声明；
- 同设备重复 line 与跨设备冲突；
- bundle 中途失败后的 IRQ index 回滚；
- dispatcher 保留队列顺序、trigger 和 vCPU 隔离；
- drain 后队列为空；
- 未注册 vCPU、runtime 不存在和 VM 状态错误；
- 入队发生在 notify/IPI 之前。

### 11.2 架构 Router

- RISC-V：现有 vPLIC pending/claim/complete 回归；
- AArch64：INTID 不被截断，远程注入只在目标 vCPU 上写 LR；
- LoongArch：平台 route 的 guest vector 和目标 vCPU 得到保留，MMIO 输出不丢失；
- x86：destination 解析、edge pulse、level raise/lower、EOI 重注入、unsupported destination；
- x86 host IRQ：host trigger 与 guest redirection trigger 分开处理；
- VM reset 后旧 runtime 队列不再接收新中断。

### 11.3 本地验证

每个模块至少执行：

```bash
cargo fmt --all --check
cargo xtask clippy --package axdevice_base
cargo xtask clippy --package axdevice
cargo xtask clippy --package axvm
```

并按受影响架构执行对应的 `cargo xtask axvisor build`。不要求在每个模块运行物理板测试，但最终合并前应完成可用的 QEMU 架构验证。

## 12. 完成判定

本次重构完成时应满足：

1. 所有模拟设备通过 `IrqLine` 发中断；
2. 设备代码不引用架构控制器或 VM runtime API；
3. Router 不承担任务唤醒和 host IPI；
4. 远程 vCPU 注入只由目标 vCPU run loop 落地；
5. RISC-V、AArch64、LoongArch 和 x86 的现有 IPI 行为不退化；
6. x86 运行时不再轮询 PIT/串口 IRQ pending 状态；
7. 旧的 VM pending interrupt 队列生产者全部移除；
8. 架构特有的 EOI/MMIO hook 没有进入共享 Fabric trait；
9. 新增资源、队列和 Router 行为都有确定性回归测试。
