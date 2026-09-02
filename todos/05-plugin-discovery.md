# 05 · 插件：发现与启用

优先级：P1 · 依赖：00、04、01

目标：实现插件发现、同名覆盖、启用/禁用判定。只填 `src/plugin/discovery.rs`
（`src/plugin/mod.rs` 中 `PluginSet` 等对外类型归本任务）。

验收：`cargo test` 过；项目层覆盖用户层、三层启用配置组合有测试。

计划参考：第三版 §2.10（位置与启用）。

## F1 · discovery.rs {#f1}

- 扫描顺序：`~/.agents/plugins/<name>/`（用户）、`<project>/.agents/plugins/<name>/`（项目）、
  配置 `plugins` 额外路径（01）、运行时 `--plugin PATH` 参数。
- 同名插件项目层覆盖用户层。
- manifest 校验失败的插件：记录原因并跳过，不中断整体发现。

## F2 · 启用判定与 PluginSet {#f2}

- 用 `01` 的三层 settings：写了 `enabledPlugins` 即白名单模式；否则"除 `disabledPlugins` 外全启用"；
  优先级 local > project > user。
- `PluginSet`：启用插件集合，迭代得到 {manifest, root 路径, 来源}，
  供 06/07/10/13/15 使用。

## F3 · 测试 {#f3}

- fixtures：多目录发现、同名覆盖、白名单/黑名单/三层冲突组合。
