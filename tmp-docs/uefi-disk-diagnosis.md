# x86 UEFI 磁盘启动排查记录

本文记录实验分支中通过 VirtIO PCI 磁盘和 OVMF 启动 x86_64 guest 的排查过程。当前目标是判断启动停滞发生在 PCI 配置访问、ACPI PM timer、中断投递还是 UEFI 磁盘路径本身，并为后续实验保留可复核的命令和证据。

## 1. 实验边界

### 1.1 Guest 启动链路

`test-suit/axvisor/experimental/qemu-uefi-disk/x86-linux-uefi-disk.toml` 配置一个单 vCPU 的 x86_64 VM。该 VM 使用 `boot_protocol = "uefi"` 和 `boot_source = "pci-disk"`，由 OVMF 从 VirtIO PCI 磁盘上的 GPT/FAT EFI System Partition 加载启动文件；Axvisor 自身仍由外层 QEMU 启动。

关键配置关系如下：

| 配置对象 | 当前值 | 作用 |
| --- | --- | --- |
| `uefi_firmware_path` | `OVMF_CODE_4M.fd` | guest 的 UEFI 固件代码 |
| `bios_load_addr` | `0xffc0_0000` | 将 UEFI 固件放入 guest 高地址内存 |
| `boot_source` | `pci-disk` | 不通过 `fw_cfg` 或直加载内核，要求固件访问 PCI 磁盘 |
| `image_path` | `/uefi/uefi-guest.img` | Axvisor 启动时从自身可见文件系统读取磁盘镜像到 RamDiskBackend |
| VirtIO BDF | `00:01.0` | 当前 PCI 拓扑中为 VirtIO block endpoint |
| VirtIO BAR0 | `0xc000_0000..0xc000_1000` | endpoint 的 PCI memory BAR |

`X86PciHostModel::requirements()` 在 `virtualization/axvm/src/arch/x86_64/pci_config.rs` 中只声明 `0xcf8..0xcff` 的 PIO 配置窗口和 `0xc000_0000..0xd000_0000` 的 PCI memory aperture。当前 x86 guest PCI host 没有 ECAM provider，也没有对应的 `MCFG` 表。

### 1.2 ACPI 与 PM timer

直接加载的 ACPI 镜像由 `build_direct_image()` 构造，位置由 `DIRECT_ACPI_BASE = 0x000e_0000` 控制。`build_fadt()` 将 `plan.power.pm_timer.port` 同时写入 FADT 的传统 PM timer 字段和 extended GAS 字段；当前测试计划中的端口是 `0x608`，长度为 4 字节。运行时设备由 `X86AcpiPmTimerDevice` 提供。

因此，当前实验中应区分两个概念：

- `0x608` 是 Axvisor 明确提供的 ACPI PM timer 端口。
- `0x8` 不是 PM timer 的绝对端口；它位于传统 PCI 配置端口范围之外，也没有被 Axvisor 的 PIO 设备映射。

## 2. 诊断观测点

### 2.1 PCI 配置访问

诊断代码位于 `X86PciConfigFrontend` 和 `PciMemoryApertureDevice`。前者统计 CF8 地址端口写入、CFC 数据端口读写，并输出解码后的 BDF 和 register；后者统计 PCI memory aperture 的读写次数。这样可以区分“guest 选择了 CF8/CFC 但读到 absent function”和“guest 使用了 ECAM 或 BAR MMIO”。

当前日志中的 `x86 PCI topology function` 已确认 Axvisor 内部拓扑包含三项：Q35 host `00:00.0`、VirtIO block `00:01.0` 和 LPC `00:1f.0`。拓扑存在不代表 guest 已经枚举到 endpoint，后者必须由运行时配置访问或 BAR 访问证明。

### 2.2 Guest 指令与停滞状态

SVM/VCPU 诊断统计 guest 的端口 I/O、`PAUSE`、guest RIP、控制寄存器和被拦截指令附近的 guest 字节。`handle_io_read()` 对端口 `0x8` 额外记录读出的值和是否命中设备。`guest run progress` 用来判断 VCPU 线程是否仍在运行，而 `vLAPIC accepted interrupt` 和 PIT/PIC 统计用来判断中断路径是否完全停止。

当前实验不是只看一条 `unimplemented` 日志，而是同时观察以下状态：

| 观测项 | 判断意义 |
| --- | --- |
| CF8/CFC BDF 与寄存器 | guest 是否在使用传统 PCI 配置机制、是否访问 VirtIO endpoint |
| PCI aperture 读写计数 | guest 是否开始访问 VirtIO BAR |
| PM timer 计数器变化 | `0x608` 是否按约 3.579545 MHz 单调前进 |
| 未映射端口与 guest RIP | 是否存在错误端口或其他固件 I/O 自旋 |
| `PAUSE` 与 guest run progress | 是否是 guest 自旋，而不是 VMM 线程完全卡死 |
| PIC/vLAPIC 统计 | 中断是否仍能投递并被 guest 接收 |

## 3. 实验结果

### 3.1 UEFI 磁盘用例

执行命令如下。外层 `timeout` 仅用于避免本地 WSL/KVM 实验无限运行；测试配置自身的 QEMU 超时仍为 1800 秒。

```text
set -o pipefail
timeout --signal=INT 75s cargo xtask axvisor test qemu --arch x86_64 --test-group experimental --test-case uefi-disk-vmx 2>&1 | tee /tmp/uefi-disk-pci-path.log
```

该命令最终以退出码 124 结束，表示触发了外层 75 秒限制，不是 Axvisor 测试用例主动报告成功或失败。关键观测如下：

1. guest 对 `0x608` 发起了 100000 次 dword 读取。日志中的 `counter_delta` 与 host elapsed time 对应，`expected_counter_ticks` 基本一致，`zero_deltas=0` 且 `host_backwards=0`。这证明 Axvisor PM timer 本身在这次运行中持续前进，没有复现“PM timer 恒定为零”或时间倒退。
2. 之后首次出现 `guest diagnostic port read: ... port=0x8 ... value=0xffffffff mapped=false`。后续端口 I/O 统计持续把 `0x8` 归为 `other`，读值一直是全 1。
3. 该读取位于 guest RIP `0x1e9b1e98` 附近；随后 guest 在 `0x1e9b1ea4` 执行 `PAUSE` 并回跳。`guest run progress`、端口 I/O 计数和 `PAUSE` 计数都继续增加，因此这是 guest 在运行中的忙等循环，不是 VMM 调度线程停止。
4. PIT IRQ 仍然被 PIC claim，并且 PIT 中断继续被 vLAPIC 接收。因而当前证据不能把停滞归因于 `vpic wire mode change to LAPIC, unimplemented` 导致的全局中断失效。
5. 这次停滞阶段没有观测到 VirtIO endpoint `00:01.0` 的 CF8/CFC 配置访问，也没有 PCI memory aperture 访问；观测到的配置访问集中在 `00:00.0` 和 `00:1f.0`。

这一结果说明 guest 在完成 PM timer 相关操作后、完成 VirtIO PCI endpoint 枚举前，就进入了另一个读取端口 `0x8` 的循环。由于该端口没有映射，返回全 1 会使循环条件无法满足。

### 3.2 运行时 ACPI 快照

为区分“Axvisor 构造的 ACPI 表错误”和“guest 启动代码选择了其他端口”，在首次 `0x8` 读取时增加了一次性 guest 内存快照。第一次实现把 XSDT 总长度错误地判断为 8 的倍数，导致合法的 `0x3c` 被拒绝；修正为检查 `length - 36` 是否为 8 的倍数后，快照成功。

修正后的实验命令如下：

```text
set -o pipefail
timeout --signal=INT 75s cargo xtask axvisor test qemu --arch x86_64 --test-group experimental --test-case uefi-disk-svm 2>&1 | tee /tmp/uefi-disk-acpi-snapshot-fixed-75s.log
```

首次 `0x8` 读取时的运行时快照为：

| 表或字段 | 运行时结果 |
| --- | --- |
| RSDP | GPA `0xe0000`，revision 2，XSDT GPA `0xe0028` |
| XSDT | 长度 `0x3c`，即 36 字节 header 加 3 个 8 字节表项 |
| XSDT 表项 | `FACP` at `0xe0068`，`APIC` at `0xe0350`，`SPCR` at `0xe0390` |
| FADT legacy PM timer | `pm_tmr_blk = 0x608` |
| FADT extended PM timer | `x_pm_tmr_blk = 0x608` |
| FADT PM timer length | `4` |

快照发生时，guest 仍然在 `RIP=0x1e9b1e98` 读取端口 `0x8`，返回值为 `0xffffffff`；随后 `PAUSE` 循环继续运行。与此同时，统计为 `pm_timer=100000`、`pci_config=126`、`other=30946`，没有出现 VirtIO `00:01.0` 的配置访问或 PCI memory aperture 访问。

这组结果证明 direct ACPI image 已经在 guest 预期地址存在，且其中 FADT 的两个 PM timer 字段都正确指向 `0x608`。它不能证明 UEFI 固件一定使用了这份表，但至少可以排除“Axvisor direct FADT 将 PM timer 发布为 `0x8`”这一解释；`0x8` 更可能来自启动代码的另一条探测路径、运行时加载内容与当前磁盘文件不一致，或相关变量被错误解析。

同一份日志还记录了 `0x8` 循环前的 Q35 LPC 配置读取：`00:1f.0` 的 register `0x40` 返回 `0x601`，register `0x44` 返回 `0x80`。这意味着 LPC 的 PMBASE 是 `0x600`，并且 ACPI I/O 空间已经使能；不能再把“LPC 没有发布 PMBASE”作为当前根因。OVMF 的 `AcpiTimerLib` 在配置了 PCI BAR 寄存器时会读取 LPC BAR、应用地址掩码并加上 PM timer 偏移，因此这组值对应的计时器端口也是 `0x608`。参考：[EDK2 BaseAcpiTimerLib.c](https://github.com/tianocore/edk2/blob/master/OvmfPkg/Library/AcpiTimerLib/BaseAcpiTimerLib.c)。

### 3.3 普通 OVMF/ACPI 对照用例

对照用例为仓库已有的 `normal/qemu-acpi-ovmf`，它同样使用 OVMF、x86_64、单 vCPU guest 和 Axvisor 的 ACPI/PCI 基础设备，但通过 initramfs 直达 Linux，不依赖 VirtIO PCI 磁盘作为 UEFI 启动盘。实际执行的 SVM 用例为：

```text
set -o pipefail
timeout --signal=INT 90s cargo xtask axvisor test qemu --arch x86_64 --test-group normal --test-case ovmf-acpi-svm 2>&1 | tee /tmp/qemu-acpi-ovmf-diagnostic.log
```

本地运行同样因为外层 90 秒限制以退出码 124 结束，不能据此宣称对照测试通过；不过它给出了有区分度的运行轨迹：

- guest 使用 `0x608` PM timer 读取，并且 PM timer 统计持续前进；没有出现 UEFI 磁盘用例中的 `0x8` 诊断读取。
- guest 后续扫描了多个 BDF，并实际访问了 `00:01.0`；该 BDF 在该用例中是 absent function，读取返回 `0xffffffff`，证明当前 CF8/CFC frontend 能正确表达传统 PCI 的 absent-function 语义。
- guest 能继续进入 Linux。Linux 日志中有 `HPET/PMTIMER calibration failed`，这是对照实验中仍需单独分析的计时源问题，但不能反推 Axvisor 的 `0x608` 设备在本次运行中不工作。

这组对照支持一个重要区分：传统 CF8/CFC 访问路径不是完全不可用；UEFI 磁盘用例的首要异常更像是磁盘启动路径中、PCI endpoint 枚举之前的固件计时或端口选择问题。

### 3.4 端口 `0x8` 的有界短跟踪

为确认 `0x8` 访问是否只是一次错误探测，SVM 和 VMX 后端都增加了一个有界跟踪：首次看到 guest 从端口 `0x8` 读入后，记录随后最多 64 次端口 I/O，并保留 guest RIP、通用寄存器和当前指令附近的字节。该实验使用 SVM：

```text
set -o pipefail
timeout --signal=INT 75s cargo xtask axvisor test qemu --arch x86_64 --test-group experimental --test-case uefi-disk-svm 2>&1 | tee /tmp/uefi-disk-io-trace-75s.log
```

命令在外层 75 秒限制处退出；guest 在约 5.8 秒开始运行，并在约 31.2 秒进入 `0x8` 跟踪。关键结果如下：

- 跟踪中的端口访问几乎全部是 `port=0x8 direction=in width=Dword`，guest RIP 始终为 `0x1e9b1e98`，`RAX` 始终为 `0xffffffff`，`RDX` 始终为 `0x8`；期间夹杂少量对端口 `0x20` 的 PIC 操作。
- RIP 附近的 guest 字节为 `[0xed, 0x44, 0x89, 0xca, 0x29, 0xc2, 0x0f, 0xba, 0xe2, 0x17, 0x72, 0x04, 0xf3, 0x90, 0xeb, 0xea]`，即以 `IN EAX,DX` 开始，随后比较结果并进入 `PAUSE`/回跳循环。
- 相邻的 `PAUSE` 指令位于 `0x1e9b1ea4`，其字节为 `[0xf3, 0x90, 0xeb, 0xea, ...]`。`PAUSE`、guest exit、guest run progress 和 vLAPIC interrupt accepted 计数都持续增加。
- 统计在 `total=131072` 时为 `pm_timer=100000`、`pci_config=126`、`other=30946`，其中 `other` 的代表访问仍是 `0x8` 输入，`pause_exits=28104`。这说明 VMM 没有死锁，而是不断处理 guest 的端口 I/O 和忙等循环。

该实验把“`0x8` 只是一次失败探测”排除为主要解释：它是一个稳定重复的、依赖全 1 读值的等待循环。由于这段运行时字节序列没有在已提取的 `BOOTX64.EFI` 主 `.text` 中找到，当前只能将代码来源表述为“OVMF 或启动加载器的运行时映像”；还不能据此确认它具体属于 OVMF、GRUB 主映像还是 GRUB 模块。

### 3.5 RIP-relative 端口变量观测

在上一轮观察到 RIP-relative 内存读取后，增加了有界解码：当端口 `0x8` 的读次数为 1、2、4… 时，若当前指令字节中出现 `8b 15 disp32`，就计算其目标地址，并通过 guest 页表读取目标 dword。实际执行命令为：

```text
set -o pipefail
timeout --signal=INT 75s cargo xtask axvisor test qemu --arch x86_64 --test-group experimental --test-case uefi-disk-svm 2>&1 | tee /tmp/uefi-disk-rip-relative-75s.log
```

本轮仍在约 30.8 秒进入相同循环，但首次采样提供了新的直接证据：

```text
rip=0x1e9b1e8d
guest_gpa=0x1e9b1e8d
rip_relative_dword=(target=0x1e9f1fa8, gpa=0x1e9f1fa8, value=0x8)
rdx=0x8
```

这里的 `8b 15 10 01 04 00` 位于首次采样的指令字节中；下一条指令地址为 `0x1e9b1e98`，加上有符号位移 `0x40110` 正好得到 `0x1e9f1fa8`。因此可以确认：guest 运行时内存中的这个 dword 实际值是 `0x8`，随后被加载到 `EDX`，并用于 `IN EAX,DX`。同一采样中 `RDI=0x1eaf09a8`，所以不能把端口值简单归因于 `RDI` 参数。

代码页和端口变量页都被映射为相同的 GPA（当前页表表现为 identity mapping），后续采样继续看到 `RIP=0x1e9b1e98`、`RDX=0x8`、读值 `0xffffffff` 和 `PAUSE` 循环。PM timer 计数仍为 `100000`，PIT/PIC/vLAPIC 统计仍在增长。由此，当前问题已经从“VMM 把合法端口解析成了 `0x8`”收窄为“某个 guest 运行时映像/数据结构把待访问的端口变量初始化成了 `0x8`，而这个值没有被转换为实际 PM timer 基址 `0x608`”。

### 3.6 同一镜像直接使用 QEMU+OVMF 对照

为区分磁盘内容/OVMF 问题和 Axvisor 虚拟硬件交互问题，将同一个 `/tmp/uefi-guest.img`、同一份 `OVMF_CODE_4M.fd` 和同一份 VARS 交给系统 QEMU。宿主没有 `/dev/kvm`，因此该对照使用 QEMU TCG：

```text
set -o pipefail
timeout --signal=INT 45s qemu-system-x86_64 \
  -machine q35 -cpu max -m 512M -smp 1 -nodefaults -nographic \
  -serial mon:stdio \
  -drive if=pflash,format=raw,readonly=on,file=OVMF_CODE_4M.fd \
  -drive if=pflash,format=raw,file=/tmp/uefi-direct-OVMF_VARS_4M.fd \
  -drive id=uefidisk,if=none,format=raw,file=/tmp/uefi-guest.img,readonly=on \
  -device virtio-blk-pci,drive=uefidisk \
  2>&1 | tee /tmp/uefi-direct-qemu.log
```

该对照在约 2.5 秒进入 guest shell，期间 Linux 输出了 `ACPI: PM-Timer IO Port: 0x608`，识别到 `virtio_blk` 和 `/dev/vda`，并继续执行 `/init`。同一日志还显示标准 QEMU 发布了 `MCFG`/ECAM，并给 VirtIO block 分配了 `00:01.0`、BAR0 I/O `0x6000` 和 BAR1 MMIO `0xc0001000`；这与 Axvisor 当前仅提供 CF8/CFC、且 VirtIO BAR 布局不同。

这个结果证明磁盘镜像、GPT/FAT、`BOOTX64.EFI`、GRUB 配置、内核和 initramfs 在标准 QEMU+OVMF 路径上可以完整启动；当前 Axvisor 中的 `0x8` 循环是 Axvisor 虚拟硬件交互触发的分支，不能归结为镜像本身损坏。由于该对照同时包含标准 QEMU 的 MCFG/ECAM、PCI BAR 布局和完整 Q35 设备集合，它还不能单独区分这些差异中的哪一项触发了 Axvisor 分支；后续需要做去掉 ACPI/MCFG 的标准 QEMU 对照，并继续在 Axvisor 中定位端口变量的初始化路径。

### 3.7 CPUID、RDTSC 与运行时 PE 签名观测

为了判断 `0x8` 循环是否由 CPU 特性或计时器观测差异触发，又增加了有界的 SVM `CPUID`/`RDTSC` 日志，并在首次诊断采样时从当前 RIP 向低地址扫描最多 4 MiB 的页边界，查找 `MZ/PE` 和 `ELF` 签名。实际执行命令为：

```text
set -o pipefail
timeout --signal=INT 75s cargo xtask axvisor test qemu --arch x86_64 --test-group experimental --test-case uefi-disk-svm 2>&1 | tee /tmp/uefi-disk-image-signature-75s.log
```

本轮以外层 `timeout` 结束；诊断日志给出了以下结果：

- 本轮日志中可见的 CPUID 样本从 `count=41` 开始。`leaf=0` 返回最大基本 leaf `0xd` 和 AMD 厂商字符串；`leaf=1` 返回 `EAX=0x00a70f41`、`EBX=0x10800`、`ECX=0xfef83203`、`EDX=0x178bfb7f`。这些值是 SVM 后端基于宿主 CPUID 再经过 Axvisor 过滤后的结果，和直接 QEMU 使用的 `-cpu max` 并非同一 CPU 模型；目前只能把它作为候选差异，不能仅凭这组值确定根因。
- 观测到一次 SVM `RDTSC`：`host_tsc=0x1a6719ec9d`、`offset=0`、`guest_tsc=0x1a6719ec9d`。至少在该采样点没有发现 Axvisor 施加了异常的 TSC offset；这不能排除 guest 使用不同 TSC 频率或其他计时源。
- 首次稳定的 `0x8` 采样仍位于 `RIP=0x1e9b1e98`，`RSP=0x1ff39a00`，guest GPA 与 RIP 相同；`RDX=0x8`、读值为 `0xffffffff`，`PAUSE` 循环继续重复。运行时寄存器为 `CR0=0x80010033`、`CR3=0x1fc01000`、`EFER=0x1d00`，代码段 base 为 0。
- 从 `RIP` 向前扫描发现一个有效的 PE 签名，基址为 `0x1e956000`；当前 RIP 与该基址的距离为 `0x5be98`。随后直接读取运行时 PE 的 section 表，确认该地址属于第 0 个 `.text` section：section RVA 起点为 `0x240`，覆盖范围为 `0x80500`，属性为 `0x60000020`（代码、可读、可执行）。因此当前 RIP 确实位于这个运行时 PE 的可执行代码中；没有发现 ELF 签名。
- 这个运行时 PE 的 section 布局与磁盘里的 `BOOTX64.EFI` 不同：后者的 `.text` 从 RVA `0x1000` 开始，大小为 `0xc000`，而当前运行时 PE 的 `.text` 从 `0x240` 开始，大小为 `0x80500`。因此当前循环不能再简单归类为 `BOOTX64.EFI` 主映像代码，更像是 UEFI 运行期间加载的另一个 PE/COFF 模块；当前日志还没有足够信息把它精确命名为某个 OVMF DXE 驱动。
- 进入循环时累计统计为 `pm_timer=100000`、`pci_config=126`，没有 VirtIO `00:01.0` 的配置访问，也没有 PCI memory aperture 访问；`guest run progress` 从 `1024` 增长到 `32768`，PIT/PIC 和 vLAPIC 的计数仍增长。`vpic wire mode change to LAPIC, unimplemented` 出现在约 `3.269 s`，而 `0x8` 循环约在 `29.7 s` 才出现，时间上也不支持把这条日志本身视为 VCPU 停止的直接原因。

因此，本轮没有发现“PM timer 不递增”或“RDTSC 被错误偏移”这两类明显故障；也进一步确认了 guest 仍在运行，只是卡在一个由未映射端口 `0x8` 驱动的忙等循环中。当前已确认循环位于运行时 PE 的可执行代码，下一步应识别该 PE/UEFI 模块，并把它在 Axvisor 中的 PCI/计时路径与标准 QEMU 的对应路径做对照。

### 3.8 运行时 PE 元数据复核

上一轮最初把 PE section 的最小对齐限制为 `0x200`，导致运行时 section 表没有通过诊断校验。随后将校验改为接受 PE/COFF 允许的、且 section/file alignment 一致的较小对齐值，并重新执行：

```text
set -o pipefail
timeout --signal=INT 120s cargo xtask axvisor test qemu --arch x86_64 --test-group experimental --test-case uefi-disk-svm > /tmp/uefi-disk-pe-stack-120s.log 2>&1
```

该轮日志中运行时映像的元数据为：

| 字段 | 结果 |
| --- | --- |
| PE 基址 / image base | `0x1e956000` / `0x1e956000` |
| machine / optional magic | `0x8664` / `0x20b` |
| section 数量 | `3` |
| section/file alignment | `0x40` / `0x40` |
| size of image / headers | `0x9f7c0` / `0x240` |
| entry RVA | `0x63b54` |
| subsystem | `11`（EFI boot-service driver） |
| 当前 `.text` | RVA `0x240`，大小 `0x80500`，属性 `0x60000020` |

这组字段在 machine、PE32+ magic、section 表、image size、image base 和 `.text` 范围之间是自洽的，因此当前 `MZ/PE` 命中不是随机数据，也不是只凭签名误判；`0x1e9b1e98` 确实落在一个已加载的 x86-64 EFI boot-service driver 的可执行 section 中。它的布局与磁盘 `BOOTX64.EFI`（`.text` RVA `0x1000`、大小 `0xc000`、subsystem 10）不同，所以当前循环来自另一个运行时 PE/COFF 映像，但尚未能从内存元数据直接得到模块名称。日志中的栈采样没有形成可复核的调用者地址，不能据此继续推断调用链。

### 3.9 标准 QEMU 去掉 ACPI/MCFG 的对照

为了单独验证“没有 MCFG/ECAM 是否会使 OVMF 无法访问 PCI 磁盘”，使用同一份 OVMF code、同一份变量文件和同一份磁盘镜像，但关闭整套 ACPI：

```text
set -o pipefail
timeout --signal=INT 30s qemu-system-x86_64 \
  -machine q35,acpi=off -cpu max -m 512M -smp 1 -nodefaults -nographic \
  -serial mon:stdio \
  -drive if=pflash,format=raw,readonly=on,file=test-suit/axvisor/experimental/qemu-uefi-disk/assets/OVMF_CODE_4M.fd \
  -drive if=pflash,format=raw,file=/tmp/uefi-direct-OVMF_VARS_4M.fd \
  -drive id=uefidisk,if=none,format=raw,file=/tmp/uefi-guest.img,readonly=on \
  -device virtio-blk-pci,drive=uefidisk \
  2>&1 | tee /tmp/uefi-direct-qemu-no-acpi.log
```

虽然外层 `timeout` 最终以 `124` 结束，但这只是因为 guest shell 仍在运行；在此前已经出现了完整的启动证据：

- OVMF 输出 `BdsDxe: loading Boot0001 ... from PciRoot(0x0)/Pci(0x1,0x0)` 并启动该设备；
- Linux 输出 `ACPI: OSL: System description tables not found`，随后输出 `PCI: Using configuration type 1 for base access`；
- Linux 发现 `0000:00:01.0: [1af4:1001]`，读取到 BAR0 I/O `0x6000-0x607f` 和 BAR1 MMIO `0xc0001000-0xc0001fff`；
- `virtio_blk` 成功注册并报告 `/dev/vda`，随后执行 `/init`。

因此，在这套 OVMF 构建中，缺少 ACPI MCFG/ECAM 并不妨碍 OVMF 通过传统 PCI configuration type 1（CF8/CFC）枚举和启动 VirtIO 磁盘。EDK2 源码链路是 `PciHostBridgeDxe -> PciSegmentLib -> PciLib -> PciCf8Lib`；`PciExpressLib` 是独立的 MMIO library，并不是这条 DXE root-bridge 配置访问路径的运行时 fallback。这个对照把“Axvisor 没有 ECAM”从首要嫌疑中移除，但还不能排除 Axvisor CF8/CFC 的具体返回值、PCI BAR/command 初始化或其他 Q35 设备差异。

### 3.10 标准 QEMU 无 ACPI/MCFG 的 modern-only VirtIO 对照

由于 Axvisor 暴露的是 VirtIO modern 设备 `1af4:1042`（revision 1），而标准 QEMU 默认使用 transitional 设备 `1af4:1001`，继续使用无 ACPI/MCFG 的环境执行了 modern-only 对照：

```text
set -o pipefail
timeout --signal=INT 30s qemu-system-x86_64 \
  -machine q35,acpi=off -cpu max -m 512M -smp 1 -nodefaults -nographic \
  -serial mon:stdio \
  -drive if=pflash,format=raw,readonly=on,file=test-suit/axvisor/experimental/qemu-uefi-disk/assets/OVMF_CODE_4M.fd \
  -drive if=pflash,format=raw,file=/tmp/uefi-direct-OVMF_VARS_4M.fd \
  -drive id=uefidisk,if=none,format=raw,file=/tmp/uefi-guest.img,readonly=on \
  -device virtio-blk-pci,drive=uefidisk,disable-legacy=on \
  2>&1 | tee /tmp/uefi-direct-qemu-modern-no-acpi.log
```

结果仍然是外层 `timeout` 因 guest shell 持续运行而返回 `124`，但启动链路已经完成：

- OVMF 从 `PciRoot(0x0)/Pci(0x1,0x0)` 加载并启动 UEFI 启动项；
- Linux 在没有 ACPI/MCFG 的情况下仍报告 `PCI: Using configuration type 1 for base access`；
- Linux 发现 `00:01.0 [1af4:1042]`，并读取到 BAR1 MMIO `0xc0001000-0xc0001fff`、BAR4 64-bit prefetch；
- `virtio_blk` 注册 `/dev/vda`，容量为 256 MiB，并完成 `/init`。

这说明当前 OVMF 的 `Virtio10Dxe` 可以在传统 PCI configuration type 1 下处理 modern-only VirtIO；问题不能简化为“Axvisor 使用了 `0x1042` 而 OVMF 只支持 transitional `0x1001`”。Axvisor 与标准 QEMU 仍存在 BAR 编号、capability 布局和 command 初始化等设备模型差异，这些需要单独比较。

### 3.11 临时将端口 `0x8` 别名到 PM timer 的因果实验

为了区分“端口 `0x8` 返回全 1 是停滞的直接原因”和“端口 `0x8` 只是恰好出现在停滞位置”，临时修改 `handle_io_read()`：仅当 guest 读取原始端口 `0x8` 时，向设备总线查询 `0x608`，但日志仍保留原始端口号。该修改只用于一次实验，随后已撤销。执行命令为：

```text
set -o pipefail
timeout --signal=INT 120s cargo xtask axvisor test qemu --arch x86_64 --test-group experimental --test-case uefi-disk-svm 2>&1 | tee /tmp/uefi-disk-port8-alias-120s.log
```

结果如下：

- 原来位于 `0x1e9b1e8a/0x1e9b1e8d` 的循环不再读取 `0xffffffff`；日志中的端口读取变为 `mapped=true`，返回值随 `0x608` PM timer 单调前进。这直接证明未映射端口返回全 1 会影响该循环的退出条件。
- 循环退出后，guest RIP 移动到 `0x1f45625a`。这个地址属于另一个运行时 PE 的 `.text` section，其指令仍然是 `in eax, dx`、按位测试和 `PAUSE` 回跳，也仍然使用 `RDX=0x8`。因此端口 `0x8` 至少还被另一条计时/等待路径使用；别名实验只是让它也能继续推进，并没有修正 guest 对端口的配置。
- 之后 guest 在 `0x1f46e560` 触发 `HLT`，`guest run progress` 仍从 `32768` 增长到 `131072`，说明 VCPU 线程没有死锁。但 120 秒内仍没有出现 VirtIO endpoint `00:01.0` 的配置访问、PCI BAR/MMIO 访问或 Linux 启动输出。

因此，这个实验把结论推进了一步：`0x8` 是当前停滞的真实输入依赖，`vpic wire mode change to LAPIC, unimplemented` 不是原始忙等循环的直接阻塞点；但“把 `0x8` 直接映射到 `0x608`”不是可接受的修复，因为 guest 仍有后续端口 `0x8` 路径，最终也没有进入 VirtIO 枚举。当前应继续定位运行时 PE 的模块身份和端口变量初始化来源，而不是保留该别名。

### 3.12 Axvisor 直接 Linux PCI 枚举对照

为判断端口 `0x21` 忙等是否只属于 VirtIO block 路径，运行已有的 `pci-enumeration-svm`。该用例使用直接加载的 Linux、独立的 `vpci-test0` 设备和 `X86PciConfigFrontend`，不加载 VirtIO block 驱动；因此它可以观察 Axvisor 的公共 PCI 枚举和中断初始化路径，但不能直接验证 UEFI 磁盘路径。

```text
set -o pipefail
timeout --signal=INT 90s cargo xtask axvisor test qemu --arch x86_64 --test-group normal --test-case pci-enumeration-svm 2>&1 | tee /tmp/axvisor-pci-enumeration-svm-90s.log
```

本轮以外层 90 秒限制退出，未产生测试成功标记。Axvisor 启动时确认 `vpci-test0` 位于 `00:01.0`，其 BAR2 为 `0xc0000000..0xc0010000`；Linux 早期确实通过 `CF8/CFC` 扫描了 Q35 host、LPC 和多个 absent function，但配置访问计数停在 `pci_config=152`，没有产生 `00:01.0` 的配置访问，也没有 PCI aperture 访问。

Linux 随后打印 `APIC: Switch to symmetric I/O mode setup`。从约 6 秒开始，SVM 统计固定显示 `port=0x21 direction=in`，guest RIP 为 `0xffffffff8483bf83`；`guest run progress`、SVM guest exit 和 vLAPIC accepted interrupt 计数持续增加，说明 VCPU 不是死锁。与此同时，PIT 统计显示第一次 IRQ0 被注入为向量 `0x30`，后续采样中 PIC 请求多次处于不可再次 claim 的状态，尝试结果主要落入 `no_route`；这说明当前直接 Linux 对照已经进入 PIC/APIC 处理路径，但仅凭这些聚合计数还不能判断是 guest 屏蔽/EOI 状态还是 vPIC 语义问题。

这一结果只能证明“直接 Linux 用例也在完成 PCI endpoint 探测之前进入了相同的中断/端口循环”，不能证明 `X86PciConfigFrontend` 无法访问 `00:01.0`。它也不能把问题归因于 `vpic wire mode change to LAPIC, unimplemented`：该日志在本轮直接 Linux 日志中没有出现，且当前循环发生在 Linux 的 APIC 切换之后。后续若要继续分析这条公共阻塞，需要记录 `0x21` 读取处的指令字节、RFLAGS.IF、PIC mask/ISR，以及 guest 是否执行了对应的 EOI；在此之前，不应把该对照当作 VirtIO/ECAM 结论。

### 3.13 直接 Linux 的 PIC I/O 一次性指令跟踪

为验证上一节中 `0x21` 循环的具体语义，在 SVM 后端增加了一个每个 vCPU 只触发一次、最多记录 16 个端口 I/O 的跟踪。它在第一次 `IN` 访问 PIC master data port `0x21` 后启动，因此不会把长时间循环打印成无界日志。执行命令如下：

```text
set -o pipefail
timeout --signal=INT 60s cargo xtask axvisor test qemu --arch x86_64 --test-group normal --test-case pci-enumeration-svm 2>&1 | tee /tmp/axvisor-pci-enumeration-pic-trace-clean-60s.log
```

本轮的 16 条跟踪记录只出现一次，随后仍由外层 60 秒限制结束；没有测试成功标记，也没有残留的 Axvisor/QEMU 进程。最关键的记录如下：

| guest RIP 附近字节 | 端口与操作 | 观测值 | 解释 |
| --- | --- | --- | --- |
| `e4 21 ...` | `IN AL,0x21` | `RFLAGS=0x46`，`IF=false` | 读取 8259 master PIC 的中断屏蔽寄存器 |
| `e6 21` | `OUT 0x21,AL` | `RFLAGS=0x46/0x96`，`IF=false` | 写 master PIC 的中断屏蔽/初始化数据 |
| `e6 20` | `OUT 0x20,AL` | `RAX=0x11` 或 `0x60` | 向 master PIC 写初始化命令或 EOI/命令值 |
| `e6 a0`、`e6 a1` | `OUT 0xa0/0xa1,AL` | `RFLAGS=0x96`，`IF=false` | 操作 slave PIC 的命令、屏蔽或初始化数据 |

完整跟踪还显示了连续的 `0x20/0x21` 与 `0xa0/0xa1` 访问：例如 `OUT 0x20,AL` 前的字节为 `e6 20`，其后接着执行对 `0x21` 的写入；随后又出现 `OUT 0xa0,AL`、多次 `OUT 0xa1,AL`，以及从 `0x21` 读取后更新屏蔽值的代码。跟踪末尾的 `0x70/0x71` 是 RTC 端口访问，说明这是从 PIC 初始化入口开始截取的端口序列，而不是单一端口的纯软件循环。

本轮同时得到以下运行时状态：

- 第一次 PIT IRQ0 成功被 PIC claim 为向量 `0x30`，对应 `mask=0xfe`、`request=0`、`in_service=1`；随后 guest 将 mask 变为 `0xff` 或保持 IRQ0 in-service，后续 `claim` 不成功。这个现象与 guest 的 PIC 屏蔽/EOI 状态一致，单凭 `no_route` 计数不能断言 vPIC 实现错误。
- guest 的 `RFLAGS.IF` 在上述 PIC I/O 点均为关闭状态；这符合 Linux 在关中断临界区中初始化或修改 8259 PIC 的行为，不能把 `IF=false` 单独解释为 VCPU 卡死。
- 本轮仍为 `pm_timer=0`、`pci_config=152`，没有 `00:01.0` endpoint 配置访问，也没有 PCI aperture 访问；`guest run progress`、SVM exit、vLAPIC accepted interrupt 计数继续增长。
- 日志中没有 `vpic wire mode change to LAPIC, unimplemented`。因此当前直接 Linux 对照的可复核结论是：guest 正在执行 8259 PIC/APIC 切换相关代码，VCPU 仍在运行；这不是 PM timer 不递增的证据，也不是 `unimplemented` 日志本身导致停死的证据。

这轮跟踪把上一节的“PIC/APIC 路径污染”从统计现象推进到了指令级证据，但仍不能证明 PIC 代码最终为何没有继续到 endpoint 探测。下一步应比较 Linux 在标准 QEMU 与 Axvisor 中的 PIC 初始化结果、APIC/IOAPIC 路由及 EOI 反馈；只有绕过或完成这条公共路径后，`CF8/CFC` 是否能访问 `00:01.0` 才能被独立判断。

### 3.14 `noapic` 对照不能绕过该路径

为了验证问题是否仅由 IOAPIC/APIC 路由触发，临时给 direct Linux 的 kernel command line 增加 `noapic`，然后运行同一个 SVM PCI 枚举用例：

```text
set -o pipefail
timeout --signal=INT 40s cargo xtask axvisor test qemu --arch x86_64 --test-group normal --test-case pci-enumeration-svm 2>&1 | tee /tmp/axvisor-pci-enumeration-noapic-40s.log
```

该实验结束后已恢复测试配置中的原始 command line。结果仍然是外层超时，且没有测试成功标记，但日志给出了有区分度的对照：

- Linux 明确输出 `ACPI: Skipping IOAPIC probe due to 'noapic' option`，说明该启动参数确实生效；但随后仍输出 `APIC: Switch to symmetric I/O mode setup`，并持续执行 `0x21` 端口访问。
- 在进入该循环前，CF8/CFC 配置访问仍只有 `pci_config=152`，访问对象仍是 Q35/LPC 与 absent function，没有 `00:01.0` endpoint 配置访问；PCI memory aperture 访问仍为零。
- 端口 I/O 聚合中的代表位置从普通对照的 `IN 0x21` 变为 `OUT 0x21`，RIP 也随 kernel 加载位置改变；但 `guest run progress`、vLAPIC accepted interrupt 和 SVM exit 计数仍增长，说明不是 VCPU 完全停止。
- `vpic wire mode change to LAPIC, unimplemented` 在本轮仍未出现。

因此，单纯关闭 IOAPIC 并没有让 Linux 继续到 PCI endpoint 探测；当前应把问题描述为“Linux 在 Axvisor 提供的 legacy PIC/timer 环境中未完成早期初始化”，还不能缩小为某一个 APIC 路由分支。这个对照也进一步说明，下一轮若要验证 CF8/CFC，应该直接给 guest 一个绕过早期 PIC/timer 探测的最小 PCI 访问路径，或在 vPIC 的命令/屏蔽/EOI 状态转换上增加更精细的日志。

### 3.15 显式指定 `pci=conf1` 的对照

为了确认 Linux 的 PCI 配置机制选择不是隐藏变量，临时在同一个 direct Linux 配置中加入 `pci=conf1`，其余设备和内核参数保持不变：

```text
set -o pipefail
timeout --signal=INT 35s cargo xtask axvisor test qemu --arch x86_64 --test-group normal --test-case pci-enumeration-svm 2>&1 | tee /tmp/axvisor-pci-enumeration-pci-conf1-35s.log
```

实验后已恢复原始 command line。日志中的 kernel command line 确实包含 `pci=conf1`，并且 Axvisor 侧继续看到 `port=0xcf8` 的配置地址写入；但结果与默认配置没有实质变化：

- `pci_config` 仍为 `152`，没有任何 `x86 PCI endpoint config` 记录，PCI aperture 也没有访问；拓扑日志仍确认 endpoint 在 `00:01.0`。
- guest 仍在 `APIC: Switch to symmetric I/O mode setup` 后反复访问 `0x21`，`guest run progress`、SVM exit 和 vLAPIC accepted interrupt 继续增长；PM timer 仍没有被读到。
- `vpic wire mode change to LAPIC, unimplemented` 仍未出现，外层 35 秒超时结束且没有成功标记。

这个结果直接确认：当前 Linux 已经在使用传统 CF8/CFC 访问（或被 `pci=conf1` 强制到该路径），但它尚未执行到 `00:01.0` 的配置读取。因而“没有 ECAM 是否会自动回退到 CF8/CFC”在本实验中的答案是：ECAM 缺失并不是当前阻塞点，CF8/CFC 已经工作并完成了 host/LPC/absent-function 访问；尚未证明的是 guest 是否能跨过早期 PIC/timer 路径后继续访问 endpoint。

### 3.16 vPIC 状态、EOI 与中断来源跟踪

为了判断反复出现的 `0x21` 访问究竟是 vPIC 状态错误，还是 Linux 正常修改 PIC 状态时留下的待处理 IRQ，又增加了三类有界日志：PIC 读写前后的 master/slave 状态、带计数的 EOI 记录，以及区分 PIT 旧路径和 PIC 写后重新调度路径的中断计数。执行命令为：

```text
set -o pipefail
timeout --signal=INT 35s cargo xtask axvisor test qemu --arch x86_64 --test-group normal --test-case pci-enumeration-svm 2>&1 | tee /tmp/axvisor-pci-enumeration-pic-state-35s.log
```

本轮没有成功标记，日志在 guest 进入 APIC/PIC 初始化之后仍然停留在早期阶段；但 PIC 状态已经可以逐步复核：

- reset 状态为 master vector `0x08`、slave vector `0x70`，两个 mask 都是 `0xff`；Linux 随后发送标准 ICW 序列，把 master/slave vector 改为 `0x30/0x38`，并完成 ICW3/ICW4。说明初始化命令没有被错误地当成普通 mask 写入。
- Linux 先写 master mask `0xfe`，第一次 PIT IRQ0 被 claim 为 vector `0x30`，状态为 `request=0`、`in_service=1`；guest 读取到 mask `0xfe`，随后写回 `0xff`，并发送 specific EOI `0x60`。日志显示 EOI 后 `in_service` 被清零，状态转换符合非 auto-EOI 的 8259 语义。
- EOI 之后下一次 PIT edge 先进入 `request=1`，但因为 mask 仍为 `0xff`，`claim_pending_interrupt()` 返回 `None`。稍后在 guest 再次修改 PIC 状态后，写后重新检查路径 claim 了同一个 vector `0x30`，并记录 `master_in_service=true`；该 claim 随后进入 vCPU dispatcher，SVM 后端和 vLAPIC 都分别记录了 ready/accepted。这个序列说明“写 PIC 后重新检查 IRR”确实能把之前被 mask 的边沿重新变成可投递中断，不能仅凭 `no_route` 计数判定为错误。
- 本轮的 `PIC after-write dispatch`、vCPU delivery 和 `vLAPIC accepted interrupt` 首个计数相互对应，仍没有出现 `vpic wire mode change to LAPIC, unimplemented`。`pm_timer=0` 说明该 direct Linux 路径在进入 PIC/APIC 阶段前尚未读取 PM timer；这不是 PM timer 停止的证据。
- 到该路径为止 `pci_config=152`，访问仍集中在 Q35/LPC/absent function，没有 `00:01.0` endpoint 配置访问，也没有 PCI aperture 访问。guest run progress 和 SVM exit 仍增长，VCPU 仍在运行。

为确认写后重新 dispatch 的触发条件，又把 `PIC after-write dispatch` 与 guest 的写端口和值关联起来。计数为 `1、2、4、...、1024` 的样本全部是 `port=0x21 value=0xfe`；其中一次完整状态为：写入前 master `mask=0xff request=1 in_service=0`，写入后变为 `mask=0xfe request=1 in_service=0`，随后 claim 得到 vector `0x30`。这正是 guest 重新解除 IRQ0 屏蔽后，vPIC 重新检查已有 IRR 请求的预期行为，不是 EOI 错误地触发了 dispatch。

随后 guest 读取到 `0xfe`，接收 vector `0x30`，再写 specific EOI `0x60`；EOI 日志显示 ISR 清零。下一次 PIT edge 可能在 mask 为 `0xff` 时留下 `request=1`，等 guest 再次写入 `0xfe` 后重新投递。因此反复出现的 vector `0x30` 与 PIC 状态变化相互吻合，当前没有暴露出 vPIC 基本状态机错误，也不能把它解释成 `vpic wire` 日志导致的停止。该轮测试仍未出现成功标记，但 VCPU 的 run-progress、SVM exit 和 vLAPIC accepted 计数持续增加。下一步应记录 `0x21` 访问位置的 guest 指令字节和控制流，确认 Linux 是在 PIC 中断处理、轮询还是重编程路径中反复运行。

### 3.17 `IN 0x21` 指令级结果与最新枚举进度

在上一轮之后增加了按 `IN 0x21` 次数采样的 SVM 日志，执行命令为：

```text
set -o pipefail
timeout --signal=INT 60s cargo xtask axvisor test qemu --arch x86_64 --test-group normal --test-case pci-enumeration-svm 2>&1 | tee /tmp/axvisor-pci-enumeration-pic-loop-60s.log
```

本轮以外层 `timeout` 退出码 `124` 结束，没有测试成功标记；但运行时已经出现了一个此前记录中没有明确捕捉到的 endpoint 配置读：`CF8/CFC` 访问 `00:01.0` 的 register `0x02`，返回 device ID `0x1110`。因此“当前所有运行都没有 endpoint 配置访问”的表述需要修正为：不同轮次的 guest 控制流不同，最新这轮已经至少访问到了 endpoint 的 vendor/device ID，但仍没有出现 PCI aperture/BAR 访问或测试成功标记。

`IN 0x21` 的重复样本全部位于同一个 guest RIP `0xffffffff9263bf83`，下一条地址为 `0xffffffff9263bf85`，指令字节为：

```text
e4 21 0f b6 05 cc e8 9f 01 e6 21 8d 43 60 e6 20
```

这段指令可解码为 `IN AL,0x21`、读取内存中的屏蔽值、`OUT 0x21,AL`，再执行 `OUT 0x20,0x60`。采样时 `RFLAGS.IF=false`、`int_state=0`、PIC 读值为 `0xfe`，后续 vLAPIC accepted、specific EOI 和写后 dispatch 计数成批同步增长。也就是说，RIP 对应的是 Linux 的 legacy PIC IRQ0 确认/屏蔽处理片段，而不是一个等待 PM timer 值变化的普通轮询循环。

在约 40 秒的有效 guest 运行期间，`IN 0x21` 样本从 `1` 增长到 `16384`，vLAPIC accepted interrupt 和 `PIC after-write dispatch` 也增长到 `16384`；`guest run progress` 从 `4` 增长到 `32768`。同期 PIT 统计达到 `attempts=16384`，其中旧的 PIT 直接 claim 仍只有 `1` 次，绝大多数 IRQ0 是在 guest 写 `0x21=0xfe` 后由 pending request 重新 dispatch 的。这个相关性说明 guest 正在反复处理 IRQ0，但尚不能仅凭日志断言是 PIT 本身频率过高、guest 有意重开 IRQ0，还是两者共同造成了高频中断风暴；下一轮应直接记录 PIT channel 0 的 reload 值、guest 可见周期和 host callback 频率。

本轮没有出现 `vpic wire mode change to LAPIC, unimplemented`，`pm_timer=0` 仍只表示该 direct Linux 阶段尚未访问 PM timer。当前最强结论从“卡在 PIC 数据端口软件循环”更新为“guest 反复进入 legacy PIC IRQ0 处理路径，且已经可以继续完成至少一次 endpoint ID 配置读取；需要继续检查 PIT 触发频率和中断重入节奏”。

### 3.18 PIT 回调频率实验：本地 SVM 启动未进入可观测阶段

为记录 PIT 的实际编程参数和 host timer callback 频率，在 `EmulatedPit` 的 channel 0 编程和 IRQ0 callback 处加入了有界日志。原计划运行：

```text
set -o pipefail
timeout --signal=INT 45s cargo xtask axvisor test qemu --arch x86_64 --test-group normal --test-case pci-enumeration-svm 2>&1 | tee /tmp/axvisor-pci-enumeration-pit-callback-45s.log
```

本次运行没有到达 Linux，也没有出现 `PIT channel 0 program`、`PIT IRQ callback`、PIC 状态或 PCI endpoint 采样。guest 在 Axvisor 启动后只完成了极少数 VCPU entry，随后连续产生未处理的 SVM `EXCP(6)`（invalid opcode）退出：

```text
guest_rip=0xfff4 guest_next_rip=0x0 rflags=0x46 rflags_if=false
entry=GPA(0x8000)
```

Axvisor 将该未实现的 VM-exit 转换为 `X86VmExit::Halt`，所以进程在外层 45 秒限制前结束。随后使用同一个用例和 25 秒限制重试，仍然在 `rip=0xfff4` 重现；而此前的同一用例运行曾经进入 Linux 并完成 `00:01.0` device ID 读取。因此这两轮不能用于推断 PIT callback 频率，反而说明当前 WSL 的嵌套 SVM 运行结果存在明显不稳定性，或者新增诊断构建改变了极早期启动时序；需要在能稳定进入 Linux 的运行器上继续该实验。

这轮结果也提醒后续分析要区分两类失败：`EXCP(6)` 是 guest 尚未开始 PIT/PCI 初始化时的启动失败；此前的 `IN 0x21`/IRQ0 风暴则发生在 guest 已经进入 Linux、并且至少访问过 endpoint ID 之后。不能把本轮早期 `EXCP(6)` 与 PIT 频率或 `vpic wire` 日志建立因果关系。

随后临时撤掉 PIT 日志、保留其余诊断改动进行隔离重跑：

```text
set -o pipefail
timeout --signal=INT 30s cargo xtask axvisor test qemu --arch x86_64 --test-group normal --test-case pci-enumeration-svm 2>&1 | tee /tmp/axvisor-pci-enumeration-pit-isolation-no-pit-log-30s.log
```

隔离轮次仍然在 `rip=0xfff4` 重现 `EXCP(6)`，没有进入 Linux；因此当前证据不支持把 PIT 新增日志视为触发原因。PIT 日志改动已恢复，工作区状态回到隔离实验前的诊断版本。

虽然本轮 callback-specific 日志没有采到有效样本，但此前能够进入 Linux 的几轮日志已经留下了 `inject_pit_irq` 路径的计数时间点，可作为 host PIT callback 的间接频率证据。例如，`pic-loop-60s` 中 `attempts=8` 到 `attempts=16384` 的时间为约 `6.908 s -> 29.053 s`，即约 `738/s`；`noapic-40s` 约为 `738/s`，`pci=conf1-35s` 约为 `746/s`，`interrupt-sources-45s` 约为 `763/s`。这些值明显高于 PIT 默认的约 `18.2 Hz`，但与 Linux 将 PIT 配置为约 `1 kHz` 后、在 WSL 嵌套虚拟化环境中实际观测到的降速相符。它们仍不能替代 channel 0 reload 值和 callback 原始时间戳，因为当前统计点位于 IRQ 注入函数，最终需要在能稳定进入 Linux 的 CI/KVM runner 上补采 `PIT channel 0 program` 和 `PIT IRQ callback`。

因此目前可以把问题进一步描述为“guest 已经进入高频 legacy IRQ0 处理，且嵌套环境下每秒约有数百次 PIT 注入尝试”，但还不能据此断言 PIT 模拟违反了 guest 编程的 reload 周期，也不能把它等同于 PM timer 停止。下一轮应优先在稳定 runner 上复现并记录 channel 0 的 divisor/mode/period，再决定是修复 PIT 重装/回调调度，还是继续追踪 IRQ0 处理为何延迟 PCI BAR 访问。

随后再次执行同一用例，使用 60 秒外层限制：

```text
set -o pipefail
timeout --signal=INT 60s cargo xtask axvisor test qemu --arch x86_64 --test-group normal --test-case pci-enumeration-svm 2>&1 | tee /tmp/axvisor-pci-enumeration-pit-callback-rerun-60s.log
```

结果与前两次早期失败一致：VCPU 仅完成两次 entry，分别在约 `2.057 s` 和 `2.100 s` 于 `RIP=0xfff4` 触发 SVM `EXCP(6)`，`RFLAGS=0x46` 且 `IF=false`；第二次退出后没有新的 guest entry、PIT、PIC 或 PCI 诊断样本。Axvisor 将该 unsupported VM-exit 转为 VCPU halt，但外层 QEMU/测试进程仍保持存活，直到 60 秒 timeout 退出码 `124` 回收。这说明当前测试的“卡住”还包含一个宿主侧终止语义问题：guest VCPU 已停止，而 QEMU 没有收到成功/失败终止信号；它不能作为 PIT/PCI 卡死阶段的有效样本。

### 3.19 实际 UEFI SVM 运行：PIT 参数正常，`0x8` 循环仍复现

为避免只用 direct Linux 用例推断，在 PIT 日志恢复后重新运行实际 UEFI 磁盘用例：

```text
set -o pipefail
timeout --signal=INT 60s cargo xtask axvisor test qemu --arch x86_64 --test-group experimental --test-case uefi-disk-svm 2>&1 | tee /tmp/uefi-disk-pit-callback-svm-rerun-60s.log
```

本轮成功启动了外层 Axvisor 和 guest VCPU，但仍在外层 60 秒限制处退出码 `124`，没有测试成功标记。与之前不同，本轮首次采到了 channel 0 的完整编程和 callback 证据：

```text
PIT channel 0 program: reload=0x2e9c divisor=11932 mode=SquareWaveGenerator period_ns=Some(10000150)
PIT IRQ callback: callbacks=1 ... period_ns=Some(10000150)
PIT IRQ callback: callbacks=32 ... period_ns=Some(10000150)
PIT IRQ callback: callbacks=64 ... period_ns=Some(10000150)
PIT IRQ callback: callbacks=128 ... period_ns=Some(10000150)
PIT IRQ callback: callbacks=256 ... period_ns=Some(10000150)
```

`11932 * 1e9 / 1193182` 约为 `10.00015 ms`，即 guest 配置的是约 `100 Hz` 的 PIT，而不是默认的约 `18.2 Hz`，也不是此前 direct Linux 轮次表现出的数百次每秒注入尝试。callback 的 deadline 与 `now` 只相差约几十至几百微秒；到 `callbacks=256` 时，PIC claim、legacy vCPU injection、vLAPIC accepted 和 specific EOI 计数均已达到相同量级，`pic_dispatch_failure=0`。因此这轮没有显示 PIT deadline 不重装、PIT callback 停止或基础 vPIC EOI 状态机失效。

在 PIT 编程后，guest 仍只完成了固定的一组 PM timer 读取（`pm_timer=100000`），随后反复访问未映射端口 `0x8` 并得到 `0xffffffff`；`0x8` 的代码页仍为运行时 PE 的 `.text`，指令字节为：

```text
ed 44 89 ca 29 c2 0f ba e2 17 72 04 f3 90 eb ea
```

这段代码可解码为 `IN EAX,DX`、用另一个寄存器相减、测试 bit 23、`PAUSE` 后回跳；当前 `RDX=0x8`。在 `0x8` 读取增长到 `2048` 的过程中，`pm_timer` 仍固定为 `100000`、`pci_config=126` 且没有 VirtIO endpoint/BAR 访问；guest `run progress` 继续增长到 `2048`，说明 guest 在执行而非 VMM 线程静止。本轮仍出现一次 `vpic wire mode change to LAPIC, unimplemented`，但它发生在约 `4.468 s` 的 guest 早期 LINT0 设置阶段，远早于约 `27.86 s` 的 PIT 编程和约 `28.15 s` 的 `0x8` 循环，时间关系不支持把该 warning 视为这次卡死的直接原因。

这轮把当前问题进一步收窄为：实际 UEFI guest 的 PM timer 读值和 PIT 周期都符合预期，PIC 中断也能持续投递；卡住点是某个运行时 EFI PE 在 PCI endpoint 枚举前使用了错误的端口变量 `0x8`，并在返回全 1 后进入 bit-23 计时等待。下一步不再优先修改 PM timer/PIT/vPIC，而是从运行时 PE 的导入表、调试目录、调用者栈或标准 QEMU 对应 I/O 序列中识别模块和端口变量初始化来源。

### 3.20 运行时 PE 身份探测与 firmware volume 匹配：确认是 `TlsDxe`

为了把 `0x8` 循环从“某个未知 PE”进一步定位到具体 UEFI/DXE 模块，在不改变任何虚拟硬件行为的前提下，为 SVM 诊断采样增加了有界 PE directory 读取：读取 PE32+ optional header 的 directory 数量，并只检查 export、import、debug 三个目录；字符串读取分别限制为 64 字节和 128 字节，import/debug 条目限制为最多 8 个。目录内容不完整时只停止该目录扫描，不使整个诊断结果失效。

实际执行过三轮探测：

1. 最初只在 `reads=1` 采集身份，但第一条可见的完整采样经常从 `reads=4` 或 `reads=8` 开始，因此日志中只能看到 `pe_identity=None`。
2. 随后改为按 PE 基址去重；身份结果可能已经在未被日志保留下来的早期幂次采样中成功生成，后续可见行仍显示 `None`。这说明“一次性结果 + 长诊断行”不适合作为证据。
3. 最后改为每个幂次采样重新读取一次身份，并把摘要拆成独立短日志。执行命令为：

```text
set -o pipefail
timeout --signal=INT 75s cargo xtask axvisor test qemu --arch x86_64 --test-group experimental --test-case uefi-disk-svm 2>&1 | tee /tmp/uefi-disk-pe-identity-summary-75s.log
```

本轮最终仍由外层 timeout 退出码 `124` 结束，但在约 `29 s` 后稳定进入 `0x8` 循环，并反复打印：

```text
[HV] SVM PE identity: rip=0x1e9b1e98 base=0x1e956000 directory_count=Some(16) export=None import=None debug=None export_name=None imports=[] pdb_path=None
```

结合前面已经验证过的 PE 元数据：machine 为 `0x8664`，optional magic 为 `0x20b`，section 数量为 `3`，section/file alignment 均为 `0x40`，image size 为 `0x9f7c0`，subsystem 为 `11`（EFI boot-service driver），当前 RIP 位于其 `.text` section。因此可以确认：

- `0x8` 循环位于一个真实、已加载的 x86-64 EFI PE/COFF 映像中，不是随机内存中的 `MZ/PE` 签名；
- 该映像的 export、import、debug directory 都为空，未包含可用的导出名、依赖模块名或 CodeView PDB 路径，所以仅靠运行时 PE directory 无法直接命名它；
- 当前仍能直接复核该映像中的端口变量：RIP-relative dword 位于 `0x1e9f1fa8`，guest 读值为 `0x8`，随后被装入 `EDX` 用于 `IN EAX,DX`；
- 本轮没有改变端口返回值、PIC、PIT、PCI 或 vLAPIC 行为。测试仍显示 PM timer `0x608` 读值递增、PIT callback 持续、PIC claim/injection 无 dispatch failure，而 `0x8` 读值仍是 `0xffffffff`。

随后对仓库中的 `assets/OVMF_CODE_4M.fd` 做了只读的 firmware volume 解压和 PE 对照。顶层 GUID-defined section 解压后的 FV 中存在唯一同时满足运行时 PE 元数据、section 布局和循环代码字节的候选：

| 项目 | 结果 |
| --- | --- |
| 解压 FV 中 PE 的 MZ 偏移 | `0x43dc04` |
| FFS 文件偏移 / 大小 / 类型 | `0x43dbe8` / `0x9f7fe` / driver `0x07` |
| FFS GUID | `3aceb0c0-3c72-11e4-9a56-74d435052646` |
| UI section | `TlsDxe` |
| PE section 起点 / 大小 | `0x43dc00` / `0x9f7c4` |
| runtime PE base | `0x1e956000` |
| runtime RVA | `0x5be98` |
| static/runtime bytes | `ed4489ca29c20fbae2177204f390ebea`，完全一致 |

EDK2 的 `TlsDxe.inf` 使用同一 GUID `3aceb0c0-3c72-11e4-9a56-74d435052646`，因此可以把运行时映像确定命名为 `TlsDxe`，而不再只是“某个未知 DXE driver”。参考：[EDK2 TlsDxe.inf](https://github.com/tianocore/edk2/blob/master/NetworkPkg/TlsDxe/TlsDxe.inf)。

对解压出的静态 `TlsDxe` 做反汇编后，RVA `0x5be98` 的循环为：

```text
mov edx,[rel 0x9bfa8]
in eax,dx
mov [rcx],al
in eax,dx
lea r9d,[rax+0x3]
mov edx,[rel 0x9bfa8]
in eax,dx
sub edx,eax
bt edx,0x17
pause
jmp 0x5be92
```

这正是旧版 OpenSSL 随机池从性能计数器取噪声的计时循环特征；EDK2 后续补丁曾将 OpenSSL `rand_pool` 对 `TimerLib` 的依赖改为 `RngLib`，可作为来源背景参考：[EDK2 OpenSSL rand_pool TimerLib removal patch](https://patchew.org/EDK2/20200826205501.1124-1-matthewfcarlson%40gmail.com/20200826205501.1124-6-matthewfcarlson%40gmail.com/)。`TlsDxe.inf` 本身没有直接声明 `TimerLib`，但其 TLS/OpenSSL 依赖链可以带入这段旧随机池逻辑。

同一映像入口附近还反汇编出 Q35 PM timer 端口初始化：代码向 `CF8` 写入 `0x8000f840`，从 `CFC` 读取 LPC register `0x40`，对 PMBASE 应用 `& 0xfe` 后加 `8`，并把结果写入相对地址 `0x9bfa8`。因此 `0x8` 的含义已经具体化为：`TlsDxe` 的计时端口变量最终保存了 `0x8`；这可能来自它看到的 PMBASE 读值为零，也可能来自后续内存/初始化路径覆盖，不能直接用已经观察到的其他时刻 `CFC -> 0x601` 读值替代。当前仍需将带 guest RIP 的 CF8/CFC trace 与 `TlsDxe` 的具体初始化调用逐条关联。

因此，模块归属已经完成；剩余问题不是“无法命名”，而是解释为什么 `TlsDxe` 没有得到预期的 `PMBASE + 8 = 0x608`，以及该错误状态为何阻止后续 VirtIO endpoint/BAR 访问。

### 3.21 最新本地 PCI trace 尝试：WSL/KVM 在 guest 前复位

为直接记录每个早期 CF8/CFC 访问的 guest RIP、PE 基址、CF8 selector 和 guest 指令，给 SVM 后端增加了最多 128 个启动期 PCI 配置 I/O trace。代码检查和格式化均通过后运行：

```text
set -o pipefail
timeout --signal=INT 60s cargo xtask axvisor test qemu --arch x86_64 --test-group experimental --test-case uefi-disk-svm 2>&1 | tee /tmp/uefi-disk-pci-config-trace-60s.log
```

该轮没有生成 `SVM PCI config I/O trace`，因为外层 QEMU 在 Axvisor 输出 `Secondary CPU 1 started` 后反复从 UEFI 重新启动；单 vCPU 临时对照也只到 `Primary CPU 0 init OK` 后复位。为排除测试驱动本身的重试，直接运行同一已生成 QEMU 产物仍复现该复位。临时将外层 QEMU 改为 `-smp 1` 并把 guest 绑定到 CPU0 后，另一轮能够进入 Axvisor VMM，但当前构建又在 guest 创建前重启；临时修改已恢复为 `-smp 2` 和 `phys_cpu_ids = [1]`。

这轮只说明当前 WSL/KVM 环境在这次诊断构建下没有稳定到达 guest，不能说明 PCI 配置读值是零，也不能推翻此前有效运行中的 `00:1f.0 register 0x40 -> 0x601` 证据。`/tmp/uefi-disk-pci-config-trace-60s.log`、`/tmp/uefi-disk-direct-qemu-20s.log` 和 `/tmp/uefi-disk-pci-config-trace-smp1-75s.log` 应作为失败运行记录保留；带 caller RIP 的配置响应关联需要在稳定的 CI/KVM runner 上补采。

### 3.22 增加带 caller RIP 的 CF8/CFC 返回值 trace

上一轮只记录了 SVM 退出侧的 PCI 配置请求，无法把请求和设备前端实际返回值严格对应。为解决这个证据缺口，继续做了诊断性改动：

- `X86VmExit::PortIoRead/PortIoWrite` 携带发生 I/O 退出时的原始 guest RIP；SVM 在推进 RIP 前保存该值，VMX 使用当前 guest RIP 填入该字段；
- `axvm` 的 x86 I/O 处理层在 PCI 配置端口 `0xcf8..0xcff` 返回值产生后记录端口、宽度、`mapped` 状态、返回值、guest RIP 和 vCPU；写访问也记录同样的 caller 信息；
- trace 采用最多 128 条的有界输出，不改变 PCI 配置设备的返回值和路由行为。

本次修改已完成 `cargo fmt --all`，并通过 `cargo xtask clippy --package x86_vcpu` 和 `cargo xtask clippy --package axvm`。重点运行命令及结果如下：

```text
timeout --signal=INT 90s cargo xtask axvisor test qemu --arch x86_64 \
  --test-group experimental --test-case uefi-disk-svm 2>&1 \
  | tee /tmp/uefi-disk-pci-response-rip-90s.log
```

该轮确实完成了 `/tmp/uefi-guest.img` 的 staging；构建日志显示镜像被写入外层 Axvisor rootfs 的 `/uefi/uefi-guest.img`。随后外层 Axvisor 在 `Secondary CPU 1 started` 后重复从 UEFI 启动，没有出现 `Secondary CPU 1 init OK`、`Using static VM configs` 或 `VM[1] VCpu[0] running`，因此新增的 `guest PCI config response` 没有机会执行。

又将外层 QEMU 临时改为 `-smp 1`、guest 绑定 CPU0 做对照，运行：

```text
timeout --signal=INT 75s cargo xtask axvisor test qemu --arch x86_64 \
  --test-group experimental --test-case uefi-disk-svm 2>&1 \
  | tee /tmp/uefi-disk-pci-response-rip-smp1-75s.log
```

单 vCPU 轮次仍未进入 `Using static VM configs`，也没有产生 guest PCI response trace；临时配置已恢复为外层 `-smp 2`、guest `phys_cpu_ids = [1]`。作为环境对照，仓库已有的普通 `qemu-pci-enumeration-svm` 能完成 `Secondary CPU 1 init OK`、进入 `Using static VM configs`、启动 `VM[1] VCpu[0]` 并产生 guest exit/progress，记录在 `/tmp/axvisor-pci-enumeration-svm-baseline-45s.log`。因此不能把本地现象概括为“所有嵌套 SVM 都无法运行”；更具体的边界是，当前 UEFI 磁盘实验在外层 Axvisor 的 CPU/启动初始化阶段复位，而普通 SVM 对照可以到达 guest。

另外，已通过生成 rootfs 的 `debugfs` 检查确认 `/uefi/uefi-guest.img` 存在且大小为 `268435456` 字节，当前证据不支持“镜像未注入外层 Axvisor 文件系统”这一解释。现阶段仍不能据此判断 `0x8000f840` 的 CFC 返回值是 `0` 还是 `0x601`；这个问题需要在能稳定进入 guest 的 runner 上读取新增的 `guest PCI config response`，再与 `TlsDxe` 的 caller RIP 对齐。

### 3.23 外层 rootfs 收尾与次 CPU precheck 的阶段边界

为了区分“外层 NVMe rootfs 初始化未返回”和“次 CPU per-CPU 初始化失败”，在 `axruntime` 和 `ax-fs-ng` 中继续加入了只读诊断标记：文件系统收尾分别标记创建 root mountpoint、获取 root location、注册 filesystem 和初始化 root context；次 CPU precheck 分别标记进入函数、逻辑 CPU index 转换、expected area 查询、pin 查询、layout 查询和启动栈查询。没有修改任何设备行为或启动顺序。

先用带有第一版收尾标记的目标用例运行：

```text
set -o pipefail
timeout --signal=INT 90s cargo xtask axvisor test qemu --arch x86_64 \
  --test-group experimental --test-case uefi-disk-svm 2>&1 \
  | tee /tmp/uefi-disk-fs-boundary-90s.log
```

这轮构建仍然是外层 QEMU `-smp 2`、2 GiB 内存、QEMU NVMe rootfs，Axvisor 使用 `features=ax-std/fs,fs`；`uefi-guest.img` 已成功从 `/tmp` staging 到外层 rootfs 的 `/uefi/uefi-guest.img`。清除串口 ANSI 控制序列后，日志中重复得到以下边界：

```text
[HV] filesystem init: installing block runtime
[HV] filesystem init: discovering root block device
  block runtime installed from RDIF sources
  raw device fs=Some(Ext4)
  discovered 1 block device candidate(s)
  discovered 1 root candidate(s)
  only one raw block device is available; using it as root
Initialize filesystem subsystem...
  selected root device: disk0 raw device
  filesystem type: "ext4"
[HV] SMP secondary 1: entered rust_main_secondary
```

上述文件系统和次 CPU 入口各出现 10 次，但以下标记全部为 0 次：

```text
root mountpoint created
root location acquired
root filesystem registered
root filesystem context initialized
filesystem init: root filesystem setup returned
primary CPU 0: filesystem init complete
SMP secondary 1: per-CPU state initialized
Using static VM configs
VM[1] VCpu[0] running
```

原始串口中还出现了两条来自 `axruntime/src/mp.rs:33` 的截断记录，内容为 `[HV] SMP seconda...`，对应 precheck 函数的第一条 `per-CPU precheck begin` 日志在外层重新启动时被截断；没有看到其后的 line 43、45、46、48、50 或 52 阶段记录。因此当前证据只能确认次 CPU 曾进入 precheck 调用点附近，不能据此断言 `ax_percpu::with_cpu_pin` 或 `ax_hal::percpu::init_secondary` 已经执行。

与之对比，普通 `pci-block-ro-svm` 在没有外层 NVMe rootfs、也不启用 `fs` 的配置下，已经完整打印 `per-CPU state initialized`、`early platform init complete`、`Using static VM configs` 和 `VM[1] VCpu[0] running`。因此这不是普通 SVM SMP 启动必然失败，而是 UEFI 磁盘实验新增的外层 `fs + NVMe rootfs` 路径触发了本地复位/终止；早期次 CPU 启动交错只是当时的候选因素。现阶段最准确的定位是：Ext4 对象已经创建并报告类型，但 root mountpoint/context 收尾和次 CPU precheck 的后续阶段没有形成可观测的完整记录；还不能把远端 guest 内的 `vpic wire mode change to LAPIC` 与这次本地外层复位等同起来。调用前标记已经继续把边界推进到 root mountpoint 创建之后，单 vCPU 结果见 3.24。

### 3.24 单 vCPU 对照仍在 root mountpoint 收尾处复位

为了排除次 CPU 启动交错是外层复位原因，临时把外层 QEMU 和 Axvisor guest 都改成单 vCPU：外层 `qemu-x86_64-svm.toml` 使用 `-smp 1`，guest 配置使用 `phys_cpu_ids = [0]`。前两次 60 秒和 90 秒尝试只耗在 `cargo xtask` 的构建/准备阶段，没有出现 `qemu-system-x86_64` 启动行，因此不作为运行结果；随后使用 180 秒总时限重新执行：

```text
set -o pipefail
timeout --signal=INT 180s cargo xtask axvisor test qemu --arch x86_64 \
  --test-group experimental --test-case uefi-disk-svm 2>&1 \
  | tee /tmp/uefi-disk-fs-smp1-180s.log
```

清理串口 ANSI 控制序列后，单 vCPU 日志中反复出现完整的启动和文件系统前半段；`smp = 1`、外层 QEMU `-smp 1` 和以下阶段均可确认：

```text
[HV] filesystem init: installing block runtime
[HV] filesystem init: discovering root block device
  filesystem type: "ext4"
  creating root mountpoint
```

上述序列共出现 12 次，表明 Axvisor 在外层 QEMU 反复重新启动；没有出现 `Using static VM configs`、`VM[1] VCpu[0] running` 或 guest 成功标记。`root mountpoint created` 之后的 `root location acquired`、root filesystem 注册、root context 初始化和 `filesystem init: root filesystem setup returned` 均没有形成完整可计数记录；其中一次 `root mountpoint created` 与随后固件输出交错，表现为截断串口记录，不能把它当成后续阶段已经完成。

因此，去掉 secondary vCPU 后现象仍然存在，当前外层问题不需要 SMP 才能触发；这轮对照排除了“次 CPU 初始化是必要条件”的假设，但尚未区分 `Mountpoint::new_root_with_source` 返回后的具体操作、异常/复位处理或串口记录丢失。临时的单 vCPU 配置已恢复为外层 `-smp 2` 和 guest `phys_cpu_ids = [1]`。这轮仍未进入 guest，所以不能用它解释或确认 guest 内的 `vpic wire mode change to LAPIC, unimplemented`。

### 3.25 QEMU 退出路径诊断：不是 `axruntime::terminate()`

为区分“Axvisor 主动调用 `system_off`”和“CPU 在未进入 Rust 终止路径前复位/三重故障”，临时在 `axruntime::terminate()` 入口使用 `emergency_console` 输出 `[HV] runtime terminate entered`，并给外层 QEMU 加上 `-no-reboot`。先前的 `-d cpu_reset,guest_errors` 轮次在 QEMU 运行约 12.19 秒时停止，串口已经到达外层 rootfs 的：

```text
filesystem type: "ext4"
```

QEMU debug 文件只记录了启动阶段的 4 条 `CPU Reset` 和 6 条 rejected MMIO read：

```text
0xFED40000, size 1
0xFED40030, size 4
0xFED40014, size 4
```

没有记录可归因于 Axvisor 的异常向量、CR2 或明确 triple-fault 文本；`0xFED40000` 位于 outer Axvisor 已打印的 `0xFED00000..0xFED01000` HPET 映射之外，当前不能把它当作 rootfs 收尾失败的证据。另一次加入 `-d int,cpu_reset,guest_errors` 的 90 秒轮次因为 WSL/KVM 诊断开销过大，只打印到外层内存布局阶段，没有到达文件系统初始化，因此不能用来判断 rootfs 边界。

使用相同实验配置和 180 秒外层时限再次运行后，QEMU 在 `filesystem type: "ext4"` 后约 8.96 秒停止；实际 Axvisor ELF 中可以找到新增的终止标记字符串，但失败 transcript 中没有出现该标记，也没有出现 `root mountpoint created`。这说明该次停止没有经过 `axruntime::terminate()`；在当前证据下，更应优先检查外层 CPU fault/triple-fault、未被目标 panic hook 捕获的 abort，或 QEMU/KVM 对 guest reset 的处理，而不是继续把原因归结为 `system_off`。

另外，`axruntime/src/lang_items.rs` 的 `#[panic_handler]` 不适用于此次 Axvisor 构建：目标规格的 `os` 是 `linux`，同时构建启用了 `axruntime/std-compat`，所以该文件的 `target_os = "none" && !std-compat` 条件不成立。本轮曾临时加入的 panic handler 标记未留在工作树中；后续若需识别 Rust panic，应在实际 Axvisor `std-compat` 路径的 panic hook/abort 入口增加诊断。

本轮配置和源代码临时改动均已恢复。运行记录保留在 `/tmp/uefi-disk-qemu-reset-150s.log`、`/tmp/uefi-disk-qemu-int-90s.log` 和 `/tmp/uefi-disk-terminate-marker-180s.log`；QEMU debug 文件为 `/tmp/uefi-disk-qemu-debug.log`、`/tmp/uefi-disk-qemu-int-debug.log`。

### 3.26 root source 指针有效，当前 fault 不是复制设备名本身

为验证 page fault 中的坏地址是否就是 root source 字符串，临时在 `init_root` 选择 source 后记录字符串元数据，并在 x86 page-fault 文本中记录从内核 trap frame 栈顶取得的 caller 返回地址。执行命令为：

```text
set -o pipefail
timeout --signal=INT 45s cargo xtask axvisor test qemu --arch x86_64 \
  --test-group experimental --test-case uefi-disk-svm 2>&1 \
  | tee /tmp/uefi-disk-pf-caller-45s.log
```

这一轮没有再次出现 `Unhandled #PF`，而是因外层超时结束；QEMU 在 45 秒内反复从 UEFI 重新启动 Axvisor，最后一次仍停在 `filesystem type: "ext4"`。不过每次进入 root 初始化时都观测到：

```text
root source default metadata: ptr=0xffff80007a652aa0, len=12
root source selected metadata: ptr=0xffff80007a652aa0, len=12
filesystem type: "ext4"
```

该指针对应 outer Axvisor 的有效 RAM，长度 12 与 `/dev/nvme0n1` 一致；而此前 page fault 的 `memcpy` 寄存器表现为 `rcx=1`、`rsi=0xffff8000ffbff000`。因此不能再把故障直接归因于默认 root source 在进入挂载代码前就是坏指针，也不像是复制 12 字节设备名的那一次调用。

结合反汇编，`Mountpoint::new_root_with_source` 复制 source 时传入的长度是 source 的实际长度，且调用前保存的 source 指针就是上述有效地址；`new_with_root_and_source` 本身没有 `memcpy`。当前更可信的分支是：page fault 来自并行的其他调用路径（尤其是 outer secondary CPU 的启动路径），或此前已有内存破坏，不能继续只沿 root source 生命周期排查。由于本轮未捕捉到 caller 返回地址，调用点仍待下一轮 `-no-reboot` 单次启动实验确认。

本轮只用于诊断的 source 元数据日志和 caller 字段不应作为最终功能改动；实验结束后应恢复。当前外层 page fault 的已知事实仍是 `memcpy` 尾部读取了未映射地址 `VA 0xffff8000ffbff000`，对应 `PA 0xffbff000`，位于固件内存图中 `0xff000000..0xffc00000` 的空洞内；但这只能说明访问非法，尚未说明哪个上层对象提供了该地址。

### 3.27 锁定为现有 PE 扫描诊断访问 guest 空洞

为区分坏地址来自正常 guest 执行还是来自 Axvisor 的 guest 指令诊断代码，本轮临时在 `SvmVcpu::read_guest_u8` 的地址翻译后、`AxvmX86HostOps::read_guest_u8` 入口以及 `memcpy` 入口同步记录地址和调用点，并给 QEMU 加上 `-no-reboot`。执行命令为：

```text
set -o pipefail
timeout --signal=INT 120s cargo xtask axvisor test qemu --arch x86_64 \
  --test-group experimental --test-case uefi-disk-svm 2>&1 \
  | tee /tmp/uefi-disk-guest-translation-120s.log
```

构建成功；QEMU 约 7.7 秒后停止。关键同步日志为：

```text
[HV] suspicious guest translation: gva=0xffbff000, gpa=0xffbff000, rip=0xfffce581
[HV] suspicious guest byte read: paddr=0xffbff000
[HV] suspicious memcpy: caller=0xffffffff80107ec6, dst=0xffff8000792bed18, src=0xffff8000ffbff000, len=1
```

`gva` 与 `gpa` 相同，说明该轮 SVM 地址翻译处于 paging level 0；`rip=0xfffce581` 位于 OVMF 固件映像的高端代码区域。`memcpy` 返回地址通过当前 Axvisor ELF 的 `addr2line` 解析为 `axvm::vm::AxVM::read_from_guest`，反汇编也确认它紧跟在该函数对 guest buffer 执行的 `copy_from_slice` 之后。因此坏的 outer VA 确实来自 Axvisor 把 guest GPA `0xffbff000` 转换为 direct-map 指针后的读取。

继续对照现有诊断代码：`guest_image_signatures(guest_rip)` 会从当前 RIP 所在页向低地址逐页扫描最多 4 MiB，并对每一页调用 `read_guest_u8` 检查 `ELF`、`MZ` 和 `PE\0\0` 签名。当前 RIP `0xfffce581` 的扫描范围包含 `0xffbff000`；静态反汇编也显示，PCI 配置端口诊断记录路径会调用 `guest_image_signatures`。而 outer 固件内存图明确把 `PA 0xffbff000` 放在 `0xff000000..0xffc00000` 的空洞中，没有对应的 RAM 映射。

所以，这次已捕获的 page fault 是现有 PE/image-signature 诊断扫描主动探测 guest 非 RAM 地址后触发的诊断副作用，不能作为 root source、PM timer 或 vPIC 故障的证据；`SMP seconda...` 与该日志交错只是串口输出顺序，不是因果关系。若继续定位正常启动路径，必须先停用这段无条件的逐页扫描，或让它只访问已知 guest-backed 区域，再重新采集一轮干净基线。生产路径暂不因这一轮证据修改 `AxVM::read_from_guest` 的语义；不过如果保留类似诊断接口，它必须把未映射 guest 地址安全地当作不可读处理，而不能依赖 outer direct map 触发 page fault。

### 3.28 关闭扫描后的干净基线：`vpic wire` 不是立即致命原因

为取得不受上述诊断副作用影响的基线，临时关闭 `record_port_io_exit` 两处 `guest_image_signatures` 调用，其他代码和 QEMU 配置保持不变。第一次执行被本地网络沙箱拦在同步 image registry 阶段；申请网络权限后用同一命令完成构建并运行：

```text
set -o pipefail
timeout --signal=INT 120s cargo xtask axvisor test qemu --arch x86_64 \
  --test-group experimental --test-case uefi-disk-svm 2>&1 \
  | tee /tmp/uefi-disk-clean-baseline-120s.log
```

这轮没有 `Unhandled #PF`，并完整经过 outer root filesystem 初始化：日志出现 `root mountpoint created`、`root filesystem initialization complete` 和 `VM[1] boot success`。因此前一轮 page fault 与 rootfs 挂载路径没有直接因果关系。临时关闭扫描后，端口诊断日志中的 `image_signatures=(None, None)` 是预期结果；运行结束时仍因外层 120 秒超时停止，未出现 guest Linux shell 或 VirtIO 磁盘读请求。

guest 仍然输出：

```text
[HV] vLAPIC LINT0 write: count=1 vcpu=0 old=0x10000 new=0x700 old_mode=0x0 new_mode=0x7 old_masked=true new_masked=false svr=0x10f
vpic wire mode change to LAPIC, unimplemented
```

但该日志之后 guest 继续执行了大量 PCI 配置访问和 PM timer 读取；没有 page fault 或立即复位。PM timer 计数正常递增，例如连续采样中的 `counter_delta` 与基于 host 时间计算的 `expected_counter_ticks` 一致。之后 guest 长时间位于 `RIP=0x1e9b1e98` 的端口 `0x8` 读取/忙等循环，读值为 `0xffffffff`，并伴随 PIT IRQ0 注入、PIC EOI 和 vLAPIC 接收中断。到 120 秒外层超时前，日志仍没有 VirtIO BAR 访问或磁盘请求。

这轮把结论分成两部分：`vpic wire mode change to LAPIC, unimplemented` 当前是一个真实的兼容性缺口，但至少不是此次 page fault 的根因，也不是触发 QEMU 立即退出的充分原因；在关闭扫描后，主阻塞现象仍是 guest 的 `0x8` 未映射端口循环以及随后的高频 IRQ0/legacy PIC 路径。由于临时扫描开关已在本轮结束后恢复，后续如需继续运行诊断，必须再次关闭该扫描或先给它增加 guest-backed 地址判断。

### 3.29 定向跟踪未捕获到 `TlsDxe` 的 CF8/CFC 访问

为验证 `TlsDxe` 是否在进入 `0x8` 忙等前直接读取了 Q35 LPC 的 PMBASE，临时按已知运行时 image range `0x1e956000..0x1e9f0000` 给所有 PCI 配置端口访问增加 guest RIP 过滤日志，同时关闭会探测 guest 物理空洞的 `guest_image_signatures` 扫描。执行命令为：

```text
set -o pipefail
timeout --signal=INT 120s cargo xtask axvisor test qemu --arch x86_64 \
  --test-group experimental --test-case uefi-disk-svm 2>&1 \
  | tee /tmp/uefi-disk-tlsdxe-pci-trace-120s.log
```

构建和磁盘镜像 staging 成功，运行因外层 `timeout` 在 120 秒时退出，退出码为 124。关闭扫描后没有再次出现 `Unhandled #PF`；guest 仍没有到达 Linux shell、VirtIO BAR 访问或磁盘请求。

这轮没有出现 `TlsDxe PCI config request` 或 `TlsDxe PCI config response`。实际捕获到的 CF8/CFC 访问仍来自 `0x1ff...` 范围的其他固件模块，其中：

- Q35 LPC register `0x40` 的 CFC 返回值为 `0x601`；
- Q35 LPC register `0x44` 的 CFC 返回值为 `0x80`；
- Q35 host bridge device ID 读取返回 `0x29c0`。

因此，本轮没有证据表明 `TlsDxe` 在观测到的 `0x8` 循环期间亲自发起了 CF8/CFC 访问，也没有证据表明该循环是由一次可见的 CFC 读零直接造成的。更可能的候选变成：该变量在更早的固件阶段被初始化或覆盖，或者真正执行配置读取的调用位于另一个运行时模块中；目前不能只凭 `TlsDxe` 的静态入口代码把坏值归因于 Axvisor 的 PCI CFC 返回值。

运行时阻塞现象没有变化：从约 29.8 秒开始，guest 长时间停在 `RIP=0x1e9b1e98`，反复执行对 `RDX=0x8` 的 `IN`，Axvisor 将未映射端口读值返回为 `RAX=0xffffffff`。PIT channel 0 以 reload `0x2e9c`、周期约 10 ms 正常运行，PM timer `0x608` 的采样仍与 host 时间换算一致；`vpic wire mode change to LAPIC, unimplemented` 在约 4.5 秒出现，之后 guest 仍继续执行。因此这轮再次把 PM timer 停摆和 vPIC warning 排除为当前端口循环的直接证据，但没有完成 `0x8` 变量的写入来源定位。

这轮新增的 TlsDxe PCI 跟踪属于一次性诊断代码，实验记录完成后应撤掉。下一轮如果继续取证，应扩大为“已知 `TlsDxe` RIP 范围内的全部端口 I/O”跟踪，而不是只筛选 CF8/CFC；这样可以确认该模块是否只读取已经错误保存的 `0x8`，以及其初始化期间是否访问过其他控制端口。

### 3.30 `TlsDxe` 全端口 I/O 跟踪：只读 `0x8`，没有 PCI/PM timer 访问

为避免只跟踪 CF8/CFC 而漏掉其他初始化端口，临时按已知 `TlsDxe` 运行时 image range `0x1e956000..0x1e9f0000` 跟踪该区间内全部端口 I/O 的请求和返回，最多分别记录 256 条；同时关闭 `guest_image_signatures` 扫描以避免再次访问 guest 物理空洞。执行命令为：

```text
set -o pipefail
timeout --signal=INT 120s cargo xtask axvisor test qemu --arch x86_64 \
  --test-group experimental --test-case uefi-disk-svm 2>&1 \
  | tee /tmp/uefi-disk-tlsdxe-io-trace-120s.log
```

构建、rootfs 准备和 UEFI 启动均成功；运行因外层 `timeout` 在 120 秒时退出。没有出现 `Unhandled #PF`，也没有出现 Linux shell、VirtIO BAR 访问或磁盘请求。

新的请求/返回日志从约 28.27 秒开始出现，全部具有相同模式：

```text
[HV] TlsDxe I/O request: ... port=0x8 direction=in width=Dword rip=0x1e9b1e98 ...
[HV] TlsDxe I/O response: ... port=0x8 width=Dword mapped=false value=0xffffffff guest_rip=0x1e9b1e98 ...
```

在达到 256 条跟踪上限之前，没有观察到 `0x600`、`0x608`、`0xcf8..0xcff`、PIC、PIT 或其他端口，也没有任何 `out` 访问。与此同时，其他固件模块仍通过传统 CF8/CFC 读取 LPC `0x40 = 0x601`、LPC `0x44 = 0x80` 和 host bridge DID `0x29c0`。

这轮把结论推进了一步：在当前 guest 启动轮次中，`TlsDxe` 进入计时循环时并没有通过 PCI 配置端口重新取得 PMBASE；它只是使用已经存在的端口变量 `0x8`，并持续收到未映射端口默认值 `0xffffffff`。因此不能再把当前卡死直接归因于“`TlsDxe` 的 CF8/CFC 读取返回了零”。`0x8` 更可能是在更早阶段由其他固件代码写入，或由固件加载/重定位/全局初始化路径产生；需要继续定位 `0x1e9f1fa8` 的写入来源。

本轮中 `vpic wire mode change to LAPIC, unimplemented` 在约 3.41 秒出现，随后仍有正常的 PM timer 访问和 guest 运行；PIT reload 仍为 `0x2e9c`、周期约 10 ms。因而本轮没有改变“PM timer 正常、vPIC warning 不是当前立即阻塞点”的判断。新增全端口跟踪是一次性诊断代码，已在记录结果后恢复。

### 3.31 变量页写监视实验：监视机制自身触发外层复位

为了定位 guest 地址 `0x1e9f1fa8` 的首次写入者，临时尝试把其所在页 `0x1e9f1000..0x1e9f2000` 的 NPT 权限改为只读，预期在 guest 第一次写入时得到带 guest RIP 的 nested page fault。实验命令为：

```text
set -o pipefail
timeout --signal=INT 300s cargo xtask axvisor test qemu --arch x86_64 \
  --test-group experimental --test-case uefi-disk-svm 2>&1 \
  | tee /tmp/uefi-disk-variable-watch-300s.log
```

构建和 UEFI staging 成功，但运行没有进入 guest 地址空间监视点安装日志，也没有出现 `SVM NPF sample`、`VM[1] boot success` 或 `vpic wire`。在约 3 分钟内日志反复出现完整的 UEFI/外层 Axvisor 启动序列，共观察到 `27` 次 `VM Load` 和 `UEFI application started`；每次都在外层 Axvisor 完成 root filesystem 初始化、即将进入 VM 管理阶段前复位。为避免继续占用资源，手动停止了该轮实验。

代码路径检查解释了这个现象：临时的 `AddrSpace::protect_region` 调用了通用页表的 `protect_page`；该函数对传入的 guest GPA 执行 `TableMeta::flush`，x86 nested paging 的 flush 实现又直接调用宿主 `INVLPG`。因此本轮传入的 `0x1e9f1000` 被当成宿主虚拟地址刷新，而不是执行 EPT/NPT 专用失效操作；它不在 Axvisor 的宿主内核映射中，导致外层 Axvisor 很早异常并由 QEMU 重新启动。由于没有到达 `diagnostic guest write watch installed`，本轮不能推断 `0x1e9f1fa8` 是否真的被 guest 写入。

这轮实验留下了一个独立的实现问题线索：当前 nested paging 页表更新路径使用了与宿主线性页表相同的 `INVLPG` 语义，至少不能直接复用于这种 guest GPA 权限监视。临时的 `protect_region` 和监视点代码已撤销；后续若仍需追踪写入者，应设计不触发宿主 `INVLPG` 的诊断专用路径，或在能正确执行 EPT/NPT 失效的后端上完成监视，再把首次 NPF 的 guest RIP 作为有效证据。

### 3.32 guest 启动前变量快照：本地 SVM 未进入 VM 创建阶段

为了避免修改 NPT，又临时在 `vm.prepare()` 完成后、注册并启动 guest vCPU 前读取 `0x1e9f1fa8`。第一次构建因为 `vm_create_config` 已经被 `prepare_guest_boot` 移动后仍被借用而失败，属于诊断代码的编译错误；将 UEFI PCI-disk 条件提前保存后，第二次构建成功。

修正后的运行命令为：

```text
set -o pipefail
timeout --signal=INT 120s cargo xtask axvisor test qemu --arch x86_64 \
  --test-group experimental --test-case uefi-disk-svm 2>&1 \
  | tee /tmp/uefi-disk-prestart-variable-120s.log
```

本地 WSL/SVM 轮次在约两分钟内反复启动 UEFI 和外层 Axvisor，日志出现 `12` 次 `VM Load`，但没有出现 `Starting virtualization`、`Creating VM`、`diagnostic pre-start guest variable` 或 `VM[1] boot success`。因此既没有读到变量值，也不能据此判定快照读取是否改变了行为；快照代码随后已撤销。该结果再次说明本地嵌套 SVM 的启动时序不稳定，后续需要在能稳定进入 guest 创建/运行阶段的 CI/KVM runner 上做阶段性取证。

### 3.33 离线解压 OVMF 并静态追踪 `0x9bfa8` 的写入来源

前面按 GUIDed section 头部解析压缩数据时曾把 `0x5d` 误认为 `DataOffset`，实际标准 `EFI_GUID_DEFINED_SECTION` 的 `DataOffset` 位于 section 起始地址加 `0x14`，本镜像的值是 `0x18`。因此 OVMF 的 LZMA 数据从 raw offset `0xa8` 开始，而不是 `0xed`。按 EDK2 的 LZMA custom format 读取 13-byte header 后，成功得到解压后的 FV：

```text
raw OVMF_CODE_4M.fd:       0x37c000 bytes
GUIDed section data:       raw 0xa8..0x16e5d2
decoded FV:                0xce0090 bytes
TlsDxe FFS GUID:           3aceb0c0-3c72-11e4-9a56-74d435052646
TlsDxe PE offset in FV:    0x43dc04
TlsDxe PE image size:      0x9f7c0
```

对该 PE 的 `.data` RVA `0x9bfa8` 做 RIP-relative 操作扫描后，只找到两处直接写入：

```text
RVA 0x63af4: mov dword ptr [0x9bfa8], 0xb008
RVA 0x63b65: mov dword ptr [0x9bfa8], ecx
```

第二处写入前的静态逻辑如下：

1. 先读取 host bridge 的 device ID；`0x1237` 走 PIIX3 的 `0xb040`，`0x29c0` 走 Q35 的 `0xf8040`，`0x0d57` 则直接把计时端口变量写成 `0xb008`。
2. 对 Q35 路径，如果 `0x9c844` 表示的 PCI-express 路径未启用，就保存当前 `CF8`，写入 `0x8000f840`，从 `CFC` 读取 LPC register `0x40`，恢复 `CF8` 和中断标志。
3. 将读值执行 `& 0xfffffffe` 后加 `8`，写入 `0x9bfa8`。

因此，静态代码本身可以解释运行时看到的 `0x8`：Q35 路径的 `CFC` 返回零时结果就是 `0x8`；另一种可能是 host bridge device ID 没有命中 `0x1237/0x29c0/0x0d57`，变量保持 `.bss` 的零值，后续使用时同样得到 `0x8`。`0x0d57` 分支不会产生 `0x8`。在当前静态扫描覆盖的直接引用中，没有发现第三处直接改写该变量；仍不能排除通过间接指针写入或 guest 内存被破坏，但“变量后来无故被覆盖”已经不是首选解释。

这项结果把下一次运行时实验收窄为一个明确判据：在 guest RIP 位于 `TlsDxe` 初始化区间时，记录 `out CF8` 的 selector 和紧随其后的 `in CFC` 返回值，特别是 selector `0x8000f840`。如果返回 `0x0`，问题落在 Axvisor 对 Q35 LPC register `0x40` 的配置返回；如果返回 `0x601` 却最终仍为 `0x8`，则应转向 device ID 分支、PCI-express 判定或间接内存写入。此前 `3.30` 的运行时循环日志只覆盖了进入忙等后的端口访问，不能替代这次初始化阶段的 selector/response 关联。

### 3.34 本地 SVM 初始化阶段 I/O 跟踪：仍在进入 guest 前复位

为验证 3.33 的判据，临时在 x86 guest I/O 出口处维护 CF8 selector，并只对已知 `TlsDxe` 初始化 RIP 区间 `0x1e9b9a94..0x1e9b9b6b` 记录配置地址写和数据读取；另外将 selector `0x8000f840` 标记为 PMBASE 查询。代码编译和 UEFI staging 均成功，运行命令为：

```text
set -o pipefail
timeout --signal=INT 180s cargo xtask axvisor test qemu --arch x86_64 \
  --test-group experimental --test-case uefi-disk-svm 2>&1 \
  | tee /tmp/uefi-disk-tlsdxe-init-trace-180s.log
```

本地结果如下：

```text
outer VM Load:              52
UEFI application started:  26
TlsDxe init PCI samples:    0
guest PCI config samples:   0
vpic wire samples:          0
```

每轮都能看到外层 Axvisor 的内存初始化和 UEFI 应用启动，但没有进入 `Starting virtualization`、`Creating VM` 或 guest vCPU 运行阶段；因此本轮没有产生有效的 `CF8/CFC` 证据。诊断日志代码没有改变 `0x8` 循环的判断，后续应直接在稳定的 CI/KVM runner 重跑同一版本，不能把“没有样本”解释成 `TlsDxe` 没有执行 PCI 访问。

### 3.35 诊断代码的构建验证与 CI 判读规则

本轮新增的 CF8 selector、CFC 返回值和 `TlsDxe` 初始化 RIP 跟踪代码已执行 `cargo fmt --all` 和 `git diff --check`。第一次 `cargo xtask clippy --package axvm` 仅因 `checked_add(...).map_or(...)` 触发 `clippy::unnecessary_map_or` 失败，改用 `is_none_or` 后重新执行，7 个 feature 检查全部通过。该代码仍是实验性诊断 instrumentation，不改变 vPIC wire mode，也不改变端口设备返回逻辑。

下一次在稳定 CI/KVM runner 获取日志后，按以下顺序判读：

1. 若出现 `TlsDxe init PCI address write`，确认其 selector 是否为 `0x8000f840`。
2. 若紧随其后出现 `TlsDxe init PCI data read` 且 value 为 `0x0`，说明 Q35 LPC register `0x40` 的 CFC 返回路径仍是首要嫌疑。
3. 若 value 为 `0x601`，但后续计时端口变量仍为 `0x8`，应检查 host bridge/device-ID 分支、PCI-express 判定或间接写入，而不是继续修改 PM timer。
4. 若 `TlsDxe init PCI` 样本仍为零，同时外层日志显示没有进入 `Creating VM`，则只能说明 runner 在 guest 启动前复位或退出，不能据此判断 CFC 行为。

### 3.36 静态分支修正：Q35 不一定因缺少 MCFG 回退到 CF8/CFC

继续检查 `TlsDxe` 的 PE entrypoint `RVA 0x63a94` 后，发现先前对“没有 ECAM 就回退 CF8/CFC”的描述不适用于这段初始化代码。入口处的实际分支是：

```text
call 0x5e376                 # 获取 host bridge device ID
cmp ax, 0x29c0               # Q35
sete byte [0x9c844]          # Q35/PCIe 路径标志
...
cmp ax, 0x29c0
je  0x63b07                  # ECAM offset = 0xf8040
...
cmp byte [0x9c844], 0
je  0x63b21                  # 只有标志为假才走 CF8/CFC
mov eax, 0xb0000000
add rcx, rax                 # Q35 PCIe 配置窗口
mov ecx, [rcx]
...
0x63b21: in eax, dx          # legacy CF8/CFC branch
```

也就是说，`TlsDxe` 的选择条件是识别到的 Q35/PCIe 平台标志，而不是“ACPI 中有没有 MCFG”。当前 Axvisor guest 的 PCI host 实现提供的是 CF8/CFC 和普通 PCI BAR aperture `0xc0000000..0xd0000000`；没有看到为 Q35 PCIe 配置窗口 `0xb0000000` 提供 ECAM device。若该固件分支在 guest 中确实执行，理论上应通过 NPF/页表缺口或 MMIO 处理路径留下 `0xb00f8040` 附近的证据，而不是出现在 CF8/CFC 日志中。

这个发现解释了此前成功 guest 运行中“普通固件代码能通过 CF8/CFC 读到 `00:1f.0` 的 `0x601`，但没有出现 `TlsDxe init PCI` 样本”的表面矛盾：前者只能证明其他代码访问 legacy configuration mechanism，不能证明 `TlsDxe` 初始化选择了同一路径。下一次 NPF 诊断将输出前 128 次 fault 的 GPA、访问类型、RIP 和指令字节，并在 PAUSE aggregate 中附带累计 NPF 数，以区分 Q35 PCIe 地址缺失和初始化代码未执行。

### 3.37 本地 SVM 重跑：仍在 guest 创建前被 WSL 环境复位

在加入 NPF 详细采样前，使用当前 CF8/CFC 初始化跟踪重跑：

```text
set -o pipefail
timeout --signal=INT 120s cargo xtask axvisor test qemu \
  --arch x86_64 --test-group experimental --test-case uefi-disk-svm 2>&1 \
  | tee /tmp/uefi-disk-tlsdxe-init-trace-rerun-120s.log
```

清理 ANSI 控制字符后统计：

```text
VM Load:                    32
UEFI application started:   16
SMP secondary 1: entered:   15
Starting virtualization:     0
Creating VM[:                0
VM[1] boot success:           0
TlsDxe init PCI:              0
vpic wire mode:               0
guest PCI config response:    0
SVM NPF sample:               0
SVM PAUSE aggregate:          0
```

命令以外层 `timeout` 的退出码 `124` 结束。该轮可以确认构建和 guest staging 成功，但所有有效轮次都停在外层 Axvisor 的 UEFI 应用启动/次级 CPU 阶段，尚未打印 `Starting virtualization`，因此没有创建 guest，也没有产生任何 guest I/O、NPF 或 vPIC 证据。这是 WSL nested SVM 的外层复位/挂起，不是本次 UEFI guest 卡在 `TlsDxe` 的结果。

与之相对，早先真正进入 guest 的 `/tmp/uefi-disk-tlsdxe-io-trace-120s.log` 记录了 `Creating VM[1]`、`VM[1] boot success`、`vpic wire mode`、`CFC=0x601` 和 `RIP=0x1e9b1e98` 的 `port=0x8` 循环。因此诊断代码已经有可用的 guest 运行路径，但当前本地重跑不能替代稳定 CI/KVM runner；尤其不能把本轮 `TlsDxe init PCI=0` 解读为 Q35 初始化没有 PCI 访问。

### 3.38 增加 NPF 详细采样后的本地重跑：仍未到达 guest

针对 3.36 提出的 Q35 PCIe 配置窗口假设，在 SVM 的 nested page fault 路径增加了有界采样：前 128 次 NPF 记录 GPA、访问类型、guest RIP、是否能解码为设备 MMIO 以及指令附近的 guest 字节；`PAUSE aggregate` 也附带累计 NPF 数。这样一旦 guest 真正运行，就可以直接区分访问 `0xb00f8040` 一类 Q35 PCIe 配置地址，还是根本没有走到该初始化分支。

执行命令如下：

```text
set -o pipefail
timeout --signal=INT 120s cargo xtask axvisor test qemu \
  --arch x86_64 --test-group experimental --test-case uefi-disk-svm 2>&1 \
  | tee /tmp/uefi-disk-npf-trace-rerun-120s.log
```

本轮构建完成并成功把磁盘镜像 staging 到外层 rootfs，但 WSL nested SVM 在 guest 创建前再次复位/重复启动 UEFI。清理 ANSI 控制字符后的计数为：

```text
VM Load:                    28
UEFI application started:   14
SMP secondary 1: entered:   13
Starting virtualization:     0
Creating VM[:                0
VM[1] boot success:           0
TlsDxe init PCI:              0
vpic wire mode:               0
guest PCI config response:    0
SVM NPF sample:               0
SVM PAUSE aggregate:          0
guest diagnostic port sample: 0
```

因此本轮没有获得 guest NPF、CF8/CFC、PCI BAR 或 vPIC 运行时证据；它只能再次确认本地 WSL 失败发生在 `Starting virtualization` 之前，不能说明 Q35 PCIe 配置窗口是否真的被 guest 访问。NPF 采样本身已经编译通过，下一次在稳定 CI/KVM runner 运行时，首要判据是是否出现接近 `0xb00f8040` 的 GPA；若出现，应优先补齐与 guest 拓扑一致的 ECAM/Q35 PCIe 配置窗口，若没有，则继续沿 `TlsDxe` 入口分支或变量早期写入来源排查。

从当前代码链路看，`decode_npt_mmio_access()` 只会把 local APIC、IOAPIC 和已注册的设备 MMIO（当前 VirtIO BAR aperture 在 `0xc0000000` 范围）解码为可模拟的 MMIO exit；未注册的 `0xb00f8040` 不会被当作普通设备读直接返回。该地址若真的被 guest 访问，应先形成 `SVM NPF sample`，随后进入 Axvisor 的 nested page fault 处理；由于 guest 地址空间当前没有覆盖 `0xb0000000`，预期还会出现 unhandled NPF，而不是静默地得到一个合法 PCI 配置值。这使 NPF 日志成为区分“走了 Q35 PCIe 分支但 Axvisor 没有 ECAM”与“根本没有执行该入口”的直接判据。

### 3.39 VMX 侧补齐同等 EPT 采样：本地缺少 KVM，暂未运行

由于持续集成矩阵同时覆盖 VMX 和 SVM，将 VMX 的 EPT violation 日志从幂次采样扩展为前 128 次采样，并增加 guest RIP 对应的 16 字节指令内容。这样即使 Q35 PCIe 配置窗口只触发一次 EPT violation，也能保留完整的 GPA、访问权限、是否已被 MMIO decoder 接管以及 faulting instruction 证据。

本次修改后执行了：

```text
cargo fmt --all
cargo xtask clippy --package x86_vcpu
```

base 与 `tracing` 两个检查均通过。当前工作环境的 `/dev/kvm` 不存在或不可读写（检查结果为失败），因此不能在本地重新运行 `uefi-disk-vmx` 或 `uefi-disk-svm` 获取 guest 级样本。下一次 CI/KVM 运行时，应同时检查：

- VMX 的 `VMX EPT violation sample` 是否出现 `gpa=0xb00f8040`；
- SVM 的 `SVM NPF sample` 是否出现同一 GPA；
- 若出现，`decoded_mmio=false` 且随后有 unhandled nested page fault，说明当前确实缺少 Q35 PCIe 配置窗口；
- 若两者均没有该 GPA，则继续追踪 `TlsDxe` 是否执行到 Q35 分支，以及 `0x1e9f1fa8` 的早期写入来源。

本轮还只读检查了远端 fork 的 CI 状态：`YoungCIoud/tgoskits` 的 `exp/uefi` 当前指向 `1f318780`，该分支没有 GitHub Actions run，也没有 check run。工作区本轮新增的 VMX/SVM NPF 采样尚未提交到远端，因此当前没有可供分析的远端 guest 日志；不能把远端“无运行记录”解释成测试通过或没有 NPF。

### 3.40 远端 CI 暴露出诊断代码自身的 secondary CPU 复位

提交 `f7a9324ca` 推送到上游 PR #2275 后，VMX（Intel/KVM）和 SVM（AMD/KVM）两个 job 的镜像准备阶段均通过，并进入 `Run UEFI guest test`。用户提供的运行日志在外层 Axvisor 启动过程中只出现了一次 `SMP secondary 1: entered rust_main_secondary`，没有出现 `per-CPU state initialized`、`early platform init complete`、`Starting virtualization`、`Creating VM[1]` 或 guest 级 EPT/NPF 日志；重复的 ArceOS 启动画面说明该阶段发生了重新启动，而不是 guest 已经进入 `TlsDxe` 循环。

对照提交差异后确认，`log_secondary_percpu_binding()` 是该提交新增的诊断函数，并且在 `rust_main_secondary()` 中位于 `ax_hal::percpu::init_secondary(cpu_id)` 之前。函数内部调用了 `unsafe { ax_percpu::with_cpu_pin(ax_percpu::current_area) }`。`cpu-local` 的契约要求当前 CPU area 已安装后才能创建 `CpuPin`；x86_64 的实现会从 GS 基址读取 CPU-local 状态，而这个基址正是在后续的 secondary CPU 初始化阶段安装的。因此这段 precheck 的调用时机不满足其 unsafe 前置条件，足以解释 secondary 入口后的早期复位，并不能作为 guest 卡死证据。

本轮已撤掉该 precheck 和调用点，保留 `entered rust_main_secondary`、`per-CPU state initialized`、`early platform init complete` 三个不读取未安装 CPU-local 状态的边界日志。修正后的下一轮 CI 必须先证明外层 SMP 初始化完成并出现 `Starting virtualization`，之后获得的 vPIC、PCI、EPT/NPF 和 VirtIO 统计才可用于继续分析 guest 启动问题。

### 3.41 为外层复位增加单次运行保护

上一轮远端运行会在 QEMU 内部重复执行完整的外层 UEFI 启动流程，导致 case 一直占用 runner，且同一份启动前日志被重复输出。为了让下一轮证据对应第一次故障，在实验 VMX/SVM 的 QEMU 配置中加入 `-no-reboot`，并启用 `-d cpu_reset,guest_errors`。前者使第一次 guest 或 host reset 后 QEMU 退出，后者把 QEMU 观察到的 CPU reset 和非法 guest 访问写入测试输出；两者都不改变 Axvisor 的 guest 设备模型。

配置入口为 `test-suit/axvisor/experimental/qemu-uefi-disk/uefi-disk/qemu-x86_64-vmx.toml` 和 `qemu-x86_64-svm.toml`。这项保护的判定是：若仍发生外层复位，CI 应在一次启动后结束并保留 reset 证据；若能够进入 guest，则应继续观察 `Creating VM[1]`、`VM[1] boot success`、PCI/EPT 或 NPF 样本以及最终 guest marker。`cargo xtask axvisor test qemu --list --arch x86_64 --test-group experimental` 已确认两个实验 case 仍能被发现。

## 4. 当前结论

### 4.1 已证实内容

当前实现确实是“传统 PCI 配置机制 #1”，即 guest 通过 `out 0xcf8` 选择 BDF/register，再通过 `in/out 0xcfc..0xcff` 访问配置数据；没有 ECAM。对当前 OVMF 构建而言，CF8/CFC 不是运行时根据 MCFG 缺失临时选择的 fallback，而是 `PciHostBridgeDxe` 使用的固定 `PciSegmentLib/PciLib` 后端。标准 QEMU 的 `acpi=off` 对照也证明，OVMF 可以在没有 ACPI/MCFG 时直接使用 configuration type 1。

Axvisor 已经把 VirtIO endpoint 放入 PCI 拓扑，且 CF8/CFC frontend 支持 absent function 返回全 1、endpoint 配置和后续 BAR 路由。此前若干轮 UEFI 磁盘用例和直接 Linux PCI 对照没有在进入端口循环前访问到 `00:01.0`；最新直接 Linux 轮次已经通过 CF8/CFC 读取了 `00:01.0` register `0x02`，返回 device ID `0x1110`，但还没有继续到 PCI aperture/BAR 访问。因此“没有 endpoint 配置日志”不能作为当前所有轮次的结论；更准确的说法是，guest 控制流存在轮次差异，尚未得到一个能完成 endpoint 初始化并继续到 BAR/设备功能访问的 Axvisor guest 基线。

当前实验已经实际观察到 `00:00.0`、`00:1f.0` 以及最新一轮的 `00:01.0` 通过 `CF8/CFC` 访问，包括读取 Q35 host bridge DID、LPC 的 PMBASE/ACPI enable 状态和 endpoint device ID；所以“没有 ECAM”并不等于“guest 完全无法访问 PCI”。标准 QEMU 的无 ACPI 对照进一步证明 `MCFG/ECAM` 不是 OVMF 枚举 VirtIO 的必要条件。当前更具体的差异是：Axvisor 的不同 guest 启动轮次会在 PCI 探测前后进入 legacy PIC IRQ0 处理路径；最新 direct Linux 轮次虽已读到 endpoint ID，但没有继续产生 BAR 访问，因此仍需先确定 IRQ0 处理的触发频率和控制流影响，再比较 BAR、command 或设备拓扑。

最新的 PIC 状态跟踪还确认了 master PIC 的 vector、mask、IRR/request、ISR/in-service 以及 specific EOI 的基本转换是连贯的；至少当前日志没有显示一个“EOI 未清 ISR”或“mask 读取值错误”的简单 vPIC 故障。PIT 在 mask 期间留下的 IRQ0 request 会在后续 PIC 写操作后重新被 claim，这解释了写后 dispatcher 中重复出现 vector `0x30` 的来源。最新一轮还捕捉到了 `00:01.0` register `0x02` 的 endpoint ID 读取，因此剩余问题应描述为“反复 IRQ0 处理阻塞或显著延迟了后续枚举/BAR 访问”，而不是绝对断言 guest 从未访问 endpoint。

### 4.2 当前最强假设

3.30 的全端口跟踪已经排除了“当前 `TlsDxe` 计时循环前刚刚执行了 CF8/CFC 读取”这一直接路径。3.33 的离线反汇编已经确认该变量的 Q35 初始化写入逻辑，以及 legacy 分支中 `CFC=0` 会产生 `0x8` 的精确条件；3.36 又确认 Q35/PCIe 标志可能让这段代码绕过 CF8/CFC，改从 `0xb0000000` 配置窗口读取。因此当前最强的运行时未决问题是：初始化阶段到底命中了 Q35 PCIe MMIO 分支还是 legacy CF8/CFC 分支；前者要看是否出现 `0xb00f8040` 附近的 NPF，后者才需要把 selector `0x8000f840` 的 `CFC` 返回值与 `TlsDxe` caller RIP 对齐。

最强的运行时证据是：guest 在 `0x608` PM timer 读取阶段之后，进入了对未映射端口 `0x8` 的定时器样式循环。运行时第一次读取处的指令以 `in eax,dx` 开始，后面还包含对 guest 内存的写入、再次 `in eax,dx`、从内存加载端口号以及按 bit 23 忙等；它不是从磁盘提取出的 `BOOTX64.EFI` 主 `.text` 中 `grub_pmtimer_wait_count_tsc` 的字节序列。因此这里应称为 `TlsDxe` 的运行时计时路径；运行时 PE 已由 3.20 的唯一 firmware-volume 对照确认是 `TlsDxe`。尚未确定的是这段初始化实际收到的 CFC 返回值，或端口变量是否随后被覆盖。

运行时快照、LPC 读值、短跟踪和 RIP-relative dword 读取共同证明：guest 可见的两条 ACPI 计时器发布路径都指向 `0x608`，但实际循环使用的端口变量在 guest 内存中明确为 `0x8`，并依赖该未映射端口返回的 `0xffffffff`。同一镜像在标准 QEMU+OVMF 下可以启动到 Linux shell，因此该分支由 Axvisor 虚拟硬件交互触发。运行时 PE 已通过 firmware volume 中的唯一 PE/FFS 对照确认是 `TlsDxe`；静态入口代码会从 Q35 LPC 配置读取 PMBASE 后计算 `PMBASE + 8`，而运行时变量却是 `0x8`。当前首要问题是确定这次 `TlsDxe` 初始化实际收到的 CFC 响应，或确认该变量是否在之后被覆盖。暂时不应通过把 `0x8` 伪装成 PM timer、实现 ECAM 或修改 vPIC 来掩盖这个分支条件。

临时别名实验进一步验证了第一句中的因果关系：给 `0x8` 提供实时计时值后，原忙等循环确实可以离开；但后续仍有另一运行时 PE 从 `0x8` 读取，且 guest 最终进入 `HLT`、没有访问 VirtIO endpoint。因此还不能把根因简化为“缺少一个 `0x8 -> 0x608` 映射”，真正缺失的是触发这些路径的固件/启动加载器运行时配置或兼容行为。

直接 Linux 对照中的 PIC 状态日志没有发现 PM timer 参与该阶段：此时 `pm_timer=0`，而 PIC 已完成重编程并处理过一次 IRQ0/EOI。最新的写入值关联还表明，写后 dispatch 是 guest 写 `0x21=0xfe`、重新解除 IRQ0 屏蔽后对 pending request 的正常重新检查；不能把这组重复投递直接归因于 EOI 或 vPIC 状态损坏。指令级采样进一步确认 guest 反复执行 IRQ0 的 PIC 确认片段，且最新轮次已经读到了 `00:01.0` 的 endpoint ID。当前应将“PM timer 没有递增”和“IRQ0 处理频率/重入节奏异常”分开验证，下一步转向 PIT reload/callback 频率。

### 4.3 最新基线结论

关闭 `guest_image_signatures` 扫描后，page fault 消失而 `vpic wire mode change to LAPIC, unimplemented` 仍出现，说明前者是诊断代码造成的假故障，后者虽是待补齐的兼容性缺口，却不是当前唯一或直接的退出原因。PM timer 已确认在 `0x608` 正常递增；当前 guest 实际长时间执行的是 `RIP=0x1e9b1e98` 上对未映射端口 `0x8` 的读取/忙等，并持续触发 PIT IRQ0、legacy PIC EOI 和 vLAPIC 中断接收。下一轮应围绕 `0x8` 的运行时来源及其与 PCI/ACPI 初始化返回值的关系继续取证，暂不实现 vPIC wire mode，也不把 page fault 修复方向放在 rootfs 或 ECAM 上。

变量页写监视实验没有产生 guest 级证据，反而暴露出 nested paging 权限修改会错误地把 guest GPA 交给宿主 `INVLPG`；该实验引起的外层复位与 guest 的 `0x8` 循环无关，不能混入基线结论。最新 NPF 详细采样轮次同样在 guest 创建前复位，所以没有改变这一边界。

远端 CI 的首轮诊断提交又发现了一个独立的外层问题：secondary CPU 在 `init_secondary()` 前被诊断代码提前读取 CPU-local GS 状态，导致测试可能在 guest 创建前反复复位。该段代码已从后续诊断提交中撤销；在修正后的 CI 运行完成前，不能把“没有 guest NPF”解释成没有 Q35 PCIe 配置窗口，也不能把重复启动归因于 PM timer、vPIC wire mode 或 VirtIO。

## 5. 后续实验

### 5.1 已完成：验证 ACPI 表的运行时内容

已在首次 `0x8` 访问时读取并记录 guest 内存中的 direct ACPI 表。结果确认：

- RSDP 位于 `DIRECT_ACPI_BASE`，XSDT 指向 `FACP`；
- FADT 的传统 PM timer 字段和 extended GAS 都是 `0x608`；
- 同时观察到 LPC `0x40 = 0x601`、`0x44 = 0x80`，按 OVMF 的 PCI BAR 推导逻辑也应得到 `0x608`；
- 已通过运行时 PE section、directory 和 OVMF firmware volume 的唯一 FFS/PE 对照定位到 `TlsDxe`；其运行时 image base 为 `0x1e956000`，RVA `0x5be98` 的代码字节与 OVMF CODE 中的静态映像完全一致。

### 5.2 找到端口 `0x8` 的代码来源

已完成端口 I/O 短跟踪、RIP-relative 变量读取、标准 QEMU+OVMF 对照、运行时 PE section/directory 判定、端口别名因果实验，以及直接 Linux `pci-enumeration-svm` 对照；代码页 `0x1e9b1e98` 与变量页 `0x1e9f1fa8` 均已确认可通过 guest 页表读取，变量值为 `0x8`，运行时 PE 基址为 `0x1e956000`，当前 RIP 位于其 `.text` section。该 PE 已与 OVMF firmware volume 中唯一匹配的 FFS/PE 对上，确认是 `TlsDxe`；其静态入口会经 CF8/CFC 读取 Q35 LPC PMBASE。临时给 `0x8` 返回实时 PM timer 后，guest 能离开第一处循环，但在另一 PE 的第二处 `0x8` 计时循环后进入 `HLT`，仍未访问 VirtIO endpoint。标准 QEMU 在有 ACPI 和无 ACPI 两种对照中都能通过传统 PCI configuration type 1 枚举 VirtIO 并启动到 Linux；Axvisor 的直接 Linux PCI 对照在不同轮次表现不同，最新轮次已通过 CF8/CFC 读取 `00:01.0` register `0x02`，但随后反复执行 legacy PIC IRQ0 处理，尚未出现 PCI BAR 访问或成功标记。因此下一步应在稳定 runner 上采集 `TlsDxe` 入口阶段带 guest RIP 的 CF8/CFC 响应序列，确认 `0x8000f840` 的 CFC 读值是否为零，同时继续比较 VirtIO BAR/command 初始化、Q35 设备拓扑，以及运行时模块是否因为这些差异走了不同路径。

本轮 PIC 状态跟踪已补充 master/slave 的 mask、request、in-service 和初始化阶段，并把 PIT 首次注入与 PIC 写后重新 dispatch 分开计数。进一步关联写入值后，确认重复的写后 claim 均由 guest 写 `0x21=0xfe` 触发，且此前确有 `request=1`；指令级采样确认这些访问来自 IRQ0 的 PIC 确认片段，PIC 初始化和 EOI 基本符合预期，不能再把重复 vector `0x30` 直接等同于 vPIC 状态损坏。PIT reload/callback 也已在实际 UEFI 轮次确认正常。3.30 已完成 `TlsDxe` 全端口 I/O 关联；后续应定位 `0x1e9f1fa8` 的早期写入来源，再继续比较 VirtIO BAR/command 初始化、Q35 设备拓扑，以及运行时模块是否因为这些差异走了不同路径。

3.33 已补充离线 OVMF 证据：`TlsDxe` 的该变量只有两处直接写入，legacy 分支中 Q35 的 `CF8=0x8000f840`/`CFC` 返回零会精确地产生 `0x8`；3.36 说明 Q35/PCIe 标志也可能使初始化改走 `0xb0000000` 配置窗口。因此下一次运行时实验应先采集 NPF，判断是否访问 `0xb00f8040`；只有确认走 legacy 分支后，才把 `TlsDxe` 初始化阶段的 CFC 返回值作为首要判据。

3.31 的变量页写监视没有到达 guest，原因是诊断路径错误地对 guest GPA 执行了宿主 `INVLPG`，并已撤销相关临时代码。后续如重新采用写监视，需要先提供不使用宿主线性页表失效语义的 EPT/NPT 专用测试路径。

3.32 的 guest 启动前变量快照也没有采到有效值：本地 SVM 在 VM 创建前反复复位，快照代码已撤销。这个问题需要在稳定 runner 上重做，不能把本地复位轮次解释成变量初始化结果。

### 5.3 完成一个可复核的终止条件

在 WSL 本地实验中继续使用外层 `timeout`，但把成功判据拆开记录：固件是否完成 VirtIO endpoint 枚举、是否访问 BAR0、VirtIO 队列是否收到磁盘读请求、Linux 是否最终输出 `/dev/vda` 检查结果。只有这些事件按顺序出现，才能判断 UEFI 磁盘启动链路真正打通。
