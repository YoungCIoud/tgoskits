# AxVisor x86 UEFI 固件入口复审

日期：2026-07-22

## 结论

本轮反馈所述的两个仓库内问题均已修复。

OVMF SEC 诊断资格现在由 `AxVM` 实例持有。固件装载完成时只记录实例状态，不再修改全局 matcher；matcher 仅在 VM 已完成注册、vCPU 首次运行且注册表中的 `Arc` 与当前实例相同时激活。多 vCPU 激活保持幂等，最后一个 vCPU 退出会清理状态，reset 创建的新 vCPU 任务会依据实例状态重新激活。准备失败、注册失败和未注册的重复 VM ID 不再进入全局诊断状态。

quick-start 也不再根据 run 进程当前的环境变量重新推导配置文件。setup 开始时清除旧选择，只在配置完整生成后原子发布 `.selected` 文件；run 会读取、校验并使用该记录。因此 verified/unverified 配置可跨进程保持一致，失败的替换 setup 不会继续启动旧选择。

本轮没有发现新的仓库实现阻塞项，功能范围也保持受控。不过，真实 tgosimages 固件资产、Q35 参考启动以及 VMX/SVM 运行 Trace 仍不可用，因此只能确认仓库内实现和合成验证成立，不能据此宣称完整端到端验收已经完成。

## 上轮问题复核

### 1. SEC 诊断状态生命周期

当前实现的关键链路如下：

- `virtualization/axvm/src/arch/x86_64/boot/mod.rs:359-373` 在固定布局固件成功复制后调用 `mark_fixed_ovmf_profile_loaded()`，不接触全局 matcher。
- `virtualization/axvm/src/vm/mod.rs:395-448` 用实例私有 `AtomicBool` 保存固件资格，并以 Release/Acquire 发布和读取。
- `virtualization/axvm/src/arch/x86_64/mod.rs:64-67` 仅从 vCPU 的 `before_first_run` hook 请求激活。
- `virtualization/axvm/src/arch/x86_64/guest_serial.rs:22-35` 同时检查实例资格和注册表中的 `Arc::ptr_eq()`；准备失败、注册失败及重复 ID 的未注册对象无法通过检查。
- `guest_serial.rs:52-61` 使用 `entry().or_default()`，多个 vCPU 不会重置已经匹配的前缀或已报告状态。
- `virtualization/axvm/src/arch/x86_64/mod.rs:84-87` 在最后一个 vCPU 退出时删除 matcher。
- `virtualization/axvm/src/vm/mod.rs:716-729` 的 reset 会等待旧 runtime 退出、重建资源并启动新 vCPU；新任务再次执行 first-run hook，实例资格本身保留。
- `os/axvisor/src/config.rs:151-159` 仍保持“装载固件 → prepare → register”的顺序，但前两步不再能提前创建全局 matcher。

上述所有权和时序能够关闭上轮发现的初始化失败、重复 ID 污染及 reset 后不再激活的问题，当前未发现新的可复现状态错误。

测试证据需要准确限定：`guest_serial.rs:143-197` 覆盖了幂等激活、失败资格、重复 ID、清理和重新激活的局部状态机；`scripts/axbuild/src/axvisor/test/tests.rs:363-376` 是源码结构断言。这些测试没有真实执行 AxVM 注册表、prepare/register 失败回滚或 reset 调用链，因此不能称为完整生命周期集成测试。该限制不改变本轮代码审阅结论，但真实双后端运行和后续集成覆盖仍不能被这些单元测试替代。

### 2. quick-start 分步运行选择

当前实现符合反馈描述：

- `os/axvisor/scripts/quick-start.sh:352-376` 只根据 setup 当次的验证结果决定生成文件名，并为每种 Guest 使用固定 selection 文件。
- `quick-start.sh:378-407` 在 setup 开始时清除旧 selection，并只允许记录四个预定义路径；目标配置必须存在，写入通过临时文件加 `mv` 发布。
- `quick-start.sh:409-440` 要求 selection 恰好一行、路径命中白名单且目标文件仍存在。
- NimbOS 和 ArceOS setup 分别在 `:442-465`、`:495-537` 完成所需配置后记录选择。
- 两个 run 路径在 `:550-568` 只读取已保存选择，不再依赖当前进程的固件环境变量。
- `scripts/axbuild/src/axvisor/test/tests.rs:261-361` 实际 source 脚本并跨进程改变环境，verified 和 unverified 两种选择均通过。

上轮的跨进程不一致问题已关闭。测试同时验证了显式清理后不能读取旧选择；setup 函数也在任何可能失败的下载、固件校验和配置生成之前执行清理。

## 范围边界核对

| 项目 | 当前状态 | 判断 |
| --- | --- | --- |
| 固定 EDK2 revision、DEBUG、4 MiB、COM1 profile | 脚本、配置、AxVM 校验和文档保持一致 | 范围内 |
| 默认从 tgosimages 拉取并校验 | 未恢复发行版 OVMF 扫描 | 范围内 |
| manifest、大小、SHA-256、拼接和固定地址校验 | 定向测试覆盖正常和拒绝路径 | 范围内 |
| Guest 固件映射 | 仍只映射 CODE，VARS 只作为资产模板 | 边界保持 |
| pflash、可写 VARS、fw_cfg、PCI 或 PM 平台能力 | 本轮反馈修复没有新增这些能力 | 边界保持 |
| 公开 VM 配置接口 | 未新增字段 | 边界保持 |
| SEC 成功来源 | 只有已注册且加载固定布局固件的 VM，其 Guest COM1 完整标记才触发 | 核心边界成立 |
| 完整 NimbOS 成功条件 | 使用 Guest 自有 `usertests passed!`，不使用 Host 任务启动日志 | 范围内 |
| direct VMX/SVM 用例 | 既有 normal case 未改动 | 边界保持 |
| CI 门禁 | UEFI job 仍为注释中的未来任务，入口诊断仍为非门禁 | 范围内 |

对所有本次变更涉及的文档、脚本注释、Rust 注释、测试配置和调试参考进行了检索，未发现内部规划阶段编号。

## 独立验证

本轮复审实际执行并通过：

- `cargo test -p axvm --lib -- --test-threads=1`：115 passed。
- `CARGO_TARGET_DIR=/tmp/tgoskits-uefi-review-target cargo test -p axbuild --lib ovmf -- --test-threads=1`：12 passed。
- 同一独立 target 目录执行 `cargo test -p axbuild --lib quick_start -- --test-threads=1`：11 passed，其中跨进程 selection 用例实际运行通过。
- `CARGO_TARGET_DIR=/tmp/tgoskits-uefi-review-target cargo xtask clippy --package axvm`：base、fs、host-fs、sstc 共 4 个检查全部通过。
- `CARGO_TARGET_DIR=/tmp/tgoskits-uefi-review-target cargo xtask clippy --package axbuild`：通过。
- `cargo fmt --all -- --check`。
- `bash -n os/axvisor/scripts/ovmf-profile.sh os/axvisor/scripts/setup_qemu.sh os/axvisor/scripts/quick-start.sh`。
- `git diff --check`。

axbuild 定向测试第一次使用仓库默认 `target/` 时，被其中不可写的既有构建产物阻断；改用 `/tmp` 下独立 target 目录后全部通过。本轮没有重跑 axbuild 的全部 759 项测试；`feedback.md` 记录此前偶发失败的 Git mock 用例单独及所在模块重跑通过，本轮独立验证聚焦于本次 UEFI 和 quick-start 变更。

## 尚缺的验收证据

本地仍没有 `qemu_x86_64_axvisor_ovmf_debug` 的真实 tgosimages bundle，因此未能独立验证：

- 两个干净 workspace 的固件逐字节可复现构建；
- 固定产物包含 Shell 和要求的 VirtIO 模块；
- Q35 + TCG 下 SEC → PEI → DXE → BDS → EFI 应用的完整参考日志；
- 二次启动时工作 VARS 可变化、发布 VARS template 哈希保持不变；
- VMX 与 SVM 上 Guest SEC 标记及首个未实现设备访问 Trace。

这些是外部验收缺口，不是当前仓库代码可以用合成 bundle 或局部 matcher 测试替代的证据。固定资产发布后仍需按计划完成标准 QEMU 和双后端运行验证。

本次复审未修改实现、测试、配置或其他文档；唯一仓库内容写入为本报告。
