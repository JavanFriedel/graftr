use crate::terminal::{heading, table, wrap};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use std::{fs, os::unix::fs::MetadataExt, path::Path, time::SystemTime};

pub(crate) const DIRECTORIES: [(&str, &str); 9] = [
    ("removed", "removed"),
    ("timings", "timings"),
    ("env-backups", "env-backups"),
    ("migration-backups", "migration-backups"),
    ("legacy-backups", "backups"),
    ("creating", "creating"),
    ("hook-work", "hook-work"),
    ("tailscale-hooks", "tailscale-hooks"),
    ("postgres-hooks", "postgres-hooks"),
];

fn metadata(path: &Path) -> Result<Option<fs::Metadata>, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn private_directory(metadata: &fs::Metadata) -> Result<(), String> {
    // getuid has no preconditions and does not change process identity.
    let uid = unsafe { libc::getuid() };
    if !metadata.is_dir() || metadata.mode() & 0o077 != 0 || metadata.uid() != uid {
        return Err("State directory must be private, owned by you, and not a symlink".into());
    }
    Ok(())
}

#[derive(Serialize)]
pub struct Summary {
    category: &'static str,
    count: u64,
    bytes: u64,
    oldest: Option<String>,
}

#[derive(Default)]
struct Totals {
    count: u64,
    bytes: u64,
    oldest: Option<SystemTime>,
}

pub(crate) fn tree(path: &Path, metadata: &fs::Metadata) -> Result<(u64, SystemTime), String> {
    let mut bytes = 0;
    let mut modified = metadata.modified().map_err(|e| e.to_string())?;
    for item in fs::read_dir(path).map_err(|e| e.to_string())? {
        let item = item.map_err(|e| e.to_string())?;
        let metadata = fs::symlink_metadata(item.path()).map_err(|e| e.to_string())?;
        let (size, time) = if metadata.is_dir() {
            tree(&item.path(), &metadata)?
        } else {
            // Match retained history: nested symlinks contribute link metadata, not target data.
            (
                metadata.len(),
                metadata.modified().map_err(|e| e.to_string())?,
            )
        };
        bytes += size;
        modified = modified.max(time);
    }
    Ok((bytes, modified))
}

fn children(path: &Path, totals: &mut Totals) -> Result<(), String> {
    let Some(metadata) = metadata(path)? else {
        return Ok(());
    };
    private_directory(&metadata)?;
    for item in fs::read_dir(path).map_err(|e| e.to_string())? {
        let item = item.map_err(|e| e.to_string())?;
        let metadata = fs::symlink_metadata(item.path()).map_err(|e| e.to_string())?;
        if !metadata.is_file() && !metadata.is_dir() {
            continue;
        }
        let (size, modified) = if metadata.is_dir() {
            tree(&item.path(), &metadata)?
        } else {
            (
                metadata.len(),
                metadata.modified().map_err(|e| e.to_string())?,
            )
        };
        totals.count += 1;
        totals.bytes += size;
        totals.oldest = Some(
            totals
                .oldest
                .map_or(modified, |oldest| oldest.min(modified)),
        );
    }
    Ok(())
}

fn summary(category: &'static str, totals: Totals) -> Summary {
    Summary {
        category,
        count: totals.count,
        bytes: totals.bytes,
        oldest: totals
            .oldest
            .map(|time| date(time).to_rfc3339_opts(SecondsFormat::Millis, true)),
    }
}
pub(crate) fn date(time: SystemTime) -> DateTime<Utc> {
    let date = DateTime::<Utc>::from(time);
    // Stats exposes a double millisecond value before Date truncates it. Near a
    // millisecond boundary, exact nanosecond truncation can differ by one millisecond.
    let millis =
        date.timestamp() as f64 * 1000.0 + date.timestamp_subsec_nanos() as f64 / 1_000_000.0;
    DateTime::from_timestamp_millis(millis.trunc() as i64)
        .expect("filesystem timestamp in supported range")
}

pub fn read(state: &Path) -> Result<Vec<Summary>, String> {
    let mut result = Vec::new();
    for (category, directory) in DIRECTORIES {
        let mut totals = Totals::default();
        children(&state.join(directory), &mut totals)?;
        result.push(summary(category, totals));
    }
    let mut totals = Totals::default();
    let logs = state.join("logs");
    if let Some(metadata) = metadata(&logs)? {
        private_directory(&metadata)?;
        for task in fs::read_dir(&logs).map_err(|e| e.to_string())? {
            let task = task.map_err(|e| e.to_string())?;
            if fs::symlink_metadata(task.path())
                .map_err(|e| e.to_string())?
                .is_dir()
            {
                children(&task.path(), &mut totals)?;
            }
        }
    }
    result.push(summary("logs", totals));
    Ok(result)
}

fn size(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let exponent = ((bytes as f64).log(1024.0).floor() as usize).min(3);
    // JS toFixed rounds positive exact ties up; Rust's formatter uses ties-to-even.
    // Byte counts divided by powers of 1024 are binary-exact, so round in integers.
    let divisor = 1024_u128.pow(exponent as u32);
    let tenths = (u128::from(bytes) * 10 + divisor / 2) / divisor;
    let formatted = format!("{}.{}", tenths / 10, tenths % 10);
    format!(
        "{} {}",
        formatted.strip_suffix(".0").unwrap_or(&formatted),
        ["B", "KiB", "MiB", "GiB"][exponent]
    )
}

pub fn format(retained: &[Summary], columns: usize) -> String {
    let count: u64 = retained.iter().map(|item| item.count).sum();
    if count == 0 {
        return wrap(
            &format!("{}\n\nNo retained history.", heading("History")),
            columns,
        );
    }
    let bytes = retained.iter().map(|item| item.bytes).sum();
    let rows = retained
        .iter()
        .filter(|item| item.count > 0)
        .map(|item| {
            let label = match item.category {
                "removed" => "Removed task records",
                "timings" => "Timing history",
                "logs" => "Run logs",
                "env-backups" => "Environment backups",
                "migration-backups" => "Migration backups",
                "legacy-backups" => "Legacy backups",
                "creating" => "Creation intents",
                "hook-work" => "Cleanup scratch files",
                "tailscale-hooks" => "Tailscale hook records",
                "postgres-hooks" => "Postgres hook records",
                _ => unreachable!(),
            };
            vec![
                label.into(),
                item.count.to_string(),
                size(item.bytes),
                item.oldest
                    .as_ref()
                    .map(|date| date[..16].replace('T', " "))
                    .unwrap_or_else(|| "unknown".into()),
            ]
        })
        .collect::<Vec<_>>();
    wrap(
        &format!(
            "{}\n\n{}\n\nTotal size: {}. Run graftr prune to review what can be removed.",
            heading(&format!(
                "History ({count} {})",
                if count == 1 { "entry" } else { "entries" }
            )),
            table(
                &["CATEGORY", "ENTRIES", "SIZE", "OLDEST (UTC)"],
                &rows,
                columns
            ),
            size(bytes)
        ),
        columns,
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn size_matches_positive_javascript_rounding_ties() {
        assert_eq!(super::size(1280), "1.3 KiB");
        assert_eq!(super::size(1792), "1.8 KiB");
        assert_eq!(super::size(1024), "1 KiB");
        assert_eq!(super::size(1), "1 B");
    }
}
