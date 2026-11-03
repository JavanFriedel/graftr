//! Records are projected into validated, immutable wrappers before any reporting.
use crate::{
    environment,
    validation::{pattern, *},
};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

macro_rules! record {
    ($name:ident) => {
        #[derive(Debug, Clone)]
        pub struct $name(Value);
        impl $name {
            pub fn value(&self) -> &Value {
                &self.0
            }
        }
    };
}
record!(Task);
record!(Setup);
record!(Adoption);
record!(Frozen);

fn duration(value: &Value, message: &str) -> Result<Value> {
    ensure(
        value.as_f64().is_some_and(|n| n.is_finite() && n >= 0.0),
        message,
    )?;
    Ok(value.clone())
}
fn command(value: &Value) -> Result<Value> {
    let args = list(value)?;
    for arg in args {
        text(arg)?;
    }
    ensure(
        !args.is_empty()
            && args.len() <= 64
            && !args.iter().any(|arg| {
                arg.as_str()
                    .unwrap()
                    .chars()
                    .any(|c| c <= '\u{1f}' || c == '\u{7f}')
            }),
        "Hook command must be a nonempty argv array without control characters",
    )?;
    Ok(value.clone())
}
fn hook_commands(value: &Value) -> Result<Vec<Value>> {
    let hooks = list(value)?;
    ensure(
        hooks.len() <= 16,
        "hooks supports at most 16 hooks per phase",
    )?;
    let mut names = HashSet::new();
    hooks.iter().map(|hook| {
        let hook = object(hook)?;
        ensure(hook.keys().all(|key| ["name", "command", "timeoutMs"].contains(&key.as_str())), "Setup hook has unsupported fields")?;
        let hook_name = string(hook, "name")?; name(hook_name)?;
        ensure(names.insert(hook_name), "Setup hook names must be unique within their phase")?;
        let timeout = hook.get("timeoutMs").cloned().unwrap_or(json!(60000));
        integer(&timeout, 100.0, 600000.0, "Setup hook timeoutMs must be between 100 and 600000 milliseconds")?;
        let command = command(field(hook, "command")?)?;
        let executable = command[0].as_str().unwrap();
        ensure(!executable.contains('/') || executable.starts_with('/'), "Cleanup executables must be absolute or resolved through PATH, not relative to the checkout")?;
        Ok(json!({"name": hook_name, "command": command, "timeoutMs": timeout}))
    }).collect()
}
fn hook_runs(value: &Value, phases: &[&str], limit: usize) -> Result<Value> {
    let hooks = value
        .as_array()
        .filter(|hooks| hooks.len() <= limit)
        .ok_or_else(|| Error::from("Invalid setup hook results"))?;
    let mut names = HashSet::new();
    let values: Result<Vec<_>> = hooks
        .iter()
        .map(|hook| {
            let hook = object(hook)?;
            let hook_name = string(hook, "name")?;
            name(hook_name)?;
            let phase = choice(field(hook, "phase")?, phases, "Invalid setup hook phase")?;
            ensure(
                names.insert(format!("{phase}/{hook_name}")),
                "Duplicate setup hook result",
            )?;
            let status = choice(
                field(hook, "status")?,
                &["running", "succeeded", "failed"],
                "Invalid setup hook status",
            )?;
            let mut result = strings(hook, &["phase", "name", "status"])?;
            optional(hook, &mut result, "elapsedMs", |value| {
                duration(value, "Invalid setup hook duration")
            })?;
            optional(hook, &mut result, "exitCode", |value| {
                ensure(
                    value
                        .as_f64()
                        .is_some_and(|n| n.is_finite() && n.fract() == 0.0),
                    "Invalid setup hook exit code",
                )?;
                Ok(value.clone())
            })?;
            optional(hook, &mut result, "reason", |value| {
                choice(
                    value,
                    &[
                        "timeout",
                        "command-failed",
                        "command-unavailable",
                        "context-changed",
                    ],
                    "Invalid setup hook failure reason",
                )?;
                Ok(value.clone())
            })?;
            ensure(
                !(status == "running"
                    && ["elapsedMs", "exitCode", "reason"]
                        .iter()
                        .any(|key| hook.contains_key(*key))
                    || status != "running" && !hook.contains_key("elapsedMs")
                    || status == "failed" && !hook.contains_key("reason")
                    || status == "succeeded"
                        && (hook.contains_key("reason")
                            || hook
                                .get("exitCode")
                                .is_some_and(|code| code.as_f64() != Some(0.0)))),
                "Inconsistent setup hook result",
            )?;
            optional(hook, &mut result, "log", |value| {
                ensure(
                    value
                        .as_str()
                        .is_some_and(|path| path.starts_with('/') && path.ends_with(".log")),
                    "Invalid hook log path",
                )?;
                Ok(value.clone())
            })?;
            Ok(Value::Object(result))
        })
        .collect();
    Ok(json!(values?))
}
fn cleanup(value: &Value, kind: &str) -> Result<Value> {
    let data = object(value)?;
    let hooks = hook_commands(field(data, "hooks")?)?;
    ensure(!hooks.is_empty(), "Empty cleanup ownership plan")?;
    integer(
        field(data, "attempt")?,
        0.0,
        9007199254740991.0,
        "Invalid cleanup attempt",
    )?;
    let status = choice(
        field(data, "status")?,
        &["armed", "queued", "running", "failed", "complete"],
        "Invalid cleanup status",
    )?;
    let results = hook_runs(field(data, "results")?, &[kind], 16)?;
    let results = results.as_array().unwrap();
    ensure(
        !results.iter().enumerate().any(|(index, result)| {
            hooks.get(index).map(|hook| &hook["name"]) != Some(&result["name"])
                || index + 1 < results.len() && result["status"] != "succeeded"
        }),
        "Cleanup observations do not match their recorded commands",
    )?;
    ensure(
        !(status == "armed" && !results.is_empty()
            || status == "complete"
                && (results.len() != hooks.len()
                    || results.iter().any(|result| result["status"] != "succeeded"))),
        "Inconsistent cleanup status",
    )?;
    Ok(
        json!({"runId": string(data, "runId")?, "attempt": data["attempt"], "hooks": hooks,
        "context": environment::parse(field(data, "context")?, false)?, "status": status, "results": results}),
    )
}
fn timings(value: &Value) -> Result<Value> {
    let data = object(value)?;
    let timing_id = string(data, "id")?;
    ensure(
        pattern!(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")
            .is_match(timing_id),
        "Invalid timing identity",
    )?;
    choice(
        field(data, "operation")?,
        &["create", "open", "setup"],
        "Invalid timing operation",
    )?;
    let timestamp = |value: &Value| -> Result<Value> {
        ensure(
            date_millis(text(value)?).is_some(),
            "Invalid timing timestamp",
        )?;
        Ok(value.clone())
    };
    let mut result = strings(data, &["id", "operation"])?;
    result.insert("startedAt".into(), timestamp(field(data, "startedAt")?)?);
    let mut stages = Object::new();
    for (stage, elapsed) in object(field(data, "stagesMs")?)? {
        let stage = if stage == "route" { "finalize" } else { stage };
        ensure(
            [
                "checkout",
                "enrollment",
                "profile",
                "environment",
                "tabs",
                "dependencies",
                "hooks",
                "finalize",
                "stateLockWait",
                "worktreeLockWait",
            ]
            .contains(&stage),
            "Invalid timing stage",
        )?;
        duration(elapsed, "Invalid timing duration")?;
        let total =
            stages.get(stage).and_then(Value::as_f64).unwrap_or(0.0) + elapsed.as_f64().unwrap();
        stages.insert(stage.into(), json!(total));
    }
    result.insert("stagesMs".into(), Value::Object(stages));
    ensure(
        data.contains_key("finishedAt") == data.contains_key("elapsedMs"),
        "Incomplete timing completion",
    )?;
    optional(data, &mut result, "finishedAt", timestamp)?;
    for key in ["elapsedMs", "checkoutReadyMs"] {
        optional(data, &mut result, key, |value| {
            duration(value, "Invalid timing duration")
        })?;
    }
    optional(data, &mut result, "dependencies", |value| {
        choice(
            value,
            &["installed", "reused", "skipped", "not-configured", "failed"],
            "Invalid timing dependency outcome",
        )?;
        Ok(value.clone())
    })?;
    Ok(Value::Object(result))
}
fn outputs(value: &Value) -> Result<Value> {
    let mut paths = HashSet::new();
    let result: Result<Vec<_>> = list(value)?
        .iter()
        .map(|item| {
            let item = object(item)?;
            choice(
                field(item, "kind")?,
                &["file", "directory"],
                "Invalid installed output kind",
            )?;
            let path = project_path(field(item, "path")?)?;
            ensure(
                paths.insert(path.clone()),
                "Duplicate installed output path",
            )?;
            Ok(json!({"path": path, "kind": item["kind"]}))
        })
        .collect();
    Ok(json!(result?))
}

impl Task {
    pub fn parse(value: &Value) -> Result<Self> {
        let data = object(value)?;
        if [Some(2.0), Some(3.0)].contains(&data.get("schemaVersion").and_then(Value::as_f64)) {
            return Err(Error { code: "LEGACY_STATE", message: "Task record uses a retired schema; run graftr migrate to convert it before continuing".into() });
        }
        ensure(
            data.get("schemaVersion").and_then(Value::as_f64) == Some(4.0),
            "Unsupported task schema; expected version 4",
        )?;
        choice(
            field(data, "status")?,
            &["preparing", "ready", "cleaned", "closed"],
            "Invalid task status",
        )?;
        let mut task = strings(
            data,
            &[
                "id",
                "project",
                "repo",
                "path",
                "branch",
                "baseCommit",
                "status",
            ],
        )?;
        task.insert("schemaVersion".into(), json!(4));
        for key in ["socket", "workspaceId", "host", "url"] {
            task.insert(key.into(), nullable(data, key)?);
        }
        let tabs = object(field(data, "tabs")?)?;
        for value in tabs.values() {
            text(value)?;
        }
        task.insert("tabs".into(), json!(tabs));
        task.insert("localPort".into(), port(field(data, "localPort")?)?);
        let https = field(data, "httpsPort")?;
        task.insert(
            "httpsPort".into(),
            if https.is_null() {
                Value::Null
            } else {
                port(https)?
            },
        );
        optional(data, &mut task, "ports", |value| {
            let ports = object(value)?;
            ensure(ports.len() <= 16, "ports supports at most 16 named ports")?;
            for (name, value) in ports {
                ensure(
                    named_port(name),
                    "Port names use lowercase letters, numbers and single hyphens (at most 32 characters)",
                )?;
                port(value)?;
            }
            ensure(!ports.is_empty(), "Recorded named ports must not be empty")?;
            Ok(value.clone())
        })?;
        for kind in ["cleanup", "retire"] {
            optional(data, &mut task, kind, |value| cleanup(value, kind))?;
        }
        task.insert(
            "envOverrides".into(),
            environment::parse(field(data, "envOverrides")?, true)?,
        );
        task.insert(
            "overrides".into(),
            environment::parse(field(data, "overrides")?, false)?,
        );
        let task = Value::Object(task);
        id(task["id"].as_str().unwrap())?;
        ensure(
            !task["socket"].is_null()
                || task["workspaceId"].is_null() && task["tabs"].as_object().unwrap().is_empty(),
            "Unattached tasks cannot claim Herdr workspaces/tabs",
        )?;
        let mut ports = vec![task["localPort"].as_f64().unwrap() as u64];
        if let Some(port) = task["httpsPort"].as_f64() {
            ports.push(port as u64);
        }
        if let Some(named) = task["ports"].as_object() {
            ports.extend(named.values().map(|port| port.as_f64().unwrap() as u64));
        }
        ensure(
            task["id"].as_str().unwrap().split("--").next() == task["project"].as_str()
                && ports.iter().copied().collect::<HashSet<_>>().len() == ports.len(),
            "Mismatched task identity/ports",
        )?;
        let valid_url = if let Some(value) = task["url"].as_str() {
            let parsed = url(value)?;
            parsed.origin().ascii_serialization() == value
                && parsed.host_str() == task["host"].as_str()
        } else {
            task["host"].is_null()
        };
        ensure(valid_url, "Mismatched task URL assignment")?;
        environment::validate_task(&task)?;
        Ok(Self(task))
    }
    pub fn public(&self) -> Value {
        let mut task = self.0.clone();
        for kind in ["cleanup", "retire"] {
            if let Some(plan) = task.get_mut(kind) {
                plan.as_object_mut().unwrap().remove("context");
                for hook in plan["hooks"].as_array_mut().unwrap() {
                    hook.as_object_mut().unwrap().remove("command");
                }
            }
        }
        task
    }
}
impl Setup {
    pub fn parse(value: &Value) -> Result<Self> {
        let data = object(value)?;
        ensure(
            data.get("schemaVersion").and_then(Value::as_f64) == Some(1.0)
                && data.get("skipInstall").is_some_and(Value::is_boolean),
            "Invalid setup receipt",
        )?;
        choice(
            field(data, "status")?,
            &["pending", "ready", "incomplete", "unmanaged", "skipped"],
            "Invalid setup status",
        )?;
        let step = if data.get("step").and_then(Value::as_str) == Some("route") {
            json!("finalize")
        } else {
            field(data, "step")?.clone()
        };
        choice(
            &step,
            &[
                "profile",
                "enrollment",
                "environment",
                "tabs",
                "dependencies",
                "hooks",
                "finalize",
                "complete",
            ],
            "Invalid setup step",
        )?;
        choice(
            field(data, "dependencies")?,
            &["not-configured", "pending", "ready", "failed", "skipped"],
            "Invalid dependency state",
        )?;
        let mut result = strings(data, &["path", "status", "message", "dependencies"])?;
        for key in ["socket", "workspaceId", "project", "taskId", "installKey"] {
            result.insert(key.into(), nullable(data, key)?);
        }
        result.insert("schemaVersion".into(), json!(1));
        result.insert("step".into(), step);
        result.insert("skipInstall".into(), data["skipInstall"].clone());
        result.insert(
            "runId".into(),
            if data.contains_key("runId") {
                nullable(data, "runId")?
            } else {
                Value::Null
            },
        );
        optional(data, &mut result, "installOutputs", outputs)?;
        optional(data, &mut result, "installLog", |value| {
            let path = text(value)?;
            ensure(
                path.starts_with('/') && path.ends_with(".log"),
                "Invalid run log path",
            )?;
            Ok(value.clone())
        })?;
        optional(data, &mut result, "timings", timings)?;
        optional(data, &mut result, "hooks", |value| {
            hook_runs(value, &["afterEnvironment", "afterDependencies"], 32)
        })?;
        Ok(Self(Value::Object(result)))
    }
}
impl Adoption {
    pub fn parse(value: &Value) -> Result<Self> {
        let data = object(value)?;
        ensure(
            data.get("schemaVersion").and_then(Value::as_f64) == Some(1.0),
            "Unsupported adoption schema",
        )?;
        let mut result = strings(
            data,
            &["id", "project", "repo", "path", "branch", "adoptedAt"],
        )?;
        for key in ["socket", "workspaceId", "url"] {
            result.insert(key.into(), nullable(data, key)?);
        }
        for key in ["environment", "dependencies"] {
            choice(
                field(data, key)?,
                &["external", "pending", "completed", "failed"],
                "Invalid adoption stage",
            )?;
            result.insert(key.into(), data[key].clone());
        }
        let port = field(data, "localPort")?;
        if !port.is_null() {
            integer(
                port,
                1.0,
                65535.0,
                "Invalid adoption identity or public address",
            )?;
        }
        result.insert("localPort".into(), port.clone());
        result.insert("schemaVersion".into(), json!(1));
        let record_id = string(data, "id")?;
        id(record_id)?;
        let project = string(data, "project")?;
        name(project)?;
        let path = string(data, "path")?;
        let repo = string(data, "repo")?;
        let valid_url = if let Some(url_) = result["url"].as_str() {
            url(url_)?.origin().ascii_serialization() == url_
        } else {
            true
        };
        ensure(
            record_id.starts_with(&format!("{project}--"))
                && date_millis(string(data, "adoptedAt")?).is_some()
                && absolute_normalized(path)
                && absolute_normalized(repo)
                && repo != path
                && result["socket"].is_null() == result["workspaceId"].is_null()
                && valid_url,
            "Invalid adoption identity or public address",
        )?;
        let hooks = field(data, "hooks")?
            .as_array()
            .ok_or_else(|| Error::from("Invalid adoption hook results"))?;
        let hooks: Result<Vec<_>> = hooks
            .iter()
            .map(|hook| {
                let hook = object(hook)?;
                choice(
                    field(hook, "status")?,
                    &["external", "pending", "completed", "failed"],
                    "Invalid adoption stage",
                )?;
                ensure(
                    [Some("afterEnvironment"), Some("afterDependencies")]
                        .contains(&hook.get("phase").and_then(Value::as_str))
                        && hook["status"] != "external",
                    "Invalid adopted setup hook",
                )?;
                Ok(Value::Object(strings(hook, &["phase", "name", "status"])?))
            })
            .collect();
        result.insert("hooks".into(), json!(hooks?));
        Ok(Self(Value::Object(result)))
    }
}
impl Frozen {
    pub fn parse(value: &Value) -> Result<Self> {
        let data = object(value)?;
        ensure(
            data.get("schemaVersion").and_then(Value::as_f64) == Some(1.0),
            "Unsupported frozen task schema",
        )?;
        let record_id = string(data, "id")?;
        id(record_id)?;
        ensure(
            pattern!(r"^[a-f0-9-]{36}$").is_match(string(data, "generation")?),
            "Invalid frozen task generation",
        )?;
        choice(
            field(data, "status")?,
            &["freezing", "frozen", "thawing", "attention", "thawed"],
            "Invalid frozen task status",
        )?;
        let removal = object(field(data, "removal")?)?;
        for key in ["path", "repo"] {
            ensure(
                absolute_normalized(string(removal, key)?),
                "Invalid frozen task path",
            )?;
        }
        ensure(
            pattern!(r"^[a-f0-9]{40,64}$").is_match(string(removal, "currentCommit")?)
                && removal.get("id").and_then(Value::as_str) == Some(record_id),
            "Invalid frozen branch snapshot",
        )?;
        ensure(
            data.get("skipInstall").is_some_and(Value::is_boolean),
            "Invalid frozen setup/retry state",
        )?;
        integer(
            field(data, "attempts")?,
            0.0,
            9007199254740991.0,
            "Invalid frozen setup/retry state",
        )?;
        let note = nullable(data, "note")?;
        if let Some(note) = note.as_str() {
            ensure(
                !note.trim_matches(js_space).is_empty()
                    && note.encode_utf16().count() <= 2000
                    && !note.chars().any(control),
                "--note must be a nonempty single line of at most 2000 characters, without terminal controls",
            )?;
        }
        let mut result = strings(data, &["id", "generation", "status", "message"])?;
        result.insert("schemaVersion".into(), json!(1));
        result.insert("note".into(), note);
        result.insert("frozenAt".into(), canonical_date(field(data, "frozenAt")?)?);
        for key in ["until", "nextAttemptAt", "thawedAt"] {
            let value = field(data, key)?;
            result.insert(
                key.into(),
                if value.is_null() {
                    Value::Null
                } else {
                    canonical_date(value)?
                },
            );
        }
        for key in ["skipInstall", "attempts"] {
            result.insert(key.into(), data[key].clone());
        }
        let mut projected = strings(
            removal,
            &[
                "id",
                "path",
                "repo",
                "branch",
                "baseCommit",
                "currentBranch",
                "currentCommit",
            ],
        )?;
        for key in ["socket", "workspaceId"] {
            projected.insert(key.into(), nullable(removal, key)?);
        }
        optional(removal, &mut projected, "cleanupRunId", |value| {
            text(value)?;
            Ok(value.clone())
        })?;
        result.insert("removal".into(), Value::Object(projected));
        Ok(Self(Value::Object(result)))
    }
    pub fn reserves(&self) -> bool {
        self.0["status"] != "thawed"
    }
    pub fn public(&self) -> Value {
        let mut result = Object::new();
        for key in [
            "id",
            "status",
            "frozenAt",
            "until",
            "note",
            "skipInstall",
            "attempts",
            "nextAttemptAt",
            "thawedAt",
            "message",
        ] {
            result.insert(key.into(), self.0[key].clone());
        }
        for (key, source) in [
            ("branch", "currentBranch"),
            ("commit", "currentCommit"),
            ("path", "path"),
        ] {
            result.insert(key.into(), self.0["removal"][source].clone());
        }
        Value::Object(result)
    }
}

fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.to_string().into()),
    }
}
pub(crate) fn private_dir(path: &Path) -> Result<()> {
    let data =
        fs::symlink_metadata(path).map_err(|_| Error::from("State directory is unreadable"))?;
    ensure(
        data.is_dir() && data.mode() & 0o077 == 0 && data.uid() == unsafe { libc::getuid() },
        "State directory must be private, owned by you, and not a symlink",
    )
}
fn json_files(path: &Path) -> Result<Vec<std::path::PathBuf>> {
    if !exists(path)? {
        return Ok(vec![]);
    }
    let mut files = Vec::new();
    for file in fs::read_dir(path).map_err(|_| Error::from("State directory is unreadable"))? {
        let file = file.map_err(|_| Error::from("State directory is unreadable"))?;
        if file.file_name().as_encoded_bytes().ends_with(b".json") {
            files.push(file.path());
        }
    }
    files.sort();
    Ok(files)
}
pub(crate) fn read_private(path: &Path) -> Result<Vec<u8>> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| Error::from("Recorded file is unreadable"))?;
    ensure(
        metadata.is_file()
            && metadata.mode() & 0o077 == 0
            && metadata.uid() == unsafe { libc::getuid() },
        "Expected a private regular file owned by you",
    )?;
    // Recheck the opened descriptor and never follow a swapped final symlink/FIFO.
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| Error::from("Expected a private regular file owned by you"))?;
    let opened = file
        .metadata()
        .map_err(|_| Error::from("Recorded file is unreadable"))?;
    ensure(
        opened.is_file()
            && opened.mode() & 0o077 == 0
            && opened.uid() == unsafe { libc::getuid() }
            && (opened.dev(), opened.ino()) == (metadata.dev(), metadata.ino()),
        "Expected a private regular file owned by you",
    )?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|_| Error::from("Recorded file is unreadable"))?;
    Ok(bytes)
}
fn read_json(path: &Path) -> Result<Value> {
    let bytes = read_private(path)?;
    serde_json::from_slice(&bytes).map_err(|_| Error { code: "INVALID_JSON", message: "Invalid JSON; review configuration/state or the Herdr response (contents were not logged)".into() })
}
pub fn task(state: &Path, record_id: &str) -> Result<Task> {
    id(record_id)?;
    let record = Task::parse(&read_json(&state.join(format!("{record_id}.json")))?)?;
    ensure(
        record.value()["id"] == record_id,
        "Task filename and identity differ",
    )?;
    Ok(record)
}
fn stem(path: &Path) -> Result<&str> {
    path.file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "Invalid record filename".into())
}
pub fn tasks(state: &Path) -> Result<Vec<Task>> {
    json_files(state)?
        .iter()
        .map(|path| task_file(path))
        .collect()
}
fn task_file(path: &Path) -> Result<Task> {
    let record_id = stem(path)?;
    id(record_id)?;
    let record = Task::parse(&read_json(path)?)?;
    ensure(
        record.value()["id"] == record_id,
        "Task filename and identity differ",
    )?;
    Ok(record)
}
pub fn setups(state: &Path) -> Result<Vec<Setup>> {
    let root = state.join("setup");
    if exists(&root)? {
        private_dir(&root)?;
    }
    json_files(&root)?
        .iter()
        .map(|path| setup_file(path))
        .collect()
}
fn setup_file(path: &Path) -> Result<Setup> {
    let record = Setup::parse(&read_json(path)?)?;
    let key = json!([record.value()["socket"], record.value()["path"]]);
    let hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&key).unwrap()));
    ensure(stem(path)? == hash, "Setup receipt filename mismatch")?;
    Ok(record)
}

pub fn setup_for(state: &Path, socket: Option<&str>, path: &str) -> Result<Option<Setup>> {
    let directory = state.join("setup");
    if exists(&directory)? {
        private_dir(&directory)?;
    }
    let key = |socket: Option<&str>| {
        format!(
            "{:x}.json",
            Sha256::digest(serde_json::to_vec(&json!([socket, path])).unwrap())
        )
    };
    let core = directory.join(key(None));
    let legacy = directory.join(key(socket));
    let has_core = exists(&core)?;
    if has_core
        && setups(state)?
            .iter()
            .filter(|r| r.value()["path"] == path)
            .count()
            > 1
    {
        return Err(
            "Multiple setup receipt lineages claim this checkout; review them before setup".into(),
        );
    }
    let file = if has_core { &core } else { &legacy };
    if !exists(file)? {
        if socket.is_some()
            && !tasks(state)?.iter().any(|task| {
                task.value()["path"] == path && task.value()["socket"].as_str() == socket
            })
        {
            return Ok(None);
        }
        let candidates: Vec<_> = setups(state)?
            .into_iter()
            .filter(|r| r.value()["path"] == path)
            .collect();
        ensure(
            candidates.len() <= 1,
            "Multiple setup receipt lineages claim this checkout; review them before setup",
        )?;
        return Ok(candidates.into_iter().next());
    }
    let receipt = Setup::parse(&read_json(file)?)?;
    ensure(
        receipt.value()["socket"].as_str() == if has_core { None } else { socket }
            && receipt.value()["path"] == path,
        "Setup receipt identity mismatch",
    )?;
    Ok(Some(receipt))
}
pub fn adoptions(state: &Path) -> Result<Vec<Adoption>> {
    let root = state.join("adopted");
    if exists(&root)? {
        private_dir(state)?;
        private_dir(&root)?;
    }
    json_files(&root)?
        .iter()
        .map(|path| adoption_file(path))
        .collect()
}
fn adoption_file(path: &Path) -> Result<Adoption> {
    let record = Adoption::parse(&read_json(path)?)?;
    ensure(
        record.value()["id"] == stem(path)?,
        "Adoption filename and identity differ",
    )?;
    Ok(record)
}
pub fn frozen(state: &Path) -> Result<Vec<Frozen>> {
    let root = state.join("frozen");
    if exists(&root)? {
        private_dir(&root)?;
    }
    json_files(&root)?
        .iter()
        .map(|path| frozen_file(path))
        .collect()
}
fn frozen_file(path: &Path) -> Result<Frozen> {
    id(stem(path)?)?;
    let record = Frozen::parse(&read_json(path)?)?;
    ensure(
        record.value()["id"] == stem(path)?,
        "Frozen task filename and identity differ",
    )?;
    Ok(record)
}

#[derive(Serialize)]
pub struct RecordIssue {
    pub path: String,
    code: &'static str,
    message: &'static str,
}
pub struct Report<T> {
    pub records: Vec<T>,
    pub issues: Vec<RecordIssue>,
}
// Reporting alone may isolate a bad file. Allocation, cleanup and all other
// lifecycle readers above stay fail-closed: an invalid record can still own leases.
fn report<T>(state: &Path, directory: &str, read: fn(&Path) -> Result<T>) -> Result<Report<T>> {
    let root = state.join(directory);
    if !directory.is_empty() && exists(&root)? {
        if directory == "adopted" {
            private_dir(state)?;
        }
        private_dir(&root)?;
    }
    let mut report = Report {
        records: Vec::new(),
        issues: Vec::new(),
    };
    for file in json_files(&root)? {
        match read(&file) {
            Ok(record) => report.records.push(record),
            Err(_) => report.issues.push(RecordIssue {
                path: file
                    .strip_prefix(state)
                    .expect("record inside state")
                    .to_string_lossy()
                    .into_owned(),
                code: "INVALID_RECORD",
                message: "Record is unreadable, unsafe or invalid; contents were not logged.",
            }),
        }
    }
    Ok(report)
}
pub fn task_report(state: &Path) -> Result<Report<Task>> {
    report(state, "", task_file)
}
pub fn setup_report(state: &Path) -> Result<Report<Setup>> {
    report(state, "setup", setup_file)
}
pub fn adoption_report(state: &Path) -> Result<Report<Adoption>> {
    report(state, "adopted", adoption_file)
}
pub fn frozen_report(state: &Path) -> Result<Report<Frozen>> {
    report(state, "frozen", frozen_file)
}
pub fn frozen_task(state: &Path, record_id: &str) -> Result<Option<Frozen>> {
    id(record_id)?;
    let root = state.join("frozen");
    if exists(&root)? {
        private_dir(&root)?;
    }
    let path = root.join(format!("{record_id}.json"));
    if !exists(&path)? {
        return Ok(None);
    }
    let record = Frozen::parse(&read_json(&path)?)?;
    ensure(
        record.value()["id"] == record_id,
        "Frozen task filename and identity differ",
    )?;
    Ok(Some(record))
}

#[cfg(test)]
mod tests {
    #[test]
    fn json_timing_numbers_round_trip_without_losing_low_bits() {
        let value: serde_json::Value = serde_json::from_str(r#"{"id":"11111111-2222-3333-4444-555555555555","operation":"setup","startedAt":"2026-01-01T00:00:00.123Z","stagesMs":{"checkout":9.345137999999999,"finalize":20.811409000000026}}"#).unwrap();
        assert_eq!(
            value["stagesMs"]["checkout"].as_f64().unwrap().to_bits(),
            9.345137999999999_f64.to_bits()
        );
        let parsed = super::timings(&value).unwrap();
        assert_eq!(parsed["stagesMs"], value["stagesMs"]);
    }
}
