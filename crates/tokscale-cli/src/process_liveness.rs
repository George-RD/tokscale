//! Cross-platform "is this PID still running" probe.
//!
//! The Antigravity and Trae sync locks both record the owning PID and evict a
//! lock only once that owner is provably dead. A wrong answer in the
//! permissive direction lets two syncs run against the same manifest and
//! delete each other's session artifacts, so every uncertain result here
//! resolves to "alive" and merely defers a sync.
//!
//! The two callers previously carried separate copies of this probe that had
//! already drifted — Antigravity's comment defended the no-overlap invariant
//! while Trae's waived it — and both were Unix-only. One module keeps the
//! policy in a single place.

/// Whether `pid` names a process that is currently running.
///
/// Returns `true` whenever liveness cannot be established. A false "dead"
/// answer costs correctness (two syncs overlap and corrupt the manifest); a
/// false "alive" answer only postpones a sync, which the caller reports and
/// the next run retries.
pub fn pid_is_alive(pid: u32) -> bool {
    // PID 0 is the kernel/idle process on every platform tokscale ships to,
    // and is also what a truncated or zero-filled lock file parses to, so it
    // never denotes a live sync.
    if pid == 0 {
        return false;
    }
    imp::pid_is_alive(pid)
}

#[cfg(unix)]
mod imp {
    /// `errno` for EPERM, identical across the Unix targets tokscale builds.
    const EPERM: i32 = 1;

    extern "C" {
        #[link_name = "kill"]
        fn libc_kill(pid: i32, sig: i32) -> i32;
    }

    pub(super) fn pid_is_alive(pid: u32) -> bool {
        // Signal 0 runs `kill`'s existence and permission checks without
        // delivering anything. EPERM still proves the process exists; it only
        // says we may not signal it.
        let result = unsafe { libc_kill(pid as i32, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(EPERM)
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;

    /// Narrower than `PROCESS_QUERY_INFORMATION`, so the probe still succeeds
    /// against a process running at a higher integrity level instead of
    /// mistaking "may not inspect" for "does not exist".
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const ERROR_ACCESS_DENIED: i32 = 5;
    const STILL_ACTIVE: u32 = 259;

    extern "system" {
        fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> *mut c_void;
        fn GetExitCodeProcess(process: *mut c_void, exit_code: *mut u32) -> i32;
        fn CloseHandle(object: *mut c_void) -> i32;
    }

    pub(super) fn pid_is_alive(pid: u32) -> bool {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            // ERROR_ACCESS_DENIED means the process exists but sits in a
            // context we may not open — the same distinction the Unix branch
            // draws for EPERM. An unused PID fails with
            // ERROR_INVALID_PARAMETER instead, which is a real "dead".
            return std::io::Error::last_os_error().raw_os_error() == Some(ERROR_ACCESS_DENIED);
        }

        let mut exit_code: u32 = 0;
        let queried = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
        unsafe {
            CloseHandle(handle);
        }

        // A failed query leaves liveness unknown, so report alive rather than
        // hand the lock to a second sync on a guess. A process that exited
        // with code 259 is likewise read as alive: STILL_ACTIVE is
        // indistinguishable from that exit status, and erring toward "alive"
        // is the direction the lock policy wants.
        queried == 0 || exit_code == STILL_ACTIVE
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    pub(super) fn pid_is_alive(_pid: u32) -> bool {
        // No liveness probe on this platform, so nothing here can prove death.
        // Reporting "alive" blocks a sync until the lock file is removed by
        // hand; reporting "dead" would silently allow the overlapping syncs
        // this module exists to prevent. Unreachable for every target
        // tokscale releases — all of them are Unix or Windows.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn pid_zero_is_never_alive() {
        assert!(!pid_is_alive(0));
    }

    /// Runs on Windows too. It was previously `#[cfg(unix)]` in `trae.rs`,
    /// which is precisely why the always-false Windows stub went unnoticed.
    #[test]
    fn current_process_is_alive() {
        assert!(pid_is_alive(std::process::id()));
    }

    #[test]
    fn exited_process_is_not_alive() {
        #[cfg(windows)]
        let mut child = Command::new("cmd")
            .args(["/C", "exit"])
            .spawn()
            .expect("spawn a process that exits immediately");
        #[cfg(unix)]
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn a process that exits immediately");

        let pid = child.id();
        child.wait().expect("child exits");

        // `child` is deliberately still in scope: on Windows an open process
        // handle pins the PID, so it cannot be recycled by an unrelated
        // process between the wait and the probe.
        assert!(!pid_is_alive(pid));
        drop(child);
    }
}
