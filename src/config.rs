use crate::{paths::Paths, validation::*};
use serde_json::Value;
use std::fs;

pub fn read(paths: &Paths) -> Result<Value> {
    let message = format!(
        "Project configuration is missing or unreadable; check {}",
        paths.config.display()
    );
    let bytes = fs::read(&paths.config).map_err(|_| Error {
        code: "CONFIG_UNAVAILABLE",
        message: message.clone(),
    })?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| Error {
        code: "INVALID_JSON",
        message,
    })?;
    let data = object(&value)?;
    ensure(
        data.get("schemaVersion").and_then(Value::as_f64) == Some(2.0),
        "Unsupported project schema; expected version 2",
    )?;
    Ok(value)
}
pub fn projects(paths: &Paths) -> Result<Object> {
    let config = read(paths)?;
    Ok(object(field(object(&config)?, "projects")?)?.clone())
}
pub fn settings(paths: &Paths, project: &str) -> Result<Object> {
    name(project)?;
    let projects = projects(paths)?;
    let value = projects
        .get(project)
        .ok_or_else(|| Error::from(format!("Unknown project: {project}")))?;
    Ok(object(value)?.clone())
}
