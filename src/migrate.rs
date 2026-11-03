use crate::{
    environment, health, locks,
    paths::{self, Paths},
    profiles, records, stateio, tailscale, terminal,
    validation::*,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::PathBuf,
};

const COMMAND: [&str; 3] = ["graftr", "hook", "tailscale"];
fn js_string(value: Option<&Value>) -> String {
    match value {
        None => "undefined".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) => "null".into(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.as_f64().unwrap().to_string(),
        _ => "[object Object]".into(),
    }
}
fn legacy_overrides(key: &str) -> Value {
    let mut value = json!({"PORT": "{{localPort}}", "BASE_URL": "{{url}}", "AUTH_URL": "{{url}}"});
    value[key] = json!("http://localhost:{{localPort}},{{url}}");
    value
}
fn rewrite(container: &mut Value, field: &str) -> Result<bool> {
    let entries = container[field]
        .as_array_mut()
        .ok_or_else(|| Error::from("Expected a JSON array"))?;
    let mut changed = false;
    for hook in entries {
        object(hook)?;
        let command = list(&hook["command"])?;
        if command.first().is_some_and(|v| v == "bun")
            && command
                .iter()
                .any(|arg| arg.as_str().is_some_and(|s| s.contains("tailscaleHook")))
        {
            hook["command"] = json!(COMMAND);
            changed = true;
        }
    }
    Ok(changed)
}
fn rewrite_hooks(hooks: &mut Value) -> Result<bool> {
    let fields: Vec<_> = object(hooks)?.keys().cloned().collect();
    let mut changed = false;
    for field in fields {
        changed = rewrite(hooks, &field)? || changed;
    }
    Ok(changed)
}
fn parse(bytes: &[u8]) -> Result<Value> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| Error {code: "INVALID_JSON", message: "Invalid JSON; review configuration/state or the Herdr response (contents were not logged)".into()})?;
    object(&value)?;
    Ok(value)
}
fn raw_tasks(paths: &Paths) -> Result<Vec<Value>> {
    let mut names = Vec::new();
    for item in
        fs::read_dir(&paths.state).map_err(|_| Error::from("State directory is unreadable"))?
    {
        let item = item.map_err(|_| Error::from("State directory is unreadable"))?;
        let name = item.file_name().to_string_lossy().into_owned();
        if name.ends_with(".json") {
            names.push(name);
        }
    }
    names.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    names
        .iter()
        .map(|name| {
            let value = parse(&records::read_private(&paths.state.join(name))?)?;
            ensure(
                format!("{}.json", js_string(value.get("id"))) == *name,
                "Task filename and identity differ",
            )?;
            Ok(value)
        })
        .collect()
}
fn project_host(project: &str, tasks: &[Value]) -> Result<String> {
    let hosts: HashSet<_> = tasks
        .iter()
        .filter(|t| t["project"] == project)
        .filter_map(|t| t["host"].as_str().map(str::to_owned))
        .collect();
    ensure(
        hosts.len() <= 1,
        "Its records use more than one Tailscale host; migrate them by hand",
    )?;
    if let Some(host) = hosts.into_iter().next() {
        Ok(host)
    } else {
        tailscale::hostname()
    }
}
fn convert_task(raw: &Value, profile: Option<&Value>) -> Result<(Value, String, Vec<String>)> {
    let route = choice(
        &raw["route"],
        &["none", "pending", "active"],
        "Invalid recorded route state",
    )?;
    let legacy = raw["schemaVersion"] == 2 || raw["routing"] == "legacy-tailscale";
    let mut fields = raw.clone();
    for key in ["route", "routing", "schemaVersion"] {
        fields.as_object_mut().unwrap().remove(key);
    }
    let mut changes = vec![format!(
        "converted schema {} to 4",
        js_string(raw.get("schemaVersion"))
    )];
    if fields.get("envOverrides").is_none() {
        let overrides = object(&fields["overrides"])?;
        let key = ["NEXT_ALLOWED_DEV_ORIGINS", "ALLOWED_DEV_ORIGINS"]
            .into_iter()
            .find(|k| overrides.contains_key(*k))
            .ok_or_else(|| {
                Error::from("Legacy record has no recognizable allowed-origins assignment")
            })?;
        fields["envOverrides"] = legacy_overrides(key);
        changes.push("spelled out its retired default envOverrides".into());
    }
    if fields.get("cleanup").is_some() {
        object(&fields["cleanup"])?;
        if rewrite(&mut fields["cleanup"], "hooks")? {
            changes
                .push("replaced the bun -e Tailscale import in its recorded cleanup plan".into());
        }
    }
    fields["schemaVersion"] = json!(4);
    let mut task = records::Task::parse(&fields)?.value().clone();
    if legacy && route != "none" {
        ensure(
            task.get("cleanup").is_none() || task["cleanup"]["status"] == "complete",
            "Its recorded cleanup is unfinished; run graftr cleanup with the previous build first",
        )?;
        let hooks = profile.map_or_else(
            || json!([{"name": "cleanup-tailscale", "command": COMMAND, "timeoutMs": 60000}]),
            |p| p["hooks"].get("cleanup").cloned().unwrap_or(json!([])),
        );
        ensure(
            hooks
                .as_array()
                .unwrap()
                .iter()
                .any(|h| h["command"] == json!(COMMAND)),
            "It owns a Tailscale route but its profile has no graftr hook tailscale cleanup command",
        )?;
        let context = if let Some(profile) = profile {
            environment::resolve(
                &profile
                    .get("envOverrides")
                    .cloned()
                    .unwrap_or(json!({"PORT": "{{localPort}}"})),
                &task,
            )?
        } else {
            task["overrides"].clone()
        };
        task["cleanup"] = json!({"runId": crate::system::uuid()?, "attempt": 0, "hooks": hooks, "context": context, "status": "armed", "results": []});
        changes.push(format!(
            "handed its {route} route to the cleanup-tailscale hook"
        ));
    } else if task["cleanup"]["status"] == "complete" {
        task.as_object_mut().unwrap().remove("cleanup");
    }
    Ok((
        records::Task::parse(&task)?.value().clone(),
        if legacy { route.into() } else { "none".into() },
        changes,
    ))
}
struct Plan {
    report: Value,
    config: Value,
    converted: Vec<(Value, String)>,
    moves: Vec<(PathBuf, PathBuf)>,
}
fn directory_moves(paths: &Paths) -> Result<Vec<(PathBuf, PathBuf)>> {
    let config = paths
        .config
        .parent()
        .ok_or_else(|| Error::from("Invalid configuration path"))?
        .to_path_buf();
    let mut moves = Vec::new();
    for path in [config, paths.state.clone()] {
        if path.file_name().is_some_and(|n| n == "devtree")
            && paths::exists(&path)?
            && !fs::symlink_metadata(&path)
                .map_err(|_| Error::from("Could not inspect legacy directory"))?
                .is_symlink()
        {
            let destination = path.with_file_name("graftr");
            moves.push((path, destination));
        }
    }
    Ok(moves)
}
fn plan(paths: &Paths) -> Result<Plan> {
    let moves = directory_moves(paths)?;
    let mut report = json!({"status": "preview", "projects": [], "tasks": [], "directories": moves.iter().map(|(from, to)| json!({"from": from.to_string_lossy(), "to": to.to_string_lossy()})).collect::<Vec<_>>(), "blocked": [], "backupPath": null});
    for (from, to) in &moves {
        if paths::exists(to)? {
            report["blocked"].as_array_mut().unwrap().push(json!({"target": format!("directory {}", from.display()), "reason": format!("{} already exists; consolidate them by hand", to.display())}));
        }
    }
    let mut config = parse(
        &fs::read(&paths.config).map_err(|_| Error::from("Project configuration is unreadable"))?,
    )?;
    ensure(
        config["schemaVersion"] == 2,
        "Unsupported project schema; expected version 2",
    )?;
    object(&config["projects"])?;
    let mut tasks = raw_tasks(paths)?;
    let mut profiles = HashMap::new();
    let projects: Vec<_> = config["projects"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    for name in projects {
        let mut raw = config["projects"][&name].clone();
        object(&raw)?;
        let mut changes = Vec::new();
        let mut convert = || -> Result<()> {
            let legacy = raw.get("httpsPorts").is_some()
                && raw.get("url").is_none()
                && raw.get("hooks").is_none();
            if let Some(hooks) = raw.as_object_mut().unwrap().remove("setupHooks") {
                raw["hooks"] = hooks;
                changes.push("renamed setupHooks to hooks".into());
            }
            if legacy {
                let host = project_host(&name, &tasks)?;
                raw["url"] = json!(format!("https://{host}:{{{{httpsPort}}}}"));
                let mut hooks = raw.get("hooks").cloned().unwrap_or(json!({}));
                object(&hooks)?;
                for (phase, name) in [
                    ("afterDependencies", "publish-tailscale"),
                    ("cleanup", "cleanup-tailscale"),
                ] {
                    let mut entries = hooks
                        .get(phase)
                        .map(list)
                        .transpose()?
                        .cloned()
                        .unwrap_or_default();
                    entries.push(json!({"name": name, "command": COMMAND}));
                    hooks[phase] = json!(entries);
                }
                raw["hooks"] = hooks;
                changes.extend([
                    format!("added url {}", raw["url"].as_str().unwrap()),
                    "added publish-tailscale/cleanup-tailscale hooks".into(),
                ]);
                if raw.get("envOverrides").is_none() {
                    let key = raw["originsKey"].as_str().ok_or_else(|| {
                        Error::from("Legacy profile has no originsKey for its default assignments")
                    })?;
                    raw["envOverrides"] = legacy_overrides(key);
                    changes.push("spelled out the retired default envOverrides".into());
                }
            }
            if raw.as_object_mut().unwrap().remove("originsKey").is_some() {
                changes.push("removed originsKey".into());
            }
            if raw.get("hooks").is_some() && rewrite_hooks(&mut raw["hooks"])? {
                changes
                    .push("replaced the bun -e Tailscale import with graftr hook tailscale".into());
            }
            profiles.insert(name.clone(), profiles::parse(&raw)?.value().clone());
            Ok(())
        };
        match convert() {
            Ok(()) if !changes.is_empty() => report["projects"]
                .as_array_mut()
                .unwrap()
                .push(json!({"project": name, "changes": changes})),
            Ok(()) => (),
            Err(error) => report["blocked"]
                .as_array_mut()
                .unwrap()
                .push(json!({"target": format!("project {name}"), "reason": error.message})),
        };
        config["projects"][&name] = raw;
    }
    let mut converted = Vec::new();
    for raw in &mut tasks {
        if raw["schemaVersion"] == 4 {
            if raw.get("cleanup").is_some() {
                object(&raw["cleanup"])?;
                if rewrite(&mut raw["cleanup"], "hooks")? {
                    let task = records::Task::parse(raw)?.value().clone();
                    converted.push((task, "none".into()));
                    report["tasks"].as_array_mut().unwrap().push(json!({"id": js_string(raw.get("id")), "fromSchema": 4, "route": "none", "changes": ["replaced the bun -e Tailscale import in its recorded cleanup plan"]}));
                }
            }
            continue;
        }
        let id = js_string(raw.get("id"));
        match convert_task(raw, profiles.get(&js_string(raw.get("project")))) {
            Ok((task, route, changes)) => {
                let schema = raw["schemaVersion"].as_f64().or_else(|| {
                    raw["schemaVersion"]
                        .as_str()
                        .and_then(|s| s.parse::<f64>().ok())
                });
                converted.push((task, route.clone()));
                report["tasks"].as_array_mut().unwrap().push(
                    json!({"id": id, "fromSchema": schema, "route": route, "changes": changes}),
                );
            }
            Err(error) => report["blocked"]
                .as_array_mut()
                .unwrap()
                .push(json!({"target": format!("task {id}"), "reason": error.message})),
        }
    }
    if ["projects", "tasks", "directories", "blocked"]
        .iter()
        .all(|key| report[key].as_array().unwrap().is_empty())
    {
        report["status"] = json!("unchanged");
    }
    Ok(Plan {
        report,
        config,
        converted,
        moves,
    })
}
pub fn migrate(paths: &Paths, apply: bool) -> Result<Value> {
    records::private_dir(&paths.state)?;
    if !apply {
        return Ok(plan(paths)?.report);
    }
    let _state = locks::state(paths)?;
    let mut plan = plan(paths)?;
    let blocked = plan.report["blocked"].as_array().unwrap();
    ensure(
        blocked.is_empty(),
        format!(
            "Migration blocked: {}. Nothing was changed.",
            blocked
                .iter()
                .map(|b| format!(
                    "{}: {}",
                    b["target"].as_str().unwrap(),
                    b["reason"].as_str().unwrap()
                ))
                .collect::<Vec<_>>()
                .join("; ")
        ),
    )?;
    if plan.report["status"] == "unchanged" {
        return Ok(plan.report);
    }
    let mut backup = None;
    if !plan.report["projects"].as_array().unwrap().is_empty()
        || !plan.report["tasks"].as_array().unwrap().is_empty()
    {
        let root = paths.state.join("migration-backups");
        locks::private_directory(&root)?;
        let directory = root.join(health::timestamp().replace([':', '.'], "-"));
        locks::private_directory(&directory)?;
        stateio::atomic_write(
            &directory.join("projects.json"),
            &fs::read(&paths.config)
                .map_err(|_| Error::from("Could not back up previous configuration"))?,
            false,
        )?;
        for (task, _) in &plan.converted {
            let file = format!("{}.json", task["id"].as_str().unwrap());
            stateio::atomic_write(
                &directory.join(&file),
                &records::read_private(&paths.state.join(&file))?,
                false,
            )?;
        }
        for (task, route) in &plan.converted {
            if route != "none" {
                tailscale::migrate_receipt(paths, task, route)?;
            }
            stateio::write_task(paths, task)?;
        }
        if !plan.report["projects"].as_array().unwrap().is_empty() {
            stateio::atomic_write(
                &paths.config,
                format!("{}\n", serde_json::to_string_pretty(&plan.config).unwrap()).as_bytes(),
                false,
            )?;
        }
        backup = Some(directory);
    }
    for (from, to) in plan.moves {
        fs::rename(&from, &to).map_err(|_| {
            Error::from(
                "Could not move legacy directory; review migration progress before retrying",
            )
        })?;
        std::os::unix::fs::symlink(to.file_name().unwrap(), &from).map_err(|_| {
            Error::from("Could not retain the legacy directory alias; review the completed move")
        })?;
        if let Some(path) = &backup
            && path.starts_with(&from)
        {
            backup = Some(to.join(path.strip_prefix(&from).unwrap()));
        }
    }
    plan.report["status"] = json!("applied");
    plan.report["backupPath"] = json!(backup.map(|p| p.to_string_lossy().into_owned()));
    Ok(plan.report)
}
pub fn format(report: &Value, columns: usize) -> String {
    let status = report["status"].as_str().unwrap();
    let mut lines = vec![format!("Migration {}.", terminal::status_color(status))];
    for (key, label, id) in [("projects", "project", "project"), ("tasks", "task", "id")] {
        for item in report[key].as_array().unwrap() {
            lines.push(format!(
                "{label} {}: {}",
                item[id].as_str().unwrap(),
                item["changes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| s.as_str().unwrap())
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
    }
    for item in report["directories"].as_array().unwrap() {
        lines.push(format!(
            "directory {}: {} to {}, leaving a devtree symlink",
            item["from"].as_str().unwrap(),
            if status == "applied" { "moved" } else { "move" },
            item["to"].as_str().unwrap()
        ));
    }
    for item in report["blocked"].as_array().unwrap() {
        lines.push(format!(
            "{} {}: {}",
            terminal::status_color("BLOCKED"),
            item["target"].as_str().unwrap(),
            item["reason"].as_str().unwrap()
        ));
    }
    if let Some(path) = report["backupPath"].as_str() {
        lines.push(format!(
            "Private backup of the previous configuration and records: {path}"
        ));
    }
    if status == "preview"
        && ["projects", "tasks", "directories"]
            .iter()
            .any(|key| !report[key].as_array().unwrap().is_empty())
    {
        lines.push(
            "Preview only; rerun with --apply to convert. Tailscale Serve is not changed.".into(),
        );
    }
    terminal::wrap(&lines.join("\n"), columns)
}
