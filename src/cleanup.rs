use crate::{
    checkout::string, hooks, locks, logs, paths::Paths, progress::Progress, records, stateio,
    status::compare, validation::*,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub fn pending(task: &Value, kind: &str) -> bool {
    task.get(kind).is_some() && task[kind]["status"] != "complete"
}
pub fn queue(paths: &Paths, task: &mut Value, kind: &str) -> Result<()> {
    if task[kind]["status"] == "armed" {
        task[kind]["status"] = json!("queued");
        stateio::write_task(paths, task)?;
    }
    Ok(())
}
fn identity(task: &Value) -> Value {
    let mut overrides: Vec<_> = task["overrides"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| json!([k, v]))
        .collect();
    overrides.sort_by(|a, b| compare(a[0].as_str().unwrap(), b[0].as_str().unwrap()));
    json!([
        task["id"],
        task["path"],
        task["repo"],
        task["branch"],
        task["baseCommit"],
        task["socket"],
        task["workspaceId"],
        task["localPort"],
        task["httpsPort"],
        task["ports"],
        task["host"],
        task["url"],
        task["cleanup"]["runId"],
        task["cleanup"]["hooks"],
        task["cleanup"]["context"],
        task["retire"]["runId"],
        task["retire"]["hooks"],
        task["retire"]["context"],
        overrides
    ])
}
pub fn run(
    paths: &Paths,
    expected: &Value,
    progress: &Progress,
    explicit: bool,
    kind: &str,
) -> Result<()> {
    let Some(recorded) = expected.get(kind) else {
        return Ok(());
    };
    let label = if kind == "cleanup" {
        "Cleanup"
    } else {
        "Retirement"
    };
    let context = identity(expected);
    let current = || -> Result<Value> {
        let task = records::task(&paths.state, string(expected, "id"))?
            .value()
            .clone();
        ensure(
            identity(&task) == context,
            format!("{label} ownership/context changed; retained resources need review"),
        )?;
        Ok(task)
    };
    let active = {
        let _lock = locks::state(paths)?;
        let mut task = current()?;
        if task[kind]["status"] == "complete" {
            false
        } else {
            let plan = &mut task[kind];
            if plan["results"].as_array().unwrap().len() == plan["hooks"].as_array().unwrap().len()
                && plan["results"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|r| r["status"] == "succeeded")
            {
                plan["status"] = json!("complete");
                stateio::write_task(paths, &task)?;
                false
            } else {
                ensure(
                    ![Some("running"), Some("failed")].contains(&plan["status"].as_str())
                        || explicit
                            && recorded["attempt"] == plan["attempt"]
                            && recorded["status"] == plan["status"],
                    format!(
                        "{label} previously failed, was interrupted or changed while waiting; inspect effects before a fresh explicit graftr {}remove retry",
                        if kind == "cleanup" { "cleanup/" } else { "" }
                    ),
                )?;
                ensure(
                    records::setup_for(
                        &paths.state,
                        task["socket"].as_str(),
                        string(&task, "path"),
                    )?
                    .is_none_or(|r| r.value()["status"] != "pending"),
                    format!("Setup was not fenced before {kind}; no hook was started"),
                )?;
                task[kind]["status"] = json!("queued");
                task[kind]["attempt"] = json!(task[kind]["attempt"].as_f64().unwrap() + 1.0);
                stateio::write_task(paths, &task)?;
                true
            }
        }
    };
    if !active {
        return Ok(());
    }
    let root = paths.state.join("hook-work");
    locks::private_directory(&root)?;
    let directory = root.join(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&json!([expected["id"], recorded["runId"]])).unwrap())
    ));
    locks::private_directory(&directory)?;
    for index in 0..recorded["hooks"].as_array().unwrap().len() {
        let running = {
            let _lock = locks::state(paths)?;
            let mut task = current()?;
            if task[kind]["results"]
                .get(index)
                .is_some_and(|r| r["status"] == "succeeded")
            {
                None
            } else {
                let hook = task[kind]["hooks"][index].clone();
                task[kind]["results"]
                    .as_array_mut()
                    .unwrap()
                    .truncate(index);
                task[kind]["results"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"phase": kind, "name": hook["name"], "status": "running"}));
                task[kind]["status"] = json!("running");
                stateio::write_task(paths, &task)?;
                let dependencies = records::setup_for(
                    &paths.state,
                    task["socket"].as_str(),
                    string(&task, "path"),
                )?
                .map_or("unknown".into(), |r| {
                    string(r.value(), "dependencies").to_owned()
                });
                Some((task, hook, dependencies))
            }
        };
        let Some((mut task, hook, dependencies)) = running else {
            continue;
        };
        task["overrides"] = task[kind]["context"].clone();
        let mut failure_result = None;
        let mut operation = || -> Result<Value> {
            let log = logs::open(
                paths,
                string(expected, "id"),
                &format!("{kind}-{}", string(&hook, "name")),
                &format!(
                    "{kind} hook {} for {}",
                    string(&hook, "name"),
                    string(expected, "id")
                ),
            )?;
            progress.run("run-hook", string(expected, "id"), || {
                hooks::run(
                    &hook,
                    kind,
                    &task,
                    &dependencies,
                    string(recorded, "runId"),
                    &directory,
                    log,
                )
                .map_err(|failure| {
                    failure_result = Some(failure.result);
                    failure.error
                })
            })
        };
        let (outcome, error) = match operation() {
            Ok(outcome) => (outcome, None),
            Err(error) => (failure_result.unwrap_or(json!({"phase": kind, "name": hook["name"], "status": "failed", "reason": "context-changed", "elapsedMs": 0})), Some(error)),
        };
        {
            let _lock = locks::state(paths)?;
            let mut task = current()?;
            task[kind]["results"][index] = outcome;
            task[kind]["status"] = json!(if error.is_some() { "failed" } else { "running" });
            stateio::write_task(paths, &task)?;
        }
        if let Some(error) = error {
            return Err(error);
        }
    }
    let _lock = locks::state(paths)?;
    let mut task = current()?;
    task[kind]["status"] = json!("complete");
    stateio::write_task(paths, &task)
}
