difficulty: hard
agent: inherit

# 04 · 会话写入与恢复预算一致

对应方案 S01。前置依赖：无。

## 涉及文件

- `src/session.rs`
- `tests/session_recovery.rs`

## T1 · 各写入入口提交前校验预算

工作：create、append_batch、rewrite/atomic_replace 的候选序列化结果遵守 DEFAULT_LIMITS 的 header、单消息行和总文件预算。按实际 JSON UTF-8 字节计数（含转义与换行），使用受检运算。不要只看 Message 文本长度或只检查追加批次大小；总预算要包含既有文件。保持公开方法签名和默认数值，复用内部预算类型即可，不引入数据库或持久缓存。

预计修改文件：`src/session.rs`。

验收：
- 不能成功写出默认 resume 会因预算拒绝的主文件。
- append 拒绝时内存和文件未变；rewrite 拒绝时不发布新主文件、不覆盖/淘汰可用备份；create 拒绝时不留下损坏会话。
- 保留已有部分 IO 写入回退、私有文件、salvage 与备份归属语义；budget 错误不回显原文。
- JSON 转义膨胀、换行开销、多个小批次跨总预算都得到正确结果。

前置依赖：无。

## T2 · 小预算故障与恢复回归

工作：用测试可注入的小预算覆盖 create/append_batch/rewrite 的边界与拒绝原子性；保留公开生产入口默认预算。补成功写入→resume 的完整流程，不把巨大 fixture 当作日常测试手段。

预计修改文件：`src/session.rs`、`tests/session_recovery.rs`。

验收：
- header、单行、总量分别测试刚好边界和超一字节。
- 消息不变量与 IO 失败路径也验证旧内容保留；失败后继续合法写入并恢复成功。
- 拒绝写入不声称外部工具副作用被回滚；不自动裁剪历史或增大恢复限额。

前置依赖：T1。

## 校验

每个 commit 执行：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

09 合入前，给执行测试的进程移除 `TOKEN_PLAN_API_KEY`（`env -u TOKEN_PLAN_API_KEY cargo test`），记录 live 用例门控返回；09 合入后默认 cargo test 应显示 live ignored，不因继承凭据而联网。不得修改全局环境、真实凭据或共享源码夹具。实现与行为回归同一 commit；不改依赖/feature、`src/lib.rs` 模块树、其他任务文件或任何既有 done 归档。

