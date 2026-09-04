# ADR 0003: 跨模块运行时策略——密钥、环境、hook、CLI 流、settings 语义与 containment 责任边界

- 状态：已接受
- 日期：2026-09-04
- 依据：`plans/repo-improvements-2026-09-04/plan.md` §总体方案第 1 条（S6、S12、S13、S16、A5、A6）
- 关联：ADR 0001（Anthropic 原生引擎移除，engine 只剩 openai / proxy）、ADR 0002（sandbox agent、工具自动执行、无 approval/trust/mode UI）
- 后续实现任务：`plans/repo-improvements-2026-09-04/todos/` 02、03、04、06、08、10、11、12（映射见各决策末尾）

## 背景

ADR 0002 定了原则（安全边界在外部 sandbox、工具直接执行），但六个跨模块
问题仍无默认可执行的策略，阻塞仓库改进队列的全部实现任务：

1. config.yaml 暴露 `api_key_env` / `api_key`（`src/config.rs:20-22`），但
   引擎实际只读 provider manifest 的 `api_key_env`
   （`src/provider/shared.rs:124-129`），两个 config 字段是死字段（S6）。
2. hooks 已有 env 白名单（`src/hooks.rs:446-471`），但 command 工具
   （`src/tools/command.rs:166-169`）、MCP stdio server
   （`src/tools/mcp.rs:230-234`）继承父环境，可能把 `OPENAI_API_KEY`
   等凭据泄露给不可信插件（S12）。
3. hook 无决策时默认 `OnFailure::Allow`（fail-open，`src/hooks.rs:153-157`），
   且失败可见性不足（A5 / A4）。
4. CLI 把文本、工具事件、错误、usage 全部打 stdout（`src/cli/render.rs`），
   与 `docs/usage.md` 描述不符，脚本无稳定机器接口（A6）。
5. settings 合并按"空数组 = 未写"处理（`src/settings.rs:110-131`、
   `src/plugin/discovery.rs:121`），但文档声称"没写"和"写了空数组"语义
   不同（`docs/usage.md:207-211`）（S16）。
6. 文件工具只拒绝最终目标 symlink（`src/tools/builtin/fs.rs:106-114`），
   中间路径 symlink / 绝对路径可逃出 cwd；是否要在应用层做全路径
   containment 未决（S13 / RM5）。

本 ADR 逐项定死默认值、例外、错误行为与落地任务。本 ADR 不恢复
Anthropic 支持（ADR 0001），不引入任何交互式 approval / trust / mode UI
（ADR 0002）——下文所有"阻断 / Block"均是配置驱动的非交互决策。

## 决定

### D1 · API key 唯一来源：provider manifest 的 `api_key_env`；删除 config 死字段

- **默认**：密钥只有一个来源——provider JSON 的 `api_key_env` 指向的环境
  变量（openai / proxy 两引擎共用 `engine_parts`，语义不变）。
  `Config.api_key_env` 与 `Config.api_key` 两字段删除；config.yaml 不允许
  出现任何形式的密钥。
- **例外**：`api_key_env` 未声明的 provider（如本地 proxy 引擎）密钥为空串，
  维持现状。
- **错误行为**：manifest 声明了 `api_key_env` 而环境变量未设置 →
  provider 构造失败，错误带 provider 名与变量名（现行为，钉死为契约）；
  config.yaml 中出现 `api_key` / `api_key_env` 键 → 加载期显式报错并给出
  迁移提示（"删除该字段，把变量名写进 provider JSON 的 `api_key_env`"），
  不得静默忽略。密钥永不写 session JSONL、settings、日志；错误输出对
  `sk-…` 形态做 redact。
- **后续实现**：todo 08（字段删除、加载期报错、来源测试）；文档同步 todo 16。

### D2 · 插件子进程环境 baseline + manifest allowlist；内置 shell 保留父环境

- **默认**：凡执行插件代码的子进程（hooks、command 工具、MCP stdio
  server）统一走现有 hook baseline：`env_clear` 后仅带 `PATH` / `HOME` /
  `LANG`（存在才带）+ `PLUGIN_ROOT` + manifest
  `extensions["dev.instagent"].env` 声明的变量名（`src/hooks.rs:446-471`
  抽为共享 helper，三处复用）。MCP server manifest 的 `env` 键值对在其上
  显式覆盖（现行为保留）。
- **builtin `shell` 工具例外**：它是内核给模型操作 sandbox 用户 shell 的
  通道，其环境就是 sandbox 注入的环境，保留父进程完整环境、不 env_clear
  （现状不变）。这是 D2 唯一例外；新增插件执行路径不得援引此例外。
- **错误行为**：声明的变量在父环境不存在 → 不设置该变量、不报错（与现
  hooks 行为一致）；`${PLUGIN_ROOT}` 一律经 `PLUGIN_ROOT` 环境变量与 argv
  传递，不再字符串替换进 `sh -c` 代码（shell metacharacter 防御，配合
  S11）。
- **后续实现**：todo 06（command / hooks env 隔离与 argv 化）、todo 10
  （MCP server env baseline）。

### D3 · hook 失败默认 fail-open，但必须可见

- **默认**：`on_failure` 缺省值维持 `Allow`（fail-open），对可阻断事件
  （PreToolUse / Stop）与非可阻断事件一致。理由：hooks 是 sandbox 内的
  可选策略层而非安全边界（安全由 sandbox 兜底，ADR 0002）；坏 hook
  （超时、起不来、载荷序列化失败）不能瘫痪整个 agent。
- **例外**：hook 作者可按条显式 `on_failure: "block"`（字段已存在），
  把自己升级为 fail-closed——这是配置驱动，不是交互审批。
- **错误行为**：fail-open 不再静默。序列化失败、spawn 失败、超时、非零
  退出全部产出 warning，包含插件名、事件、hook 命令、阶段与原因；
  CLI 侧 session start/end hook 失败从 `let _ =`（`src/cli/handlers.rs`）
  改为输出 stderr warning 行，不改变退出码。
- **后续实现**：todo 06（策略钉死 + 诊断结构）、todo 11（A4 可见性）。

### D4 · CLI 机器契约：stdout 只放答案，其余全走 stderr

- **默认**：`instagent` CLI 中 stdout 只输出模型最终回答文本流
  （TextDelta）；工具事件（`▶` / `✓` / `✗` 行）、预览、usage、compaction
  提示、错误、诊断与 tracing 日志全部走 stderr。`>file` / 管道消费方
  因此拿到纯答案。
- **错误行为**：运行失败 → 非零退出码，stderr 末行为 `error: {message}`；
  渲染 / 关闭 stdout 失败（如 EPIPE）不改退出码。`--output json` 类
  结构化输出不在本轮，出现需求时另立决策。
- **例外**：无。
- **后续实现**：todo 12（render.rs 迁移 + 契约回归测试，现有接受混合
  输出的测试同步改）。

### D5 · `enabledPlugins` tri-state：缺失 ≠ 空数组

- **默认**：每层 settings 的 `enabledPlugins` 三态——**缺失** = 该层不表态
  （低层值延续）；**非空** = 白名单；**`[]`** = 显式空白名单 = 本作用域
  禁用全部插件。合并规则保持"被高层提到过的名字不被低层恢复"，仅把
  高层显式 `[]` 从"视同缺失"改为可表达的终值。`disabledPlugins` 维持
  并集语义，缺失与 `[]` 等价（黑名单没有"显式清空"场景）。
- **错误行为**：`enabledPlugins: []` 下若 `--provider` 指向插件 provider，
  照常报 provider 未找到并提示 settings 来源；不新增专门错误类型。
- **例外**：无。文档 `docs/usage.md:207-211` 的既有描述即本决策语义，
  以实现追平文档，不降级文档。
- **后续实现**：todo 03（merge 终值化 + 三层合并测试）。

### D6 · 路径 containment：sandbox 负责，应用层只做纵深防御、不承诺 containment

- **默认**：延续 ADR 0002——外部 sandbox 是唯一强制隔离层。instagent
  **不**实现全路径 containment（不逐级检查 parent symlink、不引入
  secure openat 依赖）。应用层保留以下纵深防御，并只承诺这些：
  session id 白名单校验（todo 02）、session / settings / 安装元数据的
  私有权限与原子写（todo 02 / 03）、文件工具对**最终目标** symlink 的
  拒绝（现状）、读写与遍历字节上限（todo 04）。
- **显式非保证**：`write linkdir/file`（cwd 内的中间 symlink）可写到 cwd
  之外、相对路径与绝对路径按当前语义解析——这是 sandbox 的责任面，
  工具文档必须如实说明，禁止"路径安全"式表述。
- **错误行为**：containment 不报错（因为不做）；上面承诺的校验失败须
  给出路径与原因。
- **例外**：若将来出现无 sandbox 本地模式或多租户需求，按 RM5 另立 ADR
  引入 containment，本决策届时被推翻或收窄。
- **后续实现**：todo 04（fs/tree/skills 边界 + 工具文档钉死当前承诺）。

## 兼容性与迁移影响

| 决策 | 变化 | 影响面 | 迁移 |
|---|---|---|---|
| D1 | config `api_key*` 由"静默忽略"变为"加载期报错" | 只在 config.yaml 写过这两键的用户（当前写了也无效） | 报错文案即迁移指引；密钥改放环境变量 |
| D2 | command 工具 / MCP server 不再继承父环境全量变量 | 依赖未声明变量的插件 | manifest `extensions["dev.instagent"].env` 声明所需变量名；诊断带变量名与插件名 |
| D3 | 无行为变化，失败从静默变可见 | 依赖静默 fail-open 的坏 hook 会开始刷 warning | 修 hook 或显式接受 warning |
| D4 | 工具事件 / 错误 / usage 从 stdout 迁到 stderr | 解析 stdout 的现有脚本（当前无契约，按混合输出写的测试需改） | `2>&1` 可还原旧行为 |
| D5 | `enabledPlugins: []` 从"等同没写（全启用）"变为"禁用全部" | 写过 `[]` 且期望全启用的配置 | 删除该键即可恢复；该组合旧语义本就不明，按 bug 修复处理 |
| D6 | 无行为变化；文档停止暗示 containment | 误以为应用层挡 symlink 逃逸的部署 | 明确 sandbox 为责任层；需要应用层保证时走 RM5 |

## 后果

- 队列 02–21 的实现任务可直接引用 D1–D6 作为验收依据，不再各自解释。
- instagent 的安全模型保持单层：sandbox 强制隔离，应用层只做密钥、
  环境、持久化与输出面的纵深防御；不存在进程内审批路径。
- 各决策的实现若与本 ADR 冲突，以实现为准需要先修订本 ADR，不允许
  静默偏离。
