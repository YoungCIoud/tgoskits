# PR 1：固定上游 OVMF 与嵌套 fw_cfg 诊断

## 1. 元信息

- 基线：`dev@343d47f4e`
- 状态：待实施设计；`feat/uefi-p0-r2` 仅作为方案参考
- 范围：固定固件资产、loader profile、设备访问诊断和 non-gating VMX/SVM 用例
- 不包含：BDS 门禁、guest 启动盘、EFI/Linux 启动、SMP

## 2. 解决的问题

`dev` 已有 x86 fw_cfg/ACPI 和 resolved device graph，但没有一条固定固件、能排除宿主 OVMF 误命中的嵌套 UEFI 诊断。开发者使用不同发行版 OVMF 或只匹配通用 SEC/BDS 串口文本时，结果既不可复现，也无法判断输出来自 QEMU 外层还是 Axvisor 嵌套 guest。

阶段 1 PR 计划固定实际运行的上游 RELEASE OVMF，并以 Axvisor 自己的 fw_cfg 设备访问作为嵌套执行证据。

## 3. 计划方案

1. 先执行 `cargo xtask ovmf --arch x86_64`，由 Ostool 获取并校验固定版本的 `code.fd`/`vars.fd`；
2. axbuild 检查固定尺寸和地址布局，打印实际摘要，不承担下载；
3. runner 把同一 CODE/VARS 注入 QEMU 外层 pflash，使外层 OVMF 能加载 Axvisor host；
4. runner 同时把同一 CODE 嵌入生成的 VM 配置，由 Axvisor loader 放入嵌套 guest `0xffc8_4000`；
5. profile 强制 CODE size `0x37c000` 和 reset vector `0xfffffff0`；
6. 嵌套 OVMF 访问 Axvisor fw_cfg PIO 时打印一次 `Nested OVMF fw_cfg accessed`；
7. VMX/SVM case 各自选择宿主 CPU 暴露方式，但 guest 配置保持 backend-neutral。

## 4. 建议提交拆分

### 提交 1：固件来源与布局检查

在 axbuild 增加类型化固件来源、固定布局检查、摘要输出和单元测试。Ostool 负责下载与固定版本完整性校验；axbuild 不联网。

### 提交 2：固定 loader profile

增加可选 `firmware_profile`，在 x86 loader 中校验 CODE size、GPA 和 reset vector，并补协议矩阵与错误路径测试。

### 提交 3：统一设备访问诊断

在既有 `DeviceRuntime` 真实访问路径记录 device、bus、方向、地址、宽度、值和结果，不新增地址旁路，不改变错误语义。

### 提交 4：嵌套 fw_cfg marker

在既有 `FwCfgPioDevice` 首次被嵌套 guest 访问时输出一次 Info marker。marker 只证明 fw_cfg 访问，不能命名为 SEC/BDS success。

### 提交 5：VMX/SVM `ovmf-entry` 用例与 runner 接线

增加 backend-neutral VM 配置、两个宿主能力变体、外层 pflash 注入和嵌套 CODE 嵌入。用例放入 `uefi` 组并保持 non-gating。

### 提交 6：文档同步

记录固件准备、两层 OVMF、`uefi = false`、profile 和 marker 的严格语义。

## 5. 公共与配置表面

- 计划为 `axvmconfig::GuestKernelConfig` 新增可选 `firmware_profile`；
- profile 只允许配合 `VMBootProtocol::Uefi`；
- 未配置 profile 的既有 VM 行为不变；
- CLI 增加 `--firmware-bundle-path` 与 `--allow-unverified-firmware`，只影响 UEFI 诊断用例。

## 6. 验证

### 最小 host 测试

```bash
cargo test -p axbuild --lib
```

应覆盖：正确目录、CODE/VARS 缺失、尺寸错误、本地文件未 opt-in、本地文件错误尺寸和路径不存在。

### 固件准备

```bash
cargo xtask ovmf --arch x86_64
```

缓存目录通常为 `$TMPDIR/ostool/ovmf/x64`，其中包含 `code.fd` 与 `vars.fd`。

### AMD/SVM

```bash
cargo xtask axvisor test qemu --arch x86_64 --test-group uefi \
  --test-case ovmf-entry-svm --firmware-bundle-path <ostool-cache-dir>
```

### Intel/VMX

```bash
cargo xtask axvisor test qemu --arch x86_64 --test-group uefi \
  --test-case ovmf-entry-vmx --firmware-bundle-path <ostool-cache-dir>
```

通过条件为日志出现：

```text
Nested OVMF fw_cfg accessed
```

该条件只证明嵌套固件访问了 fw_cfg。

## 7. 已知限制

- `uefi` 组计划保持 non-gating；
- nested guest 没有持久化 VARS flash；
- 计划 marker 早于 BDS，不证明 ACPI 安装完成；
- 外层 QEMU 仍需要 host NVMe rootfs，这个磁盘不是嵌套 guest 启动盘；
- VMX/SVM 需要对应 Intel/AMD KVM 宿主，不能互换运行；
- 未验证本地固件运行结果不能作为功能通过证据。

## 8. PR 标题与正文草稿

### 标题

```text
feat(axvisor): add reproducible x86 OVMF entry diagnostics
```

### 正文

```markdown
## 解决的问题

Axvisor 的 x86 UEFI 路径缺少固定固件和能够区分 QEMU 外层 OVMF 与嵌套 guest OVMF 的诊断用例，导致不同环境的结果不可复现，通用串口 marker 也可能误命中宿主固件。

## 改了什么

- 使用 Ostool 固定版本的上游 RELEASE OVMF，并校验 CODE/VARS 固定布局；
- 为 x86 guest loader 增加固定 firmware profile；
- 增加 VMX/SVM 两个 non-gating `ovmf-entry` 用例；
- 让 QEMU 外层 pflash 与嵌套 guest 使用同一 CODE；
- 在 Axvisor fw_cfg PIO 设备首次被访问时输出嵌套专属 marker；
- 保留显式 opt-in 的未验证本地固件诊断入口。

## 每步的逻辑

Ostool 负责固定版本固件的下载和完整性校验，axbuild 只验证当前运行资产的布局并打印摘要。runner 使用同一 CODE 启动外层 Axvisor host 和嵌套 guest。外层 OVMF 的 fw_cfg 访问发生在 QEMU 内部，不经过 Axvisor，因此 `Nested OVMF fw_cfg accessed` 只可能来自嵌套 guest，避免通用 SEC/BDS 文本误命中。

## 验证

- `cargo test -p axbuild --lib`
- `cargo xtask ovmf --arch x86_64`
- `cargo xtask axvisor test qemu --arch x86_64 --test-group uefi --test-case ovmf-entry-svm --firmware-bundle-path <dir>`
- Intel 主机对应运行 `ovmf-entry-vmx`

该用例只证明嵌套 OVMF 访问 fw_cfg，并保持 non-gating；BDS、启动盘和 Linux EFI 启动不在本 PR 范围。
```

## 9. 合入检查

- [ ] 分支基于目标 `dev`，PR 描述中的 commit/命令与 head 一致；
- [ ] `cargo fmt` 与所有修改 crate 的目标 clippy/test 通过；
- [ ] 至少一个真实 AMD/SVM 或 Intel/VMX 用例在当前 head 通过；
- [ ] 未验证固件没有被用于声明通过；
- [ ] 用户文档、设计文档与最终 fw_cfg marker 一致；
- [ ] PR 标题英文、正文中文，无自推广或工具签名。
