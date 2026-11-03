use crate::{
    human, locks, paths, progress::Progress, records, stateio, system, targets::TargetError,
    validation::*,
};
use serde_json::{Value, json};
use std::env;

const MANIFEST: &str = include_str!("../plugins/herdr/herdr-plugin.toml");
const HEADER: &str =
    "# Managed by graftr plugin install. Reinstall to update; do not edit this copy.\n";
fn parse(text: &str) -> Result<Value> {
    toml::from_str(text)
        .map_err(|_| Error::from("Invalid plugin manifest TOML; contents were not logged"))
}
pub fn install(progress: &Progress) -> std::result::Result<Value, TargetError> {
    let manifest = parse(MANIFEST)?;
    let id = text(&manifest["id"])?;
    let cwd = env::current_dir().map_err(|_| Error::from("Current directory is unavailable"))?;
    let executable = system::which("herdr", &cwd).ok_or_else(|| TargetError {
        code: "HERDR_NOT_INSTALLED",
        message: "Plugin installation requires the Herdr CLI on PATH; no plugin files were written"
            .into(),
        details: Value::Null,
    })?;
    let data_home = env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or(paths::home()?.join(".local/share"));
    ensure(
        absolute_normalized(&data_home.to_string_lossy()),
        "Plugin installation requires an absolute, normalized XDG_DATA_HOME",
    )?;
    let directory = data_home.join("graftr/plugin");
    let path = directory.join("herdr-plugin.toml");
    locks::private_directory(&directory)?;
    if paths::exists(&path)? {
        let previous = records::read_private(&path)?;
        let previous = String::from_utf8_lossy(&previous);
        ensure(
            previous.starts_with(HEADER) && parse(&previous)?["id"] == id,
            "Refusing to overwrite a foreign plugin manifest; keep it and review the managed plugin directory",
        )?;
    }
    progress.run("write-plugin", id, || {
        stateio::atomic_write(
            &path,
            format!("{HEADER}{}\n", MANIFEST.trim_end_matches(js_space)).as_bytes(),
            false,
        )
    })?;
    progress.run("link-plugin", id, || system::run(&[&executable.to_string_lossy(), "plugin", "link", &directory.to_string_lossy()], None, true))
        .map_err(|_| TargetError {code: "PLUGIN_LINK_FAILED", message: "Herdr plugin registration was not confirmed. The managed manifest was retained; inspect herdr plugin list and retry installation after resolving the issue.".into(), details: json!({"id": id, "directory": directory.to_string_lossy()})})?;
    let startup: Vec<_> = manifest["startup"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .map(|s| s["command"].clone())
        .collect();
    let events: Vec<_> = manifest["events"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .map(|s| s["on"].clone())
        .collect();
    Ok(
        json!({"id": id, "status": "installed", "directory": directory.to_string_lossy(), "hooks": {"startup": startup, "events": events}, "message": "Plugin linked and enabled for this user across Herdr sessions. Startup hooks run on future server starts/handoffs, not during installation. Graftr and Herdr executables must remain available on Herdr's PATH."}),
    )
}
pub fn format(report: &Value, columns: usize) -> String {
    human::fields(
        &format!("{}: installed", report["id"].as_str().unwrap()),
        &[(
            "Manifest directory",
            Some(report["directory"].as_str().unwrap().into()),
        )],
        report["message"].as_str(),
        "installed",
        columns,
    )
}
