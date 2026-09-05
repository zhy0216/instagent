difficulty: hard

# 05 · Proxy 就绪与并发重启

优先级：P1。模型：`bailian-token-plan/qwen3.8-max`。方案发现：R01–R02。前置依赖：无。

## 涉及文件

- `src/provider/proxy.rs`
- `tests/provider_proxy.rs`
- `tests/fixtures/fake_proxy_server.rs`

## T1 · 有限启动重试与总就绪期限

要做：在 launch/spawn_and_wait 中为选端口后竞争导致的候选启动失败提供有限换端口重试（最多额外 2 次），明确可重试和不可重试错误，保留 provider/命令/尝试次数/退出状态。spawn 不存在、配置无效等立即失败。就绪 probe 和 sleep 都受剩余总期限约束，重试不能重新获得一份完整 timeout。使用受检时间运算防止极大 timeout panic。

预计修改文件：`src/provider/proxy.rs`、`tests/provider_proxy.rs`、`tests/fixtures/fake_proxy_server.rs`。

验收：可控端口分配/占用或 fixture 注入让第一次候选失败而下一次成功；持续提前退出在固定尝试数后结束；永不就绪的 HTTP handler 不能超出总预算加少量调度容差；所有失败子进程组被回收。若需要 stderr 诊断必须有界收集，不重新引入无限日志缓冲。`${PORT}` 与 INSTAGENT_PORT、ready path、环境 allowlist 保持兼容。

前置依赖：无。

## T2 · 并发连接失败合并重启

要做：对同一 ProxyProvider 的 restart 引入异步串行协调与实例代次检查；第二个失败请求发现别的请求已成功替换实例时复用新实例。不要在持 std::sync::Mutex guard 时 await；被取消的候选实例能被 drop 回收。

预计修改文件：`src/provider/proxy.rs`、`tests/provider_proxy.rs`、`tests/fixtures/fake_proxy_server.rs`。

验收：两个受屏障控制的请求同时发现旧 proxy 死亡，仅启动一份替代实例；两个请求均可成功，后到者不杀正在服务的实例；单调用最多重试一次 HTTP stream 的既有契约保留；drop 最终 provider 清除 listener/子进程。

前置依赖：本文件 T1。

## 校验与完成

```bash
cargo test provider::proxy
cargo test --test provider_proxy
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

确定性用例通过后，按 docs/release.md 的规模做一次并发采样（3 波 × 8 进程；使用当前构建的测试二进制、独立进程组与超时），记录命令及失败数。采样不是证明端口 TOCTOU 已消除；完整端口交接协议属于 roadmap。一个本地 commit，采样结果写完成记录供 09 更新 release 文档。
