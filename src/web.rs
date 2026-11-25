use std::{sync::Arc, time::Duration};

use axum::{
    extract::{Path, Query, Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post, put},
    Json, Router,
};
use rand::RngCore;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{settings::Settings, store::now, App};

const INDEX: &str = include_str!("../static/index.html");

pub fn router(app: Arc<App>) -> Router {
    let protected = Router::new()
        .route("/api/me", get(me))
        .route("/api/stats", get(stats))
        .route("/api/config", get(get_config))
        .route("/api/devices", get(devices))
        .route("/api/requests", get(requests))
        .route("/api/settings", get(get_settings).put(put_settings))
        .route("/api/devices/:ip", put(put_device).delete(delete_device))
        .route("/api/objects", get(objects))
        .route("/api/objects/:key", delete(delete_object))
        .route("/api/purge", post(purge))
        .route("/api/logout", post(logout))
        .route_layer(middleware::from_fn_with_state(app.clone(), require_auth));
    Router::new()
        .route("/", get(|| async { Html(INDEX) }))
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/login", post(login))
        .merge(protected)
        .with_state(app)
}

// ---------- auth ----------

fn session_token(h: &HeaderMap) -> Option<String> {
    let cookies = h.get(header::COOKIE)?.to_str().ok()?;
    cookies
        .split(';')
        .find_map(|p| p.trim().strip_prefix("wuc_session="))
        .map(String::from)
}

fn authed(app: &App, h: &HeaderMap) -> bool {
    let Some(tok) = session_token(h) else { return false };
    let mut sessions = app.sessions.lock().unwrap();
    let exp = sessions.get(&tok).copied();
    match exp {
        Some(e) if e > now() => true,
        Some(_) => {
            sessions.remove(&tok);
            false
        }
        None => false,
    }
}

async fn require_auth(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    if authed(&app, req.headers()) {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, Json(json!({ "error": "unauthorized" }))).into_response()
    }
}

/// Constant-time string comparison (compares fixed-length digests).
fn ct_eq(a: &str, b: &str) -> bool {
    let (x, y) = (Sha256::digest(a.as_bytes()), Sha256::digest(b.as_bytes()));
    x.iter().zip(y.iter()).fold(0u8, |acc, (p, q)| acc | (p ^ q)) == 0
}

#[derive(Deserialize)]
struct LoginReq {
    username: String,
    password: String,
}

async fn login(State(app): State<Arc<App>>, Json(req): Json<LoginReq>) -> Response {
    {
        let mut fails = app.login_fails.lock().unwrap();
        let t = now();
        while fails.front().map_or(false, |&x| t.saturating_sub(x) > 60) {
            fails.pop_front();
        }
        if fails.len() >= 10 {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({ "error": "Too many failed attempts. Try again in a minute." })),
            )
                .into_response();
        }
    }
    let ok = ct_eq(&req.username, &app.cfg.admin_user) & ct_eq(&req.password, &app.cfg.admin_password);
    if !ok {
        app.login_fails.lock().unwrap().push_back(now());
        tokio::time::sleep(Duration::from_millis(700)).await;
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "Wrong username or password." })),
        )
            .into_response();
    }
    let mut raw = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut raw);
    let token = hex::encode(raw);
    {
        let t = now();
        let mut s = app.sessions.lock().unwrap();
        s.retain(|_, exp| *exp > t);
        s.insert(token.clone(), t + app.cfg.session_secs);
    }
    let cookie = format!(
        "wuc_session={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
        app.cfg.session_secs
    );
    let mut res = Json(json!({ "ok": true })).into_response();
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        res.headers_mut().insert(header::SET_COOKIE, v);
    }
    res
}

async fn logout(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    if let Some(tok) = session_token(&headers) {
        app.sessions.lock().unwrap().remove(&tok);
    }
    let mut res = Json(json!({ "ok": true })).into_response();
    res.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("wuc_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0"),
    );
    res
}

// ---------- API ----------

async fn me(State(app): State<Arc<App>>) -> Json<Value> {
    Json(json!({ "user": app.cfg.admin_user }))
}

async fn stats(State(app): State<Arc<App>>) -> Json<Value> {
    let (objects, bytes, downloading) = app.store.summary();
    Json(json!({
        "stats": app.stats.snapshot(app.settings.get().dashboard_limit),
        "names": app.names.all(),
        "cache": {
            "objects": objects,
            "bytes": bytes,
            "downloading": downloading,
            "max_bytes": app.cfg.max_cache_bytes,
        }
    }))
}

async fn get_config(State(app): State<Arc<App>>) -> Json<Value> {
    Json(json!({
        "domains": app.cfg.allowed_hosts,
        "default_password": app.cfg.default_password,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// Devices seen so far plus any named ones that haven't connected yet.
#[derive(Deserialize)]
struct PageQuery {
    q: Option<String>,
    page: Option<usize>,
}

/// (page used, start index, end index)
fn page_bounds(total: usize, page: usize, size: usize) -> (usize, usize, usize) {
    let size = size.max(1);
    let pages = ((total + size - 1) / size).max(1);
    let page = page.clamp(1, pages);
    let start = (page - 1) * size;
    (page, start, (start + size).min(total))
}

async fn devices(State(app): State<Arc<App>>, Query(q): Query<PageQuery>) -> Json<Value> {
    let names = app.names.all();
    let mut rows: Vec<Value> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for c in app.stats.clients() {
        seen.insert(c.ip.clone());
        rows.push(json!({
            "ip": c.ip,
            "name": names.get(&c.ip).cloned().unwrap_or_default(),
            "requests": c.requests,
            "bytes": c.bytes,
            "last_seen": c.last_seen,
        }));
    }
    for (ip, name) in &names {
        if !seen.contains(ip) {
            rows.push(json!({ "ip": ip, "name": name, "requests": 0, "bytes": 0, "last_seen": 0 }));
        }
    }
    rows.sort_by(|a, b| {
        b["last_seen"].as_u64().cmp(&a["last_seen"].as_u64()).then_with(|| {
            a["ip"].as_str().cmp(&b["ip"].as_str())
        })
    });
    let size = app.settings.get().page_size;
    let total = rows.len();
    let (page, start, end) = page_bounds(total, q.page.unwrap_or(1), size);
    Json(json!({ "devices": rows[start..end].to_vec(), "total": total, "page": page, "page_size": size }))
}

async fn requests(State(app): State<Arc<App>>, Query(q): Query<PageQuery>) -> Json<Value> {
    let size = app.settings.get().page_size;
    let (rows, total, page) = app.stats.requests(q.page.unwrap_or(1), size);
    Json(json!({
        "requests": rows,
        "total": total,
        "page": page,
        "page_size": size,
        "names": app.names.all(),
    }))
}

async fn get_settings(State(app): State<Arc<App>>) -> Json<Settings> {
    Json(app.settings.get())
}

async fn put_settings(State(app): State<Arc<App>>, Json(req): Json<Settings>) -> Response {
    match app.settings.set(req) {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize)]
struct NameReq {
    name: String,
}

async fn put_device(State(app): State<Arc<App>>, Path(ip): Path<String>, Json(req): Json<NameReq>) -> Response {
    match app.names.set(&ip, &req.name) {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

async fn delete_device(State(app): State<Arc<App>>, Path(ip): Path<String>) -> Response {
    match app.names.remove(&ip) {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

async fn objects(State(app): State<Arc<App>>, Query(q): Query<PageQuery>) -> Json<Value> {
    let size = app.settings.get().page_size;
    let (list, total, page) =
        app.store
            .list_page(q.q.as_deref().unwrap_or(""), q.page.unwrap_or(1), size);
    Json(json!({ "objects": list, "total": total, "page": page, "page_size": size }))
}

async fn delete_object(State(app): State<Arc<App>>, Path(key): Path<String>) -> Response {
    if key.len() != 64 || !key.chars().all(|c| c.is_ascii_hexdigit()) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid key" }))).into_response();
    }
    Json(json!({ "removed": app.store.remove(&key) })).into_response()
}

async fn purge(State(app): State<Arc<App>>) -> Json<Value> {
    let (n, bytes) = app.store.purge_all();
    Json(json!({ "removed": n, "bytes": bytes }))
}
