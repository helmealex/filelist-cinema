//! qBittorrent Web API client (FileList only accepts whitelisted clients, so all downloading happens there).

use std::path::PathBuf;
use std::sync::{Mutex as StdMutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use reqwest::multipart;
use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::util::TtlCache;

#[derive(Clone)]
pub struct Credentials {
    pub url: String,
    pub username: String,
    pub password: String,
}

/// Request body; rebuilt for each attempt (a request may be retried after logging in or starting qBittorrent).
pub enum Body {
    None,
    Form(Vec<(String, String)>),
    Torrent { bytes: Vec<u8>, filename: String, fields: Vec<(String, String)> },
}

pub struct Qbit {
    creds: RwLock<Credentials>,
    exe: Option<PathBuf>,
    sid: StdMutex<Option<String>>,
    http: reqwest::Client,
    last_launch: StdMutex<Option<Instant>>,
    cache: TtlCache<Value>,
}

impl Qbit {
    pub fn new(creds: Credentials, exe: Option<PathBuf>) -> Qbit {
        Qbit {
            creds: RwLock::new(Credentials { url: creds.url.trim_end_matches('/').to_string(), ..creds }),
            exe,
            sid: StdMutex::new(None),
            http: reqwest::Client::builder().timeout(Duration::from_secs(60)).build().expect("http client"),
            last_launch: StdMutex::new(None),
            cache: TtlCache::default(),
        }
    }

    pub fn credentials(&self) -> Credentials {
        self.creds.read().unwrap().clone()
    }

    pub fn set_credentials(&self, creds: Credentials) {
        *self.creds.write().unwrap() = Credentials { url: creds.url.trim_end_matches('/').to_string(), ..creds };
        *self.sid.lock().unwrap() = None;
    }

    fn base(&self) -> String {
        self.creds.read().unwrap().url.clone()
    }

    async fn send(&self, endpoint: &str, query: &[(&str, &str)], body: &Body) -> reqwest::Result<reqwest::Response> {
        let url = format!("{}/api/v2/{endpoint}", self.base());
        let mut req = match body {
            Body::None => self.http.get(&url),
            Body::Form(fields) => self.http.post(&url).form(fields),
            Body::Torrent { bytes, filename, fields } => {
                let mut form = multipart::Form::new().part(
                    "torrents",
                    multipart::Part::bytes(bytes.clone())
                        .file_name(filename.clone())
                        .mime_str("application/x-bittorrent")
                        .expect("mime"),
                );
                for (k, v) in fields {
                    form = form.text(k.clone(), v.clone());
                }
                self.http.post(&url).multipart(form)
            }
        };
        if !query.is_empty() {
            req = req.query(query);
        }
        if let Some(sid) = self.sid.lock().unwrap().clone() {
            req = req.header("Cookie", sid);
        }
        req.send().await
    }

    pub async fn request(&self, endpoint: &str, query: &[(&str, &str)], body: Body) -> Result<reqwest::Response> {
        let mut res = match self.send(endpoint, query, &body).await {
            Ok(r) => r,
            Err(_) => {
                self.launch().await;
                self.send(endpoint, query, &body).await.with_context(|| {
                    format!("qBittorrent is not reachable at {}. Start qBittorrent with its Web UI enabled.", self.base())
                })?
            }
        };
        if res.status() == StatusCode::FORBIDDEN || res.status() == StatusCode::UNAUTHORIZED {
            self.login().await?;
            res = self.send(endpoint, query, &body).await?;
        }
        if !res.status().is_success() {
            let status = res.status();
            bail!("qBittorrent {endpoint}: HTTP {status} {}", res.text().await.unwrap_or_default());
        }
        Ok(res)
    }

    pub async fn json(&self, endpoint: &str, query: &[(&str, &str)]) -> Result<Value> {
        Ok(self.request(endpoint, query, Body::None).await?.json().await?)
    }

    pub async fn post(&self, endpoint: &str, fields: &[(&str, &str)]) -> Result<()> {
        let form = fields.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        self.request(endpoint, &[], Body::Form(form)).await.map(|_| ())
    }

    async fn login(&self) -> Result<()> {
        let creds = self.credentials();
        if creds.username.is_empty() {
            bail!("qBittorrent requires a login: set the qBittorrent username and password in the setup screen (.env)");
        }
        let res = self
            .http
            .post(format!("{}/api/v2/auth/login", creds.url))
            .form(&[("username", creds.username.as_str()), ("password", creds.password.as_str())])
            .send()
            .await?;
        let sid = res
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find(|c| c.starts_with("SID="))
            .map(|c| c.split(';').next().unwrap_or(c).to_string());
        match sid {
            Some(sid) if res.status().is_success() => {
                *self.sid.lock().unwrap() = Some(sid);
                Ok(())
            }
            _ => bail!("qBittorrent login failed: check the qBittorrent username and password"),
        }
    }

    /// Start qBittorrent if it is installed but not running.
    async fn launch(&self) {
        let Some(exe) = self.exe.clone() else { return };
        {
            let mut last = self.last_launch.lock().unwrap();
            if last.is_some_and(|t| t.elapsed() < Duration::from_secs(30)) {
                return;
            }
            *last = Some(Instant::now());
        }
        println!("[qbit] not reachable, starting {}", exe.display());
        let _ = crate::config::spawn_detached(&exe);
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if self.http.get(format!("{}/api/v2/app/version", self.base())).send().await.is_ok() {
                return;
            }
        }
    }

    // ---------- queries (briefly cached: the UI polls every 2 s, streams check pieces every second) ----------

    pub async fn infos(&self, hashes: &[String]) -> Result<Vec<Value>> {
        if hashes.is_empty() {
            return Ok(vec![]);
        }
        let joined = hashes.join("|");
        Ok(self.json("torrents/info", &[("hashes", &joined)]).await?.as_array().cloned().unwrap_or_default())
    }

    pub async fn info(&self, hash: &str) -> Result<Option<Value>> {
        let key = format!("info:{hash}");
        if let Some(v) = self.cache.get(&key, Duration::from_secs(1)) {
            return Ok(Some(v).filter(|v| !v.is_null()));
        }
        let info = self.infos(&[hash.to_string()]).await?.into_iter().next().unwrap_or(Value::Null);
        self.cache.put(key, info.clone());
        Ok(Some(info).filter(|v| !v.is_null()))
    }

    pub async fn files(&self, hash: &str) -> Result<Vec<Value>> {
        self.cached_json(&format!("files:{hash}"), Duration::from_secs(2), "torrents/files", hash).await
    }

    pub async fn piece_states(&self, hash: &str) -> Result<Vec<Value>> {
        self.cached_json(&format!("pieces:{hash}"), Duration::from_secs(1), "torrents/pieceStates", hash).await
    }

    pub async fn trackers(&self, hash: &str) -> Result<Vec<Value>> {
        self.cached_json(&format!("trackers:{hash}"), Duration::from_secs(10), "torrents/trackers", hash).await
    }

    async fn cached_json(&self, key: &str, ttl: Duration, endpoint: &str, hash: &str) -> Result<Vec<Value>> {
        if let Some(v) = self.cache.get(key, ttl) {
            return Ok(v.as_array().cloned().unwrap_or_default());
        }
        let v = self.json(endpoint, &[("hash", hash)]).await?;
        self.cache.put(key, v.clone());
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    pub fn forget(&self, hash: &str) {
        for prefix in ["info:", "files:", "pieces:", "trackers:"] {
            self.cache.remove_prefix(&format!("{prefix}{hash}"));
        }
    }

    pub async fn add_torrent(&self, bytes: &[u8], filename: &str, fields: Vec<(String, String)>) -> Result<()> {
        let body = Body::Torrent { bytes: bytes.to_vec(), filename: filename.to_string(), fields };
        self.request("torrents/add", &[], body).await.map(|_| ())
    }

    // ---------- settings ----------

    /// Settings that keep big downloads healthy on a hard drive (applied once at startup).
    pub async fn ensure_preferences(&self) {
        let Ok(prefs) = self.json("app/preferences", &[]).await else { return };
        // Pre-allocated files are written into one contiguous block instead of being scattered across the disk
        // in download order, which made reading a movie ~4x slower than the drive can do.
        if prefs["preallocate_all"] == json!(false) {
            if self.set_preferences(json!({ "preallocate_all": true })).await.is_ok() {
                println!("[qbit] enabled 'Pre-allocate disk space for all files' (less fragmentation on the hard drive)");
            }
        }
    }

    pub async fn set_preferences(&self, prefs: Value) -> Result<()> {
        self.post("app/setPreferences", &[("json", &prefs.to_string())]).await
    }

    // ---------- holding downloads while a video plays ----------

    /// Stop torrents (qBittorrent 5 calls it "stop"; 4.x "pause").
    pub async fn stop(&self, hashes: &[String]) -> Result<()> {
        let hashes = hashes.join("|");
        if self.post("torrents/stop", &[("hashes", &hashes)]).await.is_err() {
            self.post("torrents/pause", &[("hashes", &hashes)]).await?;
        }
        Ok(())
    }

    pub async fn start(&self, hashes: &[String]) -> Result<()> {
        let hashes = hashes.join("|");
        if self.post("torrents/start", &[("hashes", &hashes)]).await.is_err() {
            self.post("torrents/resume", &[("hashes", &hashes)]).await?;
        }
        Ok(())
    }

    /// Older versions switched qBittorrent to alternative speed limits while streaming and remembered the user's
    /// own limits in this file. If one is left over (the app was closed mid-stream), put things back.
    pub async fn undo_old_throttle(&self, marker: &std::path::Path) {
        let Ok(text) = std::fs::read_to_string(marker) else { return };
        let Ok(saved) = serde_json::from_str::<Value>(&text) else { return };
        let restore = async {
            let alt_mode = self.request("transfer/speedLimitsMode", &[], Body::None).await?.text().await?;
            if alt_mode.trim() == "1" {
                self.post("transfer/toggleSpeedLimitsMode", &[]).await?;
            }
            self.set_preferences(saved).await
        };
        match restore.await {
            Ok(()) => {
                let _ = std::fs::remove_file(marker);
                println!("[qbit] removed the old streaming speed limits");
            }
            Err(err) => eprintln!("[qbit] could not remove the old streaming speed limits: {err:#}"),
        }
    }
}
