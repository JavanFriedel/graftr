use crate::{
    checkout::string,
    envdoc, environment, locks,
    paths::{self, Paths},
    profiles, records, stateio,
    validation::*,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

struct Source {
    full: url::Url,
    bare: url::Url,
    password: Option<String>,
    host: String,
    port: String,
    user: String,
    database: String,
}
fn decode(value: &str) -> Result<String> {
    let mut bytes = Vec::new();
    let mut offset = 0;
    let raw = value.as_bytes();
    while offset < raw.len() {
        if raw[offset] == b'%' {
            let hex = raw
                .get(offset + 1..offset + 3)
                .and_then(|s| std::str::from_utf8(s).ok())
                .and_then(|s| u8::from_str_radix(s, 16).ok())
                .ok_or_else(|| Error::from("Invalid URI encoding; contents were not logged"))?;
            bytes.push(hex);
            offset += 3;
        } else {
            bytes.push(raw[offset]);
            offset += 1;
        }
    }
    String::from_utf8(bytes).map_err(|_| "Invalid URI encoding; contents were not logged".into())
}
fn encode(value: &str) -> String {
    value
        .as_bytes()
        .iter()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(b) {
                (*b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
fn source(bytes: &[u8]) -> Result<Source> {
    let values = envdoc::loose(bytes);
    let value = values
        .get("DATABASE_URL")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::from("The envSource baseline does not define DATABASE_URL"))?;
    let full = url::Url::parse(value)
        .map_err(|_| Error::from("Invalid database URL; contents were not logged"))?;
    ensure(
        ["postgres", "postgresql"].contains(&full.scheme()),
        "DATABASE_URL must be a postgres:// or postgresql:// URL",
    )?;
    let database = decode(full.path().strip_prefix('/').unwrap_or(full.path()))?;
    ensure(
        !database.is_empty() && !database.contains('/'),
        "DATABASE_URL must name a database",
    )?;
    let password = full
        .password()
        .filter(|p| !p.is_empty())
        .map(decode)
        .transpose()?;
    let mut bare = full.clone();
    bare.set_password(None)
        .map_err(|_| Error::from("Invalid database URL; contents were not logged"))?;
    let host = full.host_str().unwrap_or("").into();
    let port = full.port().map_or("5432".into(), |p| p.to_string());
    let user = decode(full.username())?;
    Ok(Source {
        full,
        bare,
        password,
        host,
        port,
        user,
        database,
    })
}
fn database(source: &str, task: &Value) -> String {
    let name = format!("{source}_{}", environment::task_slug(task));
    if name.len() <= 63 {
        name
    } else {
        format!(
            "{}_{}",
            String::from_utf16_lossy(&name.encode_utf16().take(54).collect::<Vec<_>>()),
            &format!("{:x}", Sha256::digest(name.as_bytes()))[..8]
        )
    }
}
fn with_database(url: &url::Url, database: &str) -> String {
    let mut url = url.clone();
    url.set_path(&format!("/{}", encode(database)));
    url.to_string()
}
fn command(executable: &str, source: &Source) -> Command {
    let mut command = Command::new(executable);
    for (name, _) in env::vars_os() {
        if name.to_string_lossy().starts_with("PG") {
            command.env_remove(name);
        }
    }
    if let Some(password) = &source.password {
        command.env("PGPASSWORD", password);
    }
    // Inherit the hook's process group so its deadline fences database clients too.
    command.stderr(Stdio::null());
    command
}
fn psql(source: &Source, sql: &str, database: &str) -> Result<String> {
    let mut child = command("psql", source)
        .args([
            "-X",
            "-q",
            "-At",
            "-v",
            "ON_ERROR_STOP=1",
            "-v",
            &format!("name={database}"),
            "--dbname",
            source.bare.as_str(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|_| Error::from("psql could not start; output was not logged"))?;
    let mut input = child.stdin.take().unwrap();
    let script = format!("{sql}\n");
    let writer = std::thread::spawn(move || input.write_all(script.as_bytes()));
    let mut stdout = child.stdout.take().unwrap();
    let mut bytes = Vec::new();
    let read = stdout.read_to_end(&mut bytes);
    let status = child
        .wait()
        .map_err(|_| Error::from("Could not reap owned psql client"))?;
    let _ = writer.join();
    use std::os::unix::process::ExitStatusExt;
    let code = status.code().unwrap_or(128 + status.signal().unwrap_or(0));
    ensure(
        code == 0,
        format!(
            "psql failed with exit {code}; output was not logged (a database in use is never dropped)"
        ),
    )?;
    read.map_err(|_| Error::from("Could not read psql result; output was not logged"))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
fn exists(source: &Source, database: &str) -> Result<bool> {
    Ok(psql(
        source,
        "SELECT 1 FROM pg_database WHERE datname = :'name';",
        database,
    )?
    .trim_matches(js_space)
        == "1")
}
fn copy(source: &Source, database: &str) -> Result<()> {
    let mut dump = command("pg_dump", source)
        .args([
            "--no-owner",
            "--no-privileges",
            "--dbname",
            source.bare.as_str(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|_| Error::from("pg_dump could not start; output was not logged"))?;
    let input = dump.stdout.take().unwrap();
    let restore = command("psql", source)
        .args([
            "-X",
            "-q",
            "-v",
            "ON_ERROR_STOP=1",
            "--dbname",
            &with_database(&source.bare, database),
        ])
        .stdin(Stdio::from(input))
        .stdout(Stdio::null())
        .spawn();
    let mut restore = match restore {
        Ok(child) => child,
        Err(_) => {
            let _ = dump.kill();
            let _ = dump.wait();
            return Err("psql restore could not start; output was not logged".into());
        }
    };
    let dumped = dump.wait();
    let restored = restore.wait();
    use std::os::unix::process::ExitStatusExt;
    let code = |status: std::io::Result<std::process::ExitStatus>| {
        status
            .map(|s| s.code().unwrap_or(128 + s.signal().unwrap_or(0)))
            .map_err(|_| Error::from("Could not reap owned database copy client"))
    };
    let dumped = code(dumped)?;
    let restored = code(restored)?;
    ensure(
        dumped == 0 && restored == 0,
        format!(
            "Copying the development database failed (pg_dump exit {dumped}, psql exit {restored}); output was not logged"
        ),
    )
}
fn record_for(task: &Value, source: &Source) -> Result<Value> {
    let database = database(&source.database, task);
    ensure(
        database != source.database,
        "The task database name must differ from the development database",
    )?;
    Ok(
        json!({"schemaVersion": 1, "kind": "postgres-database", "id": task["id"], "host": source.host, "port": source.port, "user": source.user, "source": source.database, "database": database, "state": "creating"}),
    )
}
fn identity(record: &Value) -> Value {
    json!([
        record["id"],
        record["host"],
        record["port"],
        record["user"],
        record["source"],
        record["database"]
    ])
}
fn directory(paths: &Paths, id: &str, run: &str) -> Result<PathBuf> {
    records::private_dir(&paths.state)?;
    let root = paths.state.join("postgres-hooks");
    locks::private_directory(&root)?;
    let key = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&json!([id, run])).unwrap())
    );
    let directory = root.join(key);
    locks::private_directory(&directory)?;
    Ok(directory)
}
fn read(directory: &Path, id: &str) -> Result<Option<Value>> {
    let file = directory.join(format!("{id}.json"));
    if !paths::exists(&file)? {
        return Ok(None);
    }
    let value: Value = serde_json::from_slice(&records::read_private(&file)?).map_err(|_| Error {code: "INVALID_JSON", message: "Invalid JSON; review configuration/state or the Herdr response (contents were not logged)".into()})?;
    ensure(
        value["schemaVersion"] == 1
            && value["kind"] == "postgres-database"
            && text(&value["id"])? == id
            && [
                Some("creating"),
                Some("created"),
                Some("copied"),
                Some("dropped"),
            ]
            .contains(&value["state"].as_str()),
        "Unsupported Postgres database receipt",
    )?;
    let mut record = json!({"schemaVersion": 1, "kind": "postgres-database", "id": id});
    for key in ["host", "port", "user", "source", "database", "state"] {
        record[key] = json!(text(&value[key])?);
    }
    Ok(Some(record))
}
fn write(directory: &Path, record: &Value) -> Result<()> {
    stateio::atomic_write(
        &directory.join(format!("{}.json", string(record, "id"))),
        format!("{}\n", serde_json::to_string_pretty(record).unwrap()).as_bytes(),
        false,
    )
}
fn point_environment(task: &Value, source: &Source, database: &str) -> Result<()> {
    let overrides = task["overrides"].as_object().unwrap();
    ensure(
        !overrides.contains_key("DATABASE_URL") && !overrides.contains_key("POSTGRES_DB"),
        "envOverrides must not manage DATABASE_URL or POSTGRES_DB when the Postgres hook owns them",
    )?;
    let path = Path::new(string(task, "path")).join(".env");
    let current = records::read_private(&path)?;
    let current = String::from_utf8_lossy(&current);
    let url = with_database(&source.full, database);
    let values = envdoc::loose(current.as_bytes());
    if values.get("DATABASE_URL") == Some(&url)
        && values.get("POSTGRES_DB").map(String::as_str) == Some(database)
    {
        return Ok(());
    }
    ensure(
        !url.chars()
            .chain(database.chars())
            .any(|c| ['\'', '\n', '\r'].contains(&c)),
        "The task database URL cannot be written as a single-quoted dotenv value",
    )?;
    let separator = if !current.is_empty() && !current.ends_with('\n') {
        "\n"
    } else {
        ""
    };
    stateio::atomic_write(&path, format!("{current}{separator}# graftr hook postgres: this task's own copy of the development database\nDATABASE_URL='{url}'\nPOSTGRES_DB='{database}'\n").as_bytes(), false)
}
pub fn hook(paths: &Paths) -> Result<()> {
    let phase = env::var("GRAFTR_HOOK_PHASE").unwrap_or_default();
    ensure(
        ["afterEnvironment", "retire"].contains(&phase.as_str()),
        "The Postgres hook runs in afterEnvironment (create and copy) and retire (drop)",
    )?;
    let task = records::task(
        &paths.state,
        &env::var("GRAFTR_TASK_ID").unwrap_or_default(),
    )?;
    let task = task.value();
    ensure(
        task["path"].as_str() == env::var("GRAFTR_WORKTREE_PATH").ok().as_deref()
            && task.get("retire").is_some()
            && task["retire"]["runId"].as_str() == env::var("GRAFTR_RETIRE_RUN_ID").ok().as_deref(),
        "Postgres hook context no longer owns this task",
    )?;
    ensure(
        task["retire"]["hooks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|hook| {
                hook["command"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .rev()
                    .take(2)
                    .map(|v| v.as_str().unwrap())
                    .collect::<Vec<_>>()
                    == ["postgres", "hook"]
            }),
        "Add graftr hook postgres to hooks.retire as well, so the task's database is dropped when the task is removed",
    )?;
    let profile = profiles::configured(paths, string(task, "project"))?;
    let baseline = profile.value()["envSource"].as_str().ok_or_else(|| {
        Error::from("The Postgres hook needs an envSource baseline that defines DATABASE_URL")
    })?;
    let source =
        source(&fs::read(baseline).map_err(|_| {
            Error::from("Could not read database baseline; contents were not logged")
        })?)?;
    let directory = directory(paths, string(task, "id"), string(&task["retire"], "runId"))?;
    let scoped = Paths {
        config: paths.config.clone(),
        state: directory.clone(),
    };
    let _lock = locks::state(&scoped)?;
    let owned = read(&directory, string(task, "id"))?;
    let desired = record_for(task, &source)?;
    ensure(
        !owned
            .as_ref()
            .is_some_and(|owned| identity(owned) != identity(&desired)),
        "The baseline's database server, user or name no longer matches the task's recorded database; preserving it for review",
    )?;
    if phase == "retire" {
        let Some(mut owned) = owned.filter(|r| r["state"] != "dropped") else {
            return Ok(());
        };
        if exists(&source, string(&owned, "database"))? {
            psql(
                &source,
                "DROP DATABASE :\"name\";",
                string(&owned, "database"),
            )?;
        }
        owned["state"] = json!("dropped");
        return write(&directory, &owned);
    }
    ensure(
        !owned.as_ref().is_some_and(|r| r["state"] == "dropped"),
        "This task's database was already dropped; create a new task for a fresh copy",
    )?;
    let mut owned = if let Some(owned) = owned {
        if owned["state"] != "copied" && exists(&source, string(&owned, "database"))? {
            psql(
                &source,
                "DROP DATABASE :\"name\";",
                string(&owned, "database"),
            )?;
        }
        owned
    } else {
        ensure(
            !exists(&source, string(&desired, "database"))?,
            format!(
                "Database {} already exists; refusing to adopt or overwrite it",
                string(&desired, "database")
            ),
        )?;
        desired
    };
    if owned["state"] != "copied" {
        owned["state"] = json!("creating");
        write(&directory, &owned)?;
        psql(
            &source,
            "CREATE DATABASE :\"name\";",
            string(&owned, "database"),
        )?;
        owned["state"] = json!("created");
        write(&directory, &owned)?;
        copy(&source, string(&owned, "database"))?;
        owned["state"] = json!("copied");
        write(&directory, &owned)?;
    }
    point_environment(task, &source, string(&owned, "database"))
}
