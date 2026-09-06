difficulty: hard
agent: inherit

# 02 · 压缩完整性与未回答输入

对应方案 C01、C02。前置依赖：无。

## 涉及文件

- `src/agent/compact.rs`（实现与模块内测试）

其他任务的 `src/agent/mod.rs` 和 `tests/agent_continuation.rs` 不在白名单内。测试使用 compact 内的假 provider/已有公开 API，不跨文件争用。

## T1 · 只允许正常结束的摘要替换历史

工作：summarize 不能仅看到任意 Done 就认定完整。要求非空文本与 EndTurn，拒绝 MaxTokens、未知异常原因、缺 Done、流错误和不应该出现在摘要请求中的工具事件。取消仍返回既有 no-op；被拒绝摘要不调用 rewrite、不发成功 Compacted 事件。

预计修改文件：`src/agent/compact.rs`。

验收：
- 正常摘要成功；length/MaxTokens、异常原因、工具事件、空文本、EOF/错误、取消矩阵均有确定性测试。
- 失败前后主 JSONL 和内存历史逐字节/结构一致，不增成功压缩记录。
- 自动压缩和强制压缩入口均调用同一完整性判断。

前置依赖：无。

## T2 · 保留末尾未回答消息的全部内容

工作：split_head_tail 不再将未回答 user 收窄成 content.first 的一个 String。按原顺序保存全部内容块，生成 summary 时把未回答输入完整带回；尤其恢复时 prepare_input 添加的第二、第三个 Text 必须保留。图片保留为原有内容块，不塞 base64 到摘要文本。含 ToolResult 的历史仍需精确配对，不放宽 message 校验。

预计修改文件：`src/agent/compact.rs`。

验收：
- 建立 user→assistant（高 usage）→user（多个 Text）的有效历史，再 run_turn 新任务触发压缩；后续 provider 请求和活动会话都含最新任务原文，顺序正确。
- 覆盖单 Text、多个 Text、Text+Image、末尾 tool results 与没有可压缩历史；图片合法且不会静默消失，所有结果 validate/resume 成功。
- 摘要失败时上述未回答输入仍在原会话，不能只依赖备份找回。
- 公开 maybe/force/maybe_cancelable/force_cancelable 兼容，避免影响独立执行的 01。

前置依赖：无；与 T1 同一 commit 完成。

## 校验

每个 commit 执行：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

09 合入前，给执行测试的进程移除 `TOKEN_PLAN_API_KEY`（`env -u TOKEN_PLAN_API_KEY cargo test`），记录 live 用例门控返回；09 合入后默认 cargo test 应显示 live ignored，不因继承凭据而联网。不得修改全局环境、真实凭据或共享源码夹具。实现与行为回归同一 commit；不改依赖/feature、`src/lib.rs` 模块树、其他任务文件或任何既有 done 归档。

