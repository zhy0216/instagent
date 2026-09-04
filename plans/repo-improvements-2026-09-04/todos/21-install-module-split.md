difficulty: medium

## T1 · 拆分 plugin install 流程

- 要做什么：在 `07` 的安装/更新回归稳定后，将 `src/plugin/install.rs` 按 source acquisition、manifest/metadata persistence、staging/copy、atomic replacement、auto-update 整理为清晰的私有实现单元；保留原子私有写入、进程组/超时/取消、symlink 契约和可恢复 `.replaced-*` 状态机，不修改 `src/lib.rs` 或既有模块声明。
- 预计修改文件：`src/plugin/install.rs`、对应安装测试（不新增需要改模块树的顶层子模块）。
- 验收条件：local/git install、update、失败回滚、孤儿清理、权限和 timeout/process-group 测试全部通过；拆分不改变 CLI 输出和 PluginSource 语义；全仓基线、rustdoc 和 release check 通过。
- 验证方式：运行 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、plugin/CLI e2e 测试、`cargo rustdoc --lib -- -D warnings` 和 release check。
- 前置依赖：`07-plugin-install-resilience.md`、`12-cli-stream-and-e2e.md`、`16-docs-and-rustdoc.md`。
