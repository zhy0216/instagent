# ADR 0002 落地：删除 approval / trust / Mode（permission UI 整体移除）

## 意图

ADR 0002 已把"用户 UI 与 permission 管理"降级为非目标，并明确 approval.rs、
trust.rs、REPL 确认路径只是冻结、"后续 todo 择机移除"。本计划执行这次移除，
先例是 ADR 0001 对 Anthropic 的整体删除（commit `7d31300`）。

用户已确认关键取舍：**整个 `Mode` 枚举删掉**（auto/approve/chat 三态、
chat 不给工具的逻辑一并消失），agent 永远直接执行工具，安全依赖 sandbox。

## 目标

- 删除 `src/agent/approval.rs`（Approval / Decision / Confirm / DEFAULT_ALWAYS_ALLOW）。
- 删除 `src/cli/trust.rs`（plugin_surfaces / user_trusted / persist_trust /
  ensure_trusted / 信任确认交互）。
- 删除 `Mode`：`src/config.rs` 的枚举与 FromStr、`Config.mode`、
  `Config.always_allow`、`INSTAGENT_MODE` 环境变量覆盖、`Config::save()`
  （唯一写回方是 approval 的 grant_always；删除后无调用者）。
- `src/agent/mod.rs`：去 `pub mod approval` 与三个 re-export；`AgentCfg.mode`、
  `Agent.approval` 字段与 assemble 中的构造；`execute_calls` 里
  `approval.decide()` 门控（保留其后的 PreToolUse hook 触发点——hooks 机制
  不在 ADR 0002 范围）；`stream_assistant` 里 `grants_tools()` 分支改为恒带
  tools。
- `src/settings.rs`：删 `trusted_plugins` 字段、LayerFile 对应项、合并逻辑与
  相关单测（enabled/disabled 不动）。
- CLI 全面去交互审批/信任：
  - `src/cli/mod.rs`：`--mode` flag（chat/run）、install/enable 的 `--yes`、
    `pub mod trust` 声明与文档注释。
  - `src/cli/assembly.rs`：删信任门控段（L114–156）、proxy provider 未信任
    bail（L185–199）、`Prompter`、`AssemblyOpts.mode/assume_yes/interactive`；
    `build()` 不再收 prompter；trusted_set 直接用发现的 `plugins`。
  - `src/cli/handlers.rs`：chat/run 去 mode 参数与 prompter 样板；
    `confirm_trust` 删；`plugin show` 去 `trusted:` 行与 surfaces 列表。
  - `src/cli/repl.rs`：`/mode` 命令、`Input::Mode/BadMode`、`mode_name`、
    横幅 mode 显示、`rt.agent.approval.confirm` 接线。
  - `src/cli/render.rs`：`CliConfirm`、`parse_confirm` 及其单测。
- 集成测试同步：
  - `tests/cli_e2e.rs`：删 trust 确认用例；install/list/show/disable/enable
    用例去 `--yes`、`trusted:` / trustedPlugins 断言；`output_with_stdin` 若
    再无使用者删掉。
  - `tests/live_e2e.rs`：删 c1（chat mode）与 c2（approve 白名单）用例、
    `trust_liveplug` helper 及其调用（liveplug 无需再预信任）。
  - `tests/fixtures/trusty-plugin/`：保留（install/show 流程仍用），仅更新
    plugin.json 里提到 trust confirmation 的 description。
- 文档：`README.md`、`docs/usage.md`（§5 审批模式与安全整节删除/改写为
  "安全依赖 sandbox"一句，目录重排号或保留标题删内容）、
  `docs/architecture.md`（approval.rs、"信任"字样）、`docs/adr/0002-*`
  状态行更新为"已落地"。历史计划文档 `docs/goose-*.md` 不回改。

## 非目标

- 不动 hooks 机制（PreToolUse 等触发点保留——它是指定扩展点，不是审批 UI）。
- 不动 REPL / rustyline 本身（CLI 仍是调试入口，只是不再有确认交互）。
- 不引入新的 sandbox 集成、不改 provider / tools / session 层。
- 不重排 `docs/usage.md` 章节编号之外的内容。

## 方案

纯删除式重构，一次编译面收敛：Rust 整仓一起编译，lib 与 bin 的引用必须同
一提交内一起清理，所以"涉及 src 的删除 + tests 修复"是一个原子任务；文档
是独立提交。commit 风格照抄 ADR 0001 先例（`feat!:` + ADR 编号）。

保留兼容行为：旧 config.yaml 里残留的 `mode:` / `always_allow:` 键因
`#[serde(default)]` 不 deny_unknown_fields 会被静默忽略，settings.json 里
的 `trustedPlugins` 同理——无需迁移代码。

## 拆解

| 任务 | 内容 | 依赖 | 难度 |
|---|---|---|---|
| 01 | src/ 全量删除 + 单测/集成测试修复（一个 commit，`feat!:`） | 无 | hard |
| 02 | 文档同步：README、docs/usage.md、docs/architecture.md、ADR 0002 状态 | 01 | easy |

02 依赖 01（文档描述必须与删除后的代码一致）。

## 校验

每任务完成时（AGENTS.md 约定）：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

附加验收：

```bash
rg -n "approval|Approval|trustedPlugins|trusted_plugins|always_allow|CliConfirm|Mode::|INSTAGENT_MODE" src tests README.md docs/usage.md docs/architecture.md
```

无输出（`docs/goose-*.md`、`docs/adr/` 历史记录、plans/ 除外）。
REPL 冒烟：`cargo run -- chat --help` 不再出现 `--mode`；
`instagent plugin install <fixture>` 不再提问。

## 风险与假设

- **风险：误删 hooks 链路。** assembly 里信任门控决定哪些插件进
  `Hooks::load` / `CommandTools::load` / MCP 连接；删除门控后这些加载改收
  全体 `plugins`，需保证 `plugins.skipped` 语义不变。
- **风险：live_e2e 有 key 实跑面变化。** c1/c2 删除后 live 用例从 12 条减到
  10 条，属预期。
- **假设：** `Config::save()` 删除后确无调用者（已 grep 确认只剩
  approval.rs 与 config 自身单测）；`Settings::save(cwd, layer)` 仍被
  install enable/disable 使用，保留。
- **假设：** usage.md §5 整节删除后目录锚点需同步，章节号保留原编号跳号
  或顺移均可，以 diff 最小为准。

## 执行结果

- 01-remove-permission-code.md ✅ → commit `d2ebbc8`（feat!: 移除 permission UI——approval.rs、trust.rs、Mode 三态整体删除，ADR 0002），已归档 `todos/done/`。
- 02-sync-docs.md ✅ → commit `590b90e`（docs: ADR 0002 落地后文档同步），已归档 `todos/done/`。
- 校验：两任务各自在独立 worktree 通过 cargo fmt --check / cargo clippy --all-targets -D warnings / cargo test 全绿；残留检查 rg 无 permission 概念命中（仅 INSTAGENT_MODEL、/v1/models 等子串误中）；chat/run --help 无 --mode，plugin install --help 无 --yes。
- blocked / deferred：无。
- 本轮 Herdr workspace（w26/w27）、worktree、任务分支已全部清理；provider_proxy::restart_is_bounded_to_once_per_call 在并行负载下出现过一次瞬时失败，单跑与复跑均绿，与本计划无关。
