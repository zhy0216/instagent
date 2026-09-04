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
| `11-agent-event-contract.md` | P2 | medium | event 背压、残缺 stream 和 session hook 错误可见性 |
| `12-cli-stream-and-e2e.md` | P1 | hard | stdout/stderr 契约、退出码、取消和 CLI/PTY 回归 |
| `13-parallel-tools.md` | P1 | hard | 只读工具并行、冲突排序、图片/并发预算和取消 |
| `done/14-manifest-schema.md` ✅ | P2 | medium | plugin manifest 版本、字段形状和跨字段校验 |
| `done/15-discovery-diagnostics.md` ✅ | P2 | medium | 插件来源类型和目录枚举错误诊断 |
| `16-docs-and-rustdoc.md` | P2 | easy | 当前工具数量、ADR/历史计划标记和 rustdoc 警告清理 |
| `17-ci-release-metadata.md` | P2 | medium | 包元数据、toolchain 支持范围、audit/doc/release CI 门槛 |
| `done/18-provider-converter.md` ✅ | P2 | medium | provider converter 的 source 注入、fixture 和 round-trip 校验 |
| `19-agent-module-split.md` | P2 | hard | 在行为测试护栏下拆分 agent loop 与工具执行职责 |
| `20-hooks-provider-split.md` | P2 | hard | 在契约锁定后拆分 hooks 与 OpenAI transport/parser |
| `21-install-module-split.md` | P2 | medium | 在安装回归稳定后拆分安装流程、git 和替换状态机 |

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
11. `11-agent-event-contract.md` — 依赖：06、09。
12. `12-cli-stream-and-e2e.md` — 依赖：06、07、10、11。
13. `13-parallel-tools.md` — 依赖：04、05、06、09、10、11。
14. `done/14-manifest-schema.md` ✅ — 依赖：01、08。完成。`plugin.json` 逐字段形状 + 跨字段校验（`version` 必填非空、非 SemVer 按 §5.4 只 warning；`author` 对象封闭；`keywords` 逐元素；`extensions` 命名空间反域名 + 值为对象，`dev.instagent.minKernel` 类型），未知顶层字段与非对象 `extensions` 按规范报告并忽略；读取 1 MiB 硬上限、解析错误只回显截断摘要；错误统一「来源文件 + 插件名 + 字段 + 建议值」。
15. `done/15-discovery-diagnostics.md` ✅ — 依赖：03。完成。`PluginSource` 拆出 `Cli` kind + `display_name()`，skipped/错误统一「来源 [绝对路径]: 原因」；目录枚举改为区分"不匹配"（散文件、无 `plugin.json` 的目录 → 静默）与"读取失败"（权限、IO、坏 symlink、逐条 entry 失败 → 汇总诊断），commands 侧同口径且有界读取（256 KiB）；`Settings::whitelist()` + 共享 `plugin_enabled()` 把 ADR 0003 D5 三态接到 discovery/bundled（显式 `[]` = 禁用全部），写回忠实表达三态。依赖：03。
16. `16-docs-and-rustdoc.md` — 依赖：08、12、13、14。
17. `17-ci-release-metadata.md` — 依赖：01、16。
18. `done/18-provider-converter.md` ✅ — 完成。converter 改 `--source DIR`/`--fixture` 显式注入（不再硬编码 ~/yyds/goose）；产物按 08 的 ProviderDef/ModelDef 契约做 schema + round-trip 校验，契约外字段剔除、约定字段保留，坏 source/schema 非零退出带文件与字段诊断；新增最小 fixture 与 11 个可重复 python 测试，真实 goose 源重生成 groq/deepseek 与提交物逐字节一致。依赖：08。
19. `19-agent-module-split.md` — 依赖：11、13、16。
20. `20-hooks-provider-split.md` — 依赖：06、08、11、13、16。
21. `21-install-module-split.md` — 依赖：07、12、16。

## 并行批次

- `02`、`03`、`04`、`05`、`08`、`09` 可在 `01` 合并后并行；它们的代码文件互不重叠（`03` 对 `plugin/install.rs` 的改动完成后再启动 `07`）。
- `06`、`07`、`10`、`14`、`15` 依赖前置底座，可按文件不重叠原则并行。
- `11` 完成后才能开始 `12`；`13` 必须等资源、MCP 和 event 契约稳定。
- `16`、`17`、`18` 属于收口阶段；`19`–`21` 是所有行为测试通过后的最后重构任务，不应提前并行。

以下内容明确不入队：方案中的 RM1–RM6 roadmap 项，以及 T6 property/fuzz/覆盖率长期门槛；它们应另立 ADR/计划。
