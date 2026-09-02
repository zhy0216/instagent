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
//! - `--crash-on-chat` + `--state-file`：首个实例在 chat 请求处自杀；state
//!   文件存在时转为稳定模式（自动重启一次后恢复）。
//! - `--always-crash`：每个 chat 请求都自杀（验证"每次调用至多重启一次"）。
//! - `--log-file`：每次进程启动 append 一行 `start <port>`（数重启次数）。
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

#[derive(Default)]
struct Args {
    port: u16,
    ready_delay: f64,
    never_ready: bool,
    exit_early: bool,
    crash_on_chat: bool,
    always_crash: bool,
    state_file: Option<PathBuf>,
    log_file: Option<PathBuf>,
    meta_file: Option<PathBuf>,
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
            "--crash-on-chat" => args.crash_on_chat = true,
            "--always-crash" => args.always_crash = true,
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
        writeln!(file, "start {}", args.port).expect("append log line");
    }
    if args.exit_early {
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
    if let Some(state) = &args.state_file {
        let first = !state.exists();
        if first {
            let _ = fs::write(state, args.port.to_string());
        }
        crash_on_chat = first;
    }

    let listener = TcpListener::bind(("127.0.0.1", args.port)).expect("bind fake proxy port");
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        handle(&mut stream, args.always_crash, crash_on_chat);
    }
}

fn handle(stream: &mut TcpStream, always_crash: bool, crash_on_chat: bool) {
    let Some((method, path)) = read_request(stream) else {
        return;
    };
    if method == "GET" && path == "/v1/models" {
        respond(
            stream,
            200,
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
