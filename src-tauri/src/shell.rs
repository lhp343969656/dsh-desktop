//! dsh host 进程的启动与整树终止。
//!
//! Windows：子进程放入 Job Object（KILL_ON_JOB_CLOSE），退出时关闭句柄即整树终止。
//! Unix：子进程独立进程组，退出时向进程组发 SIGTERM，宽限期后 SIGKILL。

use std::io;
use std::io::BufRead;
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
#[cfg(windows)]
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    SetInformationJobObject, TerminateJobObject,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
};

/// host 启动参数。生产环境不经过 shell，直接以参数数组执行。
#[derive(Clone, Debug)]
pub struct LaunchSpec {
    pub node_bin: PathBuf,
    pub dsh_bin: PathBuf,
    pub dsh_home: PathBuf,
    pub ready_timeout: Duration,
}

pub struct HostProcess {
    child: Child,
    /// 最近 40 行 stderr，出错时用于展示诊断信息。
    stderr_lines: Arc<Mutex<Vec<String>>>,
    #[cfg(windows)]
    job: Option<HANDLE>,
}

// HANDLE 是裸指针，Rust 认为不可跨线程移动；本结构体对 job 句柄的访问
// 始终由所有权串行化（启动线程创建并移交，退出回调/析构使用），因此安全。
#[cfg(windows)]
unsafe impl Send for HostProcess {}

impl HostProcess {
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    pub fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    pub fn stderr_log(&self) -> Vec<String> {
        self.stderr_lines.lock().unwrap().clone()
    }

    pub fn kill_tree(&mut self) -> io::Result<()> {
        #[cfg(windows)]
        {
            // 关闭 Job Object 句柄即终止整个进程树（KILL_ON_JOB_CLOSE）
            if let Some(job) = self.job.take() {
                unsafe {
                    let _ = TerminateJobObject(job, 1);
                    CloseHandle(job);
                }
                return Ok(());
            }
            self.child.kill()
        }
        #[cfg(unix)]
        {
            let pid = self.child.id() as i32;
            unsafe {
                let _ = libc::kill(-pid, libc::SIGTERM);
            }
            // 宽限期：等待优雅退出，超时后强制终止
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while std::time::Instant::now() < deadline {
                if let Ok(Some(_)) = self.child.try_wait() {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            unsafe {
                let _ = libc::kill(-pid, libc::SIGKILL);
            }
            Ok(())
        }
    }
}

impl Drop for HostProcess {
    fn drop(&mut self) {
        let _ = self.kill_tree();
    }
}

pub fn spawn_host(spec: &LaunchSpec) -> io::Result<HostProcess> {
    let mut cmd = Command::new(&spec.node_bin);
    cmd.arg(&spec.dsh_bin)
        .arg("--profile")
        .arg("web")
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg("0")
        .env("DSH_HOME", &spec.dsh_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // 启动前把自己设为独立进程组组长，便于整组终止
        unsafe {
            cmd.pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            });
        }
    }

    let mut child = cmd.spawn()?;

    // 后台排空 stderr，避免管道写满阻塞子进程；保留最近 40 行供诊断
    let stderr = child.stderr.take().expect("stderr piped");
    let stderr_lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let log = stderr_lines.clone();
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let mut guard = log.lock().unwrap();
                        guard.push(line.trim_end().to_string());
                        if guard.len() > 40 {
                            guard.remove(0);
                        }
                    }
                }
            }
        });
    }

    #[cfg(windows)]
    {
        let job = unsafe { create_job()? };
        if let Err(e) = unsafe { assign_to_job(job, child.id()) } {
            unsafe {
                let _ = TerminateJobObject(job, 1);
                CloseHandle(job);
            }
            let _ = child.kill();
            return Err(e);
        }
        Ok(HostProcess {
            child,
            stderr_lines,
            job: Some(job),
        })
    }
    #[cfg(unix)]
    {
        Ok(HostProcess {
            child,
            stderr_lines,
        })
    }
}

#[cfg(windows)]
unsafe fn create_job() -> io::Result<HANDLE> {
    let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut info = std::mem::zeroed::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>();
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let ok = SetInformationJobObject(
        job,
        JobObjectExtendedLimitInformation,
        &info as *const _ as *const std::ffi::c_void,
        std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
    );
    if ok == 0 {
        let err = io::Error::last_os_error();
        CloseHandle(job);
        return Err(err);
    }
    Ok(job)
}

#[cfg(windows)]
unsafe fn assign_to_job(job: HANDLE, pid: u32) -> io::Result<()> {
    let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    let ok = AssignProcessToJobObject(job, process);
    CloseHandle(process);
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
