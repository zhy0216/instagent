# repo-improvements

## 意图

无任务 prompt 的 repo 探索：对 instagent（todos 00–19 已全部完成、`cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` / `cargo test` 全绿）做系统性排查，产出完整改进清单并拆成可并行的任务队列。探索基线：2026-09-03，HEAD `24c2316`，三条校验命令 + `--features anthropic-engine` 两步全部通过，无失败无警告。

## 探索结论摘要

整体质量很高：非测试代码零 `panic!`/`todo!`/`unreachable!`，17 处 `unwrap`/`expect` 全部是构造不变量或 Mutex 中毒守卫，外部输入路径无可恐慌点；单测 273 个，插件五件套、registry、compact、message 覆盖充分。改进空间集中在：

1. **两个 provider 引擎（openai/anthropic）的流状态机骨架大段同构**（最大重复块 ~50 行×2），值得抽公共驱动层；
2. **内置工具与会话文件的健壮性**（非原子写、符号链接穿透、全量读大文件、会话损坏无恢复）；
3. **CLI 二进制层零集成测试**（plugin 子命令、`run -t` 全链路、审批输入解析）；
4. **工程配套缺口**（无 cargo-audit、CI 只跑 ubuntu、scripts/ci.sh 与 ci.yml 不对齐）；
5. 少量死代码与过期文案。

## 发现清单（完整）

优先级：P0 阻断 / P1 应修 / P2 可选。难度：easy / medium / hard。

### 正确性

| # | 位置 | 问题 | 优先级 | 难度 |
|---|---|---|---|---|
| C1 | `src/session.rs:201-232` | `rewrite` 两步 rename 之间崩溃 → 主文件消失只剩 `.bak`，resume 直接失败 | P1 | medium（→T3） |
| C2 | `src/session.rs:100-130` | JSONL 任一行损坏整个会话打不开，无 salvage 模式 | P1 | medium（→T3） |
| C3 | `src/session.rs:132` | 过期注释 `TODO(18) 接线`（实际已接线） | P2 | easy（→T3） |
| C4 | `src/session.rs:39` + `src/cli/handlers.rs:186` | `SessionHeader.name` 半死字段：永远写 None，`sessions list` 永远显示 `-` | P2 | easy（→T3） |
| C5 | `src/plugin/bundled.rs:35` | `pub fn materialize_dir()` 死函数，生产/测试均走 `materialize_at` | P2 | easy（→T1） |
| C6 | `bundled/plugin.json:5` | description 仍写 "placeholders until TODO(10)"，TODO(10) 早已完成 | P2 | easy（→T1） |
| C7 | `bundled/dev.instagent/providers/openrouter.json:9` | `http-referer` 指向占位地址 `https://github.com/instagent`，随每个请求发出 | P2 | easy（→T1） |
| C8 | `docs/goose-plugin-core-plan.md:174` | proxy 描述写 `MINI_AGENT_PORT`，代码实际 `INSTAGENT_PORT` | P2 | easy（→T1） |

### 健壮性

| # | 位置 | 问题 | 优先级 | 难度 |
|---|---|---|---|---|
| R1 | `src/tools/builtin/fs.rs:79,99` | write/edit 非原子（直接覆盖），中断留截断文件；edit 有读-改-写 TOCTOU | P1 | medium（→T2） |
| R2 | `src/tools/builtin/fs.rs` | write/edit 跟随符号链接，`link -> ~/.ssh/authorized_keys` 可写穿目标 | P1 | medium（→T2） |
| R3 | `src/tools/builtin/fs.rs:37` | `read` 先全量读入内存再截取行数，大文件内存暴涨 | P1 | medium（→T2） |
| R4 | `src/tools/builtin/fs.rs:47,55` | `line=0` 静默当第 1 行、`limit=0` 返回空窗口，语义坑 | P2 | easy（→T2） |
| R5 | `src/tools/builtin/tree.rs:154-159` | 对树内每个文件全量 `read_to_string` 只为数行数，大仓库极慢 | P1 | medium（→T2） |
| R6 | `src/tools/builtin/shell.rs:249-255` | 全量输出落共享临时目录，默认 0644 同机其他用户可读 | P2 | easy（→T2） |
| R7 | `src/session.rs:165-174` | 会话追加无锁，两进程同开会话会互相漂移（设计假设单进程，记风险） | P2 | 接受现状，写入 README 风险说明（→T1） |
| R8 | `src/session.rs:204` | rewrite 临时文件名固定，并发会互踩（依赖单进程假设） | P2 | medium（→T3，随机后缀） |
| R9 | `src/hooks.rs:474-554` ↔ `src/tools/command.rs:37,149-156,264-274` | `drain`/`read_all`/`OUTPUT_DRAIN_TIMEOUT`（同为 500ms）两套相同实现 | P1 | medium（→T4） |

### 安全

| # | 位置 | 问题 | 优先级 | 难度 |
|---|---|---|---|---|
| S1 | CI | 无依赖漏洞扫描（cargo-audit / RUSTSEC） | P1 | easy（→T8） |
| S2 | `src/tools/builtin/fs.rs` + `src/agent/approval.rs:18` | `read`/`tree` 在默认审批白名单且无路径围栏，approve 模式可无确认读任意用户可读文件。**设计决策**：本代理定位是用户环境代理（同 goose），保持行为不变，在 README 明示该语义 | P2 | easy，仅文档（→T1） |
| S3 | 全仓库 | 无硬编码密钥（provider 一律 `api_key_env` 读环境变量），无注入新增面 | — | 无需处理 |

### 性能

| # | 位置 | 问题 | 优先级 | 难度 |
|---|---|---|---|---|
| P1 | 同 R3/R5 | 全量读文件路径（`read`/`tree`）是唯一明显慢路径 | P1 | medium（→T2） |

### 测试

| # | 位置 | 问题 | 优先级 | 难度 |
|---|---|---|---|---|
| T1 | `src/cli/handlers.rs:203-329` | plugin 子命令（install/list/update/enable/disable/show ~130 行）零测试 | P1 | hard（→T9） |
| T2 | `tests/` | 无 CLI 二进制级集成测试：`run -t` 全链路、clap 参数、退出码未触达 | P1 | hard（→T9） |
| T3 | `src/cli/render.rs:122-135` | `CliConfirm` 的 y/yes/a/always 决策解析零测试（审批语义关键路径） | P1 | medium（→T7） |
| T4 | `tests/` | `--resume` 跨命令链路无集成测试 | P2 | hard（→T9，sessions 侧） |

### 工程 / DX

| # | 位置 | 问题 | 优先级 | 难度 |
|---|---|---|---|---|
| E1 | `.github/workflows/ci.yml` | 只跑 ubuntu-latest；开发机是 macOS，测试含 unix 特性，跨平台回归无保障 | P2 | easy（→T8） |
| E2 | `scripts/ci.sh` vs `ci.yml` | ci.sh 多一步 `cargo run -q -- --help` smoke，README 称其"= CI 全量"不准确 | P2 | easy（→T1 文案 + T8 对齐） |
| E3 | README:18 | `cargo install --path .` 会把 2 个测试 fixture 二进制一起装进 `~/.cargo/bin` | P1 | easy（→T1） |
| E4 | 覆盖率 | 无 coverage 工具（cargo-llvm-cov） | P2 | roadmap |

### 代码质量

| # | 位置 | 问题 | 优先级 | 难度 |
|---|---|---|---|---|
| Q1 | `openai.rs:306-369/438-466` ↔ `anthropic.rs:353-415/543-559` | `sse_to_stream_events` unfold 驱动 + `StreamState` + `finalize` 骨架 ~90% 同构（最大重复块） | P1 | hard（→T6） |
| Q2 | `openai.rs:73-152` ↔ `anthropic.rs:83-170` | 引擎构造/请求头/`stream()`/`to_provider_error`/`PendingCall` 脚手架重复；`anthropic.rs:48` 还从 openai 模块 import `sanitize_function_name`（公共层信号） | P1 | hard（→T6） |
| Q3 | 两引擎测试段 | `fast_retry`/`def`/`provider_at`/`collect`/`run_sse` 等脚手架 ~110 行×2 近乎逐行重复 | P2 | hard（→T6，testutil 共享） |
| Q4 | `src/agent/mod.rs:187-283` | `run_turn` 工具执行段 ~100 行内联（审批→PreToolUse→执行→PostToolUse→deny），嵌套 3 层 | P1 | medium（→T5） |
| Q5 | `src/agent/mod.rs` 4 处 | `HookContext::new(...).with_*(...)` 构造序列重复 | P2 | medium（→T5） |
| Q6 | `src/provider/mod.rs:142` | `ProviderDef.display_name`/`description` 已建模未接线，生产从不读取 | P2 | roadmap（涉及 CLI 展示面设计） |
| Q7 | `src/provider/proxy.rs:92`、`src/tools/mcp.rs:186,191`、两引擎 `with_retry` | 仅测试使用的公共 API | P2 | 接受现状（测试支撑 API，注明即可） |

## 目标

1. 消除两引擎流状态机重复，建立共享 SSE 驱动层（Q1–Q3）。
2. 内置工具与会话文件的原子性、符号链接、大文件、损坏恢复加固（R1–R8、C1–C4）。
3. CLI 层补上二进制级集成测试与审批解析单测（T1–T4）。
4. CI 加依赖审计与 macOS matrix，消除脚本/文档不一致（E1–E3、C5–C8）。
5. 重复的进程输出读取模板收敛到一处（R9）。

## 非目标

- 不改依赖清单、不升级依赖（`todos/01` 锁定，AGENTS.md 约定）。
- 不改 fs 工具的审批语义（保持 `read`/`tree` 默认白名单，只补文档说明）。
- 不做会话文件锁 / 多进程并发会话（维持单进程假设，文档注明）。
- 不动 `todos/done/`；不改 `src/lib.rs` 模块树声明。
- 不做 Windows 支持、不发布二进制、不上覆盖率工具（roadmap）。

## 方案

- **共享流驱动层（T6）**：在 `src/provider/` 内（`http.rs` 或新私有模块）抽通用 `unfold` 驱动器 + 共享 `StreamState`/`PendingCall`/`finalize` 钩子；引擎只提供"事件 → 状态"回调与鉴权头；`sanitize_function_name` 上提；测试脚手架收敛为共享 testutil。全程保持两引擎既有测试绿。
- **内置工具加固（T2）**：write/edit 改临时文件 + rename 原子替换，写前 `symlink_metadata` 拒绝符号链接目标；`read` 改 `BufReader` 流式读取到窗口即停并加文件大小上限；`tree` 数行改流式按字节计数；shell 输出目录 0700 / 文件 0600。
- **会话加固（T3）**：rewrite 改为"写随机后缀 tmp → `rename(tmp→主文件)` 覆盖、旧文件先复制为 bak"（或等价顺序，保证主文件任何时刻存在）；resume 增加坏行 salvage（截断到最后合法行 + 警告）；删半死字段 `SessionHeader.name`。
- **CLI e2e（T9）**：`tests/cli_e2e.rs` 经 `CARGO_BIN_EXE_instagent` 起真进程，用沙箱三变量隔离，覆盖 `run -t`、`sessions`、`plugin install/list/show/enable/disable --yes`、`--plugin PATH`。
- 其余为小修（T1 文案/死代码、T4 drain 上提、T5 run_turn 抽函数、T7 审批解析抽纯函数、T8 CI）。

## 拆解

任务队列在 `todos/`，与下表一一对应（依赖列"无"即可并行）：

| 文件 | 难度 | 依赖 |
|---|---|---|
| `01-quick-fixes.md` | easy | 无 |
| `02-builtin-tools-hardening.md` | medium | 无 |
| `03-session-robustness.md` | medium | 无 |
| `04-subprocess-io-dedup.md` | medium | 无 |
| `05-agent-run-turn-extract.md` | medium | 无 |
| `06-provider-shared-stream.md` | hard | 无 |
| `07-cli-confirm-parse.md` | medium | 无 |
| `08-ci-audit-matrix.md` | easy | 无 |
| `09-cli-e2e-tests.md` | hard | 02、03、04、05、06（在被重构后的最终形态上写集成测试，避免返工） |

01–08 文件/模块互不相交，可最多 5 路并行；09 收尾。

## 校验

仓库级校验命令（每个任务完成后全部执行，含 feature 两步）：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo clippy --all-targets --features anthropic-engine -- -D warnings
cargo test --features anthropic-engine
```

最终验收：上述五条全绿 + `bash scripts/ci.sh` 通过 + `tests/cli_e2e.rs` 全绿。

## 风险与假设

- **T6 是最大风险点**：两个 1200+ 行引擎的重构，测试必须全程保持绿；若共享抽象与某引擎语义冲突，允许保留少量引擎特有代码，不强行抽象。
- **T8 的 cargo-audit 设为不阻断**（`continue-on-error: true`）：若发现 RUSTSEC 通告，修复需要升级依赖，违反依赖锁定约定——只在 CI 输出中暴露，由维护者决策。
- **macOS matrix（T8）无法本地验证**，以推送后 CI 绿为准；测试均为 unix 语义，预期通过。
- 假设 `cargo install --path . --bin instagent` 用法成立（README 改法，T1 验证）。
- 假设 `CARGO_BIN_EXE_instagent` 对集成测试可用（`default-run` 已设，T9 验证）。
- 会话单进程假设保持不变；若未来要支持多进程，另开计划。
