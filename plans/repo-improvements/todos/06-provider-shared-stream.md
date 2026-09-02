difficulty: hard

openai 与 anthropic 两个引擎的 SSE 流状态机骨架大段同构（最大重复块 ~50 行×2），抽公共驱动层。这是本队列最大风险点：两个 1200+ 行文件、各 ~700 行测试，重构全程测试必须保持绿。

## T1 · 共享小件上提

- 把逐字节相同的零散件从 `src/provider/openai.rs` / `src/provider/anthropic.rs` 提到共享位置（优先扩展 `src/provider/http.rs`，或新建 `src/provider/` 私有模块——不改 `lib.rs` 模块树即可）：
  - `to_provider_error`（openai.rs:143-148 ↔ anthropic.rs:165-170）；
  - `PendingCall {id,name,arguments}`（openai.rs:299-304 ↔ anthropic.rs:301-306）；
  - `sanitize_function_name`（openai.rs:279-293，anthropic.rs:48 目前跨模块 import，是公共层信号）；
  - 空 arguments 兜底 `"{}"`、`format_tools` 的 sanitize + `seen` 去重 + `ensure!` 循环。
- 预计修改文件：`src/provider/openai.rs`、`src/provider/anthropic.rs`、`src/provider/http.rs`（或新私有模块）。
- 验收：两引擎现有测试（23 + 20）保持绿；`clippy --features anthropic-engine` 无警告。
- 前置依赖：无。

## T2 · 共享流驱动层（核心）

- `openai.rs:316-369` ↔ `anthropic.rs:364-415` 的 `sse_to_stream_events` `stream::unfold` 驱动 ~90% 相同（弹队列→查 ended→取 SSE→JSON parse→`Transport("malformed SSE chunk…")`→断流 `finalize`），`StreamState`（306-314 ↔ 353-362）与 `finalize`（438-466 ↔ 543-559）同形。
- 抽通用驱动器：引擎只提供"事件→状态"回调（`apply_chunk`/`apply_event`）与 `finalize` 钩子；共享 `StreamState` 结构。保留两引擎各自的 `map_stop_reason` 映射表（仅表不同）。
- **风险约束**：若某引擎语义与共享抽象冲突，允许保留少量引擎特有代码，不强行抽象（见 plan 风险）。
- 预计修改文件：`src/provider/openai.rs`、`src/provider/anthropic.rs`、`src/provider/http.rs`（或新私有模块）。
- 验收：两引擎现有测试 + `registry` 分发测试 + `anthropic-engine` feature 全绿；SSE 事件序列行为逐字节不变。
- 前置依赖：依赖 06 文件内 T1。

## T3 · 引擎脚手架收敛

- `new()`（openai.rs:73-98 ↔ anthropic.rs:83-108）、`request_headers()`（107-116 ↔ 117-127）、`stream()`（125-141 ↔ 136-152）骨架同构。抽共享构造/请求头骨架，引擎只声明 `EngineKind` 与补充鉴权头（Bearer vs x-api-key+version）。
- 预计修改文件：同 T2。
- 验收：同 T2；引擎公开 `Provider` 行为不变。
- 前置依赖：依赖 06 文件内 T2。

## T4 · 测试脚手架去重

- `openai.rs:507-579,730-765` ↔ `anthropic.rs:608-680,839-874` 的 `fast_retry`/`def`/`provider_at`/`request`/`collect`/`sse_body`/`mount_once`/`event_to_json`/`run_sse`/`fixture_pair` 近乎逐行重复（~110 行×2）。收敛为 `#[cfg(test)]` 共享 testutil（放共享位置），泛型化 provider 构造。
- 预计修改文件：`src/provider/openai.rs`、`src/provider/anthropic.rs`、共享 testutil。
- 验收：两引擎测试数不减少、全绿；重复脚手架收敛到一处。
- 前置依赖：依赖 06 文件内 T3。

## 本文件整体验证

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo clippy --all-targets --features anthropic-engine -- -D warnings
cargo test --features anthropic-engine
```
