mod config;
mod names;
mod settings;
mod proxy;
mod stats;
mod store;
mod web;

use std::{
    collections::{HashMap, VecDeque},
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::net::TcpListener;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

pub struct App {
    pub cfg: config::Config,
    pub store: store::Store,
    pub stats: stats::Stats,
    pub names: names::Names,
    pub settings: settings::SettingsStore,
    pub http: reqwest::Client,
    pub sessions: Mutex<HashMap<String, u64>>,
    pub login_fails: Mutex<VecDeque<u64>>,
}

/// `winupdate-cache healthcheck` – used by the Docker HEALTHCHECK.
fn healthcheck() -> i32 {
    let port = config::Config::from_env().ui_listen.port();
    let probe = || -> std::io::Result<bool> {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
        s.set_read_timeout(Some(Duration::from_secs(3)))?;
        s.write_all(b"GET /healthz HTTP/1.0\r\n\r\n")?;
        let mut out = String::new();
        s.read_to_string(&mut out)?;
        Ok(out.starts_with("HTTP/1.0 200") || out.starts_with("HTTP/1.1 200"))
    };
    if probe().unwrap_or(false) { 0 } else { 1 }
}

#[tokio::main]
async fn main() {
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        std::process::exit(healthcheck());
    }
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let cfg = config::Config::from_env();
    let store = store::Store::open(&cfg.data_dir).unwrap_or_else(|e| {
        error!("cannot open cache directory {}: {e}", cfg.data_dir.display());
        std::process::exit(1)
    });
    let stats = stats::Stats::load(cfg.data_dir.join("stats.json"));
    let names = names::Names::load(cfg.data_dir.join("devices.json"));
    let settings = settings::SettingsStore::load(cfg.data_dir.join("settings.json"));
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(5))
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(8)
        .build()
        .expect("failed to build HTTP client");

    let (objects, bytes, _) = store.summary();
    info!("winupdate-cache {} starting: {objects} cached objects ({} MiB)", env!("CARGO_PKG_VERSION"), bytes / 1048576);
    info!("serving hosts: {}", cfg.allowed_hosts.join(", "));
    if cfg.default_password {
        warn!("ADMIN_PASSWORD is not set (or is 'changeme'): change it in your docker compose environment!");
    }

    let app = Arc::new(App {
        cfg,
        store,
        stats,
        names,
        settings,
        http,
        sessions: Mutex::new(HashMap::new()),
        login_fails: Mutex::new(VecDeque::new()),
    });

    // background maintenance
    {
        let app = app.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let n = app.store.evict(app.cfg.max_cache_bytes, app.cfg.max_age_secs);
                if n > 0 {
                    info!("evicted {n} cached objects");
                }
            }
        });
    }
    {
        let app = app.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                app.stats.save();
            }
        });
    }

    let proxy_l = TcpListener::bind(app.cfg.proxy_listen).await.unwrap_or_else(|e| {
        error!("cannot bind {}: {e}", app.cfg.proxy_listen);
        std::process::exit(1)
    });
    let ui_l = TcpListener::bind(app.cfg.ui_listen).await.unwrap_or_else(|e| {
        error!("cannot bind {}: {e}", app.cfg.ui_listen);
        std::process::exit(1)
    });
    info!("cache listening on {}, web UI on {}", app.cfg.proxy_listen, app.cfg.ui_listen);

    let proxy_svc = proxy::router(app.clone()).into_make_service_with_connect_info::<SocketAddr>();
    let ui_svc = web::router(app.clone()).into_make_service();
    let proxy_task = tokio::spawn(async move { axum::serve(proxy_l, proxy_svc).await });
    let ui_task = tokio::spawn(async move { axum::serve(ui_l, ui_svc).await });

    tokio::select! {
        r = proxy_task => error!("cache server stopped: {r:?}"),
        r = ui_task => error!("web UI server stopped: {r:?}"),
        _ = shutdown() => info!("shutting down"),
    }
    app.stats.save();
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}
