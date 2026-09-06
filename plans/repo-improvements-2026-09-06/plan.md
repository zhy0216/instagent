# instagent 仓库改进方案（2026-09-06）

## 意图与探索结论

用户调用 `$auto-dev`，没有指定开发需求，按仓库探索模式检查当前项目并安排后续执行。基线为 `8a446c4`。前几轮仓库改进和 headless 迁移已经落地，本轮重点是组合路径中的正确性：异常工具响应仍会产生副作用，压缩可能接受截断摘要或丢失最新任务，文件原子替换会改变权限。另补齐写入与读取预算、插件安装边界及真实模型测试隔离。当前 session 只生成本计划目录内的方案和队列；实现由后续 Herdr 协调器执行。

### 证据与基线校验

- 已读取根 `AGENTS.md`、README、Cargo/toolchain/CI 配置、当前 architecture/usage/release、ADR 0004，以及既有改进方案和完成记录。历史设计不作为恢复交互 CLI 的依据。
- 语义检索返回 `INDEX_MISSING`，之后使用 `rg`、定点源码读取和隔离复现；没有创建或重建持久索引。
- 初始工作区存在 `?? tests/fixtures/liveplug/.hook-out/`。它在本轮运行测试前就存在，不能当成本轮独占产物清理。现有 live 测试也会写这个目录；本轮不提交、stash 或删除其中内容。
- `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 通过。
- 原样 `cargo test` 退出 101：当前环境已有 `TOKEN_PLAN_API_KEY`，触发 10 个 live 用例，全部在 180 秒整体期限超时。共同诊断为 `tests/live_e2e.rs:124:10` 的 `instagent 二进制执行超时（180s）: Elapsed(())`。不能仅凭超时认定远端具体故障，也不把这次运行称为全绿。
- `env -u TOKEN_PLAN_API_KEY cargo test` 退出 0：实际执行 586 个 Rust 测试（lib 463、bin 10、agent_continuation 21、cli_e2e 49、mcp_e2e 15、provider_proxy 15、session_recovery 9、tool_inventory 4）；live 的 10 个测试函数门控返回，未验证在线行为；doc-test 0。
- Python 回归 `PYTHONDONTWRITEBYTECODE=1 python3 -W error::ResourceWarning -m unittest discover -s tests -p 'test_*.py'`：11 个通过。
- `cargo rustdoc --lib -- -D warnings`、`cargo check --release --all-targets`、`cargo +1.93.1 check --locked --all-targets`、当前二进制 `--help` 均通过；`cargo machete` 没有报告未使用的直接依赖。
- 本机没有 cargo-audit，未安装审计工具或升级依赖；CI 已有定时 RustSec 扫描。常见凭据字面量筛查只命中两个明确的测试假密钥，不代表完成了全面秘密或漏洞审计。
- 可执行源码没有发现待实现宏；唯一 TODO 字样是 `src/lib.rs` 的历史归属说明。没有为了清理注释而扩大修改范围。

### 隔离复现

使用当前 `target/debug/instagent`、临时 config/data/agents/cwd 和仅监听 loopback 的 Python HTTP fixture；进程独立成组并设整体期限。没有修改业务源码或真实配置，以下复现没有访问外部模型。源码推断另行标注。

| 场景 | 当前结果 | 发现 |
| --- | --- | --- |
| provider 返回参数完整的 shell call，以 `finish_reason: length` 结束，下一响应正常 | shell 标记文件已创建，最终 `completed` / 0 | A01 |
| 已有历史触发压缩；摘要文本非空但以 `length` 结束 | 活动历史中的原始标记消失，被截断摘要替代；最终 `completed` / 0，旧内容仅可能保留在备份中 | C01 |
| 会话末尾有未回答 user，恢复时追加新任务并立即压缩 | 最新任务标记既不在摘要/后续请求中，也不在活动历史中；旧任务仍在，最终 `completed` / 0 | C02 |
| 在 umask 022 下 edit 普通文件，原权限分别为 0755 和 0600 | 两者都变成 0644，执行位丢失或访问权限扩大 | F01 |
| edit 33,554,430 字节的文件，将唯一 `old` 换为 `larger` | 文件增至 33,554,433 字节，超过 33,554,432 字节预算，仍报告成功 | F02 |
| provider 声明必需 `api_key_env`，对应变量设为空字符串 | 向假 provider 发出 1 次请求并 `completed` / 0 | P01 |
| 本地安装源目录包含配置后的 agents 安装根 | 复制进入自身 staging，最终退出 1，诊断为路径过长；应在递归复制前明确拒绝 | I01 |

补充排除：以 `timeout_secs = u64::MAX` 调用执行 `true` 的 shell 没有出现 panic，不能将极大 timeout 直接列为已确认崩溃。正常工具名已由 Registry 映射，不重复安排 provider 工具名反向映射修复。

## 目标与非目标

目标：不执行已知异常响应里的工具；压缩保留完整摘要与最新输入；文件编辑保留权限并遵守预算；成功写入的会话可在相同预算下再次恢复；组件文件读取有实际字节上限；本地安装拒绝递归复制自身；显式必需的空凭据在构造期失败；普通回归默认离线且测试不写共享源码夹具。

约束：保持现有依赖、feature、Rust 模块树和 headless 接口。每个实现任务只修改自己列出的文件，并在同一任务补行为测试。子进程沿用进程组和 `kill_on_drop(true)`；不改 goose 参考树，不改根或旧计划的 `todos/done/`。执行只包含本地 worktree、commit、rebase、合并与任务目录归档，不包括 push、发 PR、发布或部署。

非目标：交互问答/审批/REPL、新 provider 引擎、新插件 ABI、全路径 containment、跨进程会话事务、依赖升级、图像 decoder 或发布渠道选择。ADR 0002 的 sandbox 边界、ADR 0003 的环境白名单和 hook 失败策略保留。

## 方案与关键决策

1. **响应完成状态先于副作用。** agent 在 PreToolUse、ToolStart 和实际执行前检查带工具响应的完整性与终止原因。正常 ToolUse、现有兼容的正常结束路径保留；MaxTokens、未知异常原因或明确不完整的响应不能执行工具，也不能通过下一次回答掩盖成成功。取消仍返回 cancelled；正常完整响应里单个 JSON 参数损坏仍按既有工具错误反馈处理。失败记录保持消息/工具结果配对。
2. **摘要必须正常完成，未回答输入按内容块保留。** `summarize` 要求非空文本和正常 EndTurn，拒绝长度截断、异常原因以及摘要请求不应返回的工具事件。`split_head_tail` 不再只取第一个 Text；保留未回答消息的全部有序内容块，特别是 `prepare_input` 合并进来的新任务。旧 tool results 的配对不放宽，图片不被静默丢弃或塞成 base64 摘要文本。
3. **文件原子替换同时保护元数据和大小。** 同目录临时文件以排他方式创建，从创建起避免内容可被额外读取；覆盖已有普通文件时保留普通 rwx 权限，特别是 0755 和 0600，不自动继承 setuid/setgid。新文件遵循现有创建权限约定。edit 预估/检查替换后字节数，越界不改文件。read 在实际读取层限制字节，不能等无界 `lines()` 已分配完长行再止损。
4. **会话写入与恢复共享预算。** create/append_batch/rewrite 对序列化后的 header、消息行和总文件大小使用与恢复一致的上限；包含 JSON 转义和换行开销，使用受检运算。越界在提交前拒绝，内存、原文件与可用备份保持原状；不静默裁剪、不自动丢历史，不调整现有默认预算。测试注入小预算，避免日常测试分配数百 MB。
5. **组件读取预算落到 reader。** manifest、mcp config、provider JSON 的 metadata 上限后补实际 `take(limit + 1)` 读取及读后检查。hooks.json 和安装元数据各增加 1 MiB 默认预算。使用现有依赖/模块内 helper，不为了共享几行代码改 `src/lib.rs`。保留各来源既有 fatal/skip/fail-open 策略，错误不回显原始文件或凭据。
6. **本地安装与任务模板都做展开前检查。** 安装前规范化源与目标/staging 的关系，拒绝源包含目标 staging 的重叠情况，同时保留重新安装一个普通已安装插件目录的能力。模板按占位符数量、参数 UTF-8 字节数和追加分支受检计算结果大小，默认展开结果上限 1 MiB；超限在分配巨大 String 和创建会话前失败。
7. **只对有效图片记账；必需凭据拒绝空值。** 图片先验证，再原子预留会话预算；被拒绝的图片不消耗其他调用额度。`api_key_env` 声明后，缺失、空串或纯空白都报只含 provider/变量名的诊断；未声明该字段的 keyless provider 保持支持。
8. **在线回归显式运行，夹具每测试独占。** live 用例标为显式 opt-in（采用 Rust ignored tests），普通 `cargo test` 即使继承 key 也不联网；显式运行时缺 key 要明确失败，不能假绿。每个 Sandbox 复制一份只含版本化输入的 liveplug，hook 输出位于自己的临时目录，不导入既有 `.hook-out`。增加无需模型的隔离回归，证明并行写入和清理不互相影响。

## 完整发现清单与拆解

P0 表示紧急事故级问题；本轮没有证据表明存在需要 P0 处置的外部事故。以下非 roadmap 条目全部进入队列；同文件发现归为同一任务。

| ID | 类别 / 位置 / 问题 | 改进与验收 | 优先级 / 难度 | 任务 |
| --- | --- | --- | --- | --- |
| A01 | 正确性：`src/agent/mod.rs::run_turn` 只在 calls 为空时检查异常终止；`exec.rs` 先执行完整参数工具 | 复现的 length 工具响应零 hook/工具副作用、终态 failed，消息可恢复；补异常原因矩阵 | P1 / hard | 01 |
| A02 | 健壮性：`src/agent/exec.rs::execute_calls` 在 image_validation_note 前 reserve_image_bytes，非法图片已占额度（源码） | 只为有效、实际保留的图片记账；非法图之后有效图仍可使用预算，并行合计不超限 | P2 / medium | 01 |
| C01 | 正确性：`src/agent/compact.rs::summarize` 见任意 Done 就接受，忽略 stop_reason | 截断/异常/带工具摘要拒绝 rewrite；原文件逐字节不变；正常摘要仍成功 | P1 / hard | 02 |
| C02 | 数据保护：`compact::split_head_tail` 对未回答 user 只保留 content.first，而 prepare_input 会追加多个 Text | 恢复后新任务、原有多个 Text/Image 块均保留；摘要与下一次请求可验证最新输入 | P1 / hard | 02 |
| F01 | 正确性/权限：`src/tools/builtin/fs.rs::atomic_write` 用 fs::write 新建临时文件再 rename，丢失原 mode | write/edit 覆盖均保留普通权限；临时文件从创建起私有、排他；失败清理 | P1 / medium | 03 |
| F02 | 资源：`edit_file_sync` 只限制旧文件，未检查替换后大小 | 替换结果越界返回工具错误，旧文件及 mode 不变；刚好预算可成功 | P2 / medium | 03 |
| F03 | 资源：`read_file_sync` 在 BufRead::lines 返回整行之后才检查累计字节；增长长行可先无界分配（源码） | reader 层有硬上限，长行/读取中增长有确定性边界测试，行号与截断诊断保持一致 | P2 / medium | 03 |
| S01 | 数据保护：`src/session.rs` 有 ReadLimits，但 create/append_batch/rewrite 不限制写入的对应大小（源码） | 不能成功写出自身默认 resume 拒绝的会话；超过任一预算零提交、正常恢复不变 | P1 / hard | 04 |
| L01 | 资源：`plugin/manifest.rs::read_bounded`、`plugin/mcp_config.rs::read_bounded`、`provider/registry.rs::read_provider_json` 只有 metadata 预检（源码） | 实际读取到 limit+1 即结束并报超限，文件增长不绕过预算 | P2 / hard | 05 |
| L02 | 资源/诊断：`src/hooks.rs::load_plugin_hooks` 直接 read_to_string，无文件预算（源码） | 1 MiB 有界读取；超限、UTF-8/JSON 错误带来源、不回显正文，加载失败语义不变 | P2 / medium | 05 |
| I01 | 安装正确性：`src/plugin/install.rs::install/copy_tree_at` 先创建 staging，再递归源目录，未拒绝源包含 staging | 重叠路径清晰拒绝、无递归副本、旧安装保持完整；普通本地安装/重装仍成功 | P1 / medium | 06 |
| I02 | 资源：`install.rs::metadata::read_install_info` 无大小限制（源码） | 1 MiB 读取上限，超限不改 metadata 或已安装插件，诊断不回显 source 中凭据 | P2 / medium | 06 |
| T01 | 资源：`src/commands.rs::expand` 无限 replace；CLI 的 1 MiB 仅检查 --task/--task-file（源码） | 展开前受检预算，重复占位符不能放大为无界任务；CLI failed / 1、session_id null，无 provider 请求 | P2 / hard | 07 |
| P01 | 配置正确性：`src/provider/shared.rs::engine_parts` 接受声明为必需的空 key，openai 请求因此不带鉴权 | 空串/纯空白与缺失一样构造失败，keyless provider 与合法 key 不受影响 | P2 / medium | 08 |
| E01 | 测试可靠性：`tests/live_e2e.rs::liveplug_fixture` 返回共享源码目录；d3 remove_dir_all 与其他 hook 并行写冲突 | 每测试独立复制夹具和输出、无共享删除；离线测试可证明隔离，仓库无新增产物 | P1 / medium | 09 |
| E02 | 测试/DX：live 仅由 key 是否存在自动启动，普通 cargo test 被环境耦合到远端，门控 return 又计为 passed | ignored 显式 opt-in；默认报告准确显示忽略，显式请求时缺 key 明确失败 | P2 / medium | 09 |
| D01 | 文档/契约：`docs/usage.md` 需要对应新模板预算/文件行为/凭据判定；README 和 release 需说明离线与 live 验证口径 | 按集成后的实际行为同步，不把跳过在线测试或未审计依赖写成验证通过 | P2 / medium | 10 |

### roadmap：记录全部较大观察，不进入执行队列

| ID | 位置与观察 | 建议及前提 | 优先级 / 难度 |
| --- | --- | --- | --- |
| RM01 | session append/salvage 多次全历史验证，validate_assistant/prepare_input 克隆历史；长会话可能呈二次成本 | 先建立确定性性能/内存基准，再设计增量校验；公开可变字段使缓存失效需单独解决 | P2 / hard |
| RM02 | compact 在格式化图片占位符前克隆历史，provider 请求构造仍复制 base64/data URL | 借用式格式化或 blob/reference 另案；本轮只修 C01/C02 的信息完整性 | P2 / hard |
| RM03 | read/image/其他本地同步 IO 对特殊文件或阻塞文件系统的强制取消无保证；图像只检查魔数 | 普通文件策略、完整 decoder/尺寸预算及平台处理需独立决策，保留 ADR 0004 的限制说明 | P2 / hard |
| RM04 | MCP v1 的 HTTP headers、SSE、非文本返回与依赖内部整体载荷仍有限制 | 新协议/内容能力需确定契约和依赖边界；本轮不将外层预算称为 rmcp 全内存约束 | P2 / hard |
| RM05 | 全批 PreToolUse 先于工具执行，PostToolUse 可并行；每事件满通道等待 250ms，慢 stdout 仍有同步阻塞 | 有状态 hook 与慢消费者的时序/性能基准后另定调度契约 | P2 / hard |
| RM06 | proxy 端口释放到子进程 bind 仍非原子；安装同步 join/递归复制仍缺 async API | 端口继承/回报协议、异步安装和总资源期限另案；历史 proxy 负载 flake 本轮离线检查未重现 | P2 / hard |
| RM07 | 直接依赖与 feature 锁定，tokio full、serde_yaml 维护告警背景保留；本地无 cargo-audit | 维护者授权解锁后基于当时公告审计与升级；本轮不推断具体漏洞或最新大版本 | P2 / medium |
| RM08 | 缺少 property/fuzz、持续覆盖率和资源基准；agent/mod 2125、session 1919、openai 1885 行，含大量测试 | 行为回归稳定后补 corpus/基准，按收益决定拆分；不以行数本身要求重构 | P2 / hard |
| RM09 | 许可选择仍待发版前复核，实体 LICENSE、发布渠道/签名/Windows 支持未确定 | 沿用 docs/release.md 的维护者决策要求，不擅自选发布方式或法律材料 | P2 / medium |
| RM10 | session/settings/同名插件跨进程事务和全路径隔离不在当前单进程/sandbox 契约内 | 新需求另立 ADR；预算与重叠路径检查不宣称提供完整并发事务或隔离 | P2 / hard |

## 任务顺序、依赖与文件归属

| 顺序 / todo | 涉及文件（实现白名单） | 难度 | 依赖 |
| --- | --- | --- | --- |
| 01-agent-completion.md | `src/agent/mod.rs`、`src/agent/exec.rs`、`tests/agent_continuation.rs` | hard | 无 |
| 02-compaction-integrity.md | `src/agent/compact.rs`（含模块内行为测试） | hard | 无 |
| 03-file-tool-io.md | `src/tools/builtin/fs.rs`（含模块内测试） | medium | 无 |
| 04-session-write-budgets.md | `src/session.rs`、`tests/session_recovery.rs` | hard | 无 |
| 05-plugin-input-limits.md | `src/plugin/manifest.rs`、`src/plugin/mcp_config.rs`、`src/provider/registry.rs`、`src/hooks.rs` | hard | 无 |
| 06-local-install-integrity.md | `src/plugin/install.rs`（含模块内测试） | medium | 无 |
| 07-template-input-budget.md | `src/commands.rs`、`src/cli/handlers.rs`、`tests/cli_e2e.rs` | hard | 无 |
| 08-provider-required-key.md | `src/provider/shared.rs`、`src/provider/openai.rs` | medium | 无 |
| 09-live-test-isolation.md | `tests/live_e2e.rs`；源码夹具只读，复制排除 `.hook-out` | medium | 无 |
| 10-docs-and-validation.md | `README.md`、`docs/usage.md`、`docs/architecture.md`、`docs/release.md` | medium | 01–09 全部合入 |

01–09 修改文件互不相交，可在资源允许时并行，按 README 顺序调度，Herdr 同时最多 5 个实现任务。02 在自己模块内构造测试 fixture，不修改 01 的集成测试文件。跨任务公共 API 保持兼容，特别是 Session 的公开方法和已有模板 expand 调用；必要的新受限入口保留旧调用的兼容路径。10 最后根据合入行为收口；任何新的文件交集必须先在队列明确调整所有权与依赖。

## 执行偏好

- `default_agent: codex`，来源：本次发起宿主 Codex；不是沿用上一轮 OpenCode 选择。
- 用户没有全局模型、推理强度或单任务 agent 覆盖；每个 todo 保存 `agent: inherit`。
- 依共享分发规则：easy → `gpt-6-astra` / `high`；medium → `gpt-6-astra` / `xhigh`；hard → `gpt-6-astra` / `max`。本队列没有 easy 任务。
- 新协调器：Codex `gpt-6-astra` / `high`；协调器启动档不覆盖任务难度映射。所有启动使用 skill 要求的显式 YOLO 参数。README 只保存 default_agent，不把难度默认映射写成用户 model/effort 覆盖。
- 新 session 必须读取本队列保存的偏好；启动前按分发规则检查实际 Codex CLI、模型和推理档位支持，不自行换类型或降档。
- 已核验本机 `codex --help` 支持 `--model`、`-c` 和 `--dangerously-bypass-approvals-and-sandbox`；本机模型元数据中的 `gpt-6-astra` 支持 high/xhigh/max。尚未创建 pane 或启动协调器。

## 校验与验收

每个实现 commit 必须通过根 AGENTS 要求：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

09 合入前，在不带 `TOKEN_PLAN_API_KEY` 的执行进程环境运行 `cargo test`，记录在线用例跳过；不修改全局环境或真实凭据。09 合入后普通测试即使继承 key 也不得联网，live 使用 `cargo test --test live_e2e -- --ignored` 显式运行，并要求有效凭据。在线不可用时明确记录未验证，不补造通过结果、不让执行协调器因反复在线重试挂住。

各任务另有预算边界、错误原子性、进程清理及行为回归。01/02 的异常流用内存假 provider；07 的 CLI 验收用 wiremock，均不能依赖真实模型。10 在最终合入态运行 `bash scripts/ci.sh` 和 `cargo +1.93.1 check --locked --all-targets`，核实 Python/rustdoc/release/CLI 冒烟；已通过后仅因新改动、失败或未解决风险重复检查。

## 风险与假设

- 0755/0600 为普通 POSIX 权限保证，不声称完整保留 ACL、扩展属性、属主或硬链接语义；特殊权限位不自动继承。
- 新 hooks/安装元数据上限和模板展开上限均采用 1 MiB；它们是本轮明确的本地资源策略，修改时需同步说明并测上限两侧，不可静默截断。
- --resume 的模型覆盖仍按现行每次调用处理，本轮不迁移会话头或更改配置优先级。
- 超预算会话写入会明确失败，工具已完成的外部副作用不能回滚；原合法会话必须保持可恢复，不能把拒绝写入表述为完整持久化。
- 真实模型测试已实测超时，原因未进一步归因；执行任务不得依赖远端健康。现有测试调用可能更新初始 `.hook-out`，该目录仍不能由本流程擅自处置。
- `HERDR_ENV=1` 检查通过，但启动前仍必须满足 auto-dev 的计划提交与干净工作区要求。若初始未跟踪目录仍在，先完成方案/队列，再报告该具体阻塞；不自动提交、stash、忽略或删除它，不绕过检查启动执行。

## 交接状态

本方案及队列用于后续执行。目前未实现业务变更、未提交计划、未启动 Herdr 协调器。提交/启动前重新检查 git status；仅当除本计划目录外没有未提交内容，或用户另有明确处理指令时继续。阶段结果不得写成业务任务已完成。

## 执行结果（2026-09-06）

本节替代前述规划阶段的“未提交/未启动”交接状态。用户已提交方案与队列至 `892dcfd7585ac0c47dafdde022f06de4242b6f5e`，并自行移出原有夹具输出；执行从该干净的 `main` 开始。10 个 todo 全部完成、分别归档到 `todos/done/`，每项保留一个实现提交，以串行 rebase、独立复核与校验、`git merge --ff-only` 合入 `main`。实际合入顺序为 03 → 02 → 01 → 05 → 04 → 06 → 08 → 09 → 07 → 10。

所有任务均由 Herdr 启动 Codex `gpt-6-astra`，显式传入 `--dangerously-bypass-approvals-and-sandbox`；实际推理强度如下。每个表中提交均在 rebase 后由协调器亲自执行仓库级校验，完整 CI 包含 fmt、clippy、Rust/Python 测试、rustdoc、release check 和 CLI help，MSRV 使用 `cargo +1.93.1 check --locked --all-targets`。

| 归档 todo | 合入 commit | Codex 推理强度 | 协调器校验证据 |
| --- | --- | --- | --- |
| [01-agent-completion.md](todos/done/01-agent-completion.md) | `6f8270343718139a4f8ea28786e4e3d077952aec` | max | fmt / clippy / test 全通过；611 实际离线通过；10 门控返回 |
| [02-compaction-integrity.md](todos/done/02-compaction-integrity.md) | `26f3486fd3ef713d41a84bb25643f7c5bfbb9137` | max | fmt / clippy / test 全通过；606 实际离线通过；10 门控返回 |
| [03-file-tool-io.md](todos/done/03-file-tool-io.md) | `a388155a6b553a8c805b94912a92b29ce8f9e1bd` | xhigh | fmt / clippy / test 全通过；600 实际离线通过；10 门控返回 |
| [04-session-write-budgets.md](todos/done/04-session-write-budgets.md) | `24810c816743974375f099887e40c56831d4dfba` | max | fmt / clippy / test 全通过；637 实际离线通过；10 门控返回 |
| [05-plugin-input-limits.md](todos/done/05-plugin-input-limits.md) | `f31ba3c315807fc1b7ec68c5f429f5ce9ea8cbb9` | max | fmt / clippy / test 全通过；625 实际离线通过；10 门控返回 |
| [06-local-install-integrity.md](todos/done/06-local-install-integrity.md) | `e9c308b1008ec511ad973744f182aff9e022e1a1` | xhigh | fmt / clippy / test 全通过；647 实际离线通过；10 门控返回 |
| [07-template-input-budget.md](todos/done/07-template-input-budget.md) | `dd382aae6fb86c113ba73041ab8ac85ed0020457` | max | fmt / clippy / test 全通过；662 passed；10 ignored |
| [08-provider-required-key.md](todos/done/08-provider-required-key.md) | `88021a55b579f2c168c9bb5573539fae0995f377` | xhigh | fmt / clippy / test 全通过；651 实际离线通过；10 门控返回 |
| [09-live-test-isolation.md](todos/done/09-live-test-isolation.md) | `152923a98f9e8ae63fe43e4a11481b332f0cc379` | xhigh | fmt / clippy / test 全通过；653 passed；10 ignored |
| [10-docs-and-validation.md](todos/done/10-docs-and-validation.md) | `ab2d89088eaef64e73fabf6d2d6445ff489307ae` | xhigh | 完整 CI + MSRV 全通过；662 passed；10 ignored |

01–06、08 的 `test` 使用 `env -u TOKEN_PLAN_API_KEY cargo test`，当时旧 live harness 的 10 个门控返回不计入实际离线测试。09 的集成校验同样移除进程凭据，但已明确显示 10 ignored；07 及 10 使用普通 `cargo test`。所有计数仅汇总各测试目标最终结果，不重复计算 lib 测试启动的子 harness。

最终集成态：Rust 662 passed、10 ignored、0 failed（lib 527、bin 11、agent_continuation 24、cli_e2e 53、live 离线 2、mcp_e2e 15、provider_proxy 15、session_recovery 11、tool_inventory 4；doc-test 0）；Python 11 项通过；rustdoc、release check、CLI help 与 Rust 1.93.1 MSRV 全通过。`cargo-audit` 未安装，按现有策略跳过，未安装全局工具或改动依赖/扫描豁免。每项具体行为回归和工作分支采样记录见上表归档文件；协调器原始校验日志保存在本机 `/tmp/instagent-herdr-20260906-qupwTc/NN-validate-*.log`，其中 `NN` 为任务编号。

09 的独立补充验收：仅向测试进程注入假非空 key，默认 `cargo test --test live_e2e` 为 2 passed / 10 ignored；移除 key 后显式 `--ignored` 按预期退出 101，10 个用例均立即报告缺少凭据。这是负向验收通过，不是在线模型通过。本轮未重试真实模型，规划阶段的线上超时证据仍有效，原因未进一步归因。

执行中处理及记录了以下情况：

- 旧 CLI 模板测试会让 SessionStart 写入源码 liveplug。07 在其原有 `tests/cli_e2e.rs` 白名单内改为只复制 9 个版本化输入，09 负责 live 测试副本；二者组合的全量测试不再产生源码输出。07 合入前各校验新生成的单个已知文件按授权精确处理，记录见相关归档。用户原始备份 `/tmp/instagent-hook-out-20260906-9kg222l8/liveplug-hook-out` 保持原位，未删除或移回。
- 04 首次实现校验曾有两个 proxy 计数断言失败；其后的隔离 15/15、完整复跑和协调器集成态校验均通过。10 的前两次完整 CI 均在 `cancelled_restart_candidate_is_reaped` 初始启动计数断言失败（2 != 1）；隔离目标 15/15 通过，用户恢复执行后的第三次完整 CI 通过，随后由协调器独立验证。04 与 10 采样的具体用例不同，原因均未确定；通过不代表已修复根因，详见 04/10 归档及 `docs/release.md`。保留波动记录，不无证据归因。
- 协调器首次独立 CI 另在 `timeout_during_mcp_initialization_reaps_process_group` 等待 `startup.sh.pid` 时失败（CLI 52 passed / 1 failed）；保留该采样及后续复验结果，不将其直接归因为 proxy 波动。原始日志为 `10-validate-first-failed.log`。
- 10 隔离 CLI 53/53 通过后的完整复验又在 `cancelled_clone_kills_process_group` 缺失 `grandchild.pid` 失败（lib 526 passed / 1 failed），该用例隔离复验通过；完整日志与可能的测试同步窗口见 10 归档，尚不构成确认根因。协调器随后在 `48305f7` 独立完整 CI/MSRV 均通过，最终任务提交另经上表对应独立门槛验证。
- rebase 冲突仅涉及队列 README 的相邻任务状态，解决时保留已合入状态与当前任务更新；实现内容未因冲突改写。

无 blocked、deferred 或未完成执行项。RM01–RM10 保持原 roadmap 范围，不属于本轮执行队列。没有新增或升级依赖/feature，没有修改 `src/lib.rs` 模块树、旧 `todos/done/` 归档或版本化源码夹具；未 push、创建 PR 或部署。10 个任务 agent 均正常退出，本轮 Herdr workspace、worktree 和任务分支全部清理；原协调器及用户已有 workspace 保留。收尾只更新本计划的执行记录与队列交接状态，提交前再次执行根 AGENTS 要求的 fmt、clippy 和普通 cargo test。
