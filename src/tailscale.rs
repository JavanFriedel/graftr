use crate::{
    checkout::string,
    locks,
    paths::{self, Paths},
    records, stateio, system,
    validation::*,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    env,
    path::{Path, PathBuf},
};

fn parse(bytes: &[u8]) -> Result<Value> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| Error {code: "INVALID_JSON", message: "Invalid JSON; review configuration/state or the Herdr response (contents were not logged)".into()})?;
    object(&value)?;
    Ok(value)
}
pub fn status() -> Result<Value> {
    parse(
        system::run_inherited(&["tailscale", "serve", "status", "--json"])?
            .stdout
            .as_bytes(),
    )
}
pub(crate) fn hostname() -> Result<String> {
    let value = parse(
        system::run_inherited(&["tailscale", "status", "--json"])?
            .stdout
            .as_bytes(),
    )?;
    let host = text(&value["Self"]["DNSName"])?;
    let host = host.strip_suffix('.').unwrap_or(host);
    ensure(
        value["BackendState"] == "Running" && pattern!(r"^[a-z0-9.-]+\.ts\.net$").is_match(host),
        "Tailscale must be running with a tailnet DNS name",
    )?;
    Ok(host.into())
}
pub(crate) fn migrate_receipt(paths: &Paths, task: &Value, route: &str) -> Result<()> {
    let directory = directory(paths, string(task, "id"), string(&task["cleanup"], "runId"))?;
    let mut record = route_for(task)?;
    record["route"] = json!(route);
    write(&directory, &record)
}
fn section<'a>(value: &'a Value, key: &str) -> Result<Option<&'a Object>> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => Ok(Some(object(value)?)),
    }
}
fn number(key: &str) -> Result<u16> {
    let key = key.trim_matches(js_space);
    let number = key
        .parse::<f64>()
        .ok()
        .filter(|n| n.fract() == 0.0 && (1.0..=65535.0).contains(n));
    number
        .map(|n| n as u16)
        .ok_or_else(|| "Malformed Serve port configuration".into())
}
pub fn configured_ports(config: &Value) -> Result<HashSet<u16>> {
    let mut ports = HashSet::new();
    if let Some(tcp) = section(config, "TCP")? {
        for key in tcp.keys() {
            ports.insert(number(key)?);
        }
    }
    if let Some(web) = section(config, "Web")? {
        for key in web.keys() {
            ports.insert(number(
                key.rsplit_once(':').map_or(key.as_str(), |(_, port)| port),
            )?);
        }
    }
    if let Some(foreground) = section(config, "Foreground")? {
        for value in foreground.values() {
            object(value)?;
            ports.extend(configured_ports(value)?);
        }
    }
    Ok(ports)
}
fn matches(record: &Value, config: &Value) -> Result<bool> {
    let secure = record["httpsPort"].as_f64().unwrap().to_string();
    let local = record["localPort"].as_f64().unwrap();
    let endpoint = format!("{}:{secure}", string(record, "host"));
    Ok(
        section(config, "TCP")?.and_then(|s| s.get(&secure)) == Some(&json!({"HTTPS": true}))
            && section(config, "Web")?.and_then(|s| s.get(&endpoint))
                == Some(
                    &json!({"Handlers": {"/": {"Proxy": format!("http://127.0.0.1:{local}")}}}),
                )
            && section(config, "AllowFunnel")?.and_then(|s| s.get(&endpoint)) != Some(&json!(true)),
    )
}
fn route_for(task: &Value) -> Result<Value> {
    let secure = task["httpsPort"].as_f64();
    ensure(
        secure.is_some()
            && !task["host"].is_null()
            && task["url"]
                == format!(
                    "https://{}:{}",
                    task["host"].as_str().unwrap_or(""),
                    secure.unwrap_or(0.0)
                ),
        "Tailscale hooks require an explicit tailnet HTTPS URL and secondary port",
    )?;
    Ok(
        json!({"schemaVersion": 1, "kind": "tailscale-route", "id": task["id"], "path": task["path"], "localPort": task["localPort"], "httpsPort": task["httpsPort"], "host": task["host"], "url": task["url"], "route": "none"}),
    )
}
fn identity(record: &Value) -> Value {
    json!([
        record["id"],
        record["path"],
        record["localPort"],
        record["httpsPort"],
        record["host"],
        record["url"]
    ])
}
fn directory(paths: &Paths, id: &str, run: &str) -> Result<PathBuf> {
    records::private_dir(&paths.state)?;
    let root = paths.state.join("tailscale-hooks");
    locks::private_directory(&root)?;
    let key = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&json!([id, run])).unwrap())
    );
    let directory = root.join(key);
    locks::private_directory(&directory)?;
    Ok(directory)
}
fn read(directory: &Path, id: &str) -> Result<Option<Value>> {
    let file = directory.join(format!("{id}.json"));
    if !paths::exists(&file)? {
        return Ok(None);
    }
    let value = parse(&records::read_private(&file)?)?;
    ensure(
        value["schemaVersion"] == 1 && value["kind"] == "tailscale-route"
            || value["schemaVersion"] == 2,
        "Unsupported Tailscale route receipt",
    )?;
    let route = choice(
        &value["route"],
        &["none", "pending", "active"],
        "Invalid Tailscale route state",
    )?;
    for key in ["id", "path", "host", "url"] {
        text(&value[key])?;
    }
    for key in ["localPort", "httpsPort"] {
        integer(&value[key], 1024.0, 65535.0, "Invalid unprivileged port")?;
    }
    let mut record = route_for(&value)?;
    record["route"] = json!(route);
    ensure(
        record["id"] == id,
        "Tailscale route receipt identity mismatch",
    )?;
    Ok(Some(record))
}
fn write(directory: &Path, record: &Value) -> Result<()> {
    stateio::atomic_write(
        &directory.join(format!("{}.json", string(record, "id"))),
        format!("{}\n", serde_json::to_string_pretty(record).unwrap()).as_bytes(),
        false,
    )
}
fn prepare(directory: &Path, record: &mut Value) -> Result<()> {
    let config = status()?;
    let secure = record["httpsPort"].as_f64().unwrap() as u16;
    if configured_ports(&config)?.contains(&secure) {
        ensure(
            record["route"] != "none" && matches(record, &config)?,
            "Serve port already has configuration; refusing to adopt or replace it",
        )?;
        record["route"] = json!("active");
        return write(directory, record);
    }
    ensure(
        system::port_free(secure),
        "HTTPS port has an existing listener",
    )?;
    record["route"] = json!("pending");
    write(directory, record)?;
    system::run_inherited(&[
        "tailscale",
        "serve",
        "--bg",
        "--yes",
        &format!("--https={secure}"),
        &format!("http://127.0.0.1:{}", record["localPort"].as_f64().unwrap()),
    ])?;
    ensure(
        matches(record, &status()?)?,
        "Serve endpoint does not match the requested route",
    )?;
    record["route"] = json!("active");
    write(directory, record)
}
fn clean(directory: &Path, record: &mut Value) -> Result<()> {
    if record["route"] == "none" {
        return Ok(());
    }
    let config = status()?;
    let secure = record["httpsPort"].as_f64().unwrap() as u16;
    if configured_ports(&config)?.contains(&secure) {
        ensure(
            matches(record, &config)?,
            "Owned Serve port changed externally; preserving it for manual review",
        )?;
        system::run_inherited(&[
            "tailscale",
            "serve",
            "--yes",
            &format!("--https={secure}"),
            "off",
        ])?;
        ensure(
            !configured_ports(&status()?)?.contains(&secure),
            "Owned Serve route is still configured",
        )?;
    }
    record["route"] = json!("none");
    write(directory, record)
}
pub fn hook(paths: &Paths) -> Result<()> {
    let phase = env::var("GRAFTR_HOOK_PHASE").unwrap_or_default();
    ensure(
        ["afterDependencies", "cleanup"].contains(&phase.as_str()),
        "Tailscale hooks require afterDependencies or cleanup",
    )?;
    let task = records::task(
        &paths.state,
        &env::var("GRAFTR_TASK_ID").unwrap_or_default(),
    )?;
    let task = task.value();
    let equal_env = |key: &str, value: Option<&str>| env::var(key).ok().as_deref() == value;
    ensure(
        task.get("cleanup").is_some()
            && equal_env("GRAFTR_CLEANUP_RUN_ID", task["cleanup"]["runId"].as_str())
            && equal_env("GRAFTR_WORKTREE_PATH", task["path"].as_str())
            && equal_env(
                "GRAFTR_LOCAL_PORT",
                Some(&task["localPort"].as_f64().unwrap().to_string()),
            )
            && equal_env(
                "GRAFTR_HTTPS_PORT",
                Some(
                    &task["httpsPort"]
                        .as_f64()
                        .map_or("null".into(), |n| n.to_string()),
                ),
            )
            && equal_env("GRAFTR_URL", task["url"].as_str()),
        "Tailscale hook context no longer owns this task/generation",
    )?;
    let desired = route_for(task)?;
    let directory = directory(paths, string(task, "id"), string(&task["cleanup"], "runId"))?;
    let scoped = Paths {
        config: paths.config.clone(),
        state: directory.clone(),
    };
    let _lock = locks::state(&scoped)?;
    let mut owned = match read(&directory, string(task, "id"))? {
        Some(owned) => {
            ensure(
                identity(&owned) == identity(&desired),
                "Tailscale ownership receipt changed; preserve the route for review",
            )?;
            owned
        }
        None if phase == "cleanup" => return Ok(()),
        None => {
            write(&directory, &desired)?;
            desired
        }
    };
    if phase == "cleanup" {
        clean(&directory, &mut owned)
    } else {
        ensure(
            hostname()? == string(&owned, "host"),
            "Configured URL does not match this machine's Tailscale identity",
        )?;
        prepare(&directory, &mut owned)
    }
}
