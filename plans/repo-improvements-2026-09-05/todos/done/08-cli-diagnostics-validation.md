difficulty: hard

# 08 · CLI 诊断、配置与跨模块回归

优先级：P1。模型：`bailian-token-plan/qwen3.8-max`。方案发现：D01–D03、A04 的 CLI 接线，以及前序修复的 CLI 验收。

前置依赖：03-agent-turn-continuation、04-plugin-settings-recovery、05-proxy-lifecycle、06-bundled-snapshots、07-tool-inventory-io 全部合入。

## 涉及文件

- `src/cli/mod.rs`
- `src/cli/assembly.rs`
- `src/cli/repl.rs`
- `src/cli/handlers.rs`
- `src/config.rs`
- `src/tools/skills.rs`
- `tests/cli_e2e.rs`

## T1 · 默认失败诊断可见

要做：init_logging 未配置 RUST_LOG 时显示 warning 到 stderr，保留显式过滤设置。assembly 将 auto_update_all 的顶层 Err 变成 notes。SkillsSource 扫描区分正常不存在与读取/权限/超限失败，后者发有界且带路径的 warning。复用 hook/command/Registry 已有 warning，不新增审批或改变 fail-open 默认。

预计修改文件：`src/cli/mod.rs`、`src/cli/assembly.rs`、`src/tools/skills.rs`、`tests/cli_e2e.rs`。

验收：不设 RUST_LOG 的真实 CLI 中，失败 SessionStart/PreToolUse hook、MCP inventory 失败、超大/不可读 SKILL.md 都有 stderr 诊断；健康来源仍可用，stdout 不含诊断。检查不重复放大同一错误、不输出真实环境值或完整损坏文件。可使用仓库既有 fake fixtures，不能访问真实插件根。

前置依赖：04-plugin-settings-recovery、07-tool-inventory-io。

## T2 · 完整配置校验与手动压缩取消

要做：合并 -m 等 CLI override 后再校验 model/provider 等参数，空白值在 provider/MCP 启动前报错；config.yaml 有界读取（建议 1 MiB），错误带来源且无 raw 回显；compaction_threshold 转为 f32 后仍须正且有限。repl::compact_now 接入 03 的可取消入口，取消后 printer/watcher 正确收尾，REPL 可继续；错误返回也不遗留打印任务。

预计修改文件：`src/config.rs`、`src/cli/assembly.rs`、`src/cli/repl.rs`、`src/cli/handlers.rs`、`tests/cli_e2e.rs`。

验收：`-m '   '`、下溢小阈值、超大 config 均早期失败且文件未改；普通用户 config 与环境优先级不变；/compact 慢响应中 Ctrl-C 可取消，原会话仍可继续；不引入项目级 config.yaml 合并这一新功能。

前置依赖：03-agent-turn-continuation。

## T3 · 将隔离复现变成 CLI 回归

要做：扩展现有 CLI e2e，覆盖流字节切分、无完成标记的工具流、重复调用 ID、压缩后继续、取消后继续/恢复、空白名单启用与跨层清空、session 错误不回显假密钥、安装不删异插件恢复副本、bundled 不加载旧文件。

预计修改文件：`tests/cli_e2e.rs`；仅为接线修复使用以上 CLI 文件，不重新修改前序任务模块。

验收：与 plan 隔离复现表对应的 8 个行为全部反转；验证 stdout/stderr、退出码、请求数、工具标记文件和 JSONL，不仅匹配错误字符串。使用 HTTP mock/可控 chunk writer、临时目录、进程组和硬超时；交互取消可沿用现有 PTY/管道 fixture。若暴露前序实现缺陷，交协调器在对应任务修正，不越过本白名单。

前置依赖：本任务全部跨文件前置依赖和 T1/T2。

## 校验与完成

```bash
cargo test config
cargo test tools::skills
cargo test --test cli_e2e
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

一个本地 commit。08 完成后 09 可依据最终行为写文档；不新增 API key 或在线测试硬门槛。

## 完成记录（2026-09-05）

实现：`src/cli/mod.rs`、`src/cli/assembly.rs`、`src/cli/repl.rs`、
`src/config.rs`、`src/tools/skills.rs` + `tests/cli_e2e.rs` 新增 19 项
（`src/cli/handlers.rs` 经检查无需改动：`run_one_turn` 已 await printer，
`report_session_hook` 与 D4 错误行语义不变）。

- T1：`init_logging` 缺省 `off`→`warn`（显式 `RUST_LOG` 优先，`RUST_LOG=off`
  同一失败保持安静已用例锁定）；`assembly` 的 `auto_update_all` 顶层 `Err`
  进 `notes`（原 `if let Ok` 静默吞掉）；`scan_skills_root` 区分不存在（静默）
  与读取/权限/超限失败（带路径有界 warning，512 字符截断），单条目读取失败
  同样 warn（原 `.flatten()` 静默丢弃）。hook/command/Registry 复用已有
  warning，未新增审批、fail-open 默认不变。
- T2：`config.yaml` 有界读取（`MAX_CONFIG_FILE_BYTES` 1 MiB，`take(MAX+1)`，
  超限/非 UTF-8/IO 错误带来源、不回显原文、原文件不动）；`compaction_threshold`
  f64 校验后加 f32 下溢复检（`1e-46`→`0.0` 拒绝）；新增
  `Config::validate_merged`（空白 provider/model/shell + 非正/非有限 f32 阈值），
  `load` 返回前调一次，`assembly` 在 `-m` 合并后再调一次（来源指向 CLI 合并结果），
  空白 `-m` 在 provider/MCP 启动前失败。`repl::compact_now` 改调
  `compact::force_cancelable` + 单次 Ctrl-C watcher：取消→`(compaction cancelled)`、
  无历史→`(nothing to compact)`、成功仍 `(compacted)`（旧 e2e 子串兼容）；
  成功/取消/错误三路都 `drop(tx)` + `watcher.abort()` + `printer.await`，无遗留任务。
- T3：`tests/cli_e2e.rs` 15→34 项。8 个隔离复现全部反转并多维断言
  （stdout/stderr/退出码/请求数/标记文件/JSONL 行数内容）：
  P01 中文切分（裸 TCP 可控两段写入）得 `中文🙂`；
  P02 无完成标记工具流错出（`without completion`）、`p02-marker` 不存在、
  单请求、header+user 保留；
  A01 复用 ID 错出（`reuses tool use id`）、双标记都不存在；
  A02 `hello`→`/compact`→`next` 全程 0 退出、双答案、无 `consecutive`、
  摘要保留且 `next` 并入；
  取消后继续（慢 shell + SIGINT 后 `again` 成功，2 请求，孙进程回收）；
  I01 空白名单 `enable` 写回白名单、`list` 显 enabled，跨层清空后 alpha/beta 均
  disabled（未退化黑名单）；
  S01 损坏 header 含 `sk-TESTSECRET08-…` 假密钥形态，`sessions list` 0 退出、
  `(no sessions)`、stderr 仅路径 warning、无回显；
  I02 安装异插件后 `.replaced-lost-*` 恢复副本保留；
  I03 旧布局幽灵 provider 文件保留在盘但 `provider: ghost` 报
  `unknown provider`（健康 fake 路径不受影响）。
  另有 T1（SessionStart/PreToolUse 失败 warning + `RUST_LOG=off` 安静对照、
  超大/不可读 SKILL.md 带路径 warning、MCP note 无环境值回显）与 T2
 （空白 `-m`/下溢阈值/超大 config 早期失败且零请求文件未改、慢摘要中 Ctrl-C
  取消后无摘要痕迹且 REPL 可继续、3 请求）。

校验：`cargo test config`（36 项）、`cargo test tools::skills`（13 项）、
`cargo test --test cli_e2e`（34 项，两遍）、`cargo fmt --check`、
`cargo clippy --all-targets -- -D warnings`、
`env -u TOKEN_PLAN_API_KEY cargo test` 全绿
（lib 457、bin 12、cli_e2e 34、live 10 skip、mcp 14、proxy 15 等；
预置 `TOKEN_PLAN_API_KEY` 时 live 试图联网 429，系环境限制，按 todo 要求
unset 后跑）。偶发失败记录：本任务执行期间未遇到需重跑的偶发失败。

前序缺陷发现：无（T3 接线未暴露前序模块缺陷；P02/A01 的 CLI 错误文案分别来自
01 的传输层与 03 的执行前校验，行为符合预期，无需交协调器）。
剩余风险：不可读 skill 用例在 root 用户下自动跳过（000 仍可读时无法模拟）；
compact 取消用例依赖 30s 延迟摘要与请求计数轮询，在极慢 CI 上靠 `WAIT` 60s/
`TIMEOUT` 120s 兜底。
