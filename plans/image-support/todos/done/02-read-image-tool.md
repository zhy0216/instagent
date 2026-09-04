difficulty: hard

# 02 read_image 工具（base64 + 魔数嗅探 + 注册）

按 `plans/image-support/plan.md` "read_image 工具"节新增内置工具。参考
goose `~/yyds/goose`（commit `4ad43df`）
`agents/platform_extensions/developer/image.rs` 与
`goose-provider-types/src/images.rs`（只读参考；成段搬运在 commit message
注明出处）。依赖 01 的
`ImageData` 与 `ToolOutput.image`。零依赖变更：base64 手写、格式判定用
魔数，不加 `base64` / `image` / `mime_guess` crate。

## T1 · 新文件 src/tools/builtin/image.rs

- 要做什么：
  - 私有 `fn b64(bytes: &[u8]) -> String`：标准字母表
    `A–Za–z0–9+/`，3 字节一组 → 4 索引，`=` 补齐（约 15 行，不需要
    解码器）。
  - 私有 `fn sniff_format(bytes: &[u8]) -> Option<&'static str>`：魔数
    判定不看扩展名——png `89 50 4E 47`、jpeg `FF D8 FF`、gif
    `"GIF87a"`/`"GIF89a"`、webp `"RIFF" + 4 字节 + "WEBP"`；返回
    `"image/png"` 等 media type。
  - `pub(crate) const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024`。
  - 工具入口（与同目录 fs.rs / shell.rs 的既有内置工具写法一致）：
    输入 `{"path": string}` 必填；路径用 `fs::resolve_path`
    （`src/tools/builtin/fs.rs:26`）按会话目录解析；流程与错误文案
    严格照方案的表：
    - 读失败：`Failed to read {path}: {io error}`；
    - 超限：先 `metadata().len()` 后读取再兜一次（稀疏文件防御，对齐
      goose `read_bounded`），文案
      `image is too large: {len} bytes exceeds 20971520 byte limit`；
    - 魔数不匹配：
      `unsupported image format; supported formats are png, jpeg, gif, and webp`；
    - 成功：`text = "Loaded image from {path} ({bytes} bytes, {media_type})."`，
      `image = Some(ImageData { data: b64(bytes), media_type })`。
    - 全部错误走 `ToolOutput::err`，不 panic、不 anyhow。
- 预计修改：新文件 `src/tools/builtin/image.rs`。
- 验收：T4 测试全绿。
- 前置依赖：01。

## T2 · 注册（builtin/mod.rs）

- 要做什么：
  - `src/tools/builtin/mod.rs` 加 `mod image;`（不动 `src/lib.rs`）；
  - `list()` 加第 6 个 spec：名 `read_image`，`read_only = true`，
    input schema `{"path"}`，描述搬运改写 goose developer image 工具
    （本地图片、支持格式、20MB 上限、"returns the image so you can
    see its content"）；
  - `call()` 加 `read_image` 分发臂。
- 预计修改：`src/tools/builtin/mod.rs`。
- 验收：`read_image` 出现在 registry `list()` 中且无 `builtin_` 前缀。
- 前置依赖：T1。

## T3 · 既有测试改 6 工具

- 要做什么：`src/tools/builtin/mod.rs:245`
  `builtin_lists_five_tools_unprefixed` 改名（如
  `builtin_lists_six_tools_unprefixed`）并把断言从 5 改为 6，列表期望补
  `read_image`；同文件其他按数量/名单断言的测试同步。
- 预计修改：`src/tools/builtin/mod.rs`（测试模块）。
- 验收：`cargo test` 绿。
- 前置依赖：T2。

## T4 · 测试（image.rs 内）

- 要做什么：已知答案测 `b64`：RFC 4648 向量
  `""→""`、`f→Zg==`、`fo→Zm8=`、`foo→Zm9v`、`foob→Zm9vYg==`、
  `fooba→Zm9vYmE=`、`foobar→Zm9vYmFy` + 1x1 PNG 字节数组 ↔
  `iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=`
  （与 goose image.rs 测试同一张图，字节与 base64 双双硬编码，无需解码器）。
  魔数嗅探：4 格式识别 + 非图片字节拒绝。经 `Registry::call` 端到端
  （真实临时文件）：1x1 PNG 成功（断言 text 文案、media_type、data 等于
  已知 base64）；`.bin` 伪装 png 内容→成功（嗅探看内容）；真文本文件→
  unsupported 文案；不存在文件→`Failed to read` 文案；稀疏文件
  `set_len(MAX+1)`→too large 文案（手法对齐 `fs.rs:493` 的既有测试）。
- 预计修改：`src/tools/builtin/image.rs`（测试模块）。
- 验收：`cargo fmt --check && cargo clippy --all-targets -- -D warnings &&
  cargo test` 全绿，既有测试零回归。
- 前置依赖：T1、T2。
