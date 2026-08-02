# TGOSKits / AxVisor 开发规范与PR复审问题总结

> 本文档总结项目仓库中声明的开发规范、AxVisor 的开发规范，以及在 PR 复审过程中碰到的问题，供后续开发参考。

---

## 必读前置

阅读本文档前，必须先完整阅读以下仓库核心规范和技能。

### 核心规范（按优先级排序）

| 优先级 | 文件 | 说明 |
|--------|------|------|
| 1 | `AGENTS.md` | 项目顶层指令：编码标准（2024 nightly、rustfmt 为格式真理、`no_std` 优先、typed IDs/newtypes、组合优先、`thiserror`）、skill 触发条件、PR 标题格式 `type(scope): content`（英文标题+中文正文）、验证命令、禁止 AI/agent 标签 |
| 2 | `book/guideline/code-quality.md` | Rust 代码质量强制基线（1930 行）：命名表达业务意图、函数（入口在前、目录式编排、≤3 参数）、模块边界（`foo/mod.rs` + `foo/child.rs`、默认私有、~400 行审视拆分/~800 行必须拆分、`#[cfg]` 收敛到最窄共同边界）、错误处理（`thiserror`、禁止裸 `unwrap`、panic 只用于不可恢复不变量）、类型系统（newtype/enum 表达约束、非法状态不可构造）、组合优先、并发（短锁、不持锁做 I/O）、测试（附录在文件末尾、禁止 `include_str!` 测代码行为）、PR 检查清单 20 条 |
| 3 | `book/guideline/feature-development.md` | 新功能设计/实现/审核准则：风险分级（局部/共享/高风险）、审核顺序（必要性→重复性→prior art→方案选择→设计→实现→验证）、方案比较表、hack 识别条件、接口设计原则、变更形状规则（一个 PR 一个问题）、验证证据映射、合入判断（阻塞/非阻塞） |

### 平台与架构技能

| 文件 | 说明 |
|------|------|
| `.claude/skills/arch-platform-porting/SKILL.md` | 架构移植规则：target/toolchain 契约、CPU-local 寄存器 ABI（x86_64/AArch64/RISC-V/LoongArch）、AArch64 GIC maintenance IRQ 完整契约（typed `IrqCapabilityStatus` 区分 `Unavailable`/`Error`、禁止 `IrqError::Unsupported` 表缺失、`Busy` 区分、per-CPU handler 注册与 host-lifetime 所有权、host IRQ 单事务 `FetchHandled`/`DeferredDispatch`、ICH fold 时序）、IRQ 命名空间规则（`IrqId` 禁止 `usize`、typed domain/hwirq）、SMP bring-up、UEFI handoff、动态平台 boot、验证梯度 |

### 其他规范

| 文件 | 说明 |
|------|------|
| `.claude/skills/cross-kernel-driver/SKILL.md` | 跨内核驱动架构：Driver Core / Capability Boundary / OS Glue / Runtime 分层 |
| `docs/community/contributing.md` | 公开贡献指南 |
| `os/StarryOS/CONTRIBUTING.md` | StarryOS 专项贡献规范 |
| `docs/community/sync-lint.md` | 原子操作 `Relaxed` 误用静态检查规则 |
| `.github/MAINTAINERS.md` | Reviewer 路由源（M/R/F/K 字段），PR reviewer 分配的严格 allowlist |

---

## 一、项目通用开发规范

### 1.1 PR 提交规范

- **标题格式**：遵循 Conventional Commits，`type(scope): content`
  - 示例：`feat(axbuild): add board test flow`、`fix(starry-process): correct tty cleanup`
- **语言**：PR 标题用**英文**，PR body 用**中文**
- **PR 描述必须覆盖**：
  1. 要解决的问题
  2. 改动内容
  3. 每步解决方案的逻辑
- **提交前本地验证 CI**：尽可能在本地运行相关 build/test/clippy，排除物理板测试和自托管测试流程
- **同步更新**：添加或修改 commit 后，更新 PR 描述以保持同步
- **禁止**：在 commit/PR 中添加 AI/agent 相关标签、签名或品牌标识

### 1.2 代码规范

- **不要**用 `#[allow(...)]` 来 silencing clippy 警告，应修复根本原因（除非用户明确要求）
- 每次代码修改后运行：
  ```bash
  cargo xtask clippy --package <crate>    # 针对性验证
  cargo fmt --all                          # 格式化
  ```
- **优先使用 `cargo xtask`** 而非原生 `cargo build` / `cargo test` / `cargo run`
- **Cargo.lock 冲突处理**：不要手动合并冲突的 `Cargo.lock`。先解决其他所有冲突，然后用 Cargo 重新生成并验证

### 1.3 代码风格

`rustfmt.toml` 强制要求：
- `group_imports = "StdExternalCrate"`
- `imports_granularity = "Crate"`
- `format_strings = true`
- `use_field_init_shorthand = true`

### 1.4 架构相关变更

当修改以下领域时，同步更新 `.claude/skills/arch-platform-porting/SKILL.md` 或其引用：
- 架构启动逻辑
- someboot 启动顺序
- UEFI handoff
- SMP bring-up
- 动态平台合约
- target JSON 假设
- 调试流程

---

## 二、AxVisor 开发规范

### 2.1 构建与测试

所有构建、运行、测试通过 `cargo xtask`：
```bash
# 生成板级配置
cargo xtask axvisor defconfig qemu-aarch64

# 构建
cargo xtask axvisor build --config os/axvisor/.build.toml

# QEMU 测试（需先准备 Guest 镜像）
(cd os/axvisor && ./scripts/setup_qemu.sh arceos)
cargo xtask axvisor qemu \
  --config os/axvisor/.build.toml \
  --qemu-config .github/workflows/qemu-aarch64.toml \
  --vmconfigs os/axvisor/tmp/vmconfigs/arceos-aarch64-qemu-smp1.generated.toml
```

### 2.2 修改验证策略

| 改动位置 | 先验证 | 再验证 |
|----------|--------|--------|
| `virtualization/axvm`、`axvcpu`、`axdevice` | `cargo xtask axvisor build` | 准备好 Guest 后 QEMU 测试 |
| `os/arceos/modules/axhal` | ArceOS helloworld | Axvisor build + QEMU |
| `os/arceos/modules/axtask` | ArceOS helloworld | Axvisor build + QEMU |
| `os/axvisor/src/*` | `cargo xtask axvisor build` | QEMU 测试 |
| `os/axvisor/configs/*` | — | 直接 QEMU / 板级测试 |

### 2.3 虚拟设备开发

设备模型三种模式：Passthrough、Emulated、Excluded。

添加新模拟设备的步骤：
1. 在 `virtualization/axdevice/` 中实现设备模拟逻辑
2. 在 VM 配置的 `emu_devices` 中注册
3. 在 `vmm` 中处理对应的 VM Exit 事件
4. 通过 Guest 驱动验证

### 2.4 首次上手路径

**不要**直接 `defconfig -> build -> qemu`，默认配置中 `vm_configs` 为空且 `rootfs.img` 不会自动生成。必须先运行 `setup_qemu.sh`。

### 2.5 物理板开发

从 QEMU 验证通过的改动迁移到物理板时，需额外关注：
- 中断控制器：GIC (通用) vs SoC 专用 GIC 配置
- 设备树：QEMU 生成 vs 板级固定 DTB
- 内存布局：可能有保留区域
- 启动方式：U-Boot 引导
- 时钟/电源：需初始化 PMU/Clock

---

## 三、近期 PR 审阅问题与教训

本节汇总 PR #1335、#1630、#1661、#1679、#1715 在审阅中暴露的问题。描述只保留可复用的模式，具体实现细节回到对应 PR 查看。

### 3.1 API 影响面评估不足

**问题**：#1335 修改设备注册 API 后，下游 `axvm`、测试 crate、架构特定导入没有同步迁移；#1679 修改 `axvm-types` 公开 trait 时，一度把有默认实现的方法改成必需方法；#1715 修改 `ArmHostOps` 这类公共边界时也需要同步 README、adapter 和契约测试。

**教训**：公共 trait、enum、helper、re-export 变更前，必须搜索全工作区和直接下游 crate；对已发布 crate 的公开 trait，优先保持兼容默认实现或明确版本/发布策略。

### 3.2 架构边界与并行设计冲突

**问题**：#1630 把 IRQ line 放入 `Device::resources()`，但开放 PR #1612 已在相同模块采用不兼容的 topology/planner 模型；#1335 也与早期设备模型重构 PR 存在方向重叠。

**教训**：共享边界开工前先检查开放 PR、issue 设计和 base 现状；若两套模型会改写同一公开 API，先确定取舍和合并顺序，再实现细节。

### 3.3 资源范围和 ID 边界处理不完整

**问题**：#1335 暴露了 zero-size、`base + size` 溢出、SysReg 区间中间地址、`u32::MAX` 单点区间、同设备内部资源重叠等问题；#1715 暴露了 AArch64 virtual INTID `u8` 截断和非法 INTID 表示能力不足。

**教训**：地址、端口、寄存器、IRQ/INTID 都要用 typed ID 和 checked conversion；范围计算明确半开/闭区间语义，并覆盖零长度、最大值、溢出、内部重叠和跨设备冲突。

### 3.4 事务语义和生命周期原子性不足

**问题**：#1335 的 `register_bundle()` 失败时曾留下部分注册状态；#1630 进一步要求 MMIO、Port、SysReg、IRQ line 都能在 bundle 失败时统一回滚；#1679 的 sender 需要在同一次 VM machine 锁内选择当前 runtime，避免 stop/reset/start 后写入旧 dispatcher。

**教训**：批量注册、runtime 选择、sender 投递都要先定义事务边界：成功后状态完整可见，失败后不泄漏部分状态，生命周期替换后不缓存旧对象。

### 3.5 并发安全不能靠 `unsafe impl` 兜底

**问题**：#1335 中 VGIC/GITS 曾用 `UnsafeCell` 加手写 `Send/Sync` 暴露共享可变状态；GITS CWRITER 处理消除了 Rust 数据竞争后，仍存在复合命令提交竞态。#1679 也反复检查 enqueue、notify、IPI 是否在正确锁边界外执行。

**教训**：共享状态必须用真实同步原语保护；`unsafe impl Send/Sync` 只能在有完整安全论证时使用。复合状态机要用单一事务锁或明确锁顺序，不能只保证单个字段访问安全。

### 3.6 新路径不能只“入队成功”而没有消费者

**问题**：#1679 早期 sender 已返回成功，但 vCPU run loop 尚未 drain 新 dispatcher，后续 Router 一旦使用 sender，中断会成功入队却永远不注入 guest。

**教训**：分阶段开发可以保留 dead code，但一旦暴露成功返回的运行时 API，就必须接通最小 consumer，或让 API 明确返回 unsupported，避免形成“成功但无效果”的路径。

### 3.7 语义元数据必须传到最终边界

**问题**：#1679 的 `PendingVcpuInterrupt` 携带 edge/level trigger，但默认注入实现曾丢弃 trigger，导致 x86 level-triggered 语义可能退化；#1715 的 GICv2 direct injection 曾无意改变 Group/EOI maintenance 策略。

**教训**：协议字段不能只在中间结构中保留，必须一直传到架构/硬件后端；兼容路径的默认行为要有回归测试，尤其是 interrupt trigger、EOI、group、priority 这类硬件语义。

### 3.8 测试必须可执行且验证真实行为

**问题**：#1661 曾把 dispatcher 测试整段注释，CI 不会执行；#1679 早期测试只覆盖队列局部，没有覆盖 sender→runtime→drain→inject；#1715 曾用 `include_str!` 搜源码文本证明 GICv2 策略，不能证明真实实现会写出正确寄存器配置。

**教训**：测试必须被 runner 发现并执行，且观察真实 API/模块行为。不能用注释测试、源码文本搜索或只测 helper 名称来替代行为回归；host-test 边界要提前拆清楚。

### 3.9 错误类型和命名要表达可恢复语义

**问题**：#1335 曾把不同 `RegistryError` 统一映射成 `InvalidInput`，也有 `iter_*_dev()` 名称暗示按 bus 过滤但实际返回全部设备；#1715 将 LR 满载、异常 LR、非法 INTID 从 panic/截断改成 typed error 后，调用方才能恢复或重试。

**教训**：错误映射要保留调用方决策所需的信息；函数名必须与实际过滤、查找、生命周期语义一致。客户机可触发路径不能 panic，必须返回明确错误。

### 3.10 格式、CI 和测试矩阵不能只看远端绿灯

**问题**：#1335、#1661 都出现过格式/import 顺序或 test target clippy 问题；#1661 的远端 formatting job 因路径矩阵跳过，没有覆盖本地 `cargo fmt --all --check` 失败；#1335 多次需要区分真实代码回归和 self-hosted/board 环境波动。

**教训**：提交前本地跑 `cargo fmt --all --check`、目标 crate clippy 和相关测试；远端 skipped 不等于通过。CI 红灯只有在有本地复现、重跑通过或可追踪基础设施证据时，才能判定为非代码阻塞。

### 3.11 模块布局和 cfg 也是公共契约

**问题**：#1715 的测试/辅助模块一度放在不符合 `axvm` 架构目录契约的位置，触发 `arch_boundary_contract` 失败；移除 crate 级 `cfg(target_arch = "aarch64")` 后又带来重复 cfg 和 host-test 暴露边界问题。

**教训**：模块位置、目录形状、`cfg` 粒度会影响测试发现、host 构建和公共导出。架构私有 helper 应放回对应 `src/arch/<arch>/`，跨架构 host-test API 要集中导出而不是散落重复 cfg。

---

## 四、综合建议清单

基于以上问题，后续大型重构应遵循以下检查清单：

### 提交前检查清单

- [ ] **影响面评估**：用 `grep` 搜索整个工作区中所有引用被修改 API 的位置
- [ ] **跨架构编译**：在非主开发架构上运行 `cargo check`（如主开发 aarch64，则检查 x86_64 / riscv64 / loongarch64）
- [ ] **测试编译**：`cargo test --no-run --workspace` 确保所有测试 crate 能编译
- [ ] **格式化**：`cargo fmt --all`
- [ ] **Clippy**：`cargo xtask clippy --package <crate>` 或 `cargo xtask clippy --since <base>`
- [ ] **边界条件**：检查所有涉及 `base + size` 的运算是否处理了溢出；检查零大小输入是否被拒绝
- [ ] **并发安全**：评估共享状态的所有访问路径，确认是否需要同步保护
- [ ] **unsafe 审查**：所有 `unsafe` 块和 `unsafe impl` 必须有明确的安全论证，考虑是否有 safe 替代方案
- [ ] **错误类型**：确认错误映射合理，不同错误条件映射到不同的错误类型
- [ ] **命名一致性**：函数名准确反映行为，不暗示未实现的过滤/语义
- [ ] **事务语义**：批量操作考虑原子性——是否需要回滚机制？

### 代码审查重点关注点

1. **范围查找逻辑**： predecessor query + 边界检查 的模式是否正确应用
2. **类型转换**：`as` 转换是否可能在关键路径上静默截断
3. **Send/Sync 实现**：`unsafe impl Send/Sync` 是否有真实的同步保证支撑
4. **资源验证**：零大小、溢出、同设备重叠、跨设备冲突是否都被覆盖
5. **下游兼容性**：trait/enum/struct 的公共 API 变更是否破坏下游调用者
