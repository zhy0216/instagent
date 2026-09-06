difficulty: medium
agent: inherit

# 10 · 用户契约和集成验证收口

对应方案 D01，并汇总 01–09。前置依赖：01-agent-completion、02-compaction-integrity、03-file-tool-io、04-session-write-budgets、05-plugin-input-limits、06-local-install-integrity、07-template-input-budget、08-provider-required-key、09-live-test-isolation 全部已合入。

## 涉及文件

- `README.md`
- `docs/usage.md`
- `docs/architecture.md`
- `docs/release.md`

本任务只做文档和验证；若检查发现代码回归，交回拥有该文件的任务处理，不在文档 commit 中混入源码修复。

## T1 · 按最终实现同步使用与资源契约

工作：更新模板展开 1 MiB 上限、文件普通权限/读写预算、会话写入拒绝策略、组件文件预算和必需凭据空值判定。说明异常响应/截断摘要的失败与恢复行为、未回答任务保留。介绍默认离线测试和显式 live ignored 命令，分清 Rust 真执行数、ignored 数和在线验证状态。

预计修改文件：`README.md`、`docs/usage.md`、`docs/architecture.md`、`docs/release.md`。

验收：
- 数值、命令、结果/退出码、文档链接与合入代码一致。
- 不恢复 REPL/审批，不夸大为完整 ACL/图像解码/全路径隔离/跨进程事务。
- 不把工具外部副作用写成可随会话预算错误回滚。
- 保留历史线上超时与 proxy 采样的证据口径；新记录明确当前实际验证结果。
- 文档变更无需增加镜像实现的测试，执行现有仓库门槛即可。

前置依赖：01–09 全部合入。

## T2 · 最终集成态校验

工作：运行仓库要求和扩展 CI 检查，确认计划没有引入依赖/feature/module 变更，校验任务产生的输出不污染源码夹具。失败由对应任务修复后再重新验证相关范围；通过后不反复压力测试。

预计修改文件：仅在 `docs/release.md` 记录对用户有用的验证结论；计划执行记录由协调器维护，不改旧 done。

验收命令：
- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`
- `bash scripts/ci.sh`（与上述命令重复时可用该脚本的一次完整输出作为对应验证证据）
- `cargo +1.93.1 check --locked --all-targets`

验收：
- Rust、Python、rustdoc、release、--help 和 MSRV 全部通过。
- 默认 cargo test 不因环境存在凭据而联网；live 显式运行及其状态另记，不能把 ignored 计成在线通过。
- 未安装 cargo-audit 时如实标注；不升级依赖、安装全局工具或修改扫描豁免。
- 最终源码夹具无新增运行产物，未处理任何本轮开始前的用户目录内容。

前置依赖：T1。

## 校验

每个 commit 执行：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

09 合入前，给执行测试的进程移除 `TOKEN_PLAN_API_KEY`（`env -u TOKEN_PLAN_API_KEY cargo test`），记录 live 用例门控返回；09 合入后默认 cargo test 应显示 live ignored，不因继承凭据而联网。不得修改全局环境、真实凭据或共享源码夹具。实现与行为回归同一 commit；不改依赖/feature、`src/lib.rs` 模块树、其他任务文件或任何既有 done 归档。

