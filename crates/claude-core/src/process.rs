//! `is_pid_running` — per-OS process liveness probe, ported from
//! `process_other.go` (unix signal-0) and `process_windows.go`
//! (OpenProcess + GetExitCodeProcess == STILL_ACTIVE).

#[cfg(unix)]
pub fn is_pid_running(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    // Signal 0 performs the existence/permission check without delivering a
    // signal. Ok(()) ⇒ the process exists and is signalable. Any error
    // (ESRCH gone, EPERM no permission) ⇒ treat as not-our-running, matching
    // Go's `err == nil`.
    kill(Pid::from_raw(pid), None).is_ok()
}

#[cfg(windows)]
pub fn is_pid_running(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // 259 == STILL_ACTIVE.
    const STILL_ACTIVE: u32 = 259;
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
        if handle.is_null() {
            return false;
        }
        let mut exit_code: u32 = 0;
        let ok = GetExitCodeProcess(handle, &mut exit_code);
        CloseHandle(handle);
        ok != 0 && exit_code == STILL_ACTIVE
    }
}
