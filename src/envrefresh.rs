use crate::{
    checkout::{self, string},
    envdoc, environment, inventory, locks,
    paths::Paths,
    profiles,
    progress::Progress,
    records, stateio, system, terminal,
    validation::*,
};
use serde_json::{Value, json};
use std::{
    fs::{self, Metadata, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

fn error(code: &'static str, message: &str) -> Error {
    Error {
        code,
        message: message.into(),
    }
}
#[derive(PartialEq)]
struct Stamp {
    dev: u64,
    ino: u64,
    mode: u32,
    uid: u32,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}
fn stamp(meta: &Metadata) -> Stamp {
    Stamp {
        dev: meta.dev(),
        ino: meta.ino(),
        mode: meta.mode(),
        uid: meta.uid(),
        size: meta.size(),
        mtime: (meta.mtime(), meta.mtime_nsec()),
        ctime: (meta.ctime(), meta.ctime_nsec()),
    }
}
#[derive(PartialEq)]
pub struct EnvironmentFile {
    pub bytes: Vec<u8>,
    stamp: Stamp,
}
pub fn read_environment(path: &str) -> Result<EnvironmentFile> {
    read_environment_mode(path, 0o077)
}
pub(crate) fn read_environment_mode(path: &str, forbidden_mode: u32) -> Result<EnvironmentFile> {
    let read = || -> std::result::Result<EnvironmentFile, ()> {
        if !absolute_normalized(path) || fs::canonicalize(path).map_err(|_| ())? != Path::new(path)
        {
            return Err(());
        }
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(|_| ())?;
        let before = file.metadata().map_err(|_| ())?;
        if !before.is_file()
            || before.uid() != unsafe { libc::getuid() }
            || before.mode() & forbidden_mode != 0
            || before.size() > envdoc::MAX_BYTES as u64
        {
            return Err(());
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take(envdoc::MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ())?;
        let after = file.metadata().map_err(|_| ())?;
        if bytes.len() > envdoc::MAX_BYTES
            || stamp(&before) != stamp(&after)
            || stamp(&after) != stamp(&fs::symlink_metadata(path).map_err(|_| ())?)
            || fs::canonicalize(path).map_err(|_| ())? != Path::new(path)
        {
            return Err(());
        }
        Ok(EnvironmentFile {
            bytes,
            stamp: stamp(&after),
        })
    };
    read().map_err(|_| error("ENV_FILE_UNAVAILABLE", "Environment files must be stable, private regular files owned by you, at canonical non-symlink paths (max 4 MiB). Contents were not logged; no file was repaired or created."))
}
fn verify_checkout(task: &Value) -> Result<()> {
    for key in ["repo", "path"] {
        let path = string(task, key);
        ensure(
            absolute_normalized(path) && fs::canonicalize(path).is_ok_and(|p| p == Path::new(path)),
            error(
                "ENV_REFRESH_OWNERSHIP",
                "Refresh requires canonical repository/checkout paths; no symlink or moved checkout is adopted.",
            ),
        )?;
    }
    let trees = inventory::registered(string(task, "repo"))?;
    let tree = trees.iter().find(|tree| tree.path == string(task, "path"));
    let matches = task["path"] != task["repo"]
        && !trees
            .first()
            .is_some_and(|t| t.path == string(task, "path"))
        && tree.is_some_and(|t| {
            !t.bare
                && !t.detached
                && t.branch.as_deref() == task["branch"].as_str()
                && t.locked.is_none()
                && t.prunable.is_none()
        })
        && checkout::verify(task).is_ok()
        && system::git(
            Path::new(string(task, "path")),
            &["symbolic-ref", "--quiet", "HEAD"],
        )? == format!("refs/heads/{}", string(task, "branch"));
    ensure(
        matches,
        error(
            "ENV_REFRESH_OWNERSHIP",
            "Checkout registration, branch, or repository ownership changed; preserve files and review it before refresh.",
        ),
    )?;
    ensure(
        system::git(
            Path::new(string(task, "path")),
            &["ls-files", "-z", "--", ".env"],
        )?
        .is_empty(),
        error(
            "ENV_REFRESH_TRACKED",
            "The target .env is tracked by Git; refresh refuses to read or replace it.",
        ),
    )
}
#[derive(PartialEq)]
struct Context {
    task: Value,
    profile: Value,
    receipt: Option<Value>,
    target: String,
    baseline: EnvironmentFile,
    current: EnvironmentFile,
}
fn context(paths: &Paths, id: &str) -> Result<Context> {
    records::private_dir(&paths.state)?;
    let task = records::task(&paths.state, id)?.value().clone();
    let profile = profiles::configured(paths, string(&task, "project"))?
        .value()
        .clone();
    let env_source = profile["envSource"].as_str();
    let templates = profile
        .get("envOverrides")
        .cloned()
        .unwrap_or(json!({"PORT": "{{localPort}}"}));
    ensure(
        profile["repo"] == task["repo"]
            && env_source.is_some_and(|p| Path::new(p).file_name().is_some_and(|n| n == ".env"))
            && templates == task["envOverrides"]
            && environment::resolve(&templates, &task)? == task["overrides"],
        error(
            "ENV_REFRESH_CONFIG",
            "Refresh requires an approved .env development baseline and unchanged recorded repository/public assignments. Review this project's profile; no values were logged.",
        ),
    )?;
    let receipt = records::setup_for(&paths.state, task["socket"].as_str(), string(&task, "path"))?
        .map(|r| r.value().clone());
    let receipt_status = receipt.as_ref().and_then(|r| r["status"].as_str());
    ensure(
        receipt_status != Some("pending")
            && !(task["status"] == "preparing" && receipt_status != Some("incomplete")),
        error(
            "ENV_REFRESH_BUSY",
            "Setup is pending/preparing without a recorded failure. Review its worker/log and wait for completion before refreshing; do not bypass its lock or infer that the worker stopped.",
        ),
    )?;
    ensure(
        !receipt.as_ref().is_some_and(|r| {
            r["taskId"] != task["id"]
                || r["project"] != task["project"]
                || r["workspaceId"] != task["workspaceId"]
        }),
        error(
            "ENV_REFRESH_OWNERSHIP",
            "The setup receipt belongs to a different task/project/workspace attempt; review it before refresh.",
        ),
    )?;
    ensure(
        !records::tasks(&paths.state)?.iter().any(|r| {
            let other = r.value();
            other["id"] != task["id"]
                && (other["path"] == task["path"]
                    || other["repo"] == task["repo"] && other["branch"] == task["branch"])
        }),
        error(
            "ENV_REFRESH_OWNERSHIP",
            "Conflicting checkout/branch ownership requires manual review before environment refresh.",
        ),
    )?;
    verify_checkout(&task)?;
    let target = format!("{}/.env", string(&task, "path"));
    ensure(
        env_source != Some(target.as_str()),
        error(
            "ENV_REFRESH_CONFIG",
            "Baseline and target must be different files; the source is never refreshed.",
        ),
    )?;
    let baseline = read_environment(env_source.unwrap())?;
    let current = read_environment(&target)?;
    Ok(Context {
        task,
        profile,
        receipt,
        target,
        baseline,
        current,
    })
}
fn unchanged(paths: &Paths, original: &Context) -> Result<()> {
    ensure(
        context(paths, string(&original.task, "id"))? == *original,
        error(
            "ENV_REFRESH_CHANGED",
            "Task, configuration, setup receipt, baseline, or target changed during refresh. Review the current files privately and preview again; no stale plan will be applied.",
        ),
    )
}
pub fn refresh(
    paths: &Paths,
    id: &str,
    apply: bool,
    selected: &[String],
    progress: &Progress,
) -> Result<Value> {
    let initial = records::task(&paths.state, id)?;
    let perform = || -> Result<Value> {
        let original = context(paths, id)?;
        ensure(
            original.task == *initial.value(),
            error(
                "ENV_REFRESH_CHANGED",
                "Task changed before refresh began; preview again after reviewing its identity.",
            ),
        )?;
        let plan = envdoc::plan(
            &original.baseline.bytes,
            &original.current.bytes,
            &original.task["overrides"],
            selected,
        )?;
        let review = plan
            .entries
            .iter()
            .any(|r| r["status"] == "different" && r["action"] == "preserve");
        let mut report = json!({"id": id, "project": original.task["project"], "path": original.task["path"], "mode": if apply {"apply"} else {"preview"}, "status": if apply {"unchanged"} else {"preview"}, "entries": plan.entries, "reviewRequired": review, "backupPath": null});
        unchanged(paths, &original)?;
        if !apply || plan.content == original.current.bytes {
            return Ok(report);
        }
        ensure(
            system::run(
                &[
                    "git",
                    "-C",
                    &paths.state.to_string_lossy(),
                    "rev-parse",
                    "--is-inside-work-tree",
                ],
                None,
                false,
            )?
            .exit_code
                != 0,
            error(
                "ENV_BACKUP_LOCATION",
                "The private state directory is inside a Git worktree; refresh refuses to save secret-bearing backups there. Review the registry location without splitting ownership records/leases.",
            ),
        )?;
        progress.run("backup-environment", id, || -> Result<()> {
            let root = paths.state.join("env-backups"); locks::private_directory(&root)?;
            ensure(system::run(&["git", "-C", &root.to_string_lossy(), "rev-parse", "--is-inside-work-tree"], None, false)?.exit_code != 0,
                error("ENV_BACKUP_LOCATION", "The backup directory belongs to a Git repository; no secret-bearing recovery file was written there. Review its location privately."))?;
            let backup = root.join(format!("{id}-{}.env", system::uuid()?));
            stateio::atomic_write(&backup, &original.current.bytes, true)?;
            stateio::sync_directory(&paths.state)?;
            report["backupPath"] = json!(backup.to_string_lossy()); Ok(())
        })?;
        unchanged(paths, &original)?;
        progress.run("refresh-environment", id, || {
            stateio::atomic_write(Path::new(&original.target), &plan.content, false)
        })?;
        report["status"] = json!("applied");
        Ok(report)
    };
    if !apply {
        return perform();
    }
    let _tree_lock = locks::worktree(paths, string(initial.value(), "path"), true)?;
    let _state_lock = locks::state(paths)?;
    perform()
}
pub fn format(report: &Value, columns: usize) -> String {
    let mut lines = vec![format!(
        "{}: environment {} ({}; values hidden)",
        string(report, "id"),
        terminal::status_color(string(report, "status")),
        string(report, "mode")
    )];
    for entry in report["entries"].as_array().unwrap() {
        lines.push(format!(
            "  {}: {}; {}",
            string(entry, "key"),
            terminal::status_color(string(entry, "status")),
            terminal::status_color(string(entry, "action"))
        ));
    }
    if !report["backupPath"].is_null() {
        lines.push(format!("Private recovery backup: {}", report["backupPath"]));
    }
    if report["reviewRequired"] == true {
        lines.push("Different existing values were preserved. There is no baseline history to distinguish local edits from baseline changes; select a key with --take-baseline only after private review.".into());
    }
    lines.push("No keys are removed. Managed assignments are preserved. Existing shells/processes are not refreshed or restarted.".into());
    if report["mode"] == "preview" {
        lines.push("Preview only; no files or locks were written. --apply adds missing keys and replaces only explicitly selected differing keys.".into());
    }
    terminal::wrap(&lines.join("\n"), columns)
}
