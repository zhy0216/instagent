# ADR 0001: 不支持 Anthropic（移除原生引擎与 anthropic-compat）

- 状态：已接受
- 日期：2026-09-03
- 取代：第三版 §2.4 的可选 anthropic engine、`todos/done/12-provider-anthropic.md`

## 背景

按第三版 §2.4 与 todo 12，我们实现了原生 Anthropic Messages API 引擎
（`src/provider/anthropic.rs`，约 1100 行，`anthropic-engine` cargo feature
门控），并 bundled 了 `anthropic-compat`（openai 引擎打 Anthropic 官方
OpenAI 兼容端点）。为此维护着：第二条引擎实现、独立的 SSE 对照夹具
（`tests/fixtures/anthropic/`）、CI 里翻倍的两步全量校验
（clippy + test 各跑一遍 `--features anthropic-engine`）、registry /
shared / 文档里的双引擎分支与说明。

但实际用户全部走便宜或内部的 OpenAI 兼容网关（如 token-plan），没有人
直接付费用 Anthropic 官方 API——价格比这些网关贵一个数量级，这个项目的
使用场景里没有任何理由花这个钱。双引擎 + 双校验面在零使用下是纯维护
成本：每个跨 provider 的改动（图片、thinking 块等）都要在两侧各做一遍。

## 决定

彻底移除 Anthropic 支持：

- 删除 `src/provider/anthropic.rs` 与 `anthropic-engine` feature；
  `EngineKind` 只剩 `openai` / `proxy`；
- 删除 bundled provider `anthropic-compat.json`；
- 删除 `tests/fixtures/anthropic/`；
- CI（`scripts/ci.sh`、`.github/workflows/ci.yml`）去掉该 feature 的两步，
  恢复单一校验面；
- README / usage / architecture 文档同步去掉相关描述。

## 后果

- provider JSON 里写 `"engine": "anthropic"` 会在加载期反序列化失败
  （serde unknown variant）——这是预期行为。
- 想用 Claude 模型的用户仍可走 OpenRouter 等第三方 OpenAI 兼容网关
  （`anthropic/...` 前缀模型），或自己写一个 openai 引擎的插件 JSON。
- 未执行的 `plans/thinking-blocks/` 失去落点（它只为原生引擎服务）；
  `plans/image-support/` 只需做 openai 引擎一侧。
- 将来若确有原生支持 Anthropic 的需求，可推翻本 ADR，实现与测试夹具都
  可从 git 历史找回（todo 12 相关提交）。
