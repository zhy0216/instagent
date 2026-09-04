//! read / write / edit 工具（第二版 §2.4）。
//!
//! edit 的匹配语义与相近上下文从 goose `developer/edit.rs:157~286`
//! （commit `4ad43df`）的 `string_replace` / `count_lines_before` /
//! `get_line_context` / `find_similar_context` / `build_file_preview`
//! 与 `resolve_path` 精简搬运（本文件标注处注明）。
//!
//! 安全边界（ADR 0003 D6）：强制隔离层是外部 sandbox，本文件只承诺这些
//! 应用层纵深防御——**最终目标** symlink 拒绝写入、读/写字节上限、取消检查。
//! 中间路径 symlink 与绝对路径按当前语义解析（相对路径按会话目录），
//! `write linkdir/file` 可经 cwd 内的链接写到外部；这是 sandbox 的责任面，
//! 不要把这些工具描述成"路径安全"。全路径 containment 走 RM5 另立决策。
//!
//! 阻塞模型（计划 R1）：所有同步文件系统操作经 [`spawn_tool`] 放到
//! tokio blocking 线程池，不占 current-thread runtime 的事件循环；
//! 长循环按行/按块检查取消令牌。

use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;

use tokio_util::sync::CancellationToken;

use crate::tools::ToolCtx;
use crate::tools::ToolOutput;

/// read 默认最多 2000 行。
pub const READ_DEFAULT_LIMIT: u32 = 2000;

/// read / edit 拒绝超过此字节数的文件。
pub const MAX_READ_BYTES: u64 = 32 * 1024 * 1024;

/// write 拒绝超过此字节数的内容（计划 R9：write 不再接受无界模型内容）。
pub const MAX_WRITE_BYTES: u64 = 32 * 1024 * 1024;

/// read 循环中取消检查的行间隔（上界 = 4096 行或剩余字节上限的读取时间）。
const READ_CANCEL_CHECK_LINES: usize = 4096;

/// 统一的取消输出：工具层把"被取消"报告为带路径的可诊断错误。
pub fn cancelled_output(path: &Path) -> ToolOutput {
    ToolOutput::err(format!("cancelled: {}", path.display()))
}

/// 把阻塞的工具逻辑放到 tokio blocking 线程池（计划 R1：同步文件系统、
/// 目录遍历等移出 current-thread async 路径）。blocking 任务异常终止时
/// 返回可诊断错误而不是 panic。
pub async fn spawn_tool<F>(work: F) -> ToolOutput
where
    F: FnOnce() -> ToolOutput + Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(out) => out,
        Err(err) => ToolOutput::err(format!("tool task aborted: {err}")),
    }
}

/// 超限文案统一格式（带路径、实际大小、上限）。
fn too_large_message(path: &Path, len: u64, limit: u64) -> String {
    format!(
        "Failed to read {}: file too large ({} bytes, limit {} bytes); use shell tools (head/grep) to inspect it",
        path.display(),
        len,
        limit
    )
}

/// 有界整读（计划 R9）：metadata 预检 + `take(MAX+1)` 读后兜底。
/// 兜底针对 metadata/read 增长竞态——文件在预检通过后继续增长时，
/// 最多读 `MAX_READ_BYTES + 1` 字节就报错，而不是无界读入内存。
fn read_capped_text(full: &Path) -> Result<String, String> {
    let fail = |e: std::io::Error| format!("Failed to read {}: {e}", full.display());
    let meta = std::fs::metadata(full).map_err(fail)?;
    if meta.len() > MAX_READ_BYTES {
        return Err(too_large_message(full, meta.len(), MAX_READ_BYTES));
    }
    let file = std::fs::File::open(full).map_err(fail)?;
    let mut buf = Vec::new();
    file.take(MAX_READ_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(fail)?;
    if buf.len() as u64 > MAX_READ_BYTES {
        return Err(format!(
            "Failed to read {}: file grew past {} bytes during read (limit {} bytes)",
            full.display(),
            MAX_READ_BYTES,
            MAX_READ_BYTES
        ));
    }
    String::from_utf8(buf).map_err(|_| {
        format!(
            "Failed to read {}: not valid UTF-8 (binary file?)",
            full.display()
        )
    })
}

/// 无匹配时给的文件预览行数（goose edit.rs:8）。
const NO_MATCH_PREVIEW_LINES: usize = 20;

/// 相对路径按会话目录解析（搬运 goose developer/edit.rs:215 `resolve_path`）。
pub fn resolve_path(path: &Path, working_dir: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        working_dir.join(path)
    }
}

/// 带行号输出（read 标 read_only = true）。
///
/// 流式按行读取：只把窗口行留在内存，窗口之后的行仅计数，
/// 保证 "(N more lines)" 与旧行为一致而不整读文件。
pub async fn read_file(
    path: &Path,
    line: Option<u32>,
    limit: Option<u32>,
    ctx: &ToolCtx,
) -> ToolOutput {
    if line == Some(0) {
        return ToolOutput::err(
            "`line` is 1-based, got line=0; use line=1 for the first line".to_string(),
        );
    }
    if limit == Some(0) {
        return ToolOutput::err(
            "`limit` must be positive, got limit=0; omit it for the default window".to_string(),
        );
    }
    let full = resolve_path(path, &ctx.cwd);
    let cancel = ctx.cancel.clone();
    spawn_tool(move || read_file_sync(&full, line, limit, &cancel)).await
}

fn read_file_sync(
    full: &Path,
    line: Option<u32>,
    limit: Option<u32>,
    cancel: &CancellationToken,
) -> ToolOutput {
    if cancel.is_cancelled() {
        return cancelled_output(full);
    }
    let fail =
        |e: std::io::Error| ToolOutput::err(format!("Failed to read {}: {e}", full.display()));

    let meta = match std::fs::metadata(full) {
        Ok(meta) => meta,
        Err(e) => return fail(e),
    };
    if meta.len() > MAX_READ_BYTES {
        return ToolOutput::err(too_large_message(full, meta.len(), MAX_READ_BYTES));
    }
    let file = match std::fs::File::open(full) {
        Ok(file) => file,
        Err(e) => return fail(e),
    };

    let start = line.unwrap_or(1) as usize - 1;
    let count = limit.unwrap_or(READ_DEFAULT_LIMIT) as usize;
    let mut text = String::new();
    let mut total = 0usize;
    let mut bytes_seen = 0u64;
    let mut grew_past_limit = false;
    for (i, line) in BufReader::new(file).lines().enumerate() {
        if i % READ_CANCEL_CHECK_LINES == 0 && cancel.is_cancelled() {
            return cancelled_output(full);
        }
        let file_line = match line {
            Ok(line) => line,
            Err(e) => return fail(e),
        };
        total = i + 1;
        bytes_seen = bytes_seen.saturating_add(file_line.len() as u64 + 1);
        if i >= start && i - start < count {
            text.push_str(&format!("{:>4}: {}\n", i + 1, file_line));
        }
        // metadata/read 增长竞态：metadata 预检通过后文件又增长时及时止损，
        // 不把无界增长的行一直数下去。
        if bytes_seen > MAX_READ_BYTES {
            grew_past_limit = true;
            break;
        }
    }

    if total == 0 {
        return ToolOutput::ok(format!("{}: (file is empty)", full.display()));
    }
    if start >= total {
        return ToolOutput::err(format!(
            "Line {} is past end of file ({total} lines total)",
            start + 1
        ));
    }
    if grew_past_limit {
        text.push_str(&format!(
            "... (stopped: file grew past {MAX_READ_BYTES} bytes during read; total line count is incomplete)\n"
        ));
    } else if total > start + count {
        text.push_str(&format!("... ({} more lines)\n", total - start - count));
    }
    ToolOutput::ok(text)
}

/// 写路径符号链接防护：目标是符号链接时拒绝（不跟随、不写穿）。
fn reject_symlink(full: &Path) -> Option<ToolOutput> {
    match std::fs::symlink_metadata(full) {
        Ok(meta) if meta.file_type().is_symlink() => Some(ToolOutput::err(format!(
            "Refusing to write through symlink: {} (write the link target directly instead)",
            full.display()
        ))),
        _ => None,
    }
}

/// 同目录临时文件 + rename 原子替换，失败路径清理临时文件。
/// 跨设备 rename 会失败，所以临时文件不放系统临时目录。
fn atomic_write(full: &Path, content: &str) -> std::io::Result<()> {
    let Some(name) = full.file_name() else {
        return std::fs::write(full, content);
    };
    let tmp = full.with_file_name(format!(
        "{}.{}.tmp",
        name.to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    match std::fs::write(&tmp, content).and_then(|()| std::fs::rename(&tmp, full)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// 建父目录，原子覆盖写。内容有 [`MAX_WRITE_BYTES`] 上限（计划 R9）。
pub async fn write_file(path: &Path, content: &str, ctx: &ToolCtx) -> ToolOutput {
    let full = resolve_path(path, &ctx.cwd);
    let content = content.to_string();
    let cancel = ctx.cancel.clone();
    spawn_tool(move || write_file_sync(&full, &content, &cancel)).await
}

fn write_file_sync(full: &Path, content: &str, cancel: &CancellationToken) -> ToolOutput {
    if cancel.is_cancelled() {
        return cancelled_output(full);
    }
    if content.len() as u64 > MAX_WRITE_BYTES {
        return ToolOutput::err(format!(
            "Failed to write {}: content too large ({} bytes, limit {} bytes)",
            full.display(),
            content.len(),
            MAX_WRITE_BYTES
        ));
    }
    if let Some(out) = reject_symlink(full) {
        return out;
    }
    if let Some(parent) = full.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return ToolOutput::err(format!(
                "Failed to create parent directory {}: {e}",
                parent.display()
            ));
        }
    }
    match atomic_write(full, content) {
        Ok(()) => ToolOutput::ok(format!(
            "Wrote {} bytes to {}",
            content.len(),
            full.display()
        )),
        Err(e) => ToolOutput::err(format!("Failed to write {}: {e}", full.display())),
    }
}

/// `before` 必须唯一精确匹配，否则报错并给出匹配次数和相近上下文；
/// `after` 为空即删除。写回走原子替换。整读经 `read_capped_text`
/// 受 [`MAX_READ_BYTES`] 约束（此前无上限，计划 R9）。
pub async fn edit_file(path: &Path, before: &str, after: &str, ctx: &ToolCtx) -> ToolOutput {
    let full = resolve_path(path, &ctx.cwd);
    let before = before.to_string();
    let after = after.to_string();
    let cancel = ctx.cancel.clone();
    spawn_tool(move || edit_file_sync(&full, &before, &after, &cancel)).await
}

fn edit_file_sync(
    full: &Path,
    before: &str,
    after: &str,
    cancel: &CancellationToken,
) -> ToolOutput {
    if cancel.is_cancelled() {
        return cancelled_output(full);
    }
    if let Some(out) = reject_symlink(full) {
        return out;
    }
    let content = match read_capped_text(full) {
        Ok(content) => content,
        Err(message) => return ToolOutput::err(message),
    };
    if cancel.is_cancelled() {
        return cancelled_output(full);
    }

    match string_replace(&content, before, after) {
        Ok(new_content) => match atomic_write(full, &new_content) {
            Ok(()) => {
                let old_lines = content.lines().count();
                let new_lines = new_content.lines().count();
                ToolOutput::ok(format!(
                    "Successfully replaced content in {} ({} lines -> {} lines)",
                    full.display(),
                    old_lines,
                    new_lines
                ))
            }
            Err(e) => ToolOutput::err(format!("Failed to write {}: {e}", full.display())),
        },
        Err(message) => ToolOutput::err(message),
    }
}

/// 唯一精确匹配才替换（搬运 goose developer/edit.rs:157 `string_replace`，
/// 加了 before 为空的显式拒绝）。
pub fn string_replace(content: &str, before: &str, after: &str) -> Result<String, String> {
    if before.is_empty() {
        return Err("`before` must not be empty (use write to create/replace files)".to_string());
    }

    let matches: Vec<_> = content.match_indices(before).collect();

    match matches.len() {
        0 => {
            let suggestion = find_similar_context(content, before);
            let mut msg = "No match found for the specified text.".to_string();
            if let Some(hint) = suggestion {
                msg.push_str(&format!("\n\nDid you mean:\n```\n{hint}\n```"));
            }
            let preview = build_file_preview(content, NO_MATCH_PREVIEW_LINES);
            msg.push_str(&format!("\n\nFile preview:\n```\n{preview}\n```"));
            Err(msg)
        }
        1 => Ok(content.replacen(before, after, 1)),
        n => {
            let mut msg = format!(
                "Found {n} matches. Please provide more context to identify a unique match:\n"
            );

            for (i, (pos, _)) in matches.iter().enumerate().take(2) {
                let line_num = count_lines_before(content, *pos);
                let context = get_line_context(content, line_num, 1);
                msg.push_str(&format!(
                    "\nMatch {} (line {line_num}):\n```\n{context}\n```",
                    i + 1
                ));
            }

            if n > 2 {
                msg.push_str(&format!("\n\n...and {} more", n - 2));
            }

            Err(msg)
        }
    }
}

/// 搬运 goose developer/edit.rs `count_lines_before`。
fn count_lines_before(content: &str, byte_pos: usize) -> usize {
    content
        .char_indices()
        .take_while(|(i, _)| *i < byte_pos)
        .filter(|(_, c)| *c == '\n')
        .count()
        + 1
}

/// 搬运 goose developer/edit.rs `get_line_context`。
fn get_line_context(content: &str, target_line: usize, context: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let start = target_line.saturating_sub(context + 1);
    let end = (target_line + context).min(lines.len());

    lines[start..end].join("\n")
}

/// 搬运 goose developer/edit.rs `find_similar_context`。
fn find_similar_context(content: &str, search: &str) -> Option<String> {
    let first_line = search.lines().next()?.trim();
    if first_line.is_empty() {
        return None;
    }

    for (i, line) in content.lines().enumerate() {
        if line.contains(first_line) || first_line.contains(line.trim()) {
            return Some(get_line_context(content, i + 1, 2));
        }
    }

    None
}

/// 搬运 goose developer/edit.rs `build_file_preview`。
fn build_file_preview(content: &str, max_lines: usize) -> String {
    if content.is_empty() {
        return "(file is empty)".to_string();
    }

    let lines: Vec<&str> = content.lines().collect();
    let preview_end = lines.len().min(max_lines);
    let mut preview = lines[..preview_end]
        .iter()
        .enumerate()
        .map(|(index, line)| format!("{:>4}: {}", index + 1, line))
        .collect::<Vec<_>>()
        .join("\n");

    if lines.len() > preview_end {
        preview.push_str(&format!("\n... ({} more lines)", lines.len() - preview_end));
    }

    preview
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn ctx(dir: &Path) -> ToolCtx {
        ToolCtx {
            cwd: dir.to_path_buf(),
            cancel: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn read_returns_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();

        let out = read_file(Path::new("a.txt"), None, None, &ctx(dir.path())).await;
        assert!(!out.is_error);
        assert_eq!(out.text, "   1: alpha\n   2: beta\n   3: gamma\n");
    }

    #[tokio::test]
    async fn read_honors_line_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        let content: String = (1..=10).map(|n| format!("line{n}\n")).collect();
        std::fs::write(&file, content).unwrap();

        let out = read_file(&file, Some(3), Some(2), &ctx(dir.path())).await;
        assert_eq!(out.text, "   3: line3\n   4: line4\n... (6 more lines)\n");

        let out = read_file(&file, Some(11), None, &ctx(dir.path())).await;
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn read_defaults_to_2000_lines() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("big.txt");
        let content: String = (1..=2500).map(|n| format!("{n}\n")).collect();
        std::fs::write(&file, content).unwrap();

        let out = read_file(&file, None, None, &ctx(dir.path())).await;
        assert!(out.text.contains("   1: 1\n"));
        assert!(out.text.contains(&format!("{:>4}: 2000\n", 2000)));
        assert!(!out.text.contains("   2001:"));
        assert!(out.text.contains("... (500 more lines)"));
    }

    #[tokio::test]
    async fn write_creates_parent_dirs_and_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let nested = Path::new("x/y/z.txt");

        let out = write_file(nested, "first", &ctx(dir.path())).await;
        assert!(!out.is_error);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("x/y/z.txt")).unwrap(),
            "first"
        );

        let out = write_file(nested, "second", &ctx(dir.path())).await;
        assert!(!out.is_error);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("x/y/z.txt")).unwrap(),
            "second"
        );
    }

    #[tokio::test]
    async fn edit_unique_match_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one\ntwo\nthree\n").unwrap();

        let out = edit_file(&file, "two", "TWO!", &ctx(dir.path())).await;
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "one\nTWO!\nthree\n"
        );
    }

    #[tokio::test]
    async fn edit_ambiguous_match_errors_with_count() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "dup\nx\ndup\ny\ndup\n").unwrap();

        let out = edit_file(&file, "dup", "new", &ctx(dir.path())).await;
        assert!(out.is_error);
        assert!(out.text.contains("Found 3 matches"));
        assert!(out.text.contains("Match 1 (line 1)"));
        assert!(out.text.contains("Match 2 (line 3)"));
        assert!(out.text.contains("...and 1 more"));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "dup\nx\ndup\ny\ndup\n"
        );
    }

    #[tokio::test]
    async fn edit_no_match_gives_similar_context_and_preview() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "hello world\nsecond line\n").unwrap();

        let out = edit_file(&file, "hello world!", "x", &ctx(dir.path())).await;
        assert!(out.is_error);
        assert!(out.text.contains("No match found"));
        assert!(out.text.contains("Did you mean:"));
        assert!(out.text.contains("File preview:"));
        assert!(out.text.contains("   1: hello world"));
    }

    #[tokio::test]
    async fn edit_empty_after_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "keep\ndrop\nkeep2\n").unwrap();

        let out = edit_file(&file, "drop\n", "", &ctx(dir.path())).await;
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep\nkeep2\n");
    }

    #[tokio::test]
    async fn edit_empty_before_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "abc\n").unwrap();

        let out = edit_file(&file, "", "x", &ctx(dir.path())).await;
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn write_atomic_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let out = write_file(Path::new("a.txt"), "content", &ctx(dir.path())).await;
        assert!(!out.is_error, "{}", out.text);
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["a.txt".to_string()]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_and_edit_fail_on_readonly_dir_without_touching_original() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("ro");
        std::fs::create_dir(&sub).unwrap();
        let file = sub.join("a.txt");
        std::fs::write(&file, "orig\n").unwrap();
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o500)).unwrap();

        let out = write_file(&file, "new", &ctx(dir.path())).await;
        assert!(out.is_error);
        let out = edit_file(&file, "orig", "new", &ctx(dir.path())).await;
        assert!(out.is_error);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "orig\n");
        let leftovers: Vec<_> = std::fs::read_dir(&sub)
            .unwrap()
            .map(|e| e.unwrap())
            .collect();
        assert_eq!(leftovers.len(), 1);

        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_and_edit_reject_symlink_targets() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.txt");
        std::fs::write(&real, "secret\n").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let out = write_file(&link, "owned", &ctx(dir.path())).await;
        assert!(out.is_error);
        assert!(out.text.contains("symlink"), "{}", out.text);
        let out = edit_file(&link, "secret", "owned", &ctx(dir.path())).await;
        assert!(out.is_error);
        assert!(out.text.contains("symlink"), "{}", out.text);
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "secret\n");
    }

    #[tokio::test]
    async fn write_creates_new_file_through_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let out = write_file(Path::new("brand/new.txt"), "x", &ctx(dir.path())).await;
        assert!(!out.is_error, "{}", out.text);
    }

    #[tokio::test]
    async fn read_rejects_oversized_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("huge.bin");
        let f = std::fs::File::create(&file).unwrap();
        // 稀疏文件：metadata 报告超大长度，读取前就被字节上限拦截。
        f.set_len(MAX_READ_BYTES + 1).unwrap();

        let out = read_file(&file, None, None, &ctx(dir.path())).await;
        assert!(out.is_error);
        assert!(out.text.contains("too large"), "{}", out.text);
    }

    #[tokio::test]
    async fn read_streams_window_of_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("many.txt");
        let content: String = (1..=50_000).map(|n| format!("line{n}\n")).collect();
        std::fs::write(&file, content).unwrap();

        let out = read_file(&file, Some(2), Some(3), &ctx(dir.path())).await;
        assert!(!out.is_error);
        assert_eq!(
            out.text,
            "   2: line2\n   3: line3\n   4: line4\n... (49996 more lines)\n"
        );
    }

    #[tokio::test]
    async fn read_rejects_zero_line_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "x\n").unwrap();

        let out = read_file(&file, Some(0), None, &ctx(dir.path())).await;
        assert!(out.is_error);
        assert!(out.text.contains("1-based"), "{}", out.text);

        let out = read_file(&file, None, Some(0), &ctx(dir.path())).await;
        assert!(out.is_error);
        assert!(out.text.contains("positive"), "{}", out.text);
    }

    #[test]
    fn resolve_path_joins_relative() {
        assert_eq!(
            resolve_path(Path::new("b/c"), Path::new("/a")),
            PathBuf::from("/a/b/c")
        );
        assert_eq!(
            resolve_path(Path::new("/x/y"), Path::new("/a")),
            PathBuf::from("/x/y")
        );
    }

    #[tokio::test]
    async fn write_rejects_oversized_content() {
        let dir = tempfile::tempdir().unwrap();
        let content = "a".repeat(MAX_WRITE_BYTES as usize + 1);

        let out = write_file(Path::new("big.txt"), &content, &ctx(dir.path())).await;
        assert!(out.is_error);
        assert!(out.text.contains("content too large"), "{}", out.text);
        assert!(!dir.path().join("big.txt").exists());
    }

    #[tokio::test]
    async fn edit_rejects_oversized_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("huge.txt");
        let f = std::fs::File::create(&file).unwrap();
        // 稀疏文件：metadata 报告超大长度，整读前就被字节上限拦截。
        f.set_len(MAX_READ_BYTES + 1).unwrap();

        let out = edit_file(&file, "x", "y", &ctx(dir.path())).await;
        assert!(out.is_error);
        assert!(out.text.contains("too large"), "{}", out.text);
    }

    #[tokio::test]
    async fn edit_rejects_non_utf8_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("bin.dat");
        std::fs::write(&file, [0xffu8, 0xfe, 0x00]).unwrap();

        let out = edit_file(&file, "x", "y", &ctx(dir.path())).await;
        assert!(out.is_error);
        assert!(out.text.contains("not valid UTF-8"), "{}", out.text);
    }

    #[tokio::test]
    async fn read_precancelled_returns_cancelled_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x\n").unwrap();
        let token = CancellationToken::new();
        token.cancel();
        let ctx = ToolCtx {
            cwd: dir.path().to_path_buf(),
            cancel: token,
        };

        let out = read_file(Path::new("a.txt"), None, None, &ctx).await;
        assert!(out.is_error);
        assert!(out.text.contains("cancelled"), "{}", out.text);
    }

    /// 取消上界验证：32MB / 1M 行的文件，中途取消后必须在可测上界内返回。
    #[tokio::test]
    async fn read_cancellation_returns_within_bound() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("big.txt");
        let line = "a".repeat(31) + "\n"; // 32 字节/行
        let content = line.repeat((MAX_READ_BYTES as usize) / 32);
        std::fs::write(&file, content).unwrap();

        let token = CancellationToken::new();
        let ctx = ToolCtx {
            cwd: dir.path().to_path_buf(),
            cancel: token.clone(),
        };
        let handle =
            tokio::spawn(async move { read_file(Path::new("big.txt"), None, None, &ctx).await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        token.cancel();
        let start = std::time::Instant::now();
        let out = handle.await.unwrap();
        let elapsed = start.elapsed();
        eprintln!("read cancellation took {elapsed:?}");

        assert!(out.is_error);
        assert!(out.text.contains("cancelled"), "{}", out.text);
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "取消应在可测上界内生效: {elapsed:?}"
        );
    }

    /// ADR 0003 D6 显式非保证：中间路径 symlink 不做 containment，
    /// `write linkdir/file` 会跟随 cwd 内的链接写到外部（隔离由 sandbox 负责）。
    #[cfg(unix)]
    #[tokio::test]
    async fn write_follows_intermediate_symlink_per_adr_d6() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let link = dir.path().join("linkdir");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();

        let out = write_file(Path::new("linkdir/f.txt"), "escaped", &ctx(dir.path())).await;
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(
            std::fs::read_to_string(outside.path().join("f.txt")).unwrap(),
            "escaped"
        );
    }
}
