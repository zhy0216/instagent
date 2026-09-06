difficulty: medium
agent: inherit

# 03 · 文件权限与实际 IO 预算

对应方案 F01–F03。前置依赖：无。

## 涉及文件

- `src/tools/builtin/fs.rs`（含模块内测试）

## T1 · 原子覆盖保留普通权限

工作：改进 atomic_write 的临时文件创建和发布。排他创建同目录临时文件，从创建起保持私有；write/edit 覆盖已有普通文件时保留普通 rwx 权限，正常文件新建遵循现行权限约定。完成写入后设置待发布的最终普通权限，禁止自动继承 setuid/setgid。失败清理临时文件，保留现有 symlink 拒绝行为。

预计修改文件：`src/tools/builtin/fs.rs`。

验收：
- Unix 下 write/edit 覆盖 0755 文件仍为 0755，覆盖 0600 文件仍为 0600；新文件权限符合约定。
- 无可预测临时文件覆盖；写/rename/权限处理失败不宣称成功，失败路径无遗留临时文件。
- 不宣称保留 ACL、扩展属性、属主、硬链接语义；不新增应用层全路径 containment。

前置依赖：无。

## T2 · edit 限制替换后的字节数

工作：在分配/提交替换内容前受检计算结果长度，使用 MAX_WRITE_BYTES；实际结果越界返回工具错误，避免成功写出后续 read/edit 拒绝的文件。保留唯一匹配、空 before 拒绝和无匹配诊断。

预计修改文件：`src/tools/builtin/fs.rs`。

验收：
- 刚好上限成功，上限+1 失败；覆盖 UTF-8 字节数、变长替换、删除、无匹配和多匹配。
- 失败后原文件字节及权限不变。
- 复现案例：33,554,430 字节文件中 old→larger 不应写成 33,554,433 字节。

前置依赖：无。

## T3 · read 的长行和增长文件有实际读取上限

工作：read_file_sync 不得先通过无界 BufRead::lines 分配整条增长长行，再检查字节数；在 reader 层施加 MAX_READ_BYTES 的硬边界并正确区分末行/缺换行、增长与非法 UTF-8。保持现有行号、limit、取消和超限提示契约。

预计修改文件：`src/tools/builtin/fs.rs`。

验收：
- 确定性小预算 fixture 验证超长无换行行、预检后增长、预算边界和跨 UTF-8 字符；读取不会越过允许探测的 limit+1 字节。
- 多行窗口、空文件、末行无换行、越界行号、预取消用例通过。
- 不用无限文件/FIFO或依赖 sleep 竞速作为唯一测试，不扩大为特殊文件策略改造。

前置依赖：无。

## 校验

每个 commit 执行：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

09 合入前，给执行测试的进程移除 `TOKEN_PLAN_API_KEY`（`env -u TOKEN_PLAN_API_KEY cargo test`），记录 live 用例门控返回；09 合入后默认 cargo test 应显示 live ignored，不因继承凭据而联网。不得修改全局环境、真实凭据或共享源码夹具。实现与行为回归同一 commit；不改依赖/feature、`src/lib.rs` 模块树、其他任务文件或任何既有 done 归档。

