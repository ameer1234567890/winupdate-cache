use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tracing::warn;

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Cache key = sha256(host, path[, query]).
pub fn make_key(host: &str, path: &str, query: Option<&str>) -> String {
    let mut h = Sha256::new();
    h.update(host.as_bytes());
    h.update([0]);
    h.update(path.as_bytes());
    if let Some(q) = query {
        h.update([0]);
        h.update(q.as_bytes());
    }
    hex::encode(h.finalize())
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Meta {
    pub key: String,
    pub host: String,
    pub path: String,
    pub size: u64,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub created: u64,
    pub last_access: u64,
    pub hits: u64,
    pub validated: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Progress {
    Running(u64),
    Done,
    Failed,
}

/// A download that is currently being written to disk.
pub struct Download {
    pub meta: Meta,
    pub rx: watch::Receiver<Progress>,
}

pub enum Registered {
    New(Arc<Download>, watch::Sender<Progress>),
    Existing(Arc<Download>),
    Complete,
}

pub struct Store {
    root: PathBuf,
    index: Mutex<HashMap<String, Meta>>,
    inflight: Mutex<HashMap<String, Arc<Download>>>,
}

impl Store {
    pub fn open(data_dir: &Path) -> io::Result<Self> {
        let root = data_dir.join("objects");
        fs::create_dir_all(&root)?;
        let mut index = HashMap::new();
        for shard in fs::read_dir(&root)?.flatten() {
            if !shard.path().is_dir() {
                continue;
            }
            for f in fs::read_dir(shard.path())?.flatten() {
                let p = f.path();
                match p.extension().and_then(|e| e.to_str()) {
                    Some("json") => {
                        let bin = p.with_extension("bin");
                        let meta: Option<Meta> = fs::read(&p)
                            .ok()
                            .and_then(|b| serde_json::from_slice(&b).ok());
                        match meta {
                            Some(m)
                                if fs::metadata(&bin).map(|x| x.len() == m.size).unwrap_or(false) =>
                            {
                                index.insert(m.key.clone(), m);
                            }
                            _ => {
                                let _ = fs::remove_file(&p);
                                let _ = fs::remove_file(&bin);
                            }
                        }
                    }
                    // leftovers of interrupted downloads
                    Some("bin") if !p.with_extension("json").exists() => {
                        let _ = fs::remove_file(&p);
                    }
                    _ => {}
                }
            }
        }
        Ok(Store {
            root,
            index: Mutex::new(index),
            inflight: Mutex::new(HashMap::new()),
        })
    }

    fn dir(&self, key: &str) -> PathBuf {
        self.root.join(&key[..2])
    }
    pub fn bin_path(&self, key: &str) -> PathBuf {
        self.dir(key).join(format!("{key}.bin"))
    }
    fn json_path(&self, key: &str) -> PathBuf {
        self.dir(key).join(format!("{key}.json"))
    }

    /// Returns the metadata of a complete object and records the access.
    pub fn lookup(&self, key: &str) -> Option<Meta> {
        let mut idx = self.index.lock().unwrap();
        let m = idx.get_mut(key)?;
        m.last_access = now();
        m.hits += 1;
        Some(m.clone())
    }

    pub fn touch_validated(&self, key: &str) {
        if let Some(m) = self.index.lock().unwrap().get_mut(key) {
            m.validated = now();
        }
    }

    pub fn get_inflight(&self, key: &str) -> Option<Arc<Download>> {
        self.inflight.lock().unwrap().get(key).cloned()
    }

    /// Lock order: inflight -> index (never the other way round while nested).
    pub fn register(&self, meta: Meta) -> io::Result<Registered> {
        let mut inf = self.inflight.lock().unwrap();
        if let Some(d) = inf.get(&meta.key) {
            return Ok(Registered::Existing(d.clone()));
        }
        if self.index.lock().unwrap().contains_key(&meta.key) {
            return Ok(Registered::Complete);
        }
        fs::create_dir_all(self.dir(&meta.key))?;
        let (tx, rx) = watch::channel(Progress::Running(0));
        let d = Arc::new(Download { meta: meta.clone(), rx });
        inf.insert(meta.key.clone(), d.clone());
        Ok(Registered::New(d, tx))
    }

    pub fn finish(&self, mut meta: Meta) {
        let t = now();
        meta.created = t;
        meta.last_access = t;
        meta.validated = t;
        match serde_json::to_vec(&meta) {
            Ok(json) => {
                if let Err(e) = fs::write(self.json_path(&meta.key), json) {
                    warn!("could not write metadata: {e}");
                }
            }
            Err(e) => warn!("could not serialise metadata: {e}"),
        }
        let key = meta.key.clone();
        self.index.lock().unwrap().insert(key.clone(), meta);
        self.inflight.lock().unwrap().remove(&key);
    }

    pub fn fail(&self, key: &str) {
        self.inflight.lock().unwrap().remove(key);
        let _ = fs::remove_file(self.bin_path(key));
    }

    pub fn remove(&self, key: &str) -> bool {
        let existed = self.index.lock().unwrap().remove(key).is_some();
        if existed {
            let _ = fs::remove_file(self.json_path(key));
            let _ = fs::remove_file(self.bin_path(key));
        }
        existed
    }

    pub fn purge_all(&self) -> (usize, u64) {
        let all: Vec<Meta> = self.index.lock().unwrap().values().cloned().collect();
        let mut bytes = 0;
        for m in &all {
            if self.remove(&m.key) {
                bytes += m.size;
            }
        }
        (all.len(), bytes)
    }

    /// One page of cached objects, most recently used first.
    /// Returns (objects, total matching, page used).
    pub fn list_page(&self, q: &str, page: usize, size: usize) -> (Vec<Meta>, usize, usize) {
        let q = q.to_lowercase();
        let idx = self.index.lock().unwrap();
        let mut v: Vec<&Meta> = idx
            .values()
            .filter(|m| q.is_empty() || m.host.contains(&q) || m.path.to_lowercase().contains(&q))
            .collect();
        v.sort_by(|a, b| b.last_access.cmp(&a.last_access));
        let total = v.len();
        let size = size.max(1);
        let pages = ((total + size - 1) / size).max(1);
        let page = page.clamp(1, pages);
        let start = (page - 1) * size;
        let end = (start + size).min(total);
        (v[start..end].iter().map(|m| (*m).clone()).collect(), total, page)
    }

    /// (objects, bytes, downloads in progress)
    pub fn summary(&self) -> (usize, u64, usize) {
        let (n, bytes) = {
            let idx = self.index.lock().unwrap();
            (idx.len(), idx.values().map(|m| m.size).sum())
        };
        let d = self.inflight.lock().unwrap().len();
        (n, bytes, d)
    }

    /// Drop expired objects, then least-recently-used ones until under the limit.
    pub fn evict(&self, max_bytes: u64, max_age: u64) -> usize {
        let t = now();
        let mut items: Vec<(String, u64, u64)> = self
            .index
            .lock()
            .unwrap()
            .values()
            .map(|m| (m.key.clone(), m.last_access, m.size))
            .collect();
        items.sort_by_key(|i| i.1);
        let mut total: u64 = items.iter().map(|i| i.2).sum();
        let mut removed = 0;
        for (key, last, size) in items {
            let expired = max_age > 0 && t.saturating_sub(last) > max_age;
            if expired || total > max_bytes {
                self.remove(&key);
                total = total.saturating_sub(size);
                removed += 1;
            } else {
                break; // sorted oldest-first: everything after is newer
            }
        }
        removed
    }
}
