difficulty: medium

## T1 · 加固 session id、恢复和持久化边界

- 要做什么：在 `src/session.rs` 集中实现并复用 `validate_session_id`，拒绝绝对路径、parent component、分隔符和非安全文件名；让 create/resume/open-or-resume/remove/list 全部走校验。坏 header/坏文件不应让 `list` 整体失败，`last` 只能从可验证会话中选择并在空结果时返回诊断。salvage 后把有效前缀原子重写回主 JSONL，保留有界、可识别的时间戳备份，保证 resume→append→resume 不再重复丢尾部。
- 要做什么：在同一模块实现 Unix 0700 sessions 目录、0600 session/tmp/backup 文件，定义并实现 append/rewrite 的最低 durability（flush，必要时 sync）；rename 失败清理临时文件，必要时同步父目录。补充 traversal、绝对路径、坏 header、坏尾部、备份、权限和 rename-failure 测试。
- 预计修改文件：`src/session.rs`。
- 验收条件：任意 CLI session id 都不能读/删 sessions 根目录外文件；坏尾部被物理修复且下一次启动可读新增消息；单个坏会话只产生带路径的诊断而不隐藏其它有效会话；Linux/macOS 权限测试按平台条件稳定通过；主校验命令全部通过。
- 验证方式：运行 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test session`（或等价过滤测试）及 `cargo test`。
- 前置依赖：`01-policy-decisions.md`。
