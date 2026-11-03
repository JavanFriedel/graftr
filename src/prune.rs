use crate::{
    history, locks,
    paths::{self, Paths},
    records, terminal,
    validation::*,
};
use chrono::SecondsFormat;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

struct Entry {
    category: &'static str,
    path: PathBuf,
    bytes: u64,
    modified: SystemTime,
}
fn names(directory: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for item in
        fs::read_dir(directory).map_err(|_| Error::from("Could not read retained directory"))?
    {
        names.push(
            item.map_err(|_| Error::from("Could not read retained directory"))?
                .file_name()
                .to_string_lossy()
                .into_owned(),
        );
    }
    Ok(names)
}
fn children(category: &'static str, directory: &Path) -> Result<Vec<Entry>> {
    if !paths::exists(directory)? {
        return Ok(Vec::new());
    }
    records::private_dir(directory)?;
    let mut entries = Vec::new();
    for name in names(directory)? {
        let path = directory.join(name);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|_| Error::from("Could not inspect retained entry"))?;
        if !metadata.is_file() && !metadata.is_dir() {
            continue;
        }
        let (bytes, modified) = if metadata.is_dir() {
            history::tree(&path, &metadata).map_err(Error::from)?
        } else {
            (
                metadata.len(),
                metadata
                    .modified()
                    .map_err(|_| Error::from("Could not inspect retained entry age"))?,
            )
        };
        entries.push(Entry {
            category,
            path,
            bytes,
            modified,
        });
    }
    Ok(entries)
}
fn entries(paths: &Paths) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for (category, name) in history::DIRECTORIES {
        entries.extend(children(category, &paths.state.join(name))?);
    }
    let logs = paths.state.join("logs");
    if paths::exists(&logs)? {
        records::private_dir(&logs)?;
        for name in names(&logs)? {
            let directory = logs.join(name);
            if fs::symlink_metadata(&directory)
                .map_err(|_| Error::from("Could not inspect retained log directory"))?
                .is_dir()
            {
                entries.extend(children("logs", &directory)?);
            }
        }
    }
    Ok(entries)
}
fn live_keys(paths: &Paths) -> Result<HashSet<String>> {
    let mut keys = HashSet::new();
    for task in records::tasks(&paths.state)? {
        for kind in ["cleanup", "retire"] {
            if let Some(plan) = task.value().get(kind) {
                keys.insert(format!(
                    "{:x}",
                    Sha256::digest(
                        serde_json::to_vec(&json!([task.value()["id"], plan["runId"]])).unwrap()
                    )
                ));
            }
        }
    }
    Ok(keys)
}
fn keep_reason(entry: &Entry, live: &HashSet<String>) -> Result<Option<String>> {
    let name = entry.path.file_name().unwrap().to_string_lossy();
    if ["hook-work", "tailscale-hooks", "postgres-hooks"].contains(&entry.category)
        && live.contains(name.as_ref())
    {
        return Ok(Some("a current task still uses it".into()));
    }
    if !["tailscale-hooks", "postgres-hooks"].contains(&entry.category) {
        return Ok(None);
    }
    for name in names(&entry.path)?.iter().filter(|s| s.ends_with(".json")) {
        let value = records::read_private(&entry.path.join(name))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        let Some(value) = value.filter(Value::is_object) else {
            return Ok(Some(
                "a receipt could not be read; review it by hand".into(),
            ));
        };
        if entry.category == "tailscale-hooks" && value["route"] != "none" {
            let route = value.get("route").map_or("undefined".into(), |v| {
                v.as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| v.to_string())
            });
            return Ok(Some(format!("it still records a {route} Serve route")));
        }
        if entry.category == "postgres-hooks" && value["state"] != "dropped" {
            return Ok(Some(
                "it still records a database that was not dropped".into(),
            ));
        }
    }
    Ok(None)
}
fn unrecognized(paths: &Paths) -> Result<Vec<String>> {
    let mut names: Vec<_> = names(&paths.state)?
        .into_iter()
        .filter(|name| {
            !history::DIRECTORIES
                .iter()
                .any(|(_, directory)| name == directory)
                && !["logs", "setup", "adopted", "operation.lock"].contains(&name.as_str())
                && !name.starts_with('.')
                && !pattern!(r"^setup-[0-9a-f]{64}\.lock$").is_match(name)
                && !pattern!(r"^[a-z0-9]+(?:-[a-z0-9]+)*--[a-z0-9]+(?:-[a-z0-9]+)*\.json$")
                    .is_match(name)
        })
        .collect();
    names.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    Ok(names)
}
pub fn prune(paths: &Paths, days: Option<&str>, apply: bool) -> Result<Value> {
    prune_with_wait(paths, days, apply, std::time::Duration::from_secs(30))
}
fn prune_with_wait(
    paths: &Paths,
    days: Option<&str>,
    apply: bool,
    wait: std::time::Duration,
) -> Result<Value> {
    let days = match days {
        None => Some(30),
        Some(s) if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) => s.parse::<u64>().ok(),
        Some(_) => None,
    };
    let days = days
        .filter(|n| (1..=3650).contains(n))
        .ok_or_else(|| Error::from("--older-than must be a whole number of days from 1 to 3650"))?;
    records::private_dir(&paths.state)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::from("System clock is unavailable"))?
        .as_millis() as i128;
    let plan = || -> Result<Value> {
        let live = live_keys(paths)?;
        let mut deleted = Vec::new();
        let mut kept = Vec::new();
        for entry in entries(paths)? {
            let date = history::date(entry.modified);
            if date.timestamp_millis() as i128 >= now - days as i128 * 86400000 {
                continue;
            }
            let mut item = json!({"category": entry.category, "path": entry.path.to_string_lossy(), "bytes": entry.bytes, "modifiedAt": date.to_rfc3339_opts(SecondsFormat::Millis, true)});
            if let Some(reason) = keep_reason(&entry, &live)? {
                item["reason"] = json!(reason);
                kept.push(item);
            } else {
                deleted.push(item);
            }
        }
        Ok(
            json!({"status": if deleted.is_empty() {"unchanged"} else {"preview"}, "olderThanDays": days, "deleted": deleted, "kept": kept, "unrecognized": unrecognized(paths)?}),
        )
    };
    if !apply {
        return plan();
    }
    let _lock = locks::state_with_timeout(paths, wait)?;
    let mut report = plan()?;
    for item in report["deleted"].as_array().unwrap() {
        let path = Path::new(item["path"].as_str().unwrap());
        let metadata = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err("Could not inspect retained deletion target".into()),
        };
        if metadata.is_dir() {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        }
        .map_err(|_| Error::from("Could not remove retained entry"))?;
    }
    let live: HashSet<_> = records::tasks(&paths.state)?
        .iter()
        .map(|t| t.value()["id"].as_str().unwrap().to_owned())
        .collect();
    let logs = paths.state.join("logs");
    if paths::exists(&logs)? {
        for task in names(&logs)? {
            if live.contains(&task) {
                continue;
            }
            let directory = logs.join(task);
            if fs::symlink_metadata(&directory)
                .map_err(|_| Error::from("Could not inspect retained log directory"))?
                .is_dir()
                && names(&directory)?.is_empty()
            {
                fs::remove_dir(&directory)
                    .map_err(|_| Error::from("Could not remove empty retained log directory"))?;
            }
        }
    }
    if !report["deleted"].as_array().unwrap().is_empty() {
        report["status"] = json!("applied");
    }
    Ok(report)
}
pub fn automatic(paths: &Paths) {
    let operation = || -> Result<()> {
        let marker = paths.state.join(".auto-prune");
        let now = SystemTime::now();
        if paths::exists(&marker)?
            && fs::symlink_metadata(&marker)
                .and_then(|m| m.modified())
                .is_ok_and(|modified| modified > now - std::time::Duration::from_secs(86400))
        {
            return Ok(());
        }
        prune_with_wait(paths, None, true, std::time::Duration::ZERO)?;
        crate::stateio::atomic_write(
            &marker,
            format!("{}\n", crate::health::timestamp()).as_bytes(),
            false,
        )
    };
    let _ = operation();
}
fn size(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let divisor = if bytes < 1024 * 1024 {
        1024
    } else {
        1024 * 1024
    };
    let tenths = (bytes as u128 * 10 + divisor / 2) / divisor;
    format!(
        "{}.{:01} {}",
        tenths / 10,
        tenths % 10,
        if divisor == 1024 { "KiB" } else { "MiB" }
    )
}
pub fn format(report: &Value, columns: usize) -> String {
    let status = report["status"].as_str().unwrap();
    let mut lines = vec![format!(
        "Prune {} (older than {} days).",
        terminal::status_color(status),
        report["olderThanDays"]
    )];
    for category in history::DIRECTORIES
        .iter()
        .map(|(category, _)| *category)
        .chain(["logs"])
    {
        let items: Vec<_> = report["deleted"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["category"] == category)
            .collect();
        if !items.is_empty() {
            lines.push(format!(
                "{} {category}: {} ({})",
                terminal::status_color(if status == "applied" {
                    "deleted"
                } else {
                    "delete"
                }),
                items.len(),
                size(items.iter().map(|i| i["bytes"].as_u64().unwrap()).sum())
            ));
        }
    }
    for item in report["kept"].as_array().unwrap() {
        lines.push(format!(
            "{} {} {}: {}",
            terminal::status_color("kept"),
            item["category"].as_str().unwrap(),
            item["path"].as_str().unwrap(),
            item["reason"].as_str().unwrap()
        ));
    }
    for name in report["unrecognized"].as_array().unwrap() {
        lines.push(format!(
            "{}, left alone: {}",
            terminal::status_color("not recognized"),
            name.as_str().unwrap()
        ));
    }
    if status == "preview" {
        lines.push("Preview only; rerun with --apply to delete. Records, setup receipts and locks are never pruned.".into());
    }
    terminal::wrap(&lines.join("\n"), columns)
}
