mod ready;
mod runtime;
mod shell;

use std::sync::Mutex;
use std::time::Duration;

use tauri::Manager;

/// 持有运行中的 dsh host 进程，随应用退出而终止。
/// 导航成功后 host 从启动线程移交到这里，避免被 drop 误杀。
pub struct HostGuard(pub Mutex<Option<shell::HostProcess>>);

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // 第二次启动：聚焦已有窗口，不再拉起第二个 host
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.set_focus();
            }
        }))
        .setup(|app| {
            app.manage(HostGuard(Mutex::new(None)));
            let handle = app.handle().clone();
            std::thread::spawn(move || startup(&handle));
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error building tauri application")
        .run(|app, event| {
            // 应用退出时整树终止 host，保证无孤儿进程
            if let tauri::RunEvent::Exit = event {
                if let Some(mut host) = app.state::<HostGuard>().inner().0.lock().unwrap().take() {
                    let _ = host.kill_tree();
                }
            }
        });
}

/// 启动流程：选 Runtime → 拉起 host → 等就绪 → 健康检查 → 窗口导航。
fn startup(app: &tauri::AppHandle) {
    let spec = match runtime::resolve(app) {
        Ok(s) => s,
        Err(e) => {
            log_line(app, &format!("runtime resolve failed: {e}"));
            show_error(app, &e);
            return;
        }
    };
    // 开发模式：确保数据目录存在
    if std::env::var_os("DSH_DESKTOP_DSH_BIN").is_some() {
        if let Err(e) = std::fs::create_dir_all(&spec.dsh_home) {
            log_line(app, &format!("create dev dsh_home failed: {e}"));
        }
    }

    let mut host = match shell::spawn_host(&spec) {
        Ok(h) => h,
        Err(e) => {
            let msg = format!("启动 dsh 失败: {e}");
            log_line(app, &msg);
            show_error(app, &msg);
            return;
        }
    };

    match ready::wait_ready(&mut host, spec.ready_timeout) {
        Ok(outcome) => {
            if let Err(e) = ready::health_check(&outcome.url, Duration::from_secs(5)) {
                let msg = format!("dsh 已就绪但健康检查失败: {e}");
                log_line(app, &msg);
                let _ = host.kill_tree();
                show_error(app, &msg);
                return;
            }
            log_line(
                app,
                &format!(
                    "host ready at {} (dsh {})",
                    outcome.url,
                    outcome.dsh_version.as_deref().unwrap_or("?")
                ),
            );
            // 移交 host 给全局状态，保持存活直到应用退出
            let mut guard = app.state::<HostGuard>().inner().0.lock().unwrap();
            *guard = Some(host);
            drop(guard);
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.navigate(outcome.url);
            }
        }
        Err(e) => {
            let stderr = host.stderr_log().join("\n");
            let msg = format!("dsh 启动失败: {e}\n{stderr}");
            log_line(app, &msg);
            let _ = host.kill_tree();
            show_error(app, &msg);
        }
    }
}

/// 把错误显示在加载页上（通过 fragment 传参，避免改 URL 路径）。
fn show_error(app: &tauri::AppHandle, message: &str) {
    if let Some(win) = app.get_webview_window("main") {
        let mut url = win
            .url()
            .unwrap_or_else(|_| tauri::Url::parse("tauri://localhost/index.html").unwrap());
        url.set_fragment(Some(&format!("error={}", encode_fragment(message))));
        let _ = win.navigate(url);
    }
}

/// 保留 RFC 3986 unreserved 字符，其余 percent 编码（含 # & =）。
fn encode_fragment(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 追加一行到用户日志目录下的 runtime.log，便于排查安装后的问题。
fn log_line(app: &tauri::AppHandle, line: &str) {
    let Ok(log_dir) = app.path().app_log_dir() else {
        return;
    };
    let _ = std::fs::create_dir_all(&log_dir);
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("runtime.log"))
    {
        let _ = writeln!(f, "{} {line}", chrono_like_timestamp());
    }
}

/// UTC 时间戳（纯 Rust 计算，避免平台时间 API 差异）。日志用 UTC 便于比对。
fn chrono_like_timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let days = now.div_euclid(86_400);
    let secs_of_day = now.rem_euclid(86_400);
    let (h, _m, s) = (secs_of_day / 3600, (secs_of_day % 3600) / 60, secs_of_day % 60);
    // 1970-01-01 起的民用日期（Howard Hinnant 算法）
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {h:02}:{m:02}:{s:02}")
}
