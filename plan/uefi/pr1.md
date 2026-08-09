## 解决的问题

最新 `dev` 已经具备 x86_64 Axvisor guest 的 OVMF、fw_cfg 和 ACPI 启动路径，并已有 VMX `ovmf-acpi` 用例能够启动 Linux initramfs。但是现有证据还不足以稳定回答以下问题：

- 本次运行实际使用了哪一份 Ostool OVMF，最终装入 guest 的 4 MiB 镜像是否可复核；
- SVM 是否能使用与 VMX 完全相同的 guest 配置到达同一个 guest-side 成功条件；
- fw_cfg ACPI table-loader 是否生成了完整且顺序正确的 allocate、pointer 和 checksum 命令；
- `AXVISOR_X86_OVMF_ACPI_PASSED` 究竟证明了哪段启动链路，又有哪些能力仍未覆盖。

本 PR 只补齐当前实现的验证与说明，不新增 guest UEFI 产品 feature，也不延续 #1888 中固定 firmware profile、首次 fw_cfg 访问 marker 或全局设备 trace 的早期诊断方案。

## 改了什么

- 在准备 x86 guest OVMF 镜像时记录实际 Ostool CODE、VARS 和最终 guest image 的路径、字节数及 SHA-256；同时区分 `split CODE/VARS` 与 `monolithic CODE` 布局，并验证最终镜像固定为 4 MiB。
- 增加 `ovmf-acpi-svm` 用例，与 VMX 用例共用同一个 build config、guest TOML、BusyBox initramfs、OVMF 镜像和成功断言；两者只在外层 QEMU 暴露的 VMX/SVM CPU 能力上不同。
- 扩充 ACPI table-loader 最低层测试，逐项解码并验证 table/rsdp 分配、FADT/XSDT/RSDP 指针修正以及各 ACPI 表 checksum 命令的文件、偏移、范围、宽度和顺序。
- 更新 Axvisor 测试文档和 x86 UEFI 调试说明，明确运行命令、证据来源、成功条件以及当前不能据此声明的能力。

## 实现逻辑

### 复用现有强证据

`dev` 已有的 `AXVISOR_X86_OVMF_ACPI_PASSED` 来自嵌套 Linux initramfs。只有在 OVMF 完成 kernel handoff、Linux 进入早期用户态，并且 guest 能读取 DSDT、APIC、FACP、SPCR、发现 `ttyS0`、初始化 IOAPIC 且 online CPU 集合为 `0` 时才会输出。

这个判据比“OVMF 首次访问 fw_cfg”更强，因此本 PR 不在产品代码中增加新的诊断 marker，也不改变 loader、fw_cfg、ACPI、PIO/MMIO 或中断运行语义。

### 固件来源保持单一

OVMF 的版本选择、下载、缓存和完整性校验继续由 Ostool 负责。Axvisor 测试层只组装当前 guest 所需的 4 MiB 镜像，并打印本次实际输入和输出的摘要，不增加第二套 firmware profile、下载器或 manifest。

### VMX/SVM 只验证 backend 差异

两个用例不通过 Cargo feature 选择 VMX 或 SVM。Axvisor 仍根据运行时 CPUID 选择 backend；测试配置只改变外层 QEMU 的 CPU capability，从而保证结果差异可以归因于 VMX/SVM，而不是不同 guest 配置或固件资产。

### 协议错误尽量在最低层暴露

端到端 OVMF 用例证明完整启动结果；table-loader 单元测试则直接检查固件将要消费的二进制命令布局。这样在后续回归时，可以区分 ACPI loader 生成错误与固件、虚拟化 backend 或 guest OS 运行错误。

## 验证

先确认测试用例能够被 runner 发现：

```bash
cargo xtask axvisor test qemu --arch x86_64 --test-group normal --list
```

当前分支已完成以下端到端验证：

```bash
cargo xtask axvisor test qemu --arch x86_64 --test-group normal \
  --test-case smoke-vmx
cargo xtask axvisor test qemu --arch x86_64 --test-group normal \
  --test-case smoke-svm
cargo xtask axvisor test qemu --arch x86_64 --test-group normal \
  --test-case ovmf-acpi-vmx
cargo xtask axvisor test qemu --arch x86_64 --test-group normal \
  --test-case ovmf-acpi-svm
```

四个用例均通过；两个 OVMF 用例均命中：

```text
AXVISOR_X86_OVMF_ACPI_PASSED
```

VMX 用例需要 Intel/VMX KVM 宿主，SVM 用例需要 AMD/SVM KVM 宿主。构建前还可以单独检查 Ostool 当前选择的固件路径：

```bash
cargo xtask ovmf --arch x86_64
```

## 已知限制

两个 OVMF 用例通过时仍观察到以下日志：

```text
vpic wire mode change to LAPIC, unimplemented
vpic wire mode change to NULL, unimplemented
VM wire mode should be changed to INTR here, unimplemented
```

这些日志表明 VM 级 legacy PIC 输出在 `INTR`、`LAPIC` 和 `NULL` 之间的路由切换尚未实现。当前单 vCPU 用例仍能通过，是因为现有中断注入路径足以支撑该工作负载；本 PR 因此只能证明当前 OVMF/Linux/ACPI 路径成立，不能证明完整 PIC/IOAPIC/LAPIC 切换、PCI 中断或 SMP 中断语义。

该缺口不在本验证 PR 中修复，也不会通过屏蔽 warning、放宽成功条件或扩大 timeout 处理；它进入后续中断完整性与 SMP feature 阶段。

## 不包含

- guest PCI 启动设备或 ESP；
- EFI 应用加载或 Linux EFI stub 磁盘启动；
- OVMF VARS 持久化；
- PCI INTx/MSI/MSI-X 完整链路；
- INIT/SIPI、NMI 或多 vCPU UEFI guest；
- 将当前 non-gating 用例升级为 CI 门禁。

## 评审重点

- diff 不应新增产品配置、公共 API 或固件/设备运行语义；
- VMX/SVM 必须共用 backend-neutral guest baseline，只允许外层 QEMU CPU capability 不同；
- 固件摘要必须对应最终实际加载的 guest image；
- table-loader 测试必须覆盖完整命令内容，而不只是检查命令数量或 checksum 初值；
- 文档不得把当前成功 marker 扩大解释为 PCI 启动、完整中断、SMP 或完整 UEFI 支持。
