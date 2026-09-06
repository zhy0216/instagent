difficulty: medium
agent: inherit

# 06 · 本地安装目录关系与元数据预算

对应方案 I01、I02。前置依赖：无。

状态：已完成（2026-09-06）；实现与行为回归同一提交。

## 涉及文件

- `src/plugin/install.rs`（含模块内测试）

## T1 · 递归复制前拒绝源包含 staging

工作：对 InstallSource::Path 的源路径和已解析安装/staging 路径检查祖先关系。源包含目标 staging 或与之重叠时，在创建递归副本前报明确错误。处理相对路径、既存父目录规范化及路径别名，继续保留源树内 symlink/特殊文件拒绝。不要简单拒绝安装根里的所有源：从一个普通已安装插件目录重装仍应有效。

预计修改文件：`src/plugin/install.rs`。

验收：
- [x] 临时安装根位于 source/agents 时清晰失败，不出现嵌套 staging 或“路径过长”。
- [x] source 等于/包含 staging 的边界、相对路径与普通兄弟目录场景有回归；失败不改变既有安装和恢复备份。
- [x] 普通源安装及从已有插件目录重装成功；测试限制目录深度，不等真正递归到平台错误。

前置依赖：无。

## T2 · 安装元数据读取有界

工作：metadata::read_install_info 对 .install.json 使用 1 MiB 实际读取预算及 UTF-8 检查；缺失、超限与解析失败正确区分，错误带路径但不回显可能含凭据的 source 内容。

预计修改文件：`src/plugin/install.rs`。

验收：
- [x] 正常、小预算边界、超一字节、预检后增长与非法编码用例通过。
- [x] update/list 等既有调用按现行容错语义处理，不把超限 metadata 默默覆盖成默认配置。
- [x] 错误路径旧插件、metadata、其他安装的备份不变。

前置依赖：无。

## 校验

每个 commit 执行：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

09 合入前，给执行测试的进程移除 `TOKEN_PLAN_API_KEY`（`env -u TOKEN_PLAN_API_KEY cargo test`），记录 live 用例门控返回；09 合入后默认 cargo test 应显示 live ignored，不因继承凭据而联网。不得修改全局环境、真实凭据或共享源码夹具。实现与行为回归同一 commit；不改依赖/feature、`src/lib.rs` 模块树、其他任务文件或任何既有 done 归档。

## 验收证据

- T1：`local_install_rejects_source_containing_agents_without_changes` 验证明确 overlap 错误，以及旧插件、元数据、恢复备份和过期 staging 均保持原样；`local_install_rejects_sources_inside_staging_before_cleanup` 覆盖源等于 staging 父目录或位于其内部，在清理前拒绝。
- T1：`copy_tree_rejects_equal_ancestor_and_descendant_destinations` 覆盖相等、双向祖先关系和未创建后缀；`local_install_resolves_source_and_staging_aliases_before_creation` 覆盖源/安装根别名、链接后的 `..` 与 staging 父目录链接回源。
- T1：`local_install_handles_relative_paths_missing_parents_and_reinstall` 覆盖相对路径、既存父目录规范化、普通兄弟目录与已安装插件重装；既有普通本地安装测试继续通过。测试构建中的递归复制及快照均限制为 8 层，符号链接四种层级/类型组合及 socket 拒绝回归通过。
- T2：`install_metadata_enforces_small_byte_budget_boundaries` 覆盖小预算上下边界；`install_metadata_bounds_reads_after_preflight_growth` 在同一打开文件预检后追加内容，证明最多读取预算加一字节；`install_metadata_distinguishes_missing_encoding_and_parse_errors` 区分 NotFound、UTF-8、JSON/字段错误且完整错误链不含假凭据。
- T2：`invalid_install_metadata_preserves_update_list_and_backup_behavior` 验证默认 1 MiB 恰好成功、超一字节失败；超限、非法编码、损坏 JSON 下 update/检查时间写入失败，list/show 保留插件并返回无 install_info，auto-update 跳过。全树快照确认旧插件、metadata 和其他安装备份均未改变。

## 校验结果

- `env -u TOKEN_PLAN_API_KEY cargo test --lib plugin::install::tests`：36 passed，0 failed（新增 10 个测试，并扩充既有 symlink 回归）。
- `cargo fmt --check`：通过。
- `cargo clippy --all-targets -- -D warnings`：通过。
- `env -u TOKEN_PLAN_API_KEY cargo test`：退出 0；610 个实际离线测试通过（lib 487、bin 10、agent_continuation 21、cli_e2e 49、mcp_e2e 15、provider_proxy 15、session_recovery 9、tool_inventory 4）；doc-test 0。
- `env -u TOKEN_PLAN_API_KEY cargo test --test live_e2e -- --nocapture`：10 个用例均打印 `skip: TOKEN_PLAN_API_KEY not set` 后门控返回；未运行真实模型测试。
- `git diff --check`：通过。

## 交接注意

- 路径检查覆盖检查时的目录关系；不扩展为跨进程目录替换事务或全路径隔离。
- 全量离线测试中的既有 `tests/cli_e2e.rs::plugin_task_template_expands_arguments_and_unknown_template_fails` 直接使用源码 liveplug，触发 SessionStart hook 并新增 `.hook-out/session_start.json`。仅本次新增输出已移到 `/tmp/instagent-todo06-test-output-took57rb/liveplug-hook-out` 保留，源码夹具恢复原状；没有查看输出内容或触碰用户指定的原备份。该 CLI 测试属于 todo 07 文件范围，交由协调器安排隔离修复，本任务未越界修改。
