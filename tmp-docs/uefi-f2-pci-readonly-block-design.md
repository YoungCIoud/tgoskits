# Axvisor x86_64 只读 VirtIO PCI 块设备基准设计

> 文档性质：当前权威设计基准。本文自包含描述 `virtio-blk-pci` 的产品范围、分层边界、协议状态机、资源所有权、错误恢复和验证要求；旧的 `uefi-f2-0814.md`、`uefi-f2-0814-refactor.md`、`uefi-f2-0820.md` 与 `uefi-f2-0820-fix-plan.md` 仅作为历史设计和实施记录，不再是理解当前设备的前置材料。
>
> 实现快照：`uefi/f2-0814@e66f6cc181d000b2d9f3477b2963c5b14010f25c`（2026-08-22）。本文描述的是应长期保持的设计契约；快照中尚未完全满足的契约在所属章节就地标注，不建立按时间排列的问题流水账。
>
> 规范基线：VirtIO 1.2 modern non-transitional PCI transport、PCI conventional configuration mechanism #1，以及 TGOSKits 当前 `DeviceGraph -> ResolvedDeviceGraph -> DeviceRuntime` 资源和能力模型。
>
> 风险等级：高。该功能同时涉及 guest 可控 PCI 配置、MMIO、DMA、split virtqueue、feature/status 协商、INTx、reset、宿主文件资产和 x86 固件描述。

## 1. 问题、用户与成功标准

### 1.1 要解决的问题

Axvisor 需要为 x86_64 guest 提供一块标准、只读的 modern virtio-blk PCI 数据盘。Linux guest 应直接使用 `virtio_pci` 和 `virtio_blk` 驱动，不依赖私有 fw_cfg 块协议、设备直通或另一套 block 实现。

主要用户是：

- 需要向 direct-boot Linux guest 注入确定性只读数据的 Axvisor 配置者；
- 后续需要复用同一 PCI/VirtIO 边界的其他 VirtIO PCI 设备实现者；
- 需要从 resolved resources 生成一致 ACPI/MP/runtime 拓扑的 x86 machine 维护者。

### 1.2 产品成功标准

一条有效配置必须形成以下垂直链路：

```text
guest TOML
  -> Axvisor catalog 解析只读 backend/asset
  -> DeviceGraph endpoint 声明 BAR0 与 INTx
  -> x86 PCI root 聚合 endpoint 发布的 PciFunction
  -> planner 分配 BDF、BAR aperture 和 GSI
  -> ACPI/MP/runtime 使用同一 resolved graph
  -> modern VirtIO PCI common/notify/ISR/device configuration
  -> axvirtio-common split queue
  -> axvirtio-blk shared request core
  -> immutable backend
  -> Linux /dev/vda 只读挂载并读取 identity
```

可观察的完成条件是：

1. Linux 枚举到 vendor/device `1af4:1042`，绑定 `virtio_pci`/`virtio_blk`；
2. BAR0 有有效的 4 KiB memory resource，PCI Memory Space Enable 和 Bus Master Enable 按驱动初始化流程生效；
3. `/sys/block/vda/ro` 为 `1`，512-byte 写入确定失败；
4. raw ext4 数据盘可用 `mount -t ext4 -o ro /dev/vda` 挂载并读到 `axvisor-readonly-data-v1`；
5. 读取期间 `/proc/interrupts` 中对应 INTx completion count 增长；
6. guest 输出独立一行 `AXVISOR_X86_PCI_BLOCK_PASSED`，下一行输出 `identity=axvisor-readonly-data-v1`；
7. VMX/SVM 使用同一 guest 配置和同一镜像 digest 通过，运行前后源镜像 digest 不变。

### 1.3 非目标

当前产品只承诺 x86_64 guest 的一块只读数据盘。以下能力不属于本设计，也不得以空实现、未声明 feature 或静默成功伪装：

- transitional/legacy VirtIO；
- MSI/MSI-X；
- 多个 guest 可见 PCI 数据盘、PCI hotplug；
- 多 queue、packed ring、indirect descriptor、event index、ring reset；
- 可写、分页或异步/deferred PCI backend；
- IOMMU、`VIRTIO_F_ACCESS_PLATFORM`；
- BAR runtime relocation；
- 从该设备完成 OVMF/EFI 启动；
- guest 内可达的 FLR/function reset 接口。

通用 PCI root 可以在 host test 中聚合多个 synthetic endpoint，这只验证 framework 的 function service、BDF 和资源隔离，不等价于当前固件和产品配置支持多盘。

## 2. 方案选择与先验依据

| 方案 | 结论 | 原因 |
| --- | --- | --- |
| modern non-transitional virtio-blk PCI | 采用 | Linux 驱动成熟，协议可复用于后续 OVMF 场景 |
| 复用 `axvirtio-common::VirtioQueue` | 采用 | split ring、descriptor、fault、completion 和 reset 只有一个实现源 |
| 复用 `VirtioBlockRequestCore` | 采用 | PCI/MMIO 共用 capacity、请求校验和只读策略 |
| 通过 catalog/device graph 接入 endpoint | 采用 | BAR、IRQ、dependency service 和 firmware 统一走既有资源模型 |
| planner 固定 BAR | 采用 | 当前 runtime 没有 owner-aware、事务化的 MMIO remap |
| 单 queue + level INTx | 采用 | 足以完成单盘 MVP，状态面显著小于 MSI-X |
| 在 x86 PCI 层复制 block/queue | 拒绝 | 会产生第二套协议和错误恢复语义 |
| 私有 fw_cfg 文件通道 | 拒绝 | 不是标准块设备，不能复用 Linux/OVMF 驱动 |
| AHCI/NVMe/宿主块设备直通 | 拒绝 | 控制器、隔离和恢复范围远大于本功能 |

外部 prior art 用于核对边界而不是引入代码：QEMU 的 block device 是通用 VirtIO PCI proxy 的薄封装；Cloud Hypervisor、rust-vmm 和 crosvm 均把设备语义、transport、PCI root 和 VMM backend 分离。TGOSKits 应复用自身的 queue、device graph、resource plan、interrupt 和 backend 接口实现同样的职责分离。

## 3. 分层架构与依赖方向

### 3.1 层级职责

| 层 | 主要位置 | 拥有 | 明确不知道 |
| --- | --- | --- | --- |
| 应用资产与 backend | `os/axvisor/src/asset.rs`、`os/axvisor/src/virtio_blk/` | 配置解析、静态资产、文件快照、ramdisk、只读 backend | PCI header、BAR、VirtIO 寄存器、GSI |
| block device core | `virtualization/axvirtio-blk` | block config、request chain、capacity、read-only policy、backend 调用、completion 结果 | PCI/MMIO 拓扑、BAR、固件 |
| VirtIO PCI transport | `virtualization/axvirtio-common/src/pci` | feature/status、common config、queue lifecycle、notify、ISR/INTx、reset、DMA capability 使用 | `VIRTIO_BLK_*`、文件路径、具体 backend |
| PCI function/root | `virtualization/axdevice/src/x86` | BDF、header、capability list、BAR、config mechanism #1、function 聚合、MMIO dispatch | block reg/firmware | `virtualization/axvm/src/arch/x86_64` | endpoint 识别、稳定 BDF、resolved BAR/GSI、ACPI `_CRS`/`_PRT`、MP routing | backend 数据和 VirtIO request |

依赖方向只能从应用和 device-specific adapter 指向通用 transport/core。`axvirtio-common/src/pci` 中不得出现 `VIRTIO_BLK_*`，PCI config/root 中不得出现文件、sector 或 block request 逻辑。

### 3.2 关键能力边界

- endpoint 通过 `PciFunctionServiceKey` 发布恰好一个 `Arc<dyn PciFunction>`；service key 的全局基数为 `Multiple`，但每个显式 PCI endpoint dependency 的局部基数必须严格为 1。
- x86 PCI root 只读取显式 dependency 的 services，不访问全局 registry，也不按 model 字符串识别 function。
- root 创建一个 `DmaGrant`，同时绑定到 root runtime device 并注入所有 function。function 不保存裸 guest-memory pointer。
- endpoint 通过 `DeviceBuildContext::irq()` 获得 level-triggered `IrqLine`，transport 不保存裸 GSI、vector 或全局回调。
- `VirtioDeviceCore` 是 device-specific capability boundary：提供 device identity/features/config、queue notify 和 reset；block adapter 在 `axvirtio-blk` 实现它。
- backend 只实现 sector I/O 能力；它不知道 guest address，也不拥有 transport 状态。

### 3.3 构建原子性

Device graph 的 build 必须满足：

1. endpoint requirements 先声明并 resolve BAR0/INTx；
2. endpoint build 成功后才发布 `PciFunction` service；
3. root 对每个 dependency 检查 service 数量，再确定性分配 BDF；
4. 所有 function 都接受并读回目标 BDF 后，才构造 root runtime device；
5. service 注册、device 注册或资源校验任一步失败时，不得留下部分 runtime/device/service 状态，重试必须从干净 builder 开始。

## 4. 用户配置、资产与只读 backend

### 4.1 推荐配置

```toml
[[devices.virtual]]
id = "readonly-data-disk"
model = "virtio-blk-pci"
backend = "file"
path = "/guest/axvisor-readonly-data.ext4"
read_only = true
```

当前 parser 识别：

| 字段 | 语义 |
| --- | --- |
| `id` | VM 内设备和静态资产的稳定 identity |
| `model` | 固定为 `virtio-blk-pci` |
| `backend` | `file` 或 `ramdisk`；省略时为 `file` |
| `path` | file backend 路径；省略时为 `/tmp/<id>.img` |
| `read_only` | PCI model 必须显式解析为 `true`，否则创建失败 |
| `capacity` | 可选 size string，如 `8MiB`；必须是 512 的倍数 |
| `capacity_sectors` | `capacity` 的 legacy 互斥形式；正整数 sector 数 |

用户不得配置 BDF、BAR base、GSI、interrupt pin 或 MSI vector。这些值由 machine/resource planner 拥有。model parser 应拒绝未支持的 transport/resource 字段，不能接受后静默忽略；当前实现只读取上述已知键，后续整理 parser 时需要补齐“未知 option 明确拒绝”的边界测试。

### 4.2 资产解析

`VmAssetKey { vm_id, asset_id, configured_path }` 是静态和运行时资产的稳定键：

- build-time 配置命中的资产由 `build.rs` 通过 `include_bytes!` 生成 `VmAsset::Static`；
- 没有静态命中且路径为绝对路径时，返回 `VmAsset::File`；
- runtime 相对路径不得被猜测或相对当前目录解析，必须明确报配置错误；
- build-time 资产 discovery 只识别文件来源，不判断 PCI/VirtIO 协议。

### 4.3 backend 契约

- `MemorySnapshotBackend` 持有 `Arc<[u8]>`。容量必须非零、512-byte 对齐，并与静态资产长度完全相等。
- `ReadOnlyFileBackend` 仅以 read mode 打开文件，在构造期读取为内存快照；VM 运行期间不再依赖文件内容变化，也不启动 write-back worker。
- `RamDiskBackend` 在 PCI model 下仍受 device-level read-only policy 约束；默认容量为 2 MiB，或使用显式 capacity。
- PCI transport 只接受 `requires_deferred_processing() == false` 的 backend。需要 worker、异步完成或 deferred retry 的 backend 在 function 构造前明确失败。
- device config 的 capacity 来自最终 backend 的 sector 数；不允许配置值、实际 snapshot 和 guest-visible capacity 互相漂移。
- write policy 是设备不变量，不以 guest 是否正确接受 `VIRTIO_BLK_F_RO` 为前提。任何 OUT 请求都不能到达 backend write。

## 5. PCI topology、配置空间与 BAR

### 5.1 topology 和资源所有权

- PCI config mechanism #1 唯一占用 PIO `0xcf8..=0xcff`；不增加 ECAM/MCFG。
- bus 0 固定保留 `00:00.0` host bridge 与 `00:1f.0` LPC bridge。
- endpoint ID 按确定性顺序从 `00:03.0` 起分配 device number；BDF 是 planner 结果，不是 block model 常量。
- endpoint 声明 4 KiB、4 KiB 对齐、32-bit、non-prefetchable BAR0，以及一个 exclusive level-triggered wired IRQ。
- ACPI PCI root `_CRS`、`_PRT`、MP table 与 runtime function 必须从同一 resolved endpoint/resource 得到 BAR aperture 和 GSI，不得各自硬编码。
- 当前 firmware plan 最多接受一个真实 PCI endpoint；超过一个时明确拒绝 VM 构建，而不是只描述第一个并让其余 function 静默运行。

### 5.2 VirtIO block PCI identity

| 字段 | 值 |
| --- | --- |
| Vendor ID | `0x1af4` |
| Device ID | `0x1042`（modern VirtIO block） |
| Revision | `1` |
| Class/Subclass/ProgIF | `0x01 / 0x80 / 0x00` |
| Header type | conventional endpoint `0` |
| Interrupt pin | INTA (`1`) |
| BAR0 | 4 KiB、32-bit memory、non-prefetchable |

PCI command 至少支持：

- Memory Space Enable：关闭时 BAR0 不参与 decode；
- Bus Master Enable：关闭时 queue enable 的 guest-memory probe 和 queue notify/DMA 必须失败；
- INTx Disable：只屏蔽物理 INTx 电平，不修改已产生的 VirtIO ISR 事件。

PCI Status 的 Interrupt Status 位由 transport 的有效 ISR pending 状态派生，不能与中断协调器各自维护独立事实。

### 5.3 固定 BAR 契约

runtime 当前没有安全的 MMIO remap 事务，因此 BAR0 使用 planner-fixed contract：

| guest 写入 | BAR0 读回 | runtime decode |
| --- | --- | --- |
| power-on/reset | resolved base + attributes | resolved base |
| all ones size probe | size mask + attributes | 仍为 resolved base |
| resolved base | resolved base + attributes | resolved base |
| 其他 base | 保持原值并记录诊断 | resolved base |

partial byte/word 写必须先按 write mask 合并完整 BAR dword，再执行同一分类。prefetchable/type attribute 不是可由 guest 翻转的地址位，普通读回、size probe 和 reset 都必须保持。只有完整落在 aperture 内的访问才转发为 BAR-relative offset。

若未来支持 BAR relocation，必须先提供 owner-aware `remap_mmio` 事务，使配置空间、runtime index、resource owner 和 firmware 状态以一个原子操作迁移；不能只修改 config header。

### 5.4 配置访问安全边界

root config mechanism 必须验证 data-port access、register、width 和 `register + size <= 256`。公开的 `PciFunction::config_read/config_write` 实现也必须独立验证相同条件并以 `DeviceError` 返回非法调用，而不能依赖 root 是唯一调用者或因 slice/shift 越界 panic。

当前 guest root 路径已有边界检查；普通与 VirtIO function 的直接公共调用仍需要补齐等价校验。这是 public capability contract 的健壮性要求，不改变正常 guest ABI。

## 6. VirtIO PCI capability 与 BAR0 布局

BAR0 固定布局：

| BAR0 offset | 区域 | 长度 | 说明 |
| --- | --- | --- | --- |
| `0x000` | common configuration | `0x38` | 完整 modern common config |
| `0x100` | queue notification | `4` | queue 0，multiplier `4` |
| `0x200` | ISR status | `1` | bit 0 为 queue interrupt |
| `0x300` | block device config | `16` | little-endian capacity、`size_max` 和 `seg_max` |

PCI capability list 必须依次包含：

1. `VIRTIO_PCI_CAP_COMMON_CFG`；
2. `VIRTIO_PCI_CAP_NOTIFY_CFG`；
3. `VIRTIO_PCI_CAP_ISR_CFG`；
4. `VIRTIO_PCI_CAP_DEVICE_CFG`；
5. `VIRTIO_PCI_CAP_PCI_CFG`。

`cap_len` 是包含 `cap_vndr/cap_next` 的 capability 总长度；`length` 是 BAR 内目标结构长度，两者不能混用。`VIRTIO_PCI_CAP_PCI_CFG` 的 bar/offset/length 是 guest-programmable 间接窗口，访问 data 字段前必须验证：

- BAR index 为 0；
- width 只能是 byte/word/dword；
- capability `length` 与本次 width/size 完全一致；
- target `offset + length` checked 且不越过 BAR0；
- data 字段内 partial access 仍落在 4-byte window 内。

间接访问最终必须调用和直接 BAR 相同的 transport dispatcher；通过 PCI_CFG 写 notify 与直接写 `BAR0+0x100` 必须产生相同 queue 事件。

## 7. Common configuration 与 feature/status 状态机

### 7.1 寄存器契约

| offset | register | width | 当前语义 |
| --- | --- | --- | --- |
| `0x00` | device_feature_select | 32 | page selector |
| `0x04` | device_feature | 32 | page 0/1 返回发布位，其余返回 0 |
| `0x08` | driver_feature_select | 32 | page selector |
| `0x0c` | driver_feature | 32 | 读回已发布且被选择的有效子集 |
| `0x10` | msix_config | 16 | 无 MSI-X，读 `0xffff`，写忽略 |
| `0x12` | num_queues | 16 | `1` |
| `0x14` | device_status | 8 | driver/device status 状态机 |
| `0x15` | config_generation | 8 | immutable config 通常保持 0 |
| `0x16` | queue_select | 16 | 仅 queue 0 有效 |
| `0x18` | queue_size | 16 | reset 为最大值 128，可缩小为不超过 128 的 2 的幂 |
| `0x1a` | queue_msix_vector | 16 | 无 MSI-X，读 `0xffff`，写忽略 |
| `0x1c` | queue_enable | 16 | queue 0 enabled 状态 |
| `0x1e` | queue_notify_off | 16 | queue 0 固定为 0 |
| `0x20..0x34` | desc/avail/used addresses | 64 | 支持 qword 或低/高 dword，读回当前值 |

不匹配的访问宽度、只读字段写入和区域越界都返回结构化 `DeviceError`。选择不存在的 queue 时 size/enable/address 读回 0，相关配置写不影响 queue 0。

当前实现出于防御性允许对 `queue_enable` 写 0 清理内部 queue；由于没有发布 `VIRTIO_F_RING_RESET`，compliant driver 不得把它视为可依赖的运行时 ring reset ABI。

### 7.2 发布 feature

block PCI adapter 只发布：

- `VIRTIO_F_VERSION_1`；
- `VIRTIO_BLK_F_SIZE_MAX`；
- `VIRTIO_BLK_F_SEG_MAX`；
- `VIRTIO_BLK_F_RO`。

device config 在 `0x08` 和 `0x0c` 分别暴露 `size_max` 与 `seg_max`；发布值与 request core 强制校验使用同一个 immutable `VirtioBlockConfig`，不能形成两套限制。Axvisor 当前默认值分别为 64 KiB 和 128 个 data segments。request core 始终先校验单 segment 长度、data segment 数量和 checked total length，再进行 fallible allocation；guest 是否接受相应 feature 不解除该安全上限，发布 feature 使 compliant driver 能在提交请求前发现并遵守限制。

没有发布的 FLUSH、MQ、EVENT_IDX、INDIRECT_DESC、DISCARD、WRITE_ZEROES 等能力不得改变 queue 或 request 行为。

### 7.3 feature negotiation

transport 分别保存：

- `driver_features`：guest 写入且 device 已发布的有效子集；
- `rejected_driver_features`：guest 在 page 0/1 写入的未发布位。

状态转换要求：

1. page 0/1 每次写入都替换该页的有效位和拒绝位，而不是只做 OR；
2. selector 大于 1 时读回 0、写入忽略，不污染 64-bit negotiation；
3. guest 设置 `FEATURES_OK` 时，如果仍有 rejected bits，或没有接受 `VIRTIO_F_VERSION_1`，device 在 status 读回中清除 `FEATURES_OK`；
4. 同一次 status 写还带 `DRIVER_OK` 时，协商失败必须同时清除 `DRIVER_OK`，写操作本身不伪装为 MMIO fault；
5. 失败后 guest 可以重写相应 feature page 并重试；
6. `FEATURES_OK` 成功后 feature 冻结，后续 driver feature 写返回明确 InvalidState；
7. status reset 清空有效位、拒绝位、selector 和 queue 状态。

RO 同时是发布 feature 和设备强制策略。即使错误 driver 没有接受 RO，也不能获得写能力；缺少 VERSION_1 则不得进入可用状态。

### 7.4 device status

正常顺序是：

```text
0
 -> ACKNOWLEDGE
 -> ACKNOWLEDGE | DRIVER
 -> ... | FEATURES_OK
 -> 配置并 enable queue 0
 -> ... | DRIVER_OK
```

非零 status 写只能添加 driver-owned 位，不能清除已经接受的位，也不能写 `DEVICE_NEEDS_RESET` 或 reserved bits。设置 `DRIVER_OK` 前必须已经具有 ACKNOWLEDGE、DRIVER、FEATURES_OK，并且 queue 0 enabled。

## 8. Queue、DMA 与 block request

### 8.1 queue 配置和锁边界

queue 0 最大深度为 128。desc、avail、used 地址和 queue size 在 enable 后冻结。enable 流程分三阶段：

1. **prepare（transport state lock 内）**：快照 queue 配置、generation 和 configuration epoch；
2. **validate（锁外）**：通过本 function 的 `DmaGrant` 和本次 `DeviceContext` 检查 desc/avail/used layout、对齐、checked range 与 guest-memory 可达性；
3. **commit（state lock 内）**：只有 generation 和 epoch 未变化时才把 candidate 标记 ready/enabled。

guest-memory callback、backend I/O 和 IRQ sink callback 都不得在 transport broad lock 或 IRQ-disabled state lock 内执行。并发 reset/reconfiguration 通过 generation/epoch 使旧 candidate 或旧 completion 失效，不允许回写到新一代设备状态。

### 8.2 Bus Master 与 DMA

- PCI Bus Master Enable 默认为关闭；配置头写入后必须同步到 transport。
- queue enable 的 guest-memory validation 和 queue notify 都要求 Bus Master Enable 已开启。
- 没有 root 注入的 `DmaGrant` 时不能访问 guest memory。
- 所有 descriptor、ring、header、data 和 status 访问都通过 scoped `GuestMemory` capability；不得保存裸 host address 到请求生命周期之外。
- Memory Space Enable 只控制 BAR decode，Bus Master Enable 独立控制 device 发起的 DMA；二者不能互相替代。
- function/root reset 恢复 PCI command 后必须同步 Bus Master 状态，避免配置头显示 disabled 而 transport 仍可 DMA。

### 8.3 block request 语义

共享 request core 负责：

- checked descriptor-chain traversal；
- header/status descriptor 存在性和方向验证；
- sector × 512、segment 总长、capacity end 的 checked arithmetic；
- `VIRTIO_BLK_T_IN` 读取 backend 并写 guest data；
- read-only 下 `VIRTIO_BLK_T_OUT -> VIRTIO_BLK_S_IOERR`，且不调用 backend write；
- 未发布/未知 request 返回 `VIRTIO_BLK_S_UNSUPP`；
- request 结果确定后才写 status 和 used ring。

错误处理必须区分两类：

- guest request 格式错误但 used ring 仍可安全访问：以确定性 error completion（status 可用时写 IOERR，否则 used length 0）归还已经消费的 head；
- ring/guest-memory 本身不可安全访问：锁存 queue fault，并通过 transport 的 `DEVICE_NEEDS_RESET` 或等价明确恢复信号让 driver reset，不能只把 head 从 avail 消费后永久静默卡住。

当前 snapshot 中 descriptor-chain 校验失败会先锁存 queue fault，随后通用 error completion 被 `QueueFaulted` 拒绝，尚未满足上述分类。修复时应先加入完整 notify 回归，证明不会出现“head 已消费、无 used completion、也无 NEEDS_RESET”的中间状态。

## 9. INTx、ISR 与通知抑制

### 9.1 状态所有权

`PciInterruptCoordinator` 统一拥有：

- VirtIO ISR bits；
- 是否存在需要通知 driver 的 pending event；
- 当前 INTx Disable；
- 当前 line asserted 状态；
- reset generation。

PCI Status Interrupt Status 必须直接反映有效 ISR pending，不能成为第五份独立状态。

### 9.2 completion 规则

当 split queue completion 返回 `notify = true` 时：

1. 先设置 ISR queue bit；
2. PCI Status Interrupt Status 随 ISR 变为 1；
3. INTx 未 disabled 时断言 level line；
4. 读取 ISR 原子取得并清零 bits，同时撤销 line；
5. ISR 写按位清除，同样同步 line。

当 `VIRTQ_AVAIL_F_NO_INTERRUPT` 使 completion 返回 `notify = false` 时，used ring 仍更新，但不能设置 ISR queue bit、PCI Interrupt Status 或断言 INTx。notification suppression 必须在 ISR、PCI status 和 physical line 三个观测面一致。

当前 snapshot 的 coordinator 在 `notify = false` 时仍设置 ISR，只是不设置 line request，尚未满足该不变量。修复回归必须同时检查 used ring 已更新、ISR 为 0、PCI status interrupt 位为 0、IRQ sink 没有 assert。

### 9.3 INTx Disable

INTx Disable 只影响 line：

- 有效通知发生时仍记录 ISR pending；
- disabled 期间不 assert；
- 重新 enable 时，如果 ISR 仍 pending，应重新 assert；
- disabled 期间读取/清除 ISR 后，重新 enable 不得产生幽灵中断。

queue notification suppression 与 PCI INTx Disable 不是同一语义：前者表示本次 completion 不产生通知事件，后者表示事件存在但暂时不驱动 pin。

## 10. Reset 与错误恢复

### 10.1 VirtIO status reset

guest 向 device status 写 0 时：

- generation 加一，transport 进入 InProgress；
- 清空 feature selectors、accepted/rejected features、status、queue configuration、fault、ISR 和 pending line；
- 调用 device core reset，丢弃 pending block head；
- 不修改 PCI configuration header，因此 PCI INTx Disable、Memory Space Enable 和 Bus Master Enable 保持 function 当前配置；
- 成功后进入 Ready；失败时 status 变为 `DEVICE_NEEDS_RESET`，进入可通过再次写 0 重试的 Failed 状态。

reset 期间的旧 queue lease/candidate/completion 因 generation 不匹配必须丢弃，不能重新发布旧 ISR 或 queue。

### 10.2 PCI function/root reset

function reset 是更宽的 power-on reset：

1. 恢复完整 PCI power-on config header，包括 Command、BAR 和 capability 可写字段；
2. 从恢复后的 Command 同步 Bus Master Enable 到 transport；
3. reset transport/core/ISR；
4. 从恢复后的 Command 同步 INTx Disable 到 coordinator；
5. 即使 transport/core reset 返回错误，也必须尝试完成 INTx 状态同步，并返回首个真实错误。

root reset 必须尝试 reset 所有 function，不能因第一个失败跳过后续清理；最终返回第一个错误。这样配置头的 INTx/BME 状态与 transport 内部协调器不会在重新枚举后分裂。

当前测试 guest 的内核/initramfs 没有可用的 driver unbind/rebind，也没有 sysfs function reset/FLR，因此“同一 VM 内 reset 后重新协商”不是当前 E2E 可达操作。该限制只改变证据层级：状态机和 root/function reset 由 deterministic host tests 验证，SVM/VMX E2E 继续证明正常启动/I/O 没有回退。

## 11. Firmware 与产品能力边界

`DeviceFirmwarePciSpec` 只描述 endpoint 的 interrupt pin、BAR slot 和 IRQ slot。x86 planner 负责赋予 architecture identity；firmware composer 只消费 resolved identity/resources。

当前产品约束是：

- 0 个 endpoint：PCI root 仅发布 config mechanism，固件不发布 endpoint window/route；
- 1 个 endpoint：发布实际 BAR aperture、BDF/INTA 到 resolved GSI 的 `_PRT` 与一致 MP route；
- 大于 1 个 endpoint：当前 firmware plan 明确报错。

因此 `PciFunctionServiceKey::Multiple` 是 root/framework 能力，不是多盘产品承诺。若以后支持多个 guest-visible endpoint，必须同时把 `X86VmPlan`、ACPI `_CRS/_PRT`、MP routing、BDF identity 和 E2E 扩成 vector；不能只让 runtime root 接受多个 function。

## 12. 验证策略

### 12.1 最低层 deterministic tests

| 层 | 成功路径 | 失败/边界 |
| --- | --- | --- |
| `axvirtio-common` queue | layout、pop、completion、suppression | cycle、bad next、overlap、overflow、memory fault、fault/reset |
| VirtIO PCI transport | feature success、queue enable/notify、ISR assert/read/deassert | rejected feature/retry/freeze/reset、missing VERSION_1、callback lock boundary、BME disabled、suppressed completion |
| `axvirtio-blk` | IN、scatter、capacity、PCI/MMIO 共用 core | OUT=IOERR、unknown=UNSUPP、malformed chain、DMA fault recovery |
| `axdevice` PCI | config routing、capability chain、fixed BAR、PCI_CFG、root reset | absent BDF、invalid config width/range、BAR partial writes、INTx Disable、BME reset sync |
| `axvm` | endpoint resource、stable BDF、root aggregation、ACPI/MP | 0/multiple services、BDF 不接受、build rollback、多 endpoint firmware limit |
| `axvisor` | static/runtime snapshot、file/ramdisk、catalog | read_only=false、missing fs/static asset、empty/unaligned/short asset、deferred backend |

修 bug 时必须先加入在旧实现上必然失败的 deterministic regression，保存 red 证据，再完成 green；不能只依赖 E2E 恰好不触发错误分支。

### 12.2 deterministic 数据盘

测试资产为 8 MiB raw ext4，无 MBR/GPT，guest 直接挂载 `/dev/vda`。生成器固定 UUID、时间、mode/mtime，关闭 lazy inode/journal 初始化，并连续生成两次验证 SHA-256 一致。镜像只包含：

```text
/axvisor/identity.txt = axvisor-readonly-data-v1\n
```

reference QEMU 用标准 read-only `virtio-blk-pci` 挂载同一 raw image，证明镜像与 Linux driver 兼容；它不能替代 Axvisor E2E。

### 12.3 E2E

```bash
cargo xtask axvisor test qemu --arch x86_64 --test-case pci-block-vmx
cargo xtask axvisor test qemu --arch x86_64 --test-case pci-block-svm
```

initramfs 按 vendor/device 扫描 endpoint，不把 `00:03.0` 当永久 ABI；检查 driver binding、BAR、只读标志、写失败、identity 和 INTx count 后才输出成功 marker。E2E 不以“设备节点存在”或“能够读 sysfs”替代真实块读取。

### 12.4 常规验证命令

```bash
cargo fmt --all --check
cargo test -p axvirtio-common
cargo test -p axvirtio-blk
cargo test -p axdevice --features host-test
cargo test -p axvm --features host-test
cargo test -p axvisor
cargo xtask clippy --package axvirtio-common --package axvirtio-blk --package axdevice --package axvm
```

Axvisor 本体 clippy 需要使用适配目标/build configuration 的 Axvisor xtask 流程；普通 package clippy 跳过不能当作已验证。

## 13. 建议的整理后 commit 序列

本节描述如果以当前最终架构重新整理 feature history，建议保留的逻辑提交。它不是现有 commit 流水账；目标是让每个提交职责单一、可单独 review、可单独回退，并且不会出现中间提交宣称半成品 feature 可用。

### 13.1 主体提交

1. `docs(axvisor): define the x86 read-only virtio-blk-pci design`
   - 加入本文，固定问题、非目标、分层、协议状态机、错误恢复和验证矩阵。
   - 不修改产品代码。

2. `refactor(axvirtio-blk): share block requests across MMIO and PCI`
   - 从 MMIO transport 抽出 `VirtioBlockRequestCore` 和 transport-independent config/policy。
   - 保持既有 MMIO API、deferred backend 和 AArch64 行为。
   - 同一提交补齐 malformed request、只读 OUT、capacity、DMA fault 的最低层测试。

3. `refactor(axdevice): establish generic x86 PCI functions and fixed BARs`
   - table-driven BDF/config mechanism、`PciBdf`、`PciBar`、`PciFunction`、`PciMmioFunction`、root aggregation。
   - 同时落实 fixed BAR、attribute preservation、config access bounds 和 reset contract；按职责拆分 `x86/pci/` 模块。
   - 不包含 VirtIO 或 block 常量。

4. `feat(axvirtio-common): add a reusable VirtIO PCI transport`
   - `VirtioDeviceCore`、capability set、common config、feature/status、queue lifecycle、PCI_CFG、ISR/INTx、Bus Master 和 reset generation。
   - 从一开始就覆盖 unsupported feature、lock-free callback、reset failure/retry、INTx Disable、notification suppression 和 BME 测试，避免后续追加多个状态机修复提交。

5. `feat(axdevice,axvirtio-blk): expose a generic VirtIO PCI block function`
   - `VirtioPciFunction<D>` 负责 PCI header/BAR/capability wiring；`VirtioBlockPciAdapter` 负责 block device config/features/notify。
   - 证明 transport 中没有 block 常量，block adapter 中没有 PCI topology/resource allocation。

6. `feat(axvm): plan PCI endpoints from the resolved device graph`
   - firmware endpoint marker、strict dependency service cardinality、稳定 BDF、root DMA grant、ACPI `_CRS/_PRT` 和 MP route 同源。
   - builder 注册失败原子性、root/function reset 和当前单 endpoint firmware 限制在本提交测试。

7. `feat(axvisor): register an immutable virtio-blk-pci data disk`
   - catalog entry、`read_only=true` policy、MemorySnapshot/ReadOnlyFile/RamDisk backend、generic VM asset lookup、x86-only registration。
   - 不包含测试镜像生成器和 CI 接线。

8. `test(axvisor): validate the PCI data disk through Linux`
   - deterministic ext4、reference QEMU、initramfs assertion、VMX/SVM test cases、digest 与 INTx 证据。
   - 测试辅助工具修正应在 squash/rebase 时吸收到该提交，而不是保留多个“修测试脚本”提交。

9. `ci(axvisor): run PCI block tests on SVM and VMX`
   - 只接入已经能在本地通过的两个 case；不混入产品逻辑。

### 13.2 当前分支收尾建议

在按上述结构整理历史前，当前 HEAD 尚需用独立、失败优先的提交完成以下契约：

1. `fix(virtio-pci): honor suppressed queue completion interrupts`
   - 使 used completion、ISR、PCI status 和 INTx 对 `NO_INTERRUPT` 一致。

2. `fix(virtio-blk): recover malformed descriptor requests`
   - 区分可 error-complete 的 guest request 错误和必须 `DEVICE_NEEDS_RESET` 的 ring/memory fault。

3. `fix(axdevice): validate direct PCI function config accesses`
   - public trait 直接调用越界返回错误，不 panic；可以和 model unknown-option hardening 一起评估，但不要与 transport 状态机修复混为一个提交。

完成并验证后，再进行 interactive rebase 或重建分支，把这些修复分别吸收到最早拥有该不变量的主体提交 3/4/2 中；保留独立修复 commit 直到 review 证据稳定，避免过早 squash 丢失 red/green 可审查性。

## 14. 合入判定

满足以下条件后，该设计才算由实现完整兑现：

- 分层依赖方向和能力所有权符合 §3；
- PCI config/BAR/PCI_CFG、feature/status、queue/DMA、INTx 和 reset 状态机符合 §5–§10；
- malformed request 和 suppressed notification 不存在无恢复或状态分裂；
- firmware/runtime 资源来自同一 resolved graph，产品仍明确限制一个 endpoint；
- 所有最低层测试、fmt、targeted clippy 通过；
- 当前最终 HEAD 的 VMX/SVM E2E 命中严格 marker，数据盘 digest 不变；
- PR 描述、验证证据和本文指向同一最终代码状态。

任一协议或恢复不变量失败时，不能通过放宽 marker、延长 timeout、让测试直接读取宿主镜像或把错误路径标成 unsupported 来替代修复。
