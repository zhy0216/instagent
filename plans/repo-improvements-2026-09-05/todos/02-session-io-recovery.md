difficulty: hard

# 02 · 会话 IO、恢复与备份

优先级：P1。模型：`bailian-token-plan/qwen3.8-max`。方案发现：S01–S04。前置依赖：无。

## 涉及文件

- `src/session.rs`（实现与模块内测试）
- `tests/session_recovery.rs`（允许新增的会话集成测试）

## T1 · 有界记录读取和可再次恢复的文件

要做：集中 header/正文的有界字节读取，消除整行 raw 错误回显及不必要的全量 raws 副本。header 坏数据明确报错；正文坏 UTF-8/坏 JSON 保留合法前缀并物理修复。有效最后一行无换行也需规范化后再允许 append。超行/总预算属于拒绝打开，原文件字节保持原状，不借 salvage 删除超预算内容。

预计修改文件：`src/session.rs`、`tests/session_recovery.rs`。

验收：含假密钥的 header 错误只输出路径/行号/约束；损坏正文恢复→追加合法 assistant→再次恢复不丢新消息；无末尾换行的 header/消息不会粘连；超预算文件完整保留；正常图片会话可恢复。测试可给私有 helper 注入小预算以避免制造百 MB fixture。

前置依赖：无。

## T2 · 成批追加和失败清理

要做：增加 `Session::append_batch` 或等效明确 API，供 03 一次提交 assistant+tool results；预序列化、校验候选历史，记录原文件长度，普通写入/flush 失败回退文件并保持内存旧状态。现有 append 保持签名并复用正确路径。若回退也失败，错误明确要求重新恢复，不能报成功。统一最终主文件 symlink 拒绝；不扩张到全路径 containment。

预计修改文件：`src/session.rs`、`tests/session_recovery.rs`。

验收：合法 batch 可恢复，非法 batch 零落盘；注入短写/写失败时磁盘与内存均保持旧前缀；失败错误链可诊断且无原文。文件/目录仍遵循 0600/0700。只保证已检测 IO 错误的回退，不宣称追加在主机掉电时具备事务原子性。

前置依赖：本文件 T1。

## T3 · 临时文件和备份归属

要做：atomic_replace 在创建后任意写/flush/sync/rename 失败均清理临时文件；备份创建即私有。按精确备份命名解析归属，避免 `a` 清理 `a.b` 的备份，保留每会话最多 5 份的原约定。

预计修改文件：`src/session.rs`、`tests/session_recovery.rs`。

验收：各失败点旧主文件仍可读，没有本次遗留 tmp；`a`/`a.b`、同秒多备份相互独立；回滚/备份错误诊断有界。优先私有 IO 注入，不通过修改真实磁盘权限或用 /dev/full 破坏环境来测。

前置依赖：本文件 T2。

## 校验与完成

```bash
cargo test session
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

本任务不修改 message 校验语义或 agent；提供批量 API 后在完成记录写明签名供 03 使用。一个本地 commit。
