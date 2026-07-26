# TGOSKits / AxVisor 开发规范与 refactor/device-manager 分支问题总结

> 本文档总结项目仓库中声明的开发规范、AxVisor 的开发规范，以及 refactor/device-manager 分支在 commit 过程中碰到的问题，供后续开发参考。

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

## 三、refactor/device-manager 分支问题总结

本分支的核心工作是：**将 AxEmuDevices（3 个 Vec，O(n) 查找）重构为基于 BTreeMap 的 DeviceManager（O(log n) 分派）**，并引入统一的 `Device` trait。该 PR (#1335) 包含 19 个 commit，其中初始重构后经历了 **14 个 fix 提交**。以下是按问题类别总结的错误及教训。

### 3.1 API 变更影响面评估不足

**涉及 commit**：`aefc9691e`, `f7eacf400`

**问题描述**：初始重构移除了旧的 per-bus 注册 helper（`add_mmio_dev`、`add_sys_reg_dev` 等）和 `DeviceRegistration::Mmio/Port/SysReg` 变体，但只检查了当前 crate 的编译，没有搜索所有下游使用者。

**具体断裂点**：
- `axvm/src/vm.rs` 仍调用已移除的 `add_sys_reg_dev()` 来注册 aarch64 vtimer 设备
- `axvm/src/vm.rs` 仍使用已标记 deprecated 的 `map_device_of_type` 进行 GICv3 SPI 分配
- `axvm/src/irq/riscv.rs` 的 PLIC 工厂仍使用旧的 `DeviceRegistration::Mmio` 变体
- `axvm/src/vcpu.rs` 仍 `pub use` 已不存在的 `arm_vgic::vtimer::get_sysreg_device`
- `PortDeviceAdapter` 的导入在非 x86_64 架构上触发 `unused-import` 警告

**教训**：
- 大型 API 变更时，**必须**用 `grep` 搜索整个工作区中的引用，而不仅仅是编译当前 crate
- 涉及跨 crate 的 trait/类型变更时，优先检查直接依赖该 crate 的所有下游包
- 架构相关的导入需用 `#[cfg(target_arch = "...")]` 包裹

### 3.2 并发安全问题：UnsafeCell + unsafe impl Send/Sync 的滥用

**涉及 commit**：`a2b28ba9d`, `4c973ec38`, `f153dfa89`

**问题描述**：
1. VGicR、VGicD、Gits 使用 `UnsafeCell<T>` 包装内部状态，然后添加 `unsafe impl Send + Sync` 来满足 `Device: Send + Sync` bound。但这些设备被多个 vCPU 通过共享引用并发访问，`UnsafeCell` 本身不提供任何同步保证。
2. Adapter 中添加了 6 个不必要的 `unsafe impl Send/Sync` 块，而编译器可以从 `Arc<T>`、`String`、`Box<[Resource]>` 自动推导正确的 bounds。

**修复**：
- 将 `UnsafeCell<VGicRRegs>` / `UnsafeCell<VirtualGitsRegs>` 替换为 `SpinNoIrq<T>`
- 删除裸引用返回方法（`regs()` / `regs_mut()`），改用持有锁的闭包（`with_regs()` / `with_regs_mut()`）
- 删除 adapter 中手写的 `unsafe impl Send/Sync`，让编译器自动推导

**教训**：
- **绝对不要**用 `unsafe impl Send/Sync` 来回避同步问题——如果类型不是线程安全的，加 unsafe impl 只会让问题更难发现
- 共享状态必须通过真实的同步原语（`SpinNoIrq`、`Mutex` 等）保护
- `unsafe impl` 的合理性需要严格审查：是否真的安全？是否有更安全的替代方案？

### 3.3 边界条件处理缺失

**涉及 commit**：`9ff062db7`, `50d1e0f08`, `4e109739c`, `6d10006c2`

#### 3.3.1 地址范围溢出（wrapped range）

**问题**：`check_mmio_conflict` 没有检测 `base + size` 溢出的资源范围。如果 base 接近 `u64::MAX`，`base + size` 会 wrap 回 0，导致范围检查逻辑完全错误。

**修复**：在注册时拒绝溢出的资源范围；在 lookup 路径上使用 `wrapping_add` 防止对已存在 wrap 值 panic。

**教训**：任何涉及 `base + size` 的地址范围计算都必须检查溢出（`checked_add`）。

#### 3.3.2 零大小范围

**问题**：初始实现没有拒绝 `size == 0` 或 `count == 0` 的资源范围。

**修复**：在 conflict check 中返回 `InvalidResourceReason::ZeroSized`。

**教训**：资源范围的 size/count 为 0 通常意味着配置错误，应在注册时拒绝。

#### 3.3.3 SysReg 范围内部地址查找失败

**问题**：`lookup_sysreg` 使用 `BTreeMap::get` 精确匹配起始地址。如果一个 SysReg 范围声明为 `0x100..=0x110`，访问 `0x108` 会返回 `NotFound`。

**修复**：改用 predecessor query（`range(..=addr).next_back()`）+ 包含范围检查（`addr <= start + count - 1`），与 `lookup_mmio` 和 `lookup_port` 的语义一致。

**教训**：范围查找应该用 predecessor query 而非精确匹配。不同 bus 的查找语义必须保持一致。

#### 3.3.4 SysReg u32::MAX off-by-one

**问题**：`addr = u32::MAX, count = 1` 被错误地拒绝为 `AddressOverflow`。实际上 `u32::MAX + 0` 不会溢出。

**修复**：使用 `checked_add(count.saturating_sub(1))` 而非 `checked_add(count)` 来检查溢出。

**教训**：包含范围 `[addr, addr + count - 1]` 的溢出检查要考虑 `count == 1` 的特殊情况。

#### 3.3.5 同设备内部资源重叠

**问题**：资源冲突检测只考虑跨设备冲突。同一设备声明的两个资源范围如果重叠，不会被检测到。

**修复**：将资源验证和索引插入合并为单遍处理，使同一设备的早期资源已进入索引，后续资源检查时会命中自身。

**教训**：资源验证需要考虑**同设备内部**的资源重叠，不能假设只有跨设备冲突。

### 3.4 事务原子性缺失

**涉及 commit**：`4c973ec38`

**问题**：`register_bundle` 注册多个设备时，如果中间某个设备失败，前面已注册的设备没有被回滚。这导致 bundle 不是真正的原子操作。

**修复**：在失败时使用 `pop()` + 索引键移除来回滚已注册的设备。

**教训**：批量操作必须考虑事务语义——要么全部成功，要么全部回滚。注册操作尤其需要原子性，因为部分成功会导致不一致的状态。

### 3.5 错误类型映射不正确

**涉及 commit**：`2fa166beb`

**问题**：`register_bundle` 将所有 `RegistryError` 统一映射为 `InvalidInput`，但 `AddressConflict` 应该映射为 `AddrInUse`。

**修复**：区分 `AddressConflict`（映射到 `AddrInUse`）和其他错误（映射到 `InvalidInput`）。

**教训**：错误类型的映射需要仔细设计。不同的底层错误应映射到不同的上层错误类型，以便调用者做出正确的处理决策。

### 3.6 命名与语义不一致

**涉及 commit**：`f153dfa89`

**问题**：`iter_mmio_dev()`、`iter_sys_reg_dev()`、`iter_port_dev()` 的命名暗示按 bus 类型过滤，但实际上返回所有设备。这种误导性命名会导致调用者做出错误假设。

**修复**：移除这些别名，调用者统一使用 `devices()` + `Device::resources()` 或 `Device::as_any()` 进行过滤。

**教训**：命名必须准确反映行为。如果函数名暗示某种过滤或语义，实际行为必须与之匹配。

### 3.7 静默类型截断

**涉及 commit**：`f153dfa89`

**问题**：`BusRouter::dispatch` 和 `lookup` 中使用 `as` 转换（`addr as u16`、`addr as u32`），可能导致静默截断，将超出范围的地址错误地路由到错误的设备。

**修复**：使用 `u16::try_from` / `u32::try_from` + `map_err` 来显式处理超出范围的情况。

**教训**：在地址/端口等关键路径上，避免使用可能静默截断的 `as` 转换。优先使用 `try_from` 并在失败时返回明确的错误。

### 3.8 测试与代码重构不同步

**涉及 commit**：`be8342680`, `1a8660592`, `5205d15d3`, `f7eacf400`

**问题**：
- 清理 `Resource` 变体（移除 `Irq`、`PciBar`、`MSI`、`DmaCapable`）后，测试中的断言没有同步更新
- 移除 `map_device_of_type` 后，测试代码仍使用该方法
- 重构错误类型（引入 `InvalidResourceReason`）后，测试的 `match` 模式不匹配
- `DeviceRegistration` 变体变更后，测试工厂函数断裂

**修复**：逐一更新所有测试文件中的断言、工厂函数和导入。

**教训**：
- **代码重构和测试更新必须同步进行**。不要先合入重构再修测试。
- 在 IDE 中运行 `cargo test --workspace` 确保所有 crate 的测试都能编译通过。
- 涉及 enum/struct 变体变更时，特别注意 pattern match 的 exhaustiveness。

### 3.9 代码格式问题

**涉及 commit**：`d2596a9a2`, `94d441f22`

**问题**：
- import 排序不符合 `rustfmt.toml` 的 `group_imports = "StdExternalCrate"` 要求
- 存在重复的 `#[allow(clippy::...)]` 属性

**教训**：每次提交前运行 `cargo fmt --all`。将格式化集成到提交前的工作流程中（如 pre-commit hook 或手动检查清单）。

### 3.10 竞态条件（Race Condition）

**涉及 commit**：`6d10006c2`

**问题**：GITS 的 CWRITER 命令队列提交路径没有加锁。两个 vCPU 可能同时读取相同的 stale `creadr` 值，然后都尝试处理已经被另一个 vCPU 处理过的命令。

**修复**：添加 `submission_lock: SpinNoIrq<()>` 保护 CWRITER 提交路径。

**教训**：任何共享可变状态的访问路径都必须评估并发安全性。特别是：
- 读取-修改-写入序列必须是原子的
- 不要假设单个寄存器操作是原子的——读取和写入之间可能有其他 vCPU 介入

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
