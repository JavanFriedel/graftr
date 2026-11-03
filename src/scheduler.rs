use crate::{
    checkout::string,
    health,
    paths::{self, Paths},
    records, stateio, system,
    validation::*,
};
use serde_json::Value;
use std::{
    env, fs,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
};

pub fn invocation() -> Result<Vec<String>> {
    Ok(vec![
        env::current_exe()
            .map_err(|_| Error::from("Could not locate native executable"))?
            .to_string_lossy()
            .into_owned(),
    ])
}
fn unit_string(value: &str, exec: bool) -> Result<String> {
    ensure(
        !value.chars().any(|c| c <= '\u{1f}' || c == '\u{7f}'),
        "Systemd unit values cannot contain control characters",
    )?;
    Ok(format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', if exec { "$$" } else { "$" })
    ))
}
fn name(record: &Value) -> String {
    format!(
        "graftr-thaw-{}-{}",
        string(record, "id"),
        string(record, "generation")
    )
}
fn header(record: &Value) -> String {
    format!(
        "# Managed by graftr: {} {}\n",
        string(record, "id"),
        string(record, "generation")
    )
}
pub struct Scheduler<'a> {
    pub paths: &'a Paths,
}
impl Scheduler<'_> {
    fn directory(&self) -> Result<PathBuf> {
        Ok(self
            .paths
            .config
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| Error::from("Scheduled thaw requires an absolute configuration path"))?
            .join("systemd/user"))
    }
    fn control(&self, args: &[&str]) -> Result<()> {
        system::run(&[&["systemctl", "--user"], args].concat(), None, true)?;
        Ok(())
    }
    pub fn preflight(&self) -> Result<()> {
        ensure(
            env::consts::OS == "linux",
            "Scheduled thaw requires Linux systemd user timers; manual freeze/thaw needs no scheduler",
        )?;
        for path in [
            self.directory()?,
            self.paths.config.clone(),
            self.paths.state.clone(),
            PathBuf::from(&invocation()?[0]),
        ] {
            let path = path.to_string_lossy();
            ensure(
                absolute_normalized(&path),
                "Scheduled thaw requires absolute executable/configuration/state paths",
            )?;
            unit_string(&path, false)?;
        }
        self.control(&["show-environment"])
    }
    fn owned(&self, file: &Path, header: &str) -> Result<()> {
        if paths::exists(file)? {
            ensure(
                records::read_private(file)?.starts_with(header.as_bytes()),
                "Refusing to replace or delete a foreign systemd unit",
            )?;
        }
        Ok(())
    }
    pub fn arm(&self, record: &Value, at: &str) -> Result<()> {
        let date = date_millis(at).ok_or_else(|| Error::from("Invalid thaw timer date"))?;
        self.preflight()?;
        let directory = self.directory()?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)
            .map_err(|_| Error::from("Could not create systemd user unit directory"))?;
        let metadata = fs::symlink_metadata(&directory)
            .map_err(|_| Error::from("Could not inspect systemd user unit directory"))?;
        ensure(
            metadata.is_dir()
                && metadata.uid() == unsafe { libc::getuid() }
                && metadata.mode() & 0o022 == 0,
            "Systemd user unit directory must be owned by you and not writable by others or symlinked",
        )?;
        let name = name(record);
        let header = header(record);
        let mut command = invocation()?;
        command.extend([
            "_thaw".into(),
            string(record, "id").into(),
            string(record, "generation").into(),
            self.paths.config.to_string_lossy().into_owned(),
            self.paths.state.to_string_lossy().into_owned(),
        ]);
        let command = command
            .iter()
            .map(|arg| unit_string(arg, true))
            .collect::<Result<Vec<_>>>()?
            .join(" ");
        let now = date_millis(&health::timestamp()).unwrap();
        let firing_at = ((date.max(now + 1000) + 999) / 1000) * 1000;
        let calendar = chrono::DateTime::from_timestamp_millis(firing_at)
            .unwrap()
            .format("%Y-%m-%d %H:%M:%S UTC");
        let environment = unit_string(
            &format!(
                "PATH={}",
                env::var("PATH").unwrap_or("/usr/local/bin:/usr/bin:/bin".into())
            ),
            false,
        )?;
        let service = format!(
            "{header}[Unit]\nDescription=Graftr scheduled thaw\n\n[Service]\nType=oneshot\nWorkingDirectory=%h\nEnvironment={environment}\nExecStart={command}\nTimeoutStartSec=infinity\nUMask=0077\n"
        );
        let timer = format!(
            "{header}[Unit]\nDescription=Graftr one-off thaw\n\n[Timer]\nOnCalendar={calendar}\nPersistent=true\nAccuracySec=1s\nUnit={name}.service\n\n[Install]\nWantedBy=timers.target\n"
        );
        for (extension, contents) in [("service", service), ("timer", timer)] {
            let file = directory.join(format!("{name}.{extension}"));
            self.owned(&file, &header)?;
            stateio::atomic_write(&file, contents.as_bytes(), false)?;
        }
        self.control(&["daemon-reload"])?;
        self.control(&["enable", &format!("{name}.timer")])?;
        self.control(&["restart", &format!("{name}.timer")])
    }
    pub fn cancel(&self, record: &Value) -> Result<()> {
        let directory = self.directory()?;
        let name = name(record);
        let header = header(record);
        let files = [
            directory.join(format!("{name}.timer")),
            directory.join(format!("{name}.service")),
        ];
        if !paths::exists(&files[0])? && !paths::exists(&files[1])? {
            return Ok(());
        }
        for file in &files {
            self.owned(file, &header)?;
        }
        if paths::exists(&files[0])? {
            self.control(&["disable", "--now", &format!("{name}.timer")])?;
        }
        for file in files {
            if paths::exists(&file)? {
                fs::remove_file(file)
                    .map_err(|_| Error::from("Could not remove owned thaw timer unit"))?;
            }
        }
        self.control(&["daemon-reload"])
    }
}
