use std::{fs, path::PathBuf, sync::Mutex};

use serde::{Deserialize, Serialize};

/// Display settings edited on the Config tab, stored in the data volume.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Rows shown in the Dashboard's Devices and Recent requests lists.
    pub dashboard_limit: usize,
    /// Items per page on the Devices, Requests and Cached files tabs.
    pub page_size: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { dashboard_limit: 10, page_size: 25 }
    }
}

impl Settings {
    fn validate(&self) -> Result<(), String> {
        if !(1..=100).contains(&self.dashboard_limit) {
            return Err("Rows on the Dashboard must be between 1 and 100.".to_string());
        }
        if !(5..=200).contains(&self.page_size) {
            return Err("Items per page must be between 5 and 200.".to_string());
        }
        Ok(())
    }
}

pub struct SettingsStore {
    cur: Mutex<Settings>,
    file: PathBuf,
}

impl SettingsStore {
    pub fn load(file: PathBuf) -> Self {
        let mut s: Settings = fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        if s.validate().is_err() {
            s = Settings::default();
        }
        SettingsStore { cur: Mutex::new(s), file }
    }

    pub fn get(&self) -> Settings {
        self.cur.lock().unwrap().clone()
    }

    pub fn set(&self, s: Settings) -> Result<(), String> {
        s.validate()?;
        *self.cur.lock().unwrap() = s.clone();
        if let Ok(json) = serde_json::to_vec_pretty(&s) {
            let tmp = self.file.with_extension("tmp");
            if fs::write(&tmp, json).is_ok() {
                let _ = fs::rename(&tmp, &self.file);
            }
        }
        Ok(())
    }
}
