use crate::{
    records::{Adoption, Frozen, Setup, Task},
    terminal::{Row, display, format_table, heading, wrap},
    validation::date_millis,
};
use icu_collator::{Collator, CollatorBorrowed, options::CollatorOptions};
use serde_json::{Value, json};
use std::{
    cmp::Ordering,
    sync::LazyLock,
    time::{SystemTime, UNIX_EPOCH},
};

pub fn target(
    tasks: &[Task],
    adopted: &[Adoption],
    frozen: &[Frozen],
    selector: Option<&str>,
    project: Option<&str>,
) -> Result<Option<Value>, crate::targets::TargetError> {
    let mut candidates: Vec<_> = tasks
        .iter()
        .map(Task::value)
        .chain(adopted.iter().map(Adoption::value))
        .collect();
    // Freeze/thaw recovery may coexist with the active task; select the checkout
    // once while still including its matching recovery record in the report.
    let recovery_targets: Vec<_> = frozen.iter().filter(|record| record.reserves())
        .map(Frozen::value)
        .filter(|record| !candidates.iter().any(|candidate| candidate["id"] == record["id"]))
        .map(|record| json!({"id": record["id"], "project": string(record, "id").split_once("--").unwrap().0,
            "repo": record["removal"]["repo"], "path": record["removal"]["path"], "branch": record["removal"]["currentBranch"]}))
        .collect();
    candidates.extend(recovery_targets.iter());
    let selected = crate::targets::select_record(&candidates, selector, project)?;
    if selected.is_none() && (selector.is_some() || project.is_some()) {
        return Err(crate::targets::TargetError {
            code: "TARGET_NOT_FOUND",
            message:
                "Current directory is not a managed worktree; supply a target or use status --all"
                    .into(),
            details: Value::Null,
        });
    }
    Ok(selected.map(|index| candidates[index].clone()))
}

pub(crate) fn compare(left: &str, right: &str) -> Ordering {
    static COLLATOR: LazyLock<CollatorBorrowed<'static>> = LazyLock::new(|| {
        Collator::try_new(Default::default(), CollatorOptions::default())
            .expect("compiled Unicode collation data")
    });
    COLLATOR.compare(left, right)
}
fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().expect("validated record string")
}
fn task_columns(id: &str) -> Vec<String> {
    id.splitn(2, "--").map(str::to_owned).collect()
}
fn setup_label(receipt: Option<&Value>) -> String {
    let Some(receipt) = receipt else {
        return "setup unknown".into();
    };
    match string(receipt, "status") {
        "ready" => "ready".into(),
        "pending" => format!("setting up ({})", string(receipt, "step")),
        "incomplete" => format!("setup incomplete ({})", string(receipt, "step")),
        "skipped" => "setup skipped".into(),
        _ => "setup unmanaged".into(),
    }
}
fn task_label(task: &Value, receipt: Option<&Value>, multiple_receipts: bool) -> String {
    let setup = if multiple_receipts {
        "setup needs review".into()
    } else {
        setup_label(receipt)
    };
    let inactive = ["closed", "cleaned"].contains(&string(task, "status"));
    let lifecycle = match string(task, "status") {
        "closed" => "closed".into(),
        "cleaned" => match task.get("cleanup").map(|plan| string(plan, "status")) {
            Some("armed") => "cleanup pending".into(),
            Some(status) if status != "complete" => format!("cleanup {status}"),
            _ => "resources cleaned".into(),
        },
        "preparing" if setup == "ready" => "setting up".into(),
        _ => setup.clone(),
    };
    let mut labels = vec![lifecycle];
    if inactive && setup != "ready" {
        labels.push(
            if let Some(receipt) = receipt.filter(|receipt| receipt["status"] == "pending") {
                format!("setup pending ({})", string(receipt, "step"))
            } else {
                setup
            },
        );
    }
    for (key, label) in [("cleanup", "cleanup"), ("retire", "retirement")] {
        if let Some(plan) = task.get(key) {
            let status = match string(plan, "status") {
                "complete" => continue,
                "armed" if key == "cleanup" && inactive => "pending",
                "armed" => continue,
                status => status,
            };
            // A successful setup receipt must not hide unfinished resource work.
            labels.retain(|label| label != "ready");
            let issue = format!("{label} {status}");
            if !labels.contains(&issue) {
                labels.push(issue);
            }
        }
    }
    labels.join(" — ")
}
fn return_time(record: &Value) -> Option<i64> {
    record["nextAttemptAt"]
        .as_str()
        .or_else(|| record["until"].as_str())
        .and_then(date_millis)
}
fn return_label(record: &Value) -> String {
    match string(record, "status") {
        "freezing" => return "after freeze completes".into(),
        "thawing" => return "thaw in progress".into(),
        "attention" => return "manual review".into(),
        _ => (),
    }
    if record["until"].is_null() {
        return "manual thaw".into();
    }
    let at = record["nextAttemptAt"]
        .as_str()
        .or_else(|| record["until"].as_str())
        .unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let prefix = if record["attempts"].as_f64().unwrap() > 0.0 {
        "retry "
    } else if date_millis(at).unwrap() <= now {
        "due "
    } else {
        ""
    };
    format!("{prefix}{}", at[..16].replace('T', " "))
}
fn frozen_records(records: &[Frozen]) -> Vec<&Value> {
    let mut records: Vec<_> = records
        .iter()
        .filter(|r| r.reserves())
        .map(Frozen::value)
        .collect();
    records.sort_by(|a, b| {
        return_time(a)
            .unwrap_or(i64::MAX)
            .cmp(&return_time(b).unwrap_or(i64::MAX))
            .then_with(|| compare(string(a, "id"), string(b, "id")))
    });
    records
}
fn frozen_table(records: &[&Value], columns: usize, indent: usize) -> String {
    let rows = records
        .iter()
        .map(|record| {
            let mut cells = task_columns(string(record, "id"));
            cells.extend([string(record, "status").into(), return_label(record)]);
            let mut details = Vec::new();
            if let Some(note) = record["note"].as_str() {
                details.push(format!("Note: {note}"));
            }
            details.push(string(record, "message").into());
            Row { cells, details }
        })
        .collect::<Vec<_>>();
    format_table(
        &["PROJECT", "TASK", "STATUS", "RETURN (UTC)"],
        &rows,
        columns,
        indent,
        &[],
    )
}
pub fn frozen(records: &[Frozen], columns: usize) -> String {
    let records = frozen_records(records);
    if records.is_empty() {
        return "No frozen tasks.".into();
    }
    wrap(
        &format!(
            "{}\n{}",
            heading(&format!("Frozen / recovery ({})", records.len())),
            frozen_table(&records, columns, 2)
        ),
        columns,
    )
}
pub fn format(
    tasks: &[Task],
    setups: &[Setup],
    adopted: &[Adoption],
    frozen: &[Frozen],
    columns: usize,
    incomplete: bool,
    target: Option<&Value>,
) -> String {
    let records = frozen_records(frozen);
    let mut matched = vec![false; setups.len()];
    let mut sections = Vec::new();
    if let Some(target) = target {
        sections.push(format!(
            "{}\n  Branch: {}\n  Path: {}",
            heading(string(target, "id")),
            display(string(target, "branch")),
            display(string(target, "path"))
        ));
    }
    if !tasks.is_empty() || !adopted.is_empty() {
        let mut rows = Vec::new();
        for task in tasks {
            let task = task.value();
            let receipts: Vec<_> = setups
                .iter()
                .enumerate()
                .filter(|(_, r)| {
                    r.value()["taskId"] == task["id"] && r.value()["path"] == task["path"]
                })
                .collect();
            let receipt = if receipts.len() == 1 {
                matched[receipts[0].0] = true;
                Some(receipts[0].1.value())
            } else {
                None
            };
            let mut details = Vec::new();
            if receipts.len() > 1 {
                details.push("Multiple setup receipts need review; no single setup result can be established.".into());
            }
            if let Some(receipt) = receipt
                && ["incomplete", "unmanaged"].contains(&string(receipt, "status"))
            {
                details.push(format!("Setup: {}", string(receipt, "message")));
            }
            for (label, key) in [("Cleanup", "cleanup"), ("Retirement", "retire")] {
                if let Some(plan) = task.get(key)
                    && !["armed", "complete"].contains(&string(plan, "status"))
                {
                    details.push(format!(
                        "{label} {}; inspect graftr logs {} before retrying.",
                        string(plan, "status"),
                        string(task, "id")
                    ));
                }
            }
            let mut cells = task_columns(string(task, "id"));
            cells.extend([
                task_label(task, receipt, receipts.len() > 1),
                task["url"].as_str().map(str::to_owned).unwrap_or_else(|| {
                    format!("localhost:{}", task["localPort"].as_f64().unwrap())
                }),
            ]);
            rows.push(Row { cells, details });
        }
        for record in adopted {
            let record = record.value();
            let attention = [
                record["environment"].as_str().unwrap(),
                record["dependencies"].as_str().unwrap(),
            ]
            .into_iter()
            .chain(
                record["hooks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|hook| string(hook, "status")),
            )
            .any(|stage| ["failed", "pending"].contains(&stage));
            let mut cells = task_columns(string(record, "id"));
            cells.extend([
                if attention {
                    "adopted — needs review".into()
                } else {
                    "adopted — external setup".into()
                },
                record["url"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        record["localPort"]
                            .as_f64()
                            .map(|port| format!("localhost:{port}"))
                            .unwrap_or_else(|| "not recorded".into())
                    }),
            ]);
            rows.push(Row { cells, details: if attention { vec![format!("A selected adoption stage failed or remains pending; inspect graftr logs {} before explicitly retrying.", string(record, "id"))] } else { vec![] } });
        }
        rows.sort_by(|a, b| {
            compare(&a.cells[0], &b.cells[0]).then_with(|| compare(&a.cells[1], &b.cells[1]))
        });
        sections.push(format_table(
            &["PROJECT", "TASK", "STATUS", "ADDRESS"],
            &rows,
            columns,
            0,
            &["PROJECT", "TASK", "ADDRESS"],
        ));
    }
    if !records.is_empty() {
        sections.push(format!(
            "{}\n{}",
            heading(&format!("Frozen / recovery ({})", records.len())),
            frozen_table(&records, columns, 0)
        ));
    }
    if tasks.is_empty() && adopted.is_empty() && records.is_empty() {
        sections.push(
            if incomplete {
                "No validated worktrees or frozen tasks to display."
            } else {
                "No managed worktrees or frozen tasks."
            }
            .into(),
        );
    }
    let mut others: Vec<_> = setups
        .iter()
        .enumerate()
        .filter(|(index, _)| !matched[*index])
        .map(|(_, setup)| setup.value())
        .collect();
    others.sort_by(|a, b| compare(string(a, "path"), string(b, "path")));
    if !others.is_empty() {
        let rows = others
            .iter()
            .map(|receipt| Row {
                cells: vec![
                    string(receipt, "path").into(),
                    if receipt["status"] == "ready" {
                        "setup completed".into()
                    } else {
                        setup_label(Some(receipt))
                    },
                ],
                details: if ["incomplete", "unmanaged"].contains(&string(receipt, "status")) {
                    vec![string(receipt, "message").into()]
                } else {
                    vec![]
                },
            })
            .collect::<Vec<_>>();
        sections.push(format!(
            "{}\n{}",
            heading(&format!("Other setup records ({})", rows.len())),
            format_table(&["CHECKOUT", "STATUS"], &rows, columns, 0, &[])
        ));
    }
    sections.push(
        if target.is_some() {
            "Use --all for all worktrees; --json for full details."
        } else {
            "Use --json for full details."
        }
        .into(),
    );
    wrap(&sections.join("\n\n"), columns)
}
pub fn data(
    tasks: &[Task],
    setups: &[Setup],
    adopted: &[Adoption],
    frozen: &[Frozen],
    retained: &[crate::history::Summary],
) -> Value {
    json!({"schemaVersion": 2, "tasks": tasks.iter().map(Task::public).collect::<Vec<_>>(), "setups": setups.iter().map(Setup::value).collect::<Vec<_>>(),
        "retained": retained, "frozen": frozen.iter().filter(|r| r.reserves()).map(Frozen::public).collect::<Vec<_>>(), "adopted": adopted.iter().map(Adoption::value).collect::<Vec<_>>()})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt(status: &str) -> Value {
        json!({"status": status, "step": "dependencies"})
    }

    #[test]
    fn combined_status_preserves_lifecycle_and_setup_problems() {
        for (lifecycle, setup, expected) in [
            ("ready", Some("ready"), "ready"),
            ("preparing", Some("ready"), "setting up"),
            ("preparing", Some("pending"), "setting up (dependencies)"),
            (
                "ready",
                Some("incomplete"),
                "setup incomplete (dependencies)",
            ),
            (
                "preparing",
                Some("incomplete"),
                "setup incomplete (dependencies)",
            ),
            ("ready", Some("skipped"), "setup skipped"),
            ("ready", Some("unmanaged"), "setup unmanaged"),
            ("ready", None, "setup unknown"),
            ("preparing", None, "setup unknown"),
            ("closed", Some("ready"), "closed"),
            (
                "closed",
                Some("pending"),
                "closed — setup pending (dependencies)",
            ),
            (
                "closed",
                Some("incomplete"),
                "closed — setup incomplete (dependencies)",
            ),
            ("closed", Some("skipped"), "closed — setup skipped"),
            ("closed", Some("unmanaged"), "closed — setup unmanaged"),
            ("closed", None, "closed — setup unknown"),
            ("cleaned", Some("ready"), "resources cleaned"),
            (
                "cleaned",
                Some("incomplete"),
                "resources cleaned — setup incomplete (dependencies)",
            ),
            ("cleaned", None, "resources cleaned — setup unknown"),
        ] {
            let task = json!({"status": lifecycle});
            let receipt = setup.map(receipt);
            assert_eq!(task_label(&task, receipt.as_ref(), false), expected);
        }
    }

    #[test]
    fn uncertain_receipts_never_show_ready() {
        for (lifecycle, expected) in [
            ("ready", "setup needs review"),
            ("preparing", "setup needs review"),
            ("closed", "closed — setup needs review"),
            ("cleaned", "resources cleaned — setup needs review"),
        ] {
            assert_eq!(
                task_label(&json!({"status": lifecycle}), None, true),
                expected
            );
        }
    }

    #[test]
    fn unfinished_resource_work_is_visible_even_with_a_successful_setup_receipt() {
        let ready_receipt = receipt("ready");
        for (lifecycle, plan, expected) in [
            ("ready", "armed", "ready"),
            ("ready", "complete", "ready"),
            ("ready", "queued", "cleanup queued"),
            ("ready", "running", "cleanup running"),
            ("ready", "failed", "cleanup failed"),
            ("closed", "armed", "closed — cleanup pending"),
            ("closed", "queued", "closed — cleanup queued"),
            ("closed", "running", "closed — cleanup running"),
            ("closed", "failed", "closed — cleanup failed"),
            ("closed", "complete", "closed"),
            ("cleaned", "armed", "cleanup pending"),
            ("cleaned", "queued", "cleanup queued"),
            ("cleaned", "running", "cleanup running"),
            ("cleaned", "failed", "cleanup failed"),
            ("cleaned", "complete", "resources cleaned"),
        ] {
            let task = json!({"status": lifecycle, "cleanup": {"status": plan}});
            assert_eq!(task_label(&task, Some(&ready_receipt), false), expected);
        }
        let task = json!({"status": "ready", "cleanup": {"status":"failed"}, "retire": {"status":"running"}});
        assert_eq!(
            task_label(&task, Some(&ready_receipt), false),
            "cleanup failed — retirement running"
        );
        assert_eq!(
            task_label(&task, Some(&receipt("incomplete")), false),
            "setup incomplete (dependencies) — cleanup failed — retirement running"
        );
    }
}
