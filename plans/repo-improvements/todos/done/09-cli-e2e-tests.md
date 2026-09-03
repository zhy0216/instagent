difficulty: hard

CLI 二进制级集成测试收尾：目前 `src/cli/handlers.rs` 的 plugin 子命令（~130 行分支逻辑）与 `run -t` 全链路零集成测试。用 `CARGO_BIN_EXE_instagent` 起真进程，沙箱三变量隔离，覆盖 README 手工验证清单里可自动化的第 4/5/7/8 条。

## T1 · 测试骨架与沙箱助手

- 新增 `tests/cli_e2e.rs`。沙箱助手：每个测试一个 `tempfile::tempdir`，设置 `INSTAGENT_CONFIG_DIR` / `INSTAGENT_DATA_DIR` / `INSTAGENT_AGENTS_DIR` 三变量（代码就是为此设计的），经 `env!("CARGO_BIN_EXE_instagent")` 起真进程，捕获 stdout/stderr/退出码。
- 参考 `src/cli/assembly.rs:410` 已有的"假 openai provider（wiremock SSE）+ handlers::run"进程内做法，把同样的 wiremock SSE 应答搬进集成测试。`wiremock` 已是 dev-dependency，不新增依赖。
- 预计修改文件：`tests/cli_e2e.rs`（新增）。
- 验收：骨架编译通过，至少一个最简 `--help`/`--version` 真进程用例跑绿。
- 前置依赖：无（但整体任务建议在 02–06 合并后执行，见文件头依赖）。

## T2 · `run -t` 全链路

- 覆盖 `instagent run -t "..."`：clap 解析 → 装配 → SSE → stdout 最终回复 + `usage:` 行 + 退出码。用 T1 的假 provider 打一条最简回复，断言最终文本与 `usage:` 出现、退出码 0。
- 预计修改文件：`tests/cli_e2e.rs`。
- 验收：该用例在真二进制上跑绿，不依赖任何真实 API key。
- 前置依赖：依赖 09 文件内 T1。

## T3 · sessions 子命令

- 覆盖 `instagent sessions list` 与 `sessions rm <id>`：先经 `run -t` 产生会话（写入沙箱 `<data>/sessions/`），再 `sessions list` 断言该会话出现，`sessions rm <id>` 后断言文件被删。替代 README 手工清单第 4 条可自动化部分。
- 预计修改文件：`tests/cli_e2e.rs`。
- 验收：list/rm 用例跑绿；不触碰测试外真实数据目录。
- 前置依赖：依赖 09 文件内 T2（需要会话先被产生）。

## T4 · plugin 子命令 + 信任确认

- 覆盖 `plugin install <本地路径> --yes` / `list` / `show` / `disable` / `enable`。本地插件夹具在 `src/cli/mod.rs:203-231` 已有形状，平移到 `tests/fixtures/`（新增纯文件夹具，不改依赖）。交互确认用 `--yes` 跳过，或管道喂 `y\n`（trust 确认读 `BufRead` stdin，非 rustyline）。
- 断言：install 后 `list` 出现且启用；`show` 输出组件；`disable`/`enable` 状态翻转；`enable` 触发信任确认并写 `trustedPlugins`。替代 README 手工清单第 7 条可自动化部分。
- 预计修改文件：`tests/cli_e2e.rs`、`tests/fixtures/`（新增插件夹具文件）。
- 验收：五个子命令用例跑绿；`--yes` 与管道确认两条路径至少各覆盖一次。
- 前置依赖：依赖 09 文件内 T1。

## T5 · `--plugin PATH` 临时加载

- 覆盖 `run --plugin <开发插件目录>`：该目录内联定义一个 provider，`run` 直接用该 provider 跑通（复用 T1 假 SSE）。对应 README 手工清单第 8 条。
- 预计修改文件：`tests/cli_e2e.rs`。
- 验收：`--plugin` 加载的 provider 可被 `run -t` 使用并跑绿。
- 前置依赖：依赖 09 文件内 T1、T2。

## 本文件整体验证

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo clippy --all-targets --features anthropic-engine -- -D warnings
cargo test --features anthropic-engine
```

依赖说明：本任务测的是 02–06 重构后的最终形态，务必在 02（工具加固）、03（会话）、04（subprocess）、05（agent）、06（provider）合并后再做，避免针对旧形态返工。
