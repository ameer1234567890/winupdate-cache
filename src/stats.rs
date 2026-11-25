use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::Mutex,
    time::Instant,
};

use serde::{Deserialize, Serialize};

use crate::store::now;

const BUCKET_SECS: u64 = 900; // 15 minutes
const MAX_BUCKETS: usize = 96; // 24 hours
const MAX_LOG: usize = 1000;

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Hit,
    Miss,
    Bypass,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Totals {
    pub hits: u64,
    pub misses: u64,
    pub bypass: u64,
    pub bytes_cache: u64,
    pub bytes_upstream: u64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Bucket {
    pub t: u64,
    pub cache: u64,
    pub upstream: u64,
}

#[derive(Clone, Serialize)]
pub struct LogEntry {
    pub t: u64,
    pub client: String,
    pub host: String,
    pub path: String,
    pub kind: Kind,
    pub status: u16,
    pub bytes: u64,
}

#[derive(Clone, Serialize)]
pub struct ClientRow {
    pub ip: String,
    pub requests: u64,
    pub bytes: u64,
    pub last_seen: u64,
}

#[derive(Serialize)]
pub struct Snapshot {
    pub totals: Totals,
    pub buckets: Vec<Bucket>,
    pub clients: Vec<ClientRow>,
    pub recent: Vec<LogEntry>,
    pub clients_total: usize,
    pub recent_total: usize,
    pub uptime: u64,
}

#[derive(Serialize, Deserialize, Default)]
struct Persisted {
    totals: Totals,
    buckets: Vec<Bucket>,
}

struct Inner {
    totals: Totals,
    buckets: VecDeque<Bucket>,
    clients: HashMap<String, ClientRow>,
    recent: VecDeque<LogEntry>,
}

impl Inner {
    fn bucket(&mut self) -> &mut Bucket {
        let t = now() / BUCKET_SECS * BUCKET_SECS;
        if self.buckets.back().map_or(true, |b| b.t != t) {
            self.buckets.push_back(Bucket { t, cache: 0, upstream: 0 });
            while self.buckets.len() > MAX_BUCKETS {
                self.buckets.pop_front();
            }
        }
        self.buckets.back_mut().unwrap()
    }
}

pub struct Stats {
    inner: Mutex<Inner>,
    started: Instant,
    file: PathBuf,
}

impl Stats {
    pub fn load(file: PathBuf) -> Self {
        let p: Persisted = std::fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Stats {
            inner: Mutex::new(Inner {
                totals: p.totals,
                buckets: p.buckets.into(),
                clients: HashMap::new(),
                recent: VecDeque::new(),
            }),
            started: Instant::now(),
            file,
        }
    }

    pub fn save(&self) {
        let p = {
            let i = self.inner.lock().unwrap();
            Persisted {
                totals: i.totals.clone(),
                buckets: i.buckets.iter().cloned().collect(),
            }
        };
        if let Ok(json) = serde_json::to_vec(&p) {
            let tmp = self.file.with_extension("tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, &self.file);
            }
        }
    }

    /// Bytes pulled from the internet (cache fills and bypassed requests).
    pub fn add_upstream(&self, n: u64) {
        let mut i = self.inner.lock().unwrap();
        i.totals.bytes_upstream += n;
        i.bucket().upstream += n;
    }

    pub fn record(&self, client: &str, host: &str, path: &str, kind: Kind, status: u16, bytes: u64) {
        let mut i = self.inner.lock().unwrap();
        match kind {
            Kind::Hit => {
                i.totals.hits += 1;
                i.totals.bytes_cache += bytes;
                i.bucket().cache += bytes;
            }
            Kind::Miss => i.totals.misses += 1,
            Kind::Bypass => i.totals.bypass += 1,
        }
        let t = now();
        if i.clients.len() < 2000 || i.clients.contains_key(client) {
            let c = i.clients.entry(client.to_string()).or_insert_with(|| ClientRow {
                ip: client.to_string(),
                requests: 0,
                bytes: 0,
                last_seen: 0,
            });
            c.requests += 1;
            c.bytes += bytes;
            c.last_seen = t;
        }
        i.recent.push_front(LogEntry {
            t,
            client: client.to_string(),
            host: host.to_string(),
            path: path.chars().take(200).collect(),
            kind,
            status,
            bytes,
        });
        while i.recent.len() > MAX_LOG {
            i.recent.pop_back();
        }
    }

    /// Every device seen since the last restart.
    pub fn clients(&self) -> Vec<ClientRow> {
        self.inner.lock().unwrap().clients.values().cloned().collect()
    }

    /// One page of the request log, newest first. Returns (rows, total, page used).
    pub fn requests(&self, page: usize, size: usize) -> (Vec<LogEntry>, usize, usize) {
        let i = self.inner.lock().unwrap();
        let total = i.recent.len();
        let size = size.max(1);
        let pages = ((total + size - 1) / size).max(1);
        let page = page.clamp(1, pages);
        let rows = i.recent.iter().skip((page - 1) * size).take(size).cloned().collect();
        (rows, total, page)
    }

    pub fn snapshot(&self, limit: usize) -> Snapshot {
        let i = self.inner.lock().unwrap();
        let mut clients: Vec<ClientRow> = i.clients.values().cloned().collect();
        clients.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
        let clients_total = clients.len();
        clients.truncate(limit);
        Snapshot {
            totals: i.totals.clone(),
            buckets: i.buckets.iter().cloned().collect(),
            clients,
            recent: i.recent.iter().take(limit).cloned().collect(),
            clients_total,
            recent_total: i.recent.len(),
            uptime: self.started.elapsed().as_secs(),
        }
    }
}
