//! HTTP routes: the embedded web UI, the JSON API and the media endpoints.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{Stream, StreamExt};
use serde_json::{json, Value};

use crate::filelist::{FileList, MOVIE_CATEGORIES};
use crate::livesubs::{parse_mkv_header, LiveCues, MkvInfo, MkvTap};
use crate::media::{preset, Caps, PRESETS};
use crate::opensubs::OpenSubs;
use crate::qbit::Credentials;
use crate::util::{file_ext, ApiError, ApiResult};
use crate::AppState;

type S = State<Arc<AppState>>;
type Q = Query<HashMap<String, String>>;

const INDEX_HTML: &str = include_str!("../public/index.html");
const APP_JS: &str = include_str!("../public/app.js");
const STYLE_CSS: &str = include_str!("../public/style.css");
const HLS_JS: &str = include_str!("../public/vendor/hls.min.js");
const ICON_PNG: &[u8] = include_bytes!("../public/icon.png");

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        // UI (TV browsers cache aggressively: always revalidate, version the script URLs per start)
        .route("/", get(index))
        .route("/index.html", get(index))
        .route("/app.js", get(|| async { asset("text/javascript", APP_JS) }))
        .route("/style.css", get(|| async { asset("text/css", STYLE_CSS) }))
        .route("/vendor/hls.min.js", get(|| async { asset("text/javascript", HLS_JS) }))
        .route("/icon.png", get(|| async { ([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "public, max-age=86400")], ICON_PNG) }))
        // API
        .route("/api/config", get(config))
        .route("/api/setup", post(setup))
        .route("/api/movies", get(movies))
        .route("/api/poster", get(poster))
        .route("/api/meta", get(meta))
        .route("/api/downloads", get(downloads))
        .route("/api/downloads/{id}", post(add_download).delete(remove_download))
        .route("/api/progress", get(get_progress))
        .route("/api/progress/{id}", axum::routing::put(put_progress))
        .route("/api/probe/{id}/{index}", get(probe))
        .route("/api/subs/{id}", get(list_subs).post(upload_sub).layer(DefaultBodyLimit::max(20 * 1024 * 1024)))
        .route("/api/subs/{id}/live", get(live_subs))
        .route("/api/subs/{id}/{key}", axum::routing::delete(remove_sub))
        .route("/api/speedtest", get(speedtest))
        .route("/api/client-log", post(client_log))
        // media
        .route("/stream/{id}/{index}", get(stream).head(stream))
        .route("/transcode/{id}/{index}", get(transcode))
        .route("/hls/{id}/{index}/{quality}/{file}", get(hls))
        .route("/subs/{id}/{key}", get(subtitle))
        .layer(middleware::from_fn_with_state(state.clone(), track_media))
        .layer(middleware::from_fn_with_state(state.clone(), common))
        .with_state(state)
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "no-cache")], body).into_response()
}

async fn index(State(st): S) -> Response {
    let html = INDEX_HTML
        .replace("app.js\"", &format!("app.js?v={}\"", st.build))
        .replace("style.css\"", &format!("style.css?v={}\"", st.build))
        .replace("hls.min.js\"", &format!("hls.min.js?v={}\"", st.build));
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-cache")], html).into_response()
}

fn remote_ip(req: &Request) -> String {
    req.extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip().to_canonical().to_string())
        .unwrap_or_else(|| "?".into())
}

/// Build id header (lets open pages reload after an update) and a request log for other devices.
async fn common(State(st): S, req: Request, next: Next) -> Response {
    let who = remote_ip(&req);
    let path = req.uri().path().to_string();
    let line = format!("{} {}", req.method(), req.uri());
    let mut res = next.run(req).await;
    if path.starts_with("/api/") {
        if let Ok(v) = HeaderValue::from_str(&st.build) {
            res.headers_mut().insert("X-Build", v);
        }
    }
    let quiet = ["/stream/", "/transcode/", "/hls/", "/api/downloads", "/api/poster"].iter().any(|p| path.starts_with(p));
    if who != "127.0.0.1" && !quiet {
        st.log_media(&format!("[http] {who} {} -> {}", line.chars().take(160).collect::<String>(), res.status().as_u16()));
    }
    res
}

// ---------- viewers: throttle downloads, pause background work, media log ----------

struct ViewerGuard {
    st: Arc<AppState>,
    counted: bool,
    log: Option<(String, Instant, u16)>,
    bytes: u64,
    complete: bool,
}

impl Drop for ViewerGuard {
    fn drop(&mut self) {
        if let Some((line, start, status)) = self.log.take() {
            let how = if self.complete { "complete" } else { "closed early" };
            self.st.log_media(&format!("{line} -> {status} {how} after {}ms ({} bytes)", start.elapsed().as_millis(), self.bytes));
        }
        if self.counted && self.st.active_streams.fetch_sub(1, Ordering::SeqCst) == 1 {
            // last viewer gone: lift the download limits a little later (players reconnect while seeking)
            let st = self.st.clone();
            let generation = st.stream_generation.load(Ordering::SeqCst);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(20)).await;
                if st.active_streams.load(Ordering::SeqCst) == 0 && st.stream_generation.load(Ordering::SeqCst) == generation {
                    st.qbit.set_throttle(false).await;
                }
            });
        }
    }
}

/// Response body that keeps a ViewerGuard alive until the viewer is done (or disconnects).
struct Tracked {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, axum::Error>> + Send>>,
    guard: ViewerGuard,
}

impl Stream for Tracked {
    type Item = Result<Bytes, axum::Error>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        let polled = self.inner.as_mut().poll_next(cx);
        match &polled {
            Poll::Ready(Some(Ok(b))) => self.guard.bytes += b.len() as u64,
            Poll::Ready(None) => self.guard.complete = true,
            _ => {}
        }
        polled
    }
}

async fn track_media(State(st): S, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let is_media = path.starts_with("/stream/") || path.starts_with("/transcode/");
    let is_viewer_traffic = is_media || path.starts_with("/hls/") || path == "/api/speedtest";
    if !is_viewer_traffic {
        return next.run(req).await;
    }
    let who = remote_ip(&req);
    let ua = req.headers().get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("-").to_string();
    // our own ffmpeg/ffprobe reading the file doesn't count as a viewer
    let internal = ua.starts_with("Lavf/");
    if !internal {
        st.active_streams.fetch_add(1, Ordering::SeqCst);
        st.stream_generation.fetch_add(1, Ordering::SeqCst);
        st.subs.pause_background(); // the disk is needed for playback now
        let qbit = st.qbit.clone();
        tokio::spawn(async move { qbit.set_throttle(true).await });
    }
    let mut log = None;
    if is_media {
        if st.seen_agents.lock().unwrap().insert(format!("{who} {ua}")) {
            st.log_media(&format!("[client] {who} {ua}"));
        }
        let range = req.headers().get(header::RANGE).and_then(|v| v.to_str().ok()).unwrap_or("-").to_string();
        let line = format!("[media] {who} {} {} range={range}", req.method(), req.uri());
        if who != "127.0.0.1" {
            st.log_media(&format!("{line} started"));
        }
        log = Some(line);
    }
    let start = Instant::now();
    let res = next.run(req).await;
    let guard = ViewerGuard {
        st: st.clone(),
        counted: !internal,
        log: log.map(|l| (l, start, res.status().as_u16())),
        bytes: 0,
        complete: false,
    };
    let (parts, body) = res.into_parts();
    let tracked = Tracked { inner: Box::pin(body.into_data_stream()), guard };
    Response::from_parts(parts, Body::from_stream(tracked))
}

// ---------- config & setup ----------

async fn config(State(st): S) -> Json<Value> {
    let s = st.settings.read().unwrap().clone();
    let filelist = st.filelist();
    Json(json!({
        "setup": filelist.is_none(),
        "mode": filelist.map(|f| f.mode()),
        "categories": MOVIE_CATEGORIES.iter().map(|(id, name)| json!({ "id": id, "name": name })).collect::<Vec<_>>(),
        "ffmpeg": st.media.ffmpeg.is_some(),
        "encoder": st.media.encoder_name(),
        "quality": PRESETS.iter().map(|(k, p)| (k.to_string(), json!(p))).collect::<serde_json::Map<_, _>>(),
        "opensubtitles": st.subs.opensubs.lock().unwrap().is_some(),
        "defaults": {
            "username": s.filelist_username,
            "qbitUrl": s.qbit_url,
            "qbitUsername": s.qbit_username,
            "hasPassword": !s.filelist_password.is_empty() || !s.filelist_passkey.is_empty(),
            "hasQbitPassword": !s.qbit_password.is_empty(),
            "opensubtitlesKey": !s.opensubtitles_api_key.is_empty(),
            "opensubtitlesUsername": s.opensubtitles_username,
        },
    }))
}

/// First-run setup and the settings screen: verify the credentials, then save them to .env and switch over
/// without a restart. Empty password fields keep the saved ones.
async fn setup(State(st): S, Json(b): Json<Value>) -> ApiResult<Json<Value>> {
    let field = |k: &str| b[k].as_str().unwrap_or("").trim().to_string();
    let current = st.settings.read().unwrap().clone();
    let keep = |new: String, old: &str| if new.is_empty() { old.to_string() } else { new };
    let username = field("username");
    let password = b["password"].as_str().unwrap_or("").to_string();
    let passkey = field("passkey");
    let (password, passkey) = if password.is_empty() && passkey.is_empty() {
        (current.filelist_password.clone(), current.filelist_passkey.clone())
    } else {
        (password, passkey)
    };
    let qbit = Credentials {
        url: keep(field("qbitUrl"), &current.qbit_url),
        username: field("qbitUsername"),
        password: keep(b["qbitPassword"].as_str().unwrap_or("").to_string(), &current.qbit_password),
    };
    let os_key = keep(field("opensubtitlesKey"), &current.opensubtitles_api_key);
    let os_user = field("opensubtitlesUsername");
    let os_pass = keep(b["opensubtitlesPassword"].as_str().unwrap_or("").to_string(), &current.opensubtitles_password);

    let client = FileList::create(&username, &password, &passkey).map_err(|e| ApiError::bad_request(e.to_string()).with(json!({ "field": "filelist" })))?;
    client.verify().await.map_err(|e| ApiError::bad_request(format!("{e:#}")).with(json!({ "field": "filelist" })))?;
    let previous = st.qbit.credentials();
    st.qbit.set_credentials(qbit.clone());
    if let Err(err) = st.qbit.json("app/version", &[]).await {
        st.qbit.set_credentials(previous);
        return Err(ApiError::bad_request(format!("{err:#}")).with(json!({ "field": "qbit" })));
    }

    let env_file = current.env_file();
    crate::config::write_env(
        &env_file,
        &[
            ("FILELIST_USERNAME", username.clone()),
            ("FILELIST_PASSWORD", password.clone()),
            ("FILELIST_PASSKEY", passkey.clone()),
            ("QBIT_URL", qbit.url.clone()),
            ("QBIT_USERNAME", qbit.username.clone()),
            ("QBIT_PASSWORD", qbit.password.clone()),
            ("OPENSUBTITLES_API_KEY", os_key.clone()),
            ("OPENSUBTITLES_USERNAME", os_user.clone()),
            ("OPENSUBTITLES_PASSWORD", os_pass.clone()),
        ],
    )
    .map_err(|e| ApiError::from(anyhow::Error::from(e)))?;
    {
        let mut s = st.settings.write().unwrap();
        s.filelist_username = username;
        s.filelist_password = password;
        s.filelist_passkey = passkey;
        s.qbit_url = qbit.url;
        s.qbit_username = qbit.username;
        s.qbit_password = qbit.password;
        s.opensubtitles_api_key = os_key.clone();
        s.opensubtitles_username = os_user.clone();
        s.opensubtitles_password = os_pass.clone();
        *st.subs.opensubs.lock().unwrap() =
            OpenSubs::new(&os_key, &os_user, &os_pass, &s.opensubtitles_languages).map(Arc::new);
    }
    *st.filelist.write().unwrap() = Some(Arc::new(client));
    st.listings.remove_prefix("");
    let lib = st.lib.clone();
    tokio::spawn(async move { let _ = lib.restore().await; });
    println!("Setup saved to {}", env_file.display());
    Ok(Json(json!({ "ok": true })))
}

fn require_filelist(st: &AppState) -> ApiResult<Arc<FileList>> {
    st.filelist().ok_or_else(|| {
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "Finish the setup first (FileList credentials).").with(json!({ "setup": true }))
    })
}

// ---------- browsing ----------

/// Listings are cached for 15 minutes: FileList allows an account only so many pages per hour. When the site
/// refuses, an older copy is better than an error.
async fn movies(State(st): S, Query(q): Q) -> ApiResult<Json<Value>> {
    let filelist = require_filelist(&st)?;
    let page: u32 = q.get("page").and_then(|p| p.parse().ok()).unwrap_or(0);
    let query = q.get("q").map(|s| s.trim().to_string()).unwrap_or_default();
    let cats: Vec<u32> = q.get("cats").map(|c| c.split(',').filter_map(|x| x.parse().ok()).collect()).unwrap_or_default();
    let key = format!("{page}|{query}|{cats:?}");
    let mut result = match st.listings.get(&key, Duration::from_secs(15 * 60)) {
        Some(hit) => hit,
        None => match filelist.browse(page, &query, &cats).await {
            Ok(fresh) => {
                st.listings.put(key, fresh.clone());
                fresh
            }
            Err(err) => st.listings.get(&key, Duration::from_secs(24 * 3600)).ok_or(err)?,
        },
    };
    for item in &mut result.items {
        item.downloaded = st.lib.has(&item.id);
    }
    Ok(Json(json!({ "page": page, "items": result.items, "hasMore": result.has_more })))
}

async fn poster(State(st): S, Query(q): Q) -> Response {
    let imdb = q.get("imdb").filter(|s| !s.is_empty()).map(String::as_str);
    let name = q.get("name").map(String::as_str).unwrap_or("");
    match st.posters.meta(imdb, name).await {
        Some(m) => {
            let mut res = Redirect::to(&m.poster).into_response();
            res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=86400"));
            res
        }
        None => (
            [(header::CONTENT_TYPE, "image/svg+xml"), (header::CACHE_CONTROL, "public, max-age=86400")],
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="400" height="600" viewBox="0 0 400 600"><rect width="400" height="600" fill="#1c1f26"/><g fill="none" stroke="#3a3f4b" stroke-width="10"><rect x="130" y="230" width="140" height="110" rx="10"/><path d="M180 260v50l40-25z" fill="#3a3f4b"/></g></svg>"##,
        )
            .into_response(),
    }
}

async fn meta(State(st): S, Query(q): Q) -> Response {
    let imdb = q.get("imdb").filter(|s| !s.is_empty()).map(String::as_str);
    let name = q.get("name").map(String::as_str).unwrap_or("");
    let body = st.posters.meta(imdb, name).await.map(|m| json!(m)).unwrap_or_else(|| json!({}));
    ([(header::CACHE_CONTROL, "public, max-age=3600")], Json(body)).into_response()
}

// ---------- downloads ----------

/// Instant: the latest snapshot from the background refresher (or the library as known from disk, right after
/// startup), never waiting for qBittorrent.
async fn downloads(State(st): S) -> Json<Value> {
    let snapshot = st.downloads.read().unwrap().clone();
    Json(Value::Array(snapshot.unwrap_or_else(|| st.lib.placeholders())))
}

/// Keeps the downloads snapshot fresh (every 2 s, or right away after a change) and runs the background work
/// that depends on it.
pub async fn refresh_downloads_forever(st: Arc<AppState>) {
    loop {
        let list = st.lib.list().await;
        if st.lib.checking_count.load(Ordering::Relaxed) > 0 {
            st.subs.pause_background(); // let qBittorrent verify at full speed
        }
        for d in &list {
            // Full extraction of embedded subtitles reads the whole movie. MKV subtitles are picked out while
            // playing instead, so only other formats get it in the background.
            let video = &d["videos"][0];
            if d["done"] == json!(true) && video["ext"] != ".mkv" {
                if let (Some(id), Some(index)) = (d["id"].as_str(), video["index"].as_u64()) {
                    st.subs.prefetch(id, index);
                }
            }
        }
        *st.downloads.write().unwrap() = Some(list);
        tokio::select! {
            _ = st.refresh_downloads.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
    }
}

async fn add_download(State(st): S, Path(id): Path<String>, body: Option<Json<Value>>) -> ApiResult<Json<Value>> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
        return Err(ApiError::bad_request("Invalid torrent id"));
    }
    let filelist = require_filelist(&st)?;
    if !st.lib.has(&id) {
        let bytes = filelist.download(&id).await?;
        let b = body.map(|Json(b)| b).unwrap_or(Value::Null);
        let name = b["name"].as_str().map(str::to_string).unwrap_or_else(|| format!("Torrent {id}"));
        let text = |k: &str| b[k].as_str().filter(|s| !s.is_empty()).map(str::to_string);
        st.lib.add(&id, &bytes, &name, text("imdb"), text("poster")).await?;
        st.refresh_downloads.notify_one();
    }
    // answer right away; the next snapshot (moments later) has the full status
    let status = st
        .downloads
        .read()
        .unwrap()
        .as_ref()
        .and_then(|l| l.iter().find(|d| d["id"] == id.as_str()).cloned())
        .or_else(|| st.lib.placeholders().into_iter().find(|d| d["id"] == id.as_str()))
        .unwrap_or_else(|| json!({ "id": id }));
    Ok(Json(status))
}

async fn remove_download(State(st): S, Path(id): Path<String>, Query(q): Q) -> ApiResult<Json<Value>> {
    let delete_files = q.get("deleteFiles").is_some_and(|v| v == "1");
    st.lib.remove(&id, delete_files).await?;
    st.media.stop_sessions(&id).await;
    st.media.forget(&id);
    st.live.forget(&id);
    st.mkv.lock().await.retain(|k, _| !k.starts_with(&format!("{id}:")));
    if delete_files {
        st.subs.remove_all(&id).await;
    }
    st.progress.lock().unwrap().remove(&id);
    st.save_progress();
    if let Some(list) = st.downloads.write().unwrap().as_mut() {
        list.retain(|d| d["id"] != id.as_str());
    }
    st.refresh_downloads.notify_one();
    Ok(Json(json!({ "ok": true })))
}

// ---------- watch progress (shared by every device: "Continue watching" and resume) ----------

async fn get_progress(State(st): S) -> Json<Value> {
    Json(Value::Object(st.progress.lock().unwrap().clone()))
}

async fn put_progress(State(st): S, Path(id): Path<String>, Json(b): Json<Value>) -> ApiResult<Json<Value>> {
    let t = b["t"].as_f64().filter(|t| t.is_finite()).ok_or_else(|| ApiError::bad_request("Invalid progress"))?;
    if !st.lib.has(&id) {
        return Err(ApiError::bad_request("Invalid progress"));
    }
    let entry = json!({
        "t": t.max(0.0).round(),
        "duration": b["duration"].as_f64().filter(|d| d.is_finite()),
        "index": b["index"].as_u64().unwrap_or(0),
        "updatedAt": crate::util::now_ms(),
    });
    st.progress.lock().unwrap().insert(id, entry.clone());
    st.save_progress();
    Ok(Json(entry))
}

// ---------- media ----------

/// Direct playback with HTTP range support, for completely downloaded files only.
async fn stream(State(st): S, Path((id, index)): Path<(String, u64)>, method: Method, headers: HeaderMap) -> ApiResult<Response> {
    let file = st.lib.file(&id, index).await.ok_or_else(|| ApiError::not_found("Not found"))?;
    if st.lib.local_path(&id, index).await.is_none() {
        return Err(still_downloading());
    }
    let total = file.size;
    let content_type = match file_ext(&file.name).as_str() {
        ".mp4" | ".m4v" | ".mov" => "video/mp4",
        ".webm" | ".mkv" => "video/webm",
        _ => "application/octet-stream",
    };
    let mut status = StatusCode::OK;
    let (mut start, mut end) = (0u64, total.saturating_sub(1));
    if let Some(range) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()).and_then(|r| r.strip_prefix("bytes=")) {
        let (a, b) = range.split_once('-').unwrap_or((range, ""));
        match (a.trim().parse::<u64>().ok(), b.trim().parse::<u64>().ok()) {
            (Some(s), e) => {
                start = s;
                if let Some(e) = e {
                    end = e.min(end);
                }
            }
            (None, Some(suffix)) => start = total.saturating_sub(suffix), // last N bytes
            _ => {}
        }
        if start > end || start >= total {
            return Ok((StatusCode::RANGE_NOT_SATISFIABLE, [(header::CONTENT_RANGE, format!("bytes */{total}"))]).into_response());
        }
        status = StatusCode::PARTIAL_CONTENT;
    }
    let mut res = Response::builder()
        .status(status)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, end - start + 1)
        .header(header::CONTENT_TYPE, content_type);
    if status == StatusCode::PARTIAL_CONTENT {
        res = res.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{total}"));
    }
    if method == Method::HEAD {
        return Ok(res.body(Body::empty()).expect("valid response"));
    }
    let is_mkv = file_ext(&file.name) == ".mkv";
    let bytes = st.lib.clone().read_range(file.clone(), start, end);
    // Pick the subtitle lines out of the bytes on their way to the player: no extra disk reads.
    let tap = if is_mkv { mkv_info(&st, &id, index, &file).await } else { None }.map(|info| {
        let live = st.live.clone();
        let prefix = format!("{id}:{index}");
        MkvTap::new(info, Box::new(move |n, s, e, text| live.add(&format!("{prefix}:{n}"), s, e, &text)))
    });
    let body = match tap {
        Some(mut tap) => Body::from_stream(bytes.map(move |chunk| {
            if let Ok(b) = &chunk {
                tap.feed(b);
            }
            chunk
        })),
        None => Body::from_stream(bytes),
    };
    Ok(res.body(body).expect("valid response"))
}

/// Matroska header (subtitle tracks) of a file, read once from its first megabytes.
async fn mkv_info(st: &AppState, id: &str, index: u64, file: &crate::library::TorrentFile) -> Option<Arc<MkvInfo>> {
    let key = format!("{id}:{index}");
    let mut cache = st.mkv.lock().await;
    if let Some(hit) = cache.get(&key) {
        return hit.clone();
    }
    let len = file.size.min(4 * 1024 * 1024);
    let mut head = Vec::with_capacity(len as usize);
    let mut stream = Box::pin(st.lib.clone().read_range(file.clone(), 0, len.saturating_sub(1)));
    let read = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(chunk) = stream.next().await {
            head.extend_from_slice(&chunk.ok()?);
        }
        Some(())
    })
    .await;
    if !matches!(read, Ok(Some(()))) {
        return None; // not downloaded yet: try again on the next request
    }
    let info = parse_mkv_header(&head).map(Arc::new);
    cache.insert(key, info.clone());
    info
}

/// Subtitle cues collected while the movie plays (see livesubs.rs), newer than `since`.
async fn live_subs(State(st): S, Path(id): Path<String>, Query(q): Q) -> ApiResult<Json<Value>> {
    let key = q.get("key").map(String::as_str).unwrap_or("");
    let mut parts = key.split(':');
    let (Some("e"), Some(video), Some(n)) = (parts.next(), parts.next().and_then(|v| v.parse::<u64>().ok()), parts.next().and_then(|n| n.parse::<usize>().ok())) else {
        return Err(ApiError::bad_request("bad key"));
    };
    let since: u64 = q.get("since").and_then(|s| s.parse().ok()).unwrap_or(0);
    let (version, cues) = st.live.since(&LiveCues::key(&id, video, n), since);
    Ok(Json(json!({
        "version": version,
        "cues": cues.into_iter().map(|(s, e, t)| json!([s, e, t])).collect::<Vec<_>>(),
        "cached": st.subs.is_cached(&id, video, n),
    })))
}

fn still_downloading() -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "This movie can be played once the download finishes.")
}

/// The file on disk; movies play only once completely downloaded.
async fn media_input(st: &AppState, id: &str, index: u64) -> ApiResult<String> {
    if st.lib.file(id, index).await.is_none() {
        return Err(ApiError::not_found("Not found"));
    }
    let path = st.lib.local_path(id, index).await.ok_or_else(still_downloading)?;
    Ok(path.to_string_lossy().into_owned())
}

async fn probe(State(st): S, Path((id, index)): Path<(String, u64)>) -> ApiResult<Json<Value>> {
    if st.media.ffprobe.is_none() {
        return Ok(Json(json!({ "duration": null })));
    }
    let input = media_input(&st, &id, index).await?;
    Ok(Json(json!(st.media.probe(&format!("{id}:{index}"), &input).await?)))
}

/// Full quality, converted for devices that can't play the file as-is (only the audio, if the device can decode
/// the video). Fragmented MP4; seeking restarts the conversion at `t`.
async fn transcode(State(st): S, Path((id, index)): Path<(String, u64)>, Query(q): Q) -> ApiResult<Response> {
    if st.media.ffmpeg.is_none() {
        return Err(ApiError::new(StatusCode::NOT_IMPLEMENTED, "ffmpeg is not installed"));
    }
    let input = media_input(&st, &id, index).await?;
    let start: f64 = q.get("t").and_then(|t| t.parse().ok()).unwrap_or(0.0f64).max(0.0);
    let audio: usize = q.get("audio").and_then(|a| a.parse().ok()).unwrap_or(0);
    let caps = Caps::parse(q.get("vc").map(String::as_str), q.get("ac").map(String::as_str));
    let stream = st.media.converted_stream(&format!("{id}:{index}"), &input, start, audio, &caps).await?;
    Ok(([(header::CONTENT_TYPE, "video/mp4"), (header::CACHE_CONTROL, "no-store")], Body::from_stream(stream)).into_response())
}

/// #5: reduced quality as HLS: the player seeks normally; segments are produced just ahead of playback.
async fn hls(State(st): S, Path((id, index, quality, file)): Path<(String, u64, String, String)>, Query(q): Q) -> ApiResult<Response> {
    let preset = preset(&quality).ok_or_else(|| ApiError::bad_request("Unknown quality"))?;
    if st.media.ffmpeg.is_none() {
        return Err(ApiError::new(StatusCode::NOT_IMPLEMENTED, "ffmpeg is not installed"));
    }
    let input = media_input(&st, &id, index).await?;
    let audio: usize = q.get("audio").and_then(|a| a.parse().ok()).unwrap_or(0);
    let file_key = format!("{id}:{index}");
    if file == "index.m3u8" {
        let probe = st.media.probe(&file_key, &input).await?;
        let duration = probe.duration.ok_or_else(|| ApiError::from(anyhow::anyhow!("Unknown duration")))?;
        let playlist = crate::media::Media::hls_playlist(duration, &format!("audio={audio}"));
        return Ok(([(header::CONTENT_TYPE, "application/vnd.apple.mpegurl"), (header::CACHE_CONTROL, "no-store")], playlist).into_response());
    }
    let n: u32 = file
        .strip_prefix("seg")
        .and_then(|r| r.strip_suffix(".ts"))
        .and_then(|r| r.parse().ok())
        .ok_or_else(|| ApiError::not_found("Not found"))?;
    let session = format!("{id}:{index}:{quality}:{audio}");
    let path = st.media.hls_segment(&session, &file_key, &input, preset, audio, n).await?;
    let bytes = tokio::fs::read(&path).await.map_err(anyhow::Error::from)?;
    Ok(([(header::CONTENT_TYPE, "video/mp2t"), (header::CACHE_CONTROL, "no-store")], bytes).into_response())
}

async fn subtitle(State(st): S, Path((id, key)): Path<(String, String)>, Query(q): Q) -> ApiResult<Response> {
    let offset: f64 = q.get("offset").and_then(|o| o.parse().ok()).unwrap_or(0.0);
    let vtt = st.subs.get(&id, &key, offset).await?;
    Ok(([(header::CONTENT_TYPE, "text/vtt; charset=utf-8")], vtt).into_response())
}

async fn list_subs(State(st): S, Path(id): Path<String>, Query(q): Q) -> Json<Value> {
    let video = q.get("video").and_then(|v| v.parse().ok());
    let snapshot = st.downloads.read().unwrap().as_ref().and_then(|l| l.iter().find(|d| d["id"] == id.as_str()).cloned());
    let status = match snapshot {
        Some(s) => s,
        None => match st.lib.list().await.into_iter().find(|d| d["id"] == id.as_str()) {
            Some(s) => s,
            None => return Json(json!([])),
        },
    };
    Json(Value::Array(st.subs.list(&id, &status, video).await))
}

async fn upload_sub(State(st): S, Path(id): Path<String>, Query(q): Q, body: Bytes) -> ApiResult<Json<Value>> {
    let name = q.get("name").cloned().unwrap_or_else(|| "subtitles.srt".into());
    let key = st.subs.upload(&id, &name, &body).await?;
    Ok(Json(json!({ "key": key })))
}

async fn remove_sub(State(st): S, Path((id, key)): Path<(String, String)>) -> ApiResult<Json<Value>> {
    st.subs.remove(&id, &key).await?;
    Ok(Json(json!({ "ok": true })))
}

/// Connection speed test for the player's automatic quality choice.
async fn speedtest(State(st): S, Query(q): Q) -> Response {
    let total: u64 = q.get("bytes").and_then(|b| b.parse().ok()).unwrap_or(8_000_000).min(50_000_000);
    let noise = st.noise.clone();
    let chunks = futures_util::stream::unfold(total, move |left| {
        let noise = noise.clone();
        async move {
            if left == 0 {
                return None;
            }
            let n = left.min(noise.len() as u64);
            Some((Ok::<_, std::io::Error>(noise.slice(0..n as usize)), left - n))
        }
    });
    Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, total)
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(chunks))
        .expect("valid response")
}

async fn client_log(State(st): S, ConnectInfo(addr): ConnectInfo<SocketAddr>, body: Bytes) -> StatusCode {
    let text: String = String::from_utf8_lossy(&body).chars().take(2000).collect();
    st.log_media(&format!("[player] {} {text}", addr.ip().to_canonical()));
    StatusCode::NO_CONTENT
}
