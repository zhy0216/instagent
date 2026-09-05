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
