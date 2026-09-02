# 15 · 工具：skills 与 command tools

优先级：P2 · 依赖：00、13、05

目标：实现 `SkillsSource`（load_skill）与 `CommandTools`（脚本即工具）。只填
`src/tools/skills.rs`、`src/tools/command.rs`。

验收：`cargo test` 过；frontmatter 校验、load_skill、command tool 执行路径有测试。

计划参考：第三版 §2.6（skills）、§2.9（command tools）。

## P1 · skills 发现与校验 {#p1}

- 发现范围：每个启用插件的 `skills/`（只看一层子目录，每个含 `SKILL.md` 的子目录是一个
  skill，不递归）+ `~/.agents/skills/` + `<project>/.agents/skills/`。
- frontmatter（Agent Skills 规范）：`name` 必需，1~64，小写字母数字和 `-`，必须等于目录名；
  `description` 必需 ≤1024；可选 `license` `compatibility` `metadata` `allowed-tools`
  （allowed-tools 只解析，v2 再用）。无效 skill 跳过不报错。
- 插件里的 skill 命名 `<plugin>:<skill>`。

## P2 · load_skill {#p2}

- `SkillsSource`（id = `"skills"`）只暴露一个工具 `load_skill(name, file?)`。
- 启动时只收集所有 skill 的 `name` + `description`（供系统提示，`16`/`18` 接线）；
  调用 `load_skill` 才读 `SKILL.md` 正文或 `references/` 下的文件。
- 工具描述文本直接抄 `~/yyds/goose` `crates/goose/src/skills/client.rs:91~103`（注明出处）。

## P3 · command tools {#p3}

- `CommandTools`（id = `cmd:<plugin>`）：解析启用插件的 `dev.instagent/tools/*.json`
  （name / description / input_schema / command / timeout_secs / read_only）。
- 执行：input JSON 写到 stdin，stdout 作为工具结果，退出码非 0 即 `is_error`；
  `${PLUGIN_ROOT}` 展开；用 `03` 的进程组。工具名 `<plugin>__<tool>`。

## P4 · 测试 {#p4}

- fixtures：规范最小示例的 skill，load_skill 返回正文与 `references/` 文件；
  frontmatter 非法（名字与目录不符、description 超长）被跳过；
  一个十行 shell 脚本的 command tool：正常返回、非零退出 → is_error、超时。
