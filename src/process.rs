//! Process identity and liveness: a pid plus its start time and the boot it
//! belongs to, so a recycled pid is never mistaken for the process Dispatch
//! recorded. Used by attached work (owner and agent), `serve` adoption, native
//! crash repair and the executor's process-group cleanup.

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start: Option<String>,
    pub boot: Option<String>,
    pub process_group: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityState {
    ExactLive,
    Gone,
    Reused,
    Unknown,
}

impl ProcessIdentity {
    pub fn current() -> Self {
        process_identity(std::process::id())
    }
}

pub fn process_identity(pid: u32) -> ProcessIdentity {
    ProcessIdentity {
        pid,
        start: process_start(pid),
        boot: boot_identity(),
        process_group: i32::try_from(pid).ok(),
    }
}

pub fn identity_state(expected: &ProcessIdentity) -> IdentityState {
    if !pid_exists(expected.pid) {
        return IdentityState::Gone;
    }
    let current = process_identity(expected.pid);
    match (
        &expected.start,
        &expected.boot,
        &current.start,
        &current.boot,
    ) {
        (Some(a), Some(b), Some(c), Some(d)) if a == c && (b == d || drifting_boot(b)) => {
            IdentityState::ExactLive
        }
        (Some(_), Some(_), Some(_), Some(_)) => IdentityState::Reused,
        _ => IdentityState::Unknown,
    }
}

/// Before 0.4.6, macOS identities recorded `kern.boottime`, which drifts while
/// the machine runs, so it can no longer be compared. Such a record is matched
/// by its pid and exact start time alone; a recycled pid does not share both.
fn drifting_boot(boot: &str) -> bool {
    boot.starts_with("{ sec = ")
}

pub fn process_group_exists(group: Option<i32>) -> Option<bool> {
    #[cfg(unix)]
    {
        let group = group?;
        // SAFETY: signal zero only queries the process-group existence.
        let result = unsafe { libc::kill(-group, 0) };
        if result == 0 {
            Some(true)
        } else {
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::ESRCH) => Some(false),
                Some(libc::EPERM) => Some(true),
                _ => None,
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = group;
        None
    }
}

pub(crate) fn pid_exists(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = i32::try_from(pid) else {
            return false;
        };
        // SAFETY: signal zero does not modify the target process.
        unsafe {
            libc::kill(pid, 0) == 0
                || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

#[cfg(target_os = "linux")]
fn boot_identity() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|value| value.trim().to_owned())
}

#[cfg(target_os = "linux")]
fn process_start(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = stat.rsplit_once(')')?.1;
    tail.split_whitespace().nth(19).map(str::to_owned)
}

/// The boot session's UUID: fixed for the life of a boot, as Linux's
/// `boot_id` is. (`kern.boottime` is not: the clock adjusts it by fractions of
/// a second while the machine runs.)
#[cfg(target_os = "macos")]
fn boot_identity() -> Option<String> {
    command_output("/usr/sbin/sysctl", &["-n", "kern.bootsessionuuid"])
}

#[cfg(target_os = "macos")]
fn process_start(pid: u32) -> Option<String> {
    command_output("/bin/ps", &["-o", "lstart=", "-p", &pid.to_string()])
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn boot_identity() -> Option<String> {
    None
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_start(_pid: u32) -> Option<String> {
    None
}

#[cfg(target_os = "macos")]
fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_with_the_old_drifting_boot_time_still_names_its_process() {
        let mut identity = ProcessIdentity::current();
        if identity.start.is_none() || identity.boot.is_none() {
            return;
        }
        identity.boot = Some("{ sec = 1788733769, usec = 196719 } Sun Sep  6 18:29:29 2026".into());
        assert_eq!(identity_state(&identity), IdentityState::ExactLive);
        identity.start = Some("Thu Jan  1 00:00:00 1970".into());
        assert_eq!(identity_state(&identity), IdentityState::Reused);
        identity.boot = Some("another boot".into());
        assert_eq!(identity_state(&identity), IdentityState::Reused);
    }

    #[test]
    fn current_process_identity_is_exact_when_supported() {
        let identity = ProcessIdentity::current();
        if identity.start.is_some() && identity.boot.is_some() {
            assert_eq!(identity_state(&identity), IdentityState::ExactLive);
        }
    }
}
