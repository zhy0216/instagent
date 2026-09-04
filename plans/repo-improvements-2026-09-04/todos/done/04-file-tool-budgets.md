difficulty: hard

## T1 · fs/tree/skills/image 的资源与路径边界

- 要做什么：把同步文件系统、目录遍历和图片/skill 读取移出 current-thread async 路径（使用受控 `spawn_blocking` 或等价方案），传递 cancellation token 并在长循环中检查；为 tree 增加 max entries、总字节、输出、深度和时间预算，达到上限返回结构化 truncation note。
- 要做什么：为 read/edit/write、skill body/supporting file、image payload 设置单文件/总量上限，处理 metadata/read 增长竞态；skill description 必须 non-empty；supporting file 明确 no-follow/containment 行为。按 `01` 的 sandbox 决策实现或记录安全边界；短期 image 至少验证 MIME、base64、长度和可接受的 payload 预算，不引入未批准 decoder 依赖。图片跨文件的单请求/单会话总预算由后续 agent/provider 任务落实，本任务提供工具层可观测的大小信息。
- 预计修改文件：`src/tools/builtin/fs.rs`、`src/tools/builtin/tree.rs`、`src/tools/builtin/image.rs`、`src/tools/skills.rs`。
- 验收条件：大目录/大文件/深度 0 不再无界增长；取消在可测上界内生效；路径含 symlink 时行为符合 ADR；超限和坏图片错误可诊断且不 OOM；新增 size/depth/time/cancel/symlink 测试通过。
- 验证方式：运行 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、tools builtin/skills 测试和 `cargo test`；对 fake 大目录/大文件记录取消耗时。
- 前置依赖：`01-policy-decisions.md`。
