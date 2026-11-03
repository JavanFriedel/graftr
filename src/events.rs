use crate::{
    checkout::{self, string},
    cleanup, herdr,
    lifecycle::Lifecycle,
    locks, paths, profiles, records,
    setup::{Engine, Options, SetupResult},
    stateio,
    storage::Storage,
    validation::*,
};
use serde_json::{Value, json};

pub enum Reply {
    Setup(Box<SetupResult>),
    Native(Value),
}
pub struct Events<'a, 'b> {
    pub engine: &'a Engine<'b>,
}
impl Events<'_, '_> {
    fn lifecycle(&self) -> Lifecycle<'_, '_> {
        Lifecycle {
            engine: self.engine,
        }
    }
    pub fn handle(&self, value: &Value) -> Result<Option<Reply>> {
        let event = object(value)?;
        let data = field(event, "data")?;
        object(data)?;
        if [Some("worktree_created"), Some("worktree_opened")].contains(&value["event"].as_str()) {
            ensure(
                data["type"] == value["event"],
                "Invalid Herdr setup EventEnvelope",
            )?;
            object(&data["workspace"])?;
            let workspace = text(&data["workspace"]["workspace_id"])?;
            object(&data["workspace"]["worktree"])?;
            let provenance = &data["workspace"]["worktree"];
            let path = text(&provenance["checkout_path"])?;
            ensure(
                text(&data["worktree"]["path"])? == path,
                "Worktree event path does not match its workspace",
            )?;
            if records::adoptions(&self.engine.paths.state)?
                .iter()
                .any(|r| r.value()["path"] == path)
            {
                return Ok(None);
            }
            let socket = herdr::caller_socket()?;
            let repo = text(&provenance["repo_root"])?;
            object(&data["worktree"])?;
            let branch = data["worktree"]["branch"].as_str();
            if profiles::herdr_mode(self.engine.paths)? == "off" {
                return Ok(None);
            }
            if let Some(branch) = branch {
                if stateio::has_creation_intent(self.engine.paths, &socket, repo, branch)? {
                    return Ok(None);
                }
                let sources = self.engine.workspaces(&socket)?;
                for source in sources {
                    if source.checkout_path.as_deref() == Some(repo)
                        && stateio::has_creation_intent(
                            self.engine.paths,
                            &socket,
                            &format!("workspace:{}", source.workspace_id),
                            branch,
                        )?
                    {
                        return Ok(None);
                    }
                }
            }
            let receipt = records::setup_for(&self.engine.paths.state, Some(&socket), path)?;
            let tasks = records::tasks(&self.engine.paths.state)?;
            let owned = tasks.iter().find(|t| {
                (t.value()["socket"].is_null() || t.value()["socket"] == socket)
                    && t.value()["path"] == path
            });
            if receipt.as_ref().is_some_and(|r| {
                r.value()["status"] == "skipped"
                    || r.value()["status"] == "pending" && r.value()["workspaceId"] == workspace
            }) || value["event"] == "worktree_created"
                && receipt.is_some()
                && owned.is_none_or(|t| !t.value()["socket"].is_null())
                || value["event"] == "worktree_opened"
                    && owned.is_none_or(|t| t.value()["workspaceId"] == workspace)
            {
                return Ok(None);
            }
            let options = Options {
                automatic: true,
                expected_path: Some(path.into()),
                expected_repo: Some(repo.into()),
                expected_branch: branch.map(str::to_owned),
                ..Default::default()
            };
            let result = match self.engine.setup_workspace(&socket, workspace, &options) {
                Ok(result) => result,
                Err(error) => SetupResult {
                    receipt: json!({"schemaVersion": 1, "socket": socket, "workspaceId": workspace, "path": path, "project": null, "taskId": null, "status": "unmanaged", "step": "profile", "message": format!("Setup could not start: {}. Checkout kept; retry graftr setup.", error.message), "installKey": null, "dependencies": "not-configured", "skipInstall": false, "runId": null}),
                    task: None,
                    local: false,
                    log_id: None,
                    attention_needed: false,
                },
            };
            self.engine.report_setup(&result);
            return Ok(Some(Reply::Setup(Box::new(result))));
        }
        let event = text(&value["event"])?;
        ensure(
            ["workspace_closed", "worktree_removed"].contains(&event) && data["type"] == event,
            "Invalid Herdr cleanup EventEnvelope",
        )?;
        Ok(Some(Reply::Native(self.cleanup_native(event, data)?)))
    }
    fn cleanup_native(&self, event: &str, data: &Value) -> Result<Value> {
        let workspace = text(&data["workspace_id"])?;
        let socket = herdr::caller_socket()?;
        let report = |status: &str, message: &str, id: Option<&str>| {
            let mut value = json!({"event": event, "nativeStatus": "completed", "cleanupStatus": status, "message": message});
            if let Some(id) = id {
                value["id"] = json!(id);
            }
            value
        };
        let pending = {
            let _state = locks::state(self.engine.paths)?;
            let tasks = records::tasks(&self.engine.paths.state)?;
            let matches: Vec<_> = tasks
                .iter()
                .filter(|t| t.value()["socket"] == socket && t.value()["workspaceId"] == workspace)
                .collect();
            ensure(
                matches.len() <= 1,
                "Multiple records claim the native event workspace; review ownership before cleanup",
            )?;
            let Some(record) = matches.first() else {
                if event != "worktree_removed" {
                    return Ok(report(
                        "ignored",
                        "No current task owns this workspace; no resources changed.",
                        None,
                    ));
                }
                let path = text(&data["worktree"]["path"])?;
                let receipt = records::setup_for(&self.engine.paths.state, Some(&socket), path)?;
                if receipt.as_ref().is_none_or(|r| {
                    r.value()["workspaceId"] != workspace || !r.value()["taskId"].is_null()
                }) || paths::exists(std::path::Path::new(path))?
                    || tasks.iter().any(|t| t.value()["path"] == path)
                {
                    return Ok(report(
                        "ignored",
                        "No retired plain-checkout receipt was verified; no resources changed.",
                        None,
                    ));
                }
                if stateio::delete_setup(
                    self.engine.paths,
                    &json!({"socket": socket, "path": path}),
                )
                .is_err()
                {
                    return Ok(report(
                        "incomplete",
                        "Native removal completed, but its plain-checkout receipt could not be cleared. Review the private setup records; no native operation was replayed.",
                        None,
                    ));
                }
                return Ok(report(
                    "complete",
                    "Native removal completed; its plain-checkout receipt was cleared.",
                    None,
                ));
            };
            let mut task = record.value().clone();
            let id = string(&task, "id").to_owned();
            ensure(
                tasks
                    .iter()
                    .filter(|t| t.value()["path"] == task["path"])
                    .count()
                    == 1,
                "Multiple records claim the native event checkout; review ownership before cleanup",
            )?;
            if !data["workspace"].is_null() {
                object(&data["workspace"])?;
                ensure(
                    text(&data["workspace"]["worktree"]["checkout_path"])? == string(&task, "path"),
                    "Close snapshot does not match the owned checkout",
                )?;
            }
            if event == "worktree_removed" {
                ensure(
                    text(&data["worktree"]["path"])? == string(&task, "path"),
                    "Removed checkout does not match the task or is still registered",
                )?;
                self.lifecycle().require_removed(&task)?;
            }
            let mut close = || -> Result<()> {
                self.lifecycle().record_closure(
                    &mut task,
                    if event == "worktree_removed" {
                        "Checkout removed by Herdr"
                    } else {
                        "Workspace closed"
                    },
                )?;
                cleanup::queue(self.engine.paths, &mut task, "cleanup")
            };
            if close().is_err() {
                return Ok(report(
                    "incomplete",
                    "Native operation completed, but Graftr cleanup failed. Records and leases are retained; inspect status and retry cleanup/removal after resolving the failure. Service/record contents were not logged.",
                    Some(&id),
                ));
            }
            if event == "workspace_closed" && !cleanup::pending(&task, "cleanup") {
                return Ok(report(
                    "complete",
                    "Native close completed; setup was fenced and configured cleanup completed. Checkout and leases remain.",
                    Some(&id),
                ));
            }
            task
        };
        let id = string(&pending, "id");
        let operation = || -> Result<Value> {
            let _tree = locks::worktree(self.engine.paths, string(&pending, "path"), false)?;
            let owned = {
                let _state = locks::state(self.engine.paths)?;
                if !paths::exists(&self.engine.paths.state.join(format!("{id}.json")))? {
                    None
                } else {
                    let task = records::task(&self.engine.paths.state, id)?.value().clone();
                    checkout::require_identity(&task, &pending)?;
                    ensure(task["status"] == "closed", "Native cleanup was superseded")?;
                    if event == "worktree_removed" {
                        self.lifecycle().require_removed(&task)?;
                    }
                    Some(task)
                }
            };
            if let Some(task) = &owned {
                cleanup::run(
                    self.engine.paths,
                    task,
                    self.engine.progress,
                    false,
                    "cleanup",
                )?;
                if event == "worktree_removed" && cleanup::pending(task, "retire") {
                    let queued = {
                        let _state = locks::state(self.engine.paths)?;
                        let mut current =
                            records::task(&self.engine.paths.state, id)?.value().clone();
                        checkout::require_identity(&current, &pending)?;
                        self.lifecycle().require_removed(&current)?;
                        cleanup::queue(self.engine.paths, &mut current, "retire")?;
                        current
                    };
                    cleanup::run(
                        self.engine.paths,
                        &queued,
                        self.engine.progress,
                        false,
                        "retire",
                    )?;
                }
            }
            let _state = locks::state(self.engine.paths)?;
            if !paths::exists(&self.engine.paths.state.join(format!("{id}.json")))? {
                return Ok(report(
                    "complete",
                    "Another cleanup already retired this task.",
                    Some(id),
                ));
            }
            let task = records::task(&self.engine.paths.state, id)?;
            let task = task.value();
            checkout::require_identity(task, &pending)?;
            ensure(
                task["status"] == "closed"
                    && records::tasks(&self.engine.paths.state)?
                        .iter()
                        .filter(|t| t.value()["path"] == task["path"])
                        .count()
                        == 1,
                "Native cleanup was superseded; ownership retained",
            )?;
            if event == "workspace_closed" {
                return Ok(report(
                    "complete",
                    "Native close completed; setup finished and recorded cleanup hooks completed. Checkout and leases remain.",
                    Some(id),
                ));
            }
            self.lifecycle().require_removed(task)?;
            ensure(
                !cleanup::pending(task, "retire"),
                "Retire hooks are unfinished",
            )?;
            self.engine.progress.run("delete-setup", id, || {
                stateio::delete_setup(self.engine.paths, task)
            })?;
            self.engine.progress.run("delete-task", id, || {
                stateio::delete_task(self.engine.paths, task)
            })?;
            Ok(report(
                "complete",
                "Native removal completed; no setup worker holds its lock, checkout disappearance was reverified, and owned resources were retired.",
                Some(id),
            ))
        };
        Ok(operation().unwrap_or_else(|_| report("incomplete", if event == "workspace_closed" {"Native close completed, but its recorded cleanup hooks are unfinished or uncertain. Keep records/leases; inspect worker activity and external effects before an explicit cleanup retry."} else {"Native removal completed, but retirement could not be verified after waiting for setup/cleanup. Keep records/leases; inspect worker activity, checkout/Git registration and ownership before retrying removal. No destructive operation was replayed."}, Some(id))))
    }
    pub fn startup(&self, socket: &str) -> Value {
        let mut report = json!({"reconciliation": {"status": "completed", "message": "Reconciled owned workspaces."}, "thaw": {"status": "completed", "message": "Checked overdue scheduled tasks for this Herdr server.", "results": []}});
        if let Err(error) = self.lifecycle().reconcile() {
            report["reconciliation"] = json!({"status": "attention", "message": error.message});
        }
        let perform = || -> Result<Value> {
            if !profiles::startup_thaw(self.engine.paths)? {
                return Ok(
                    json!({"status": "disabled", "message": "Startup thaw is disabled by startupThaw=false; per-task timers are unchanged.", "results": []}),
                );
            }
            if profiles::herdr_mode(self.engine.paths)? == "off" {
                return Ok(
                    json!({"status": "disabled", "message": "Herdr integration is off; no startup thaws were attempted.", "results": []}),
                );
            }
            let results = Storage {
                engine: self.engine,
            }
            .due(socket)?;
            Ok(
                json!({"status": if results.iter().any(|r| r["status"] == "attention") {"attention"} else {"completed"}, "message": if results.iter().any(|r| r["status"] == "attention") {"Some scheduled returns need review; inspect the results and graftr frozen/status."} else {"Checked overdue scheduled tasks for this Herdr server."}, "results": results}),
            )
        };
        report["thaw"] = perform().unwrap_or_else(
            |error| json!({"status": "attention", "message": error.message, "results": []}),
        );
        report
    }
}
