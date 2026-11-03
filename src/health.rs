use crate::{
    checkout, human, locks, paths::Paths, profiles, records, system, terminal, validation::*,
};
use serde_json::{Value, json};
use std::{
    io::{self, Read, Write},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream},
    path::Path,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub fn timestamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch");
    chrono::DateTime::from_timestamp(now.as_secs() as i64, now.subsec_nanos())
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
fn remaining(deadline: Instant) -> io::Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(io::ErrorKind::TimedOut.into())
    } else {
        Ok(remaining)
    }
}
fn network_probe(port: u16, check: &Value) -> Value {
    let timeout = Duration::from_millis(check["timeoutMs"].as_f64().unwrap() as u64);
    let deadline = Instant::now() + timeout;
    let inspect = || -> io::Result<Option<u16>> {
        // No DNS, proxies, redirects, authentication or response body collection.
        let mut stream = TcpStream::connect_timeout(
            &SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)),
            remaining(deadline)?,
        )?;
        if check["type"] == "tcp" {
            return Ok(None);
        }
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n",
            check["path"].as_str().unwrap()
        );
        let mut offset = 0;
        while offset < request.len() {
            stream.set_write_timeout(Some(remaining(deadline)?))?;
            match stream.write(&request.as_bytes()[offset..]) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(size) => offset += size,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => (),
                Err(error) => return Err(error),
            }
        }
        let mut headers = Vec::new();
        let mut bytes = [0_u8; 1024];
        loop {
            stream.set_read_timeout(Some(remaining(deadline)?))?;
            let count = match stream.read(&mut bytes) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            headers.extend_from_slice(&bytes[..count]);
            if headers.len() > 64 * 1024 {
                return Err(io::ErrorKind::InvalidData.into());
            }
            while let Some(end) = headers.windows(4).position(|b| b == b"\r\n\r\n") {
                let first = headers.split(|b| *b == b'\n').next().unwrap();
                let first = std::str::from_utf8(first)
                    .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
                let mut fields = first.split_ascii_whitespace();
                let version = fields.next().unwrap_or("");
                let status = fields
                    .next()
                    .filter(|s| s.len() == 3)
                    .and_then(|s| s.parse::<u16>().ok())
                    .filter(|s| *s >= 100);
                if !["HTTP/1.0", "HTTP/1.1"].contains(&version) || status.is_none() {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                let status = status.unwrap();
                if status >= 200 || status == 101 {
                    return Ok(Some(status));
                }
                headers.drain(..end + 4);
            }
        }
    };
    match inspect() {
        Ok(None) => json!({"ok": true}),
        Ok(Some(status)) if check["expectedStatus"].as_f64() == Some(status.into()) => {
            json!({"ok": true, "httpStatus": status})
        }
        Ok(Some(status)) => {
            json!({"ok": false, "reason": "unexpected-status", "httpStatus": status})
        }
        Err(error) => {
            json!({"ok": false, "reason": if [io::ErrorKind::TimedOut, io::ErrorKind::WouldBlock].contains(&error.kind()) { "timeout" } else { "connection-failed" }})
        }
    }
}
fn check(paths: &Paths, task: &Value, checks: &[Value]) -> Result<Value> {
    let setup = records::setup_for(
        &paths.state,
        task["socket"].as_str(),
        checkout::string(task, "path"),
    )?;
    let mut results = Vec::new();
    for check in checks {
        let started = Instant::now();
        let mut outcome = if check["type"] == "command" {
            let args: Vec<_> = check["command"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            system::probe_command(
                &args,
                Path::new(checkout::string(task, "path")),
                task["overrides"].as_object().unwrap(),
                Duration::from_millis(check["timeoutMs"].as_f64().unwrap() as u64),
            )
        } else {
            network_probe(task["localPort"].as_f64().unwrap() as u16, check)
        };
        let ok = outcome.as_object_mut().unwrap().remove("ok").unwrap() == true;
        outcome["name"] = check["name"].clone();
        outcome["type"] = check["type"].clone();
        outcome["status"] = json!(if ok { "healthy" } else { "unhealthy" });
        outcome["elapsedMs"] = json!(started.elapsed().as_secs_f64() * 1000.0);
        results.push(outcome);
    }
    let status = if results.is_empty() {
        "not-configured"
    } else if results.iter().all(|r| r["status"] == "healthy") {
        "healthy"
    } else {
        "unhealthy"
    };
    Ok(
        json!({"id": task["id"], "taskStatus": task["status"], "setupStatus": setup.as_ref().map_or("unknown", |r| r.value()["status"].as_str().unwrap()),
        "health": {"status": status, "checkedAt": timestamp(), "checks": results}}),
    )
}
pub fn inspect(paths: &Paths, id: &str) -> Result<Value> {
    let initial = records::task(&paths.state, id)?;
    let initial = initial.value();
    let profile = profiles::configured(paths, checkout::string(initial, "project"))?;
    ensure(
        profile.value()["repo"] == initial["repo"],
        "Project profile no longer owns this repository",
    )?;
    let checks = profile.value()["healthChecks"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if checks.is_empty() {
        return check(paths, initial, checks);
    }
    let _lock = locks::worktree(paths, checkout::string(initial, "path"), true)?;
    let task = records::task(&paths.state, id)?;
    let task = task.value();
    checkout::require_identity(task, initial)?;
    ensure(
        checkout::canonical(checkout::string(task, "path"))? == checkout::string(task, "path"),
        "Health checks require the recorded canonical checkout",
    )?;
    checkout::verify(task)?;
    ensure(
        checkout::registered(task)?.is_some_and(|tree| {
            !tree.detached && tree.branch.as_deref() == task["branch"].as_str()
        }),
        "Health checkout registration/branch changed; review ownership first",
    )?;
    check(paths, task, checks)
}
pub fn format(report: &Value, columns: usize) -> String {
    let status = report["health"]["status"].as_str().unwrap();
    let heading = human::fields(
        &format!("{}: health {status}", checkout::string(report, "id")),
        &[
            (
                "Worktree",
                Some(checkout::string(report, "taskStatus").into()),
            ),
            (
                "Setup",
                Some(checkout::string(report, "setupStatus").into()),
            ),
        ],
        (status == "not-configured").then_some("No application health checks are configured."),
        status,
        columns,
    );
    let rows: Vec<_> = report["health"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|check| {
            let mut details = Vec::new();
            if let Some(reason) = check["reason"].as_str() {
                details.push(reason.replace('-', " "));
            }
            if let Some(status) = check["httpStatus"].as_u64() {
                details.push(format!("HTTP {status}"));
            }
            if let Some(code) = check["exitCode"].as_i64() {
                details.push(format!("exit {code}"));
            }
            details.push(format!(
                "{} ms",
                check["elapsedMs"].as_f64().unwrap().round() as u64
            ));
            vec![
                checkout::string(check, "name").into(),
                checkout::string(check, "status").into(),
                details.join("; "),
            ]
        })
        .collect();
    terminal::wrap(
        &if rows.is_empty() {
            heading
        } else {
            format!(
                "{heading}\n\n{}",
                terminal::table(&["CHECK", "RESULT", "DETAILS"], &rows, columns)
            )
        },
        columns,
    )
}
