//! Runtime 选择与校验。
//!
//! 优先级：
//!   1. 环境变量覆盖（开发模式）：DSH_DESKTOP_DSH_BIN / DSH_DESKTOP_NODE_BIN / DSH_DESKTOP_DATA_ROOT
//!   2. 安装包内置 Runtime：resource_dir/bundled-runtime（manifest.json 描述）

use std::path::{Path, PathBuf};
use std::time::Duration;

use tauri::Manager;

use crate::shell::LaunchSpec;

const READY_TIMEOUT_SECS: u64 = 30;

pub fn resolve(app: &tauri::AppHandle) -> Result<LaunchSpec, String> {
    if let Some(dsh_bin) = std::env::var_os("DSH_DESKTOP_DSH_BIN") {
        return resolve_dev_override(dsh_bin, app);
    }
    resolve_bundled(app)
}

fn resolve_dev_override(dsh_bin: std::ffi::OsString, app: &tauri::AppHandle) -> Result<LaunchSpec, String> {
    let node_bin = std::env::var_os("DSH_DESKTOP_NODE_BIN")
        .map(PathBuf::from)
        .ok_or("开发模式设置了 DSH_DESKTOP_DSH_BIN，但缺少 DSH_DESKTOP_NODE_BIN")?;
    if !node_bin.is_file() {
        return Err(format!("Node 二进制不存在: {}", node_bin.display()));
    }
    let dsh_bin = PathBuf::from(dsh_bin);
    if !dsh_bin.is_file() {
        return Err(format!("dsh 入口不存在: {}", dsh_bin.display()));
    }
    let dsh_home = std::env::var_os("DSH_DESKTOP_DATA_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| default_dsh_home(app));
    Ok(LaunchSpec {
        node_bin,
        dsh_bin,
        dsh_home,
        ready_timeout: Duration::from_secs(READY_TIMEOUT_SECS),
    })
}

fn resolve_bundled(app: &tauri::AppHandle) -> Result<LaunchSpec, String> {
    // Tauri NSIS 安装器把资源放在 <install>/_up_/resources 下（更新器整目录替换用），
    // 而 resource_dir() 返回安装根目录；不同打包布局下候选位置不同，逐个尝试。
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(resource_dir) = app.path().resource_dir() {
        candidates.push(resource_dir.join("bundled-runtime"));
        candidates.push(resource_dir.join("_up_/resources/bundled-runtime"));
    }
    if let Ok(exe_dir) = app.path().executable_dir() {
        candidates.push(exe_dir.join("bundled-runtime"));
        candidates.push(exe_dir.join("_up_/resources/bundled-runtime"));
        candidates.push(exe_dir.join("resources/bundled-runtime"));
    }

    let mut last_error = String::new();
    for rt_dir in &candidates {
        match load_bundled_spec(rt_dir, app) {
            Ok(spec) => return Ok(spec),
            Err(e) => last_error = format!("{}: {e}", rt_dir.display()),
        }
    }
    Err(format!("内置 Runtime 缺失（已尝试 {} 个位置）: {last_error}", candidates.len()))
}

fn load_bundled_spec(rt_dir: &PathBuf, app: &tauri::AppHandle) -> Result<LaunchSpec, String> {
    // tauri 的路径 API 在 Windows 上返回 \\?\ 扩展长度前缀，Node 无法正确解析，
    // 传给子进程前必须还原为普通盘符路径。
    let rt_dir = strip_extended_prefix(rt_dir);
    if !rt_dir.is_dir() {
        return Err("目录不存在".into());
    }
    let manifest_path = rt_dir.join("manifest.json");
    let manifest_text = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("读取 manifest 失败: {e}"))?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest_text)
        .map_err(|e| format!("manifest 格式错误: {e}"))?;

    check_platform(&manifest)?;

    let node_name = if cfg!(windows) { "node.exe" } else { "node" };
    let node_bin = rt_dir.join(node_name);
    if !node_bin.is_file() {
        return Err(format!("Node 二进制缺失: {}", node_bin.display()));
    }

    let entry = manifest
        .get("entryPath")
        .and_then(|v| v.as_str())
        .ok_or("manifest 缺少 entryPath")?;
    let dsh_bin = rt_dir.join(entry);
    if !dsh_bin.is_file() {
        return Err(format!("dsh 入口缺失: {}", dsh_bin.display()));
    }

    Ok(LaunchSpec {
        node_bin,
        dsh_bin,
        dsh_home: default_dsh_home(app),
        ready_timeout: Duration::from_secs(READY_TIMEOUT_SECS),
    })
}

/// 校验 manifest 的平台/架构与当前系统一致，避免跨平台误用。
fn check_platform(manifest: &serde_json::Value) -> Result<(), String> {
    let want_os = manifest.get("platform").and_then(|v| v.as_str());
    if let Some(want) = want_os {
        let have = std::env::consts::OS; // "windows" | "macos"
        let normalized = match want {
            "win32" => "windows",
            "darwin" => "macos",
            other => other,
        };
        if normalized != have {
            return Err(format!(
                "Runtime 平台不匹配: manifest={want}, 当前系统={have}"
            ));
        }
    }
    let want_arch = manifest.get("arch").and_then(|v| v.as_str());
    if let Some(want) = want_arch {
        // Node process.arch 与 Rust consts::ARCH 命名不同：x64==x86_64, arm64==aarch64
        let normalized = match want {
            "x64" => "x86_64",
            "arm64" => "aarch64",
            other => other,
        };
        if normalized != std::env::consts::ARCH {
            return Err(format!(
                "Runtime 架构不匹配: manifest={want}, 当前系统={}",
                std::env::consts::ARCH
            ));
        }
    }
    Ok(())
}

/// 去掉 Windows 扩展长度路径前缀 `\\?\`，保证 Node 子进程能正确解析。
fn strip_extended_prefix(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => path.to_path_buf(),
    }
}

/// 用户数据目录：应用数据目录下的 dsh-home（与会话、凭证分离）。
fn default_dsh_home(app: &tauri::AppHandle) -> PathBuf {
    let base = app
        .path()
        .app_data_dir()
        .unwrap_or_else(|_| std::env::temp_dir().join("dsh-desktop"));
    let home = strip_extended_prefix(&base).join("dsh-home");
    if !home.exists() {
        let _ = std::fs::create_dir_all(&home);
    }
    home
}
