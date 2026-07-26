# AxVisor UEFI probe.efi 设计

## 1. 文档状态

本文档是 `probe.efi` 第一阶段的设计草案，对应 [issue.md](./issue.md)。第一阶段只交付独立 UEFI 应用及其原生 OVMF/QEMU 验证链，不包含 AxVisor 嵌套 Guest 集成。

设计以 UEFI 2.11 和 ACPI 6.6 的公开契约为依据，并遵循仓库现有 `axloader` 的 UEFI target、串口输出和 QEMU smoke test 方式。具体实现仍应以仓库固定的 Rust nightly、`uefi` crate 版本和 xtask 接口为准。

## 2. 背景

AxVisor Guest UEFI 支持需要逐步补齐以下平台能力：

- OVMF 固件执行和 UEFI ABI；
- ACPI 平台描述；
- PCI Root Bridge 和设备枚举；
- Block IO 与启动介质；
- Runtime Services；
- 后续的设备中断和完整操作系统启动。

如果每一阶段都直接使用 Linux、NimbOS 或 ArceOS 作为唯一 Guest 测试载荷，失败时很难判断问题位于固件、平台描述、设备模型、Guest 驱动还是操作系统。`probe.efi` 的作用是提供一个更小的 Guest 侧观测点，在 `ExitBootServices` 之前直接验证标准 UEFI 表和协议。

`probe.efi` 不是 UEFI 实现的完整一致性测试套件，也不替代操作系统启动测试。它验证的是 AxVisor UEFI 计划当前需要、且能够通过标准 UEFI 应用观察到的平台契约。

## 3. 设计目标

### 3.1 必须实现的目标

1. **独立于 AxVisor 内部实现。**
   probe 只能使用 UEFI 表、Configuration Table 和标准 protocol，不能链接或调用 AxVisor、AxVM、axdevice 私有 API。

2. **探测与策略分离。**
   探测器产生“发现了什么”的事实，profile 决定某项事实是必须、可选还是不适用。不同平台阶段使用同一份 EFI 文件。

3. **默认只读。**
   默认 profile 不修改磁盘、PCI 配置、固件变量、系统时间或复位状态。唯一写操作是显式启用并保证删除清理的变量 round-trip。

4. **结果可确定性判断。**
   输出包含版本化 marker、稳定检查名、固定状态和结构化原因。CI 不依赖固件地址、时间值、表顺序、handle 顺序或非确定性日志。

5. **probe 自身可被独立验证。**
   固件表解析、profile 策略和报告器可在 host 上测试；EFI 入口和 UEFI protocol 交互在已知可用的 OVMF/QEMU 上测试。

6. **失败可定位。**
   解析错误、协议缺失、能力不满足、测试配置错误和 runner 超时必须有不同的检查名或失败原因。

### 3.2 非目标

- 不实现 bootloader，不加载 PE、ELF、Linux EFI stub 或其他内核；
- 不调用 `ExitBootServices`，不验证退出后的 Runtime virtual mapping；
- 不初始化 PCI 设备，不写 BAR、command register、MMIO 或 I/O port；
- 不提交 VirtIO request，不验证 DMA 或 queue；
- 不验证 LAPIC、IOAPIC、INTx、MSI/MSI-X、EOI 或 vCPU 中断注入；
- 不提供完整 UEFI/ACPI 规范一致性认证；
- 第一阶段不修改 AxVisor VM 配置、Guest loader 或设备模型；
- 不为通过测试而固定某个 OVMF 版本的私有实现细节。

## 4. 总体架构

```text
UEFI entry
   │
   ├── parse load options ───────────────┐
   │                                     │
   ▼                                     ▼
ProbeProfile                         ProbePolicy
   │                                     │
   └──────────────┬──────────────────────┘
                  ▼
              ProbeRunner
                  │
       ┌──────────┼───────────┬────────────┐
       ▼          ▼           ▼            ▼
   UEFI Core     ACPI       PCI/Block    Runtime
       │          │           │            │
       └──────────┴───────────┴────────────┘
                  ▼
             ObservedFacts
                  │
                  ▼
             CheckResults
                  │
                  ▼
           Reporter → stdout/Serial IO
                  │
                  ▼
      UEFI_PROBE_PASS / UEFI_PROBE_FAIL
```

边界约束：

- `ProbeRunner` 只按固定顺序编排检查，不包含底层解析细节；
- UEFI protocol 类型限制在 UEFI adapter 层，不进入纯解析器和策略类型；
- ACPI 裸指针只在固件边界模块解引用，解析器接收有明确长度的字节切片；
- `ObservedFacts` 表达观察结果，`ProbePolicy` 决定 required/optional；
- `Reporter` 只序列化结果，不改变检查状态；
- 任一检查失败不应阻止仍可安全执行的后续检查，以便一次运行收集完整诊断。

## 5. 工程组织

建议目录：

```text
test-suit/axvisor/uefi/probe/
├── Cargo.toml
├── README.md
├── src/
│   ├── main.rs
│   ├── lib.rs
│   └── probe/
│       ├── mod.rs
│       ├── profile.rs
│       ├── result.rs
│       ├── report.rs
│       ├── system.rs
│       ├── memory.rs
│       ├── acpi.rs
│       ├── pci.rs
│       ├── block.rs
│       └── runtime.rs
└── tests/
    └── report_protocol.rs
```

package 和 binary：

```toml
[package]
name = "axvisor-uefi-probe"

[[bin]]
name = "probe"
path = "src/main.rs"
```

目标产物：

```text
target/x86_64-unknown-uefi/release/probe.efi
```

模块职责：

- `main.rs`：条件化的 `no_std`/`no_main`、UEFI entry、初始化和最终状态返回；
- `lib.rs`：host 和 UEFI target 共享的稳定入口；
- `probe/mod.rs`：目录式编排和模块导出，不承载大段实现；
- `profile.rs`：load options、profile 和期望设备的强类型配置；
- `result.rs`：检查名、状态、原因和汇总不变量；
- `report.rs`：版本化机器输出；
- `system.rs`/`memory.rs`：UEFI 核心表和 Memory Map；
- `acpi.rs`：Configuration Table 边界和有界 ACPI 解析；
- `pci.rs`/`block.rs`/`runtime.rs`：对应 UEFI protocol adapter。

所有实现默认私有，仅导出 host 测试和入口编排真正需要的类型。生产路径不使用裸 `unwrap()`；可恢复的协议缺失或数据错误使用具体错误类型返回。

## 6. 配置与 Profile

### 6.1 配置文件与 Load Options

probe 首先尝试从当前 Loaded Image 所在文件系统读取固定路径
`\EFI\BOOT\probe.cfg`，再读取 Loaded Image 的 UTF-16 load options。配置文件是
UTF-8 文本，最大 4 KiB，允许空行和以 `#` 开头的注释；文件存在时必须声明
`version=1`。Load options 使用空格分隔的相同 `key=value` 语法，但不携带
`version`。

固件 adapter 必须在复制前执行输入边界检查：先通过文件 metadata 拒绝超过 4 KiB 的
`probe.cfg`，并先检查借用的 Load Options slice 不超过 2048 个 UTF-16 code units，再
创建 owned buffer。

版本 1 支持：

```text
profile=uefi-core
profile=pc-platform
profile=runtime-vars
expect-pci=1af4:*
expect-block-magic=AXPROBE1
variable-mode=read-only
variable-mode=roundtrip
```

约束：

- 合并优先级固定为内置默认值、配置文件、Load Options，后者覆盖前者；
- 配置文件缺失且未提供 Load Options 时使用 `uefi-core`；
- 每个输入层内的 key 重复、未知 key、非法 UTF-8/UTF-16、非法十六进制或空值均为配置错误；
- 不使用多个布尔 flag 表示组合状态；profile 和 variable mode 使用 enum；
- `expect-pci` 和 `expect-block-magic` 是显式测试条件，不从枚举顺序猜测目标设备；
- 配置错误必须在 `UEFI_PROBE_BEGIN` 后输出 `config` 检查失败和最终 FAIL。

### 6.2 Profile 定义

三个 profile 按能力递增：`pc-platform` 包含 `uefi-core` 的要求，`runtime-vars` 再包含 `pc-platform` 的要求。这样 profile 名同时表达本次运行希望验证到的平台阶段。

| 能力 | `uefi-core` | `pc-platform` | `runtime-vars` |
|---|---|---|---|
| System Table / services | required | required | required |
| Loaded Image | required | required | required |
| Memory Map | required | required | required |
| ACPI RSDP / XSDT | optional | required | required |
| FADT / MADT | optional | required | required |
| MCFG | optional | required | required |
| PCI Root Bridge | optional | required | required |
| 目标 PCI 设备 | 未配置则 skip | 按参数 required | 按参数 required |
| Block IO 只读 | optional | required | required |
| 测试盘 magic | 未配置则 skip | 按参数 required | 按参数 required |
| GetTime | optional | optional | required |
| 变量服务查询 | optional | optional | required |
| 变量 round-trip | skip | skip | 仅显式启用时 required |

`optional` 表示缺失时记录 `SKIP`，但如果能力存在而内容损坏，应记录 `FAIL`，不能用 optional 掩盖无效数据。

## 7. 检查模型

### 7.1 状态

```rust
enum CheckStatus {
    Pass,
    Fail,
    Skip,
}
```

每个检查结果至少包含：

- 稳定且唯一的检查名；
- `PASS`、`FAIL` 或 `SKIP`；
- 可选的稳定 reason code；
- 有界的诊断字段。

状态规则：

- required 能力满足：`PASS`；
- required 能力缺失或无效：`FAIL`；
- optional 能力缺失：`SKIP`；
- optional 能力存在但结构无效或 protocol 调用失败：`FAIL`；
- 当前 profile 不允许执行有副作用的检查：`SKIP`；
- probe 自身配置或内部不变量错误：`FAIL`。

### 7.2 稳定失败原因

第一版 reason code 至少包括：

```text
not-found
unsupported
protocol-error
invalid-signature
invalid-length
invalid-checksum
truncated
integer-overflow
invalid-field
expectation-not-met
cleanup-failed
invalid-config
internal-error
```

reason code 属于输出协议。增加新值允许向后兼容；重命名或改变既有值语义需要提升协议版本。

## 8. 能力设计

### 8.1 UEFI System Table

检查内容：

- UEFI revision；
- firmware vendor 和 firmware revision；
- System Table header；
- Boot Services 和 Runtime Services 表是否可访问；
- Loaded Image protocol；
- 当前 image device handle 和可用的 device path。

probe 可以报告 header revision、header size 等诊断值，但不得把某个 OVMF vendor 字符串或具体地址作为通过条件。

UEFI helper 初始化失败时仍应尽可能通过最低层输出路径报告 `internal-error`；如果无法建立任何输出通道，则以错误 UEFI status 返回。

### 8.2 输出通道

输出优先使用 UEFI stdout，同时通过 UEFI Serial IO protocol 写入测试 runner 捕获的串口。实现可复用 axloader 已验证的换行处理和 Serial IO protocol 枚举方式，不直接操作 COM1 I/O port。

要求：

- 串口参数设为 115200 8N1；
- 输出 `\r\n`，runner 同时接受行尾 `\n` 和 `\r\n`；
- stdout 与 Serial IO 指向同一设备导致重复行时，结果仍可识别；
- 最终 marker 必须语义唯一，runner 不以“出现次数恰好为 1”判断成功；
- 输出失败不得改变已经计算的检查状态，但必须影响最终执行状态或留下可观察的错误状态。

### 8.3 Memory Map

probe 在 Boot Services 有效期间获取 Memory Map，不调用 `ExitBootServices`。

检查内容：

- map 非空；
- descriptor size 至少能容纳当前 descriptor；
- descriptor version 可被当前实现处理；
- `page_count * 4096` 和 `physical_start + length` 不溢出；
- 对每种 memory type 汇总 descriptor 数和页数；
- 不要求 descriptor 按地址排序，也不要求相邻区间合并。

Memory Map 在分配后可能变化，因此 probe 不比较 map key，也不把 descriptor 总数写成固定期望。

### 8.4 ACPI

#### Configuration Table 选择

1. 查找 ACPI 2.0 Configuration Table GUID；
2. 找不到时查找 ACPI 1.0 GUID；
3. profile required 时两者均不存在则 `FAIL/not-found`；
4. optional 时不存在则 `SKIP/not-found`。

#### RSDP

- 校验 `RSD PTR ` signature；
- 始终校验 ACPI 1.0 前 20 字节 checksum；
- revision >= 2 时校验 length 下限、上限和 extended checksum；
- revision >= 2 优先使用 XSDT，旧 revision 使用 RSDT；
- 地址转换和长度计算使用 checked arithmetic。

#### System Description Table

- 先读取固定大小 header；
- 校验 signature、length 下限和实现定义的最大表长；
- 在构造完整 slice 前确认长度上限；
- 校验整个表 checksum；
- XSDT/RSDT payload 必须是表项宽度的整数倍；
- 表项数量设定明确上限，防止损坏固件造成无界遍历；
- 根表重复同一物理地址时，`acpi-root` 以 `invalid-field` 失败，并只遍历该地址的首次出现；同一必需 signature 的异常重复归到对应固定检查；
- 未识别 signature 仍执行公共 SDT header、length 和 checksum 校验；有效未知表不新增固定检查，损坏未知表使 `acpi-root` 失败。

建议限制单表最大长度为 1 MiB、根表表项最多 4096 个。该限制是 probe 的防御性边界，不是 UEFI/ACPI 平台能力要求，应在错误中明确报告为 `invalid-length`。

#### 目标表

- FADT：检查 header、checksum 和最小可读取长度；
- MADT：检查 local APIC address、flags 和 variable entries 的长度推进；
- MCFG：检查 allocation entry 对齐、segment 和 bus range，确保 start bus 不大于 end bus；
- SMBIOS：可作为后续 optional 检查，不进入第一阶段完成条件。

probe 不根据表中地址访问 LAPIC、IOAPIC 或 ECAM MMIO；这些地址只作为事实报告和结构检查。

### 8.5 PCI

通过 UEFI `PciRootBridgeIo` protocol 枚举可访问的 PCI 地址范围，并读取每个 BDF 的标准配置头：

- vendor ID；
- device ID；
- revision/class/subclass/prog-if；
- header type；
- segment、bus、device、function。

约束：

- vendor ID 为 `0xffff` 表示 function 不存在，不作为协议错误；
- multifunction 扫描依据 header type，不盲目要求所有 function；
- 不读取超出标准 header 的 capability，除非后续 profile 明确增加；
- 不写 PCI configuration space；
- 不假设目标设备是枚举到的第一个设备；
- `expect-pci=vendor:device` 按集合匹配，`*` 表示任意 device ID；
- 输出按 segment/BDF 排序，屏蔽 protocol 返回顺序差异。

### 8.6 Block IO

枚举所有 Block IO handle，记录：

- media ID；
- media present；
- logical partition；
- read only；
- block size；
- I/O alignment；
- last block。

只读检查：

1. 跳过 `media_present = false` 的 handle；
2. 在分配前验证 block size 为 `1..=1 MiB`；
3. 在分配前验证 `io_align` 为 0、1 或不超过 1 MiB 的 2 的幂；
4. 使用 fallible allocation 创建不超过 2 MiB 的对齐 backing buffer；
5. 读取 LBA 0；
6. 在起始位置匹配 `expect-block-magic`；
7. 从全部 handle 中匹配目标，不依赖 handle 顺序。

若 handle 存在但全部 protocol open 都失败，结果为 `protocol-error`，不能降级为设备
缺失；部分 handle 打开失败时仍聚合成功打开的设备。

测试盘应是临时、确定性的 raw image。第一版只需要固定 magic，不必实现完整文件系统。probe 不调用 Block IO 写接口。

### 8.7 Runtime Services

#### GetTime

调用 `GetTime` 后检查：

- month 为 1..=12；
- hour 为 0..=23；
- minute 和 second 在规范范围内；
- nanosecond 小于 1 秒；
- timezone/daylight 字段只做结构检查。

不把具体时间与 host wall clock 比较，避免 CI 时区、RTC 和执行延迟造成不稳定。

#### 变量查询

只读模式检查变量服务是否可调用，并查询变量存储信息。已有固件变量的名称和数量只作为诊断，不设置固定值。

#### 变量 round-trip

仅在 `profile=runtime-vars variable-mode=roundtrip` 时执行：

1. 使用项目专属 vendor GUID；
2. 变量名固定为 `AxVisorUefiProbeV1`；
3. 写入固定、短小的 payload；
4. 读取并逐字节比较；
5. 删除变量；
6. 再次读取并确认不存在。

删除或确认清理失败必须输出 `cleanup-failed` 并令最终结果失败。测试不修改 Secure Boot、BootOrder、Boot#### 或其他已有变量。

单次运行只验证 round-trip。版本 1 不实现跨重启持久性测试，也不通过默认 probe 行为
隐式保留变量。

## 9. 输出协议

### 9.1 行格式

协议版本 1 使用一行一条记录、空格分隔的 `key=value`：

```text
UEFI_PROBE_BEGIN version=1 profile=pc-platform arch=x86_64
UEFI_PROBE_CHECK name=system-table status=PASS
UEFI_PROBE_CHECK name=memory-map status=PASS detail=descriptors%3D48
UEFI_PROBE_CHECK name=acpi-rsdp status=PASS detail=revision%3D2
UEFI_PROBE_CHECK name=acpi-mcfg status=PASS detail=allocations%3D1
UEFI_PROBE_CHECK name=pci-expected status=PASS detail=matches%3D1
UEFI_PROBE_CHECK name=block-magic status=PASS detail=matches%3D1
UEFI_PROBE_SUMMARY passed=16 failed=0 skipped=0
UEFI_PROBE_PASS
```

失败示例：

```text
UEFI_PROBE_CHECK name=acpi-rsdp status=FAIL reason=invalid-checksum
UEFI_PROBE_SUMMARY passed=15 failed=1 skipped=0
UEFI_PROBE_FAIL
```

### 9.2 编码规则

- record name、key、status 和 reason 只使用 ASCII 固定枚举；
- 动态诊断统一放在可选 `detail=<percent-encoded-UTF-8>` 字段中；不定义
  `UEFI_PROBE_INFO` record；
- 百分号编码使用大写十六进制且必须规范化：安全字符不得转义，其他字节必须转义；
- 单行最大 512 字节，超出部分截断并增加 `truncated=1`；
- 检查按设计定义的固定顺序输出；PCI/Block/ACPI 枚举结果先排序再输出；
- version 1 envelope 的首次逻辑记录必须严格遵循 BEGIN、16 个 CHECK、SUMMARY、terminal marker；相同的物理日志副本可以重复出现，但不能改变首次逻辑记录顺序；
- 每个稳定检查名在逻辑结果集合中只出现一次；
- `truncated=1` 只能跟在 `detail` 后；parser 拒绝其他值；
- `passed + failed + skipped` 必须等于检查结果总数；
- 出现任意 `FAIL` 时最终 marker 必须为 `UEFI_PROBE_FAIL`。

### 9.3 UEFI 返回状态

- 全部 required 检查通过：`Status::SUCCESS`；
- 出现检查失败或配置错误：返回错误 status，并输出 `UEFI_PROBE_FAIL`；
- panic handler 输出 `panic:` 后停止，不得伪造最终 PASS。

runner 验证固定顺序、完整字段 schema、summary 和完整行 marker，同时检查 panic、QEMU
提前退出和超时。看到 terminal marker 后先进入有界输出排空期，再终止 QEMU 和解析完整
transcript，避免漏掉晚到的冲突 marker 或 panic。不能只依赖进程退出码，因为 QEMU
进程状态不等同于 EFI application 返回状态。完整矩阵中的 core 用例运行两次，并比较
profile、check status/reason 和最终状态；动态 detail 不参与跨运行一致性判断。

## 10. 安全与健壮性

### 10.1 固件信任边界

UEFI Configuration Table 提供的是固件地址。probe 必须把它视为不可信结构数据，但 EFI 应用无法在解引用前证明固件给出的地址一定映射有效。因此安全契约分为两层：

1. 根据 UEFI ABI，固件保证 Configuration Table 指针在 Boot Services 期间可访问；
2. probe 对指针指向的数据执行 signature、长度、checksum、对齐和溢出验证。

所有裸指针解引用必须位于小型边界函数中，并在 `unsafe` 前说明上述前提。读取多字节字段应考虑未对齐数据，不能直接构造可能违反对齐要求的 Rust 引用。

### 10.2 有界资源使用

- ACPI 单表长度、根表项数和设备枚举数均有上限；
- 所有乘加运算使用 checked arithmetic；
- 诊断字符串和输出行有最大长度；
- protocol 调用失败转换为具体错误，不使用 `unwrap()` 或 `.ok()` 忽略；
- 一项检查失败后，仅继续执行不依赖该无效输入的检查；
- 不因缺少 optional protocol 猜测默认地址、设备或表。

### 10.3 副作用控制

- 默认 profile 只读；
- 变量 round-trip 需要 profile 和 variable mode 双重显式选择；
- 变量删除是测试成功的必要条件；
- runner 只使用临时磁盘和临时 OVMF VARS 副本；
- 不对系统 OVMF 模板、开发者磁盘或现有固件变量执行写操作。

## 11. 验证设计

验证分为四层。前三层属于本设计完成条件，第四层是后续 AxVisor 集成。

### 11.1 第一层：host 单元测试

纯逻辑模块在 host target 上测试：

#### ACPI fixture

- 合法 RSDP 1.0 和 2.0；
- 合法 RSDT、XSDT、FADT、MADT、MCFG；
- 错误 signature；
- primary 和 extended checksum 错误；
- header 或 payload 截断；
- length 小于 header；
- length 超过防御性上限；
- 表项宽度不整除；
- 地址和长度整数溢出；
- MADT variable entry 长度为零或越界；
- MCFG bus range 反转；
- 未知表和异常重复表。

fixture 应直接构造字节布局并独立计算期望 checksum，不能使用被测解析器生成自己的全部输入。

#### Profile 和结果

- required 能力缺失为 FAIL；
- optional 能力缺失为 SKIP；
- optional 能力存在但损坏为 FAIL；
- 未启用副作用检查为 SKIP；
- 重复检查名和非法状态转换被拒绝；
- summary 计数和最终状态一致。

#### 报告协议

- 输出顺序固定；
- 动态字符串正确编码；
- 控制字符不能注入新 marker；
- 超长字段被有标志地截断；
- PASS 和 FAIL marker 互斥；
- `\n`、`\r\n` 均可被 runner 解析。

### 11.2 第二层：target 和产物验证

建议命令：

```bash
rustup target add x86_64-unknown-uefi

cargo fmt --all -- --check
cargo test -p axvisor-uefi-probe --all-targets
cargo xtask clippy --package axvisor-uefi-probe

cargo clippy \
  -p axvisor-uefi-probe \
  --target x86_64-unknown-uefi \
  --bin probe \
  -- -D warnings

cargo build \
  -p axvisor-uefi-probe \
  --target x86_64-unknown-uefi \
  --bin probe \
  --release

llvm-readobj --file-headers \
  target/x86_64-unknown-uefi/release/probe.efi
```

产物必须满足：

- machine 为 AMD64；
- subsystem 为 EFI application；
- 有有效 entry point；
- release 产物名称固定为 `probe.efi`。

如果 xtask 尚不能表达 UEFI target 的特殊 clippy/build 参数，应先检查并扩展 xtask；原生 Cargo 命令只作为与 xtask 参数一致的底层验证。

### 11.3 第三层：原生 OVMF/QEMU

新增命令：

```bash
cargo xtask uefi-probe build
cargo xtask uefi-probe test qemu
```

runner 设计：

1. 定位匹配的 OVMF CODE 和 VARS 模板；
2. 创建临时目录和 ESP；
3. 将 `probe.efi` 安装为 `EFI/BOOT/BOOTX64.EFI`；
4. 将 VARS 模板复制到临时可写文件；
5. 创建带固定 magic 的临时 raw Block IO 测试盘；
6. 使用 `q35`、TCG、串口 stdio 和明确 timeout 启动 QEMU；
7. 收集 stdout/stderr transcript；
8. 解析完整行 marker 和 summary；
9. 终止 QEMU，并在失败时打印完整命令、固件路径和 transcript。

基础矩阵：

| 用例 | QEMU 设备 | Profile | 预期 |
|---|---|---|---|
| core | OVMF + ESP | `uefi-core` | PASS |
| PC platform | Q35 + OVMF + VirtIO Block | `pc-platform` | PASS |
| runtime variables | 同一临时 VARS 副本 | `runtime-vars` | PASS 并完成清理 |
| expected failure | 要求不存在的 PCI ID | `pc-platform` | probe 输出预期 FAIL，runner 将其判为负向用例通过 |

已知正确的 OVMF 正向用例证明 EFI 入口、protocol adapter、输出和 runner 可以协同工作；故意失败用例证明 profile 约束、失败 marker 和 runner 不会产生假阳性。host 损坏 fixture 证明解析器不会因为正向 OVMF 恰好提供合法数据而掩盖错误路径。

### 11.4 第四层：AxVisor 嵌套集成

第四层不属于本 issue。PR #1612 合入后另行设计：

```text
outer QEMU/host
  → AxVisor
  → guest OVMF
  → probe.efi
  → UEFI_PROBE_PASS
```

集成测试复用完全相同的 `probe.efi` 和输出协议，不在 probe 中增加 AxVisor 特判。AxVisor test-suit 只负责准备 Guest ESP、VM 配置、虚拟设备和 marker 匹配。

该测试可以证明 AxVisor 向 Guest 暴露了所需 UEFI/ACPI/PCI/Block IO 契约，但仍不能证明设备中断、VirtIO queue、DMA 或完整操作系统启动正确；这些能力继续由专用 Guest workload 验证。

## 12. xtask 与 CI 集成

现有 axloader 已实现 UEFI target 构建、OVMF 搜索、临时 ESP、QEMU 子进程、timeout 和 transcript 收集。probe runner 应优先抽取小型通用 UEFI QEMU 支持层，至少复用：

- firmware candidate 和环境变量解析；
- 临时 ESP staging；
- CODE/VARS pflash 参数；
- 输出读取、timeout 和进程终止；
- 失败 transcript 格式。

通用层只处理 QEMU/OVMF 生命周期，不理解 axloader HTTP 协议或 probe marker。axloader 和 probe 各自保留自己的场景判定器，避免形成同时理解多个测试协议的“大 runner”。

CI 建议增加：

1. host tests 和 target clippy/build；
2. 原生 OVMF `uefi-core` smoke；
3. Q35 + VirtIO Block `pc-platform` smoke；
4. 负向 expectation 用例。

probe 自身不需要 KVM，优先使用 TCG，避免把独立自测绑定到 self-hosted runner。后续 AxVisor VMX/SVM 嵌套执行再使用对应的 KVM/self-hosted 环境。

## 13. 与其他开发方向的关系

### 13.1 PR #1612

本设计第一至第三层只新增独立 package、解析器、报告协议和原生 OVMF runner，不接触 PR #1612 正在修改的 AxVisor VM 配置、设备管理和 Guest 启动接口，因此可以独立评审和合入。

第四层 AxVisor 集成必须基于 PR #1612 合入后的实际接口重新确认配置路径，不在第一阶段提前适配审阅中的内部结构。

### 13.2 Guest UEFI 支持

`probe.efi` 为 ACPI、PCI、Block IO 和 Runtime Services 提供阶段性验收载荷。它不会自动实现这些能力，也不能替代 Linux EFI stub、NimbOS 或 ArceOS 的端到端启动测试。

### 13.3 设备中断重构

probe 第一阶段不产生设备 I/O，不建立 VirtIO queue，也不等待设备中断，因此不依赖设备中断架构重构。后续若要验证 INTx、MSI/MSI-X、EOI 或跨 vCPU 中断，应该新增专用 Guest workload，而不是扩大 probe 的只读职责。

## 14. 实施顺序与退出条件

### 阶段 A：协议和 host 核心

- 完成 profile、结果模型、报告协议和 host 测试；
- 完成 ACPI 有界解析器和损坏 fixture；
- 退出条件：所有纯逻辑错误路径可在 host 确定性复现。

### 阶段 B：UEFI adapter

- 完成 EFI entry、System Table、Memory Map、ACPI、PCI、Block IO 和 Runtime adapter；
- 完成 UEFI target clippy/build 和 PE/COFF 检查；
- 退出条件：产物是合法 EFI application，生产路径无裸 `unwrap()`，所有 `unsafe` 有明确契约。

### 阶段 C：OVMF/QEMU 闭环

- 完成 xtask runner、正向矩阵和故意失败用例；
- 退出条件：一条 xtask 命令能够稳定判断 probe 的正向与负向结果，失败时提供完整诊断。

### 阶段 D：后续 AxVisor 集成

- PR #1612 合入后另开 issue；
- 退出条件：相同 probe 产物在 AxVisor Guest 中通过选定 profile，且 test-suit 不依赖普通调试日志。

## 15. 完成标准

- probe crate、输出协议和 profile 边界有文档；
- 默认路径只读，副作用检查必须显式启用并完整清理；
- host 测试覆盖合法输入、错误路径、边界和输出注入；
- UEFI target 通过格式、clippy、build 和 PE/COFF 检查；
- 原生 OVMF/QEMU 正向和负向用例均可由 xtask 自动判断；
- required/optional 语义不会把“存在但损坏”的能力降级为 SKIP；
- 输出结果不依赖地址、时间、枚举顺序或 OVMF 私有值；
- 第一阶段不依赖 PR #1612，也不修改 AxVisor 内部接口；
- 后续 AxVisor 集成使用同一份 probe 和协议，不增加平台特判。

## 16. 参考资料

- [UEFI Specification 2.11](https://uefi.org/specs/UEFI/2.11/)
- [UEFI System Table](https://uefi.org/specs/UEFI/2.11/04_EFI_System_Table.html)
- [UEFI Protocols — Media Access](https://uefi.org/specs/UEFI/2.11/13_Protocols_Media_Access.html)
- [UEFI Protocols — PCI Bus Support](https://uefi.org/specs/UEFI/2.11/14_Protocols_PCI_Bus_Support.html)
- [ACPI Specification 6.6 — Software Programming Model](https://uefi.org/specs/ACPI/6.6/05_ACPI_Software_Programming_Model.html)
- [`bootloader/axloader`](../bootloader/axloader/README.md)
- [`plan/design_uefi.md`](./design_uefi.md)
