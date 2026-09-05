difficulty: hard

# 07 · 一致工具清单和有界 MCP 日志

优先级：P1。模型：`bailian-token-plan/qwen3.8-max`。方案发现：T01–T02。前置依赖：无。

## 涉及文件

- `src/tools/mod.rs`
- `src/tools/mcp.rs`
- `tests/mcp_e2e.rs`
- `tests/fixtures/mcp_stdio_server.rs`
- `tests/tool_inventory.rs`（允许新增的 Registry 行为测试）

## T1 · 清单与路由原子发布、失效和恢复

要做：将 specs/routes/list_errors 作为同一快照发布，异步刷新只允许一个在途请求；invalidate 递增代次，旧枚举结果不能覆盖新一代。健康缓存仍直接命中。含失败来源的缓存记录错误和有界重试时机，后续 list 可恢复来源，不能永久冻结为空清单；重试不得形成每轮无上限网络风暴。同来源重复工具名去重并诊断。

预计修改文件：`src/tools/mod.rs`、`tests/tool_inventory.rs`；必要的真实 MCP 场景在 `tests/mcp_e2e.rs`/fixture。

验收：同步屏障控制 list 与 invalidate 交错，模型可见名始终路由到对应来源；并发 list 合并刷新；一个来源先失败后恢复能重新出现，健康来源持续可用；重复工具名不会让 provider 请求整个失败。使用可注入时间或私有时钟状态测试重试期限，不新增 tokio feature。

前置依赖：无。

## T2 · 无换行 stderr 有界且持续排空

要做：替换 pipe_stderr_to_logs 的无界 lines()，按固定块增量解码 UTF-8，长行保留有界片段并发截断说明，持续消费剩余字节防止子进程卡在写管道；对大量日志做限速/计数摘要。超限针对日志，不因此杀死仍健康的共享 MCP server。

预计修改文件：`src/tools/mcp.rs`、`tests/mcp_e2e.rs`、`tests/fixtures/mcp_stdio_server.rs`。

验收：fixture 在 stderr 写大量无换行内容时 initialize/list/call 仍成功；单条保留文本和采样窗口内诊断数量有上限；跨块 Unicode 不损坏；shutdown/进程退出结束 drain，无残留进程。不要声称解决了 rmcp 内部所有网络载荷分配或 HTTP headers/SSE transport 支持。

前置依赖：无，可独立于本文件 T1 实现。

## 校验与完成

```bash
cargo test tools
cargo test --test mcp_e2e
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

一个本地 commit。保留 Registry::list/call/capability/invalidate 的外部调用方式，默认诊断可见性由 08 的 CLI 配置落地。

## 完成记录

状态：完成。只改 `src/tools/mod.rs`、`src/tools/mcp.rs`、`tests/mcp_e2e.rs`、`tests/fixtures/mcp_stdio_server.rs`，新增 `tests/tool_inventory.rs`；未动 `Cargo.toml`/`Cargo.lock`/`src/lib.rs`，未新增 tokio feature。

关键行为：Registry 以 `Snapshot{specs,routes,errors,failures}` 同一快照原子发布，`tokio::sync::Mutex` 单飞合并并发 list，`invalidate` 经 `AtomicU64` 递增代次并清空快照、旧枚举跨代次丢弃；健康无失败时直接命中缓存，失败来源记 `next_retry`（基线 5s、上限 60s 指数退避，到期前复用缓存、到期后合并刷新一次），`force_retry_due` 为私有时钟的可注入控制；同源重复实名去重保留首份并记 `duplicate … kept first` 诊断。MCP stderr 改固定 8 KiB 块增量 UTF-8 解码，单行至多保留 8 KiB 头部并附截断说明、余下持续排空，采样窗口 10s 内至多 20 条、超限计数汇总；超限只丢日志，不杀共享 server（rmcp 内部载荷/headers/SSE 仍为 roadmap RM04，本次不碰）。

验收证据：T1 由 `tests/tool_inventory.rs` 4 例覆盖——`concurrent_lists_merge_into_single_refresh`（10 并发经 Barrier 同起，枚举==1、再查仍命中）、`invalidate_during_enumeration_discards_stale_snapshot`（Notify 门控首轮枚举、在途改名+invalidate、旧结果丢弃后得新清单且可见名路由到对应来源）、`failed_source_recovers_after_retry_due_while_healthy_stays`（失败→窗口内零重枚举→拨到期后一次刷新恢复、错误清空）、`duplicate_tool_names_are_deduped_without_failing_source`（重复只留一份、可调用、健康源可用、错误含 duplicate 诊断）；T2 由 `mcp.rs` 3 单元测试（跨块中文/emoji 全切分点重组、无半字符，单行 8 KiB 封顶+说明，限速器注入时间 100 突发只放 20 条+汇总）与 `mcp_e2e::stderr_flood_without_newlines_keeps_server_healthy`（256 KiB 无换行+逐字节 Unicode+100 行洪泛下 initialize/list/call 成功、shutdown 后 `kill -0` 轮询确认无残留）覆盖；既有 `mcp_e2e` 14 例与 `tools` 单元保持。
