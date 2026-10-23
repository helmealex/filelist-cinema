//! Small shared helpers: API errors, release-name parsing, a tiny TTL cache.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use regex::Regex;
use serde_json::json;

/// Error returned by API handlers as `{ "error": "..." }` with a status code.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
    pub extra: Option<serde_json::Value>,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        ApiError { status, message: message.into(), extra: None }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }
    pub fn with(mut self, extra: serde_json::Value) -> Self {
        self.extra = Some(extra);
        self
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.status.is_server_error() {
            eprintln!("[error] {}", self.message);
        }
        let mut body = json!({ "error": self.message });
        if let (Some(serde_json::Value::Object(extra)), Some(obj)) = (self.extra, body.as_object_mut()) {
            obj.extend(extra);
        }
        (self.status, Json(body)).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        // keep the chain short and readable for the UI
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}"))
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

/// "The.Matrix.1999.1080p.BluRay.x264-GRP" -> ("The Matrix", Some("1999"))
pub fn parse_release_name(name: &str) -> (String, Option<String>) {
    static SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[._]+|\s+").unwrap());
    static WITH_YEAR: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^(.+?)\s+[(\[]?((?:19|20)\d{2})[)\]]?(?:\s|$)").unwrap());
    static NO_YEAR: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\s(?:\d{3,4}p|UHD|BluRay|BDRip|BRRip|WEB|WEBRip|HDTV|DVDRip|DVD|x264|x265|HEVC|H 26[45])\b").unwrap()
    });
    let clean = SPACES.replace_all(name, " ").trim().to_string();
    if let Some(c) = WITH_YEAR.captures(&clean) {
        return (c[1].trim().to_string(), Some(c[2].to_string()));
    }
    let title = NO_YEAR.split(&clean).next().unwrap_or(&clean).trim().to_string();
    (title, None)
}

pub fn format_bytes(bytes: u64) -> String {
    if bytes == 0 {
        return String::new();
    }
    let units = ["B", "KB", "MB", "GB", "TB"];
    let i = ((bytes as f64).ln() / 1024f64.ln()).floor().min(4.0) as usize;
    let value = bytes as f64 / 1024f64.powi(i as i32);
    if i >= 3 { format!("{value:.2} {}", units[i]) } else { format!("{value:.0} {}", units[i]) }
}

pub fn decode_html_entities(s: &str) -> String {
    s.replace("&quot;", "\"").replace("&lt;", "<").replace("&gt;", ">").replace("&#39;", "'").replace("&amp;", "&")
}

/// Base-36 timestamp, used as the UI build id (cache busting for TV browsers).
pub fn base36_now() -> String {
    let mut n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    while n > 0 {
        out.push(digits[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

/// A small thread-safe cache with per-entry time-to-live.
pub struct TtlCache<V: Clone> {
    entries: Mutex<HashMap<String, (Instant, V)>>,
}

impl<V: Clone> Default for TtlCache<V> {
    fn default() -> Self {
        TtlCache { entries: Mutex::new(HashMap::new()) }
    }
}

impl<V: Clone> TtlCache<V> {
    pub fn get(&self, key: &str, ttl: Duration) -> Option<V> {
        let entries = self.entries.lock().unwrap();
        entries.get(key).filter(|(at, _)| at.elapsed() < ttl).map(|(_, v)| v.clone())
    }
    pub fn put(&self, key: impl Into<String>, value: V) {
        let mut entries = self.entries.lock().unwrap();
        if entries.len() > 2000 {
            entries.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(3600));
        }
        entries.insert(key.into(), (Instant::now(), value));
    }
    pub fn remove_prefix(&self, prefix: &str) {
        self.entries.lock().unwrap().retain(|k, _| !k.starts_with(prefix));
    }
}

pub fn file_ext(name: &str) -> String {
    std::path::Path::new(name)
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
        .unwrap_or_default()
}

pub fn base_name(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_names() {
        assert_eq!(parse_release_name("The.Matrix.1999.1080p.BluRay.x264-GRP"), ("The Matrix".into(), Some("1999".into())));
        assert_eq!(parse_release_name("Blade Runner 2049 2017 2160p"), ("Blade Runner".into(), Some("2049".into())));
        assert_eq!(parse_release_name("Some.Show.1080p.WEB"), ("Some Show".into(), None));
    }

    #[test]
    fn bytes() {
        assert_eq!(format_bytes(14_692_431_750), "13.68 GB");
        assert_eq!(format_bytes(0), "");
    }
}
