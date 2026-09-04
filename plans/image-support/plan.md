# 图片支持：read_image 工具 + Content::Image（v2 候选落地）

## 意图

让模型能"看见"本地图片：新增内置工具 `read_image`，读取本地图片文件（png /
jpeg / gif / webp），产出 `Content::Image`（base64 数据 + media type）；图片块随
会话持久化（JSONL），并在 openai 引擎里序列化进请求，多模态模型真正看到
像素（anthropic 引擎已按 ADR 0001 移除，本项只做 openai 侧）。即 `docs/goose-from-scratch-plan.md` §7 v2 表里的"图片：read_image 工具 +
Content::Image"（原估 300 行）。

调研结论（全部来自代码阅读，无凭空设计）：

- `src/message.rs:27` 的 `Content` 是 `#[serde(untagged)]` 三变体
  （Text/ToolUse/ToolResult），`ToolResult.content` 是纯 `String`——图片无处可放，
  必须新增变体。
- `src/tools/mod.rs:46` 的 `ToolOutput { text, is_error }` 只有文本通道；
  `execute_calls`（`src/agent/mod.rs:220`）经 `Content::tool_result` 把结果折叠成
  单个块，`finish_turn`（`src/agent/mod.rs:501`）整组落盘——接线点在
  `execute_calls`，`finish_turn` 无需改。
- 对 `Content` 做穷举匹配的生产代码共两处，都是新变体的必改点：
  `src/provider/openai.rs:142` `format_messages`、
  `src/agent/compact.rs:192` `format_history`。
- goose 参考（commit `4ad43df`）：
  - `agents/platform_extensions/developer/image.rs`：`MAX_IMAGE_BYTES = 20MB`；
    支持 png/jpeg/gif/webp（`image::guess_format` 按**内容**判格式，不看扩展名）；
    工具结果 = 一条摘要文本 + 一个 image block；
  - `goose-provider-types/src/images.rs` `convert_image`：OpenAI wire 形状
    `{"type":"image_url","image_url":{"url":"data:{mime};base64,{data}"}}`
    （Anthropic 形状随 ADR 0001 不再搬运）；
  - `formats/openai.rs`：OpenAI 的 `tool` 角色消息**不能带图片**，goose 在 tool
    消息里放占位文本、图片挪进紧随其后的 user 消息。
- 依赖锁定（`todos/01`）：**没有** `base64` / `image` / `mime_guess` crate。
  base64 编码需手写（约 15 行）；格式判定用魔数嗅探（`image::guess_format` 的
  无依赖等价物）。

## 目标

- 新内置工具 `read_image`：读本地文件，20MB 上限（对齐 goose），
  png/jpeg/gif/webp 之外的文件报清晰错误，`read_only = true`。
- `Content::Image` 新变体：base64 data + media_type，一次编码。
- openai 引擎支持序列化图片（anthropic 引擎已按 ADR 0001 移除，不在本项范围）。
- 会话不变量（`src/message.rs` 四条）不被破坏；压缩不把 base64 灌进摘要器；
  旧会话文件 100% 可读。

## 非目标

- **不做 URL / 剪贴板图片**：默认只做本地文件。goose 的 `source` 参数兼收
  http(s) URL，但那是 `url` + `reqwest` 下载 + 30s 超时一整套；instagent 的
  `read`/`write` 也都是纯本地路径语义，保持一致。模型要网络图片可用 shell 下载
  后再 read_image。
- **不做 crop**：goose 的 `CropParams` 依赖 `image` crate 解码重编码，依赖锁定下
  无法实现，直接砍掉。
- **不做图片解码 / 尺寸读取**：没有 `image` crate，摘要文本里没有宽高
  （只有字节数与 media type）。
- **不做 vision 能力探测**（goose openai 格式的 `supports_vision` 开关）：
  不支持图片的模型由 provider 报错兜底，不预置配置项，YAGNI。
- **不支持 MCP 插件返回图片**：`McpSource::call` 把结果折叠成文本
  （`src/tools/mcp.rs`），`ToolOutput.image` 只有内置来源会填。goose 里 MCP
  image block 的转发不在本次范围。
- 不动 `docs/goose-from-scratch-plan.md` §7 表（roadmap 文档不回改）。

## 方案

### 数据模型（`src/message.rs`、`src/tools/mod.rs`）

```rust
// src/tools/mod.rs（新增，被 message 复用；消息层已依赖工具层，无环）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageData {
    /// base64 标准字母表编码的原始字节。
    pub data: String,
    /// image/png | image/jpeg | image/gif | image/webp。
    pub media_type: String,
}

// ToolOutput 增加可选字段（#[serde(default)]，JSON 形状向后兼容）
pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageData>,
}

// src/message.rs：第四变体（untagged 追加在末尾）
pub enum Content {
    Text(String),
    ToolUse { .. },       // 不变
    ToolResult { .. },    // 不变
    Image(ImageData),
}
```

**untagged 兼容性论证**：序列化形状是 `{"data": "...", "media_type": "..."}`。
反序列化按声明顺序试探——`Text` 要字符串、`ToolUse` 要 `id/name/input`、
`ToolResult` 要 `tool_use_id/content/is_error`，旧 JSONL 的任何行都不会误配到
`Image`；`data/media_type` 也不与任何现有字段冲突。

**为什么不把图片塞进 `ToolResult`**：instagent 的 `ToolResult.content` 是
`String`，扩成块数组会动会话模型与全部序列化点；而 OpenAI 的 `tool` 消息本就
不能带图片（见下），独立 `Image` 块放在同一条 user 消息里改动最小。

### read_image 工具（新文件 `src/tools/builtin/image.rs` + `builtin/mod.rs` 注册）

- **输入**：`{"path": string}`（必填，与 `read` 工具的 `path` 参数同名同义）。
- **路径解析**：复用 `fs::resolve_path`（相对路径按会话目录，`src/tools/builtin/fs.rs:26`）。
- **流程与错误**（全部 `ToolOutput::err`，不 panic、不 anyhow）：
  | 情况 | 行为 |
  |---|---|
  | 文件不存在 / 无权限 | `Failed to read {path}: {io error}`（对齐 `read` 工具措辞） |
  | 超过 20MB | `image is too large: {len} bytes exceeds 20971520 byte limit`（goose 同文案同上限，`MAX_IMAGE_BYTES = 20 * 1024 * 1024`）——先查 `metadata().len()`，读取再兜一次（稀疏文件防御，对齐 goose `read_bounded`） |
  | 魔数不匹配 4 种格式 | `unsupported image format; supported formats are png, jpeg, gif, and webp`（goose 原文案） |
- **成功输出**：`text = "Loaded image from {path} ({bytes} bytes, {media_type})."`
  （goose `LoadedImage::summary` 去掉宽高与 crop 注记），
  `image = Some(ImageData { data: base64, media_type })`。
- **格式判定用魔数嗅探，不看扩展名**（任务约束说"按扩展名 match"，这里选更准的
  等价方案并说明理由）：`image` crate 不可用，魔数是 `guess_format` 的无依赖等价
  物，约 15 行纯函数，且扩展名说谎（`x.bin` 里的 png、无扩展名文件）时仍然正确：
  ```text
  png : 89 50 4E 47 ..
  jpeg: FF D8 FF
  gif : "GIF87a" / "GIF89a"
  webp: "RIFF" ++ 4 字节长度 ++ "WEBP"
  ```
- **base64 解法（依赖锁定的核心应对）**：手写编码器，约 15 行（3 字节一组 →
  4 个 6 位索引，`=` 补齐），放在 `image.rs` 内私有。不加 `base64` 依赖。
  不需要解码器——测试用已知答案（RFC 4648 向量 + 1x1 PNG 常量对）。
- **注册**：`builtin/mod.rs` `list()` 加第 6 个 spec（描述搬运改写自 goose
  developer 的 image 工具：本地图片、格式与大小限制、"returns the image so you
  can see its content"），`read_only = true`；`call()` 加 `read_image` 分发臂。
  现有测试 `builtin_lists_five_tools_unprefixed`（断言恰好 5 个工具）同步改成 6 个。

### loop 接线（`src/agent/mod.rs` `execute_calls`）

`execute_calls` 末尾（现 `:318`）：

```rust
let image = output.image.take();            // 先取出（output 需 mut）
results.push(Content::tool_result(call, output));
if let Some(img) = image {
    results.push(Content::Image(img));      // 与 ToolResult 同一条 user 消息
}
```

- 不变量 2 不受影响（校验只看 ToolUse/ToolResult 的 id 配对，`Image` 块被
  `validate` 天然忽略）；不变量 3 不受影响（`Image` 不是文本块）。
- `finish_turn` 原样工作（`results` 非空即整组落盘）。
- ToolDone 预览 / PostToolUse hook 只经 `output.text`，无需改。

### OpenAI 引擎（`src/provider/openai.rs` `format_messages`）

关键约束（调研所得）：**OpenAI `tool` 角色消息只能带字符串**。图片只能进 user
消息。现 user 分支把 texts 拼成单字符串；改为：

- user 消息**无** `Image` → 行为逐字不变（`{"role":"user","content": "..."}`）；
- **有** `Image` → `content` 改为部分数组：每个文本一个
  `{"type":"text","text":...}`，每张图片一个
  `{"type":"image_url","image_url":{"url":"data:{media_type};base64,{data}"}}`
  （goose `convert_image` OpenAI 形状）；只有图片没有文本时也发该 user 消息。
- 顺序仍是 `results（tool 消息）→ user 消息`，怪癖 3（tool 消息紧跟 assistant）
  不被破坏。
- assistant 分支加 `Content::Image(_) => {}` 忽略臂（模型流不会产出图片，仅保穷举）。
- 不做 media type 白名单降级——我们只生产 4 种合法类型（MCP 图片是非目标），
  不为不可能出现的输入写防御分支。

openai 引擎服务 openai/ollama/openrouter/deepseek 一大票网关，`image_url` data
URL 是 Chat Completions 标准形状，改动集中在一个函数的一个分支。
anthropic 引擎已移除（ADR 0001），将来若恢复原生引擎再补映射。

### 压缩（`src/agent/compact.rs` `format_history`）

必须处理，否则新变体让穷举编译不过，且**绝不能把 base64 灌进摘要器**
（一张 20MB 图 ≈ 27MB base64，摘要请求直接爆）。加一臂输出占位：

```text
{role}: [image: image/png, {data.len()} base64 bytes omitted]
```

`split_head_tail` 不受影响（只认 `Content::Text` 开头的未答复 user 消息，
图片只会出现在已答复的工具结果消息里）。压缩后图片从上下文消失——与所有被压缩
内容同语义，摘要里的占位描述保留"看过一张图"的事实。

### 会话兼容（`src/session.rs`，不改代码）

- **新读旧**：保证。新变体只由新代码写出，旧 JSONL 没有 `{"data",...}` 行。
- **旧读新**（旧二进制打开含图片的新会话）：图片行 parse 失败，走
  `resume` 的 salvage（`src/session.rs:96`）——截断到第一条坏行前、再回退到最近
  合法前缀并警告丢弃行数。**代价**：图片行之后的历史一并被旧二进制丢弃。这是
  单向升级的已知代价，写入风险节，不为此改 salvage（v2 候选天然只向前）。
- **体积**：图片以约 1.37 倍原始大小的 base64 行进 JSONL（20MB 图 ≈ 27MB 行）。
  `serde_json` 按行解析可承受；写风险节提醒。

## 拆解

任务队列（依赖列"无"即可并行；总量约 315 行，对齐原表"约 300 行"）：

| # | 任务 | 依赖 | 涉及文件 | 估算 |
|---|---|---|---|---|
| 1 | 数据模型：`ImageData`、`ToolOutput.image`、`Content::Image` + serde round-trip 测试 | 无 | `src/tools/mod.rs`、`src/message.rs` | ~40 行 |
| 2 | `read_image` 工具：base64 编码器、魔数嗅探、读取/校验/错误、spec 与分发注册、既有 5 工具测试改 6 | 1 | 新文件 `src/tools/builtin/image.rs`，`src/tools/builtin/mod.rs` | ~200 行 |
| 3 | loop 接线：`execute_calls` 拆出 Image 块 + loop 级测试（read_image 成功后会话含 Image、不变量校验通过） | 1 | `src/agent/mod.rs` | ~35 行 |
| 4 | OpenAI 引擎序列化 + 测试（含图时 content 数组形状、无图时行为不变、怪癖 3 顺序保持） | 1 | `src/provider/openai.rs` | ~35 行 |
| 5 | `format_history` 图片占位 + 测试（base64 不进摘要文本） | 1 | `src/agent/compact.rs` | ~12 行 |

执行顺序：1 先行，2–5 四路可并行。任务 2 的测试夹具用 1x1 PNG（68 字节，
与 goose `image.rs` 测试同一张，其 base64 即
`iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=`），
字节数组与 base64 串双双硬编码为常量做已知答案测试，**不需要解码器**。

不改的东西（明确清单之外一律不动）：`src/lib.rs` 模块树（新文件是 `builtin` 的
子模块声明，加在 `src/tools/builtin/mod.rs`，不动 `lib.rs`）、`Cargo.toml`
（零依赖变更）、`src/session.rs`、`src/cli/`、`todos/`。

## 校验

仓库级命令（每个任务完成后全部执行）：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

各组新增测试的验收方式：

- **任务 1**：`Content::Image` JSONL round-trip（现有 `content_jsonl_round_trip`
  追加样本）；旧形状三变体反序列化结果不变（untagged 顺序回归）。
- **任务 2**：已知答案测 base64（RFC 4648 向量 `""`/`f`/`fo`/`foo`/`foob`/
  `fooba`/`foobar` + 1x1 PNG 常量对）；魔数嗅探 4 格式 + 拒绝（`.bin` 伪图片，
  对齐 goose 测试）；超限文件拒绝（稀疏文件 `set_len(20MB+1)`，对齐
  `fs.rs:493` 的手法）；不存在文件 / 格式错误的文案断言；经 `Registry::call`
  的 `read_image` 端到端（真实临时文件）。
- **任务 3**：`execute_calls` 后 user 消息 = `[ToolResult, Image]` 且
  `message::validate` 通过；无图片的工具调用行为逐字不变。
- **任务 4**：含图 user 消息 → `content` 是数组（text + `image_url` data URL）；
  无图 → 与现有 `quirk3_*` 断言逐字一致（回归）；`tool` 消息仍紧跟 assistant。
- **任务 5**：`format_history` 对含图历史输出占位、**不含** base64 子串
  （断言 `!history.contains(&img.data)`）。

最终验收：三条命令全绿 + 既有测试零回归。

## 风险与假设

- **风险**：旧二进制读含图片的新会话，第一张图片之后的历史被 salvage 丢弃
  （单向升级代价，已述）。
- **风险**：会话文件体积膨胀（20MB 图 ≈ 27MB 行）；`/compact` 后图片消失只留
  占位描述。均为已知取舍。
- **风险**：各家网关单图限制不一（OpenAI data URL 约 10MB 量级），20MB 上限沿用
  goose，超大图会得到 provider 400——错误经现有 `map_http_error` 正常呈现，不特判。
- **假设**：不支持视觉的模型收到 `image_url` 会报错而非静默忽略；
  不做能力探测（非目标），报错即反馈。
- **假设**：MCP / command 插件工具不产出图片（`ToolOutput.image` 恒 None）；
  其图片支持需要 rmcp `ContentBlock::Image` 的搬运，另开任务。
- 基线确认：改动面全部在列明文件内，`src/lib.rs` 模块树与 `Cargo.toml`
  依赖清单（`todos/01` 锁定）零变更。
