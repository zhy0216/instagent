difficulty: hard

# 01 删除 approval / trust / Mode（src + tests，原子 commit）

按 ADR 0002 与 `plans/adr2-remove-approval-trust/plan.md` 执行纯删除式重构：
permission/approval/trust/Mode 全部移除，agent 永远直接执行工具。commit
风格照抄 ADR 0001 先例（`feat!:` 前缀，message 注明依据 ADR 0002）。

## T1 · lib 侧删除（config / agent / settings）

- 要做什么：
  - 删文件 `src/agent/approval.rs`；
  - `src/config.rs`：删 `Mode` 枚举与 `FromStr`、`Config.mode`、
    `Config.always_allow`、`INSTAGENT_MODE` 覆盖、`Config::save()`
    （唯一调用方是 approval 写回；删后单测里 save/load round-trip 改
    手写文件验证 load，或删该用例中 save 依赖）；相关单测
    （`missing_file_falls_back_to_defaults` 的 mode 断言、
    `save_load_round_trip`、`partial_yaml_uses_defaults_for_absent_fields`、
    `env_vars_override_file` 的 mode 部分、`invalid_mode_env_is_error`）
    同步删改；
  - `src/agent/mod.rs`：删 `pub mod approval` 与 `Approval/Confirm/Decision`
    re-export、`AgentCfg.mode`、`Agent.approval` 字段、assemble 中
    `Approval::new(...).with_config(...)`；`execute_calls` 删
    `self.approval.decide(call)` match，PreToolUse hook 检查直接前置执行；
    `stream_assistant` 的 `if self.approval.grants_tools()` 改为恒
    `self.tools.list().await`；测试 helper `fn agent(...)` 与
    `fn auto()` 去 mode/approval 参数，`approve_deny_*`、`approve_allow_*`、
    `chat_mode_sends_no_tools`、`assemble_from_config` 的 always_allow
    断言删除；
  - `src/agent/event.rs` 顶部注释去掉 approval::Confirm 指涉；
  - `src/settings.rs`：删 `trusted_plugins` 字段、`LayerFile.trusted_plugins`
    与合并分支、相关单测（`trusted_plugins_merge_independently` 等）；
    `src/plugin/discovery.rs` 测试里 `trusted_plugins: vec![]` 字面量删除。
- 预计修改：`src/agent/approval.rs`（删）、`src/agent/mod.rs`、
  `src/agent/event.rs`、`src/config.rs`、`src/settings.rs`、
  `src/plugin/discovery.rs`。
- 验收：`cargo check --lib` 通过（bin 允许暂红，T2 修复）。
- 前置依赖：无。

## T2 · bin 侧删除（cli）

- 要做什么：
  - 删文件 `src/cli/trust.rs`；
  - `src/cli/mod.rs`：删 `pub mod trust`、chat/run 的 `--mode` flag、
    install/enable 的 `--yes` flag、`Mode` import，文档注释与 help 文案
    （`/mode` 等）同步；
  - `src/cli/assembly.rs`：删信任门控段（约 L114–156）、proxy provider
    未信任 bail（约 L185–199）、`Prompter`、`AssemblyOpts.mode/
    assume_yes/interactive`；`build(&opts)` 单参数；MCP/CommandTools/Hooks
    的加载集合直接用 `plugins`（发现结果，含 skipped 语义不变）；
    信任门控相关测试（`untrusted_exec_plugin_*`、
    `untrusted_proxy_provider_*`）删除，`run_task_end_to_end_*` 适配新
    `build` 签名；`fixtures::add_exec_surfaces` 若无其他使用者则删；
  - `src/cli/handlers.rs`：chat/run 签名去 `mode`、去 prompter/stdin 样板；
    `confirm_trust` 删；install/enable 不再提问；`plugin show` 删
    `trusted:` 行与 surfaces 列表段；
  - `src/cli/repl.rs`：删 `Input::Mode/BadMode`、`/mode` 分支、`mode_name`、
    横幅中 mode、`rt.agent.approval.confirm` 接线、`print_help` 的 mode
    行；`classify` 的 mode 测试删除；
  - `src/cli/render.rs`：删 `CliConfirm`、`parse_confirm` 与其单测、
    `Confirm/Decision/ToolCall/async_trait` import，模块注释更新。
- 预计修改：`src/cli/trust.rs`（删）、`src/cli/mod.rs`、
  `src/cli/assembly.rs`、`src/cli/handlers.rs`、`src/cli/repl.rs`、
  `src/cli/render.rs`、`src/main.rs`（若有引用）。
- 验收：`cargo clippy --all-targets -- -D warnings` 零警告（含 T3 完成前
  tests/ 若仍红可暂列 T3，但 `cargo check --all-targets` 的 src 部分绿）。
- 前置依赖：T1。

## T3 · 集成测试同步

- 要做什么：
  - `tests/cli_e2e.rs`：删 `plugin_trust_confirmation_via_piped_stdin`；
    `plugin_install_list_show_disable_enable_with_yes` 改名去 `--yes`，
    删 "trusted" 输出与 `trustedPlugins` 断言（show 断言去 `trusted: true`
    行）；`output_with_stdin` 无使用者则删；
  - `tests/fixtures/trusty-plugin/plugin.json`：description 去掉
    "for trust confirmation" 措辞（目录与插件名不动）；
  - `tests/live_e2e.rs`：删 `live_c1_chat_mode_has_no_tools`、
    `live_c2_approve_whitelist`、`trust_liveplug` helper 及全部调用
    （liveplug 经 `--plugin` 加载后无需预信任即生效）。
- 预计修改：`tests/cli_e2e.rs`、`tests/live_e2e.rs`、
  `tests/fixtures/trusty-plugin/plugin.json`。
- 验收：`cargo fmt --check && cargo clippy --all-targets -- -D warnings &&
  cargo test` 全绿（live_e2e 走 skip 路径）；
  `rg -n "approval|Approval|trustedPlugins|trusted_plugins|always_allow|CliConfirm|Mode::|INSTAGENT_MODE|trust" src tests`
  无命中（`guard.sh`/fixture 路径等中性词除外，实际以不残留
  permission 概念为准）。
- 前置依赖：T1、T2。

## T4 · 提交

- 要做什么：T1–T3 一个 commit：`feat!: 移除 permission UI——approval.rs、trust.rs、Mode 三态整体删除（ADR 0002）……`，
  message 里写明依据 `docs/adr/0002-*`，行为变化（默认全放行、旧 config
  的 mode/always_allow 键被静默忽略）。
- 验收：工作区干净，三条校验命令全绿。
- 前置依赖：T3。
