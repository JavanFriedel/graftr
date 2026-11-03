use crate::{
    herdr, inventory,
    paths::Paths,
    profiles,
    records::{self, Adoption, Task},
    system,
    terminal::{status_color, wrap},
    validation::*,
};
use serde_json::{Value, json};
use std::{fs, path::Path, time::Duration};

fn finding(
    area: &str,
    status: &str,
    code: &str,
    message: impl Into<String>,
    next: &[&str],
) -> Value {
    json!({"area": area, "status": status, "code": code, "message": message.into(), "nextSteps": next})
}
fn canonical(path: &str) -> Result<String> {
    fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|_| "Could not establish canonical directory".into())
}
fn s<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().expect("validated record string")
}
fn leased(task: &Value) -> Vec<f64> {
    let mut ports = vec![task["localPort"].as_f64().unwrap()];
    if let Some(port) = task["httpsPort"].as_f64() {
        ports.push(port);
    }
    if let Some(named) = task["ports"].as_object() {
        ports.extend(named.values().map(|port| port.as_f64().unwrap()));
    }
    ports
}
fn checkout(task: &Value) -> Value {
    let review = [
        "Preserve the checkout, branch, records, and leases. Review Git worktree registration and the recorded path/branch before any setup or removal.",
    ];
    let observe = || -> Result<Value> {
        let path = s(task, "path");
        let repo = s(task, "repo");
        if !absolute_normalized(path) || !absolute_normalized(repo) {
            return Ok(finding(
                "checkout",
                "attention",
                "noncanonical-path",
                "A recorded checkout/repository path is not absolute and normalized.",
                &review,
            ));
        }
        let present = match fs::symlink_metadata(path) {
            Ok(metadata) => {
                if !metadata.is_dir() || canonical(path)? != path {
                    return Ok(finding(
                        "checkout",
                        "attention",
                        "noncanonical-path",
                        "The recorded checkout is not a canonical directory; symlinks are not adopted.",
                        &review,
                    ));
                }
                true
            }
            Err(error)
                if [Some(libc::ENOENT), Some(libc::ENOTDIR)].contains(&error.raw_os_error()) =>
            {
                false
            }
            Err(_) => return Err("Could not inspect checkout".into()),
        };
        let registered = inventory::registered(repo)?;
        let tree = registered.iter().find(|tree| tree.path == path);
        if !present {
            return Ok(finding(
                "checkout",
                "attention",
                if tree.is_some() {
                    "missing-checkout"
                } else {
                    "checkout-and-registration-missing"
                },
                if tree.is_some() {
                    "The checkout is missing but Git still has its registration."
                } else {
                    "Both checkout and registration are missing; this may be interrupted removal, not permission to recreate or discard ownership."
                },
                &review,
            ));
        }
        if path == repo || registered.first().is_some_and(|tree| tree.path == path) {
            return Ok(finding(
                "checkout",
                "attention",
                "main-checkout",
                "The task record points at the main checkout.",
                &review,
            ));
        }
        let Some(tree) = tree else {
            return Ok(finding(
                "checkout",
                "attention",
                "not-registered",
                "The directory is not registered with the recorded repository.",
                &review,
            ));
        };
        if canonical(repo)? != repo
            || system::git(Path::new(path), &["rev-parse", "--show-toplevel"])? != path
            || system::git(
                Path::new(path),
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )? != system::git(
                Path::new(repo),
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )?
        {
            return Ok(finding(
                "checkout",
                "attention",
                "repository-mismatch",
                "The directory's Git repository disagrees with the recorded repository.",
                &review,
            ));
        }
        if tree.bare
            || tree.detached
            || tree.branch.as_deref() != Some(s(task, "branch"))
            || system::git(Path::new(path), &["symbolic-ref", "--quiet", "HEAD"])?
                != format!("refs/heads/{}", s(task, "branch"))
        {
            return Ok(finding(
                "checkout",
                "attention",
                "branch-mismatch",
                "The attached branch disagrees with the record or HEAD is detached.",
                &review,
            ));
        }
        if tree.locked.is_some() || tree.prunable.is_some() {
            return Ok(finding(
                "checkout",
                "attention",
                "registration-needs-review",
                "Git marks the worktree locked or prunable; no metadata was repaired or pruned.",
                &review,
            ));
        }
        Ok(finding(
            "checkout",
            "ok",
            "checkout-matches",
            "Canonical checkout, repository, registration, and attached branch agree. Work content and dependency integrity were not audited.",
            &[],
        ))
    };
    observe().unwrap_or_else(|_| finding("checkout", "unknown", "checkout-unavailable", "Git/filesystem evidence could not be read or validated; absence has not been established.", &review))
}
fn ownership(paths: &Paths, task: &Value) -> Value {
    let review = [
        "Review private project configuration and ownership records without discarding them or releasing leases. Resolve conflicts before changing resources.",
    ];
    let observe = || -> Result<Value> {
        let leases = leased(task);
        let tasks = records::tasks(&paths.state)?;
        if tasks.iter().map(Task::value).any(|other| {
            other["id"] != task["id"]
                && (other["path"] == task["path"]
                    || other["repo"] == task["repo"] && other["branch"] == task["branch"]
                    || other["socket"] == task["socket"]
                        && !task["workspaceId"].is_null()
                        && other["workspaceId"] == task["workspaceId"]
                    || leased(other).iter().any(|port| leases.contains(port)))
        }) {
            return Ok(finding(
                "ownership",
                "attention",
                "ownership-conflict",
                "Another record claims the same checkout, repository branch, workspace, or port lease.",
                &review,
            ));
        }
        let profile = profiles::configured(paths, s(task, "project"))?;
        if profile.value()["repo"] != task["repo"] {
            return Ok(finding(
                "ownership",
                "attention",
                "profile-repository-mismatch",
                "The current project profile names a different repository.",
                &review,
            ));
        }
        Ok(finding(
            "ownership",
            "ok",
            "ownership-matches",
            "No conflicting record/lease was found and the project's repository agrees. Existing recorded paths and port leases remain authoritative.",
            &[],
        ))
    };
    observe().unwrap_or_else(|_| finding("ownership", "unknown", "ownership-unavailable", "Registry or project configuration could not be read or validated; ownership is not confirmed.", &review))
}
fn setup(paths: &Paths, task: &Value) -> Value {
    let review = [
        "Review graftr status and the setup log privately. After resolving ownership/configuration issues, inspect installer/hook effects before deliberately retrying graftr setup --wait inside the owned Herdr workspace.",
    ];
    let observe = || -> Result<Value> {
        let receipt = records::setup_for(&paths.state, task["socket"].as_str(), s(task, "path"))?;
        let Some(receipt) = receipt else {
            return Ok(finding(
                "setup",
                "unknown",
                "receipt-missing",
                "There is no setup receipt for this recorded server/path; readiness cannot be confirmed.",
                &review,
            ));
        };
        let receipt = receipt.value();
        if receipt["taskId"] != task["id"]
            || receipt["project"] != task["project"]
            || receipt["workspaceId"] != task["workspaceId"]
        {
            return Ok(finding(
                "setup",
                "attention",
                "receipt-identity-mismatch",
                "The setup receipt belongs to a different task, project, or workspace attempt.",
                &review,
            ));
        }
        if receipt["status"] == "pending" {
            return Ok(finding(
                "setup",
                "unknown",
                "setup-pending",
                format!(
                    "Last recorded setup is pending at {}; this does not prove that a worker is running, stopped, or safe to replay.",
                    s(receipt, "step")
                ),
                &[
                    "Check the original Herdr setup log/active operation first. Wait for a running setup; review effects before an explicit retry. Do not bypass its lock or remove the checkout.",
                ],
            ));
        }
        if receipt["status"] != "ready" {
            return Ok(finding(
                "setup",
                "attention",
                "setup-incomplete",
                format!(
                    "Last setup is {} at {}; dependencies are {}. Diagnosis did not retry or undo command effects.",
                    s(receipt, "status"),
                    s(receipt, "step"),
                    s(receipt, "dependencies")
                ),
                &review,
            ));
        }
        if receipt["step"] != "complete"
            || [Some("pending"), Some("failed")].contains(&receipt["dependencies"].as_str())
            || receipt["hooks"]
                .as_array()
                .is_some_and(|hooks| hooks.iter().any(|hook| hook["status"] != "succeeded"))
        {
            return Ok(finding(
                "setup",
                "attention",
                "receipt-inconsistent",
                "A ready receipt contains unfinished/failed stages; it is not sufficient readiness evidence.",
                &review,
            ));
        }
        if task["status"] == "preparing" {
            return Ok(finding(
                "setup",
                "unknown",
                "task-preparing",
                "The task is still recorded preparing despite a ready receipt; the operation may be unfinished or its observations may have changed.",
                &review,
            ));
        }
        Ok(finding(
            "setup",
            "ok",
            "setup-recorded-ready",
            format!(
                "The last receipt records completed setup; dependencies are {}. This is not current environment, dependency, worker, or application health verification.",
                s(receipt, "dependencies")
            ),
            &[],
        ))
    };
    observe().unwrap_or_else(|_| {
        finding(
            "setup",
            "unknown",
            "receipt-unavailable",
            "The setup receipt could not be read or validated; its contents were not logged.",
            &review,
        )
    })
}
fn workspace(task: &Value) -> Value {
    let Some(socket) = task["socket"].as_str() else {
        return finding(
            "workspace",
            "not-checked",
            "not-attached",
            "Standalone task; no Herdr workspace attachment is required or contacted.",
            &[],
        );
    };
    let review = [
        "Review Herdr workspace identity and checkout paths manually. Do not adopt, close, or recreate a workspace to bypass an ownership conflict.",
    ];
    let observe = || -> Result<Value> {
        let spaces = herdr::workspaces(&herdr::request(
            Some(socket),
            "workspace.list",
            json!({}),
            Duration::from_secs(60),
        )?)?;
        let owned: Vec<_> = spaces
            .iter()
            .filter(|space| Some(space.workspace_id.as_str()) == task["workspaceId"].as_str())
            .collect();
        if owned.len() > 1
            || owned
                .iter()
                .any(|space| space.checkout_path.as_deref() != Some(s(task, "path")))
            || spaces.iter().any(|space| {
                space.checkout_path.as_deref() == Some(s(task, "path"))
                    && Some(space.workspace_id.as_str()) != task["workspaceId"].as_str()
            })
        {
            return Ok(finding(
                "workspace",
                "attention",
                "workspace-conflict",
                "Workspace identity/path or another workspace at this checkout conflicts with recorded ownership.",
                &review,
            ));
        }
        if task["status"] == "closed" {
            return Ok(if owned.is_empty() {
                finding(
                    "workspace",
                    "ok",
                    "workspace-closed",
                    "The task is recorded closed and its workspace is absent on the recorded server; retained files/leases are intentional.",
                    &[],
                )
            } else {
                finding(
                    "workspace",
                    "attention",
                    "closed-workspace-present",
                    "The task is recorded closed but its owned workspace is still present.",
                    &review,
                )
            });
        }
        if owned.is_empty() {
            return Ok(finding(
                "workspace",
                "attention",
                "workspace-missing",
                "The task's recorded workspace is absent on the responding Herdr server.",
                &[&format!(
                    "After resolving local findings and reviewing pending command effects, use graftr open {} from Herdr only if reopening this retained checkout is intended.",
                    s(task, "id")
                )],
            ));
        }
        Ok(finding(
            "workspace",
            "ok",
            "workspace-matches",
            "The recorded workspace ID and checkout path agree on the recorded Herdr server.",
            &[],
        ))
    };
    observe().unwrap_or_else(|_| finding("workspace", "unknown", "workspace-unavailable", "Herdr did not provide a usable workspace inventory; workspace closure has not been established.",
        &["Restore access to the recorded Herdr server and rerun doctor --live. Do not treat an outage as permission to remove resources."]))
}
fn report(record: &Value, live: bool, status: &str, complete: bool, findings: Vec<Value>) -> Value {
    json!({"id": record["id"], "project": record["project"], "path": record["path"], "branch": record["branch"], "mode": if live { "live" } else { "local" }, "status": status, "complete": complete, "findings": findings})
}
pub fn diagnose(paths: &Paths, id: &str, live: bool) -> Result<Value> {
    records::private_dir(&paths.state)?;
    let task = records::task(&paths.state, id)?;
    let task = task.value();
    let mut findings = vec![
        checkout(task),
        ownership(paths, task),
        setup(paths, task),
        if live {
            workspace(task)
        } else {
            finding(
                "workspace",
                "not-checked",
                "live-disabled",
                "Herdr was not contacted.",
                &["Use --live to inspect the recorded Herdr server."],
            )
        },
    ];
    if let Some(plan) = task.get("cleanup") {
        let status = s(plan, "status");
        let success = plan["results"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["status"] == "succeeded")
            .count();
        findings.push(finding("cleanup", if status == "failed" { "attention" } else if ["queued", "running"].contains(&status) { "unknown" } else { "ok" }, &format!("cleanup-{status}"),
            format!("Recorded cleanup is {status}; {success}/{} commands have recorded success. This is not proof a worker is active or external resources are healthy.", plan["hooks"].as_array().unwrap().len()),
            if ["failed", "running", "queued"].contains(&status) { &["Inspect the original worker and external effects before an explicit cleanup/remove retry. Keep ownership and leases; arbitrary hook commands are not automatically idempotent."] } else { &[] }));
    }
    match records::task(&paths.state, id) {
        Ok(current) if current.value() != task => findings.push(finding("ownership", "unknown", "task-changed", "The task record changed during diagnosis; observations may describe different attempts.", &["Wait for the active operation, then run doctor again. Do not act on this report as a single snapshot."])),
        Err(_) => findings.push(finding("ownership", "unknown", "task-unavailable", "The task record could not be reread after diagnosis.", &["Preserve records and leases; review concurrent operations and rerun doctor."])), _ => (),
    }
    let attention = findings.iter().any(|f| f["status"] == "attention");
    let unknown = findings.iter().any(|f| f["status"] == "unknown");
    Ok(report(
        task,
        live,
        if attention {
            "attention"
        } else if unknown {
            "unknown"
        } else {
            "ok"
        },
        !unknown,
        findings,
    ))
}

pub(crate) fn adopted_checkout(path: &str) -> Result<Value> {
    let requested = path.to_owned();
    ensure(
        canonical(&requested)? == requested,
        "Adoption requires a canonical directory, not a symlink",
    )?;
    let path = system::git(Path::new(&requested), &["rev-parse", "--show-toplevel"])?;
    ensure(canonical(&path)? == path, "Checkout path must be canonical")?;
    let trees = inventory::registered(&path)?;
    let repo = trees
        .first()
        .ok_or_else(|| Error::from("Missing Git repository"))?;
    let tree = trees
        .iter()
        .find(|tree| tree.path == path)
        .ok_or_else(|| Error::from("Missing Git worktree"))?;
    ensure(
        path != repo.path
            && !tree.bare
            && !tree.detached
            && tree.branch.is_some()
            && tree.locked.is_none()
            && tree.prunable.is_none()
            && canonical(&repo.path)? == repo.path
            && system::git(Path::new(&path), &["symbolic-ref", "--quiet", "HEAD"])?
                == format!("refs/heads/{}", tree.branch.as_deref().unwrap())
            && system::git(
                Path::new(&path),
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )? == system::git(
                Path::new(&repo.path),
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )?,
        "Adoption requires a present, unlocked linked Git worktree with an attached branch, not the main checkout",
    )?;
    Ok(json!({"repo": repo.path, "path": path, "branch": tree.branch}))
}
pub fn diagnose_adoption(paths: &Paths, record: &Adoption, live: bool) -> Result<Value> {
    let record = record.value();
    let mut findings = Vec::new();
    match adopted_checkout(s(record, "path")) {
        Ok(observed) => {
            let matches = observed["repo"] == record["repo"]
                && observed["path"] == record["path"]
                && observed["branch"] == record["branch"];
            findings.push(finding(
                "checkout",
                if matches { "ok" } else { "attention" },
                if matches {
                    "checkout-matches"
                } else {
                    "branch-mismatch"
                },
                if matches {
                    "Canonical checkout, repository and attached branch match the adoption record."
                } else {
                    "The checkout no longer matches its adopted identity."
                },
                &[],
            ));
        }
        Err(_) => findings.push(finding(
            "checkout",
            "unknown",
            "checkout-unavailable",
            "Checkout evidence could not be verified; no files or Git metadata were changed.",
            &["Review the checkout and original adoption record."],
        )),
    }
    let current: Vec<_> = records::adoptions(&paths.state)?
        .into_iter()
        .filter(|r| r.value()["id"] == record["id"] || r.value()["path"] == record["path"])
        .collect();
    let normal = records::tasks(&paths.state)?.iter().any(|r| {
        r.value()["id"] == record["id"]
            || r.value()["path"] == record["path"]
            || r.value()["repo"] == record["repo"] && r.value()["branch"] == record["branch"]
    });
    let ownership = current.len() == 1 && current[0].value() == record && !normal;
    findings.push(finding(
        "ownership",
        if ownership { "ok" } else { "attention" },
        if ownership {
            "adoption-matches"
        } else {
            "ownership-conflict"
        },
        if ownership {
            "One adoption record tracks this checkout; no provisioned task claims it."
        } else {
            "Tracking changed or conflicts with a provisioned task; review before acting."
        },
        &[],
    ));
    findings.push(finding("setup", "unknown", "setup-external", "Existing setup is externally configured, not verified or owned by Graftr. Selected stage results do not establish overall readiness.", &["Use graftr adopt PATH to explicitly select installation, environment refresh or individual setup hooks; no stage is replayed automatically."]));
    if [
        record["environment"].as_str().unwrap(),
        record["dependencies"].as_str().unwrap(),
    ]
    .into_iter()
    .chain(
        record["hooks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|hook| s(hook, "status")),
    )
    .any(|status| ["failed", "pending"].contains(&status))
    {
        findings.push(finding("setup", "attention", "adoption-stage-review", "A selected adoption stage failed or remains pending. This does not prove that its worker has stopped.", &["Inspect the worker and graftr logs before explicitly retrying a selected stage. Default adoption never replays it."]));
    }
    if live && !record["socket"].is_null() && !record["workspaceId"].is_null() {
        let inspect = || -> Result<Value> {
            let spaces = herdr::workspaces(&herdr::request(
                record["socket"].as_str(),
                "workspace.list",
                json!({}),
                Duration::from_millis(1500),
            )?)?;
            let matches: Vec<_> = spaces
                .iter()
                .filter(|space| {
                    space.workspace_id == s(record, "workspaceId")
                        || space.checkout_path.as_deref() == Some(s(record, "path"))
                })
                .collect();
            let present = matches.len() == 1
                && matches[0].workspace_id == s(record, "workspaceId")
                && matches[0].checkout_path.as_deref() == Some(s(record, "path"));
            Ok(finding(
                "workspace",
                if present { "ok" } else { "unknown" },
                if present {
                    "workspace-matches"
                } else {
                    "workspace-not-confirmed"
                },
                if present {
                    "The observed Herdr attachment still matches."
                } else {
                    "The original Herdr attachment was not confirmed; it was not reopened or rebound."
                },
                &[],
            ))
        };
        findings.push(inspect().unwrap_or_else(|_| {
            finding(
                "workspace",
                "unknown",
                "workspace-unavailable",
                "Herdr could not be inspected; no workspace was opened or changed.",
                &[],
            )
        }));
    } else {
        findings.push(finding(
            "workspace",
            "not-checked",
            "live-disabled",
            if live {
                "No Herdr attachment was recorded."
            } else {
                "Herdr was not contacted."
            },
            &[],
        ));
    }
    let status = if findings.iter().any(|f| f["status"] == "attention") {
        "attention"
    } else {
        "unknown"
    };
    Ok(report(record, live, status, false, findings))
}
pub fn format(report: &Value, columns: usize) -> String {
    let mut lines = vec![
        format!(
            "{}: {} ({}; {})",
            s(report, "id"),
            status_color(s(report, "status")),
            s(report, "mode"),
            if report["complete"] == true {
                "requested observations complete"
            } else {
                "incomplete evidence"
            }
        ),
        format!("  {} on {}", report["path"], report["branch"]),
    ];
    for finding in report["findings"].as_array().unwrap() {
        lines.extend([
            "".into(),
            format!(
                "{}: {} [{}]",
                s(finding, "area"),
                status_color(s(finding, "status")),
                s(finding, "code")
            ),
            format!("  {}", s(finding, "message")),
        ]);
        for step in finding["nextSteps"].as_array().unwrap() {
            lines.push(format!("  Next: {}", step.as_str().unwrap()));
        }
    }
    lines.extend(["".into(), "Read-only, point-in-time observations; not an atomic snapshot, repair, or application health check. No environment files or project commands were read/run.".into()]);
    wrap(&lines.join("\n"), columns)
}
