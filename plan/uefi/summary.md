# Phase 1: OVMF SEC 诊断 — 端到端验证总结

## 分支目标

在 AxVisor 上启动固定 profile 的 OVMF，通过 guest COM1 输出流检测 `SecCoreStartupWithStack(` 标记，验证 UEFI 固件加载链路可达 SEC 阶段。

## 测试方式对比

### 测试框架（`cargo xtask axvisor test qemu`）

```
QEMU → host OVMF 固件 → axvisor.efi (UEFI 应用)
     → 加载 nested VM → nested OVMF (OVMF_CODE.fd)
```

- 构建配置：`test-suit/axvisor/uefi/ovmf-entry/build-x86_64-unknown-none-svm.toml`
- QEMU 配置：`test-suit/axvisor/uefi/ovmf-entry/qemu-x86_64-svm.toml`
- VM 配置由 `setup_qemu.sh nimbos-uefi` 生成，编译时嵌入
- 通过 success_regex 匹配输出中的 observer 日志确认 SEC 到达

### 手动测试（`cargo xtask axvisor qemu --vmconfigs <config>`）

```
QEMU → axvisor (raw kernel / -kernel 启动)
     → 加载 nested VM → nested OVMF
```

- 不走 host OVMF，axvisor 作为 raw kernel 直接启动
- 需要 `image_location = "memory"` 将固件编译进二进制

## 已修复的问题

| # | 文件 | 问题 | 修复 |
|---|------|------|------|
| 1 | `nimbos-uefi-smp1.toml` | 低端 RAM 16MB，OVMF PEI 阶段访问 0x1a00000 导致 nested page fault | 改为 128MB |
| 2 | `nimbos-uefi-smp1.toml` | `bios_load_addr = 0xffc0_0000`（VARS 基址）、固件窗口 `[0xffc0_0000, 0x40_0000]` 与仅加载 CODE 段不匹配 | 还原为原始值：`setup_qemu.sh` 会用 `0x100000000 - align_up_4k(firmware_size)` 动态算出 `0xffc84000`，sed 替换 |
| 3 | `setup_qemu.sh`、`quick-start.sh`、`ovmf-profile.sh` | `cargo axvisor image pull` 子命令在 PR #1182 后已提升为 `cargo xtask image pull`，旧命令不存在 | 14 处全部替换 |
| 4 | `write_bytes` (mod.rs) | `Self::current_vm_id()` 在 vCPU 未 pin 时 `.expect()` panic，静默终止后续 observer 调用 | 改为 `with_current_vcpu` + `Option` 安全跳过 |

## 测试中遇到的障碍

### 1. `ovmf-entry` test case 命名

`ovmf-entry` 目录下有两个 build 变体（`-vmx` / `-svm`），实际 case 名为 `ovmf-entry-vmx` 和 `ovmf-entry-svm`。需指定完整名称。本机是 AMD，使用 `ovmf-entry-svm`。

### 2. rootfs 自动推断问题

`infer_rootfs_path` 从 VM config 的 `kernel_path` 同级目录查找 `rootfs.img`。generated config 的 `kernel_path` 指向 `/tmp/.axvisor-images/qemu_x86_64_nimbos/qemu-x86_64`，自动找到了 nimbos rootfs（64MB），覆盖了 QEMU 配置中写死的 alpine rootfs（2GB）。nimbos rootfs 与 NVMe 驱动不兼容导致 I/O 阻塞。

**workaround**：移除 nimbos rootfs 让推断失败，回退到 alpine rootfs。

### 3. 默认 build config 的 virtio-blk 问题

`tmp/axbuild/config/axvisor/build-x86_64-unknown-none.toml` 残留了 `ax-driver/virtio-blk` feature，该 feature 在 ax-driver 0.12.2 中已不存在。手动测试需要改为 `ax-driver/nvme`。

### 4. 增量编译缓存

修改 `axvm` crate 后仅 `touch` build.rs 不会触发重编，需 touch axvm 源文件（`mod.rs`、`guest_serial.rs`）才能使改动生效。

## 尚未解决的阻塞问题

### COM1 模拟未启用

**根因**：`x86_64/vm.rs:106-109` 仅在 VM config 的 `emu_devices` 中包含 `Console` 类型设备时才设置 `emulate_com1 = true`。当前模板 `emu_devices = []`，guest OVMF 向 I/O port 0x3F8 的写入不被拦截，`AxvmX86HostOps::write_bytes` 永远不会被调用，observer 收不到任何字节。

**验证方式**：在 `write_bytes` 中插入 `ax_hal::console::write_bytes(b"[OBS]")` 直接写串口的原始诊断，确认 guest OVMF 的 COM1 输出完全不经过该函数。

**预期修复**：在模板中配置 `emu_devices = [["com1", 0, 0, 0, 2]]`（`2` = `EmulatedDeviceType::Console`）。

**当前状态**：加了 Console 设备后 VM 启动阶段出现 main task 死循环 yield，VM 未创建。需单独排查 Console 设备模拟与其他配置的冲突。

## Observer 激活链路（已确认正确）

```
config.rs:152  AxVM::new(vm_config)
config.rs:164  prepared_boot.load_images(main_mem, vm.clone(), &provider)
               → boot/mod.rs:372  mark_fixed_ovmf_profile_loaded(&loader.vm)
               → FIXED_OVMF_INSTANCES.lock().insert(vm.id(), Arc::downgrade(vm))

config.rs:171  axvm::register_vm(vm.clone())
               → manager 中存储同一 Arc

runtime/vcpus.rs:486  CurrentArch::before_first_run(&vm, &vcpu)
               → mod.rs:72  activate_ovmf_sec_diagnostic(vm)
               → FIXED_OVMF_INSTANCES 中 Weak::upgrade → ptr_eq → 匹配成功
               → OVMF_SEC_DIAGNOSTICS.lock().activate(vm_id, true, true)

serial.rs:202  host::write_bytes::<H>(&[value])
               → host.rs:127  H::write_bytes(bytes)
               → mod.rs:285  write_bytes → observe(vm_id, bytes)
               → guest_serial.rs:22  MarkerMatcher 逐字节匹配 "SecCoreStartupWithStack("
               → info!("VM[{vm_id}] guest COM1 reached OVMF SEC: ...")
```

## 本分支待提交的改动

1. `nimbos-uefi-smp1.toml`: 低端 RAM 128MB
2. `setup_qemu.sh`, `quick-start.sh`, `ovmf-profile.sh`: `cargo axvisor image pull` → `cargo xtask image pull`
3. `guest_serial.rs`: observer 激活诊断日志 + write_bytes 安全修复
4. `mod.rs`: write_bytes 避免 panic

> **作废声明**：本文件“# 冲突分析”标题之前的全部内容（分支目标、测试方式对比、已修复的问题、测试中遇到的障碍、尚未解决的阻塞问题、Observer 激活链路、本分支待提交的改动）基于已被 dev 删除的旧测试框架（`setup_qemu.sh`/`quick-start.sh`/`ovmf-profile.sh`、NimbOS 配置、`guest_serial.rs` observer、`before_first_run` 挂钩、`VM[1] guest COM1 reached ...` marker 等），**不可作为当前实现依据**。这些历史记录仅作背景参考；当前实现方案见 `plan/uefi/solve_conflict.md`。

# 冲突分析

结论：不建议直接 rebase 后逐个解冲突。应保留 uefi/p0 作为参考，从最新 dev 新建实现分支，按 dev 的新架构重做 P0。旧实现约有一半以上依赖已被删除或重构的边界，硬合并会
  把旧架构重新引入。

  已按本地 dev@4c7aa8577 复核，冲突文件清单见 solve_conflict.md §1.4。

  ## 冲突规模

  - dev 相对共同基点前进 34 个提交。
  - uefi/p0 只有 1 个集中提交，涉及 24 个文件，约 +1707/-193。
  - 只读合并模拟发现（逐文件核对，见 solve_conflict.md §1.4）：
      - 16 个文件双方同时修改。
      - 7 个文件已被 dev 删除，但 P0 仍在修改。
      - 8 个文件为 P0 新增（dev 中不存在）。
      - 更严重的是大量“文本上能合并、语义上已经无效”的新增代码。

  ## dev 的关键变化

  ### 1. 串口已经变成 machine-owned 资源

  dev 的 x86 machine profile 固定提供 16550 COM1、PIO 0x3f8、GSI 4，见 virtualization/axvm/src/machine.rs:335。

  新配置明确禁止 emu_devices、serial、裸地址和 IRQ 字段，见 docs/docs/architecture/axvisor-guest-machine.md:70。

  因此 P0 中这些内容必须删除：

  - UEFI TOML 里的 x86-com1
  - guest_serial.rs 的全局 VM ID/Weak 引用表
  - before_first_run / on_last_vcpu_exit 串口诊断挂钩
  - 通过 x86 vlapic host ops 截获串口输出

  dev 已建立每次设备 generation 都创建新 backend 的生命周期，见 virtualization/axdevice/src/serial/backend.rs:19，输出统一经过 os/axvisor/src/guest_console/mux/
  mod.rs:308。

  实际上 P0 不必重新实现 SEC matcher：ovmf-entry 是单 VM 测试，可以直接将 SecCoreStartupWithStack( 作为成功正则。输出路径本身已经是 guest COM1，反而更符合“成功只能来自
  固件串口输出”的要求。

  ### 2. 独立脚本和 NimbOS 测试已被删除

  d7050469f 删除了：

  - setup_qemu.sh
  - quick-start.sh
  - NimbOS guest 配置
  - qemu-nimbos 用例
  - 旧 CI/脚本文档

  所以不能保留 P0 的 ovmf-profile.sh，也不能恢复这些脚本或 NimbOS 用例。固件拉取、manifest 校验和配置生成应迁入 cargo xtask axvisor / axbuild 的 Rust 编排层。

  ### 3. VM 配置模型已变化

  P0 配置仍使用：

  vm_type = 1
  emu_devices = [...]

  dev 已改为：

  guest_type = "passthrough"

  [devices]
  passthrough = []
  disabled = []

  现存配置见 os/axvisor/configs/vms/qemu/x86_64/arceos-uefi-smp1.toml:1。P0 的旧配置无法做机械合并。

  ### 4. VM 创建和设备生命周期已重构

  dev 当前顺序是：

  1. 准备 boot resources
  2. 创建 VM
  3. 准备内存
  4. 装载镜像
  5. 创建设备和 vCPU
  6. 注册 VM

  见 os/axvisor/src/config.rs:139。

  旧 P0 为了防止 VM ID 重用、注册失败和 reset 污染而增加的全局诊断状态，已经不适合这个生命周期。新的 per-generation serial backend 已经解决大部分问题。

  ### 5. 测试规范也发生了变化

  P0 的 axbuild 测试有不少读取 Rust 源码并断言实现文本的测试。当前规范明确禁止用源码文本检查验证 Rust 行为，要求编译并调用真实 API，见 book/guideline/code-
  quality.md:1709。

  这些测试应全部重写，不应在冲突解决时保留。

  ## 建议的保留与重做范围

   P0 内容与处理方式
   固定 edk2 tag/commit、构建开关和 manifest 契约      保留

   CODE/VARS 大小、SHA-256、拼接关系验证               保留，改写为 Rust typed verifier

   CODE GPA 0xffc84000、大小 0x37c000、reset vector    保留，重新应用到最新 x86 loader

   只向 AxVisor 装载 CODE，不加载 VARS                 保留

   未验证固件必须显式 opt-in 并打印哈希                保留，放到 xtask CLI

   PIO/MMIO Trace 语义                                 保留，重新应用到最新 DeviceRuntime

   ovmf-entry VMX/SVM 诊断                             保留，重新接入最新测试编排

   guest TOML 注册 COM1                                删除

   guest_serial.rs 全局 matcher                        删除

   setup_qemu.sh、quick-start.sh、ovmf-profile.sh      删除，不恢复

   NimbOS 配置和 qemu-nimbos                           删除

   源码文本断言测试                                    删除并改写

   旧 quick-start 文档路径                             删除，迁入现有 architecture/development 文档

  ## 推荐重做方案

  不要对 `941134916` 整体 cherry-pick。建议保留旧分支，然后从 dev 建一个新的 P0 分支，分成以下小提交：

  1. feat(axbuild): verify fixed OVMF debug bundles
      - 在 axbuild 增加 OvmfManifest、VerifiedOvmfBundle 等私有类型。
      - 使用 serde/TOML 和 deny_unknown_fields。
      - 复用现有 image registry 下载和 archive SHA-256。
      - 校验单文件大小、哈希、CODE/VARS 拼接、profile 和 GPA。
      - 用 FirmwareSource 枚举区分固定资产与显式未验证本地固件，避免布尔参数扩散。

  2. feat(axvm): enforce the fixed x86 OVMF CODE layout
      - 在最新 virtualization/axvm/src/arch/x86_64/boot/mod.rs:202 上增加小型、私有的 layout 校验。
      - 同时覆盖内存镜像和文件镜像路径。
      - UEFI 固件必须是 0x37c000，加载到 0xffc84000，入口为 0xfffffff0。
      - multiboot/direct boot 保持不变。
      - 成功日志打印 profile、路径、大小、GPA 和 reset vector。

  3. test(axvisor): add fixed OVMF entry diagnostics
      - 由 xtask 在构建前准备并验证 bundle、生成专用 VM 配置。
      - 不绑定 NimbOS。
      - 配置中不声明 COM1。
      - VMX/SVM 使用相同的 backend-neutral build 配置，分别使用对应 QEMU/KVM 参数。
      - success_regex 直接匹配 guest 串口的 SecCoreStartupWithStack\(。
      - 保持非门禁，直到固定资产和两类硬件证据齐全。

  4. docs(axvisor): document the fixed OVMF profile
      - 更新现有架构文档和 arch-platform-porting reference。
      - 不恢复已删除的 quick-start 文档和脚本。

  ## 必须重新编写的测试

  - manifest 正常、错误哈希、错误大小、错误 base、未知字段、重复字段。
  - 未验证固件缺少显式 opt-in 时拒绝。
  - x86 loader 对错误 CODE 大小、GPA、entry point 的真实 API 测试。
  - serial generation/reset 由现有 machine/backend 测试覆盖；P0 不再另建全局状态。
  - axdevice 注册/未注册、read/write/error、不同宽度的真实 dispatch 测试。
  - VMX/SVM ovmf-entry 端到端测试。
  - 标准 QEMU q35 的 SEC→PEI→DXE→BDS→EFI marker 资格验证继续作为固件资产发布前验证。

  ## 最终建议

  应“推翻旧实现，保留设计目标”：

  - 旧分支留作需求和边界条件参考。
  - 不进行传统冲突解决。
  - 从最新 dev 重新实现。
  - 第一项工作应先同步 plan/uefi/design.md，明确删除脚本、NimbOS、guest-owned COM1 和 arch-level matcher，再开始代码。
