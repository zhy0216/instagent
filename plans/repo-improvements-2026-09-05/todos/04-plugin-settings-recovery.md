difficulty: hard

# 04 · 插件白名单与安装恢复

优先级：P1。模型：`bailian-token-plan/qwen3.8-max`。方案发现：I01–I02、D03 的 settings 部分。前置依赖：无。

## 涉及文件

- `src/settings.rs`
- `src/plugin/install.rs`
- `src/plugin/discovery.rs`

## T1 · 全入口保留白名单模式

要做：Settings 合并、直接 serde 反序列化/序列化及 load/save 都保留“未声明/白名单非空/白名单为空”。install::enable 使用 whitelist 语义，把显式 [] 中启用的名字写入白名单；disable/高层过滤最后一项后仍保留白名单模式。同步相关注释和旧错误预期，不改 ADR 的用户/项目/local 优先顺序。

预计修改文件：`src/settings.rs`、`src/plugin/install.rs`；discovery.rs 仅其消费行为回归测试。

验收：[]→enable alpha 后仅 alpha enabled；[alpha]→disable alpha 后 beta 不会启用；低层白名单被高层 disabled 清空后仍禁用其他名字；三态 JSON 往返不漂移；高层 enable 与低层显式空白名单的现有优先级保留。

前置依赖：无。

## T2 · 设置读取预算和来源诊断

要做：read_layer 使用有界读取（建议 1 MiB），解析/IO 错误带具体层的文件路径；只为 NotFound 返回缺省。不要回显 settings 原文或任意密钥值。

预计修改文件：`src/settings.rs`。

验收：用户/项目/local 中超限和坏 JSON 均指向对应文件；原文件不变；普通三层合并和 0600 原子保存测试通过。

前置依赖：本文件 T1。

## T3 · 保留安装恢复副本，排除内部目录

要做：去掉 cleanup_orphan_backups 的无条件全目录删除，只在本次 replace 成功后移除该次拥有的备份。未知/失败回滚备份保留且诊断可说明路径。discovery、install list、auto_update 的安装根扫描排除 `.replaced-*` 和自身 staging 内部目录；不把另一个正在安装/待恢复的副本发现成插件。

预计修改文件：`src/plugin/install.rs`、`src/plugin/discovery.rs`。

验收：安装 newplugin 时 lost 的唯一 `.replaced-lost-<uuid>` 内容不变；模拟另一个活动替换，备份不被删除；隐藏恢复副本不出现在 list/provider discovery/auto-update；本次成功 replace 清自己的备份，失败仍回滚。不得以年龄或前缀单独判断可删除数据，不新增全局破坏性清理命令。

前置依赖：无，可在本任务内独立于 T1/T2 实现。

## 校验与完成

```bash
cargo test settings
cargo test plugin::install
cargo test plugin::discovery
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

一个本地 commit。CLI 白名单集成回归由 08 添加，文档由 09 同步。
