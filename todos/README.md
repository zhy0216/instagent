# instagent TODO 队列

用 Rust 从零实现一个**插件为核心的最小 agent**。任务队列按 `todos/README.md` 的顺序推进，
每个 todo 文件 = 一个独立任务 = 一个任务分支 = 最终一个 commit。

## 设计依据

- **主依据**：`docs/goose-plugin-core-plan.md`（第三版：插件模型、provider、工具来源、内核边界）
- **补充**：`docs/goose-from-scratch-plan.md`（第二版 §2.2 消息、§2.5 loop、§2.6 会话、§2.7 压缩、§2.11 CLI 被第三版沿用）
- **参考基线**：`~/yyds/goose`（block/goose commit `4ad43df`，只读）。本地不存在时先
  `git clone https://github.com/block/goose ~/yyds/goose && git -C ~/yyds/goose checkout 4ad43df`。
  只作参照与文本/代码搬运来源，不改它。

## 命名约定（计划文档 → 本仓库）

| 计划文档里 | 本仓库 |
|---|---|
| `mini-agent` | `instagent` |
| `dev.miniagent` | `dev.instagent` |
| `MINI_AGENT_*` 环境变量 | `INSTAGENT_*` |
| `~/.config/mini-agent/` | `~/.config/instagent/` |
| `~/.local/share/mini-agent/` | `~/.local/share/instagent/` |

## 仓库级校验命令

每个任务分支在 rebase/合并前必须全部通过（`AGENTS.md` 同步）：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## 并行策略（给协调器）

Rust 整仓一起编译，所以**第一个任务 `00` 必须串行先做**：它建好 Cargo 工程和完整模块树
（所有模块都是可编译的空壳），之后每个 todo 只填充自己名下的文件，互不重叠。

- `00` 完成合并前，不要启动任何其他任务。
- `00` 合并后，下面"依赖"只含已完成任务的文件可以并行，最多同时 5 个。
- 两个任务"涉及文件"有交集时不得并行（交集主要出现在 `provider/registry.rs`、`agent/mod.rs`、
  `src/main.rs`、`Cargo.toml`——这些都由单一任务独占，见各文件的"涉及文件"）。
- `Cargo.toml` 的依赖清单在 `00` 里一次性声明完整（后续任务不加依赖），避免并行改 `Cargo.toml`。
- 测试也要写在各自任务里；集成测试若跨多个模块，放在**较晚完成**的那个任务里。

## 优先级

| 优先级 | 文件 | 说明 |
|---|---|---|
| P0 | done/00-skeleton.md ✅ | Cargo 工程 + 完整模块树空壳 + 依赖锁定 |
| P1 | 01-config-settings.md, 02-message-session.md, 03-subprocess.md | 内核基础件 |
| P1 | 04-plugin-manifest.md, 05-plugin-discovery.md, 06-plugin-mcp-config.md, 07-plugin-install-bundled.md | 插件加载 |
| P1 | 08-provider-core-http.md, 09-provider-openai.md | provider 主路径 |
| P2 | 10-provider-registry.md, 11-provider-proxy.md | provider 装配 |
| P2 | 12-provider-anthropic.md | 可选 feature，可整段延后 |
| P2 | 13-tools-core-builtin.md, 14-tools-mcp.md, 15-tools-command-skills.md | 工具层 |
| P3 | 16-agent-loop.md, 17-hooks-commands.md | loop 与扩展点 |
| P4 | 18-cli.md, 19-hardening.md | 收口 |

## 文件

按数字前缀升序；"依赖"指必须先合并的任务。

1. [done/00-skeleton.md](done/00-skeleton.md) — ✅ 完成。依赖：无。串行第一个。
2. [done/01-config-settings.md](done/01-config-settings.md) — ✅ 完成。依赖：00
3. [02-message-session.md](02-message-session.md) — 依赖：00
4. [03-subprocess.md](03-subprocess.md) — 依赖：00
5. [04-plugin-manifest.md](04-plugin-manifest.md) — 依赖：00
6. [05-plugin-discovery.md](05-plugin-discovery.md) — 依赖：00、04、01
7. [06-plugin-mcp-config.md](06-plugin-mcp-config.md) — 依赖：00、04
8. [07-plugin-install-bundled.md](07-plugin-install-bundled.md) — 依赖：00、04、05
9. [08-provider-core-http.md](08-provider-core-http.md) — 依赖：00
10. [09-provider-openai.md](09-provider-openai.md) — 依赖：00、08
11. [10-provider-registry.md](10-provider-registry.md) — 依赖：00、08、09、07
12. [11-provider-proxy.md](11-provider-proxy.md) — 依赖：00、08、03、10
13. [12-provider-anthropic.md](12-provider-anthropic.md) — 依赖：00、08、10（可选）
14. [13-tools-core-builtin.md](13-tools-core-builtin.md) — 依赖：00、03
15. [14-tools-mcp.md](14-tools-mcp.md) — 依赖：00、03、13、06
16. [15-tools-command-skills.md](15-tools-command-skills.md) — 依赖：00、13、05
17. [16-agent-loop.md](16-agent-loop.md) — 依赖：00、02、08、13、01
18. [17-hooks-commands.md](17-hooks-commands.md) — 依赖：00、03、05、16
19. [18-cli.md](18-cli.md) — 依赖：00、16、10、11、14、15、17、01
20. [19-hardening.md](19-hardening.md) — 依赖：全部
