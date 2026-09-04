//! read_image 工具（图片支持方案 §read_image 工具）。
//!
//! 上限、大小文案与读取防御对齐 goose `developer/image.rs`（commit `4ad43df`）
//! 的 `MAX_IMAGE_BYTES` / `ensure_image_size` / `read_bounded`；摘要文案是其
//! `LoadedImage::summary` 去掉宽高；格式判定用魔数嗅探（`image::guess_format`
//! 的无依赖等价物，不看扩展名）；base64 手写（依赖锁定，不加 base64 crate）。
//!
//! 校验边界（计划 S14 / todo 04）：短期只做魔数 MIME 判定、长度与
//! [`MAX_IMAGE_BYTES`] payload 预算（含 `take(MAX+1)` 读后兜底防御
//! metadata/read 增长竞态）；结构化解码与像素/压缩炸弹预算属于 RM2，
//! 不在本任务引入未批准的 decoder 依赖。摘要文本带原始字节数与 base64
//! 负载字节数，为后续 agent/provider 层落实单请求/单会话图片总预算
//! （计划 S15 / todo 13）提供工具层可观测的大小信息。阻塞读取经
//! [`super::fs::spawn_tool`] 移出 current-thread async 路径（计划 R1）。

use std::io::Read;
use std::path::Path;

use tokio_util::sync::CancellationToken;

use crate::tools::ImageData;
use crate::tools::ToolCtx;
use crate::tools::ToolOutput;

use super::fs;

/// read_image 拒绝超过此字节数的文件（搬运 goose developer/image.rs:14）。
pub(crate) const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// base64 标准字母表编码（RFC 4648）：3 字节一组 → 4 个 6 位索引，`=` 补齐。
fn b64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(char::from(B64_ALPHABET[(n >> 18) as usize & 0x3f]));
        out.push(char::from(B64_ALPHABET[(n >> 12) as usize & 0x3f]));
        match chunk.len() {
            3 => {
                out.push(char::from(B64_ALPHABET[(n >> 6) as usize & 0x3f]));
                out.push(char::from(B64_ALPHABET[n as usize & 0x3f]));
            }
            2 => {
                out.push(char::from(B64_ALPHABET[(n >> 6) as usize & 0x3f]));
                out.push('=');
            }
            _ => out.push_str("=="),
        }
    }
    out
}

/// 魔数嗅探判格式，不看扩展名（`image::guess_format` 的无依赖等价物）。
fn sniff_format(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, 0x50, 0x4e, 0x47]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && bytes[8..12] == *b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn too_large(len: u64) -> ToolOutput {
    ToolOutput::err(format!(
        "image is too large: {len} bytes exceeds {MAX_IMAGE_BYTES} byte limit"
    ))
}

/// 读本地图片文件：先 `metadata().len()` 拦截，读取后再兜一次（稀疏文件防御，
/// 对齐 goose `read_bounded`），魔数嗅探判格式，产出 `ToolOutput.image`。
pub async fn read_image(path: &Path, ctx: &ToolCtx) -> ToolOutput {
    let full = fs::resolve_path(path, &ctx.cwd);
    let cancel = ctx.cancel.clone();
    fs::spawn_tool(move || read_image_sync(&full, &cancel)).await
}

fn read_image_sync(full: &Path, cancel: &CancellationToken) -> ToolOutput {
    if cancel.is_cancelled() {
        return fs::cancelled_output(full);
    }
    let fail =
        |e: std::io::Error| ToolOutput::err(format!("Failed to read {}: {e}", full.display()));

    let len = match std::fs::metadata(full) {
        Ok(meta) => meta.len(),
        Err(e) => return fail(e),
    };
    if len > MAX_IMAGE_BYTES {
        return too_large(len);
    }

    let file = match std::fs::File::open(full) {
        Ok(file) => file,
        Err(e) => return fail(e),
    };
    let mut bytes = Vec::new();
    if let Err(e) = file.take(MAX_IMAGE_BYTES + 1).read_to_end(&mut bytes) {
        return fail(e);
    }
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return too_large(bytes.len() as u64);
    }

    let Some(media_type) = sniff_format(&bytes) else {
        return ToolOutput::err(
            "unsupported image format; supported formats are png, jpeg, gif, and webp".to_string(),
        );
    };

    let data = b64(&bytes);
    let mut out = ToolOutput::ok(format!(
        "Loaded image from {} ({} bytes, {media_type}, {} bytes base64).",
        full.display(),
        bytes.len(),
        data.len()
    ));
    out.image = Some(ImageData {
        data,
        media_type: media_type.to_string(),
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Registry;
    use crate::tools::ToolCall;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    /// 1x1 PNG（与 goose developer/image.rs 测试同一张），字节与 base64 双硬编码。
    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5,
        0x1c, 0x0c, 0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64,
        0xf8, 0x0f, 0x00, 0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    const PNG_1X1_B64: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    fn ctx(dir: &Path) -> ToolCtx {
        ToolCtx {
            cwd: dir.to_path_buf(),
            cancel: CancellationToken::new(),
        }
    }

    fn registry() -> Registry {
        let mut registry = Registry::new();
        registry.register(std::sync::Arc::new(crate::tools::BuiltinTools::new(None)));
        registry
    }

    async fn call_read_image(registry: &Registry, ctx: &ToolCtx, path: &str) -> ToolOutput {
        registry
            .call(
                &ToolCall {
                    id: "t".into(),
                    name: "read_image".into(),
                    input: json!({"path": path}),
                },
                ctx,
            )
            .await
    }

    #[test]
    fn b64_matches_rfc4648_vectors() {
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(b"foob"), "Zm9vYg==");
        assert_eq!(b64(b"fooba"), "Zm9vYmE=");
        assert_eq!(b64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn b64_encodes_1x1_png() {
        assert_eq!(b64(PNG_1X1), PNG_1X1_B64);
    }

    #[test]
    fn sniff_detects_four_formats_and_rejects_non_image() {
        assert_eq!(
            sniff_format(&[0x89, 0x50, 0x4e, 0x47, 0, 0]),
            Some("image/png")
        );
        assert_eq!(sniff_format(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        assert_eq!(sniff_format(b"GIF87a..."), Some("image/gif"));
        assert_eq!(sniff_format(b"GIF89a..."), Some("image/gif"));
        assert_eq!(
            sniff_format(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
            Some("image/webp")
        );

        assert_eq!(sniff_format(b"not an image"), None);
        assert_eq!(sniff_format(b""), None);
        assert_eq!(sniff_format(b"RIFF\x00\x00\x00\x00WAVEfmt "), None);
    }

    #[tokio::test]
    async fn read_image_returns_summary_and_base64_image() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.png");
        std::fs::write(&file, PNG_1X1).unwrap();

        let out = call_read_image(&registry(), &ctx(dir.path()), "a.png").await;
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(
            out.text,
            format!(
                "Loaded image from {} (68 bytes, image/png, {} bytes base64).",
                file.display(),
                PNG_1X1_B64.len()
            ),
            "摘要带原始字节数与 base64 负载字节数（供后续总预算任务观测）"
        );
        let image = out.image.expect("image payload");
        assert_eq!(image.media_type, "image/png");
        assert_eq!(image.data, PNG_1X1_B64);
    }

    #[tokio::test]
    async fn read_image_sniffs_content_not_extension() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.bin");
        std::fs::write(&file, PNG_1X1).unwrap();

        let out = call_read_image(&registry(), &ctx(dir.path()), "x.bin").await;
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(out.image.unwrap().media_type, "image/png");
    }

    #[tokio::test]
    async fn read_image_rejects_non_image_text_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.png"), b"not an image").unwrap();

        let out = call_read_image(&registry(), &ctx(dir.path()), "a.png").await;
        assert!(out.is_error);
        assert_eq!(
            out.text,
            "unsupported image format; supported formats are png, jpeg, gif, and webp"
        );
        assert!(out.image.is_none());
    }

    #[tokio::test]
    async fn read_image_missing_file_reports_failed_to_read() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.png");

        let out = call_read_image(&registry(), &ctx(dir.path()), "missing.png").await;
        assert!(out.is_error);
        assert!(
            out.text
                .starts_with(&format!("Failed to read {}:", missing.display())),
            "{}",
            out.text
        );
    }

    #[tokio::test]
    async fn read_image_rejects_oversized_sparse_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("huge.png");
        let f = std::fs::File::create(&file).unwrap();
        // 稀疏文件：metadata 报告超大长度，读取前就被字节上限拦截。
        f.set_len(MAX_IMAGE_BYTES + 1).unwrap();

        let out = call_read_image(&registry(), &ctx(dir.path()), "huge.png").await;
        assert!(out.is_error);
        assert_eq!(
            out.text,
            format!(
                "image is too large: {} bytes exceeds {MAX_IMAGE_BYTES} byte limit",
                MAX_IMAGE_BYTES + 1
            )
        );
    }

    #[tokio::test]
    async fn read_image_rejects_truncated_magic_bytes() {
        let dir = tempfile::tempdir().unwrap();
        // 只有 2 个字节的前缀：不够任何魔数，按内容拒绝。
        std::fs::write(dir.path().join("trunc.png"), [0x89u8, 0x50]).unwrap();

        let out = call_read_image(&registry(), &ctx(dir.path()), "trunc.png").await;
        assert!(out.is_error);
        assert!(
            out.text.contains("unsupported image format"),
            "{}",
            out.text
        );
        assert!(out.image.is_none());
    }

    #[tokio::test]
    async fn read_image_precancelled_returns_cancelled_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.png"), PNG_1X1).unwrap();
        let token = CancellationToken::new();
        token.cancel();
        let ctx = ToolCtx {
            cwd: dir.path().to_path_buf(),
            cancel: token,
        };

        let out = call_read_image(&registry(), &ctx, "a.png").await;
        assert!(out.is_error);
        assert!(out.text.contains("cancelled"), "{}", out.text);
        assert!(out.image.is_none());
    }
}
