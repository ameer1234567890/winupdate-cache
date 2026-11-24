use std::{
    collections::HashMap,
    fs,
    net::IpAddr,
    path::PathBuf,
    sync::Mutex,
};

/// Friendly names for device IP addresses, persisted as JSON in the data volume.
pub struct Names {
    map: Mutex<HashMap<String, String>>,
    file: PathBuf,
}

fn normalize(ip: &str) -> Result<String, String> {
    let ip: IpAddr = ip
        .trim()
        .parse()
        .map_err(|_| "That is not a valid IP address.".to_string())?;
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    };
    Ok(ip.to_string())
}

impl Names {
    pub fn load(file: PathBuf) -> Self {
        let map = fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Names { map: Mutex::new(map), file }
    }

    pub fn all(&self) -> HashMap<String, String> {
        self.map.lock().unwrap().clone()
    }

    /// An empty name removes the entry.
    pub fn set(&self, ip: &str, name: &str) -> Result<(), String> {
        let key = normalize(ip)?;
        let name = name.trim();
        if name.chars().count() > 64 || name.chars().any(|c| c.is_control()) {
            return Err("Names can be at most 64 characters.".to_string());
        }
        {
            let mut m = self.map.lock().unwrap();
            if name.is_empty() {
                m.remove(&key);
            } else {
                m.insert(key, name.to_string());
            }
        }
        self.save();
        Ok(())
    }

    pub fn remove(&self, ip: &str) -> Result<(), String> {
        self.set(ip, "")
    }

    fn save(&self) {
        let snapshot = self.all();
        if let Ok(json) = serde_json::to_vec_pretty(&snapshot) {
            let tmp = self.file.with_extension("tmp");
            if fs::write(&tmp, json).is_ok() {
                let _ = fs::rename(&tmp, &self.file);
            }
        }
    }
}
