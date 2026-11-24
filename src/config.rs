use std::{env, net::SocketAddr, path::PathBuf};

const DEFAULT_HOSTS: &str =
    "download.windowsupdate.com,dl.delivery.mp.microsoft.com,download.microsoft.com";

#[derive(Clone)]
pub struct Config {
    pub proxy_listen: SocketAddr,
    pub ui_listen: SocketAddr,
    pub data_dir: PathBuf,
    pub max_cache_bytes: u64,
    pub max_age_secs: u64,
    pub admin_user: String,
    pub admin_password: String,
    pub default_password: bool,
    pub allowed_hosts: Vec<String>,
    pub session_secs: u64,
    pub key_includes_query: bool,
}

fn var(name: &str, default: &str) -> String {
    match env::var(name) {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => default.to_string(),
    }
}

impl Config {
    pub fn from_env() -> Self {
        let proxy_listen: SocketAddr = var("PROXY_LISTEN", "0.0.0.0:8080")
            .parse()
            .expect("invalid PROXY_LISTEN");
        let ui_listen: SocketAddr = var("UI_LISTEN", "0.0.0.0:8081")
            .parse()
            .expect("invalid UI_LISTEN");
        let gb: f64 = var("MAX_CACHE_SIZE_GB", "200")
            .parse()
            .expect("MAX_CACHE_SIZE_GB must be a number");
        let days: u64 = var("CACHE_MAX_AGE_DAYS", "90")
            .parse()
            .expect("CACHE_MAX_AGE_DAYS must be a whole number");
        let hours: u64 = var("SESSION_HOURS", "168")
            .parse()
            .expect("SESSION_HOURS must be a whole number");
        let password = env::var("ADMIN_PASSWORD").ok().filter(|p| !p.is_empty());
        let allowed_hosts = var("ALLOWED_HOSTS", DEFAULT_HOSTS)
            .split(',')
            .map(|h| h.trim().trim_start_matches("*.").to_ascii_lowercase())
            .filter(|h| !h.is_empty())
            .collect();
        Config {
            proxy_listen,
            ui_listen,
            data_dir: PathBuf::from(var("DATA_DIR", "/data")),
            max_cache_bytes: (gb * 1024.0 * 1024.0 * 1024.0) as u64,
            max_age_secs: days * 86_400,
            admin_user: var("ADMIN_USER", "admin"),
            default_password: password.as_deref().map_or(true, |p| p == "changeme"),
            admin_password: password.unwrap_or_else(|| "changeme".to_string()),
            allowed_hosts,
            session_secs: hours * 3600,
            key_includes_query: var("CACHE_KEY_INCLUDES_QUERY", "false") == "true",
        }
    }

    /// Only these hosts (and their subdomains) are ever fetched, so the cache
    /// can't be abused as an open proxy.
    pub fn host_allowed(&self, host: &str) -> bool {
        self.allowed_hosts.iter().any(|d| {
            host == d || host.strip_suffix(d.as_str()).map_or(false, |p| p.ends_with('.'))
        })
    }
}
