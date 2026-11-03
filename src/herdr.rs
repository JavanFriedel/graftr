use crate::{system, validation::*};
use serde_json::{Value, json};
use std::{
    env,
    io::{ErrorKind, Read, Write},
    mem,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, net::UnixStream},
    },
    path::Path,
    time::{Duration, Instant},
};

pub fn caller_socket() -> Result<String> {
    if env::var("HERDR_ENV").as_deref() != Ok("1") {
        return Err("Run inside Herdr; no focused-session fallback is used".into());
    }
    env::var("HERDR_SOCKET_PATH")
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "Run inside Herdr; no focused-session fallback is used".into())
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    pub workspace_id: String,
    pub label: String,
    pub checkout_path: Option<String>,
}
pub struct Opened {
    pub workspace: Workspace,
    pub already_open: bool,
}
pub fn opened(reply: &Value) -> Result<Opened> {
    ensure(
        [Some("worktree_created"), Some("worktree_opened")].contains(&reply["type"].as_str()),
        "Unexpected Herdr worktree response",
    )?;
    ensure(
        reply["type"] != "worktree_opened" || reply["already_open"].is_boolean(),
        "Missing Herdr open status",
    )?;
    text(&reply["tab"]["tab_id"])?;
    let mut spaces = workspaces(&json!({"workspaces": [reply["workspace"]]}))?;
    Ok(Opened {
        workspace: spaces.remove(0),
        already_open: reply["type"] == "worktree_opened" && reply["already_open"] == true,
    })
}
pub struct Tab {
    pub tab_id: String,
    pub label: String,
    pub pane_count: f64,
}
pub fn tabs(reply: &Value) -> Result<Vec<Tab>> {
    list(field(object(reply)?, "tabs")?)?
        .iter()
        .map(|value| {
            let value = object(value)?;
            let count = field(value, "pane_count")?
                .as_f64()
                .filter(|n| n.fract() == 0.0)
                .ok_or_else(|| Error::from("Invalid Herdr pane count"))?;
            Ok(Tab {
                tab_id: string(value, "tab_id")?.into(),
                label: string(value, "label")?.into(),
                pane_count: count,
            })
        })
        .collect()
}

pub fn workspaces(reply: &Value) -> Result<Vec<Workspace>> {
    list(field(object(reply)?, "workspaces")?)?
        .iter()
        .map(|value| {
            let value = object(value)?;
            let checkout_path = match value.get("worktree") {
                None | Some(Value::Null) => None,
                Some(worktree) => Some(string(object(worktree)?, "checkout_path")?.into()),
            };
            Ok(Workspace {
                workspace_id: string(value, "workspace_id")?.into(),
                label: string(value, "label")?.into(),
                checkout_path,
            })
        })
        .collect()
}

fn unavailable(message: impl Into<String>) -> Error {
    Error {
        code: "HERDR_UNAVAILABLE",
        message: message.into(),
    }
}
fn connection_unavailable() -> Error {
    Error {
        code: "HERDR_CONNECTION_UNAVAILABLE",
        message: "Herdr socket is unavailable".into(),
    }
}

fn connect(path: &Path, deadline: Instant) -> Result<UnixStream> {
    let bytes = path.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { mem::zeroed() };
    ensure(
        bytes.len() < address.sun_path.len() && !bytes.contains(&0),
        "Invalid Herdr socket path",
    )?;
    address.sun_family = libc::AF_UNIX as _;
    for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
        *target = *byte as libc::c_char;
    }
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        return Err(unavailable("Could not create Herdr connection"));
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let status = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            mem::size_of_val(&address) as _,
        )
    };
    if status != 0 {
        let error = std::io::Error::last_os_error();
        if ![Some(libc::EINPROGRESS), Some(libc::EAGAIN)].contains(&error.raw_os_error()) {
            return Err(connection_unavailable());
        }
        system::wait_fd(fd.as_raw_fd(), libc::POLLOUT, deadline)
            .map_err(|_| unavailable("Herdr connection timed out"))?;
        let mut error = 0_i32;
        let mut length = mem::size_of_val(&error) as libc::socklen_t;
        let status = unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&mut error as *mut i32).cast(),
                &mut length,
            )
        };
        if status != 0 || error != 0 {
            return Err(connection_unavailable());
        }
    }
    let stream = UnixStream::from(fd);
    stream
        .set_nonblocking(false)
        .map_err(|_| unavailable("Could not prepare Herdr connection"))?;
    Ok(stream)
}

pub fn request(
    socket: Option<&str>,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value> {
    let socket = socket.ok_or_else(|| {
        Error::from("No Herdr attachment; this action requires an explicitly attached workspace")
    })?;
    let deadline = Instant::now() + timeout;
    let mut connection = connect(Path::new(socket), deadline)?;
    let id = format!("graftr:{}", system::uuid()?);
    let request = format!(
        "{}\n",
        json!({"id": id, "method": method, "params": params})
    );
    let timed_out = || unavailable(format!("Herdr {method} timed out"));
    let mut offset = 0;
    while offset < request.len() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(timed_out());
        }
        connection
            .set_write_timeout(Some(remaining))
            .map_err(|_| timed_out())?;
        match connection.write(&request.as_bytes()[offset..]) {
            Ok(0) => return Err(unavailable("Herdr closed the connection before responding")),
            Ok(count) => offset += count,
            Err(error) if error.kind() == ErrorKind::Interrupted => (),
            Err(_) => return Err(timed_out()),
        }
    }
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(timed_out());
        }
        connection
            .set_read_timeout(Some(remaining))
            .map_err(|_| timed_out())?;
        let previous = buffer.len();
        match connection.read(&mut chunk) {
            Ok(0) => return Err(unavailable("Herdr closed the connection before responding")),
            Ok(count) => buffer.extend_from_slice(&chunk[..count]),
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return Err(timed_out()),
        }
        ensure(
            buffer.len() <= 4 * 1024 * 1024,
            "Herdr response exceeds size limit",
        )?;
        // Earlier chunks had no delimiter; rescanning them makes large replies quadratic.
        let Some(boundary) = buffer[previous..]
            .iter()
            .position(|b| *b == b'\n')
            .map(|index| previous + index)
        else {
            continue;
        };
        let reply: Value = serde_json::from_slice(&buffer[..boundary]).map_err(|_| Error { code: "INVALID_JSON", message: "Invalid JSON; review configuration/state or the Herdr response (contents were not logged)".into() })?;
        let reply = object(&reply)?;
        ensure(
            reply.get("id").and_then(Value::as_str) == Some(&id),
            "Herdr response identity mismatch",
        )?;
        if let Some(error) = reply.get("error") {
            let error = object(error)?;
            return Err(format!(
                "Herdr {method}: {}: {}",
                string(error, "code")?,
                string(error, "message")?
            )
            .into());
        }
        return Ok(Value::Object(object(field(reply, "result")?)?.clone()));
    }
}

pub fn in_plugin_action() -> bool {
    let (Ok(action), Ok(root), Ok(directory)) = (
        env::var("HERDR_PLUGIN_ACTION_ID"),
        env::var("HERDR_PLUGIN_ROOT"),
        env::current_dir(),
    ) else {
        return false;
    };
    !action.is_empty() && directory.to_str() == Some(&root)
}

pub fn caller_workspace(socket: Option<&str>) -> Result<Option<String>> {
    let Some(socket) = socket else {
        return Ok(None);
    };
    if env::var("HERDR_ENV").as_deref() != Ok("1")
        || env::var("HERDR_SOCKET_PATH").as_deref() != Ok(socket)
    {
        return Ok(None);
    }
    if !in_plugin_action()
        && let Ok(pane) = env::var("HERDR_PANE_ID")
        && !pane.is_empty()
    {
        let reply = request(
            Some(socket),
            "pane.current",
            json!({"caller_pane_id": pane}),
            Duration::from_secs(60),
        )?;
        return Ok(Some(
            string(object(field(object(&reply)?, "pane")?)?, "workspace_id")?.into(),
        ));
    }
    Ok(env::var("HERDR_WORKSPACE_ID").ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader},
        os::unix::{fs::PermissionsExt, net::UnixListener},
        thread,
    };

    struct SocketPath(std::path::PathBuf);
    impl std::ops::Deref for SocketPath {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for SocketPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
        }
    }
    fn fixture_socket() -> (SocketPath, UnixListener) {
        let directory =
            std::env::temp_dir().join(format!("graftr-rpc-{}", system::uuid().unwrap()));
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.join("s");
        let listener = UnixListener::bind(&path).unwrap();
        (SocketPath(path), listener)
    }

    #[test]
    fn trickled_response_cannot_extend_the_absolute_deadline() {
        let (path, listener) = fixture_socket();
        let server = thread::spawn(move || {
            let (mut peer, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(peer.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            for _ in 0..100 {
                if peer.write_all(b" ").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
        });
        let started = Instant::now();
        let error = request(
            path.to_str(),
            "workspace.list",
            json!({}),
            Duration::from_millis(40),
        )
        .err()
        .unwrap();
        assert_eq!(error.code, "HERDR_UNAVAILABLE");
        assert!(started.elapsed() < Duration::from_millis(500));
        server.join().unwrap();
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn early_eof_is_unavailable_and_oversized_data_is_refused_without_echo() {
        for oversized in [false, true] {
            let (path, listener) = fixture_socket();
            let server = thread::spawn(move || {
                let (mut peer, _) = listener.accept().unwrap();
                let mut request = String::new();
                BufReader::new(peer.try_clone().unwrap())
                    .read_line(&mut request)
                    .unwrap();
                if oversized {
                    let _ = peer.write_all(&vec![b'x'; 4 * 1024 * 1024 + 1]);
                }
            });
            let error = request(
                path.to_str(),
                "workspace.list",
                json!({}),
                Duration::from_secs(2),
            )
            .err()
            .unwrap();
            assert_eq!(
                error.code,
                if oversized {
                    "OPERATION_FAILED"
                } else {
                    "HERDR_UNAVAILABLE"
                }
            );
            assert!(error.message.len() < 150);
            server.join().unwrap();
            std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
    }
}
