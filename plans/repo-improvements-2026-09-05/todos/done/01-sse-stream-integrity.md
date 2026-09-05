difficulty: hard

# 01 · SSE 解码、终止与资源边界

优先级：P1。模型：`bailian-token-plan/qwen3.8-max`。方案发现：P01–P04。前置依赖：无。

## 涉及文件

- `src/provider/http.rs`
- `src/provider/shared.rs`
- `src/provider/openai.rs`
- `tests/fixtures/openai/` 下现有/新增 SSE 和 expected JSON fixture（仅本任务的协议样例）

## T1 · 按字节增量解析 SSE

要做：替换 event_stream 每块 `from_utf8_lossy` 的转换；在 http.rs 内保留未完成 UTF-8 字节与未结束行。支持 LF、CRLF、CR、开头 BOM、多行 data、注释，避免每次追加都 replace/drain 整段历史。保留 SseParser 公开入口的兼容调用方式，必要时添加字节入口。

预计修改文件：`src/provider/http.rs`；上述 SSE fixture。

验收：中文/emoji 在每个字节切分点、跨 CRLF/BOM 分块时产物一致；原有解析 fixture 仍通过；EOF 中未完成 SSE 事件不被派发。使用纯解析器分块测试加至少一条本地 HTTP 字节分块测试，不用网络包恰好如何合并作为唯一证据。

前置依赖：无。

## T2 · 区分完成与断流，控制缓冲

要做：shared 驱动区分 `[DONE]`、已收到非空 finish_reason 的 EOF、没有完成信息的 EOF。最后一种报 ProviderError，不 flush pending tools 成可执行调用；usage/Done 只发一次。给单 SSE 事件、响应文本、累计 tool arguments 和工具数量加上限；超限终止并给有界、无原始载荷的诊断。usage 大整数转换不得回绕。

预计修改文件：`src/provider/http.rs`、`src/provider/shared.rs`、`src/provider/openai.rs`；协议 fixture。

验收：有 finish_reason 无 `[DONE]` 的兼容流通过；无二者的文本/工具流明确失败；完整参数也不能因 EOF 自动执行；有效终止后的 usage 保留；无换行大输入、多 index、大 arguments、超大 usage 覆盖边界。建议常量见 plan；数据超限不静默截成另一份有效 JSON。不改变 HTTP 认证、重试政策或图片请求预算。

前置依赖：本文件 T1。

## 校验与完成

```bash
cargo test provider::http
cargo test provider::openai
cargo test provider::shared
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

仅更新本任务和队列状态，形成一个本地 commit；跨 CLI 的复现由 08 在此基础上补，不修改 agent 或 CLI 文件。
