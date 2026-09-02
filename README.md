# instagent

插件为核心的最小 agent（Rust 从零实现）——一句话占位，完整文档由 todos/19-hardening.md 补全。

```bash
cargo run -- --help
```

## 手工验证清单（todos/18 · CLI 与运行时装配）

`cargo test` 覆盖不到的交互路径，按此清单手工验证（建议先
`INSTAGENT_CONFIG_DIR=$(mktemp -d) cargo run -- chat` 用沙箱目录，不碰真实配置）：

1. **多轮对话**：`cargo run -- chat`（配好 provider/model），连续问两三条消息，
   确认流式文本逐字打印、每轮末尾打 `usage:` 行、会话 JSONL 落在
   `<data>/sessions/`。
2. **Ctrl-C 一次取消、两次退出**：发起一条会让模型长时间输出的消息，轮内按
   Ctrl-C → 打印 `^C cancelling current turn`、本轮以 `(turn cancelled)` 结束、
   REPL 可继续；再发一条并按两次 Ctrl-C → 进程退出。空闲提示符下按两次
   Ctrl-C 也应退出。
3. **/compact**：多聊几轮后输入 `/compact`，看到 `· compacted X → Y tokens` 与
   `(compacted)`；`/clear` 会丢弃上下文（会话文件原子重写、留 `.bak.jsonl`）。
4. **--resume last**：退出后 `cargo run -- chat --resume last`，确认历史被读回、
   模型能引用上一会话内容；`instagent sessions list` 两行、`sessions rm <id>` 可删。
5. **run -t**：`cargo run -- run -t "list files"`，无交互跑完，stdout 有最终回复
   与 `usage:` 行，工具调用打 `▶ shell  …` + 预览/耗时。
6. **approve 模式审批提示**：`cargo run -- chat --mode approve`，让模型跑 `shell`，
   出现 `allow this call? [y]es / [a]lways / [n]o:`；选 `a` 后同会话再跑 shell
   不再询问，且 config.yaml 的 `always_allow` 多了一条。
7. **plugin 子命令 + 首次启用确认**：
   `cargo run -- plugin install <本地含 hooks/mcp/tools 的插件路径>` → 列出全部
   命令要求确认；答 `y` 后 `~/.config/instagent/settings.json` 出现
   `trustedPlugins`；`plugin list / show / disable / enable`（enable 同样触发确认）；
   未信任的插件在 chat 启动时只打 `note: plugin ... is not trusted` 且其
   mcp/hooks/command tools 不加载；`--yes` 跳过确认。
8. **--plugin PATH 临时加载**：把含 provider JSON 的开发目录经 `--plugin` 传入，
   `chat` 里 `provider` 可直接用该插件定义；`/help` 列出插件斜杠命令并可用
   `/review <args>` 展开成 prompt。
