# Axvisor x86_64 UEFI PCI 磁盘实验

这个用例验证一条实验性的启动链路：外层 QEMU 启动 Axvisor，Axvisor 为
guest 注册 modern `virtio-blk` PCI 设备，guest 内的 OVMF 再从该设备读取
GPT/FAT32 ESP 并尝试启动其中的 Linux EFI 应用。

## 本地输入

用例默认读取以下宿主机文件：

- `/tmp/uefi-guest.img`：guest 启动盘镜像，大小必须是非零的 512 字节整数倍；
- `/usr/share/OVMF/OVMF_CODE_4M.fd`；
- `/usr/share/OVMF/OVMF_VARS_4M.fd`。

`/tmp/uefi-guest.img` 不需要预先复制到仓库。`axbuild` 在构建测试 rootfs
时会校验镜像并通过 `debugfs` 注入到外层 Axvisor rootfs 的
`/uefi/uefi-guest.img`。guest TOML 中的 `image_path` 使用的正是这个外层
rootfs 路径，而不是宿主机路径。

宿主机 OVMF 的 CODE/VARS 会被组装为一个 4 MiB 的 guest 固件镜像，并由
guest 配置中的 `uefi_firmware_path` 加载。当前 pinned `ostool` OVMF 资产
不包含可用的 VirtIO block UEFI 驱动，因此这个实验显式使用系统 OVMF
CODE/VARS；这也是一个本地实验前提，不适合作为可移植 CI 资产。

## 运行

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

## 当前实验结果

截至 2026-09-04：

- `uefi-guest.img` 已确认是可读的 GPT/FAT32 UEFI 镜像；用宿主 QEMU 配合
  系统 OVMF 和 VirtIO block 可以直接启动其中的 Linux；
- `axbuild` 已能把该镜像注入外层 rootfs，并在 Axvisor 中按
  `/uefi/uefi-guest.img` 创建 ramdisk-backed VirtIO block PCI 设备；
- VMX 实验能完成外层 Axvisor 启动、guest 创建和 vCPU 启动，但在 600 秒
  内没有出现 Linux shell 或 `AXVISOR_X86_UEFI_DISK_PASSED`，最终结果为
  `QEMU timed out after 600s`；日志中没有 Axvisor panic、page fault 或 VM
  exit failure。

因此这个目录目前记录的是可复现的实验入口和失败证据，不能把当前 VMX/SVM
用例标记为已通过。下一步应继续定位 guest vCPU 在 OVMF 初始化阶段停住的
原因，再把该用例提升为正常回归测试。
