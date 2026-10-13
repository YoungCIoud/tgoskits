# X86_64 Linux 客户机 UEFI 启动链路推进路线

> 文档性质：工作路线规划（非设计基准）。记录在只读盘（`uefi/f2-0814` 的 virtio-blk-pci）合并周期之外、可以独立推进的工作项，并逐项说明其对推进 x86_64 Linux 经 UEFI（客户机内嵌 OVMF）启动的作用。
>
> 基准快照：`uefi/f2-0814` 本地 HEAD `7e33699c7`；`upstream/dev` `3a3af5167`（本分支落后 dev 23 个提交）。dev 上 `virtualization/x86_vlapic`、`virtualization/x86_vcpu` 无任何改动，axvisor x86 测试矩阵仍全部 `cpu_num = 1`。
>
> 现状基线：单 vCPU、内核/initrd/cmdline 走 fw_cfg 的客户机 OVMF 链路已端到端自洽并通过 VMX/SVM 验证（`test-suit/axvisor/normal/qemu-acpi-ovmf/`，成功 marker `AXVISOR_X86_OVMF_ACPI_PASSED`）；F2 只读 virtio-blk-pci 数据盘仅在 direct-boot 路径 E2E 通过。

## 0. 缺口总览与依赖关系

```text
标准 PC UEFI 流程（OVMF 从 virtio-blk 启动盘引导完整 Linux）
  ├─ [依赖只读盘] virtio-blk-pci 接入 OVMF 枚举 + ESP(GPT/FAT) 启动盘镜像   ← 等待盘合并
  ├─ [独立] ① 客户机 SMP（INIT-SIPI / AP-hold）
  ├─ [独立] ② fw_cfg typed 内存图（etc/e820）
  ├─ [独立] ③ SMBIOS 真实表
  ├─ [独立] ④ 可写 EFI 变量存储（VARS/varstore）
  └─ [弱依赖 dev 重构] ⑤ PCI0 _CRS 通用 aperture
```

只读盘合并后，UEFI 从盘启动 = ⑤ + 盘接入 OVMF；而 ①②③④ 与盘完全正交，现在即可开工。

### 0.1 并行开发可行性（相互独立性分析）

| 组合 | 文件重叠 | 语义耦合 | 结论 |
| --- | --- | --- | --- |
| ① × ② | 无 | 无 | 完全独立 |
| ① × ③ | 无 | 无 | 完全独立 |
| ① × ④ | `axvm/src/arch/x86_64/mod.rs` 不同区段（exit dispatch vs 内存/设备注册）；都可能动 `qemu-acpi-ovmf` 用例资产 | ① 需修改 initramfs `online == "0"` 断言，④ 不涉及 | 可并行，仅低风险文本冲突 |
| ② × ③ | 同文件 `fw_cfg/data.rs` + `tests.rs`（memmap 与 smbios 为不同函数） | 无 | 合并为一个 PR 或严格排序 |
| ② × ④ | 无直接文件冲突 | **有依赖**：VARS 成为可写 trap 区后必须在 e820 标 reserved，否则 OVMF 视为可用 RAM | ② 先合，④ 跟进 |
| ③ × ④ | 无 | 无 | 完全独立 |
| ⑤ × 其余 | — | — | 冻结，等 dev #2138 重构落地 |

补充约束：

1. **共同移动基线**：所有分支从 `upstream/dev` 拉出；dev 的"统一虚拟设备注册"（#2138）会给各 `DeviceModel` 增加 `firmware()` 方法，与 ① 所在的 `arch/x86_64/mod.rs` 同文件不同区段，各分支提交前必须 rebase 最新 dev。
2. **验证资源串行**：E2E 依赖 self-hosted KVM runner（本地无 `/dev/kvm`），并行项在 CI 排队上互斥，属调度约束而非代码冲突。

---

## ① 客户机 x86 SMP：INIT-SIPI / AP-hold（首选）

**现状**：`x86_vlapic/src/vlapic.rs::process_init_sipi` 是打日志的 no-op（"ignoring ... in current single-vCPU bring-up path"）；所有 vCPU 同时从同一入口起跑——direct 路径下每个 vCPU 都执行 boot stub，UEFI 路径下每个 vCPU 都执行 OVMF 复位向量。x86 没有 CPU-up exit 流程（RISC-V SBI HVC / AArch64 PSCI 都已接入 `architecture/cpu_up.rs` 的 `CpuUpExit { target_cpu, entry_point, arg }` + `CpuUpOps` 抽象）。

**对推进 UEFI 启动的作用**：
- 这是 UEFI 与 direct 两条路径**共同**的缺口。没有它，MADT/fw_cfg 声明的多 CPU 永远无法上线，Linux guest 只能单核运行；任何接近真实部署形态的 UEFI guest（systemd 并行启动、IRQ 分布、per-CPU 子系统）都被阻塞。
- 固件侧已经就绪（MADT 按 `cpu_num` 枚举 LAPIC、fw_cfg `NB_CPUS/MAX_CPUS` 已接通），本项是纯粹的 vCPU 语义补全，做完即同时解锁两条路径的多核。
- 也是后续"多 vCPU + 从盘启动"组合用例的前置件。

**落点**：
1. `virtualization/x86_vlapic/src/vlapic.rs`：实现 INIT（AP 复位进 wait-for-SIPI）与 SIPI（delivery mode 6 → 产出 CPU-up 事件，entry = vector << 12）；ICR 投递状态机补齐。
2. `virtualization/axvm/src/arch/x86_64/mod.rs`：exit dispatcher 增加 CPU-up 分支；为 `X86_64Arch` 实现 `CpuUpOps`（AP vCPU 复位到实模式入口）。
3. Axvisor 侧确认 BSP/AP 入口区分（现 `bsp_entry/ap_entry` 已分离，UEFI 路径二者同为复位向量是符合预期的，INIT 后 AP 才走 SIPI 向量）。

**验证**：vlapic ICR INIT/SIPI host-test 单测；新增 `cpu_num=2` direct-boot Linux 用例断言 `/sys/devices/system/cpu/online` 含 CPU1（需 self-hosted KVM runner）；随后扩展 ovmf-acpi 用例为双 vCPU。

**独立性**：`x86_vlapic`/`x86_vcpu` 在 F2 分支与 dev 上均零改动，冲突面为零。

---

## ② fw_cfg typed 内存图（`etc/e820`）

**现状**：`axdevice/src/fw_cfg/data.rs::build_memmap` 生成的 `etc/memmap` 全部是 type-1（RAM）条目，无 reserved / ACPI-reclaim / NVS 类型；且上游 QEMU/OVMF（PlatformInitLib `PlatformScanE820`）消费的文件名是 **`etc/e820`**。

**对推进 UEFI 启动的作用**：
- OVMF 用 e820 划分它自己的内存池、ACPI reclaim 区和 reserved 区；当前靠 OVMF 自行探测回退才通过。一旦内存布局变复杂（固件区、设备窗口、后续 VARS trap 区），RAM-only 图会让 OVMF 把不可写区域当可用内存，出现隐蔽的内存破坏。
- typed 内存图是从"最小能跑"走向"标准固件输入"的地基，也是 ④（可写 VARS）声明保留区间的载体。

**落点**：`fw_cfg/data.rs` 类型化条目 + 数据源从 resolved plan 取（RAM 之外补 reserved/reclaim）；文件名补 `etc/e820`（是否保留 `etc/memmap` 别名先核实现网行为再定）。

**验证**：host-test 断言条目类型与区间；ovmf-acpi/direct-acpi 两用例不回归。

**风险提示**：当前 ovmf-acpi 能通过说明存在回退路径，改动前先用现有用例锁定行为基线（先加回归，再改实现）。

---

## ③ SMBIOS 真实表

**现状**：`build_smbios_tables()` 只输出 type-127 end-of-table + 裸 `_SM3_` anchor（`fw_cfg/data.rs`），无 Type 0(BIOS)/1(System)/3(Chassis)/4(Processor) 结构。

**对推进 UEFI 启动的作用**：
- `etc/smbios/smbios-anchor|tables` 通道已具备，OVMF SmbiosPlatformDxe 直接消费；真实表让 guest `dmidecode`/`/sys/class/dmi` 得到正确的厂商、机型与按 `cpu_num` 的处理器描述。
- 对启动本身非必需，但它是 UEFI guest"像一台标准 PC"的外围完整性项，成本极低（纯数据生成 + host tests），适合作为独立小 PR 穿插推进。

**落点**：`axdevice/src/fw_cfg/data.rs` 补 Type0/1/3/4；Type4 条目数跟随 `cpu_num`。

**验证**：结构校验单测（checksum/长度/句柄引用）；guest 内 `dmidecode -t` 输出断言（可并入现有 busybox initramfs 或单独小用例）。

---

## ④ 可写 EFI 变量存储（VARS/varstore）

**现状**：`scripts/axbuild/src/axvisor/test/ovmf.rs::assemble_ovmf_image` 把 VARS 作为一次性前缀固化进 4 MiB `OVMF_CODE_4M.fd` 加载到 guest `0xffc00000`；guest 对该区的写入不落盘、重启即失。无独立可写 varstore、无 flash write-back。

**对推进 UEFI 启动的作用**：
- 标准 UEFI 流程的可持久状态全部住在 varstore 里：BootOrder/Boot####、SecureBoot、OVMF 的首次运行初始化等。没有可写 VARS，每次启动都从出厂镜像重来，无法承载"装机一次、多次引导"的真实用法。
- 这是从"fw_cfg 直启内核"过渡到"标准 UEFI 引导流程"（含 ⑤ 之后 OVMF 从盘启动）的必要基础设施。

**落点**：
1. axvm/axvmconfig：把固件区拆成 CODE(ROM) + VARS(可写 MMIO) 两个映射区间；
2. Axvisor 运行时：对 VARS GPA 区间做写 trap + 宿主后备存储（进程内存起步，文件持久化可选），等效 pflash 语义；
3. `ovmf.rs` 组装逻辑同步拆分（CODE/VARS 分开提供）。

**验证**：host-test 写陷阱/回读/复位语义；ovmf-acpi 回归 + 新增跨重启变量留存断言（同 QEMU 进程内 reset 即可验证）。

**排序建议**：放在 ① 之后——它引入新的内存区域划分，最好等 typed 内存图（②）就位后一并声明保留。

---

## ⑤ PCI0 `_CRS` 通用 aperture —— 缓办，等 dev 重构落地

**现状**：有 endpoint 时 PCI0 `_CRS` 只含总线号 walk + 单个 BAR0 窗口（`axvm/src/arch/x86_64/boot/acpi/aml.rs`），无 I/O aperture、无通用总线内存 aperture；endpoint 数量被 firmware plan 硬限为一个。

**对推进 UEFI 启动的作用**：
- OVMF 的 PCI 总线枚举依赖 `_CRS` 里的 I/O + memory aperture 给 BAR 分配资源。只有 BAR0 单窗口时，OVMF 要么分配失败要么绕过；这是"virtio-blk-pci 被 OVMF 枚举并挂上 VirtioBlkDxe"的硬前置。

**为什么缓办**：`upstream/dev` #2138 正在把设备 ACPI 描述迁往 `DeviceFirmwareSpec` / resolved contributions 统一模型（`boot/acpi/config.rs` +285 行，IOAPIC/PIT/PIC/CMOS/fw_cfg 已迁移）。现在在本分支做必然返工；应等 rebase 到 dev 后基于新结构实现。

---

## 明确不做 / 低优先级

| 项 | 结论 | 原因 |
| --- | --- | --- |
| MCFG/ECAM | 不做 | PCI 配置固定 CF8/CFC conventional mechanism，无 ECAM 硬件 |
| HPET | 低优先级 | LAPIC timer + PM timer 已满足 Linux 时钟树 |
| x86 SRAT | 不做 | 单节点无 NUMA 需求 |
| MSI/MSI-X | 不做 | 属于后续中断完整性阶段，INTx 已够单盘 MVP |

## 工作方式

不切换当前分支，从 `upstream/dev` 拉 worktree 开新分支：

```bash
git worktree add ../tgoskits-smp upstream/dev -b feat/x86-guest-smp
```

**推荐顺序**：

1. ① SMP（主菜，一个完整 feature PR，含 host-test 与 cpu_num=2 E2E）
2. ② e820 与 ③ SMBIOS（各自小而独立的 PR，可穿插）
3. ④ 可写 VARS
4. ⑤ 等 F2 合并、分支 rebase 到 dev 新 ACPI 模型后，与"盘接入 OVMF + ESP 镜像"一起构成 UEFI 从盘阶段

每项均遵守项目规范：修 bug 先加必然失败的确定性回归；改架构/平台启动契约时同步更新 `.claude/skills/arch-platform-porting/SKILL.md` 及其 references。
