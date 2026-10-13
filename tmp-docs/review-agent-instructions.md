# 只读分支审查代理指令（共享版）

仓库：/home/young/code/tgoskits。工作区当前检出在 feat/vPCI_enum，tmp-docs/ 未跟踪。

## 硬性约束
- 只读审查：禁止修改任何文件、git checkout/switch/commit/push/worktree、cargo 任何命令、QEMU、向 GitHub 提交任何内容。gh 在本机已坏，不要用。
- 只能用只读命令：`git show <branch>:<path>`、`git diff origin/dev...<branch>`、`git grep -n <pat> <branch> -- <paths>`、`git log`、`git rev-parse`，以及 read/grep/glob 工具。必要时可少量 curl GitHub API（未认证 60 次/小时，非必要不用）。
- 审查其他分支时，文件内容一律用 `git show <branch>:<path>` 读取，不要依赖工作区文件（工作区是 vPCI_enum 的内容）。

## 两轮流程（必须两轮）
第一轮：完整静态审查，五类视角全覆盖：可维护性 / 正确性 / 安全与健全性 / 硬件与 ABI / 文档与兼容性。对照设计文档逐条核验。
第二轮：逐条核实第一轮每个发现：重新读精确代码行，确认 path:line 与严重度，补触发场景与证据；再主动找遗漏。无法证实的删除或标"待确认"。

## 每条发现的格式
级别【阻塞/建议/信息】+ path:line + 问题描述 + 触发场景与证据 + 建议修改方式。阻塞项必须含七段粗体标题：**为什么需要改动**、**改动收益**、**改动前逻辑（基准分支）**、**改动后逻辑（当前分支/PR）**、**触发场景与证据**、**问题级别**、**建议修改方式**。

## 工作纪律（重要）
- 每次只做一个小的核实步骤，紧接着调用工具（git show / git grep / read）验证，然后写下结论。禁止长时间连续推理而不调用工具。
- 每完成一个文件/模块的核实就把它标为完成，最后统一汇总。
- 如果任务太大，可以分模块逐段进行，但最终必须给出完整报告。
- 不臆测：所有断言都要有工具调用得到的证据（引用实际代码行）。

## 必读文件（先读，再开始审查）
1. /home/young/code/tgoskits/.claude/skills/review-single-pr/SKILL.md
2. /home/young/code/tgoskits/docs/guideline/code-quality.md（1929 行，分段读完）
3. /home/young/code/tgoskits/docs/guideline/feature-development.md（398 行）
4. 对应设计文档（见各分支指令）

## 最终输出结构（中文 Markdown）
```
# <branch> 审查报告
## 审查范围与基准
## 设计与规范符合性
## 发现清单（每条含级别、path:line、证据、建议）
## 测试与验证评估
## 重复/重叠分析
## 优点
## 总体结论（APPROVE / REQUEST_CHANGES 语义，仅报告不提交）
```
