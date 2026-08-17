//! dsh 就绪协议：解析 host 标准输出中的 ready 行，并做 HTTP 健康检查。

use std::io::BufRead;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tauri::Url;

use crate::shell::HostProcess;

pub struct ReadyOutcome {
    pub url: Url,
    pub dsh_version: Option<String>,
}

pub enum ReadyError {
    Timeout,
    ProcessExited(Option<i32>),
    InvalidOutput(String),
    HealthCheck(String),
}

impl std::fmt::Display for ReadyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadyError::Timeout => write!(f, "等待就绪超时"),
            ReadyError::ProcessExited(code) => write!(f, "host 进程提前退出 (code={:?})", code),
            ReadyError::InvalidOutput(s) => write!(f, "无法解析就绪输出: {s}"),
            ReadyError::HealthCheck(s) => write!(f, "健康检查失败: {s}"),
        }
    }
}

/// 等待 host 输出就绪行，返回校验通过的 URL。
/// 兼容两种格式：
///   JSONL: {"event":"ready","url":"http://127.0.0.1:43127","dshVersion":"..."}
///   文本:  dsh web: http://127.0.0.1:43127
pub fn wait_ready(host: &mut HostProcess, timeout: Duration) -> Result<ReadyOutcome, ReadyError> {
    let stdout = host
        .take_stdout()
        .ok_or_else(|| ReadyError::InvalidOutput("无法读取 host 输出".into()))?;

    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.send(line.trim_end().to_string()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ReadyError::Timeout);
        }
        match rx.recv_timeout(remaining) {
            Ok(line) => {
                if let Some(outcome) = parse_ready_line(&line) {
                    return Ok(outcome);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => return Err(ReadyError::Timeout),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // 输出流关闭：进程多半已退出
                let code = host.try_wait().ok().flatten().map(|s| s.code().unwrap_or(-1));
                return Err(ReadyError::ProcessExited(code));
            }
        }
    }
}

fn parse_ready_line(line: &str) -> Option<ReadyOutcome> {
    let trimmed = line.trim();
    if trimmed.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if v.get("event").and_then(|e| e.as_str()) == Some("ready") {
                let url = v.get("url").and_then(|u| u.as_str())?;
                let dsh_version = v
                    .get("dshVersion")
                    .and_then(|s| s.as_str())
                    .map(String::from);
                return validate_url(url).map(|u| ReadyOutcome { url: u, dsh_version });
            }
        }
    }
    // 文本格式：取第一个 http:// 开头的 token
    if let Some(idx) = trimmed.find("http://") {
        let url_str: String = trimmed[idx..]
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != ',' && *c != ')')
            .collect();
        if let Some(url) = validate_url(&url_str) {
            return Some(ReadyOutcome {
                url,
                dsh_version: None,
            });
        }
    }
    None
}

/// 就绪 URL 必须满足：http、127.0.0.1、端口 1-65535、路径 /、无凭据与附加字段。
fn validate_url(url: &str) -> Option<Url> {
    let u = Url::parse(url).ok()?;
    if u.scheme() != "http" {
        return None;
    }
    if u.host_str()? != "127.0.0.1" {
        return None;
    }
    if u.port()? < 1 {
        return None;
    }
    if u.path() != "/" {
        return None;
    }
    if !u.username().is_empty() || u.password().is_some() {
        return None;
    }
    if u.query().is_some() || u.fragment().is_some() {
        return None;
    }
    Some(u)
}

/// 对就绪 URL 发起 GET 健康检查，要求 2xx。
pub fn health_check(url: &Url, timeout: Duration) -> Result<(), ReadyError> {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let host = url
        .host_str()
        .ok_or_else(|| ReadyError::HealthCheck("URL 缺少 host".into()))?;
    let port = url
        .port()
        .ok_or_else(|| ReadyError::HealthCheck("URL 缺少端口".into()))?;
    let addr = format!("{host}:{port}");
    let socket_addr = addr
        .parse()
        .map_err(|e| ReadyError::HealthCheck(format!("地址无效: {e}")))?;

    let mut stream = TcpStream::connect_timeout(&socket_addr, timeout)
        .map_err(|e| ReadyError::HealthCheck(e.to_string()))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| ReadyError::HealthCheck(e.to_string()))?;

    let req = format!(
        "GET / HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| ReadyError::HealthCheck(e.to_string()))?;

    let mut buf = [0u8; 64];
    let n = stream
        .read(&mut buf)
        .map_err(|e| ReadyError::HealthCheck(e.to_string()))?;
    let head = String::from_utf8_lossy(&buf[..n]);
    let code = head
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| ReadyError::HealthCheck("无 HTTP 状态行".into()))?;
    let code: u16 = code
        .parse()
        .map_err(|_| ReadyError::HealthCheck(format!("状态码异常: {code}")))?;
    if (200..300).contains(&code) {
        Ok(())
    } else {
        Err(ReadyError::HealthCheck(format!("HTTP {code}")))
    }
}
