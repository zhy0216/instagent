# 07 · 插件：install 与 bundled 插件

优先级：P1 · 依赖：00、04、05

目标：实现插件安装/更新数据层 + 内嵌 bundled 插件。只填 `src/plugin/install.rs`、
`src/plugin/bundled.rs`、`bundled/` 目录。

验收：`cargo test` 过；本地路径安装、file:// git 仓库安装、更新节流有测试。

计划参考：第三版 §1（bundled）、§2.10（安装/信任的位置与 PLUGIN_DATA）。

## H1 · install.rs {#h1}

- `install(source, opts)` 数据层：git-url 用 `git` 子进程 clone（不引 libgit2），
  本地路径直接复制 → 用 `04` 校验 `plugin.json` → 写 `.install.json`（source、commit、时间）
  → 放入 `~/.agents/plugins/`。
- `update`：按 `.install.json` 重新拉取；24h 自动更新节流（记录上次检查时间）。
- list / enable / disable / show 的数据层函数（读 manifest + `01` settings 写回）；
  CLI 接线在 `18`。
- `PLUGIN_DATA`：`~/.local/share/instagent/plugins/<name>/`，按需创建。

## H2 · bundled.rs {#h2}

- `include_dir` 内嵌仓库内 `bundled/` 目录（`plugin.json` + `dev.instagent/providers/`，
  provider JSON 由 `10` 生成，本任务先放合法占位）。
- 加载路径与外部插件一致（走 `05` 的发现/启用，作为内置来源注入），名为 `bundled`。
- bundled provider 定义中不使用 `${PLUGIN_ROOT}`（无文件系统 root）。

## H3 · 测试 {#h3}

- fixtures：本地路径安装、`file://` git 仓库安装、`.install.json` 内容、24h 节流、
  bundled 出现在 PluginSet 中。
