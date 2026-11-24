use std::{
    io::{self, SeekFrom},
    net::SocketAddr,
    sync::Arc,
};

use axum::{
    body::{Body, Bytes},
    extract::{ConnectInfo, Request, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    Router,
};
use futures_util::{stream, StreamExt, TryStreamExt};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    sync::watch,
};
use tracing::{info, warn};

use crate::{
    stats::Kind,
    store::{make_key, now, Download, Meta, Progress, Registered},
    App,
};

const LOOP_HEADER: &str = "x-winupdate-cache";
/// A range starting further than this beyond the already-downloaded part is
/// proxied straight to the internet instead of waiting for the fill.
const NEAR_BYTES: u64 = 8 * 1024 * 1024;
const REPORT_STEP: u64 = 256 * 1024;
const CHUNK: u64 = 256 * 1024;
/// Small objects (indexes, .cab manifests) can change in place: re-check them hourly.
const SMALL_OBJECT: u64 = 4 * 1024 * 1024;
const REVALIDATE_SECS: u64 = 3600;

pub fn router(app: Arc<App>) -> Router {
    Router::new().fallback(handle).with_state(app)
}

struct Ctx {
    app: Arc<App>,
    client: String,
    host: String,     // as sent by the client (may contain a port)
    hostname: String, // without port
    path: String,
    pq: String, // path + query
    method: Method,
    headers: HeaderMap,
    key: String,
}

impl Ctx {
    fn url(&self) -> String {
        format!("http://{}{}", self.host, self.pq)
    }
    fn range(&self) -> Option<&str> {
        self.headers.get(header::RANGE).and_then(|v| v.to_str().ok())
    }
    fn record(&self, kind: Kind, status: u16, bytes: u64) {
        self.app
            .stats
            .record(&self.client, &self.hostname, &self.path, kind, status, bytes);
    }
    fn upstream(&self, method: Method) -> reqwest::RequestBuilder {
        let mut rb = self.app.http.request(method, self.url()).header(LOOP_HEADER, "1");
        if let Some(ua) = self.headers.get(header::USER_AGENT) {
            rb = rb.header(header::USER_AGENT, ua.clone());
        }
        rb
    }
}

fn plain(status: StatusCode, msg: &'static str) -> Response {
    (status, msg).into_response()
}

fn build(status: StatusCode, headers: HeaderMap, body: Body) -> Response {
    let mut r = Response::new(body);
    *r.status_mut() = status;
    *r.headers_mut() = headers;
    r
}

async fn handle(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
) -> Response {
    let method = req.method().clone();
    if method != Method::GET && method != Method::HEAD {
        return plain(StatusCode::METHOD_NOT_ALLOWED, "only GET and HEAD are supported");
    }
    let headers = req.headers().clone();
    if headers.contains_key(LOOP_HEADER) {
        return plain(
            StatusCode::LOOP_DETECTED,
            "request loop: the cache is resolving upstream hosts through the DNS that redirects to it",
        );
    }
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_ascii_lowercase());
    let Some(host) = host else {
        return plain(StatusCode::BAD_REQUEST, "missing Host header");
    };
    let hostname = host.split(':').next().unwrap_or_default().to_string();
    if !app.cfg.host_allowed(&hostname) {
        return plain(StatusCode::FORBIDDEN, "host is not served by this cache");
    }
    let uri = req.uri();
    let path = uri.path().to_string();
    let pq = uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let key = make_key(
        &hostname,
        &path,
        if app.cfg.key_includes_query { uri.query() } else { None },
    );
    let ctx = Ctx {
        app,
        client: peer.ip().to_string(),
        host,
        hostname,
        path,
        pq,
        method,
        headers,
        key,
    };
    serve(&ctx).await
}

async fn serve(ctx: &Ctx) -> Response {
    let app = &ctx.app;
    if let Some(meta) = app.store.lookup(&ctx.key) {
        if fresh(ctx, &meta).await {
            return serve_cached(ctx, meta).await;
        }
        app.store.remove(&ctx.key);
    }
    if ctx.method == Method::HEAD {
        return bypass(ctx).await;
    }
    if let Some(d) = app.store.get_inflight(&ctx.key) {
        return serve_download(ctx, d, true).await;
    }
    miss(ctx).await
}

/// Cheap upstream check for small, possibly mutable objects.
async fn fresh(ctx: &Ctx, m: &Meta) -> bool {
    if m.size >= SMALL_OBJECT || now().saturating_sub(m.validated) < REVALIDATE_SECS {
        return true;
    }
    match ctx.upstream(Method::HEAD).send().await {
        Ok(r) if r.status() == StatusCode::OK => {
            let h = r.headers();
            let get = |n: HeaderName| h.get(n).and_then(|v| v.to_str().ok()).map(String::from);
            let len = get(header::CONTENT_LENGTH).and_then(|s| s.parse::<u64>().ok());
            let same = get(header::ETAG) == m.etag
                && get(header::LAST_MODIFIED) == m.last_modified
                && len == Some(m.size);
            if same {
                ctx.app.store.touch_validated(&m.key);
            }
            same
        }
        _ => true, // upstream unreachable: serving a possibly stale copy beats failing
    }
}

// ---------- byte ranges ----------

enum ByteRange {
    Full,
    Part(u64, u64), // inclusive
    Invalid,
}

fn parse_range(h: Option<&str>, size: u64) -> ByteRange {
    let Some(h) = h else { return ByteRange::Full };
    let Some(spec) = h.trim().strip_prefix("bytes=") else { return ByteRange::Full };
    if spec.contains(',') {
        return ByteRange::Full; // multi-range: answer with the whole object
    }
    let Some((a, b)) = spec.split_once('-') else { return ByteRange::Full };
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() {
        let Ok(n) = b.parse::<u64>() else { return ByteRange::Full };
        if n == 0 || size == 0 {
            return ByteRange::Invalid;
        }
        let n = n.min(size);
        return ByteRange::Part(size - n, size - 1);
    }
    let Ok(s) = a.parse::<u64>() else { return ByteRange::Full };
    if s >= size {
        return ByteRange::Invalid;
    }
    let e = if b.is_empty() {
        size - 1
    } else {
        match b.parse::<u64>() {
            Ok(e) => e.min(size - 1),
            Err(_) => return ByteRange::Full,
        }
    };
    if e < s {
        return ByteRange::Full;
    }
    ByteRange::Part(s, e)
}

struct Plan {
    status: StatusCode,
    headers: HeaderMap,
    start: u64,
    len: u64,
}

fn make_plan(meta: &Meta, range: Option<&str>) -> Result<Plan, Response> {
    let (status, start, len) = match parse_range(range, meta.size) {
        ByteRange::Full => (StatusCode::OK, 0, meta.size),
        ByteRange::Part(s, e) => (StatusCode::PARTIAL_CONTENT, s, e - s + 1),
        ByteRange::Invalid => {
            let mut h = HeaderMap::new();
            if let Ok(v) = HeaderValue::from_str(&format!("bytes */{}", meta.size)) {
                h.insert(header::CONTENT_RANGE, v);
            }
            return Err(build(StatusCode::RANGE_NOT_SATISFIABLE, h, Body::empty()));
        }
    };
    let mut h = HeaderMap::new();
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    let ct = meta.content_type.as_deref().unwrap_or("application/octet-stream");
    if let Ok(v) = HeaderValue::from_str(ct) {
        h.insert(header::CONTENT_TYPE, v);
    }
    if let Some(v) = meta.etag.as_deref().and_then(|s| HeaderValue::from_str(s).ok()) {
        h.insert(header::ETAG, v);
    }
    if let Some(v) = meta.last_modified.as_deref().and_then(|s| HeaderValue::from_str(s).ok()) {
        h.insert(header::LAST_MODIFIED, v);
    }
    if status == StatusCode::PARTIAL_CONTENT {
        if let Ok(v) = HeaderValue::from_str(&format!("bytes {}-{}/{}", start, start + len - 1, meta.size)) {
            h.insert(header::CONTENT_RANGE, v);
        }
    }
    Ok(Plan { status, headers: h, start, len })
}

// ---------- serving from disk ----------

/// Streams `[start, end)` of a file. If `rx` is set the file is still being
/// written, so the stream follows the download progress.
fn file_body(file: File, start: u64, end: u64, rx: Option<watch::Receiver<Progress>>) -> Body {
    struct St {
        file: File,
        pos: u64,
        end: u64,
        rx: Option<watch::Receiver<Progress>>,
    }
    let state = St { file, pos: start, end, rx };
    let s = stream::try_unfold(state, |mut st| async move {
        if st.pos >= st.end {
            return Ok(None);
        }
        let avail = match st.rx.as_mut() {
            None => st.end - st.pos,
            Some(rx) => loop {
                let p = *rx.borrow_and_update();
                match p {
                    Progress::Done => break st.end - st.pos,
                    Progress::Failed => {
                        return Err(io::Error::new(io::ErrorKind::Other, "upstream download failed"))
                    }
                    Progress::Running(done) => {
                        if done > st.pos {
                            break done.min(st.end) - st.pos;
                        }
                        if rx.changed().await.is_err() {
                            return Err(io::Error::new(io::ErrorKind::Other, "download aborted"));
                        }
                    }
                }
            },
        };
        let n = avail.min(CHUNK) as usize;
        let mut buf = vec![0u8; n];
        st.file.read_exact(&mut buf).await?;
        st.pos += n as u64;
        Ok(Some((Bytes::from(buf), st)))
    });
    Body::from_stream(s)
}

async fn serve_cached(ctx: &Ctx, meta: Meta) -> Response {
    let plan = match make_plan(&meta, ctx.range()) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let mut headers = plan.headers;
    headers.insert("x-cache", HeaderValue::from_static("HIT"));
    if ctx.method == Method::HEAD {
        ctx.record(Kind::Hit, plan.status.as_u16(), 0);
        return build(plan.status, headers, Body::empty());
    }
    let mut file = match File::open(ctx.app.store.bin_path(&meta.key)).await {
        Ok(f) => f,
        Err(_) => {
            ctx.app.store.remove(&meta.key);
            return bypass(ctx).await;
        }
    };
    if file.seek(SeekFrom::Start(plan.start)).await.is_err() {
        return plain(StatusCode::INTERNAL_SERVER_ERROR, "cache read error");
    }
    ctx.record(Kind::Hit, plan.status.as_u16(), plan.len);
    build(plan.status, headers, file_body(file, plan.start, plan.start + plan.len, None))
}

/// Serve a client from an object that is still being downloaded.
async fn serve_download(ctx: &Ctx, d: Arc<Download>, joined: bool) -> Response {
    let plan = match make_plan(&d.meta, ctx.range()) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let p = *d.rx.borrow();
    let progress = match p {
        Progress::Running(n) => n,
        Progress::Done => d.meta.size,
        Progress::Failed => return bypass(ctx).await,
    };
    if plan.start > progress + NEAR_BYTES {
        return bypass(ctx).await;
    }
    let mut file = match File::open(ctx.app.store.bin_path(&d.meta.key)).await {
        Ok(f) => f,
        Err(_) => return bypass(ctx).await,
    };
    if file.seek(SeekFrom::Start(plan.start)).await.is_err() {
        return plain(StatusCode::INTERNAL_SERVER_ERROR, "cache read error");
    }
    let (kind, tag) = if joined { (Kind::Hit, "JOIN") } else { (Kind::Miss, "MISS") };
    ctx.record(kind, plan.status.as_u16(), plan.len);
    let mut headers = plan.headers;
    headers.insert("x-cache", HeaderValue::from_static(tag));
    build(
        plan.status,
        headers,
        file_body(file, plan.start, plan.start + plan.len, Some(d.rx.clone())),
    )
}

// ---------- filling the cache ----------

async fn miss(ctx: &Ctx) -> Response {
    let app = &ctx.app;
    let resp = match ctx.upstream(Method::GET).send().await {
        Ok(r) => r,
        Err(e) => {
            warn!("upstream request failed for {}{}: {e}", ctx.hostname, ctx.path);
            return plain(StatusCode::BAD_GATEWAY, "upstream unreachable");
        }
    };
    let size = resp.content_length();
    let cacheable = resp.status() == StatusCode::OK
        && size.map_or(false, |s| s > 0 && s <= app.cfg.max_cache_bytes);
    if !cacheable {
        return fallback(ctx, resp).await;
    }
    let size = size.unwrap_or(0);
    let hv = |n: HeaderName| resp.headers().get(n).and_then(|v| v.to_str().ok()).map(String::from);
    let t = now();
    let meta = Meta {
        key: ctx.key.clone(),
        host: ctx.hostname.clone(),
        path: ctx.path.clone(),
        size,
        content_type: hv(header::CONTENT_TYPE),
        etag: hv(header::ETAG),
        last_modified: hv(header::LAST_MODIFIED),
        created: t,
        last_access: t,
        hits: 0,
        validated: t,
    };
    match app.store.register(meta.clone()) {
        Err(e) => {
            warn!("cache directory error: {e}");
            fallback(ctx, resp).await
        }
        Ok(Registered::Complete) => {
            drop(resp);
            match app.store.lookup(&ctx.key) {
                Some(m) => serve_cached(ctx, m).await,
                None => bypass(ctx).await,
            }
        }
        Ok(Registered::Existing(d)) => {
            drop(resp); // someone else started the same download a moment ago
            serve_download(ctx, d, true).await
        }
        Ok(Registered::New(d, tx)) => {
            let file = match File::create(app.store.bin_path(&ctx.key)).await {
                Ok(f) => f,
                Err(e) => {
                    warn!("cannot create cache file: {e}");
                    app.store.fail(&ctx.key);
                    tx.send_replace(Progress::Failed);
                    return fallback(ctx, resp).await;
                }
            };
            tokio::spawn(download_task(app.clone(), meta, resp, file, tx));
            serve_download(ctx, d, false).await
        }
    }
}

/// Runs to completion even if the requesting client goes away.
async fn download_task(
    app: Arc<App>,
    meta: Meta,
    resp: reqwest::Response,
    mut file: File,
    tx: watch::Sender<Progress>,
) {
    let key = meta.key.clone();
    let size = meta.size;
    let label = format!("{}{}", meta.host, meta.path);
    let result: Result<(), String> = async {
        let mut body = Box::pin(resp.bytes_stream());
        let (mut written, mut reported) = (0u64, 0u64);
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|e| e.to_string())?;
            written += chunk.len() as u64;
            if written > size {
                return Err("upstream sent more data than Content-Length".to_string());
            }
            file.write_all(&chunk).await.map_err(|e| e.to_string())?;
            app.stats.add_upstream(chunk.len() as u64);
            if written - reported >= REPORT_STEP {
                file.flush().await.map_err(|e| e.to_string())?;
                reported = written;
                tx.send_replace(Progress::Running(written));
            }
        }
        file.flush().await.map_err(|e| e.to_string())?;
        if written != size {
            return Err(format!("short download: {written} of {size} bytes"));
        }
        Ok::<(), String>(())
    }
    .await;
    drop(file);
    match result {
        Ok(()) => {
            app.store.finish(meta);
            info!("cached {label} ({size} bytes)");
            tx.send_replace(Progress::Done);
        }
        Err(e) => {
            warn!("download of {label} failed: {e}");
            app.store.fail(&key);
            tx.send_replace(Progress::Failed);
        }
    }
}

// ---------- pass-through ----------

fn is_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "transfer-encoding"
            | "te"
            | "trailer"
            | "upgrade"
            | "proxy-authenticate"
            | "proxy-authorization"
    )
}

/// Response we decided not to cache. If the client asked for a range we have
/// to re-ask upstream with that range instead of forwarding a full body.
async fn fallback(ctx: &Ctx, resp: reqwest::Response) -> Response {
    if resp.status() == StatusCode::OK && ctx.range().is_some() {
        drop(resp);
        bypass(ctx).await
    } else {
        passthrough(ctx, resp)
    }
}

async fn bypass(ctx: &Ctx) -> Response {
    let mut rb = ctx.upstream(ctx.method.clone());
    for name in [header::RANGE, header::IF_RANGE, header::IF_NONE_MATCH, header::IF_MODIFIED_SINCE] {
        if let Some(v) = ctx.headers.get(&name) {
            rb = rb.header(name.clone(), v.clone());
        }
    }
    match rb.send().await {
        Ok(r) => passthrough(ctx, r),
        Err(e) => {
            warn!("upstream request failed for {}{}: {e}", ctx.hostname, ctx.path);
            plain(StatusCode::BAD_GATEWAY, "upstream unreachable")
        }
    }
}

fn passthrough(ctx: &Ctx, resp: reqwest::Response) -> Response {
    let status = resp.status();
    let mut headers = HeaderMap::new();
    for (k, v) in resp.headers() {
        if !is_hop(k) {
            headers.append(k.clone(), v.clone());
        }
    }
    headers.insert("x-cache", HeaderValue::from_static("BYPASS"));
    ctx.record(Kind::Bypass, status.as_u16(), resp.content_length().unwrap_or(0));
    let body = if ctx.method == Method::HEAD {
        Body::empty()
    } else {
        let app = ctx.app.clone();
        Body::from_stream(
            resp.bytes_stream()
                .inspect_ok(move |b| app.stats.add_upstream(b.len() as u64)),
        )
    };
    build(status, headers, body)
}
