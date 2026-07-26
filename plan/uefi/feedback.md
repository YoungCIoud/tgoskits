• 已修复本轮 review 指出的两个问题。

  - OVMF SEC 状态改为由 virtualization/axvm/src/vm/mod.rs:395，固件装载阶段不再直接修改全局 matcher。
  - 仅在 VM 注册后、vCPU 首次运行时，校验 registry 中是同一个 Arc 实例后激活 matcher，避免准备失败、注册失败及重复 VM ID 污染。virtualization/axvm/src/arch/x86_64/
    guest_serial.rs:22

  - 多 vCPU 重复激活保持幂等；最后 vCPU 退出清理；reset 后依据实例状态重新激活。
  - 增加准备/注册失败、重复 ID、reset、部分标记不被重置的回归测试。
  - quick-start setup 持久记录实际生成的 verified/unverified 配置，run 不再根据当前环境重新推导。os/axvisor/scripts/quick-start.sh:352
  - 新 setup 开始时清除旧选择，仅在完整成功后发布新 .selected，避免失败后启动陈旧配置。
  - 已同步 UEFI profile、quick-start 中英文文档及 arch-platform-porting 调试参考。

  验证结果：

  - axvm：115/115 tests passed
  - OVMF 定向测试：12/12 passed
  - quick-start 跨进程及失败清理回归：passed
  - cargo xtask clippy --package axvm：4 个组合全部通过
  - cargo xtask clippy --package axbuild：通过
  - cargo fmt --check、三个 shell 脚本语法检查、git diff --check：通过
  - axbuild 全量测试中的偶发 Git mock 用例单独及整个 26 用例模块重跑通过；UEFI 相关用例均通过。

  真实 tgosimages OVMF bundle、Q35 参考启动及 VMX/SVM 实机 Trace 仍因资产未发布而无法执行，未用合成测试冒充这些外部验收证据。