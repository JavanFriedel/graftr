use crate::{
    branches,
    checkout::string,
    health, herdr, human, inventory,
    lifecycle::Lifecycle,
    locks,
    paths::{self, Paths},
    profiles, records,
    scheduler::Scheduler,
    setup::{Engine, Options, Restore},
    stateio,
    validation::*,
};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

fn now() -> i64 {
    date_millis(&health::timestamp()).unwrap()
}
fn stamp(millis: i64) -> String {
    chrono::DateTime::from_timestamp_millis(millis)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
pub fn thaw_date(value: &str) -> Result<String> {
    let captures = pattern!(r"^([0-9]{4})-([0-9]{2})-([0-9]{2})T([0-9]{2}):([0-9]{2})(?::([0-9]{2})(\.[0-9]{1,3})?)?(Z|[+-][0-9]{2}:[0-9]{2})$").captures(value);
    let parsed = captures.and_then(|c| {
        if c[1] == *"0000"
            || &c[4] > "23"
            || &c[5] > "59"
            || c.get(6).is_some_and(|s| s.as_str() > "59")
        {
            return None;
        }
        let normalized = format!(
            "{}-{}-{}T{}:{}:{}{}{}",
            &c[1],
            &c[2],
            &c[3],
            &c[4],
            &c[5],
            c.get(6).map_or("00", |s| s.as_str()),
            c.get(7).map_or("", |s| s.as_str()),
            &c[8]
        );
        chrono::DateTime::parse_from_rfc3339(&normalized)
            .ok()
            .filter(|d| d.timestamp_millis() > now())
            .map(|d| d.timestamp_millis())
    });
    parsed.map(stamp).ok_or_else(|| {
        "--until requires a future ISO timestamp with timezone, e.g. 2026-11-06T09:00:00-05:00"
            .into()
    })
}
pub fn note(value: Option<&str>) -> Result<Value> {
    if let Some(value) = value {
        ensure(
            !value.trim_matches(js_space).is_empty()
                && value.encode_utf16().count() <= 2000
                && !value.chars().any(control),
            "--note must be a nonempty single line of at most 2000 characters, without terminal controls",
        )?;
    }
    Ok(json!(value))
}
pub fn resolve(paths: &Paths, selector: &str, project: Option<&str>) -> Result<Value> {
    if let Some(project) = project {
        name(project)?;
    }
    let records: Vec<_> = records::frozen(&paths.state)?
        .into_iter()
        .filter(|r| {
            project.is_none_or(|p| {
                r.value()["id"]
                    .as_str()
                    .unwrap()
                    .starts_with(&format!("{p}--"))
            })
        })
        .collect();
    if let Some(exact) = records.iter().find(|r| r.value()["id"] == selector) {
        return Ok(exact.value().clone());
    }
    let matches: Vec<_> = records
        .iter()
        .filter(|r| {
            r.reserves()
                && (r.value()["removal"]["currentBranch"] == selector
                    || r.value()["removal"]["path"] == selector
                    || Path::new(string(&r.value()["removal"], "path"))
                        .file_name()
                        .is_some_and(|n| n == selector))
        })
        .collect();
    ensure(
        matches.len() == 1,
        "Supply an unambiguous frozen task ID; inspect graftr frozen",
    )?;
    Ok(matches[0].value().clone())
}
pub struct Storage<'a, 'b> {
    pub engine: &'a Engine<'b>,
}
impl Storage<'_, '_> {
    fn lifecycle(&self) -> Lifecycle<'_, '_> {
        Lifecycle {
            engine: self.engine,
        }
    }
    fn scheduler(&self) -> Scheduler<'_> {
        Scheduler {
            paths: self.engine.paths,
        }
    }
    fn require(&self, id: &str, generation: Option<&str>) -> Result<Value> {
        records::frozen_task(&self.engine.paths.state, id)?
            .filter(|r| generation.is_none_or(|g| r.value()["generation"] == g))
            .map(|r| r.value().clone())
            .ok_or_else(|| "Frozen task is missing or its request was superseded".into())
    }
    fn save(&self, record: &Value) -> Result<()> {
        self.engine
            .progress
            .run("save-frozen", string(record, "id"), || {
                stateio::write_frozen(self.engine.paths, record)
            })
    }
    fn cancel(&self, record: &Value) -> Result<()> {
        self.engine
            .progress
            .run("cancel-thaw", string(record, "id"), || {
                self.scheduler().cancel(record)
            })
    }
    fn arm(&self, record: &mut Value, date: &str) -> Result<()> {
        if let Err(error) = self
            .engine
            .progress
            .run("schedule-thaw", string(record, "id"), || {
                self.scheduler().arm(record, date)
            })
        {
            record["message"] = json!(
                "Task remains frozen, but its timer could not be armed. Retry graftr schedule or thaw manually; the startup check can also recover an overdue return when enabled."
            );
            self.save(record)?;
            return Err(error);
        }
        Ok(())
    }
    pub fn freeze(
        &self,
        id: &str,
        confirmed: &Value,
        until: Option<&str>,
        memo: Option<&str>,
    ) -> Result<Value> {
        let until_date = until.map(thaw_date).transpose()?;
        let memo_value = note(memo)?;
        let prepared = {
            let _frozen = locks::frozen(self.engine.paths, id)?;
            if let Some(previous) = records::frozen_task(&self.engine.paths.state, id)?
                .filter(|r| r.value()["status"] != "thawed")
            {
                ensure(
                    previous.value()["status"] == "freezing",
                    "Task already has frozen recovery state; inspect graftr frozen",
                )?;
                ensure(
                    until.is_none() && memo.is_none(),
                    "Finish the interrupted freeze without new options, then use graftr schedule to change its return date",
                )?;
                previous.value().clone()
            } else {
                ensure(
                    confirmed["currentBranch"]
                        .as_str()
                        .is_some_and(|b| !b.is_empty())
                        && confirmed["currentCommit"]
                            .as_str()
                            .is_some_and(|c| !c.is_empty()),
                    "Freeze requires a verified retained branch and commit",
                )?;
                ensure(
                    self.lifecycle().preview(id)? == *confirmed,
                    "Confirmed freeze target changed; review and confirm again",
                )?;
                let task = records::task(&self.engine.paths.state, id)?;
                let task = task.value();
                let profile = profiles::configured(self.engine.paths, string(task, "project"))?;
                let root = string(profile.value(), "worktreeRoot");
                ensure(
                    absolute_normalized(root)
                        && string(task, "path") != root
                        && Path::new(string(task, "path")).starts_with(root),
                    "Freeze requires a checkout beneath the configured worktreeRoot so thaw can safely recreate it; use close for this checkout or review its project configuration",
                )?;
                if until_date.is_some() {
                    ensure(
                        !task["socket"].is_null(),
                        "Attach this task to Herdr before scheduling its return; manual freeze/thaw works standalone",
                    )?;
                    ensure(
                        Path::new(string(task, "socket")).is_absolute(),
                        "Scheduled thaw requires an absolute recorded Herdr socket path",
                    )?;
                    self.scheduler().preflight()?;
                }
                let receipt = records::setup_for(
                    &self.engine.paths.state,
                    task["socket"].as_str(),
                    string(task, "path"),
                )?;
                let record = json!({"schemaVersion": 1, "id": id, "generation": crate::system::uuid()?, "status": "freezing", "frozenAt": health::timestamp(), "until": until_date, "note": memo_value, "skipInstall": receipt.as_ref().is_some_and(|r| r.value()["skipInstall"] == true), "removal": confirmed, "attempts": 0, "nextAttemptAt": until_date, "message": "Freeze requested; checkout removal/retirement has not completed.", "thawedAt": null});
                {
                    let _state = locks::state(self.engine.paths)?;
                    self.save(&record)?;
                }
                record
            }
        };
        if paths::exists(&self.engine.paths.state.join(format!("{id}.json")))?
            && let Some(handoff) = self.lifecycle().handoff_freeze(&prepared)?
        {
            return Ok(handoff);
        }
        self.finish_freeze(id, string(&prepared, "generation"))
    }
    pub fn finish_freeze(&self, id: &str, generation: &str) -> Result<Value> {
        let _frozen = locks::frozen(self.engine.paths, id)?;
        let mut record = self.require(id, Some(generation))?;
        if record["status"] == "frozen" {
            return Ok(record);
        }
        ensure(
            record["status"] == "freezing",
            "Freeze worker was superseded; no checkout was removed",
        )?;
        if paths::exists(&self.engine.paths.state.join(format!("{id}.json")))? {
            let current = self.lifecycle().preview(id)?;
            let expected = &record["removal"];
            let mut identity = current.clone();
            identity["currentBranch"] = expected["currentBranch"].clone();
            identity["currentCommit"] = expected["currentCommit"].clone();
            ensure(
                identity == *expected
                    && (current["currentBranch"].is_null()
                        || current["currentBranch"] == expected["currentBranch"]
                            && current["currentCommit"] == expected["currentCommit"]),
                "Frozen removal target changed; keep the checkout and review its recovery record",
            )?;
            ensure(
                self.lifecycle().remove(id, &current)?["status"] == "removed",
                "Freeze needs its independent worker; checkout retirement has not completed",
            )?;
        }
        self.require_removed(&record)?;
        record["status"] = json!("frozen");
        record["message"] = json!(
            "Frozen: branch retained; checkout, ignored files and retired resources removed. Environment will be regenerated on thaw."
        );
        self.save(&record)?;
        if let Some(date) = record["until"].as_str().map(str::to_owned) {
            self.arm(&mut record, &date)?;
        }
        Ok(record)
    }
    pub fn schedule(&self, id: &str, until: Option<&str>) -> Result<Value> {
        let date = until.map(thaw_date).transpose()?;
        let _frozen = locks::frozen(self.engine.paths, id)?;
        let mut record = self.require(id, None)?;
        ensure(
            record["status"] == "frozen",
            "Only fully frozen tasks can be scheduled; inspect graftr frozen",
        )?;
        if date.is_some() {
            ensure(
                !record["removal"]["socket"].is_null(),
                "Scheduled thaw needs the task's recorded Herdr server",
            )?;
            ensure(
                Path::new(string(&record["removal"], "socket")).is_absolute(),
                "Scheduled thaw requires an absolute recorded Herdr socket path",
            )?;
            self.scheduler().preflight()?;
        }
        let previous = record.clone();
        record["generation"] = json!(crate::system::uuid()?);
        record["until"] = json!(date);
        record["nextAttemptAt"] = json!(date);
        record["attempts"] = json!(0);
        record["message"] = json!(if date.is_none() {
            "Scheduled return cancelled; task remains frozen."
        } else {
            "Scheduled return requested."
        });
        self.save(&record)?;
        self.cancel(&previous)?;
        if let Some(date) = date {
            self.arm(&mut record, &date)?;
        }
        Ok(record)
    }
    fn require_removed(&self, record: &Value) -> Result<()> {
        let removal = &record["removal"];
        ensure(
            !paths::exists(Path::new(string(removal, "path")))?
                && !inventory::registered(string(removal, "repo"))?
                    .iter()
                    .any(|t| t.path == string(removal, "path")),
            "Frozen checkout still exists or is registered; keep it and inspect ownership before recovery",
        )?;
        ensure(
            branches::local_commit(string(removal, "repo"), string(removal, "currentBranch"))?
                .as_deref()
                == removal["currentCommit"].as_str(),
            "Frozen branch snapshot is no longer retained; no branch was reset",
        )
    }
    fn preflight(
        &self,
        record: &Value,
        socket: Option<&str>,
        automatic: bool,
    ) -> Result<std::result::Result<Option<Value>, String>> {
        if automatic {
            ensure(
                socket.is_some(),
                "Scheduled thaw has no recorded Herdr attachment",
            )?;
            ensure(
                socket.is_some_and(|s| Path::new(s).is_absolute()),
                "Scheduled thaw requires an absolute recorded Herdr socket path",
            )?;
        }
        if let Some(socket) = socket {
            ensure(
                profiles::herdr_mode(self.engine.paths)? != "off",
                "Herdr integration is off; enable it or thaw manually with --no-herdr",
            )?;
            let spaces = match herdr::request(
                Some(socket),
                "workspace.list",
                json!({}),
                Duration::from_millis(1500),
            )
            .and_then(|r| herdr::workspaces(&r))
            {
                Ok(s) => s,
                Err(e)
                    if automatic
                        && ["HERDR_UNAVAILABLE", "HERDR_CONNECTION_UNAVAILABLE"]
                            .contains(&e.code) =>
                {
                    return Ok(Err("Recorded Herdr server is unavailable.".into()));
                }
                Err(e) => return Err(e),
            };
            let parents = spaces
                .iter()
                .filter(|s| s.checkout_path.as_deref() == record["removal"]["repo"].as_str())
                .count();
            if parents == 0 && automatic {
                return Ok(Err(
                    "Open the parent repository in the recorded Herdr server.".into(),
                ));
            }
            ensure(
                parents == 1,
                "Thaw requires exactly one parent repository workspace in the recorded Herdr server",
            )?;
        }
        let project = string(record, "id").split_once("--").unwrap().0;
        ensure(
            profiles::configured(self.engine.paths, project)?.value()["repo"]
                == record["removal"]["repo"],
            "Frozen project repository changed; no checkout was created",
        )?;
        ensure(
            branches::local_commit(
                string(&record["removal"], "repo"),
                string(&record["removal"], "currentBranch"),
            )?
            .as_deref()
                == record["removal"]["currentCommit"].as_str(),
            "Frozen branch is missing or moved; review Git history before thawing. No branch was reset",
        )?;
        let active = records::tasks(&self.engine.paths.state)?
            .into_iter()
            .find(|t| t.value()["id"] == record["id"])
            .map(|t| t.value().clone());
        ensure(
            active.as_ref().is_none_or(|t| {
                t["path"] == record["removal"]["path"]
                    && t["repo"] == record["removal"]["repo"]
                    && t["branch"] == record["removal"]["currentBranch"]
                    && t["baseCommit"] == record["removal"]["currentCommit"]
            }),
            "Another active checkout owns this frozen identity; no setup was run",
        )?;
        ensure(
            active.is_none() || !automatic,
            "An active task already owns this frozen identity; review it and thaw manually",
        )?;
        if active.is_none() {
            self.require_removed(record)?;
        }
        Ok(Ok(active))
    }
    fn welcome(&self, record: &Value, task: &Value) -> Result<()> {
        if task["socket"].is_null() || task["workspaceId"].is_null() {
            return Ok(());
        }
        let mut command = crate::scheduler::invocation()?;
        command.extend([
            "_thaw-message".into(),
            string(record, "id").into(),
            string(record, "generation").into(),
            self.engine.paths.config.to_string_lossy().into_owned(),
            self.engine.paths.state.to_string_lossy().into_owned(),
        ]);
        let reply = herdr::request(
            task["socket"].as_str(),
            "layout.apply",
            json!({"workspace_id": task["workspaceId"], "tab_label": "welcome back", "focus": false, "root": {"type": "pane", "label": "Graftr thaw", "cwd": record["removal"]["path"], "command": command}}),
            Duration::from_secs(60),
        )?;
        object(&reply["layout"])?;
        ensure(
            text(&reply["layout"]["workspace_id"])? == string(task, "workspaceId"),
            "Welcome layout belongs to another workspace",
        )?;
        let tab = text(&reply["layout"]["tab_id"])?;
        let tabs = herdr::tabs(&herdr::request(
            task["socket"].as_str(),
            "tab.move",
            json!({"tab_id": tab, "insert_index": 0}),
            Duration::from_secs(60),
        )?)?;
        ensure(
            tabs.first().is_some_and(|t| t.tab_id == tab),
            "Welcome tab was not confirmed at the first position",
        )
    }
    pub fn thaw(
        &self,
        id: &str,
        automatic: bool,
        generation: Option<&str>,
        no_herdr: bool,
    ) -> Result<Value> {
        let _frozen = locks::frozen(self.engine.paths, id)?;
        let mut record = self.require(id, None)?;
        if generation.is_some_and(|g| record["generation"] != g) {
            return Ok(
                json!({"id": id, "status": "skipped", "message": "Superseded thaw schedule; no resources changed."}),
            );
        }
        if record["status"] == "thawed" {
            if !record["until"].is_null() {
                self.cancel(&record)?;
            }
            return Ok(result(&record, "thawed"));
        }
        if automatic
            && (record["status"] != "frozen"
                || record["until"].is_null()
                || date_millis(string(&record, "until")).unwrap() > now())
        {
            return Ok(result(&record, "skipped"));
        }
        ensure(
            record["status"] != "freezing",
            "Freeze has not completed; retry graftr freeze before thawing",
        )?;
        let socket = if no_herdr {
            None
        } else {
            record["removal"]["socket"].as_str().map(str::to_owned)
        };
        let active = match self.preflight(&record, socket.as_deref(), automatic) {
            Ok(Ok(active)) => active,
            Ok(Err(reason)) => {
                let attempts = record["attempts"].as_f64().unwrap() as u64;
                let delay = (300000_i64 * 2_i64.pow(attempts.min(7) as u32)).min(21600000);
                let date = stamp(now() + delay);
                record["attempts"] = json!(attempts + 1);
                record["nextAttemptAt"] = json!(date);
                record["message"] = json!(format!(
                    "{reason} Thaw deferred; a one-off retry is scheduled for {date}."
                ));
                self.save(&record)?;
                self.arm(&mut record, &date)?;
                return Ok(result(&record, "deferred"));
            }
            Err(error) if automatic => {
                record["status"] = json!("attention");
                record["nextAttemptAt"] = Value::Null;
                record["message"] = json!(format!(
                    "{}. Automatic thaw stopped before setup; review and thaw manually.",
                    error.message
                ));
                self.save(&record)?;
                if !record["until"].is_null() {
                    self.cancel(&record)?;
                }
                return Ok(result(&record, "attention"));
            }
            Err(error) => return Err(error),
        };
        record["status"] = json!("thawing");
        record["nextAttemptAt"] = Value::Null;
        record["message"] = json!(
            "Thaw started. Interrupted setup requires explicit review/retry; it will not be replayed automatically."
        );
        self.save(&record)?;
        let mut operation = || -> Result<()> {
            if !record["until"].is_null() {
                self.cancel(&record)?;
            }
            let options = Options {
                wait: true,
                no_herdr,
                herdr_socket: socket.clone(),
                skip_install: Some(record["skipInstall"] == true),
                restore: Some(Restore {
                    generation: string(&record, "generation").into(),
                    repo: string(&record["removal"], "repo").into(),
                    path: string(&record["removal"], "path").into(),
                    commit: string(&record["removal"], "currentCommit").into(),
                }),
                ..Default::default()
            };
            let setup = if active.is_some() {
                self.engine.open(id, &options)?
            } else {
                let (project, name) = id.split_once("--").unwrap();
                let options = Options {
                    branch: Some(string(&record["removal"], "currentBranch").into()),
                    ..options
                };
                self.engine.create(project, name, "HEAD", &options)?
            };
            ensure(
                setup.task.is_some() && setup.receipt["status"] == "ready",
                string(&setup.receipt, "message"),
            )?;
            record["status"] = json!("thawed");
            record["thawedAt"] = json!(health::timestamp());
            record["message"] = json!(if automatic {
                "Thawed automatically at its scheduled return date. Setup complete."
            } else {
                "Thawed manually. Setup complete."
            });
            self.save(&record)?;
            self.welcome(&record, setup.task.as_ref().unwrap())
        };
        if let Err(error) = operation() {
            if record["status"] != "thawed" {
                record["status"] = json!("attention");
            }
            record["message"] = json!(if record["status"] == "thawed" {
                "Thaw and setup completed, but the welcome pane could not be confirmed. Inspect the returned Herdr workspace; setup will not be replayed automatically.".into()
            } else {
                format!(
                    "{}. Automatic setup retries are disabled; inspect graftr frozen/status and retry thaw explicitly after reviewing effects.",
                    error.message
                )
            });
            self.save(&record)?;
            if record["status"] != "thawed"
                && let Some(task) = records::tasks(&self.engine.paths.state)?
                    .iter()
                    .find(|t| t.value()["id"] == id)
            {
                let _ = self.welcome(&record, task.value());
            }
            return Ok(result(&record, "attention"));
        }
        Ok(result(&record, "thawed"))
    }
    pub fn due(&self, socket: &str) -> Result<Vec<Value>> {
        let mut results = Vec::new();
        for record in records::frozen(&self.engine.paths.state)? {
            let r = record.value();
            if r["status"] != "frozen"
                || r["until"].is_null()
                || date_millis(string(r, "until")).unwrap() > now()
                || r["removal"]["socket"] != socket
            {
                continue;
            }
            results.push(
                self.thaw(string(r, "id"), true, r["generation"].as_str(), false)
                    .unwrap_or_else(
                        |e| json!({"id": r["id"], "status": "attention", "message": e.message}),
                    ),
            );
        }
        Ok(results)
    }
}
fn result(record: &Value, status: &str) -> Value {
    json!({"id": record["id"], "status": status, "message": record["message"]})
}
pub fn public(record: &Value) -> Result<Value> {
    Ok(records::Frozen::parse(record)?.public())
}
pub fn format_record(record: &Value, columns: usize) -> String {
    human::fields(
        &format!("{}: {}", string(record, "id"), string(record, "status")),
        &[
            (
                "Branch",
                record["removal"]["currentBranch"]
                    .as_str()
                    .map(str::to_owned),
            ),
            (
                "Path",
                record["removal"]["path"].as_str().map(str::to_owned),
            ),
            (
                "Scheduled return",
                record["until"].as_str().map(str::to_owned),
            ),
            (
                "Next attempt",
                (record["attempts"].as_f64().unwrap() > 0.0)
                    .then(|| record["nextAttemptAt"].as_str().map(str::to_owned))
                    .flatten(),
            ),
            ("Note", record["note"].as_str().map(str::to_owned)),
        ],
        record["message"].as_str(),
        string(record, "status"),
        columns,
    )
}
pub fn format_thaw(result: &Value, columns: usize) -> String {
    human::fields(
        &format!("{}: {}", string(result, "id"), string(result, "status")),
        &[],
        result["message"].as_str(),
        string(result, "status"),
        columns,
    )
}
pub fn format_thaws(results: &[Value], columns: usize) -> String {
    if results.is_empty() {
        return "No overdue scheduled tasks for this Herdr server.".into();
    }
    let rows: Vec<_> = results
        .iter()
        .map(|r| {
            let (project, task) = string(r, "id")
                .split_once("--")
                .unwrap_or(("", string(r, "id")));
            crate::terminal::Row {
                cells: vec![project.into(), task.into(), string(r, "status").into()],
                details: vec![string(r, "message").into()],
            }
        })
        .collect();
    crate::terminal::format_table(&["PROJECT", "TASK", "RESULT"], &rows, columns, 2, &[])
}
pub fn message(record: &Value) -> String {
    let mut lines = vec![
        "Graftr — welcome back".into(),
        "".into(),
        string(record, "message")
            .chars()
            .map(|c| {
                if c <= '\u{1f}' || ('\u{7f}'..='\u{9f}').contains(&c) {
                    ' '
                } else {
                    c
                }
            })
            .collect(),
        "".into(),
        format!("Task: {}", record["id"]),
        format!("Branch: {}", record["removal"]["currentBranch"]),
        format!("Frozen: {}", string(record, "frozenAt")),
    ];
    if let Some(date) = record["until"].as_str() {
        lines.push(format!("Scheduled return: {date}"));
    }
    if let Some(note) = record["note"].as_str() {
        lines.push(format!("Your note: {note}"));
    }
    lines.extend(["".into(), "The environment was regenerated from current project configuration; old ignored files were not restored.".into(), "Development servers were not started.".into(), "".into(), "Inspect with graftr status. You can close this tab when finished.".into()]);
    lines.join("\n")
}
