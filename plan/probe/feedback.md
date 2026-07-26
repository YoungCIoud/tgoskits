已根据 `plan/review.md` 的第二轮复审继续修正全部指出的问题，并完成实际 package/publish dry-run 与原生 OVMF 验证。

本轮修正：

- `axbuild` 不再依赖 `publish = false` 的 `axvisor-uefi-probe`。runner 在 `scripts/axbuild` 内维护独立的 version 1 host parser，根 workspace dependency、`axbuild` dependency 和 lockfile 依赖均已移除。
- 实际 `cargo package` 进一步发现 `axbuild` 的 agent-review-bench `include_str!` 指向 crate 外资源；已将三份只读契约资源放入发布包并改用包内路径，使生成的 crate tarball 可以独立编译。
- Block IO adapter 只跳过 absent media。介质存在但 block size 为零时进入统一 geometry 校验并报告 `invalid-field`；adapter 使用的 media-selection/allocation helper 具有确定性 host 回归。
- renderer 省略空 `detail`，保持所有公开 `with_detail("")` 输出可被 parser 接受；probe parser 和 axbuild host parser 都要求首次逻辑记录严格按 BEGIN、16 个 CHECK、SUMMARY、terminal marker 排列，同时仍接受一致的重复物理日志副本。
- ACPI v1 契约固定为：未知 signature 也校验公共 header、length 和 checksum；损坏未知表使 `acpi-root` 失败。根表重复物理地址同样使 `acpi-root` 以 `invalid-field` 失败，固件 adapter 仅遍历首次出现的地址。
- `plan/design_probe.md`、`plan/issue.md` 和 probe README 已同步上述 ACPI 与报告 envelope 语义。

新增回归覆盖：

- present media 的零 block size 不得被归类为 missing；
- 空 detail 的 render-to-parse round trip；
- CHECK-before-BEGIN、SUMMARY-before-CHECK、terminal-before-SUMMARY；
- probe producer parser 与 axbuild 独立 consumer parser 的严格 envelope/canonical encoding；
- 未知 ACPI 表公共 checksum 失败；
- 重复未知表物理地址诊断与去重。

验证结果：

- `cargo fmt --all -- --check`：通过。
- `cargo test -p axvisor-uefi-probe --all-targets`：44/44 通过。
- `cargo test -p axbuild --lib uefi_probe`：5/5 通过。
- `cargo xtask clippy --package axvisor-uefi-probe`：通过。
- `cargo xtask clippy --package axbuild`：通过。
- `x86_64-unknown-uefi` target check/clippy：通过。
- `cargo package -p axbuild --allow-dirty`：完成打包并从 crate tarball 独立编译通过。
- `cargo publish -p axbuild --dry-run --allow-dirty`：通过；仅提示当前 `0.4.20` 版本已存在，未上传内容。
- `cargo xtask test`：48 个 std 白名单 package 全部通过。
- `cargo xtask uefi-probe test qemu`：core 双跑、pc、runtime、negative 全部通过；negative 仍只有 `pci-expected/expectation-not-met` 一个逻辑失败。
- `cargo axloader test qemu --target x86_64-unknown-uefi`：HTTP smoke 通过。
- `cargo test -p axbuild --lib`：除一个既有时序型用例外的 750/750 通过；`agent_review_bench::tests::mock_agents_write_artifacts_and_reject_invalid_json` 单独运行通过，但在并行全量运行中偶发临时 Git 仓库 `git commit` 返回 “nothing to commit”。该失败不涉及本轮 probe、parser 或发布资源逻辑，未在本轮扩大范围修改。

QEMU 使用临时 ESP、测试盘和 OVMF VARS 副本；系统 OVMF CODE/VARS 模板及非临时磁盘未被修改。未修改 AxVisor VM/test-suit 配置，变更尚未提交 commit。
