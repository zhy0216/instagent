# repo-improvements-2026-09-04 任务队列

本队列基于 `plans/repo-improvements-2026-09-04/plan.md` 与当前仓库实现拆解。
每个文件是一个独立任务、一个 worktree、一个最终 commit；任务完成后必须运行：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

easy / medium 任务使用 flash，hard 任务使用 max。

## 优先级

| 文件 | 优先级 | 难度 | 说明 |
|---|---|---|---|
| `done/01-policy-decisions.md` ✅ | P1 | medium | 固化密钥、环境、hook、CLI 流和 settings 语义，以及 sandbox 责任边界 |
| `done/02-session-boundary.md` ✅ | P1 | medium | session id、坏尾部 salvage、权限、耐久性和 `last` 选择 |
| `done/03-settings-atomic-merge.md` ✅ | P1 | medium | settings / 安装元数据原子私有写入、tri-state 合并和本地配置忽略 |
| `done/04-file-tool-budgets.md` ✅ | P1 | hard | fs/tree/skills/image 的阻塞、路径、大小、深度和取消边界 |
| `done/05-subprocess-collector.md` ✅ | P1 | medium | 进程输出上限、增量 UTF-8 和无 pipe 配置错误 |
| `done/06-process-tool-isolation.md` ✅ | P1 | hard | shell/command/hooks 的 bounded output、argv/env 隔离和 hook 策略 |
| `done/07-plugin-install-resilience.md` ✅ | P1 | medium | git 安装超时/进程组、替换回滚、孤儿备份和诊断 |
| `done/08-config-provider-validation.md` ✅ | P1 | hard | config/provider 字段校验、key 来源实现、schema drift 和 context limit 诊断 |
| `done/09-message-contract.md` ✅ | P1 | medium | message role/tool id/image 合约与统一边界校验 |
| `done/10-mcp-inventory.md` ✅ | P1 | medium | MCP 连接部分失败、inventory timeout/cancel、缓存和可见诊断 |
| `done/11-agent-event-contract.md` ✅ | P2 | medium | event 背压、残缺 stream 和 session hook 错误可见性 |
| `done/12-cli-stream-and-e2e.md` ✅ | P1 | hard | stdout/stderr 契约、退出码、取消和 CLI/PTY 回归 |
| `done/13-parallel-tools.md` ✅ | P1 | hard | 只读工具并行、冲突排序、图片/并发预算和取消 |
| `done/14-manifest-schema.md` ✅ | P2 | medium | plugin manifest 版本、字段形状和跨字段校验 |
| `done/15-discovery-diagnostics.md` ✅ | P2 | medium | 插件来源类型和目录枚举错误诊断 |
| `done/16-docs-and-rustdoc.md` ✅ | P2 | easy | 当前工具数量、ADR/历史计划标记和 rustdoc 警告清理 |
| `done/17-ci-release-metadata.md` ✅ | P2 | medium | 包元数据、toolchain 支持范围、audit/doc/release CI 门槛 |
| `done/18-provider-converter.md` ✅ | P2 | medium | provider converter 的 source 注入、fixture 和 round-trip 校验 |
| `19-agent-module-split.md` | P2 | hard | 在行为测试护栏下拆分 agent loop 与工具执行职责 |
| `20-hooks-provider-split.md` | P2 | hard | 在契约锁定后拆分 hooks 与 OpenAI transport/parser |
| `done/21-install-module-split.md` ✅ | P2 | medium | 在安装回归稳定后拆分安装流程、git 和替换状态机 |

## 文件

1. `done/01-policy-decisions.md` ✅ — 完成。ADR 0003 落地六项决策（D1–D6），解锁所有策略相关实现。
2. `done/02-session-boundary.md` ✅ — 完成。validate_session_id 白名单、salvage 原子写回+有界时间戳备份、0700/0600 权限、rename 失败清理、list/`last` 诊断跳过。
3. `03-settings-atomic-merge.md` — 依赖：01。
4. `done/04-file-tool-budgets.md` ✅ — 完成。fs/tree/skills/image 阻塞逻辑移入 `spawn_blocking` 并按行/按块检查取消；tree 增加 entries/字节/输出/深度/时间五预算与结构化截断 note（`depth=0` 受内部深度兜底）；read/edit/write 与 skill 文件设字节上限（metadata 预检 + `take(MAX+1)` 兜底增长竞态）；skill description 强制非空、supporting file 明确 no-follow；image 摘要带 base64 负载大小供后续总预算观测（新增 22 测试，取消耗时实测 ≤0.5ms）。
5. `done/05-subprocess-collector.md` ✅ — 完成。`run_bounded` 硬上限 collector（越限杀组、保留摘要、增量 UTF-8），`wait_and_drain` 无 pipe 改带上下文错误。
6. `done/06-process-tool-isolation.md` ✅ — 完成。shell/command/hooks 接入 `run_bounded` 显式预算（越限杀组、保留可操作摘要，`wait_and_drain` 兼容入口随之删除）；`${PLUGIN_ROOT}` 改经环境变量传递不拼 `sh -c`，command 工具接入共享 env baseline + manifest allowlist；hook 失败全量可见（spawn/超时/超限/无决策 warning 带插件/事件/命令/原因），on_failure 缺省 fail-open、显式 block 为 fail-closed；shell spill 加会话标记/随机后缀 + TTL 清理 + 失败诊断；tool JSON 定义 64KiB 读取上限带来源诊断。含空格/引号/换行路径与 env 隔离测试。
7. `done/07-plugin-install-resilience.md` ✅ — 完成。git clone/rev-parse 走 `run_bounded` wrapper（进程组+kill_on_drop、超时、取消、bounded output，超时 fake git 进程组无残留），错误只回显截断摘要且去 URL 凭据；replace_dir 回滚失败指明备份路径、`.replaced-*` 孤儿备份换入前清扫、`.tmp-install` staging 孤儿 TTL 清扫；copy_tree symlink 契约（一律拒绝）+ 测试。
8. `done/08-config-provider-validation.md` ✅ — 依赖：01。完成。config 字段级校验 + ADR 0003 D1 密钥唯一来源；provider schema 补齐 display_name/description/max_tokens，context_limit 诊断不再静默，provider JSON/HTTP 错误 body 读取有界。
9. `done/09-message-contract.md` ✅ — 完成。validate 扩为 role/content 矩阵（ToolUse 仅 assistant、ToolResult/Image 仅 user、id/name 非空且全局唯一、紧邻结果 id 多重集精确相等、image MIME/canonical base64/20MiB）；provider wire（openai，proxy 经由它）、Session append/rewrite/salvage 统一调用同一校验核心；错误带消息/block 索引与约束。依赖：01。
10. `done/10-mcp-inventory.md` ✅ — 完成。connect/list 硬超时、`inventory()` 错误通道不静默空列表、单 server 失败保留健康 source 并产出带 plugin/server 的 note、Registry 清单+route 缓存与 invalidate、mcp.json 1MiB 上限与 headers/command 来源诊断、stdio env 走 D2 baseline（含 PLUGIN_ROOT 环境变量）。依赖：01、05。
11. `done/11-agent-event-contract.md` ✅ — 完成。event 通道消费契约：`emit` 先 `try_send`，满载最多等 `EMIT_GRACE`(250ms) 后丢弃并经进程级 `dropped_event_count()` 计数（tracing debug 记明细），接收端断开即丢即计数——不消费 / 慢消费 / 断开的调用 turn 都有明确终止，不再无限期卡住；调用方可按容量与 drain 方式自选非阻塞/丢弃语义。残缺 provider stream（A3）：`ToolUseEnd` 前 EOF 的 tool-use 保留已收集片段、按 malformed 提升（loop 补 is_error ToolResult，模型可见，不再静默丢弃），`Done` 前 EOF / 截断记入 `AssistantStream::incomplete`（`StreamIncomplete`，阶段+计数、不回显原文，对齐 09 message 错误模型），同一 note 以 `Event::Error` 上报事件层；取消不算截断，流内 ProviderError 原样上抛（ContextOverflow 重试依赖）。CLI session hook（A4 / D3）：chat/run 的 4 处 `let _ =` 改为 `report_session_hook`，失败输出含事件阶段与来源（错误链）的 stderr warning 行、退出码不变，Block/None 意外决策同样 fail-open + warning。新增 6 个 agent 测试（含不消费/断开 channel 回归）+ 3 个 handler 纯逻辑稳定断言测试。依赖：06、09。
12. `done/12-cli-stream-and-e2e.md` ✅ — 依赖：06、07、10、11。完成。落地 ADR 0003 D4 输出契约：`render_event`/`finish_turn` 改为注入答案流（stdout，只放 TextDelta）与诊断流（stderr，工具事件/预览/usage/compaction/error），写失败（EPIPE）忽略、不改退出码；repl 横幅、斜杠命令反馈、`/help`、Ctrl-C 提示全部迁 stderr；chat 的装配 notes 迁 stderr；`plugin update` 失败从 stdout 静默改到 stderr `error:` 行并返回非零退出。`tests/cli_e2e.rs` 扩为 15 个真进程测试：纯答案 stdout 断言（含工具轮）、EPIPE 退出码不变、坏参数/无 provider/连接失败的非零退出与末行 `error:`、resume 跨命令会话连续、`/clear`/`/compact`、MCP 失败可见 note 不致命、轮内 Ctrl-C 取消并整组回收 shell 孙进程（`kill -0` 断言）。交互路径经管道 stdin 驱动（不新增 PTY 依赖），空闲 Ctrl-C 为 tty/PTY 专有分支不覆盖。`live_e2e.rs` 按 D4 同步改混合输出断言（仍门控可选）。
13. `done/13-parallel-tools.md` ✅ — 完成。工具能力元数据（`ToolKind` / `CallCapability` / `Registry::capability`：`read_only` 取各来源声明，资源键只由内核为内置 fs 系工具按输入 `path` 生成，未知工具保守串行）；`execute_calls` 三段式（串行决策 → `execution_units` 划分 → 单元串行 / 单元内 `join_all` + `Semaphore` 有界并发，`PARALLEL_TOOL_LIMIT=4`），写 / 同资源 / 顺序敏感调用保持原顺序，结果按原下标落槽；取消时在途任务短路、未启动单元补 `interrupted`，事件与结果按调用 id 一一对应、会话不变量不变；会话图片预算 `SESSION_IMAGE_BUDGET=64MiB`（超限拒绝新图、工具结果带可操作提示，CAS 防并行超限），请求侧 `REQUEST_IMAGE_BUDGET=32MiB`（相同内容去重、超限按生命周期淘汰最旧优先，替换为可重读提示）；`plans/parallel-tool-execution/plan.md` 同步重写，删除 ADR 0002 之前的 approval/trust 假设并写明 sandbox 边界与并行安全前提。新增 17 测试覆盖并发 / 顺序 / 取消 / 超时 / 失败隔离 / 事件配对 / 图片预算；未引入新 rustdoc 警告（存量 8 个 private-link 警告归 16）。依赖：04、05、06、09、10、11。
14. `done/14-manifest-schema.md` ✅ — 依赖：01、08。完成。`plugin.json` 逐字段形状 + 跨字段校验（`version` 必填非空、非 SemVer 按 §5.4 只 warning；`author` 对象封闭；`keywords` 逐元素；`extensions` 命名空间反域名 + 值为对象，`dev.instagent.minKernel` 类型），未知顶层字段与非对象 `extensions` 按规范报告并忽略；读取 1 MiB 硬上限、解析错误只回显截断摘要；错误统一「来源文件 + 插件名 + 字段 + 建议值」。
15. `done/15-discovery-diagnostics.md` ✅ — 依赖：03。完成。`PluginSource` 拆出 `Cli` kind + `display_name()`，skipped/错误统一「来源 [绝对路径]: 原因」；目录枚举改为区分"不匹配"（散文件、无 `plugin.json` 的目录 → 静默）与"读取失败"（权限、IO、坏 symlink、逐条 entry 失败 → 汇总诊断），commands 侧同口径且有界读取（256 KiB）；`Settings::whitelist()` + 共享 `plugin_enabled()` 把 ADR 0003 D5 三态接到 discovery/bundled（显式 `[]` = 禁用全部），写回忠实表达三态。依赖：03。
16. `done/16-docs-and-rustdoc.md` ✅ — 依赖：08、12、13、14。完成。README/usage/architecture/tools 模块文档的内置工具数同步为 6（含 `read_image`）；api_key/api_key_env 描述按 ADR 0003 D1 校正（config.yaml 不含密钥、旧键加载期报错、唯一来源为 provider JSON 指向的环境变量），run -t 输出契约描述按 ADR 0003 D4 改为 stdout 仅最终答案，settings 三态与 settings.local gitignore 说明对齐 ADR 0003 D5，README 补 ADR 0003 索引；`docs/goose-*.md` 两份历史设计文档加"历史/当前映射"标记（不整体改写）；`plans/thinking-blocks/plan.md` 标记已被 ADR 0001 淘汰关闭（parallel-tool-execution 已由 13 重写，无残留）。rustdoc：8 处 public→private intra-doc 链接（hooks/message/shell/command）降级为代码段，`cargo rustdoc --lib -- -D warnings` 通过；bundled/openai/fake_proxy_server 存量已在 08 前后清零。
17. `done/17-ci-release-metadata.md` ✅ — 依赖：01、16。完成。Cargo.toml 补齐发布元数据（license Apache-2.0 对齐 goose 基线、repository/homepage/documentation/keywords/categories、`rust-version = "1.93"` 以 `cargo +1.93.1 check --all-targets` 实证）；rust-toolchain.toml 钉死 1.94.0，CI 移除 moving stable、与本地共读同一文件；ci.yml/ci.sh 增加 rustdoc-as-error、release smoke、weekly scheduled audit（PR 非阻断 + 原因/owner 记录，schedule 阻断），新增 docs/release.md 记录安全政策、tokio feature 收窄评估（不执行，依赖锁定）与 A8 采样：顺序 5×7 全过、并发 24 run 复现 1 次 readiness flake，根因定为 `free_port()` bind→drop→child bind 的 TOCTOU 竞态，确定性修复点在 src/（超出本任务文件面）留待后续任务，并发采样命令作为其回归验收。
18. `done/18-provider-converter.md` ✅ — 完成。converter 改 `--source DIR`/`--fixture` 显式注入（不再硬编码 ~/yyds/goose）；产物按 08 的 ProviderDef/ModelDef 契约做 schema + round-trip 校验，契约外字段剔除、约定字段保留，坏 source/schema 非零退出带文件与字段诊断；新增最小 fixture 与 11 个可重复 python 测试，真实 goose 源重生成 groq/deepseek 与提交物逐字节一致。依赖：08。
19. `19-agent-module-split.md` — 依赖：11、13、16。
20. `20-hooks-provider-split.md` — 依赖：06、08、11、13、16。
21. `done/21-install-module-split.md` ✅ — 依赖：07、12、16。完成。`install.rs` 按职责拆为五个私有子模块（acquire git 获取与错误脱敏回显 / metadata `.install.json` 读写 / staging 目录与 copy_tree symlink 契约 / replace `.replaced-*` 回滚状态机 / update 手动与 24h 节流），公开 API、CLI 输出与 `PluginSource` 语义不变、不动模块树；闭环 15 的残留：`list()` enabled 列改用共享 `plugin_enabled()`（显式 `[]` 白名单不再误报"全部已启用"），新增回归测试。

## 并行批次

- `02`、`03`、`04`、`05`、`08`、`09` 可在 `01` 合并后并行；它们的代码文件互不重叠（`03` 对 `plugin/install.rs` 的改动完成后再启动 `07`）。
- `06`、`07`、`10`、`14`、`15` 依赖前置底座，可按文件不重叠原则并行。
- `11` 完成后才能开始 `12`；`13` 必须等资源、MCP 和 event 契约稳定。
- `16`、`17`、`18` 属于收口阶段；`19`–`21` 是所有行为测试通过后的最后重构任务，不应提前并行。

以下内容明确不入队：方案中的 RM1–RM6 roadmap 项，以及 T6 property/fuzz/覆盖率长期门槛；它们应另立 ADR/计划。
