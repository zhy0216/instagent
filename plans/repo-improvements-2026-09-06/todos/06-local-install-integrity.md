difficulty: medium
agent: inherit

# 06 · 本地安装目录关系与元数据预算

对应方案 I01、I02。前置依赖：无。

## 涉及文件

- `src/plugin/install.rs`（含模块内测试）

## T1 · 递归复制前拒绝源包含 staging

工作：对 InstallSource::Path 的源路径和已解析安装/staging 路径检查祖先关系。源包含目标 staging 或与之重叠时，在创建递归副本前报明确错误。处理相对路径、既存父目录规范化及路径别名，继续保留源树内 symlink/特殊文件拒绝。不要简单拒绝安装根里的所有源：从一个普通已安装插件目录重装仍应有效。

预计修改文件：`src/plugin/install.rs`。

验收：
- 临时安装根位于 source/agents 时清晰失败，不出现嵌套 staging 或“路径过长”。
- source 等于/包含 staging 的边界、相对路径与普通兄弟目录场景有回归；失败不改变既有安装和恢复备份。
- 普通源安装及从已有插件目录重装成功；测试限制目录深度，不等真正递归到平台错误。

前置依赖：无。

## T2 · 安装元数据读取有界

工作：metadata::read_install_info 对 .install.json 使用 1 MiB 实际读取预算及 UTF-8 检查；缺失、超限与解析失败正确区分，错误带路径但不回显可能含凭据的 source 内容。

预计修改文件：`src/plugin/install.rs`。

验收：
- 正常、小预算边界、超一字节、预检后增长与非法编码用例通过。
- update/list 等既有调用按现行容错语义处理，不把超限 metadata 默默覆盖成默认配置。
- 错误路径旧插件、metadata、其他安装的备份不变。

前置依赖：无。

## 校验

每个 commit 执行：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

09 合入前，给执行测试的进程移除 `TOKEN_PLAN_API_KEY`（`env -u TOKEN_PLAN_API_KEY cargo test`），记录 live 用例门控返回；09 合入后默认 cargo test 应显示 live ignored，不因继承凭据而联网。不得修改全局环境、真实凭据或共享源码夹具。实现与行为回归同一 commit；不改依赖/feature、`src/lib.rs` 模块树、其他任务文件或任何既有 done 归档。

