use crate::{
    paths, records,
    status::compare,
    terminal::{Row, format_table},
    validation::{pattern, *},
};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{Seek, SeekFrom, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

pub struct RunLog {
    pub file: File,
    pub path: PathBuf,
}
impl RunLog {
    pub fn finish(&mut self, summary: &str) -> Result<()> {
        let size = self
            .file
            .metadata()
            .map_err(|_| Error::from("Could not inspect private command log"))?
            .len();
        const LIMIT: u64 = 4 * 1024 * 1024;
        if size > LIMIT {
            self.file
                .set_len(LIMIT)
                .map_err(|_| Error::from("Could not bound private command log"))?;
        }
        self.file
            .seek(SeekFrom::End(0))
            .map_err(|_| Error::from("Could not finish private command log"))?;
        write!(
            self.file,
            "{}\n# {summary}\n",
            if size > LIMIT {
                format!("\n# output truncated at {LIMIT} bytes")
            } else {
                String::new()
            }
        )
        .map_err(|_| Error::from("Could not finish private command log"))
    }
}
pub fn open(paths: &paths::Paths, task_id: &str, label: &str, header: &str) -> Result<RunLog> {
    id(task_id)?;
    let root = paths.state.join("logs");
    crate::locks::private_directory(&root)?;
    let directory = root.join(task_id);
    crate::locks::private_directory(&directory)?;
    let stamp = crate::health::timestamp().replace(['-', ':', '.'], "");
    let label = pattern!(r"[^a-z0-9]+")
        .replace_all(&label.to_lowercase(), "-")
        .trim_matches('-')
        .to_owned();
    let path = directory.join(format!(
        "{stamp}-{label}-{}.log",
        &crate::system::uuid()?[..6]
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|_| Error::from("Could not create private command log"))?;
    writeln!(
        file,
        "# graftr {header}\n# started {}",
        crate::health::timestamp()
    )
    .map_err(|_| Error::from("Could not write private log header"))?;
    let mut names = Vec::new();
    for entry in
        fs::read_dir(&directory).map_err(|_| Error::from("Could not read private log directory"))?
    {
        let entry = entry.map_err(|_| Error::from("Could not read private log directory"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if pattern!(r"^[0-9]{8}T[0-9]{9}Z-[a-z0-9]+(?:-[a-z0-9]+)*-[0-9a-f]{6}\.log$")
            .is_match(&name)
        {
            names.push(name);
        }
    }
    names.sort();
    for name in names.iter().take(names.len().saturating_sub(50)) {
        fs::remove_file(directory.join(name))
            .map_err(|_| Error::from("Could not prune old private command log"))?;
    }
    Ok(RunLog { file, path })
}

pub fn list(state: &Path, id: &str) -> Result<Vec<Value>> {
    let directory = state.join("logs").join(id);
    if !paths::exists(&directory)? {
        return Ok(vec![]);
    }
    records::private_dir(&directory)?;
    let mut logs = Vec::new();
    for item in fs::read_dir(&directory).map_err(|_| Error::from("Could not read log directory"))? {
        let item = item.map_err(|_| Error::from("Could not read log directory"))?;
        let file = item.file_name().to_string_lossy().into_owned();
        let Some(captures) =
            pattern!(r"^([0-9]{8}T[0-9]{9}Z)-([a-z0-9]+(?:-[a-z0-9]+)*)-[0-9a-f]{6}\.log$")
                .captures(&file)
        else {
            continue;
        };
        let metadata = fs::symlink_metadata(item.path())
            .map_err(|_| Error::from("Could not inspect log metadata"))?;
        if !metadata.is_file() {
            continue;
        }
        let stamp = &captures[1];
        let started = format!(
            "{}-{}-{}T{}:{}:{}.{}Z",
            &stamp[..4],
            &stamp[4..6],
            &stamp[6..8],
            &stamp[9..11],
            &stamp[11..13],
            &stamp[13..15],
            &stamp[15..18]
        );
        logs.push(json!({"file": file, "path": item.path().to_string_lossy(), "label": captures[2].to_string(), "startedAt": started, "bytes": metadata.len()}));
    }
    logs.sort_by(|a, b| compare(b["file"].as_str().unwrap(), a["file"].as_str().unwrap()));
    Ok(logs)
}
pub fn latest(path: &Path) -> Result<String> {
    Ok(String::from_utf8_lossy(&records::read_private(path)?).into_owned())
}
pub fn format(id: &str, logs: &[Value], columns: usize) -> String {
    if logs.is_empty() {
        return format!(
            "{}: no installer or hook run logs yet",
            crate::terminal::display(id)
        );
    }
    let rows: Vec<_> = logs
        .iter()
        .map(|log| Row {
            cells: vec![
                log["startedAt"].as_str().unwrap().into(),
                log["label"].as_str().unwrap().into(),
                format!("{} B", log["bytes"]),
                log["path"].as_str().unwrap().into(),
            ],
            details: vec![],
        })
        .collect();
    format_table(&["STARTED", "RUN", "SIZE", "FILE"], &rows, columns, 2, &[])
}
