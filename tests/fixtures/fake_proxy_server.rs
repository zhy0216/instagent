//! 假 openai 兼容 proxy server（todo 11 测试 fixture，仅 std，不起网络依赖）。
//!
//! 用 Rust 而非 python3 写：本机 macOS 26 在测试期间开始把 python.org Python
//! 的监听 socket 黑洞掉（跨进程连 127.0.0.1 全部超时），sh/rust 不受影响；
//! 形态沿用 `mcp_stdio_server.rs` 的 fixture bin 先例（只加 [[bin]] target，
//! 不加依赖）。
//!
//! 端点：`GET /v1/models`（就绪探针）与 `POST /v1/chat/completions`
//! （SSE：一个 content delta + `[DONE]`）。标志：
//!
//! - `--port N`：监听端口（由被测代码从 `${PORT}` 替换注入）。
//! - `--ready-delay SECS`：先睡再 bind（慢就绪）。
//! - `--never-ready`：永不 bind、不退出（就绪超时）。
//! - `--exit-early`：立即以 2 退出（就绪前子进程退出）。
//! - `--exit-after SECS`：睡 SECS 后以 2 退出（就绪前延迟退出，耗总期限）。
//! - `--fail-first-start FILE`：首次启动写 FILE 后以 4 退出；FILE 已存在则
//!   正常启动（注入"第一候选失败、下一候选成功"，供换端口重试用）。
//! - `--ready-status N`：就绪探针返回状态码 N（默认 200）。非 200 表示永不就绪。
//! - `--accept-and-hold`：接受连接但不回包（探针挂起，验证探针超时受限）。
//! - `--crash-on-chat` + `--state-file`：首个实例在 chat 请求处自杀；state
//!   文件存在时转为稳定模式（自动重启一次后恢复）。
//! - `--always-crash`：每个 chat 请求都自杀（验证"每次调用至多重启一次"）。
//! - `--late-ready-delay SECS` + `--state-file`：仅在重启后的稳定实例
//!   （state 已存在）上生效的就绪延迟，用于让重启候选慢就绪以便取消。
//! - `--log-file`：每次进程启动 append 一行 `start <port> <pid>`（数重启次数）。
//! - `--meta-file`：写入启动时观察到的 port/env/pid（断言证据）。

use std::env;
use std::fs;
use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::net::TcpStream;
use std::path::PathBuf;
use std::process;
use std::thread::sleep;
use std::time::Duration;

struct Args {
    port: u16,
    ready_delay: f64,
    never_ready: bool,
    exit_early: bool,
    exit_after: f64,
    ready_status: u16,
    accept_and_hold: bool,
    crash_on_chat: bool,
    always_crash: bool,
    late_ready_delay: f64,
    fail_first_start: Option<PathBuf>,
    state_file: Option<PathBuf>,
    log_file: Option<PathBuf>,
    meta_file: Option<PathBuf>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            port: 0,
            ready_delay: 0.0,
            never_ready: false,
            exit_early: false,
            exit_after: 0.0,
            ready_status: 200,
            accept_and_hold: false,
            crash_on_chat: false,
            always_crash: false,
            late_ready_delay: 0.0,
            fail_first_start: None,
            state_file: None,
            log_file: None,
            meta_file: None,
        }
    }
}

fn parse_args() -> Args {
    let mut args = Args::default();
    let mut items = env::args().skip(1);
    while let Some(flag) = items.next() {
        match flag.as_str() {
            "--port" => args.port = items.next().and_then(|v| v.parse().ok()).expect("--port"),
            "--ready-delay" => {
                args.ready_delay = items
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--ready-delay")
            }
            "--never-ready" => args.never_ready = true,
            "--exit-early" => args.exit_early = true,
            "--exit-after" => {
                args.exit_after = items
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--exit-after")
            }
            "--ready-status" => {
                args.ready_status = items
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--ready-status")
            }
            "--accept-and-hold" => args.accept_and_hold = true,
            "--crash-on-chat" => args.crash_on_chat = true,
            "--always-crash" => args.always_crash = true,
            "--late-ready-delay" => {
                args.late_ready_delay = items
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--late-ready-delay")
            }
            "--fail-first-start" => args.fail_first_start = items.next().map(PathBuf::from),
            "--state-file" => args.state_file = items.next().map(PathBuf::from),
            "--log-file" => args.log_file = items.next().map(PathBuf::from),
            "--meta-file" => args.meta_file = items.next().map(PathBuf::from),
            other => panic!("unknown flag {other}"),
        }
    }
    args
}

fn json_opt(value: Option<String>) -> String {
    match value {
        Some(v) => format!("\"{v}\""),
        None => "null".to_string(),
    }
}

fn main() {
    let args = parse_args();
    if let Some(path) = &args.meta_file {
        let content = format!(
            "{{\"args_port\":{},\"env_port\":{},\"marker\":{},\"secret\":{},\"pid\":{}}}",
            args.port,
            json_opt(env::var("INSTAGENT_PORT").ok()),
            json_opt(env::var("FAKE_PROXY_MARKER").ok()),
            json_opt(env::var("INSTAGENT_TEST_SECRET").ok()),
            process::id(),
        );
        fs::write(path, content).expect("write meta file");
    }
    if let Some(path) = &args.log_file {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open log file");
        writeln!(file, "start {} {}", args.port, process::id()).expect("append log line");
    }
    if args.exit_early {
        process::exit(2);
    }
    // 注入"第一候选失败"：首个进程留下标记后退出，下一候选见标记正常启动。
    if let Some(path) = &args.fail_first_start {
        if !path.exists() {
            let _ = fs::write(path, "1");
            process::exit(4);
        }
    }
    if args.exit_after > 0.0 {
        sleep(Duration::from_secs_f64(args.exit_after));
        process::exit(2);
    }
    if args.never_ready {
        loop {
            sleep(Duration::from_secs(600));
        }
    }
    if args.ready_delay > 0.0 {
        sleep(Duration::from_secs_f64(args.ready_delay));
    }

    // crash-on-chat：state 文件不存在 = 第一个实例（自杀一次）；存在 = 重启后的稳定实例。
    let mut crash_on_chat = args.crash_on_chat;
    let mut restarted = false;
    if let Some(state) = &args.state_file {
        let first = !state.exists();
        restarted = !first;
        if first {
            let _ = fs::write(state, args.port.to_string());
        }
        crash_on_chat = first;
    }
    // 仅重启后的稳定实例慢就绪：给并发重启/取消重启留出可观察窗口。
    if restarted && args.late_ready_delay > 0.0 {
        sleep(Duration::from_secs_f64(args.late_ready_delay));
    }

    let listener = TcpListener::bind(("127.0.0.1", args.port)).expect("bind fake proxy port");
    if args.accept_and_hold {
        // 接受连接但不回包也不关闭：就绪探针挂起，直到其自身受限超时。
        let mut held = Vec::new();
        for stream in listener.incoming().flatten() {
            held.push(stream);
        }
        return;
    }
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        handle(
            &mut stream,
            args.always_crash,
            crash_on_chat,
            args.ready_status,
        );
    }
}

fn handle(stream: &mut TcpStream, always_crash: bool, crash_on_chat: bool, ready_status: u16) {
    let Some((method, path)) = read_request(stream) else {
        return;
    };
    if method == "GET" && path == "/v1/models" {
        respond(
            stream,
            ready_status,
            "application/json",
            r#"{"object":"list","data":[{"id":"fake-model"}]}"#,
        );
    } else if method == "POST" && path == "/v1/chat/completions" {
        if always_crash || crash_on_chat {
            process::exit(3);
        }
        respond(
            stream,
            200,
            "text/event-stream",
            "data: {\"choices\":[{\"delta\":{\"content\":\"pong\"}}]}\n\ndata: [DONE]\n\n",
        );
    } else {
        respond(stream, 404, "application/json", r#"{"error":"not found"}"#);
    }
}

/// 读到头部终止符，解析 method/path 并按 Content-Length 抽干 body。
fn read_request(stream: &mut TcpStream) -> Option<(String, String)> {
    let mut buf: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(0) | Err(_) => return None,
            Ok(_) => buf.push(byte[0]),
        }
        if buf.len() > 64 * 1024 {
            return None;
        }
    }
    let head = String::from_utf8_lossy(&buf).into_owned();
    let mut lines = head.lines();
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_string();
    let path = request_line.next()?.to_string();
    let content_length = lines
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.trim().eq_ignore_ascii_case("content-length") {
                Some(value.trim())
            } else {
                None
            }
        })
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut remaining = content_length;
    let mut sink = [0u8; 4096];
    while remaining > 0 {
        let want = remaining.min(sink.len());
        match stream.read(&mut sink[..want]) {
            Ok(0) | Err(_) => break,
            Ok(n) => remaining -= n,
        }
    }
    Some((method, path))
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &str) {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Both);
}
