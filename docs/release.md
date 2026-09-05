# 发布与校验政策（toolchain / MSRV / 安全扫描 / CI 门槛）

- 日期：2026-09-05（`plans/repo-improvements-2026-09-04` todos/17，plan E1/E2/E9/A8）
- owner：仓库维护者 @zhy0216（下列所有豁免与政策变更由其批准）

## 发布目标与包元数据

当前发布目标 = 可复现的源码构建与本地安装（`cargo build --release` /
`cargo install --path .`）；未决定发布到 crates.io。`Cargo.toml` 已按发布
规范补齐 `license` / `repository` / `homepage` / `documentation` /
`keywords` / `categories`，正式发版前只需补 LICENSE 文件与版本策略，不再
动元数据结构。

- **license = Apache-2.0**：与只读参考基线 block/goose（Apache-2.0）对齐；
  AGENTS.md 约定成段搬运在 commit 注明出处，衍生代码需保持 Apache-2.0
  兼容。正式对外发布前由 owner 复核此选择。

## Toolchain / MSRV

- **固定策略**：`rust-toolchain.toml` 钉死 `1.94.0`（含 rustfmt、clippy
  组件）；本地和 CI（rustup 读取同一文件）使用同一可复现 toolchain，
  不允许无记录的 moving stable。
- **MSRV**：`Cargo.toml` 的 `rust-version = "1.93"`。验证证据：
  2026-09-05 在本机 `cargo +1.93.1 check --locked --all-targets` 通过（全部依赖 +
  测试 target 可编译）；CI `msrv` job 固定 1.93.1 持续执行同一命令，主工具链
  仍由 `rust-toolchain.toml` 的 1.94.0 控制。该值是已验证下界，不是承诺支持更早版本。
- **升级政策**：钉版升级 = 一次独立 commit，PR 内必须记录新版本号并跑完
  下方全部门槛命令；不允许顺手 bump。降级 MSRV 需重新验证依赖树。

## CI 门槛（与 scripts/ci.sh 一一对应）

`check` job（阻断）：

1. `cargo fmt --check`
2. `cargo clippy --all-targets -- -D warnings`
3. `cargo test`
4. Python 回归：`PYTHONDONTWRITEBYTECODE=1 python3 -W error::ResourceWarning -m unittest discover -s tests -p 'test_*.py'`
   （解释器由 `actions/setup-python` 固定；`ResourceWarning` 直接失败，不全局屏蔽）
5. `cargo rustdoc --lib -- -D warnings`（intra-doc 断链即失败）
6. `cargo check --release --all-targets`（release smoke）
7. `cargo run -q -- --help`（CLI 冒烟）

`msrv` job（阻断）：`cargo +1.93.1 check --locked --all-targets`（固定最低版本；
主工具链仍由 `rust-toolchain.toml` 的 1.94.0 控制；`--locked` 保证不动依赖与 feature）。

`audit` job（cargo-audit / RUSTSEC 公告）：

- **PR/push 不阻断**。原因：修复公告必然要求升级依赖，与 AGENTS.md
  「不加依赖、不升级依赖」的锁定冲突；锁定解除前阻断只会强制违反仓库
  约定。owner：@zhy0216 负责评估公告并决定是否解锁升级（届时把该步改为
  阻断）。
- **scheduled scan 阻断**：每周期的定时运行（`schedule` cron）不豁免，
  公告以红灯 workflow run 通知 owner；这是"定期暴露"机制，不是合并门槛。
- 本地 `scripts/ci.sh` 中 cargo-audit 为信息性步骤（未安装则跳过提示）。
- 未引入 cargo-deny：audit-check 已覆盖 RUSTSEC 公告面，license/sources
  检查在发布到 crates.io 前再评估（YAGNI，owner 决定）。

## tokio feature 收窄评估（plan E9，本轮不改）

`tokio = { version = "1", features = ["full"] }` 超配。实际用到的能力（按
全仓 grep `tokio::`）：`macros`（`#[tokio::main]` / `join!` / `select!`）、
`rt`（current_thread + spawn_blocking）、`process`（子进程）、`signal`
（Ctrl-C）、`time`（timeout/sleep）、`io-util`（stdout/stderr piped +
读取）、`sync`（broadcast/mpsc oneshot Mutex）。候选窄化：
`["macros","rt","process","signal","time","io-util","sync"]`。
不执行原因：AGENTS.md 由 todos/01 锁定 Cargo.toml 依赖与 feature，任务
17 的约束同样禁止改依赖声明；解除锁定后按此清单一次变更并以
`cargo check --all-targets` + 测试矩阵验证。

## provider proxy readiness flake 采样记录（plan A8）

历史执行记录称并发构建下 readiness 偶发 flake。2026-09-05 采样：

- 顺序运行 5×（`cargo test --test provider_proxy`，7 测试）：**0 失败**。
- 并发运行 3 波 × 8 进程 × `--test-threads 4`（同一测试二进制并行，
  ≈168 次测试执行）：**1 失败**
  （`dropping_provider_kills_proxy_leaving_no_listener`，
  `fake-proxy-server exited (exit status: 101) before ready`）。
- 根因（可复现、确定）：`src/provider/proxy.rs` 的 `free_port()` 用
  bind :0 → 读端口 → **立即 drop listener** 的方式选端口，父进程与子
  进程 bind 之间存在 TOCTOU 竞态；并行测试进程/并发 launch 时端口可被
  抢注，fixture 在 `bind(...).expect(...)` panic（exit 101）→ 表现为
  "exited before ready"。串行几乎不触发，与历史"并发下偶发"一致。
- 本轮未修复原因：确定性修复点在 `src/provider/proxy.rs`
  （`launch`/`spawn_and_wait` 对"子进程在 ready 前退出"换端口重试 1–2
  次，或让子进程自选端口回报），超出本任务允许修改的文件面；测试侧
  缓解只会掩盖竞态，不做。修复任务落地时以本文档的并发采样命令作为
  回归验收（并发 24 run 至少 0 失败）。

本轮验证（todo 05，当前构建的 `tests/provider_proxy` 二进制
`--test-threads 4`，timeout 120s）：并发 3 波 × 8 进程 = 24 run
全部通过（0 失败，360 次测试执行）。采样只补充说明有限重试与总期限的
缓解效果，不证明 TOCTOU 已消除：选端口（bind→释放）与子进程 bind 之间
仍无原子交接，完整端口继承协议属 roadmap RM06。

## 全部验证命令

```bash
bash scripts/ci.sh                                  # 1–7 全量 + audit 提示
cargo +1.93.1 check --locked --all-targets          # MSRV 下界复验（需装有 1.93.1）
```
