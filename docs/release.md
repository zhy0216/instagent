# 发布与校验政策（toolchain / MSRV / 安全扫描 / CI 门槛）

- 政策建立：2026-09-05（`plans/repo-improvements-2026-09-04` todos/17，plan E1/E2/E9/A8）
- 契约与集成验证更新：2026-09-06（`plans/repo-improvements-2026-09-06` todo 10）
- owner：仓库维护者 @zhy0216（下列所有豁免与政策变更由其批准）

## 发布目标与包元数据

当前发布目标 = 可复现的源码构建与本地安装（`cargo build --release` /
`cargo install --path .`）；未决定发布到 crates.io。`Cargo.toml` 已按发布
规范补齐 `license` / `repository` / `homepage` / `documentation` /
`keywords` / `categories`，正式发版前只需补 LICENSE 文件与版本策略，不再
动元数据结构。

- **license = Apache-2.0**：与只读参考基线 block/goose（Apache-2.0）对齐；
  AGENTS.md 约定成段搬运在 commit 注明出处，衍生代码需保持 Apache-2.0
  兼容。正式对外发布前由 owner 复核此选择。

## Toolchain / MSRV

- **固定策略**：`rust-toolchain.toml` 钉死 `1.94.0`（含 rustfmt、clippy
  组件）；本地和 CI（rustup 读取同一文件）使用同一可复现 toolchain，
  不允许无记录的 moving stable。
- **MSRV**：`Cargo.toml` 的 `rust-version = "1.93"`。验证证据：
  2026-09-05 在本机 `cargo +1.93.1 check --locked --all-targets` 通过（全部依赖 +
  测试 target 可编译）；CI `msrv` job 固定 1.93.1 持续执行同一命令，主工具链
  仍由 `rust-toolchain.toml` 的 1.94.0 控制。该值是已验证下界，不是承诺支持更早版本。
- **升级政策**：钉版升级 = 一次独立 commit，PR 内必须记录新版本号并跑完
  下方全部门槛命令；不允许顺手 bump。降级 MSRV 需重新验证依赖树。

## CI 门槛（与 scripts/ci.sh 一一对应）

`check` job（阻断）：

1. `cargo fmt --check`
2. `cargo clippy --all-targets -- -D warnings`
3. `cargo test`
4. Python 回归：`PYTHONDONTWRITEBYTECODE=1 python3 -W error::ResourceWarning -m unittest discover -s tests -p 'test_*.py'`
   （解释器由 `actions/setup-python` 固定；`ResourceWarning` 直接失败，不全局屏蔽）
5. `cargo rustdoc --lib -- -D warnings`（intra-doc 断链即失败）
6. `cargo check --release --all-targets`（release smoke）
7. `cargo run -q -- --help`（CLI 冒烟）

`msrv` job（阻断）：`cargo +1.93.1 check --locked --all-targets`（固定最低版本；
主工具链仍由 `rust-toolchain.toml` 的 1.94.0 控制；`--locked` 在需要修改锁文件时失败，
依赖声明、feature 和模块树是否变化另用 Git diff 核对）。

`audit` job（cargo-audit / RUSTSEC 公告）：

- **PR/push 不阻断**。原因：修复公告必然要求升级依赖，与 AGENTS.md
  「不加依赖、不升级依赖」的锁定冲突；锁定解除前阻断只会强制违反仓库
  约定。owner：@zhy0216 负责评估公告并决定是否解锁升级（届时把该步改为
  阻断）。
- **scheduled scan 阻断**：每周期的定时运行（`schedule` cron）不豁免，
  公告以红灯 workflow run 通知 owner；这是"定期暴露"机制，不是合并门槛。
- 本地 `scripts/ci.sh` 中 cargo-audit 为信息性步骤（未安装则跳过提示）。
- 未引入 cargo-deny：audit-check 已覆盖 RUSTSEC 公告面，license/sources
  检查在发布到 crates.io 前再评估（YAGNI，owner 决定）。

## tokio feature 收窄评估（plan E9，本轮不改）

`tokio = { version = "1", features = ["full"] }` 超配。实际用到的能力（按
全仓 grep `tokio::`）：`macros`（`#[tokio::main]` / `join!` / `select!`）、
`rt`（current_thread + spawn_blocking）、`process`（子进程）、`signal`
（Ctrl-C）、`time`（timeout/sleep）、`io-util`（stdout/stderr piped +
读取）、`sync`（broadcast/mpsc oneshot Mutex）。候选窄化：
`["macros","rt","process","signal","time","io-util","sync"]`。
不执行原因：AGENTS.md 由 todos/01 锁定 Cargo.toml 依赖与 feature，任务
17 的约束同样禁止改依赖声明；解除锁定后按此清单一次变更并以
`cargo check --all-targets` + 测试矩阵验证。

## provider proxy readiness flake 采样记录（plan A8）

历史执行记录称并发构建下 readiness 偶发 flake。2026-09-05 采样：

- 顺序运行 5×（`cargo test --test provider_proxy`，7 测试）：**0 失败**。
- 并发运行 3 波 × 8 进程 × `--test-threads 4`（同一测试二进制并行，
  ≈168 次测试执行）：**1 失败**
  （`dropping_provider_kills_proxy_leaving_no_listener`，
  `fake-proxy-server exited (exit status: 101) before ready`）。
- 根因（可复现、确定）：`src/provider/proxy.rs` 的 `free_port()` 用
  bind :0 → 读端口 → **立即 drop listener** 的方式选端口，父进程与子
  进程 bind 之间存在 TOCTOU 竞态；并行测试进程/并发 launch 时端口可被
  抢注，fixture 在 `bind(...).expect(...)` panic（exit 101）→ 表现为
  "exited before ready"。串行几乎不触发，与历史"并发下偶发"一致。
- 本轮未修复原因：确定性修复点在 `src/provider/proxy.rs`
  （`launch`/`spawn_and_wait` 对"子进程在 ready 前退出"换端口重试 1–2
  次，或让子进程自选端口回报），超出本任务允许修改的文件面；测试侧
  缓解只会掩盖竞态，不做。修复任务落地时以本文档的并发采样命令作为
  回归验收（并发 24 run 至少 0 失败）。

此前修复后验证（`plans/repo-improvements-2026-09-05` todo 05，当时构建的 `tests/provider_proxy` 二进制
`--test-threads 4`，timeout 120s）：并发 3 波 × 8 进程 = 24 run
全部通过（0 失败，360 次测试执行）。采样只补充说明有限重试与总期限的
缓解效果，不证明 TOCTOU 已消除：选端口（bind→释放）与子进程 bind 之间
仍无原子交接，完整端口继承协议属 roadmap RM06。

2026-09-06 的会话预算任务（todo 04）首次全量回归另有两个计数断言失败：
`concurrent_transport_failures_restart_once_and_both_succeed`（2 != 1）和
`restart_is_bounded_to_once_per_call`（3 != 2）。随后隔离复跑 15/15、完整复跑
以及协调器的 04 集成态完整回归均通过。该次波动未进一步归因，不能据此认定
与上述端口竞态同因，也不能归因于文档变更或某个确定的环境问题；原始记录见
[04 归档](../plans/repo-improvements-2026-09-06/todos/done/04-session-write-budgets.md)。

## 默认离线与显式在线测试

普通 `cargo test`（含 `scripts/ci.sh`）默认只执行离线回归，继承凭据也不会触发
真实模型访问。`tests/live_e2e.rs` 包含 2 个离线夹具隔离测试及 10 个 `#[ignore]`
在线用例。CLI 模板测试与 live 测试都将 9 个版本化 liveplug 输入复制到各自的
临时目录，保留脚本权限，排除 `.hook-out` 和其他运行输出；hook 写入与清理由各
临时目录独占，源码夹具只读。

在线测试只在提前注入非空白 `TOKEN_PLAN_API_KEY` 后显式运行：

```bash
cargo test --test live_e2e -- --ignored
```

默认模型 `qwen3.6-flash`，可通过 `INSTAGENT_LIVE_MODEL` 覆盖；provider 请求
超时为 120 秒，每用例的二进制执行整体超时为 180 秒。缺凭据、空串或纯空白
立即失败，不能用门控 return 计为通过。

历史与当前口径分开保留：

- 本轮修复前基线 `8a446c4` 的原样 `cargo test` 因环境已有凭据启动了 10 个
  live 用例，全部在 180 秒整体期限超时，命令退出 101；共同诊断为
  `instagent 二进制执行超时（180s）: Elapsed(())`。远端具体原因未确定。
- 同一基线 `env -u TOKEN_PLAN_API_KEY cargo test` 退出 0，实际执行 586 个
  离线 Rust 测试；另 10 个 live 函数门控返回，被旧 harness 显示为 passed，
  没有验证在线行为。Python 11 个通过，doc-test 0。
- 09 已验证默认 live target 在无 key 和进程内假非空 key 两种环境下均为
  2 passed、10 ignored。其缺 key 负向验收
  `env -u TOKEN_PLAN_API_KEY cargo test --test live_e2e -- --ignored`
  **预期退出 101**，10 个用例立即提示缺少非空凭据；单用例空串/纯空白也预期失败。
  这是门控机制验证，不是模型验证，详见[09 归档](../plans/repo-improvements-2026-09-06/todos/done/09-live-test-isolation.md)。
- 本次文档收口不运行真实在线 `--ignored`，真实模型未重试、未验证。统计只累加
  每个顶层测试目标最终 `0 filtered out` 的 summary；lib 中 3 个一测试子 harness
  的 `filtered out > 0` 输出不重复计数，ignored 与实际 passed 分列。

本轮用户契约包括模板展开 1 MiB、文件 32 MiB 读写预算和 Unix 普通权限保留、
会话读写共享预算、组件 1 MiB 实际读取上限、安装重叠拒绝和必需凭据空白拒绝。
异常工具响应在副作用前失败，压缩拒绝截断摘要并保留未回答输入的全部内容块。
详见[使用说明](usage.md)和[架构说明](architecture.md)；这些保证不扩展为完整 ACL、
图像解码、全路径隔离或跨进程事务，会话写入拒绝也不回滚工具外部副作用。

## 2026-09-06 集成态验证

本节记录初次文档收口：验证输入为已合入 01–09 的 `dd382aa` 加文档变更，在本机
macOS 上执行。该阶段最终（第三次）`bash scripts/ci.sh` 退出 0，MSRV 单独校验
退出 0；未运行远端 CI。后续独立集成失败及最终协调器通过结果见下一节。

| 检查 | 初次收口结果 |
|---|---|
| `cargo fmt --check` | 通过（完整 CI 脚本） |
| `cargo clippy --all-targets -- -D warnings` | 通过，无 warning（完整 CI 脚本） |
| `cargo test` | 662 passed、0 failed、10 ignored；doc-test 0（完整 CI 脚本） |
| Python 回归（ResourceWarning 作为错误） | 11 passed（完整 CI 脚本） |
| `cargo rustdoc --lib -- -D warnings` | 通过（完整 CI 脚本） |
| `cargo check --release --all-targets` | 通过（完整 CI 脚本） |
| `cargo run -q -- --help` | 通过（完整 CI 脚本） |
| `cargo +1.93.1 check --locked --all-targets` | 通过，MSRV 1.93.1 |
| cargo-audit | 本机未安装，脚本明确跳过；未安装工具、升级依赖或改变扫描豁免，不能据此宣称依赖已审计 |

初次收口通过的完整运行 Rust 顶层计数如下，lib 内 3 个子 harness 的输出不重复累加，
此前隔离复跑也不加入总数：

| target | 实际 passed | ignored |
|---|---:|---:|
| lib | 527 | 0 |
| instagent bin | 11 | 0 |
| agent_continuation | 24 | 0 |
| cli_e2e | 53 | 0 |
| live_e2e（离线夹具测试） | 2 | 10 |
| mcp_e2e | 15 | 0 |
| provider_proxy | 15 | 0 |
| session_recovery | 11 | 0 |
| tool_inventory | 4 | 0 |
| **合计** | **662** | **10** |

过程中的失败仍保留：首次 `bash scripts/ci.sh` 退出 101，
`tests/provider_proxy.rs:559` 的 `cancelled_restart_candidate_is_reaped` 在初始启动
计数断言处失败（2 != 1），该 target 为 14 passed、1 failed，脚本随即停止。
随后 `cargo test --test provider_proxy` 隔离复跑 15/15 通过，但第二次完整脚本仍在
同一断言失败（同为 2 != 1、退出 101）。第三次原样完整运行通过，本次未修改 proxy
源码或测试，未确定这两次计数波动的原因；不能与此前的两个计数失败或端口竞态直接
等同，也不因最终通过而宣称该风险已消除。中间单独补跑的 Python、rustdoc、release、
`--help`、session_recovery、tool_inventory 和 doc-test 也通过，未另做压力采样。

相对计划提交 `892dcfd`，`Cargo.toml`、`Cargo.lock`、`rust-toolchain.toml` 和
`src/lib.rs` 无差异，未引入依赖/feature/模块树变更。校验前后的源码夹具共 73 个
路径/类型/权限条目及文件 SHA-256 完全一致，liveplug 没有新增 `.hook-out`；
07 的 CLI 模板路径与 09 的 live 路径隔离组合已在完整回归中核实。
未处理本轮开始前的用户目录内容或备份；真实模型仍未重试、未验证。

## 2026-09-06 后续集成复验与新增风险

协调器复核 `b01e30e` 并独立运行 `bash scripts/ci.sh` 时退出 101。fmt、clippy、
lib 527、bin 11、agent_continuation 24 均先通过，随后 cli_e2e 为 52 passed、
1 failed：`timeout_during_mcp_initialization_reaps_process_group` 在
`tests/cli_e2e.rs:388` 等待 `plugins/slowmcp/startup.sh.pid` 失败。
原始日志保留在 `/tmp/instagent-herdr-20260906-qupwTc/10-validate-first-failed.log`。

只读定位确认，这次断言停在取得启动 PID 的前置步骤，尚未执行 JSON 终态、退出码
及进程组回收断言。用例的 `run --timeout` 为 2 秒，覆盖初始化；PID 等待另以
200 次 × 50 ms 轮询，不能延长 run 的期限。原日志未记录该子进程的 stdout/stderr
或终态，因此不能确定 PID 未出现的原因，也不能直接判定进程组泄漏或环境故障。

本任务随后的复验只使用现有测试与默认参数，没有修改源码、断言或放宽超时：

| 命令 | 实际结果 |
|---|---|
| `cargo test --test cli_e2e` | 退出 0，53 passed；上述 MCP 初始化超时用例通过 |
| `bash scripts/ci.sh` | 退出 101；fmt/clippy 通过，lib 526 passed、1 failed，尚未运行到 CLI 或后续 target |
| `cargo test --lib plugin::install::tests::cancelled_clone_kills_process_group -- --exact` | 退出 0，1 passed、526 filtered out；仅验证新增失败用例，不计入完整运行总数 |

本次完整复验的新增失败为 `plugin::install::tests::cancelled_clone_kills_process_group`：
`src/plugin/install.rs:1997` 报 `no pid recorded in .../grandchild.pid`。源码可见一个
时序窗口：取消线程只等待 `git.pid` 出现，而 fake git 脚本随后才启动 sleep 并写
`grandchild.pid`；读取两个 PID 完成后才进入进程组消失断言。这与缺失孙进程 PID
的现象相容，但日志不足以证实当次时序，不将其当作已确认的根因或回收失败。

新增复验日志在 `/tmp/instagent-docs-mcp-validation-20260906-qftg_phm/`：
`cli-e2e-isolated.log`、`ci-recheck.log`、`cancelled-clone-isolated.log`。
CLI target 与新增 lib 用例隔离通过，不能替代该次完整 CI 失败；该次集成复验未通过，
最终协调器结果见本节末尾。这些失败不与此前两次 proxy 计数失败合并归因。
该次只追加文档证据，按协调器授权复用初次已通过的完整 CI/MSRV 作为 amend 门槛，
没有再次运行 MSRV 或反复压力复跑。源码夹具 73 个路径/权限/内容条目仍与复验前一致，
未产生 liveplug `.hook-out`，未处理既有用户目录或备份，未运行真实模型。

最终，协调器在 `48305f7` 上亲自执行 `bash scripts/ci.sh` 和
`cargo +1.93.1 check --locked --all-targets`，均退出 0：Rust 662 passed、0 failed、
10 ignored，Python 11 passed，fmt/clippy/rustdoc/release/`--help` 与 MSRV 全部通过。
日志分别保留在 `/tmp/instagent-herdr-20260906-qupwTc/48305f7-10-validate-0.log` 和
`/tmp/instagent-herdr-20260906-qupwTc/48305f7-10-validate-1.log`。
截至本次验证，todo 10 任务验收完成；所有历史失败仍保留，已有波动根因尚未解决，
完整通过不代表风险已消除。真实模型仍未验证，cargo-audit 仍因未安装而跳过。

## 全部验证命令

```bash
bash scripts/ci.sh                                  # 1–7 全量 + audit 提示
cargo +1.93.1 check --locked --all-targets          # MSRV 下界复验（需装有 1.93.1）
```
