difficulty: easy

# 01 liveplug 插件夹具

为 `tests/live_e2e.rs`（02）准备静态、无害、可被 `--plugin` 临时加载的插件夹具。
格式一律对照 `docs/usage.md` §9.1 / §9.4 / §9.5 / §9.6 / §9.7，可参考现有
`tests/fixtures/trusty-plugin/` 的布局与脚本写法。全部组件不含网络调用，
命令只做 echo / 写 `$PLUGIN_DATA` 下文件。

## T1 · plugin.json

- 要做什么：`tests/fixtures/liveplug/plugin.json`，`name` 为 `liveplug`，
  字段按 §9.1；需声明 hooks / command tools / skills / commands 生效所需的
  字段（照 §9 与 trusty-plugin 抄，不臆造 schema）。
- 预计修改：`tests/fixtures/liveplug/plugin.json`（新建）。
- 验收：`python3 -m json.tool` 解析通过（或 `cargo test` 不受影响即编译层无感）；
  字段与 trusty-plugin 的 plugin.json 结构一致。
- 前置依赖：无。

## T2 · command tool echoer

- 要做什么：`tests/fixtures/liveplug/dev.instagent/tools/echoer.json`，
  单个工具对象：`name: "echoer"`、`description`、`input_schema`（一个
  `text: string` 属性即可）、`command: "${PLUGIN_ROOT}/scripts/echoer.sh"`。
  配套 `scripts/echoer.sh`：把入参 JSON 里的 `text` 原样 echo 到 stdout
  （jq 不一定有，用 `sed`/`grep -o` 或直接 `cat` 全文回显均可——02 的断言
  只查标记词出现在输出链中）。脚本加执行位（`git update-index` 或直接
  `chmod +x`，与 trusty-plugin 的 guard.sh 保持一致）。
- 预计修改：`tests/fixtures/liveplug/dev.instagent/tools/echoer.json`、
  `tests/fixtures/liveplug/scripts/echoer.sh`（均新建）。
- 验收：`sh -c` 手跑 `echo '{"text":"MANGO_77"}' | scripts/echoer.sh` 输出含
  `MANGO_77`；文件有执行位。
- 前置依赖：无（与 T1 同目录，一并提交）。

## T3 · hooks（guard + 载荷落盘）

- 要做什么：`tests/fixtures/liveplug/dev.instagent/hooks.json`（格式照 §9.5）：
  - `PreToolUse`（matcher `shell`）→ `scripts/guard.sh`：stdin 载荷中含
    `forbidden_marker` 则 exit 2（stderr 给理由），否则 exit 0 放行；
  - `SessionStart` → `scripts/session_start.sh`：把 stdin 载荷原文写到
    `$PLUGIN_DATA/session_start.json`；
  - `PostToolUse` → `scripts/post_tool_use.sh`：把 stdin 载荷原文写到
    `$PLUGIN_DATA/post_tool_use.json`。
  注意 §9.5 环境变量白名单：脚本只依赖 `PATH` + `PLUGIN_ROOT`/`PLUGIN_DATA`
  （以 hooks 实现实际注入的变量为准，先读 `src/hooks.rs` 确认 `PLUGIN_DATA`
  是否可用；若没有该变量，改为写 `${PLUGIN_ROOT}/.hook-out/` 并在脚本注释
  说明——02 按实际路径断言）。
- 预计修改：`tests/fixtures/liveplug/dev.instagent/hooks.json`、
  `tests/fixtures/liveplug/scripts/guard.sh`、
  `tests/fixtures/liveplug/scripts/session_start.sh`、
  `tests/fixtures/liveplug/scripts/post_tool_use.sh`（均新建）。
- 验收：`printf '{"tool_input":{"command":"run forbidden_marker"}}' | sh guard.sh; echo $?` 为 2；
  不含 marker 时为 0；JSON 可解析。
- 前置依赖：无。

## T4 · skill + 斜杠命令

- 要做什么：
  - `tests/fixtures/liveplug/skills/secret/SKILL.md`：frontmatter
    `name: secret`（等于目录名）、`description`；正文写明"被问暗号时只回复
    KIWI_55"。
  - `tests/fixtures/liveplug/dev.instagent/commands/greet.md`：frontmatter
    `description`、正文含 `$ARGUMENTS`（如"只回复一个单词：$ARGUMENTS"）。
- 预计修改：以上两文件（新建）。
- 验收：目录/命名符合 §9.6（`name` 小写且等于目录名、description 存在）；
  greet.md 含 `$ARGUMENTS` 字样。
- 前置依赖：无。

## 验证方式（本 todo 独立完成）

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

夹具是纯静态文件，不参与编译，三条命令必须与改动前同样全绿；再加 T2/T3 的
shell 手跑检查。
