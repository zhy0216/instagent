difficulty: medium
agent: inherit

# 10 · 用户契约和集成验证收口

对应方案 D01，并汇总 01–09。前置依赖：01-agent-completion、02-compaction-integrity、03-file-tool-io、04-session-write-budgets、05-plugin-input-limits、06-local-install-integrity、07-template-input-budget、08-provider-required-key、09-live-test-isolation 全部已合入。

状态：已完成并归档（2026-09-06），截至协调器在 `48305f7` 上完整 CI/MSRV 均退出 0 的最终验证，T1/T2 任务验收完成。后续集成中曾出现的 MCP 启动 PID、clone 取消 PID 失败及初次两次 proxy 计数失败均保留，已有波动根因尚未解决，详见末尾追加记录。真实模型未运行。

## 涉及文件

- `README.md`
- `docs/usage.md`
- `docs/architecture.md`
- `docs/release.md`

本任务只做文档和验证；若检查发现代码回归，交回拥有该文件的任务处理，不在文档 commit 中混入源码修复。

## T1 · 按最终实现同步使用与资源契约

工作：更新模板展开 1 MiB 上限、文件普通权限/读写预算、会话写入拒绝策略、组件文件预算和必需凭据空值判定。说明异常响应/截断摘要的失败与恢复行为、未回答任务保留。介绍默认离线测试和显式 live ignored 命令，分清 Rust 真执行数、ignored 数和在线验证状态。

预计修改文件：`README.md`、`docs/usage.md`、`docs/architecture.md`、`docs/release.md`。

验收：
- [x] 数值、命令、结果/退出码、文档链接与合入代码一致。
- [x] 不恢复 REPL/审批，不夸大为完整 ACL/图像解码/全路径隔离/跨进程事务。
- [x] 不把工具外部副作用写成可随会话预算错误回滚。
- [x] 保留历史线上超时与 proxy 采样的证据口径；新记录明确当前实际验证结果。
- [x] 文档变更无需增加镜像实现的测试，执行现有仓库门槛即可。

前置依赖：01–09 全部合入。

## T2 · 最终集成态校验

工作：运行仓库要求和扩展 CI 检查，确认计划没有引入依赖/feature/module 变更，校验任务产生的输出不污染源码夹具。失败由对应任务修复后再重新验证相关范围；通过后不反复压力测试。

预计修改文件：仅在 `docs/release.md` 记录对用户有用的验证结论；计划执行记录由协调器维护，不改旧 done。

验收命令：
- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`
- `bash scripts/ci.sh`（与上述命令重复时可用该脚本的一次完整输出作为对应验证证据）
- `cargo +1.93.1 check --locked --all-targets`

验收：
- [x] Rust、Python、rustdoc、release、--help 和 MSRV 全部通过。
- [x] 默认 cargo test 不因环境存在凭据而联网；live 显式运行及其状态另记，不能把 ignored 计成在线通过。
- [x] 未安装 cargo-audit 时如实标注；不升级依赖、安装全局工具或修改扫描豁免。
- [x] 最终源码夹具无新增运行产物，未处理任何本轮开始前的用户目录内容。

前置依赖：T1。

## 校验

每个 commit 执行：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

09 合入前，给执行测试的进程移除 `TOKEN_PLAN_API_KEY`（`env -u TOKEN_PLAN_API_KEY cargo test`），记录 live 用例门控返回；09 合入后默认 cargo test 应显示 live ignored，不因继承凭据而联网。不得修改全局环境、真实凭据或共享源码夹具。实现与行为回归同一 commit；不改依赖/feature、`src/lib.rs` 模块树、其他任务文件或任何既有 done 归档。

## 完成记录

实现基线为 `dd382aa`（01–09 均在当前分支历史中）。只更新四份用户文档、本 todo 归档与队列 README 的 10 状态/链接；没有修改源码或新增镜像测试。

T1 验收证据：

- `docs/usage.md` §3/§8.7 对照 `commands::expand_bounded` 与 CLI：模板文件 256 KiB、展开结果 1 MiB，按 UTF-8 字节及所有占位符受检预估；参数 trim、内部文本/两个换行追加语义准确。超限 `failed` / 1、不发模型请求、新会话 id 为 null，已恢复会话保留 id。
- §5.1 对照 `tools/builtin/fs.rs`：读/edit 输入与 write/edit 结果均为 32 MiB；区别 metadata 超限错误、read 读取中增长的窗口截断提示和 edit 增长拒绝。Unix 普通 rwx 保留、0600 排他临时文件、新文件 umask、特殊位/ACL/扩展属性/属主/硬链接边界明确；图片先校验再记入 64 MiB 会话预算，不声称完整解码。
- §6 对照 `session.rs` 和 `agent/compact.rs`：header 64 KiB、消息行 96 MiB、文件 256 MiB，计入 JSON 转义与总量换行；超预算拒绝保留内存/主文件/备份，但不回滚工具外部副作用。截断/异常/带工具/缺 Done/Done 后事件的摘要不覆盖历史，未回答 Text/Image 块及 resume 新任务有序保留。
- §7.2/§8.9/§9.2 对照安装与各组件 loader：五类 JSON 各 1 MiB 实际读取限制；各来源 fatal/skip 策略分开，hooks 加载错误不混同脚本 fail-open；安装 staging 双向重叠拒绝、普通重装兼容；必需凭据缺失、不可读、空串/Unicode 空白构造失败，不回显原值，keyless 保持支持。
- §9.5 对照 agent 完成状态检查：异常响应在工具/PreToolUse/PostToolUse/ToolStart 前拒绝；保存可校验的助手与配对错误结果，受会话预算约束，保留失败与取消终态。纠正旧文档把所有非空 finish_reason 当作成功的表述。
- README、architecture、release 同步以上契约；保留 headless/sandbox/单进程边界。release 分别保留基线 10 个 live 180 秒超时、旧门控返回不算在线、历史 proxy 采样、04 两项未归因计数失败与本次实际结果。
- 显式在线命令为 `cargo test --test live_e2e -- --ignored`，本任务未运行。用 [09 已完成的缺 key/空白负向验收](09-live-test-isolation.md) 说明显式请求时立即失败机制，不重复运行真实模型，不读取或打印真实凭据、不修改全局环境。

T2 校验结果：

| 命令 | 结果 |
| --- | --- |
| `bash scripts/ci.sh`（初次收口最终第三次） | 退出 0，完整覆盖 fmt/clippy/test/Python/rustdoc/release/help |
| `cargo fmt --check` | 通过，使用最终完整脚本输出作为证据 |
| `cargo clippy --all-targets -- -D warnings` | 通过，无 warning，使用最终完整脚本输出 |
| `cargo test` | 662 passed、0 failed、10 ignored，doc-test 0，使用最终完整脚本输出 |
| Python（ResourceWarning 作为错误） | 11 passed，使用最终完整脚本输出 |
| `cargo rustdoc --lib -- -D warnings` | 通过，使用最终完整脚本输出 |
| `cargo check --release --all-targets` | 通过，使用最终完整脚本输出 |
| `cargo run -q -- --help` | 通过，使用最终完整脚本输出 |
| `cargo +1.93.1 check --locked --all-targets` | 退出 0 |
| `cargo test --test provider_proxy` | 首次完整失败后的隔离复跑 15/15 通过；不计入最终总数 |
| 中间补跑 `cargo test --test session_recovery --test tool_inventory`、`cargo test --doc` | 分别 11 + 4 passed、doc-test 0；不计入最终总数 |

Rust 总数只累加各 target 的最终 `0 filtered out` summary：lib 527、bin 11、agent_continuation 24、cli_e2e 53、live_e2e 离线 2（在线 10 ignored）、mcp_e2e 15、provider_proxy 15、session_recovery 11、tool_inventory 4，共 662。lib 内 3 个子 harness 的一测试输出不重复计数。

初次收口前两次完整 `bash scripts/ci.sh` 均退出 101：`tests/provider_proxy.rs:559` 的 `cancelled_restart_candidate_is_reaped` 在初始启动计数断言处失败（2 != 1），target 为 14 passed、1 failed；该断言发生在取消场景执行之前。首次失败后隔离复跑 15/15 通过，第二次完整仍失败，第三次原样完整通过。未修改 proxy 源码、测试或 fixture，也未确定首次候选退出的具体原因；不将其直接归因于历史端口竞态、04 的另两个计数失败或确定的环境原因。第三次通过只满足初次收口门槛，不证明已有波动风险消失；该阶段通过后未再做压力复跑。

全部日志与夹具快照保留在 `/tmp/instagent-docs-validation-20260906-t59d4ug9/`：`ci.log` / `ci-recheck.log` 为失败的两次完整输出，`provider-proxy-recheck.log` 为隔离通过，`ci-third.log` 为最终完整通过，`msrv.log` 为 MSRV；另保留中间独立检查日志。四份用户文档的本地链接和锚点检查通过；`git diff --check` 通过。

相对 `892dcfd`，`Cargo.toml`、`Cargo.lock`、`rust-toolchain.toml`、`src/lib.rs` 无差异；本轮未改依赖、feature、模块树、扫描豁免或安装全局工具。本机未安装 cargo-audit，完整脚本如实 skipped，不能称为安全审计通过。

源码 `tests/fixtures` 校验前后 73 个路径/类型/权限条目与文件 SHA-256 相同，liveplug 无 `.hook-out` 新产物；07 CLI 与 09 live 的组合隔离在全量回归中成立。用户指定的 `/tmp/instagent-hook-out-20260906-9kg222l8/liveplug-hook-out` 及任何先前备份/用户目录内容均未处理。真实模型未重试、未验证；初次收口时无剩余验收 blocker，后续集成复验状态见以下追加记录，proxy 计数波动仍作为风险交接。

## 后续集成复验追加记录（2026-09-06）

- 协调器独立验证 `b01e30e`，`bash scripts/ci.sh` 退出 101；fmt/clippy、lib 527、bin 11、agent_continuation 24 先通过。cli_e2e 52 passed、1 failed，`timeout_during_mcp_initialization_reaps_process_group` 在 `tests/cli_e2e.rs:388` 等待 `plugins/slowmcp/startup.sh.pid` 未出现。原日志 `/tmp/instagent-herdr-20260906-qupwTc/10-validate-first-failed.log` 保留未改。
- 只读定位：`interrupted_mcp_startup_reaps_process_group(None)` 使用 `run --timeout 2`，启动后先以 `wait_pid_file` 轮询 200 × 50 ms，取得 PID 后才等待 JSON `timed_out` / 124 并验证进程回收。失败在 PID 前置同步，尚未执行终态/回收断言；初始化也受 2 秒期限约束，10 秒 PID 轮询不会延长它。日志没有该 CLI 子进程 stdout/stderr 或终态，未确定根因，不直接归因环境或清理逻辑。
- 按交接隔离执行 `cargo test --test cli_e2e`，退出 0，53 passed，包含上述失败用例。未修改源码、断言或超时，未增加测试参数/环境放宽条件。
- 随后仅执行一次 `bash scripts/ci.sh` 完整复验，退出 101：fmt/clippy 通过，lib 526 passed、1 failed，新增失败 `plugin::install::tests::cancelled_clone_kills_process_group` 在 `src/plugin/install.rs:1997` 报 `no pid recorded in .../grandchild.pid`。该次脚本停在 lib，CLI 与后续 target 尚未执行，不能视为完整通过或证实 MCP 集成风险消失。
- 对新增失败只读核对并单独运行 `cargo test --lib plugin::install::tests::cancelled_clone_kills_process_group -- --exact`，退出 0，1 passed、526 filtered out。源码中取消线程只等待 `git.pid`，fake git 后续才启动 sleep 并记录 `grandchild.pid`，存在取消与写入孙进程 PID 的时序窗口；该窗口与失败现象相容，但本次日志不能证实当次时序。读取 PID 失败早于 `wait_group_gone` 的断言，不将其直接当作进程泄漏或与 MCP/proxy 同因。
- 日志与前后夹具快照保存于 `/tmp/instagent-docs-mcp-validation-20260906-qftg_phm/`：`cli-e2e-isolated.log`、`ci-recheck.log`、`cancelled-clone-isolated.log`。隔离通过只说明相应复验结果，不与历史 662 passed 累加，也不能替代该次完整 CI 失败；该次集成复验未通过，最终协调器结果见本节末尾。
- 该次只修改 `docs/release.md` 和本归档风险记录，并按协调器明确授权复用初次完整 CI 与 MSRV 通过记录作为文案 amend 门槛；未重复 MSRV 或继续压力复跑。原有两次 proxy 失败、历史线上超时和所有原始日志保留。该次 73 个源码夹具路径/类型/权限/文件 SHA-256 与复验前相同，liveplug 无 `.hook-out` 产物，未处理既有用户目录或任何备份，未运行真实模型。
- 最终协调器在 `48305f7` 上亲自执行 `bash scripts/ci.sh` 与 `cargo +1.93.1 check --locked --all-targets`，均退出 0。完整结果为 Rust 662 passed、0 failed、10 ignored，Python 11 passed，fmt/clippy/rustdoc/release/`--help` 与 MSRV 全部通过；日志分别为 `/tmp/instagent-herdr-20260906-qupwTc/48305f7-10-validate-0.log` 和 `/tmp/instagent-herdr-20260906-qupwTc/48305f7-10-validate-1.log`。截至本次验证 T1/T2 任务验收完成，历史失败和未解决的波动根因继续保留，不以通过宣称风险消除。此次仅更正两份文档的状态措辞，按授权使用这次协调器完整 CI/MSRV 作为 amend 门槛，未重复测试。
