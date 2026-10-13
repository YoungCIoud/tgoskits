# 实习开发总结与工作交接

## 1. 实习工作总结

7 至 9 月，我围绕本计划推进了 Axvisor 的共享中断基础设施和 x86_64 Guest UEFI 支持。7 月完成 IRQ line 资源注册、架构无关的虚拟中断类型，以及 sender、dispatcher 到目标 vCPU run loop 的投递通道。随后工作重点转向 UEFI：先补齐 OVMF/ACPI 的 VMX/SVM 验证基线，再完成共享 virtqueue 加固、普通设备 MMIO 解码、通用 vPCI 和同步 VirtIO block PCI ramdisk，最后推进 OVMF 从 guest 磁盘 ESP 经 GRUB 启动 Linux。

实习期间完成的主线成果已经以独立 PR 合入。完整磁盘启动方案保留在 #2275：早期 ramdisk 版本有 VMX/SVM 启动通过记录，9月底的 raw 文件启动盘与 ext4 数据盘版本已有本地 SVM 双盘读写、重挂载回读的通过记录，但尚未合入，VMX 和持续 CI 的对应验证仍需补齐。本次实习结束，开发计划据此收尾；尚未完成的交付与原计划扩展项分别在下文说明，供后续开发者继续推进。

## 2. 已完成成果

### 2.1 共享中断基础设施

#1630、#1661 和 #1679 建立设备 IRQ 资源到运行时递送的共享基础。

### 2.2 UEFI 验证与 PCI 数据路径

UEFI 工作从验证已有 fw_cfg、ACPI 和固件交接路径开始，再逐步解决 PCI 块设备的实际消费者需求。以下 PR 已合入上游 `dev`：

1. #1931：OVMF/ACPI 验证。记录实际固件路径、大小、摘要，增加 VMX/SVM 对称用例及 ACPI table-loader 逐项断言。
2. #1959：SVM 持续验证。将 OVMF/ACPI SVM 用例接入自托管 AMD CI。
3. #1984：共享 virtqueue 加固。统一 descriptor 链边界，校验 ring 布局，在运行期故障后停止访问并要求 reset。
4. #2082：普通设备 MMIO。VMX/SVM 根据设备图识别普通 MMIO，支持 byte、word、dword、qword MOV 访问。
5. #2197：通用 vPCI。统一 PCI function、BDF、BAR 规划和配置分派，增加 Linux guest 枚举验证。
6. #2070：PCI块设备。接入 modern VirtIO PCI、共享 block core、DMA 与 INTx，完成同步 ramdisk 读写用例。
7. #2501：Q35 ECAM。补齐 ECAM、扩展配置访问、MCFG 和窗口保留描述 。
8. #2510：块请求限制MMIO/PCI 按实际协商的 SIZE_MAX、SEG_MAX feature 检查请求。
9. #2518：Legacy PIC 投递.保留中断来源，避免 PIC 事件被 LAPIC PPR 筛选阻挡。

### 2.3 磁盘启动验证

#2275 将 guest 启动来源从 fw_cfg 提供 kernel/initramfs 调整为 PCI 磁盘。`boot_source = "pci-disk"` 让 OVMF 从 ESP 中读取 `BOOTX64.EFI`，经 GRUB 加载 Linux 和 initramfs；`image_format = "raw"` 让 file 后端接受带 GPT 和 ESP 的整盘镜像。

验证经历了两个版本，需要分别看待其证据：

- 早期版本使用 ramdisk 启动盘。Discussion 的 9 月 7 至 13 日进度记录了 [VMX 日志](https://github.com/rcore-os/tgoskits/actions/runs/34466205400/job/102835276387) 和 [SVM 日志](https://github.com/rcore-os/tgoskits/actions/runs/34466205400/job/102835276075)，当时以进入 Linux shell 的输出判断启动完成。
- 9月底版本（当前版本）使用 raw 文件启动盘与 ext4 数据盘。内层 `/init` 检查启动盘参数、GPT/ESP 和 ext4 超级块，分别写入两块盘并在卸载、重挂载后回读，全部通过才输出 `AXVISOR_X86_UEFI_FILE_DUAL_DISK_PASSED`。该版本已有本地 SVM 通过记录，VMX 尚需补证。

## 3. 后续开发衔接

### 3.1 代码入口

| 入口 | 接手用途 |
| --- | --- |
| `virtualization/axvmconfig/src/lib.rs` | 查看 `VMBootSource::PciDisk` 及启动来源配置 |
| `virtualization/axvm/src/arch/x86_64/boot/` | 查看固件装载、fw_cfg、ACPI 与磁盘启动分支 |
| `virtualization/axvm/src/configured/devices/virtio_blk/` | 查看 file/ramdisk 后端选项和镜像检查 |
| `virtualization/axdevice/src/pci/` | 查看配置分派、资源规划、endpoint 路由及生命周期 |
| `virtualization/axvirtio-common/src/pci/`、`virtualization/axvirtio-blk/src/` | 查看共享 PCI transport、队列与块请求处理 |
| `scripts/axbuild/src/axvisor/test/uefi_file.rs` | 查看启动镜像校验、解压和 rootfs 临时副本注入 |
| `test-suit/axvisor/normal/qemu-uefi-file/` | 查看双盘 VM 配置、固件/磁盘资产、GRUB 和内层 `/init` |

`uefi_file.rs` 和 `qemu-uefi-file/` 属于 #2275 分支，尚未进入主线。接手前先与最新 `dev` 对齐；不要直接批量 cherry-pick。

### 3.2 完成现有启动方案

首要未完成项是 #2275 的合入与验证。运行环境需要提供可用的 KVM 和嵌套虚拟化：VMX 在合适的 Intel 宿主验证，SVM 在合适的 AMD 宿主验证。

在包含 #2275 的工作树中使用项目入口运行双盘用例：

```sh
cargo xtask axvisor test qemu --arch x86_64 --test-case uefi-file-vmx
cargo xtask axvisor test qemu --arch x86_64 --test-case uefi-file-svm
```

通过证据应包含内层 `AXVISOR_X86_UEFI_FILE_DUAL_DISK_PASSED` 及双盘检查日志。外层 QEMU 启动、Axvisor 的 VM 启动成功、OVMF 横幅或 Linux shell 输出都不能代替新版用例的完整判定。还需要让持续 CI 实际执行这两个 KVM 用例，不能只看汇总状态。

资产交付尚未完成。`prepare_boot_image()` 校验并解压仓库内的 `boot.img.gz`，`prepare_configured_boot_rootfs()` 将其注入 AxVisor fs 。最终期望 `/guest/boot.img` 纳入 tgosimages 的 fs 制作和发布流程，再删除测试专用的解压、复制、注入流程及环境变量。`boot.img` 中具体的内容（内核、initramfs等）都可以与当前使用的不同，但是可能需要修改测试中 pass 的条件。修改 GRUB 配置或内层 `/init` 时，需要同步更新压缩盘内容与摘要，避免源文件和实际运行资产不一致。

另一个技术欠项是未分配 MMIO 的处理。当前在 `0xfed4_0000` 注册“没有 TPM”的兼容设备，读取返回对应宽度的全 1、写入忽略，使 OVMF 探测继续。后续应根据平台语义整理未分配 MMIO 访问的通用处理，避免每发现一个探测地址就增加一个兼容窗口。
