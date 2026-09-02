# 00 · 骨架：Cargo 工程 + 完整模块树空壳

优先级：P0 · 依赖：无 · **串行第一个，合并前不得启动其他任务。**

目标：建好可编译的 Cargo 工程和**完整模块树**（每个模块都是类型完整但逻辑为空
的空壳），并一次性锁定全部依赖。之后每个 todo 只填充自己名下的文件。

涉及文件：`Cargo.toml`、`rust-toolchain.toml`、`.gitignore`（已存在）、`src/main.rs`、
`src/lib.rs`、`src/error.rs`，以及下列所有模块的空壳文件。

验收：`cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` 全绿；
`cargo run -- --help` 能打印帮助。

计划参考：第三版 §1（内核边界）、§4（模块表）；第二版 §2.1、§3（依赖清单）。

## A1 · Cargo 工程与依赖锁定 {#a1}

- package 名 `instagent`，edition 2021，bin + lib（`src/main.rs` + `src/lib.rs`）。
- 一次性声明全部直接依赖（后续任务不加依赖），按第二版 §3 + 第三版 §4：
  `tokio`(full)、`tokio-util`、`futures`、`async-trait`、`reqwest`(default-features=false,
  rustls-tls, json, stream)、`serde`(derive)、`serde_json`、`serde_yaml`、`rmcp`(client,
  transport-child-process, transport-streamable-http-client；版本以 `~/yyds/goose` 根
  `Cargo.toml` 的 workspace 版本为准，本地没有就先 clone 到 `~/yyds/goose` 再查)、
  `clap`(derive)、`rustyline`、`anyhow`、`thiserror`、`tracing`、`tracing-subscriber`、
  `tracing-appender`、`etcetera`、`ignore`、`chrono`、`uuid`(v4)、`shellexpand`、
  `include_dir`、`regex`。dev-dependencies：`wiremock`、`tempfile`。
- 预声明 cargo feature `anthropic-engine`（默认关闭，`12` 用）。
- `rust-toolchain.toml` 固定 stable。

## A2 · 错误类型 {#a2}

- `src/error.rs`：`anyhow::Result` 到顶层；`ProviderError` 单独枚举
  （`RateLimited { retry_after: Option<Duration> }`、`ContextOverflow`、`Auth`、
  `Http(u16, String)`、`Transport(String)`），第二版 §2.3。

## A3 · 模块树空壳 {#a3}

`src/lib.rs` 声明下列模块，每个给一个能通过编译、带类型签名的空壳
（`todo!()` / `unimplemented!()` 允许出现在函数体里，但不允许出现在类型定义处）。
空壳里用 `// TODO(<todo 编号>)` 注明由哪个任务填充：

```
config.rs  settings.rs  message.rs  session.rs  subprocess.rs
plugin/{mod.rs, manifest.rs, discovery.rs, install.rs, mcp_config.rs, bundled.rs}
provider/{mod.rs, registry.rs, http.rs, openai.rs, proxy.rs, anthropic.rs}
tools/{mod.rs, builtin/{mod.rs,shell.rs,fs.rs,tree.rs}, mcp.rs, command.rs, skills.rs}
hooks.rs  commands.rs
agent/{mod.rs, approval.rs, compact.rs, prompt.rs, event.rs}
```

- 各模块的**公共类型**（第二版 §2.2/§2.3/§2.4/§2.5、第三版 §2.5 的结构体、枚举、trait 签名）
  在空壳阶段就定义完整，后续任务只填实现，不改类型布局。这样并行任务之间类型一致。
- `anthropic.rs` 整个文件放在 `#[cfg(feature = "anthropic-engine")]` 后面。

## A4 · CLI 入口空壳 {#a4}

- `src/main.rs`：clap 骨架，四个子命令 `chat` / `run` / `sessions` / `plugin`，
  处理体 `todo!()`，由 `18` 填充。

## A5 · CI 与最小文档占位 {#a5}

- `.github/workflows/ci.yml`：fmt → clippy -D warnings → test。
- `README.md` 写一句话占位，`19` 补全。

## A6 · 提交 {#a6}

- 单个 commit，message 形如 `feat(skeleton): cargo project + full module tree stubs`。
