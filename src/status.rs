use crate::{
    records::{Adoption, Frozen, Setup, Task},
    terminal::{Row, format_table, heading, wrap},
    validation::date_millis,
};
use icu_collator::{Collator, CollatorBorrowed, options::CollatorOptions};
use serde_json::{Value, json};
use std::{
    cmp::Ordering,
    sync::LazyLock,
    time::{SystemTime, UNIX_EPOCH},
};

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
        return "not recorded".into();
    };
    match string(receipt, "status") {
        "ready" => "completed".into(),
        "pending" | "incomplete" => format!(
            "{} ({})",
            string(receipt, "status"),
            string(receipt, "step")
        ),
        status => status.into(),
    }
}
fn task_label(task: &Value) -> String {
    match string(task, "status") {
        "ready" => "prepared".into(),
        "cleaned" => match task.get("cleanup").map(|plan| string(plan, "status")) {
            Some("armed") => "cleanup pending".into(),
            Some(status) if status != "complete" => format!("cleanup {status}"),
            _ => "resources cleaned".into(),
        },
        status => status.into(),
    }
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
        &["PROJECT", "TASK", "STATE", "RETURN (UTC)"],
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
) -> String {
    let records = frozen_records(frozen);
    let mut matched = vec![false; setups.len()];
    let mut sections = Vec::new();
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
                task_label(task),
                if receipts.len() > 1 {
                    "needs review".into()
                } else {
                    setup_label(receipt)
                },
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
                "adopted".into(),
                if attention {
                    "needs review".into()
                } else {
                    "external".into()
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
            &["PROJECT", "TASK", "STATE", "SETUP", "ADDRESS"],
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
                cells: vec![string(receipt, "path").into(), setup_label(Some(receipt))],
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
            format_table(&["CHECKOUT", "SETUP"], &rows, columns, 0, &[])
        ));
    }
    sections.push("Use --json for full details.".into());
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
