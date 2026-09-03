difficulty: easy

# 02 文档同步（ADR 0002 落地后）

01 已删除 approval/trust/Mode 代码，本任务把面向用户的文档改到与之一致。
只改文档，不碰 src/ 与 tests/。

## T1 · README.md

- 要做什么：
  - 快速上手 config 示例删 `mode: approve` 行；
  - "配置与环境变量"表：config.yaml 行去 mode / `always_allow`；
    settings.json 行去 `trustedPlugins`；环境变量段去 `INSTAGENT_MODE`；
  - 删"read / tree 在默认审批白名单"设计语义条目；
  - REPL 注释 `/help /exit /clear /compact /mode /tools` 去 `/mode`；
  - 插件管理命令块去 `[--yes]`；信任确认段落（"信任确认（instagent plugin
    enable 回答 yes…）"）改写为一句"插件组件直接加载，安全由 sandbox 隔离
    承担（ADR 0002）"；
  - 手动走查清单删第 6 步（approve 审批提示），第 7 步信任走查删改。
- 预计修改：`README.md`。
- 验收：`rg -in "approve|always_allow|trusted|INSTAGENT_MODE|/mode" README.md`
  无命中（决策记录索引里指向 ADR 0002 的链接除外）。
- 前置依赖：01。

## T2 · docs/usage.md

- 要做什么：
  - §5"审批模式与安全"整节删除（含 5.1 三种模式、5.2 插件信任、审批提示
    示例）；目录同步；后续章节编号顺移或跳号取 diff 最小；
  - §2 快速上手、§3 命令参考去 `--mode` / `--yes` / `/mode`；§4.1 配置表去
    `mode` / `always_allow`，§4.2 环境变量表去 `INSTAGENT_MODE`，§4.3
    settings 去 `trustedPlugins`；
  - §6 REPL 表删 `/mode` 行、启动横幅说明去 mode；§8 插件管理去信任确认
    描述；正文其他 §5.x 锚点引用（如 §9.4 "受信任机制约束（§5.2）"）改
    写为 sandbox 一句或删除；
  - 文首加一句定位："instagent 运行在 sandbox 内，工具直接执行，安全边界
    由 sandbox 承担（见 docs/adr/0002）。"
- 预计修改：`docs/usage.md`。
- 验收：`rg -in "approve|always_allow|trustedPlugins|INSTAGENT_MODE|/mode|信任" docs/usage.md`
  仅剩 sandbox 定位句相关措辞。
- 前置依赖：01。

## T3 · architecture.md 与 ADR 状态

- 要做什么：
  - `docs/architecture.md`：模块表 `src/agent/` 行去"审批（approval.rs）"、
    `src/plugin/` 行去"信任"、`src/config.rs settings.rs` 行去"信任状态"；
  - `docs/adr/0002-sandbox-agent-no-ui-permission.md`：状态"已接受"下加
    "- 落地：approval/trust/Mode 已于 commit（01）整体移除"，正文"冻结、
    后续 todo 择机移除"改为过去时描述（保留决策记录原文，用追加行说明）。
- 预计修改：`docs/architecture.md`、`docs/adr/0002-*.md`。
- 验收：`rg -n "approval" docs/architecture.md` 无命中；ADR 0001 先例格式
  一致；`cargo fmt --check && cargo clippy --all-targets -- -D warnings &&
  cargo test` 全绿（文档改动不应影响，跑一遍确认）。
- 前置依赖：01、T1、T2。
