# Axvisor x86 Guest SMP INIT-SIPI 启动设计

## 背景与目标

x86_64 Linux guest 通过 INIT-SIPI（Intel SDM Vol.3 §9.4 MP 初始化协议）启动
secondary CPU。此前 emulated local APIC 对 INIT/SIPI 只记录日志并丢弃
（`virtualization/x86_vlapic/src/vlapic.rs` 的 `process_init_sipi`），所有 x86
guest 测试用例都固定 `cpu_num = 1`，多 vCPU guest 无法上线第二个 CPU。

本设计为直接启动与 OVMF 启动的 x86 guest 打通 INIT-SIPI 路径：

1. BSP vLAPIC 收到 INIT ICR 写入后，把目标 AP 置于 wait-for-SIPI 状态；
2. SIPI 以协议入口 `CS = vector << 8, CS base = vector << 12, IP = 0` 启动目标，
   复用与 PSCI/SBI 相同的惰性 vCPU 任务拉起路径；
3. 不支持的 ICR 形式（逻辑目的模式、SELF/AllIncludingSelf 简写、SMI/NMI/
   Remote-Read）保持日志加忽略，绝不静默误路由或使 guest 致命。

非目标：对已运行 AP 的 INIT 复位（guest reboot/shootdown 路径）、逻辑模式寻址、
lowest-priority 投递、AP CPU_OFF 热插拔、x2APIC 专项测试。

## 方案比较

| 方案 | 结论 | 原因 |
| --- | --- | --- |
| 类型化 `X86VmExit::CpuUp` 退出（仿 RISC-V） | 不采用 | LAPIC 页的 MMIO/MSR 访问在 x86_vcpu 后端内部处理并折叠为 `X86VmExit::Nothing`，没有可拦截的退出；重新穿线后端代价远超收益 |
| 全局事件队列由 dispatcher 轮询 | 不采用 | 隐藏可变状态、额外顺序风险、当前无第二个消费者 |
| 在发送线程的 `set_entry` 里直接改写 AP 的 VMCS | 不采用 | 对 AP VMCS 执行 `vmptrld` 会破坏发送物理 CPU 的 current-VMCS 指针 |
| vLAPIC host-ops 回调 + bind 时编程 SIPI 入口态 | 采用 | host-ops 是既有边界（`inject_interrupt`、`register_timer`），回调在 BSP 退出分发内同步执行；SIPI 入口态只能在拥有 VMCS/VMCB 的目标线程上编程 |

## 所有权边界

### `x86_vlapic`

- 拥有 ICR 位域解码：Destination Mode、Level、Trigger Mode、Delivery Mode、Vector；
- 定义 host-op 契约 `X86VlapicHostOps::{request_ap_init, request_sipi_startup}`
  （默认 `Err(Unsupported)`，宿主必须显式接入才能支持 SMP）；
- 只做规则判定：INIT 接受条件为 `!(trigger == LEVEL ∧ level == DEASSERT)`（与
  KVM/QEMU 一致）；SELF/AllIncludingSelf × NMI/INIT/StartUp 维持拒绝；逻辑目的
  模式告警忽略；host-op 错误只记日志，绝不传播回 `handle_vcpu_exit_bound`。

### `axvm` arch/x86_64/smp

私有模块 `virtualization/axvm/src/arch/x86_64/smp.rs` 拥有：

- 每 VM 的 wait-for-SIPI 位图（`WAIT_FOR_SIPI_MASKS`），随
  `ArchOps::enter_runtime/exit_runtime` 注册与销毁，VM start/reset/stop 后状态
  确定性重建或丢弃；
- INIT/SIPI 决策：运行中目标的 INIT 忽略并告警（跨线程复位违反 `AxVCpu`
  非 Send/Sync 所有权模型）；无 wait-for-SIPI 标记的 SIPI 按硬件语义丢弃；
- 重试语义：仅 `VcpuOnError::StartFailed` 恢复门位（Linux 每个 AP 发两次
  STARTUP）；`AlreadyOn`/`OnPending` 保持门关闭直到下一次 INIT。

### `architecture/cpu_up`

`boot_secondary_cpu` 是传输无关核心：解析架构 CPU ID → `runtime::vcpus::vcpu_on`。
成功/失败对发送方 vCPU 的信号通过 `CpuUpOps::{set_cpu_up_success,
report_cpu_up_failure}` 钩子表达；x86 两者覆写为空操作——INIT/SIPI 没有返回值
ABI，改写发送方 RAX 会破坏存活 guest 状态。`handle` 返回启动结果供中断驱动型
调用方（x86）映射重试决策，hypercall 驱动型调用方（riscv/aarch64）解包 bound
exit 后行为不变。

### `x86_vcpu`

- `x86_sipi_entry_state(vector)` 给出协议精确实模式入口态（selector = vector<<8、
  base = vector<<12、RIP = 0），与真实硅片一致，trampoline 可从 `%cs` 推导基址；
- 两个后端各自记录 pending vector 并在 `bind()` 时应用：VMX 在 `vmptrld` 之后写
  VMCS guest 字段，SVM 直接写 VMCB 内存。BSP 的 `set_entry` 路径保持逐位不变。

## 数据流

```
guest BSP 写 ICR (MMIO/MSR, 后端内部处理)
  → x86_vlapic 解码 + 规则判定
  → X86VlapicHostOps::request_ap_init / request_sipi_startup
  → axvm smp: 门状态机（置位 / 原子取位）
  → cpu_up::handle::<X86_64Arch>(vm, 发送方 vCPU, CpuUpExit{target, vector<<12, vector})
      ├─ vcpu_on: 预留 → Starting → spawn AP 任务
      └─ ArchOps::set_vcpu_on_args(arg = vector) → AxvmX86Vcpu::set_sipi_vector
  → AP 任务在其宿主线程 bind(): 应用 CS/base/RIP 实模式入口态 → 进入 guest
```

## 错误与原子性

每条错误路径都是"诊断 + 无 guest 可见副作用"：

| 路径 | 行为 |
| --- | --- |
| 未知 VM / 缺失运行时 | warn，忽略 |
| INIT 到运行中 AP | warn "reset unsupported"，忽略 |
| INIT 到发送者自身 | warn，忽略 |
| 无 wait-for-SIPI 的 SIPI | warn "dropped"，忽略 |
| 架构 CPU ID 无法解析 | `boot_secondary_cpu` 内 warn，不改写发送方寄存器 |
| `vcpu_on` StartFailed | warn + 恢复门位（重试 SIPI 可成功） |
| AlreadyOn / OnPending | warn，门位保持清除 |
| 逻辑模式 / 非法简写 / SMI/NMI/Reserved | 日志加忽略，ICR 写返回 Ok |

## 锁与并发

门锁（`IrqSafeMutex<BTreeMap<VMId, usize>>`）绝不跨越阻塞式 secondary 启动
（`vcpu_on` 会等待 AP bind ack）也不跨越任何 runtime 锁。每次请求的序列：
registry 查找 → 任务存活检查（短临界区）→ 门更新（短临界区，drop 锁）→ 阻塞
启动 → 可选恢复（独立临界区）。与 `VM_REGISTRY`、`vcpu_task_list` 无嵌套环。
SIPI 取位是原子的 test-and-clear，重复 SIPI 天然只生效一次。

## 验证与回滚

分层回归（全部本地可跑）：

```bash
cargo fmt
cargo test -p x86_vlapic              # INIT-SIPI 解码/分发红先绿后回归
cargo test -p axvm --features host-test  # 门状态机、入口地址数学、失败映射
cargo test -p x86_vcpu                # SIPI 入口态与 apply-once 语义
cargo test -p axbuild                 # initramfs 断言内容
cargo xtask clippy --package x86_vlapic --package axvm --package axbuild
```

E2E（需 KVM runner）：`smp-direct-vmx` 用例以 `cpu_num = 2` 直启 Linux，initramfs
断言 MADT 可读、online 恰为 `0-1`、`cpu1` sysfs 存在且 IO-APIC 已初始化，输出
`AXVISOR_X86_SMP_PASSED` / `AXVISOR_X86_SMP_FAILED`；CI 入口为
`test-axvisor-x86-64-smp-boot-vmx`。

回滚：revert 本分支提交即可。`cpu_num = 1` 的既有用例不受影响——单 vCPU guest 至
多触发 self-INIT 告警（纯日志），广播掩码为空，BSP bind 仅增加一个 `None` 分支，
固件表不变。
