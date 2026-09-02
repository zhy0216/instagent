difficulty: medium

会话文件（JSONL）健壮性：原子重写崩溃窗口、损坏恢复、半死字段清理。

## T1 · rewrite 消除崩溃窗口 + 临时文件随机后缀

- `src/session.rs:201-232` `rewrite` 当前流程：写固定名 `.jsonl.tmp` → `rename(主文件 → .bak)` → `rename(tmp → 主文件)`。两次 rename 之间崩溃会导致主文件消失（只剩 `.bak`），`resume` 直接失败。
- 改为：写**随机后缀**临时文件（`uuid` 已是依赖，消除 `:204` 固定名并发互踩）→ `sync_all` → 把当前主文件**复制**（`fs::copy`，best-effort，失败只警告不致命）为原有命名规则的 `.bak` → `rename(tmp → 主文件)`。不变量：任何时刻主文件要么是旧内容要么是新内容，绝不缺失。
- 保留 `backup_path`（`session.rs:262-276`）现有命名规则与测试。
- 预计修改文件：`src/session.rs`。
- 验收：新增测试——模拟"复制 bak 之后、rename 之前失败"（可通过只读目录/注入方式，或至少单测各步骤顺序）不丢主文件；rewrite 后 `.bak` 内容与旧主文件一致；原有 6 个 session 测试及 `agent/mod.rs` 中依赖 rewrite/.bak 的压缩测试保持绿。
- 前置依赖：无。

## T2 · resume 坏行 salvage

- `src/session.rs:100-130` `resume` 目前任一行 JSON 解析失败即整体报错。改为：逐行解析，遇到坏行时截断到最后一条合法行、向用户输出可读警告（含丢弃行数），用合法前缀继续打开。
- 截断后仍需满足 `message::validate`：若前缀不满足不变量（如尾部 tool_use 无结果），继续回退到最近的合法前缀（利用 `message::validate` 现有逻辑，不改 validate 语义）；回退到只剩 header 时按空会话打开并警告；一行都不合法才报错。
- `parse_line`（`session.rs:301-307`）保留原有带行号错误信息，供完全无法恢复时使用。
- 预计修改文件：`src/session.rs`。
- 验收：新增测试——尾部追加一条坏行（手写非 JSON）后 resume 成功且消息数正确、警告可见；尾部是不完整 tool_use 对时回退到合法前缀；全坏文件仍报错；原有测试保持绿。
- 前置依赖：无。

## T3 · 删除半死字段 `SessionHeader.name` 与过期注释

- `src/session.rs:39` `SessionHeader.name` 永远被写为 `None`（`:85` 唯一构造点），但 `src/cli/handlers.rs:186` `sessions list` 渲染它（永远显示 `-`）。删除该字段及对应渲染列（或改为不显示该列），同步修改断言它的测试（如 `handlers.rs` 的 `sessions_list_rows_and_rm`）。serde 默认忽略未知字段，旧会话文件含 `"name": null` 仍可正常反序列化，加一条测试验证。
- 顺带删除 `src/session.rs:132` 过期注释 `TODO(18) 接线`（实际已在 `cli/handlers.rs:58` 接线）。
- 预计修改文件：`src/session.rs`、`src/cli/handlers.rs`。
- 验收：`sessions list` 输出不再含恒为 `-` 的列；旧格式（含 `name` 字段）文件可打开；`cargo test` 全绿。
- 前置依赖：无。

## 本文件整体验证

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo clippy --all-targets --features anthropic-engine -- -D warnings
cargo test --features anthropic-engine
```
