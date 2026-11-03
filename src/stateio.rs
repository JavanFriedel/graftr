use crate::{system, validation::*};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub fn sync_directory(path: &Path) -> Result<()> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .and_then(|file| file.sync_all())
        .map_err(|_| Error::from("Could not sync state directory"))
}
pub fn write_adoption(paths: &crate::paths::Paths, record: &serde_json::Value) -> Result<()> {
    let record = crate::records::Adoption::parse(record)?;
    crate::locks::private_directory(&paths.state)?;
    let directory = paths.state.join("adopted");
    crate::locks::private_directory(&directory)?;
    atomic_write(
        &directory.join(format!("{}.json", record.value()["id"].as_str().unwrap())),
        format!(
            "{}\n",
            serde_json::to_string_pretty(record.value()).unwrap()
        )
        .as_bytes(),
        false,
    )
}
pub fn write_task(paths: &crate::paths::Paths, record: &serde_json::Value) -> Result<()> {
    let record = crate::records::Task::parse(record)?;
    crate::locks::private_directory(&paths.state)?;
    atomic_write(
        &paths
            .state
            .join(format!("{}.json", record.value()["id"].as_str().unwrap())),
        format!(
            "{}\n",
            serde_json::to_string_pretty(record.value()).unwrap()
        )
        .as_bytes(),
        false,
    )
}
pub fn setup_path(
    paths: &crate::paths::Paths,
    socket: &serde_json::Value,
    path: &serde_json::Value,
) -> PathBuf {
    use sha2::{Digest, Sha256};
    paths.state.join("setup").join(format!(
        "{:x}.json",
        Sha256::digest(serde_json::to_vec(&serde_json::json!([socket, path])).unwrap())
    ))
}
pub fn write_setup(paths: &crate::paths::Paths, receipt: &serde_json::Value) -> Result<()> {
    let receipt = crate::records::Setup::parse(receipt)?;
    crate::locks::private_directory(&paths.state)?;
    crate::locks::private_directory(&paths.state.join("setup"))?;
    atomic_write(
        &setup_path(paths, &receipt.value()["socket"], &receipt.value()["path"]),
        format!(
            "{}\n",
            serde_json::to_string_pretty(receipt.value()).unwrap()
        )
        .as_bytes(),
        false,
    )
}
pub fn creation_intent(
    paths: &crate::paths::Paths,
    socket: Option<&str>,
    repo: &str,
    branch: &str,
    active: bool,
) -> Result<()> {
    use sha2::{Digest, Sha256};
    let directory = paths.state.join("creating");
    if crate::paths::exists(&directory)? {
        crate::records::private_dir(&directory)?;
    }
    let file = directory.join(format!(
        "{:x}.json",
        Sha256::digest(serde_json::to_vec(&serde_json::json!([socket, repo, branch])).unwrap())
    ));
    if active {
        crate::locks::private_directory(&paths.state)?;
        crate::locks::private_directory(&directory)?;
        atomic_write(
            &file,
            format!(
                "{}\n",
                serde_json::json!({"socket": socket, "repo": repo, "branch": branch})
            )
            .as_bytes(),
            false,
        )
    } else {
        match fs::remove_file(file) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err("Could not remove creation intent".into()),
        }
    }
}
pub fn delete_setup(paths: &crate::paths::Paths, task: &serde_json::Value) -> Result<()> {
    if let Some(receipt) = crate::records::setup_for(
        &paths.state,
        task["socket"].as_str(),
        task["path"].as_str().unwrap(),
    )? {
        let file = setup_path(paths, &receipt.value()["socket"], &receipt.value()["path"]);
        match fs::remove_file(file) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => return Err("Could not delete setup receipt".into()),
        }
    }
    Ok(())
}
pub fn has_creation_intent(
    paths: &crate::paths::Paths,
    socket: &str,
    repo: &str,
    branch: &str,
) -> Result<bool> {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::MetadataExt;
    let directory = paths.state.join("creating");
    if crate::paths::exists(&directory)? {
        crate::records::private_dir(&directory)?;
    }
    let file = directory.join(format!(
        "{:x}.json",
        Sha256::digest(serde_json::to_vec(&serde_json::json!([socket, repo, branch])).unwrap())
    ));
    if !crate::paths::exists(&file)? {
        return Ok(false);
    }
    let metadata =
        fs::symlink_metadata(file).map_err(|_| Error::from("Could not inspect creation intent"))?;
    ensure(
        metadata.is_file()
            && metadata.mode() & 0o077 == 0
            && metadata.uid() == unsafe { libc::getuid() },
        "Expected a private regular file owned by you",
    )?;
    Ok(true)
}
pub fn delete_task(paths: &crate::paths::Paths, task: &serde_json::Value) -> Result<()> {
    ensure(
        !crate::cleanup::pending(task, "cleanup"),
        "Cleanup hooks are unfinished; task ownership and leases must be retained",
    )?;
    fs::remove_file(
        paths
            .state
            .join(format!("{}.json", task["id"].as_str().unwrap())),
    )
    .map_err(|_| Error::from("Could not retire task record; preserve its leases for review"))?;
    sync_directory(&paths.state)
}
pub fn write_frozen(paths: &crate::paths::Paths, record: &serde_json::Value) -> Result<()> {
    let record = crate::records::Frozen::parse(record)?;
    crate::locks::private_directory(&paths.state)?;
    let directory = paths.state.join("frozen");
    crate::locks::private_directory(&directory)?;
    atomic_write(
        &directory.join(format!("{}.json", record.value()["id"].as_str().unwrap())),
        format!(
            "{}\n",
            serde_json::to_string_pretty(record.value()).unwrap()
        )
        .as_bytes(),
        false,
    )
}
struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        if !self.0.as_os_str().is_empty() {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
pub fn atomic_write(path: &Path, bytes: &[u8], exclusive: bool) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::from("Missing destination directory"))?;
    let temporary = parent.join(format!(".graftr-{}", system::uuid()?));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&temporary)
        .map_err(|_| Error::from("Could not create private temporary directory"))?;
    let mut temporary = Temporary(temporary);
    let operation = || -> Result<()> {
        fs::set_permissions(&temporary.0, fs::Permissions::from_mode(0o700))
            .map_err(|_| Error::from("Could not protect private temporary directory"))?;
        let data = temporary.0.join("data");
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&data)
            .map_err(|_| Error::from("Could not create private temporary file"))?;
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| Error::from("Could not write private file; contents were not logged"))?;
        drop(file);
        if exclusive {
            fs::hard_link(&data, path)
                .map_err(|_| Error::from("Could not create exclusive private file"))?;
        } else {
            match fs::symlink_metadata(path) {
                Ok(metadata) => ensure(!metadata.is_symlink(), "Refusing to replace a symlink")?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(_) => return Err("Could not inspect replacement destination".into()),
            }
            fs::rename(&data, path).map_err(|_| Error::from("Could not replace private file"))?;
        }
        Ok(())
    }();
    let cleaned = fs::remove_dir_all(&temporary.0)
        .map_err(|_| Error::from("Could not remove private temporary directory"));
    if cleaned.is_ok() {
        temporary.0.clear();
    }
    let synced = sync_directory(parent);
    cleaned?;
    synced?;
    operation
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    #[test]
    fn replacement_is_private_independent_durable_and_exclusive() {
        let root = std::env::temp_dir().join(format!("graftr-write-{}", system::uuid().unwrap()));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let target = root.join("target");
        let backup = root.join("backup");
        atomic_write(&target, b"original", true).unwrap();
        atomic_write(&backup, b"original", true).unwrap();
        assert!(atomic_write(&backup, b"overwrite", true).is_err());
        atomic_write(&target, b"updated", false).unwrap();
        assert_eq!(fs::read(&backup).unwrap(), b"original");
        assert_eq!(fs::metadata(&target).unwrap().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(&backup).unwrap().nlink(), 1);
        assert_ne!(
            fs::metadata(&target).unwrap().ino(),
            fs::metadata(&backup).unwrap().ino()
        );
        let link = root.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(atomic_write(&link, b"unsafe", false).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"updated");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 3);
        fs::remove_dir_all(root).unwrap();
    }
}
