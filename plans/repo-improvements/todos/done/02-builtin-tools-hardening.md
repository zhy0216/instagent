difficulty: medium

内置工具健壮性加固：原子写、符号链接防护、流式读大文件、临时文件权限。只改 `src/tools/builtin/` 下三个文件，不改工具对外接口与返回格式（除新增错误文案）。

## T1 · write/edit 原子替换

- `src/tools/builtin/fs.rs` 的 `write_file`（约 70-90 行）与 `edit_file`（约 93-105 行）目前直接 `std::fs::write` 覆盖，中断会留截断文件。改为：写同目录临时文件（随机后缀，`uuid` 已是依赖）→ `fs::rename` 原子替换；失败路径清理临时文件。临时文件放目标同目录（跨设备 rename 会失败，不要放系统临时目录）。
- 预计修改文件：`src/tools/builtin/fs.rs`。
- 验收：新增测试覆盖——写成功后无临时文件残留；目标只读目录等失败路径下原文件内容不变；原有 10 个 fs 测试保持绿。
- 前置依赖：无。

## T2 · 写路径符号链接防护

- `write_file`/`edit_file` 写前用 `fs::symlink_metadata` 检查目标：若目标是符号链接，返回 `ToolOutput::err`（可读文案，说明拒绝写穿符号链接），不跟随。对不存在的路径（新建）不受影响。
- 预计修改文件：`src/tools/builtin/fs.rs`。
- 验收：新增测试：`std::os::unix::fs::symlink` 建 `link -> 真实文件`，`write_file`/`edit_file` 对 `link` 返回 err 且真实文件内容不变。
- 前置依赖：与 T1 同文件，一起做。

## T3 · read 流式读取 + 大小上限

- `read_file`（`fs.rs:37` 附近）目前 `read_to_string` 全量读入后按 `limit` 截取。改为 `BufReader::lines()` 流式读取，读到 `start + limit` 行即停；同时用 `fs::metadata().len()` 预检，超过字节上限（新增常量，如 `MAX_READ_BYTES = 32 * 1024 * 1024`）直接返回可读错误，提示文件过大。保留现有行窗口与 "(N more lines)" 输出格式。
- 预计修改文件：`src/tools/builtin/fs.rs`。
- 验收：新增测试——构造超上限文件（可用稀疏文件或临时调常量：常量设计成可在测试中用小值构造，或直接写 33MB 临时文件，取实现方便者）返回错误；大文件只读前窗口行时耗时与内存不全量加载（功能测试即可，不做基准）；原有测试保持绿。
- 前置依赖：与 T1 同文件，一起做。

## T4 · 显式拒绝非法窗口参数

- `read_file` 的 `line=0` 目前被 `saturating_sub` 静默当第 1 行，`limit=0` 返回空窗口。改为两者都返回 `ToolOutput::err` 并给出可读提示（line 从 1 计、limit 需为正）。
- 预计修改文件：`src/tools/builtin/fs.rs`。
- 验收：新增测试覆盖 `line=0` 与 `limit=0`；原有测试若依赖旧行为需同步修正。
- 前置依赖：与 T1 同文件，一起做。

## T5 · tree 行数统计流式化

- `src/tools/builtin/tree.rs:154-159` `count_file_lines` 对每个文件全量 `read_to_string` 只为数行数。改为 `BufReader` 按字节块流式统计 `\n`；并设单文件读取字节上限（如 10MB），超限即停并显示占位（保持现有输出格式兼容，超限可显示 `?` 或截断说明，以现有调用处的展示格式为准）。
- 预计修改文件：`src/tools/builtin/tree.rs`。
- 验收：原有 4 个 tree 测试保持绿；新增测试——超限大文件不报错、行数显示降级。
- 前置依赖：无。

## T6 · shell 全量输出收紧权限

- `src/tools/builtin/shell.rs:249-255` `save_full_output` 把可能含敏感信息的输出写到共享临时目录，默认 0644。改为：目录创建后 `set_permissions` 0o700；文件写后 0o600。
- 预计修改文件：`src/tools/builtin/shell.rs`。
- 验收：新增测试断言目录/文件权限位；原有 11 个 shell 测试保持绿。
- 前置依赖：无。

## 本文件整体验证

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo clippy --all-targets --features anthropic-engine -- -D warnings
cargo test --features anthropic-engine
```
