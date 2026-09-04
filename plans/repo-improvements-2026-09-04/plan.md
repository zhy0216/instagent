# instagent 当前仓库审计与改进方案（2026-09-04）

## 意图

这是一份针对当前工作区的仓库级审计方案：记录现在已经具备的能力、可复现的校验基线，以及仍值得投入的正确性、安全性、资源控制、协议一致性、测试和工程化改进点。方案只描述工作，不在本轮修改业务代码、不拆分 todo、不执行实现。

## 当前状态

### 基线

- 分支：`main`，工作区干净，`HEAD` 为 `6962ca2`，与 `origin/main` 对齐。
- 项目：Rust 2021 的插件核心 agent，约 1.8 万行生产 Rust；核心模块包括 agent loop、message、session、settings、plugin、provider、hooks、subprocess 和 tools。
- Provider：当前以 `openai` 与 `proxy` 为原生 engine；Anthropic 原生 engine 已由 ADR 0001 移除。
- Built-in tools：当前为 6 个：`shell`、`read`、`write`、`edit`、`tree`、`read_image`。
- 任务状态：原始 `todos/00`–`todos/19` 已完成；历史 `plans/repo-improvements/` 的 9 项也已完成；图片支持已合入。`plans/` 中仍有部分历史方案需要与当前 ADR 对齐。

### 校验结果

已执行并通过：

```text
bash scripts/ci.sh
  cargo fmt --check                         PASS
  cargo clippy --all-targets -- -D warnings PASS
  cargo test                                PASS
  cargo run -- --help                       PASS

cargo check --release --all-targets         PASS
```

当前测试合计 282 个通过（lib 244、main 6、CLI E2E 5、live E2E 10、MCP E2E 10、provider proxy 7）。这说明主流程已经有较好的回归基线，但不代表恶意路径、资源上限和取消语义已被覆盖。

额外审计结果：

- `cargo doc --no-deps --document-private-items` 能生成文档，但有 4 个 intra-doc link 警告。
- `cargo rustdoc --lib -- -D warnings` 当前失败，失败点是 3 个公开文档指向私有项目的链接。
- 本机没有 `cargo-audit` / `cargo-deny`；CI 的 `rustsec/audit-check@v2.0.0` 目前是 `continue-on-error`，因此依赖公告不会阻断合并。
- `git diff --check` 通过；生产代码没有待处理的 `TODO`/`FIXME` 功能标记。

### 已有优势（不应被回归）

- 插件、provider、工具来源和内核边界已经分层，且有较完整的单测和端到端测试。
- 文件工具已有原子写入、最终目标 symlink 检查、读取行数/大小控制等基础防护；shell 输出临时文件已有私有权限；子进程 drain 逻辑已有统一抽取。
- ADR 0002 已明确工具自动执行，安全边界交给外部 sandbox；后续方案不重新引入 approval/trust/mode UI。
- 会话已经有临时文件重写、备份和损坏尾部 salvage 的基础实现；下面只补足其持久化、权限和边界缺口。

## 目标

1. 防止 session、配置、插件和子进程边界导致的越权读写、密钥泄露或数据损坏。
2. 为每个外部输入和长任务建立可测量的大小、时间、并发和取消上限。
3. 让 config、message、provider、MCP 和 CLI 的行为契约显式、可诊断、可回归。
4. 保持插件优先和 sandbox 假设，不通过大规模重构或无授权升级依赖来解决问题。
5. 使文档、计划、CI 与当前实现一致，降低下一轮开发误读风险。

## 非目标与约束

- 不恢复 Anthropic 原生 provider，不恢复被 ADR 0002 删除的审批 UI。
- 不把历史 `docs/goose-*.md` 当作当前实现说明；历史设计文档只增加映射/历史标记，不整体改写。
- 不在未获授权时新增或升级 Cargo 依赖；如果结构化图片解码、secure openat 等方案确实需要依赖，先单独做 ADR 和成本评估。
- 不把“外部 sandbox 已负责隔离”误解成进程环境、路径和资源限制可以完全不做；这里关注的是应用层的纵深防御。
- 本轮不创建 `todos/`，以下只是可执行的分组和顺序。

## 总体方案

按下列层次推进：

1. **先定策略**：明确 API key 优先级、插件环境变量 allowlist、CLI stdout/stderr 契约、hook fail-open 是否保留、`enabledPlugins: []` 的语义。
2. **加固持久化边界**：集中校验 session id，修复 salvage 持久化、权限、原子替换和项目本地设置忽略规则。
3. **建立资源/取消底座**：统一 bounded output、超时、进程组、`spawn_blocking`/异步文件访问和总量预算。
4. **收紧数据契约**：补齐 config/message/provider/skill/manifest 校验，改善 MCP inventory 的错误和缓存语义。
5. **调整 agent/CLI 行为**：在安全条件下并行工具调用，处理事件背压、残缺 stream、hook 错误和交互式退出。
6. **用回归测试和文档收口**：安全边界测试优先，随后是资源/协议/CLI 测试，再清理文档、CI 和发布元数据。

## 完整发现清单与拆解

优先级含义：P1 表示应在下一轮迭代处理，P2 表示应排入近期维护或明确记录。没有发现必须立即停发的 P0。难度是相对估计：easy（局部改动）、medium（跨模块/需要策略）、hard（架构或依赖决策）。

### 一、持久化、输入边界与安全

| ID | 证据位置与现状 | 改进建议 | 优先级/难度 | 依赖 |
| --- | --- | --- | --- | --- |
| S1 | `src/session.rs:96-98,194-198` 直接用 `sessions_dir().join(format!("{id}.jsonl"))`；CLI 提供的 id 可包含 `..` 或绝对路径，resume/remove 可能访问或删除 sessions 目录外的 `.jsonl`。 | 集中实现 `validate_session_id`：只允许 UUID/安全文件名，拒绝绝对路径、parent component 和分隔符；所有 create/resume/remove/list 入口复用；加入 traversal、绝对路径和删除保护测试。 | P1 / medium | 无 |
| S2 | `src/session.rs:112-125,319-341` salvage 只在内存中截断到有效前缀；恢复后 append 仍写在坏行之后，下次 resume 会再次丢弃新消息，形成永久数据丢失。 | 恢复时把有效前缀原子重写回原文件，保留带时间戳的备份并发出明确 warning；设计幂等流程，测试 resume→append→resume。 | P1 / medium | S1、S4 |
| S3 | `Session::list` (`src/session.rs:177-191`) 遇到一个坏 header 就整体失败；`open_or_resume("last")` 按 mtime 选最新文件但不先验证，坏文件可遮蔽有效会话。 | list 对单个坏项跳过并汇总诊断；`last` 按“最新且可验证”选择，空结果时返回原因；保持脚本可解析的稳定输出。 | P1 / medium | S1、S2 |
| S4 | `src/session.rs:72-85,167-174,216-226` 创建目录/文件和重写临时文件时未显式设置 0700/0600；append 只有 flush，没有 `sync_all` 策略。会话可能包含 prompt、tool output、图片和 secrets。 | Unix 下创建私有目录/文件并保留合适 mode；定义 append/rewrite 的 durability 等级（至少提供可选 sync）；跨平台行为写入文档并测试权限。 | P1 / medium | 策略决策 |
| S5 | `src/settings.rs:60-66,77-87`、`src/plugin/install.rs:299-306,378-381` 使用直接 `fs::write`，崩溃可能留下半份配置/安装信息，且没有私有 mode。 | 复用统一的原子私有写入 helper（临时文件、fsync、rename、必要的目录 fsync），对 settings、user plugin settings、install info 全部采用；保留备份/恢复策略。 | P1 / medium | S4 |
| S6 | `src/config.rs:20-22` 暴露 `api_key_env` 与 `api_key`，但 registry/shared provider (`src/provider/registry.rs:107-117`, `src/provider/shared.rs:110-133`) 实际只读 provider manifest 的 `api_key_env`；配置字段会被静默忽略。 | 二选一并写 ADR：删除死字段和误导文档，或定义安全的优先级（环境变量/manifest/config）并确保原始 key 不被持久化、日志不泄露；为每种来源加测试。 | P1 / medium | 策略决策 |
| S7 | `Config::load` 主要做反序列化和 env override；`Agent::assemble` 仅检查 model 非空。`max_tokens/max_turns/context_limit/timeout_seconds` 可为 0，compaction threshold 可越界或为 NaN，空 model/headers 形状也可能延迟到运行时才失败。 | 在 config/provider 装载边界做字段级校验，错误中带来源文件、字段和建议值；明确 0 是否代表 disabled；覆盖 JSON、环境变量和默认值组合。 | P1 / medium | S6（字段语义先定） |
| S8 | `src/message.rs:150-198` 检查角色、非空文本及部分 assistant ToolUse→下一条 ToolResult 关系，但不拒绝 orphan/extra/duplicate ToolResult、错误角色中的 ToolUse/ToolResult、重复 tool id、空 id/name，以及坏 image metadata/base64。 | 实现 role/content 矩阵和“下一条结果 id 集合精确相等”校验；要求 id/name 非空且唯一；校验图片 MIME、编码和大小；provider wire/session 入口统一调用，补 malformed-message 测试。 | P1 / medium | S7（共享错误模型） |
| S9 | bundled JSON 与 `scripts/convert_providers.py` 保留 `display_name`、`description`、每 model `max_tokens`，但 `ProviderDef/ModelDef` (`src/provider/mod.rs:134-169`) 不建模也不使用，生成数据与运行时契约漂移。 | 明确字段是展示/限制的一部分并实现使用，或从生成产物剔除；为 converter 加 schema fixture/round-trip 测试，避免“看起来支持但实际忽略”。 | P2 / medium | S7 |
| S10 | `src/tools/skills.rs:139-157` 对 description 使用默认空串，未落实 spec 的 required/non-empty；`load_skill_body`/`load_supporting_file` (`239-261`) 无大小上限，supporting file 的 symlink 可逃出 skill root。 | 要求非空 description；设置单文件/总量上限；采用 no-follow/realpath containment 策略或明确仅信任安装目录；超限错误应可诊断，补 symlink/大文件测试。 | P1 / medium | S1、资源预算策略 |
| S11 | `src/hooks.rs:272-274,376-378`、`src/tools/command.rs:142-174` 把 `${PLUGIN_ROOT}` 直接替换进 `sh -c` 字符串；带空格、引号或 shell metacharacter 的路径会破坏解析，甚至改变命令含义。 | 不把路径拼进 shell 代码：优先通过环境变量和 argv 传递，必要时使用可靠 quoting；command tool 也设置明确 env；加入含空格/引号/换行路径测试。 | P1 / medium | S12 |
| S12 | hook 使用 `env_clear`/allowlist (`src/hooks.rs:446-471`)，但 command tool、builtin shell (`src/tools/command.rs:171-176`, `src/tools/builtin/shell.rs:69-74`) 和 MCP (`src/tools/mcp.rs:230-234`) 继承父环境，可能暴露 `OPENAI_API_KEY` 等 secrets 给不可信插件。 | 定义插件子进程的 baseline + 声明式 allowlist；至少清除 provider credentials、session secrets 和无关系统变量；明确 builtin shell 是否保留用户环境；以 env-isolation test 固化。 | P1 / medium | 策略决策、S11 |
| S13 | `src/tools/builtin/fs.rs:106-114` 只拒绝最终目标 symlink；`resolve_path` 允许相对 parent symlink/绝对路径，`write linkdir/file` 可跟随 cwd 内的链接写到外部。 | 若安全边界要求应用层 containment，逐级检查 parent 或采用 secure openat/no-follow；若 sandbox 明确负责，至少在工具文档中说明并测试当前承诺，避免声称“整个路径安全”。 | P2 / hard | sandbox/依赖 ADR |
| S14 | `src/tools/builtin/image.rs:46-59` 只检查少量 magic prefix，截断或随机 payload 仍可能被当成图片。 | 评估结构化 decoder 的依赖和 CPU/尺寸限制；短期加强长度、MIME、像素预算检查并在失败时拒绝；将 decoder 方案放入后续 ADR。 | P2 / medium | RM2、依赖策略 |
| S15 | `read_image` 最多读 20MB 后转 base64；`Agent::execute_calls` 保存 `Content::Image`，`src/provider/openai.rs:180-220` 每次请求都嵌入 data URL。重复图片会放大 JSONL、内存和 HTTP body，usage token 也未计入图片成本。 | 设单请求/单会话总字节预算，图片去重并采用 blob/reference 或生命周期淘汰；在 provider 层拒绝超限并给出可操作提示。完整 blob 方案属于 roadmap。 | P1 / hard | 资源预算、RM2 |
| S16 | settings merge (`src/settings.rs:36-43,110-131`) 丢失 `Vec` 字段“未设置”和“显式空数组”的区别；文档 `docs/usage.md:207-211` 却声称二者是 whitelist/blacklist 的重要区别。 | 使用 tri-state（缺失/空/非空）表示，或调整文档与实际语义；增加全局、项目、local 三层合并测试。 | P2 / medium | 策略决策 |
| S17 | `.gitignore` 仅忽略 `/target`、备份 jsonl 和 `.DS_Store`；`docs/usage.md:198` 建议忽略 `settings.local.json`，当前容易把本地 secrets 提交。 | 增加精确的 project-local settings pattern，并在文档说明如何显式 force-add 示例模板而不提交 secret。 | P2 / easy | S5 |
| S18 | `session::parse_line` (`src/session.rs:311-317`) 把整行 raw JSON 放入错误；provider/manifest/MCP/command 的若干 JSON/text 读取也无统一上限。坏输入可能制造巨大日志或把 secret 回显到终端。 | 统一 bounded read/parse helper；错误只展示截断后的摘要、来源和行号，敏感字段做 redact；为超长和含 secret 的坏输入加测试。 | P2 / medium | R3、R9 |

### 二、资源、取消与进程健壮性

| ID | 证据位置与现状 | 改进建议 | 优先级/难度 | 依赖 |
| --- | --- | --- | --- | --- |
| R1 | `src/main.rs:6` 使用 `tokio::main(flavor = "current_thread")`；fs/tree/image/skills 等 async tool 内部调用同步文件系统或 WalkBuilder。同步段执行时，`tokio::select!` 无法及时响应 Ctrl-C、provider 事件或取消。 | 对可阻塞工作使用 `spawn_blocking` 并传递预算/取消检查，或改用异步 fs；测量大型目录/文件下的取消延迟，避免在 blocking closure 中无限运行。 | P1 / medium | R2、R9 |
| R2 | `src/tools/builtin/tree.rs:30-47,111-146,165-198` 仅限制单文件约 10MB；文件数、总字节、输出、深度（0 表示无限）和遍历时间没有总上限，聚合 BTreeMap 可无限增长。 | 增加 max entries/bytes/output/depth/time，达到上限时返回结构化 truncation note；让遍历检查 cancellation token。 | P1 / medium | R1 |
| R3 | `src/subprocess.rs:237-255` 把 stdout/stderr 全部 append 到 String；shell 只在进程退出后截断 (`src/tools/builtin/shell.rs:120-155`)，command/hook 仍可返回完整输出，存在 OOM/日志 DoS。 | 设计 bounded collector（硬上限、ring 或受控 spill），超限时终止进程组并保留摘要；把上限传到 shell/command/hooks，明确 stdout/stderr 合并规则。 | P1 / medium | R1、R6 |
| R4 | `subprocess.rs:247-251` 和 `provider/http.rs:222-228` 对每个 chunk 独立 `from_utf8_lossy`；多字节字符跨 chunk 时会产生 replacement character。 | 使用增量 UTF-8 decoder 或保留尾部字节再解码；加入人为切断 UTF-8 边界的单测。 | P2 / easy | 无 |
| R5 | `wait_and_drain` (`src/subprocess.rs:281-287`) 对 stdout/stderr `.take().expect("... piped")`；当前调用者满足 invariant，但 helper 配置错误会 panic。 | 让 helper 返回带上下文的错误，或把“必须 piped”编码到构造器；增加未配置 pipe 的回归测试。 | P2 / easy | 无 |
| R6 | `src/plugin/install.rs:455-491` 使用 std `git_command().output()` (`src/subprocess.rs:185-195`)，没有进程组、`kill_on_drop`、超时或取消；在 current-thread runtime 中还会阻塞。 | 统一使用 Tokio subprocess wrapper，设置 process group/`kill_on_drop(true)`、超时和 bounded output；安装/更新命令接入 session cancellation。 | P1 / medium | R1、R3 |
| R7 | MCP inventory (`src/tools/mcp.rs:304-331`) 调用 `list_all_tools().await` 无 timeout/cancel，失败被 log 后转为空工具列表；`Registry::list` (`src/tools/mod.rs:140-180`) 每轮重复调用所有 source。 | per-source timeout + cancellation；失败返回结构化 note/可见诊断而非静默空列表；缓存工具清单并按连接/配置变化失效，避免每轮网络枚举。 | P1 / medium | R1、A2 |
| R8 | `connect_plugin` (`src/tools/mcp.rs:280-295`) 第一个 server 连接失败即 `?`，同插件其它健康 server 也被丢弃。 | 每个 server 独立建立连接，收集成功 source 与失败 note；仅在没有任何可用 source 时决定是否让 assembly 失败。 | P2 / medium | R7 |
| R9 | `fs.rs:163-188` edit 全量读文件且无 `MAX_READ_BYTES`，metadata 与 read 之间还有增长竞态；write 接受无界 model content；provider `http.rs:205-207` 对错误响应 `resp.text()` 无界。 | 统一输入/输出预算：read/edit/write、skill、manifest、provider error body 各有硬上限；超限前取消或截断并保留状态；测试增长竞态和错误响应。 | P1 / medium | R1、S18 |
| R10 | session rewrite (`src/session.rs:212-239`) 临时文件命名安全，但 rename 失败会遗留 tmp；没有 parent-dir fsync，备份会无限增长且 mode 未继承。 | 失败路径清理 tmp；必要时 fsync parent；限制/轮换备份并继承私有权限；加入 crash/rename-failure 模拟测试。 | P2 / medium | S2、S4 |
| R11 | shell 超大输出写到 `temp_dir/instagent-shell-output` (`src/tools/builtin/shell.rs:172-186`)，当前有安全 mode 但没有 TTL/清理，敏感结果会长期留在磁盘。 | 文件名加入 session/随机后缀，记录创建时间并在成功消费、session 结束或 TTL 到期清理；清理失败要有可见诊断。 | P2 / easy | R3、S4 |
| R12 | plugin/provider/skills/commands/install 多处 `read_dir` 使用 `entries.flatten()` 或等价静默丢弃 (`src/plugin/discovery.rs:174-181` 等)，权限/IO 错误难以诊断。 | 统一枚举 helper：区分“不匹配”和“读取失败”，汇总跳过项并保留 source/path；测试权限错误和坏 symlink。 | P2 / easy | S18 |
| R13 | `ProviderRegistry::context_limit` (`src/provider/registry.rs:142-157`) 对未知/歧义 provider 静默 fallback，之后 assembly 可能报不同的 provider 错误，诊断不准确。 | 返回 `Result` 或明确 warning，携带 provider/model/source；不要把未知 provider 当成默认上下文限制。 | P2 / easy | S7 |

### 三、Agent、协议和 CLI 行为

| ID | 证据位置与现状 | 改进建议 | 优先级/难度 | 依赖 |
| --- | --- | --- | --- | --- |
| A1 | `src/agent/mod.rs:206-295` 逐个 serial 执行同一 assistant turn 的 tool calls；独立只读调用不能利用并发，慢工具会拖延整轮。 | 以工具 capability/显式 metadata 区分可并行的只读调用；保留依赖调用顺序，设置 bounded concurrency、总预算和 cancellation；更新历史 parallel-tool plan 以删除过时 approval 语义。 | P1 / hard | R1、R3、R7、ADR2 |
| A2 | `Agent::emit` (`src/agent/mod.rs:440-443`) await 有界 sender；库调用者若不消费 event channel，整个 turn 会被背压卡住。 | 提供明确的消费契约和非阻塞/drop 策略，或允许调用者选择 unbounded/回调 sink；记录丢弃计数，测试不消费 channel 的行为。 | P2 / medium | 观测策略 |
| A3 | `stream_assistant` (`src/agent/mod.rs:430-437`) 在没有 ToolUseEnd 时直接 drop pending tool block，没有向模型/调用者报告残缺协议。 | 将残缺 stream 转成结构化错误/assistant note，保留已收集片段但禁止静默丢失；覆盖 provider disconnect、EOF 和 malformed delta。 | P2 / medium | S8 |
| A4 | CLI handlers (`src/cli/handlers.rs:42-50,75-83`) 对 session start/end hook 使用 `let _ = run_session_event(...)`，hook 失败完全不可见。 | 依据策略把错误变成 warning、event note 或非零退出；至少输出 source/hook/阶段，避免破坏正常 session 的默认兼容性。 | P2 / easy | A5 |
| A5 | `hooks.rs:299-305` 序列化失败和默认 `on_failure` 为 Allow，属于 fail-open；在安全敏感 hook 中可能绕过预期策略。 | 将 fail-open/fail-closed 变成显式配置和文档化策略，按 hook 类型给安全默认值；保留 ADR2 的自动执行模型，不引入交互审批。 | P2 / medium | 策略决策 |
| A6 | `docs/usage` 描述工具/运行输出走 stderr，但 `src/cli/render.rs:20-59` 对 text/tool/error/usage 多用 stdout；当前测试也接受这种混合行为。 | 定义稳定的机器接口：stdout 只放最终答案或 JSON，诊断/工具事件统一 stderr，或反向修正文档；为 pipe/redirect/JSON consumer 加契约测试。 | P2 / medium | 策略决策 |
| A7 | `tests/cli_e2e.rs` 仅覆盖 help/run/sessions/plugin/`--plugin`，没有 PTY 覆盖 Ctrl-C、`/clear`、`/compact`、resume 跨命令、坏参数退出码、provider/MCP 失败。 | 增加纯逻辑测试和少量 PTY smoke；重点验证取消后进程组回收、stderr/stdout 契约和 session 一致性，避免把 live API 作为必需条件。 | P1 / hard | A6、R6 |
| A8 | 历史执行记录报告 provider proxy 在并发构建下 readiness 偶发 flake；当前单次测试 7/7 通过，源代码大多已有 15s timeout，仅有 timeout 专测为 1s。 | 在 CI 并发/重复运行下做稳定性采样，再决定是否调整 readiness handshake、重试或固定端口；不要把历史记录直接当成当前确定性 bug。 | P2 / medium | 测试基础设施 |

### 四、测试与可观测性缺口

这些不是“功能代码坏了”，而是当前绿灯尚未覆盖的风险面，应该与对应修复一起落地。

| ID | 缺口 | 验收方向 | 优先级/难度 | 依赖 |
| --- | --- | --- | --- | --- |
| T1 | 没有 session traversal、symlink、坏 header、salvage 后 append、权限和原子替换失败测试。 | 临时目录中验证不会越界读/删；坏尾部被物理修复；并发/崩溃模拟后只出现完整 JSONL 和可识别备份。 | P1 / medium | S1-S5、R10 |
| T2 | 没有 output cap、UTF-8 chunk、blocking fs/tree cancellation、git timeout/process-group、MCP inventory timeout 测试。 | 用可控 fake child/server 证明超限会终止、取消有上界、不会 OOM，且错误不会静默变成成功/空列表。 | P1 / medium | R1-R9 |
| T3 | 没有 config numeric/key precedence、message role/tool-id 集合、provider schema drift、skill required fields/size/symlink 的系统测试。 | 每个错误都指出 source/field/约束；旧 manifest 和新 manifest 的兼容行为有 fixture。 | P1 / medium | S6-S10 |
| T4 | MCP 测试覆盖连接/调用，但不覆盖多 server 部分失败、header 传递、非文本结果和缓存失效。 | 成功 source 可用、失败 source 有 note；timeout/重连/配置变更不会使用过期工具清单。 | P2 / medium | R7-R8、RM1 |
| T5 | CLI 没有 PTY/pipe 双模式下的输出和取消回归。 | 固定 stdout/stderr、退出码、Ctrl-C 后 child/session 状态；live key tests 继续保持可选而非 CI 硬依赖。 | P1 / hard | A6-A7、R6 |
| T6 | 当前没有明确的覆盖率、property/fuzz 或持续资源回归门槛。 | 先给 session/message/parser/subprocess collector 建立 property/fuzz target 和最小覆盖率趋势；作为 roadmap，不阻塞本轮核心修复。 | P2 / hard | RM3 |

### 五、工程化、文档与开发体验

| ID | 证据位置与现状 | 改进建议 | 优先级/难度 | 依赖 |
| --- | --- | --- | --- | --- |
| E1 | `Cargo.toml` 缺少 `license`、`repository`、`homepage`、`documentation`、`rust-version`、keywords/categories；`rust-toolchain.toml` 和 CI 使用 moving `stable`。 | 若目标包含发布/复现，补齐包元数据并锁定经过验证的 toolchain/MSRV；若暂不发布，至少记录支持范围和升级策略。 | P2 / easy-medium | 发布目标 |
| E2 | CI (`.github/workflows/ci.yml`) 运行 fmt/clippy/test/help，但 advisory audit 是 non-blocking，也没有 scheduled dependency scan 或 doc-as-error。 | 先明确安全政策；然后安排定期 audit/deny（或等价服务），把 rustdoc link 检查和 release smoke 加入 CI；对暂时不能阻断的项显式记录原因。 | P2（安全政策确认后可升 P1）/ medium | E1 |
| E3 | 当前文档仍多处写“5 个 built-in tools”：`README.md:3`、`docs/usage.md:3-6,500`、`docs/architecture.md:3,12,23`、`src/tools/mod.rs:3`；设计文档保留历史数字。`plans/thinking-blocks/plan.md` 已被 ADR1 淘汰，`plans/parallel-tool-execution/plan.md` 仍含 ADR2 前的 approval 叙述。 | 同步当前 README/usage/architecture/module docs 为 6；给历史 goose 文档加“历史/当前映射”标记；关闭或改写 obsolete plan，执行 parallel plan 前先删掉 approval 假设。 | P2 / easy | ADR1、ADR2 |
| E4 | `cargo doc` 的 4 个警告：`src/plugin/bundled.rs:84`、`src/provider/openai.rs:14,70` 指向私有项，`tests/fixtures/fake_proxy_server.rs:5` 的 `bin` intra-doc link 无法解析；`rustdoc -D warnings` 因前三项失败。 | 改成公开 API/路径的正确链接或代码格式；修复 fixture 注释；把 `cargo rustdoc --lib -- -D warnings` 纳入 CI。 | P2 / easy | 无 |
| E5 | `scripts/convert_providers.py` 硬编码 `~/yyds/goose`，没有可注入 source/CI fixture；同时会携带运行时不使用的字段。 | 支持显式 source 参数和 fixture mode；生成后做 schema/round-trip 校验；在 CI 用小型 fixture 防止依赖本机目录。 | P2 / medium | S9 |
| E6 | `PluginManifest` 主要校验 schema/name，未系统校验 version/semver、扩展字段形状或跨字段约束；坏 manifest 可能在安装后才失败。 | 补充 manifest schema/version/路径字段校验，错误带插件名和文件；对未知字段采取明确的兼容策略。 | P2 / medium | S7、S18 |
| E7 | `src/plugin/install.rs:424-448` 替换失败/崩溃可能留下 `.replaced-*` 备份；`copy_tree` 的 symlink 行为未形成明确契约。 | 为替换状态建立可恢复清理流程，限制孤儿备份；明确 symlink 是拒绝、复制链接还是跟随，并加入 interrupted-install 测试。 | P2 / medium | S4、E6 |
| E8 | `PluginSource` CLI path 使用 `Extra` tag，诊断上不能区分“用户显式路径”和其它来源；不是安全 bug，但会降低错误可读性。 | 增加专用 source kind/显示名，错误中同时给出来源和解析后的绝对路径。 | P2 / easy | 无 |
| E9 | `tokio = { features = ["full"] }` 比代码实际需要的 runtime/process/signal/time/io/sync/macros 更宽；依赖锁定约束下暂不应直接改。 | 在依赖政策允许时收窄 feature，并以 `cargo tree`/编译矩阵验证；若不能动依赖，记录为维护债务而非临时改 Cargo.lock。 | P2 / medium | 依赖策略 |
| E10 | `src/agent/mod.rs`、`hooks.rs`、`provider/openai.rs`、`plugin/install.rs` 等文件已达约 900–1300 行，修改时容易跨越生命周期、IO 和协议边界。 | 在上述安全/资源改动稳定后，按职责拆分 parser/policy/transport/renderer；先用公共行为测试锁定，不做无收益的大重构。 | P2 / hard | T1-T5 |

## 路线图（明确不属于本轮立即拆解）

以下能力有价值，但需要单独的 ADR、依赖或产品决策；不要把它们混入基础安全修复的验收条件。

| ID | 路线图项 | 触发条件/注意事项 |
| --- | --- | --- |
| RM1 | MCP streamable HTTP 的配置 headers、SSE transport、图片/resource 等非文本 `ToolOutput` 的一等支持。当前 `src/tools/mcp.rs:147-158,381-405` 忽略 headers/非文本内容，SSE 被显式跳过；这是已知 v1 限制，应拒绝得更响亮或实现完整契约。 | 先确定凭据注入和 `ToolOutput` 内容模型，再做 transport/compatibility ADR。 |
| RM2 | 图片结构化解码、像素/压缩炸弹防护、blob/reference 存储、跨轮次去重与淘汰。 | 先完成总请求字节预算；评估新增 decoder 依赖、隐私和 session 格式迁移。 |
| RM3 | parser/subprocess/session/message 的 property/fuzz、覆盖率趋势、长时间资源回归。 | 不以单次覆盖率数字替代边界测试；先选稳定的 corpus 和 CI 时间预算。 |
| RM4 | 可发布包的签名/校验、固定 toolchain 的多平台构建、Windows 权限/进程组实现。 | 明确发布渠道、MSRV、目标平台和密钥管理；不要在核心修复中隐式扩大平台范围。 |
| RM5 | 全路径 secure openat/no-follow containment 和可选外部 sandbox adapter。 | 先与现有 sandbox 责任边界做 ADR，避免重复或冲突的安全模型。 |
| RM6 | 大模块的长期架构重组、统一 telemetry/metrics、MCP inventory 的持久化索引。 | 以 T1-T5 的行为测试为前置，避免在未锁定契约前重写。 |

## 执行顺序与依赖

这只是实现顺序，不创建 todo 文件：

1. **策略决策（短、阻塞性）**：确定 S6 key precedence、S12 env policy、A5 hook failure、A6 CLI stream、S16 settings tri-state，以及外部 sandbox 对 S13 的责任边界。
2. **存储安全层**：S1 → S2 → S3；并行处理 S4/S5/S17，随后 R10。完成后先跑 T1。
3. **资源与进程底座**：R3/R5/R6/R9，配合 R1；再补 R2、R11、R12、R13。完成后跑 T2。
4. **输入和协议契约**：S7 → S8 → S9/S10/S18；并行收紧 MCP 的 R7/R8。完成后跑 T3/T4。
5. **agent/CLI 行为**：A2/A3/A4 先于 A1；A1 依赖资源预算和 MCP inventory 语义；A6 决策后实施 A7，跑 T5。
6. **工程收口**：E3/E4 可立即处理；E1/E2/E5-E9 按发布和依赖政策安排；E10 只在测试护栏建立后进行。
7. **单独 roadmap**：RM1-RM6 不阻塞上述阶段，按产品价值和 ADR 另立计划。

## 验收与校验

每个实现提交仍必须通过仓库约定：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

建议增加的门槛和专项命令：

```bash
cargo rustdoc --lib -- -D warnings
cargo check --release --all-targets
bash scripts/ci.sh
```

专项验收至少应证明：

- 任意 session id 都不能逃出 sessions 根目录，坏会话恢复后追加内容可在下一次启动读回。
- 会话、settings、安装信息在崩溃/并发读取时要么是旧完整版本、要么是新完整版本；权限不向同机其它用户公开 secrets。
- 子进程、MCP、HTTP 错误 body、tree、read/edit/skill 都有硬上限；超限、取消、超时会终止相关工作且错误可见。
- plugin command/MCP 不会默认继承 provider credentials；路径含 shell 特殊字符时仍按字面传递。
- malformed message/config/provider/manifest 会在边界被拒绝，错误指出来源和字段，而不是在后续 loop 中模糊失败。
- CLI 的 stdout/stderr、退出码和 Ctrl-C 行为有稳定测试，历史 live key 测试保持可选。
- 当前文档数字、ADR 与计划状态一致，rustdoc 无 warning；依赖审计政策明确且可重复执行。

## 风险与假设

- **安全责任边界**：本方案假设外部 sandbox 是最终隔离层，但 instagent 仍需做路径、环境、资源和错误输出的纵深防御。
- **兼容性**：session JSONL、settings merge 和 provider manifest 都可能已有用户数据；迁移应优先向后兼容，破坏性变更须带版本/迁移提示。
- **性能**：bounded output/timeout 可能改变目前“返回完整输出”的行为；必须先定义用户可取得的 spill/reference 方式，再设置默认上限。
- **并发**：并行 tool execution 只允许无依赖、可证明安全的调用；写操作、同一资源冲突和顺序敏感调用继续串行。
- **凭据**：删除或启用 config API key 字段是产品选择，未决前不应悄悄改变优先级。
- **历史文档**：`docs/goose-*.md` 是只读参考，不应被当作当前功能清单；只加清晰的历史标记和当前映射。
- **依赖**：结构化图片、secure openat、fuzz 和平台发布可能需要新增依赖/工具链，必须单独批准，不在本方案中隐式引入。
