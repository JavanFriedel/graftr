use crate::validation::{Error, Result};
use std::{
    env,
    ffi::CStr,
    fs,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub struct Paths {
    pub config: PathBuf,
    pub state: PathBuf,
}

pub fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.to_string().into()),
    }
}

pub fn home() -> Result<PathBuf> {
    if let Some(home) = env::var_os("HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    let mut size = 16384;
    loop {
        let mut buffer = vec![0_u8; size];
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // getpwuid_r writes into our live entry/buffer; pw_dir is copied before either drops.
        let status = unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && size < 1024 * 1024 {
            size *= 2;
            continue;
        }
        if status != 0 || result.is_null() {
            return Err("Could not determine the current user's home directory".into());
        }
        let directory = unsafe { (*result).pw_dir };
        if directory.is_null() {
            return Err("Could not determine the current user's home directory".into());
        }
        let bytes = unsafe { CStr::from_ptr(directory) }.to_bytes().to_vec();
        return Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)));
    }
}

pub fn defaults() -> Result<Paths> {
    let config_home = match env::var_os("XDG_CONFIG_HOME") {
        Some(path) => PathBuf::from(path),
        None => home()?.join(".config"),
    };
    let state_home = match env::var_os("XDG_STATE_HOME") {
        Some(path) => PathBuf::from(path),
        None => home()?.join(".local/state"),
    };
    let mut config = config_home.join("graftr");
    if !exists(&config)? && exists(&config_home.join("devtree"))? {
        config = config_home.join("devtree");
    }
    let state = state_home.join("graftr");
    let legacy = state_home.join("devtree");
    let present = exists(&state)?;
    let old = exists(&legacy)?;
    if present
        && old
        && fs::canonicalize(&state).map_err(|e| Error::from(e.to_string()))?
            != fs::canonicalize(&legacy).map_err(|e| Error::from(e.to_string()))?
    {
        return Err("Both graftr and devtree state directories exist; consolidate them before continuing so worktree records and port leases stay together".into());
    }
    Ok(Paths {
        config: config.join("projects.json"),
        state: if present || !old { state } else { legacy },
    })
}
