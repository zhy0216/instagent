difficulty: medium
agent: inherit
status: done

# 03 · 文件权限与实际 IO 预算

对应方案 F01–F03。前置依赖：无。

## 涉及文件

- `src/tools/builtin/fs.rs`（含模块内测试）

## T1 · 原子覆盖保留普通权限

工作：改进 atomic_write 的临时文件创建和发布。排他创建同目录临时文件，从创建起保持私有；write/edit 覆盖已有普通文件时保留普通 rwx 权限，正常文件新建遵循现行权限约定。完成写入后设置待发布的最终普通权限，禁止自动继承 setuid/setgid。失败清理临时文件，保留现有 symlink 拒绝行为。

预计修改文件：`src/tools/builtin/fs.rs`。

验收：
- [x] Unix 下 write/edit 覆盖 0755 文件仍为 0755，覆盖 0600 文件仍为 0600；新文件权限符合约定。
- [x] 无可预测临时文件覆盖；写/rename/权限处理失败不宣称成功，失败路径无遗留临时文件。
- [x] 不宣称保留 ACL、扩展属性、属主、硬链接语义；不新增应用层全路径 containment。

前置依赖：无。

## T2 · edit 限制替换后的字节数

工作：在分配/提交替换内容前受检计算结果长度，使用 MAX_WRITE_BYTES；实际结果越界返回工具错误，避免成功写出后续 read/edit 拒绝的文件。保留唯一匹配、空 before 拒绝和无匹配诊断。

预计修改文件：`src/tools/builtin/fs.rs`。

验收：
- [x] 刚好上限成功，上限+1 失败；覆盖 UTF-8 字节数、变长替换、删除、无匹配和多匹配。
- [x] 失败后原文件字节及权限不变。
- [x] 复现案例：33,554,430 字节文件中 old→larger 不应写成 33,554,433 字节。

前置依赖：无。

## T3 · read 的长行和增长文件有实际读取上限

工作：read_file_sync 不得先通过无界 BufRead::lines 分配整条增长长行，再检查字节数；在 reader 层施加 MAX_READ_BYTES 的硬边界并正确区分末行/缺换行、增长与非法 UTF-8。保持现有行号、limit、取消和超限提示契约。

预计修改文件：`src/tools/builtin/fs.rs`。

验收：
- [x] 确定性小预算 fixture 验证超长无换行行、预检后增长、预算边界和跨 UTF-8 字符；读取不会越过允许探测的 limit+1 字节。
- [x] 多行窗口、空文件、末行无换行、越界行号、预取消用例通过。
- [x] 不用无限文件/FIFO或依赖 sleep 竞速作为唯一测试，不扩大为特殊文件策略改造。

前置依赖：无。

## 校验

每个 commit 执行：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

09 合入前，给执行测试的进程移除 `TOKEN_PLAN_API_KEY`（`env -u TOKEN_PLAN_API_KEY cargo test`），记录 live 用例门控返回；09 合入后默认 cargo test 应显示 live ignored，不因继承凭据而联网。不得修改全局环境、真实凭据或共享源码夹具。实现与行为回归同一 commit；不改依赖/feature、`src/lib.rs` 模块树、其他任务文件或任何既有 done 归档。

## 完成记录（2026-09-06）

- T1：同目录随机临时文件以 `create_new(true)` / Unix 0600 创建，完整写入后设置普通 rwx 权限再 rename。新文件用私有目录内的空探针获取正常创建权限，不修改进程 umask。权限矩阵覆盖 write/edit 的 0755、0600、6755、7755；新文件与普通创建一致，另在进程级 umask 077 下通过。排他创建碰撞、symlink、只读目录、模拟写入/权限错误及实际 rename 错误均验证原内容不变和临时文件清理。
- T2：唯一匹配后、分配结果前使用 checked_sub/checked_add 校验 MAX_WRITE_BYTES。小预算测试覆盖 UTF-8、增长、缩短、删除、错误诊断和整数溢出。真实 33,554,430 字节 fixture 的 old→larger 与 old→界界 均拒绝，原字节和 0600 不变；old→large 正好写入 33,554,432 字节并可再次 edit。
- T3：`BufReader` 内侧使用 `take(limit + 1)`，按实际字节读取后再解析 UTF-8；保留行窗口和增长提示，预算截断字符不误报非法 UTF-8，完整文件的非法 UTF-8 仍报错。确定性长行及预检后追加 fixture 在 8 字节预算下均只读取 9 字节；LF/CRLF、空行/空文件、末行无换行、字符分块、越界行号和预取消通过。

校验：

- `env -u TOKEN_PLAN_API_KEY cargo test tools::builtin::fs::tests --lib`：37 passed。
- `cargo fmt --check`：通过。
- `cargo clippy --all-targets -- -D warnings`：通过。
- `env -u TOKEN_PLAN_API_KEY cargo test`：通过；600 个离线 Rust 测试（lib 477、bin 10、agent_continuation 21、cli_e2e 49、mcp_e2e 15、provider_proxy 15、session_recovery 9、tool_inventory 4），doc-test 0。另有 10 个 live 测试函数因 key 缺失门控返回，测试框架仍计为 passed；未验证在线行为。
- `umask 077 && env -u TOKEN_PLAN_API_KEY cargo test --lib tools::builtin::fs::tests::new_file_permissions_match_normal_creation -- --exact`：1 passed；umask 仅作用于该 shell/test 进程。

交接注意：现有离线 `tests/cli_e2e.rs::plugin_task_template_expands_arguments_and_unknown_template_fails` 仍直接加载源码 liveplug，全量测试生成本 worktree 的 `.hook-out/session_start.json`。已仅清理本次生成的文件及空目录；未触碰用户备份或修改白名单外测试。该离线夹具问题需要其所属任务/协调器处理，不能只依赖 live 测试隔离。普通权限保证不包含 ACL、扩展属性、属主或硬链接语义，未扩展 containment 或特殊文件策略。
