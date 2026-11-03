use crate::{adopt, environment, paths::Paths, records, system, tailscale, validation::*};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

pub fn configured_url(profile: &Value, task: &Value) -> Result<Value> {
    if let Some(template) = profile.get("url") {
        let mut identity = task.clone();
        identity["host"] = Value::Null;
        identity["url"] = Value::Null;
        let value = environment::resolve(&json!({"BASE_URL": template}), &identity)?;
        Ok(json!(
            url(value["BASE_URL"].as_str().unwrap())?
                .origin()
                .ascii_serialization()
        ))
    } else {
        Ok(Value::Null)
    }
}
pub fn verify(profile: &Value, task: &Value) -> Result<()> {
    let names = |value: &Value| {
        let mut keys: Vec<_> = value
            .as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        keys.sort();
        keys
    };
    ensure(
        task["httpsPort"].is_null() == profile.get("httpsPorts").is_none()
            && names(&profile["ports"]) == names(&task["ports"])
            && configured_url(profile, task)? == task["url"],
        "Recorded address configuration changed; existing assignments/environment were kept. Use a new task rather than implicit port/URL reassignment",
    )
}
pub fn allocate(paths: &Paths, profile: &Value, identity: &Value) -> Result<Value> {
    let mut leased = HashSet::new();
    let publishes = profile["hooks"]["afterDependencies"]
        .as_array()
        .is_some_and(|hooks| {
            hooks.iter().any(|h| {
                h["command"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .rev()
                    .take(2)
                    .map(|s| s.as_str().unwrap())
                    .collect::<Vec<_>>()
                    == ["tailscale", "hook"]
            })
        });
    if publishes {
        leased.extend(tailscale::configured_ports(&tailscale::status().map_err(
            |e| {
                Error::from(format!(
                    "Could not read Tailscale Serve status to choose a free URL port: {}",
                    e.message
                ))
            },
        )?)?);
    }
    for task in records::tasks(&paths.state)? {
        leased.insert(task.value()["localPort"].as_f64().unwrap() as u16);
        if let Some(port) = task.value()["httpsPort"].as_f64() {
            leased.insert(port as u16);
        }
        if let Some(ports) = task.value()["ports"].as_object() {
            leased.extend(ports.values().map(|p| p.as_f64().unwrap() as u16));
        }
    }
    for record in records::adoptions(&paths.state)? {
        leased.extend(adopt::ports(record.value()).iter().map(|p| *p as u16));
    }
    let mut checked = HashMap::new();
    let mut candidates = |range: &Value| -> Vec<u16> {
        let Some(range) = range.as_array() else {
            return Vec::new();
        };
        let mut result = Vec::new();
        for port in range[0].as_f64().unwrap() as u32..range[1].as_f64().unwrap() as u32 {
            let port = port as u16;
            if !leased.contains(&port)
                && *checked
                    .entry(port)
                    .or_insert_with(|| system::port_free(port))
            {
                result.push(port);
                if result.len() == 2 {
                    break;
                }
            }
        }
        result
    };
    let local = candidates(&profile["localPorts"]);
    let secure = candidates(&profile["httpsPorts"]);
    let pair = if profile.get("httpsPorts").is_none() {
        local.first().map(|p| (*p, None))
    } else {
        local
            .iter()
            .find_map(|p| secure.iter().find(|q| *q != p).map(|q| (*p, Some(*q))))
    }
    .ok_or_else(|| {
        Error::from(
            "No free configured ports remain; retained leases and local listeners were preserved",
        )
    })?;
    leased.insert(pair.0);
    if let Some(port) = pair.1 {
        leased.insert(port);
    }
    let mut ports = serde_json::Map::new();
    if let Some(named) = profile["ports"].as_object() {
        for (name, range) in named {
            let chosen = (range[0].as_f64().unwrap() as u32..range[1].as_f64().unwrap() as u32).map(|p| p as u16).find(|p| !leased.contains(p) && system::port_free(*p))
            .ok_or_else(|| Error::from(format!("No free configured port remains for ports.{name}; retained leases and local listeners were preserved")))?;
            ports.insert(name.clone(), json!(chosen));
            leased.insert(chosen);
        }
    }
    let mut address = json!({"schemaVersion": 4, "localPort": pair.0, "httpsPort": pair.1, "host": null, "url": null});
    if !ports.is_empty() {
        address["ports"] = json!(ports);
    }
    let mut task = identity.clone();
    for (key, value) in address.as_object().unwrap() {
        task[key] = value.clone();
    }
    address["url"] = configured_url(profile, &task)?;
    address["host"] = json!(
        address["url"]
            .as_str()
            .map(url)
            .transpose()?
            .and_then(|u| u.host_str().map(str::to_owned))
    );
    Ok(address)
}
