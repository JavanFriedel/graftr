use crate::{paths::Paths, records, validation::*};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::Path,
    thread,
    time::{Duration, Instant},
};

// Retain lock files permanently: unlinking one can split ownership across inodes.
pub struct Lock(File);
impl Drop for Lock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
pub fn private_directory(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|_| Error::from("Could not create private state directory"))?;
    records::private_dir(path)
}
fn acquire(paths: &Paths, name: &str, timeout: Duration, message: &str) -> Result<Lock> {
    private_directory(&paths.state)?;
    let path = paths.state.join(name);
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
        .map_err(|_| Error::from("Expected a private regular file owned by you"))?;
    let validate = || -> Result<()> {
        let opened = file
            .metadata()
            .map_err(|_| Error::from("Lock file is unreadable"))?;
        let named =
            fs::symlink_metadata(&path).map_err(|_| Error::from("Lock file is unreadable"))?;
        ensure(
            opened.is_file()
                && named.is_file()
                && opened.uid() == unsafe { libc::getuid() }
                && opened.mode() & 0o077 == 0
                && (opened.dev(), opened.ino()) == (named.dev(), named.ino()),
            "Expected a private regular file owned by you",
        )
    };
    validate()?;
    let deadline = Instant::now() + timeout;
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted
            && error.raw_os_error() != Some(libc::EWOULDBLOCK)
        {
            return Err(message.into());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(message.into());
        }
        thread::sleep(remaining.min(Duration::from_millis(10)));
    }
    validate()?;
    Ok(Lock(file))
}
pub fn state(paths: &Paths) -> Result<Lock> {
    state_with_timeout(paths, Duration::from_secs(30))
}
pub fn state_with_timeout(paths: &Paths, timeout: Duration) -> Result<Lock> {
    acquire(
        paths,
        "operation.lock",
        timeout,
        "Could not acquire the graftr state lock; another registry operation may be running",
    )
}
pub fn worktree(paths: &Paths, path: &str, nonblocking: bool) -> Result<Lock> {
    let key = format!("{:x}", Sha256::digest(path.as_bytes()));
    acquire(
        paths,
        &format!("setup-{key}.lock"),
        Duration::from_secs(if nonblocking { 0 } else { 660 }),
        "Worktree setup is still running; keep the checkout and retry when setup finishes",
    )
}
pub fn frozen(paths: &Paths, task_id: &str) -> Result<Lock> {
    id(task_id)?;
    acquire(
        paths,
        &format!("frozen-{task_id}.lock"),
        Duration::ZERO,
        "Another freeze/thaw operation owns this task; retry after it finishes",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn locks_are_exclusive_private_retained_and_released_on_drop() {
        let root =
            std::env::temp_dir().join(format!("graftr-lock-{}", crate::system::uuid().unwrap()));
        let paths = Paths {
            config: root.join("unused"),
            state: root.clone(),
        };
        let first = worktree(&paths, "/fixture", true).unwrap();
        assert!(worktree(&paths, "/fixture", true).is_err());
        let file = fs::read_dir(&root).unwrap().next().unwrap().unwrap().path();
        let inode = fs::metadata(&file).unwrap().ino();
        assert_eq!(fs::metadata(&file).unwrap().mode() & 0o777, 0o600);
        drop(first);
        drop(worktree(&paths, "/fixture", true).unwrap());
        assert_eq!(fs::metadata(&file).unwrap().ino(), inode);
        fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink("/dev/null", &file).unwrap();
        assert!(worktree(&paths, "/fixture", true).is_err());
        fs::remove_dir_all(&root).unwrap();
    }
}
