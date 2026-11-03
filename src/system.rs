use crate::validation::{Error, Result};
use std::{
    io::{ErrorKind, Read},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::process::{CommandExt, ExitStatusExt},
    },
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

pub struct CommandResult {
    pub exit_code: i32,
    pub stdout: String,
}

pub fn wait_fd(fd: i32, events: i16, deadline: Instant) -> std::io::Result<()> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ErrorKind::TimedOut.into());
        }
        let mut item = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        // poll borrows one valid descriptor for a finite absolute-deadline wait.
        let status = unsafe {
            libc::poll(
                &mut item,
                1,
                remaining.as_millis().clamp(1, i32::MAX as u128) as i32,
            )
        };
        if status > 0 {
            return if item.revents & libc::POLLNVAL != 0 {
                Err(ErrorKind::InvalidInput.into())
            } else {
                Ok(())
            };
        }
        if status == 0 {
            continue;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

pub fn run(args: &[&str], cwd: Option<&Path>, check: bool) -> Result<CommandResult> {
    run_with_timeout(args, cwd, check, Duration::from_secs(60))
}

fn wait_without_pidfd(child: &mut std::process::Child, deadline: Instant) -> std::io::Result<()> {
    // Older/restricted Linux hosts still need the same finite command deadline.
    loop {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let status = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if status != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if unsafe { info.si_pid() } == child.id() as i32 {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ErrorKind::TimedOut.into());
        }
        thread::sleep(remaining.min(Duration::from_millis(1)));
    }
}

fn wait_child(child: &mut Child, timeout: Duration) -> std::io::Result<(ExitStatus, bool)> {
    wait_child_with_cleanup(child, timeout, false, true)
}
fn wait_child_with_cleanup(
    child: &mut Child,
    timeout: Duration,
    terminate_remaining: bool,
    owned_group: bool,
) -> std::io::Result<(ExitStatus, bool)> {
    let pid = child.id() as i32;
    let deadline = Instant::now() + timeout;
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    let waited = if descriptor < 0 {
        wait_without_pidfd(child, deadline)
    } else {
        let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
        wait_fd(descriptor.as_raw_fd(), libc::POLLIN, deadline)
    };
    if waited.is_err() || terminate_remaining {
        // A shared hook group belongs to our caller: signal only this owned child.
        unsafe {
            libc::kill(if owned_group { -pid } else { pid }, libc::SIGKILL);
        }
    }
    let status = child.wait()?;
    match waited {
        Ok(()) => Ok((status, false)),
        Err(error) if error.kind() == ErrorKind::TimedOut => Ok((status, true)),
        Err(error) => Err(error),
    }
}

pub struct Execution {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
}
pub fn run_logged(
    args: &[&str],
    cwd: &Path,
    env: &serde_json::Map<String, serde_json::Value>,
    timeout: Duration,
    output: &std::fs::File,
    hook: bool,
) -> Result<Execution> {
    let mut command = Command::new(args[0]);
    command
        .args(&args[1..])
        .current_dir(cwd)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(
            output
                .try_clone()
                .map_err(|_| Error::from("Could not prepare private command output"))?,
        )
        .stderr(
            output
                .try_clone()
                .map_err(|_| Error::from("Could not prepare private command output"))?,
        );
    if hook {
        for (key, _) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if [
                "GRAFTR_LOCAL_PORT",
                "GRAFTR_HTTPS_PORT",
                "GRAFTR_URL",
                "GRAFTR_CLEANUP_RUN_ID",
                "GRAFTR_RETIRE_RUN_ID",
            ]
            .contains(&name.as_ref())
                || name.starts_with("GRAFTR_PORT_")
            {
                command.env_remove(key);
            }
        }
    }
    for (key, value) in env {
        command.env(key, value.as_str().expect("validated command environment"));
    }
    let mut child = command.spawn().map_err(|_| {
        Error::from("Configured command could not start; command output was not logged")
    })?;
    let (status, timed_out) = wait_child_with_cleanup(&mut child, timeout, hook, true)
        .map_err(|_| Error::from("Could not wait for owned command"))?;
    Ok(Execution {
        exit_code: status.code(),
        signal: status.signal(),
        timed_out,
    })
}

pub fn probe_command(
    args: &[&str],
    cwd: &Path,
    overrides: &serde_json::Map<String, serde_json::Value>,
    timeout: Duration,
) -> serde_json::Value {
    use serde_json::json;
    let mut command = Command::new(args[0]);
    command
        .args(&args[1..])
        .current_dir(cwd)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in overrides {
        command.env(key, value.as_str().expect("validated override"));
    }
    let Ok(mut child) = command.spawn() else {
        return json!({"ok": false, "reason": "command-unavailable"});
    };
    match wait_child(&mut child, timeout) {
        Ok((_, true)) => json!({"ok": false, "reason": "timeout"}),
        Ok((status, false)) if status.success() => json!({"ok": true, "exitCode": 0}),
        Ok((status, false)) => {
            let mut result = json!({"ok": false, "reason": "command-failed"});
            if let Some(code) = status.code() {
                result["exitCode"] = json!(code);
            }
            result
        }
        Err(_) => json!({"ok": false, "reason": "command-unavailable"}),
    }
}

pub fn run_with_timeout(
    args: &[&str],
    cwd: Option<&Path>,
    check: bool,
    timeout: Duration,
) -> Result<CommandResult> {
    run_scoped(args, cwd, check, timeout, true)
}
pub fn run_inherited(args: &[&str]) -> Result<CommandResult> {
    run_scoped(args, None, true, Duration::from_secs(60), false)
}
fn run_scoped(
    args: &[&str],
    cwd: Option<&Path>,
    check: bool,
    timeout: Duration,
    owned_group: bool,
) -> Result<CommandResult> {
    let executable = args
        .first()
        .ok_or_else(|| Error::from("Missing command executable"))?;
    let label = Path::new(executable)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let mut command = Command::new(executable);
    command
        .args(&args[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if owned_group {
        command.process_group(0);
    }
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let mut child = command.spawn().map_err(|_| {
        Error::from(format!(
            "{label} could not start; subprocess output was not logged"
        ))
    })?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let output = thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = {
            let mut stream = stdout;
            stream.read_to_end(&mut bytes)
        };
        result.map(|_| bytes)
    });
    let errors = thread::spawn(move || {
        let mut stream = stderr;
        std::io::copy(&mut stream, &mut std::io::sink())
    });
    let waited = wait_child_with_cleanup(&mut child, timeout, false, owned_group);
    let stdout = output
        .join()
        .map_err(|_| Error::from("Could not collect subprocess output"))?
        .map_err(|_| Error::from("Could not read subprocess output"))?;
    errors
        .join()
        .map_err(|_| Error::from("Could not collect subprocess errors"))?
        .map_err(|_| Error::from("Could not read subprocess errors"))?;
    let (status, timed_out) =
        waited.map_err(|_| Error::from("Could not wait for owned subprocess"))?;
    if timed_out {
        return Err(format!("{label} timed out; subprocess output was not logged").into());
    }
    let exit_code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
    if check && exit_code != 0 {
        return Err(format!(
            "{label} failed with exit {exit_code}; subprocess output was not logged"
        )
        .into());
    }
    Ok(CommandResult {
        exit_code,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
    })
}

pub fn git(checkout: &Path, args: &[&str]) -> Result<String> {
    let path = checkout
        .to_str()
        .ok_or_else(|| Error::from("Git path is not valid UTF-8"))?;
    Ok(run(&[&["git", "-C", path], args].concat(), None, true)?
        .stdout
        .trim_matches(crate::validation::js_space)
        .into())
}
pub fn which(executable: &str, cwd: &Path) -> Option<std::path::PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let candidates: Vec<_> = if executable.contains('/') {
        vec![cwd.join(executable)]
    } else {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|part| cwd.join(part).join(executable))
            .collect()
    };
    candidates
        .into_iter()
        .find(|path| {
            path.is_file()
                && std::ffi::CString::new(path.as_os_str().as_bytes())
                    .is_ok_and(|path| unsafe { libc::access(path.as_ptr(), libc::X_OK) } == 0)
        })
        .map(|p| std::path::PathBuf::from(crate::validation::normalized_path(&p.to_string_lossy())))
}
pub fn port_free(port: u16) -> bool {
    fn can_bind(port: u16, ipv6: bool) -> bool {
        let domain = if ipv6 { libc::AF_INET6 } else { libc::AF_INET };
        let fd = unsafe { libc::socket(domain, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return ipv6
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::EAFNOSUPPORT);
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let enabled = 1_i32;
        let option = |level, option| unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                level,
                option,
                (&enabled as *const i32).cast(),
                std::mem::size_of_val(&enabled) as _,
            )
        };
        if option(libc::SOL_SOCKET, libc::SO_REUSEADDR) != 0
            || ipv6 && option(libc::IPPROTO_IPV6, libc::IPV6_V6ONLY) != 0
        {
            return false;
        }
        let status = if ipv6 {
            let address = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as _,
                sin6_port: port.to_be(),
                sin6_flowinfo: 0,
                sin6_addr: libc::in6_addr { s6_addr: [0; 16] },
                sin6_scope_id: 0,
            };
            unsafe {
                libc::bind(
                    fd.as_raw_fd(),
                    (&address as *const libc::sockaddr_in6).cast(),
                    std::mem::size_of_val(&address) as _,
                )
            }
        } else {
            let address = libc::sockaddr_in {
                sin_family: libc::AF_INET as _,
                sin_port: port.to_be(),
                sin_addr: libc::in_addr { s_addr: 0 },
                sin_zero: [0; 8],
            };
            unsafe {
                libc::bind(
                    fd.as_raw_fd(),
                    (&address as *const libc::sockaddr_in).cast(),
                    std::mem::size_of_val(&address) as _,
                )
            }
        };
        if status != 0 {
            return ipv6
                && [Some(libc::EAFNOSUPPORT), Some(libc::EADDRNOTAVAIL)]
                    .contains(&std::io::Error::last_os_error().raw_os_error());
        }
        (unsafe { libc::listen(fd.as_raw_fd(), 1) }) == 0
    }
    can_bind(port, false) && can_bind(port, true)
}

pub fn uuid() -> Result<String> {
    let mut bytes = [0_u8; 16];
    let mut offset = 0;
    while offset < bytes.len() {
        let count = unsafe {
            libc::getrandom(bytes[offset..].as_mut_ptr().cast(), bytes.len() - offset, 0)
        };
        if count < 0 {
            if std::io::Error::last_os_error().kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err("Secure random source is unavailable".into());
        }
        if count == 0 {
            return Err("Secure random source is unavailable".into());
        }
        offset += count as usize;
    }
    bytes[6] = (bytes[6] & 15) | 64;
    bytes[8] = (bytes[8] & 63) | 128;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runs_literal_arguments_and_never_returns_private_errors() {
        assert_eq!(
            run(&["printf", "%s", "$(touch not-executed)"], None, true)
                .unwrap()
                .stdout,
            "$(touch not-executed)"
        );
        let error = run(
            &["sh", "-c", "printf private-value >&2; exit 7"],
            None,
            true,
        )
        .err()
        .unwrap();
        assert!(!error.message.contains("private-value"));
        assert!(error.message.contains("exit 7"));
        assert_eq!(
            run(&["sh", "-c", "kill -TERM $$"], None, false)
                .unwrap()
                .exit_code,
            143
        );
    }
    #[test]
    fn timeout_is_finite_and_kills_only_the_owned_command_group() {
        let started = Instant::now();
        let error = run_with_timeout(
            &["sh", "-c", "sleep 60"],
            None,
            true,
            Duration::from_millis(30),
        )
        .err()
        .unwrap();
        assert!(error.message.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(run(&["true"], None, true).unwrap().exit_code, 0);
    }
    #[test]
    fn identifiers_are_random_uuid4_values() {
        let first = uuid().unwrap();
        let second = uuid().unwrap();
        assert_ne!(first, second);
        assert_eq!(first.len(), 36);
        assert_eq!(&first[14..15], "4");
    }

    #[test]
    fn fallback_wait_has_the_same_success_and_timeout_contract() {
        let mut completed = Command::new("true").spawn().unwrap();
        wait_without_pidfd(&mut completed, Instant::now() + Duration::from_secs(1)).unwrap();
        assert!(completed.wait().unwrap().success());
        let mut pending = Command::new("sleep").arg("60").spawn().unwrap();
        let result = wait_without_pidfd(&mut pending, Instant::now() + Duration::from_millis(10));
        pending.kill().unwrap();
        pending.wait().unwrap();
        assert_eq!(result.unwrap_err().kind(), ErrorKind::TimedOut);
    }
    #[test]
    fn resource_clients_inherit_the_hook_group() {
        let output =
            run_inherited(&["/usr/bin/python3", "-c", "import os; print(os.getpgrp())"]).unwrap();
        assert_eq!(output.stdout.trim().parse::<i32>().unwrap(), unsafe {
            libc::getpgrp()
        });
    }
}
