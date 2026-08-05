# UEFI P0-R2 评审遗留 Issue 清单

> 来源：`feat/uefi-p0-r2` 分支任务 0–5（axbuild OVMF 校验、axvm loader layout 校验、axdevice trace、ovmf-entry 用例、文档）的多个 subagent 评审中标记为 `action=issue` 的意见与遗留事项。
> 说明：这些意见无需在实现轮内修复，统一收敛到本清单跟踪，后续按优先级单独处理。

## 总览

| 编号 | 标题 | 类型 | 优先级 | 影响 |
| --- | --- | --- | --- | --- |
| 1 | agent_review_bench flaky 测试 | 测试基建 | 高（多轮评审反复出现） | CI |
| 2 | overview.md 旧路径指代过时 | 文档 | 中 | 文档 |
| 3 | boot-debugging.md 措辞精度 | 文档 | 低 | 文档 |
| 4 | CI 矩阵变更时文档同步 | 文档 | 低（触发式） | 文档/CI |
| 5 | 固定常量跨 crate 重复定义 | 代码一致性 | 中 | 代码 |
| 6 | CI gating 前硬件验证 | 硬件验证 | 高（任务 4 遗留） | CI/测试 |

## 1. agent_review_bench flaky 测试（测试基建，高优先级）

- **位置**：`scripts/axbuild/src/agent_review_bench/sandbox.rs`，`ReviewSandbox::create`（约 :236 的 git helper bail）
- **问题描述**：`mock_agents_write_artifacts_and_reject_invalid_json` 测试间歇失败。隔离单跑 25–40 次约 15–30% 概率失败；批量运行时约 5% 概率失败。失败点是第二次 git commit 报 `'nothing to commit, working tree clean'`，git 退出码 1。
- **根因/背景**：
  - git 2.34.1 的 commit 会触发 `git maintenance run --auto`，与紧随其后的 add/commit 产生竞态；
  - tar 解包覆盖工作树后 git 索引陈旧，叠加本机 10–11 秒的文件时间戳时钟偏差；
  - 已确认该模块未被 `feat/uefi-p0-r2` 分支改动（分支上 git diff 仅涉及 `axvisor/test/qemu.rs`），非本分支引入，属既有环境 flake。
- **建议处理方式**：
  - `ReviewSandbox::create` 的 `commit_all` 改为检查 `'nothing to commit'` 输出时回退处理，或
  - 在测试环境禁用 `maintenance.auto`；
  - CI 失败时按已知 flake 单独重跑确认。
- **影响**：CI 并行跑 axbuild 全量测试可能偶发失败，需在合入前按上述方式规避。

## 2. overview.md 旧路径指代过时（文档，中优先级）

- **位置**：`docs/docs/architecture/axvisor/overview.md` 第 107 行「架构适配」段；同文件第 19/61/63/74 行的 `hal/arch/*`、`src/hal/*` 描述一并过时。
- **问题描述**：文档以 `hal/arch/` 指代四套架构适配的实现位置，但实际代码位于 `virtualization/axvm/src/arch/<arch>/`（x86_64 经 `x86_vcpu`/`x86_vlapic` 核心实现）；`os/axvisor/src` 下已无 `arch/` 目录（仅有 `shell`/`guest_console`/`manager` 等）。「运行时模块」表中 `src/hal/*` 描述同样过时。
- **根因/背景**：改动前文档已存在过时表述；任务 5 新增 x86_64 行恰好放大该偏差。
- **建议处理方式**：统一更新为当前分层表述（改为 `virtualization/axvm` 或泛化描述）。
- **影响**：文档（架构读者定位代码路径时会指向不存在的目录）。

## 3. boot-debugging.md 措辞精度（文档，低优先级）

- **位置**：`.claude/skills/arch-platform-porting/references/boot-debugging.md` 第 139 行 Wiring points 第一条。
- **问题描述**：'matches the full case name exactly' 与源码不完全逐字一致：`indexed_case_matches_selected`（`scripts/axbuild/src/test/qemu/discovery.rs:557-568`）对 `case.name` 是精确相等，但对 `display_name` 还接受 `{selected}/` 前缀形式（如 `ovmf-entry/<subdir>`）。
- **根因/背景**：措辞精度问题，不构成误导——对 ovmf-entry 的实际使用（名字为 `ovmf-entry-vmx`/-svm）精确匹配结论成立。
- **建议处理方式**：补充 `display_name` 前缀形式的精确描述。
- **影响**：文档（描述与实际匹配逻辑存在细微出入）。

## 4. CI 矩阵变更时文档同步（文档，低优先级，触发式）

- **位置**：`docs/docs/build/axvisor/test.md` §3.3 与 `docs/docs/build/ci.md` 的 non-gating 表述。
- **问题描述**：当前表述基于 `ci.yml` 矩阵现状（仅 smoke-svm/smoke-vmx 两个 x86_64 self-hosted job）；CI 矩阵变更时表述会与实际脱节。
- **建议处理方式**：CI 矩阵变更时跟进同步更新相关文档。
- **影响**：文档/CI（仅矩阵调整时触发，非当前问题）。

## 5. 固定常量跨 crate 重复定义（代码一致性，中优先级）

- **位置**：
  - `scripts/axbuild/src/axvisor/ovmf.rs`：`OVMF_PROFILE_NAME` / `OVMF_CODE_BASE` / `OVMF_CODE_SIZE` / `OVMF_VARS_BASE` / `OVMF_VARS_SIZE` / `OVMF_COMBINED_SIZE` / `OVMF_RESET_VECTOR`；
  - `virtualization/axvm/src/arch/x86_64/boot/mod.rs`：`FIXED_OVMF_PROFILE` / `FIXED_OVMF_CODE_GPA` / `FIXED_OVMF_CODE_SIZE` / `FIXED_OVMF_RESET_VECTOR`。
- **问题描述**：axvm 禁止依赖 axbuild，两处值需人工保持同步；若 axbuild 侧调整 OVMF bundle 布局，axvm 侧的校验会静默失效，且没有编译期信号提示不同步。
- **建议处理方式**：
  - 评估收敛为单一来源（如 build 期生成文件），或
  - 在 `boot/mod.rs` 增加编译期一致性校验；
  - 至少提供一个集成测试同时引用两处值。
- **影响**：代码（OVMF bundle 布局调整时校验可能静默失效）。

## 6. CI gating 前硬件验证（硬件验证，高优先级，任务 4 遗留）

- **内容**：ovmf-entry 用例保持 non-gating，直到固定 DEBUG bundle 在 VMX 与 SVM 两台机器上各验证一次 SEC marker。
- **背景**：任务 4 实现时本机 WSL2 嵌套虚拟化存疑，未完成真实验证；评审 agent 实测 RELEASE OVMF 无 DEBUG SEC marker，因此必须以固定 DEBUG bundle 在真实硬件上确认。
- **建议处理方式**：在 VMX（Intel）/SVM（AMD）两机各运行：

  ```bash
  cargo xtask axvisor test qemu --arch x86_64 --test-group uefi --test-case ovmf-entry-{vmx,svm} --firmware-bundle-path <bundle>
  ```

  确认 SEC marker 后再将 ovmf-entry 用例升级为 gating。
- **影响**：CI/测试（升级 gating 的前置条件，未完成前用例保持 non-gating）。
