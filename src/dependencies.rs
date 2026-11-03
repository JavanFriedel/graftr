use crate::{checkout::string, status::compare, system, validation::*};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    env,
    fs::{self, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

const INPUTS: &[&str] = &[
    "package.json",
    "pnpm-lock.yaml",
    "pnpm-workspace.yaml",
    "bun.lock",
    "bun.lockb",
    "bunfig.toml",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    ".yarnrc",
    ".yarnrc.yml",
    ".npmrc",
    ".pnpmfile.cjs",
    "pnpmfile.cjs",
];
pub struct Inputs {
    pub key: String,
    pub manifests: Vec<String>,
}
fn tool(command: Option<&str>, cwd: &Path) -> Result<Value> {
    let Some(command) = command else {
        return Ok(Value::Null);
    };
    let Some(executable) = system::which(command, cwd) else {
        return Ok(json!([command, null]));
    };
    let path = fs::canonicalize(executable)
        .map_err(|_| Error::from("Could not inspect dependency tool identity"))?;
    let metadata = fs::metadata(&path)
        .map_err(|_| Error::from("Could not inspect dependency tool identity"))?;
    let mtime = metadata.mtime() as f64 * 1000.0 + metadata.mtime_nsec() as f64 / 1_000_000.0;
    // JSON.stringify writes whole-valued JS numbers without a fractional suffix.
    let mtime = if mtime.fract() == 0.0 {
        json!(mtime as i64)
    } else {
        json!(mtime)
    };
    Ok(json!([
        command,
        path.to_string_lossy(),
        metadata.dev(),
        metadata.ino(),
        metadata.size(),
        mtime
    ]))
}
pub fn inputs(profile: &Value, task: &Value) -> Result<Inputs> {
    let root = Path::new(string(task, "path"));
    let mut args = vec![
        "git".to_owned(),
        "-C".into(),
        string(task, "path").into(),
        "ls-files".into(),
        "--cached".into(),
        "--others".into(),
        "--exclude-standard".into(),
        "-z".into(),
        "--".into(),
    ];
    args.extend(INPUTS.iter().map(|name| format!(":(glob)**/{name}")));
    args.extend([
        ":(glob)patches/**/*.patch".into(),
        ":(glob,exclude)**/node_modules/**".into(),
    ]);
    let output = system::run(
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
        None,
        true,
    )?
    .stdout;
    let mut inputs: HashSet<String> = output
        .split('\0')
        .filter(|p| {
            !p.is_empty()
                && (Path::new(p)
                    .file_name()
                    .is_some_and(|n| INPUTS.iter().any(|input| n == *input))
                    || p.starts_with("patches/") && p.ends_with(".patch"))
        })
        .map(str::to_owned)
        .collect();
    inputs.extend(INPUTS.iter().map(|s| (*s).to_owned()));
    if let Some(paths) = profile["installInputs"].as_array() {
        inputs.extend(paths.iter().map(|s| s.as_str().unwrap().to_owned()));
    }
    let mut inputs: Vec<_> = inputs.into_iter().collect();
    inputs.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    let mut overrides: Vec<_> = task["overrides"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| json!([k, v]))
        .collect();
    overrides.sort_by(|a, b| compare(a[0].as_str().unwrap(), b[0].as_str().unwrap()));
    let environment: Vec<_> = [
        "NODE_ENV",
        "CI",
        "npm_config_production",
        "npm_config_omit",
        "npm_config_include",
    ]
    .iter()
    .map(|name| {
        json!([
            name,
            task["overrides"]
                .get(name)
                .cloned()
                .unwrap_or_else(|| env::var(name).ok().map_or(Value::Null, Value::String))
        ])
    })
    .collect();
    let tools = [
        tool(profile["installCommand"][0].as_str(), root)?,
        tool(Some("node"), root)?,
        tool(Some("bun"), root)?,
    ];
    let mut header = json!({"version": 2});
    if let Some(command) = profile.get("installCommand") {
        header["command"] = command.clone();
    }
    if let Some(outputs) = profile.get("installOutputs") {
        header["outputs"] = outputs.clone();
    }
    header["overrides"] = json!(overrides);
    header["platform"] = json!(env::consts::OS);
    header["arch"] = json!(match env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        arch => arch,
    });
    header["environment"] = json!(environment);
    header["tools"] = json!(tools);
    let mut hash = Sha256::new();
    hash.update(serde_json::to_vec(&header).unwrap());
    for input in &inputs {
        project_path(&json!(input))?;
        let path = root.join(input);
        let read = || -> std::io::Result<Option<String>> {
            let canonical = match fs::canonicalize(&path) {
                Ok(p) => p,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e),
            };
            if canonical != path {
                return Err(std::io::ErrorKind::InvalidInput.into());
            }
            let mut file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(path)?;
            if !file.metadata()?.is_file() {
                return Err(std::io::ErrorKind::InvalidInput.into());
            }
            let mut digest = Sha256::new();
            let mut buffer = [0_u8; 65536];
            loop {
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                digest.update(&buffer[..count]);
            }
            Ok(Some(format!("{:x}", digest.finalize())))
        };
        let content = read().map_err(|_| {
            Error::from(format!(
                "Could not fingerprint dependency input {input}; check file type and permissions"
            ))
        })?;
        hash.update(serde_json::to_vec(&json!([input, content])).unwrap());
    }
    let manifests = inputs
        .into_iter()
        .filter(|p| {
            Path::new(p)
                .file_name()
                .is_some_and(|n| n == "package.json")
        })
        .collect();
    Ok(Inputs {
        key: format!("v2:{:x}", hash.finalize()),
        manifests,
    })
}
fn output(root: &Path, path: &str) -> Result<Option<Value>> {
    project_path(&json!(path))?;
    match fs::metadata(root.join(path)) {
        Ok(metadata) if metadata.is_file() || metadata.is_dir() => Ok(Some(
            json!({"path": path, "kind": if metadata.is_dir() {"directory"} else {"file"}}),
        )),
        Ok(_) => Ok(None),
        Err(error) if [Some(libc::ENOENT), Some(libc::ENOTDIR)].contains(&error.raw_os_error()) => {
            Ok(None)
        }
        Err(_) => {
            Err(format!("Could not inspect installed output {path}; check permissions").into())
        }
    }
}
pub fn installed_outputs(profile: &Value, task: &Value, manifests: &[String]) -> Result<Value> {
    let explicit = profile["installOutputs"].as_array();
    let mut candidates = vec!["node_modules".to_owned()];
    for manifest in manifests {
        let path = Path::new(manifest)
            .parent()
            .unwrap()
            .join("node_modules")
            .to_string_lossy()
            .into_owned();
        if !candidates.contains(&path) {
            candidates.push(path);
        }
    }
    if let Some(paths) = explicit {
        candidates = paths.iter().map(|p| p.as_str().unwrap().into()).collect();
    }
    let root = Path::new(string(task, "path"));
    let mut outputs = Vec::new();
    for path in &candidates {
        if let Some(output) = output(root, path)? {
            outputs.push(output);
        } else if explicit.is_some() {
            return Err(format!("Configured install output is missing: {path}").into());
        }
    }
    if explicit.is_none() {
        for path in [
            "node_modules/.pnpm",
            "node_modules/.pnpm/lock.yaml",
            "node_modules/.modules.yaml",
            "node_modules/.package-lock.json",
            "node_modules/.bun",
            "node_modules/.bin",
        ] {
            if let Some(output) = output(root, path)? {
                outputs.push(output);
            }
        }
    }
    outputs.sort_by(|a, b| compare(a["path"].as_str().unwrap(), b["path"].as_str().unwrap()));
    Ok(json!(outputs))
}
pub fn outputs_present(task: &Value, outputs: Option<&Value>) -> Result<bool> {
    let Some(outputs) = outputs.and_then(Value::as_array) else {
        return Ok(false);
    };
    for expected in outputs {
        if output(Path::new(string(task, "path")), string(expected, "path"))?
            .is_none_or(|v| v["kind"] != expected["kind"])
        {
            return Ok(false);
        }
    }
    Ok(true)
}
