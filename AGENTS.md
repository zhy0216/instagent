# AGENTS.md

instagent：插件为核心的最小 agent，Rust 从零实现。

- 设计主依据：`docs/goose-plugin-core-plan.md`（第三版：插件模型、provider、工具来源、内核边界）
- 设计补充：`docs/goose-from-scratch-plan.md`（第二版 §2.2 消息、§2.5 loop、§2.6 会话、§2.7 压缩、§2.11 CLI 被第三版沿用）
- 任务队列：`todos/`（顺序与依赖见 `todos/README.md`）
- goose 参考基线：`~/yyds/goose`（block/goose commit `4ad43df`，只读参考）。本地若不存在：
  `git clone https://github.com/block/goose ~/yyds/goose && git -C ~/yyds/goose checkout 4ad43df`

## 校验命令（每个 commit 必须全部通过）

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## 命名对照（计划文档 → 本仓库）

| 计划文档里 | 本仓库 |
|---|---|
| `mini-agent` | `instagent` |
| `dev.miniagent` | `dev.instagent` |
| `MINI_AGENT_*` 环境变量 | `INSTAGENT_*` |
| `~/.config/mini-agent/` | `~/.config/instagent/` |
| `~/.local/share/mini-agent/` | `~/.local/share/instagent/` |

## 约定

- `Cargo.toml` 直接依赖与 cargo feature 由 `todos/01` 锁定，后续任务不加依赖、不升级依赖。
- `src/lib.rs` 模块树由 `todos/01` 锁定，后续任务只填充自己负责的模块，不改模块声明、不动别人的模块。
- 只改当前 todo"涉及文件"里列出的东西，不顺手重构。
- 写代码同时写测试；子进程一律进程组 + `kill_on_drop(true)`。
- goose 代码只读参考，成段搬运时在 commit message 注明出处。
- 不要动 `todos/done/` 下已归档的文件。
