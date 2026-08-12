# 加固 axvirtio-common 中面向不可信 guest 的共享 virtqueue

> 状态：提案，决策已定（2026-08-12），尚未在 GitHub 提交。
> 范围：`virtualization/axvirtio-common`（被 `axvirtio-net` 与即将合入的 `axvirtio-blk` 共用的队列层）。
> 威胁模型：guest 不被 hypervisor 信任。当前仅有的消费者运行可信的在树 guest，因此缺失的校验尚属潜在问题；一旦支持不可信 guest 即会暴露，而且**今天**一个 buggy（不必恶意）的 guest 驱动就可能触发。

## 1. 问题概述

`axvirtio-common::VirtioQueue` 是本仓库所有 virtio 设备模型共用的唯一 virtqueue 实现（目前是 `axvirtio-net`，#1935 之后还有 `axvirtio-blk`）。它的 descriptor 链处理在**较新的代码路径**里确实是有防御的（`DescriptorTable::descriptor_chain` 校验索引越界、拒绝 `VIRTQ_DESC_F_INDIRECT`、限制遍历步数），但 VirtIO 规范的若干结构性保证并未被强制，且有一条路径可以让 host 的 vCPU 线程挂死。本 issue 列出发现的缺口（附代码位置）、每个缺口对不可信 guest 意味着什么，以及一个不破坏既有消费者的 API 演进方案。

## 2. 发现

### 2.1 `descriptor_chain` 的链长边界过严（off-by-one），拒绝合法链 — `queue/descriptor.rs`

`DescriptorTable` 有两条链遍历路径，链长边界不一致：

- `follow_chain`：`if descriptors.len() > self.size as usize { return Err(...) }` — 链长最大 `size`
- `descriptor_chain`：`if descriptors.len() >= self.size as usize { return Err(...) }` — 链长最大 `size-1`

**后果**：virtio 允许一条链合法地使用全部 `size` 个索引（链长 `len == size`，例如在 `size` 大小的表上串联全部描述符）。`follow_chain`（`> size`）放行这条合法链，`descriptor_chain`（`>= size`）把它当成错误拒绝——同一条合法输入在两条路径上行为不同，而 `descriptor_chain` 是驱动主动使用的路径（`pop_available` / `validate_virtio_block_chain` / `get_data_buffers` / `get_status_addr` 都走它）。

修复：`descriptor_chain` 的链长边界改为 `> size`，与 `follow_chain` 一致（一条链最大 `size` 个节点，访问第 `size+1` 个节点即错——该场景已被每次循环开头的 `current >= size` 前置检查拦截）。

### 2.2 缺少 enable 期布局校验 — `queue/mod.rs`

`set_desc_table_addr` / `set_avail_ring_addr` / `set_used_ring_addr` 接受任意非零地址，不做任何检查：

- **对齐**：desc 表必须 16 字节对齐、avail 环 2 字节、used 环 4 字节（virtio 1.x §2.7 "Virtqueue Layout"）；
- **地址运算**：`addr + size*elem` 可能溢出 guest 地址空间（三个环的大小由 `size` 推导，而 `size` 是 guest 通过 `set_size` 编程的）；
- **互不重叠**：三个环区域可能互相重叠（guest 可以把 used 环别名到 descriptor 表上，使一次 used 元素写入破坏设备即将读取的 descriptor）。

`is_valid()` 只检查非零地址 + `ready`，以上均不检查。这些校验与传输无关（MMIO 与 PCI 的环布局完全一致），因此应放在共享队列层，而不是每个 transport 各做一遍。

### 2.3 `ready` 可以在未验证的布局上生效 — `mmio/state.rs`

MMIO transport 先编程三个地址、然后 `q.set_ready(val != 0)`（`mmio/state.rs:298`）而**不执行任何布局校验**；`set_ready` 本身只是普通 flag setter。在 2.2 未修复的情况下，guest 可以带着畸形布局把 `ready` 置位，队列就在其上服务请求。校验需要一个单一强制点，落在"队列变为可用"的路径上（见 3.2）。

### 2.4 `VirtioError` 无法表达"布局为何无效" — `error.rs`

当前环形布局失败可映射到的变体是 `InvalidQueue` / `InvalidAddress` / `InvalidDescriptor`，不够具体，transport 无法据此做出正确响应：例如 PCI transport 上报非法队列布局的方式是**不置** queue-ready 位，驱动需要区分"布局错误"与"队列未就绪"。错误类型需要布局专用变体，让每个 transport 不必猜测即可决定响应。（新增变体对消费者是安全的：`axvirtio-net` 的 `From<VirtioError>` 已有通配臂（`error.rs:47`），不会有任何消费者的穷举 match 编译失败。）

## 3. 提议的 API 演进

目标：修复 2.1–2.4，**不改变既有公开 API 的任何签名**，使 `axvirtio-net` 与 MMIO transport 无需重写。

### 3.1 统一链长边界（修复 2.1）

`descriptor_chain` 的链长边界从 `>= size` 改为 `> size`，与 `follow_chain` 一致（一条链最大 `size` 个节点）。两条路径行为一致，合法全索引链不再被拒。

### 3.2 `validate_layout()` + transport 强制（修复 2.2、2.3）


- 新增 `VirtioQueue::validate_layout() -> VirtioResult<()>`，在队列即将被使用的时间点检查：
  - 三个环地址均非零，
  - `addr + size*elem_size` 不溢出，
  - 各环满足要求的对齐，
  - 三个区域互不重叠。
- 新增 `VirtioError` 变体，例如 `InvalidRingLayout` / `RingOverlap` / `RingMisaligned`（见 2.4）。
- `is_valid()` 增加对 `validate_layout()` 的调用（返回 `bool` 的查询可保持签名不变），MMIO transport 在响应 `ready` 写之前检查其结果。设备驱动（net/blk）无需为此改动（faulted 语义见 3.3）；它们只会观察到畸形 ring 现在被拒绝而不是被静默服务。

### 3.3 faulted 状态

当运行期 ring/descriptor 校验失败时，将队列标记为 `faulted`；在 `reset()` 之前拒绝后续 pop/complete。这与硬件 transport 暴露不可恢复队列错误的方式一致，并防止"错误成功"的 completion。

- 新增 `VirtioQueue::is_faulted() -> bool`；`pop`/`complete` 在 faulted 状态下直接返回错误（复用 2.4 新增的布局/队列错误变体），`reset()` 清除 faulted 状态。
- 行为影响：任何运行期校验失败都会把队列置为不可用直到 `reset()`——`axvirtio-net`/`axvirtio-blk` 需要在错误处理路径中检查 `is_faulted()` 并据此决定 reset。可信 in-tree guest 的正常驱动不会触发，预期无行为变化。

## 4. 对既有消费者的影响

- **`axvirtio-net`**（当前唯一在树设备）：签名不变；行为仅对原本就畸形的输入收紧（2.2/2.3），以及 2.1 修正合法全索引链被误拒；运行期校验失败会把队列置为 faulted（3.3），其错误处理路径需增加 `is_faulted()` 检查。可信 ArceOS guest 预期无 net 行为变化。
- **MMIO transport**（`mmio/state.rs`）：响应 `ready` 前增加一次校验调用；错误仅在之前被静默接受的输入上传播。
- **`axvirtio-common` 测试**（`tests/queue_tests.rs`）：部分测试故意使用非对齐环地址来验证 setter 的覆盖语义（如 `queue_tests.rs:168-182`）；这些断言需要从"setter 接受"改为"setter 接受、校验拒绝"（或改用对齐的 fixture）。
- **`axvirtio-blk`（#1935）**：合入后直接受益，无需为 3.1/3.2 改代码；驱动错误处理需包含 `is_faulted()` 检查（3.3）。

## 5. 验收标准

- 链遍历在 `follow_chain` 与 `descriptor_chain` 上行为一致：合法全索引链（`len == size`）两条路径都放行；链长超过 `size` 的链在两条路径上都以 `Err` 终止。
- `validate_layout()` 拒绝：未对齐的环、溢出的环区域、互相重叠的环；通过 MMIO transport 无法在畸形布局上使 `ready` 生效。
- 既有公开函数无签名变化；`axvirtio-net` 原样编译、测试通过（除故意使用畸形布局的 fixture 外）。
- 为 2.1–2.3 各新增回归测试，并为 faulted 状态（3.3）新增回归测试：运行期校验失败后 `is_faulted()` 为真、后续 pop/complete 拒绝、`reset()` 清除 faulted 且无"错误成功"completion。