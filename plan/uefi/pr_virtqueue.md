# PR: 加固 axvirtio-common 中面向不可信 guest 的共享 virtqueue

> 解决 [issue #1979](https://github.com/rcore-os/tgoskits/issues/1979)：加固 `axvirtio-common::VirtioQueue` 面向不可信 guest 的校验缺口。
> 实现分支：`feat/virtqueue-hardening`（基于 dev，共 4 个 commit：`be21d595a`、`b5f3ce262`、`fe9c817da`、`7eaf5f423`）。
> 状态：PR #1984 已提交；GitHub 两轮审查意见与本地三轮 fix/review 的发现均已修复并合入分支，待合并。

## 1. 要解决的问题

`axvirtio-common::VirtioQueue` 是本仓库所有 virtio 设备模型共用的唯一 virtqueue 实现（当前消费者 `axvirtio-net`，`#1935` 合入后还有 `axvirtio-blk`）。面向不可信 guest（或 buggy 驱动）时存在以下缺口（issue 2.1–2.4）：

1. **链长边界 off-by-one**：`descriptor_chain` 用 `>= size` 拒绝合法全索引链（`len == size`），与 `follow_chain` 的 `> size` 不一致，同一条合法链两条路径行为不同；
2. **缺少 enable 期布局校验**：三个环地址接受任意非零值，不对齐、`addr + size*elem` 溢出、三区域互相重叠（used 环别名到 desc 表可破坏设备将读的 descriptor）均不检查；
3. **`ready` 可在未验证布局上生效**：MMIO transport 编程地址后直接置 ready，不执行布局校验；
4. **`VirtioError` 无法表达布局为何无效**：`InvalidQueue`/`InvalidAddress`/`InvalidDescriptor` 不够具体，transport 无法据此决定响应（如 PCI 不置 ready 位需区分"布局错误"与"队列未就绪"）。

## 2. 改动内容

### `virtualization/axvirtio-common/`

- **`queue/descriptor.rs`**：`descriptor_chain` 链长边界 `>= size` → `> size`，与 `follow_chain` 一致（一条链最大 `size` 个节点），合法全索引链不再被拒（issue 3.1）。
- **`queue/mod.rs`**：
  - 新增 `validate_layout()`——检查三环地址非零、对齐（desc 16B / avail 2B / used 4B）、`addr + size*elem` 不溢出、三区域互不重叠（issue 3.2）；
  - 新增 `validate_layout_with_memory()`——利用 guest 地址翻译/limit 检查每环完整落在 guest 地址空间内（design 7.6 要求）；只探测每环首尾字节作为 enable 期筛查，区域内映射由运行期逐字节访问兜底；不可翻译的占位 accessor（如 `NoGuestMemoryAccessor`）无法通过筛查，其布局被拒绝（每个配置周期首次拒绝 warn、后续仅 trace）；
  - 新增 `faulted` 状态（`is_faulted()`/`latch_fault()`，`AtomicBool` Acquire/Release）：运行期 ring/descriptor 校验失败即置位，`pop`/`complete` 及 5 条 guest-data 路径（`descriptor_chain_with_memory`/`validate_virtio_block_chain`/`get_data_buffers`/`get_status_addr`/`write_status_byte`）在 faulted 下拒绝且不写 guest 内存，`reset()` 清除；faulted 检查优先于 `is_valid`（始终返回 `QueueFaulted` 而非 `QueueNotReady`）；未配置（`QueueNotReady`）不触发 fault（issue 3.3）；
  - `ring_regions()` 的环区域大小**无条件包含 2 字节 EVENT_IDX footer**（与协商状态无关），并集中到 `AvailableRing`/`UsedRing`/`DescriptorTable::layout_size`（`pub(crate)`）单一来源，`total_size()`/footer 地址计算均从它推导；footer 别名（`RingOverlap`）或越出 guest 地址空间的布局被拒绝；
  - 非 `_with_memory` 入口（`read_avail_idx`/`read_avail_entry`）改为委托 `_with_memory` 孪生，运行期读失败同样 latch fault（面向未来 `axvirtio-blk` 消费者）；`add_used` 在 used ring 未配置时返回 `QueueNotReady` 而非静默推进 `next_used`（消灭"错误成功"completion）；
  - `is_valid()` 内部增加 `validate_layout()` 调用（签名不变）。
- **`mmio/state.rs`**：MMIO `QUEUE_READY` 写强制点统一为"三地址非零（`is_configured`）+ `validate_layout_with_memory`"，未配置或畸形布局时 `ready` 不生效、地址保留可重编程重试（issue 2.3/3.2 单一强制点）；新增 `mmio_write_with_memory`，把 vCPU 访问时的 scoped `GuestMemory` 传入 `QUEUE_READY` 校验（原 `mmio_write` 签名不变、回退到队列自带 accessor），axvisor 生产路径（`NoGuestMemoryAccessor`）因此可以正常置 ready；持队列锁完成"校验 + 置 ready"保证单一强制点原子性。
- **`error.rs`**：新增 `RingMisaligned`/`RingOverlap`/`InvalidRingLayout`/`QueueFaulted` 四变体，rustdoc 明确 `QueueFaulted`（运行期故障需 reset）与 `QueueNotReady`（正常预配置状态）的区别；`axvirtio-net` 的 `From<VirtioError>` 通配臂保证编译兼容（issue 2.4）。

### `virtualization/axvirtio-net/`

- **`device.rs`**：TX `handle_tx_notify` 每轮 drain 前检查 `tx.is_faulted()`，faulted 时 warn + 停止 drain；RX `receive_frame_with_memory` peek 前检查 `rx.is_faulted()`，faulted 时返回 `NetError::Queue(QueueFaulted)`；`mmio_write_with_memory` 把 scoped guest memory 透传给共享状态的 `QUEUE_READY` 校验。队列不再被静默吞错永久停滞（issue 3.3 / 第 4 节要求的 `is_faulted()` 消费者接线）。公开 API 签名零变化。

### 测试（`tests/queue_tests.rs`、`tests/mmio_state_tests.rs`）

- 链长：合法全索引链两条路径一致、超长链两条路径 Err；
- 布局：零地址/未对齐/溢出/重叠/环越出 guest 地址空间 5 类拒绝 + 合法布局通过；EVENT_IDX footer 别名（used 环指向 avail footer、avail 环指向 used footer）与 footer 越界拒绝；MMIO 畸形布局 ready 不生效、重编程可恢复、写 0 清除；
- faulted：运行期校验失败置位、pop/complete 及 5 条 guest-data 路径拒绝、不写 guest 内存、未配置不 fault、reset 清除且恢复后 complete 正确写 used ring（无"错误成功"completion）；非 `_with_memory` 预读失败同样 latch；`add_used` 未配置环返回 `QueueNotReady`；
- MMIO 强制点：队列用不可翻译 accessor 构建时，`mmio_write`（accessor 路径）拒绝 ready，`mmio_write_with_memory`（scoped 内存路径）可正常置 ready——axvisor 生产路径回归测试。

## 3. 各步解决方案的逻辑

- **布局校验放共享队列层**：virtio 1.x §2.7 的环布局约束与 transport 无关（MMIO 与 PCI 布局一致），放 `axvirtio-common` 让所有消费者（net、未来的 blk/PCI）一次受益，避免各 transport 各做一遍。
- **校验时机选"延迟到使用点"**（issue 决策，2026-08-12）：MMIO 对 64 位地址分两次写 setter，setter 内即时校验会在第一次写入时误报；`validate_layout` 只在"队列变为可用"的单一强制点（QUEUE_READY 写）执行，保留 setter 覆盖语义。
- **`faulted` 状态纳入本次**（issue 决策，2026-08-12）：运行期校验失败把队列置为不可用直到 reset，防止恶意/异常 ring 造成"错误成功"completion——与硬件 transport 暴露不可恢复队列错误的方式一致。
- **可达性检查用 `_with_memory` 变体**：遵循既有 `pop_available_head_with_memory` 等先例，`GuestMemoryAccessor` 的地址翻译/limit 为每个环区域做可达性验证，补齐 design 7.6 "完整落在 guest 地址空间内"的要求。
- **`QUEUE_READY` 强制点用 scoped 内存而非队列自带 accessor**：axvisor 等运行时以 `NoGuestMemoryAccessor` 构建设备、真实 guest 内存只存在于 MMIO 访问时刻的 scoped capability，用队列 accessor 校验会让所有队列永远无法 ready；`mmio_write_with_memory` 把 scoped `GuestMemory` 传入强制点，无 scoped 内存的调用方保留原 accessor 回退。
- **环区域大小无条件包含 EVENT_IDX footer**：guest 一旦协商 EVENT_IDX 就会读写 footer 字节，因此别名 footer 的布局无论协商状态都必须拒绝；无条件覆盖是保守包络，且无需把协商状态传入队列层。
- **faulted 检查优先于 `is_valid`**：faulted 是比"未就绪"更重的状态，必须返回 `QueueFaulted` 让消费者决策（reset），不能因 `is_valid` 为假而返回 `QueueNotReady` 掩盖故障。
- **net 只接线不吞错**：队列层新增 `is_faulted()` 后，net 的 TX/RX 路径显式检查并停止服务（warn），避免 `unwrap_or` 吞掉故障导致队列永久停滞（对照 dev_guidelines 3.6 教训"成功但无效果"路径）。

## 4. 验证

| 验证 | 结果 |
|---|---|
| `cargo fmt --all --check` | 通过 |
| `cargo xtask clippy --package axvirtio-common` | 通过（0 warning） |
| `cargo xtask clippy --package axvirtio-net` | 通过（0 warning） |
| `cargo clippy --no-deps -p axvirtio-common --all-targets -- -D warnings` | 通过 |
| `cargo clippy --no-deps -p axvirtio-net --all-targets -- -D warnings` | 通过 |
| `cargo test -p axvirtio-common` | 55 通过（4 单测 + 13 mmio 集成 + 38 queue 集成） |
| `cargo test -p axvirtio-net` | 32 通过（14 单测 + 18 集成） |
| `cargo xtask axvisor build --config os/axvisor/.build.toml` | 通过（x86_64-unknown-linux-musl 目标，验证 scoped-memory 改动在真实运行时路径可编译） |

（`cargo check -p axvisor` host 目标失败为预先存在——裸机 target-gated 依赖，与本改动无关。）

## 5. 验收标准（issue 第 5 节）对照

| 验收项 | 结果 |
|---|---|
| 链遍历两条路径行为一致（`len == size` 放行、超长 Err） | ✅ 满足 |
| `validate_layout()` 拒绝未对齐/溢出/重叠环；MMIO 无法在畸形布局上使 `ready` 生效 | ✅ 满足 |
| 既有公开函数无签名变化；`axvirtio-net` 原样编译、测试通过 | ✅ 满足 |
| 2.1–2.3 与 faulted（3.3）各新增回归测试（is_faulted 为真、pop/complete 拒绝、reset 清除、无错误成功 completion） | ✅ 满足 |

## 6. 审查记录

- **第一轮审查**（impl 后）：发现 5 个阻塞项——A1 MMIO 0 地址绕过 ready 校验、A2 `overlaps` 对溢出区间短路、A3 缺 guest 地址空间可达性校验、A4 `add_used` 失败不置 faulted 且 faulted 检查在 `is_valid` 之后、A5 faulted 下 guest-data 路径仍可写 guest 内存且触发面不一致；以及一般项 A6（`should_notify` 语义）、A7/D1（net 吞错）、B1（`Cell<bool>` 并发论证）、B2（错误变体命名）、C1/C2/C3（测试补强）、E2（rustdoc 对齐）。
- **第二轮审查**（fix 后）：5 个阻塞项全部独立核验确认修复，未引入新阻塞问题，验收标准逐条满足，本地验证全绿。结论：可以提交。
- **GitHub PR #1984 两轮审查**（mai-team-app，均 CHANGES_REQUESTED）：
  - 第一轮阻塞项：TX avail 预读失败被 `unwrap_or(0)` 吞成"空队列"且不 latch fault——已修复（`read_avail_idx/entry_with_memory` 预读失败 latch、TX 停止 drain、TX/RX 回归测试，commit `b5f3ce262`）。
  - 第二轮阻塞项：`ring_regions()` 漏掉 EVENT_IDX 协商后两个环末尾实际访问的 2-byte event footer，可能接受实际重叠或越界的布局——已修复（footer 无条件计入区域大小 + footer 别名/越界回归测试）。
- **第三轮本地审查**（双视角：正确性/安全 + 可维护性/规范，两轮迭代）：
  - 修复：非 `_with_memory` 入口不 latch fault；`Cell<bool>` 并发论证薄弱 → `AtomicBool`；`validate_layout_with_memory` 契约/占位 accessor 语义不明确；布局拒绝 warn 可被 guest 无界触发 → 每配置周期一次的门闩；`add_used` 未配置时"错误成功"；布局大小计算重复 → `layout_size` 单一来源；faulted/校验相关 rustdoc 与实现不一致；若干测试补强（全部失败先行验证）。
  - 发现并修复一个功能性 bug：`QUEUE_READY` 校验使用队列自带 accessor，axvisor 生产路径（`NoGuestMemoryAccessor`）下任何 guest 都无法使队列 ready、virtio-net 实际不可用——新增 `mmio_write_with_memory` 把 vCPU 访问的 scoped `GuestMemory` 传入强制点（commit `7eaf5f423`），并以 `cargo xtask axvisor build` 验证真实目标编译。
  - 第三轮审查结论：APPROVE，无阻塞项。
- **遗留建议项（非阻塞，后续 PR）**：N1 EVENT_IDX 协商接线（`event_idx_enabled` 仍为死字段，footer 已无条件覆盖，协商实现后无需再改布局校验）；N2 `VirtioError` 未用 `thiserror`/无 `Display`（预存问题）；N3 net 层 faulted 状态可见性可进一步增强。

## 7. 相关

- Issue：#1979（本 PR 修复）
- 关联：#1927（axvirtio-common/net 引入）、#1935（axvirtio-blk，合入后直接受益）
- 参照实现：`uefi/f2` 分支 `axdevice/src/virtio/split_queue.rs`（校验规格来源：布局校验/faulted 语义）
