# Axvisor x86_64 UEFI PCI 磁盘实验

## 1. 实验目标

这个用例验证一条实验性的启动链路：外层 QEMU 启动 Axvisor，Axvisor 为
guest 注册 modern `virtio-blk` PCI 设备，guest 内的 OVMF 再从该设备读取
GPT/FAT32 ESP 并尝试启动其中的 Linux EFI 应用。它当前用于验证启动链路，
不属于稳定回归测试。

## 2. 输入和镜像路径

用例使用仓库中的 OVMF 输入和宿主机上的启动盘。`axbuild` 会在准备外层
Axvisor rootfs 时校验启动盘，并通过 `debugfs` 把它复制到 rootfs 内；因此
guest 配置的 `image_path` 指向的是 Axvisor 可访问的文件系统路径。

- `/tmp/uefi-guest.img`：本地 guest 启动盘镜像，大小必须是非零的 512 字节整数倍；
- `assets/uefi-guest.img.gz`：CI 使用的启动盘压缩副本，CI 会将它恢复到上述路径；
- `assets/OVMF_CODE_4M.fd` 和 `assets/OVMF_VARS_4M.fd`：随仓库提交的 OVMF CODE/VARS。

guest TOML 使用 `/uefi/uefi-guest.img`，不需要预先把镜像复制到仓库：

```toml
[[devices.virtual]]
model = "virtio-blk"
transport = "pci"
backend = "ramdisk"
image_path = "/uefi/uefi-guest.img"
read_only = true
```

仓库中的 OVMF CODE/VARS 会被组装为一个 4 MiB 的 guest 固件镜像，并由
`uefi_firmware_path` 加载。当前 pinned `ostool` OVMF 资产不包含可用的
VirtIO block UEFI 驱动，因此 build 配置显式使用 `assets/` 中的 OVMF，CI
不再依赖宿主机的 `/usr/share/OVMF` 路径。

## 3. 运行入口

实验由 `test-suit/axvisor/experimental/qemu-uefi-disk` 下的 build/QEMU
配置驱动。guest 配置 `x86-linux-uefi-disk.toml` 已设置 `cpu_num = 1`，
用于控制实验变量并避免引入多 vCPU 干扰；外层 QEMU 的 `-smp 2` 只代表
Axvisor 所在的宿主环境，不改变 guest 的 vCPU 数量。

```bash
cargo xtask axvisor test qemu \
  --arch x86_64 --test-group experimental --test-case uefi-disk-vmx

cargo xtask axvisor test qemu \
  --arch x86_64 --test-group experimental --test-case uefi-disk-svm
```

成功条件是 guest Linux 的 shell 中同时存在 `/dev/vda`，并且
`/sys/class/block/vda/ro` 为 `1`；随后测试脚本输出独立一行
`AXVISOR_X86_UEFI_DISK_PASSED`。这个 marker 来自内层 guest，不来自外层
QEMU 或 Axvisor 日志。

## 4. vPIC 状态

当前分支不实现 vPIC 到 vLAPIC LINT0 的 wire-mode 切换。guest 修改
`VirtualApicRegs::write_lvt()` 中的 LINT0 ExtINT 屏蔽状态时，仍按原行为输出
`vpic wire mode change to LAPIC, unimplemented` 或
`vpic wire mode change to NULL, unimplemented`；guest 关闭 APIC 时，
`VirtualApicRegs::write_svr()` 也继续报告 wire mode 尚未实现。PIC 向量仍由
现有的调用方 vCPU 直接注入路径处理。

## 5. 当前验证结果

截至 2026-09-04，启动盘仍已确认是可读的 GPT/FAT32 UEFI 镜像，宿主 QEMU
配合系统 OVMF 和 VirtIO block 可以直接启动其中的 Linux。代码层面的
`x86_vlapic` 单测、`x86_vlapic`/`axdevice`/`axvm` clippy 检查均已通过。

此前的实验能够完成镜像注入、Axvisor UEFI 启动、guest 创建和
`VCpu[0] running on CPU0`；本机日志实际选择了 SVM 后端，因此用例名不等同于
硬件后端。恢复 vPIC wire mode 的 `unimplemented` 路径后，该用例仍不能据此
证明 OVMF 已完成从 VirtIO 磁盘启动；是否出现 Linux shell 或
`AXVISOR_X86_UEFI_DISK_PASSED` 需要在目标环境重新运行确认。
