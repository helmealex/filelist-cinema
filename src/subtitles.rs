//! Subtitles for a downloaded movie, all served as WebVTT. Sources (track keys):
//!   u:<file>         files the user added, or downloaded from OpenSubtitles (data/subs/<id>/)
//!   t:<index>        .srt/.vtt shipped inside the torrent
//!   e:<video>:<n>    text tracks embedded in the video (extracted once with ffmpeg; reads the whole movie)
//!   o:<file_id>      OpenSubtitles results (downloaded on first use, then they become a u: file)

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use axum::http::StatusCode;
use futures_util::StreamExt;
use regex::Regex;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{watch, Notify};

use crate::config::tokio_command;
use crate::library::Library;
use crate::media::Media;
use crate::opensubs::OpenSubs;
use crate::posters::Posters;
use crate::util::{file_ext, ApiError};

// Text codecs ffmpeg can turn into WebVTT. Image-based ones (PGS, VobSub) would need OCR.
const TEXT_CODECS: &[&str] = &["subrip", "srt", "ass", "ssa", "webvtt", "mov_text", "text", "microdvd"];
const UPLOAD_EXT: &[&str] = &[".srt", ".vtt", ".ass", ".ssa", ".sub"];

#[derive(Clone)]
struct Embedded {
    n: usize,
    codec: String,
    language: Option<String>,
    title: Option<String>,
    forced: bool,
}

#[derive(Clone)]
struct EmbeddedInfo {
    streams: Vec<Embedded>,
    duration: Option<f64>,
}

type JobResult = Option<Result<(), String>>;

struct Job {
    id: String,
    video: u64,
    urgent: bool,
    queued: bool,
    progress: Arc<StdMutex<f64>>,
    abort: Arc<Notify>,
    done: watch::Sender<JobResult>,
}

#[derive(Default)]
struct Scheduler {
    jobs: HashMap<String, Job>,
    queue: VecDeque<String>,
    running: Option<String>,
}

pub struct Subtitles {
    lib: Arc<Library>,
    media: Arc<Media>,
    posters: Arc<Posters>,
    pub opensubs: StdMutex<Option<Arc<OpenSubs>>>,
    dir: PathBuf,
    stream_base: String,
    probes: tokio::sync::Mutex<HashMap<String, EmbeddedInfo>>,
    sched: StdMutex<Scheduler>,
    wake: Notify,
    failed: StdMutex<HashMap<String, Instant>>,
    is_busy: Box<dyn Fn() -> bool + Send + Sync>,
}

impl Subtitles {
    pub fn new(
        lib: Arc<Library>,
        media: Arc<Media>,
        posters: Arc<Posters>,
        opensubs: Option<OpenSubs>,
        data_dir: &Path,
        stream_base: String,
        is_busy: Box<dyn Fn() -> bool + Send + Sync>,
    ) -> Arc<Subtitles> {
        let subs = Arc::new(Subtitles {
            lib,
            media,
            posters,
            opensubs: StdMutex::new(opensubs.map(Arc::new)),
            dir: data_dir.join("subs"),
            stream_base,
            probes: tokio::sync::Mutex::new(HashMap::new()),
            sched: StdMutex::new(Scheduler::default()),
            wake: Notify::new(),
            failed: StdMutex::new(HashMap::new()),
            is_busy,
        });
        let worker = subs.clone();
        tokio::spawn(async move { worker.run_worker().await });
        subs
    }

    fn user_dir(&self, id: &str) -> PathBuf {
        self.dir.join(id.chars().filter(char::is_ascii_digit).collect::<String>())
    }

    pub fn is_cached(&self, id: &str, video: u64, n: usize) -> bool {
        self.embedded_cache(id, video, n).exists()
    }

    fn embedded_cache(&self, id: &str, video: u64, n: usize) -> PathBuf {
        self.user_dir(id).join(format!(".embedded-{video}-{n}.vtt"))
    }

    fn stream_url(&self, id: &str, video: u64) -> String {
        format!("{}/stream/{id}/{video}", self.stream_base)
    }

    /// Available subtitle tracks for a download (`status` as in the downloads list).
    pub async fn list(&self, id: &str, status: &Value, video_index: Option<u64>) -> Vec<Value> {
        let status = status.clone();
        let mut tracks = Vec::new();

        let mut saved_online: HashSet<u64> = HashSet::new();
        let mut uploads: Vec<String> = crate::config::read_dir_names(&self.user_dir(id));
        uploads.sort();
        for file in uploads.iter().filter(|f| !f.starts_with('.') && f.ends_with(".vtt")) {
            static PREFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d+|os(\d+))-").unwrap());
            if let Some(c) = PREFIX.captures(file) {
                if let Some(fid) = c.get(2).and_then(|m| m.as_str().parse().ok()) {
                    saved_online.insert(fid);
                }
            }
            let label = PREFIX.replace(file, "").trim_end_matches(".vtt").to_string();
            tracks.push(json!({ "key": format!("u:{file}"), "source": "upload", "lang": guess_lang(&label), "label": label, "ready": true, "cached": true }));
        }

        for s in status["subtitles"].as_array().cloned().unwrap_or_default() {
            let path = s["path"].as_str().unwrap_or("");
            tracks.push(json!({ "key": format!("t:{}", s["index"]), "source": "torrent", "label": path, "lang": guess_lang(path), "ready": true, "cached": true }));
        }

        let videos = status["videos"].as_array().cloned().unwrap_or_default();
        let video = video_index.and_then(|vi| videos.iter().find(|v| v["index"].as_u64() == Some(vi))).or(videos.first());
        if let (Some(video), true) = (video, self.media.ffmpeg.is_some() && self.media.ffprobe.is_some()) {
            let vi = video["index"].as_u64().unwrap_or(0);
            let complete = video["progress"].as_f64().unwrap_or(0.0) >= 1.0 && status["checking"] != json!(true);
            // MKV: subtitle lines are picked out of the stream while playing, so they work right away
            let live_capable = video["ext"] == ".mkv";
            if let Ok(info) = self.embedded(id, vi).await {
                let job = {
                    let sched = self.sched.lock().unwrap();
                    sched.jobs.get(&format!("{id}:{vi}")).map(|j| (j.queued, *j.progress.lock().unwrap()))
                };
                for s in info.streams {
                    let cached = self.embedded_cache(id, vi, s.n).exists();
                    let supported = TEXT_CODECS.contains(&s.codec.as_str());
                    let label = [s.title.clone(), s.language.clone(), s.forced.then(|| "forced".to_string()), Some(s.codec.clone())]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join(" · ");
                    let lang = s.language.as_deref().map(|l| guess_lang(l).unwrap_or_else(|| l.to_string())).or_else(|| guess_lang(s.title.as_deref().unwrap_or("")));
                    tracks.push(json!({
                        "key": format!("e:{vi}:{}", s.n), "source": "embedded", "label": label, "lang": lang,
                        "ready": supported && (cached || complete || live_capable), "cached": cached,
                        "live": supported && !cached && live_capable,
                        "reason": if supported { "available when the download finishes" } else { "image-based, cannot be displayed" },
                        "extracting": if supported && !cached { job.map(|(queued, progress)| json!({ "queued": queued, "progress": progress })) } else { None },
                    }));
                }
            }
        }

        // #6: online subtitles, found by IMDb id
        let opensubs = self.opensubs.lock().unwrap().clone();
        if let Some(os) = opensubs {
            if let Some(imdb) = self.imdb_of(id, &status).await {
                if let Ok(results) = os.search(&imdb).await {
                    for r in results.into_iter().filter(|r| !saved_online.contains(&r.file_id)) {
                        let mut label = r.release.clone();
                        if r.hearing_impaired {
                            label.push_str(" · SDH");
                        }
                        if r.machine_translated {
                            label.push_str(" · machine translated");
                        }
                        label.push_str(&format!(" · {} downloads", r.downloads));
                        tracks.push(json!({ "key": format!("o:{}", r.file_id), "source": "online", "label": label, "lang": r.language, "ready": true, "cached": false }));
                    }
                }
            }
        }
        tracks
    }

    async fn imdb_of(&self, id: &str, status: &Value) -> Option<String> {
        if let Some(imdb) = status["imdb"].as_str().filter(|s| s.starts_with("tt")) {
            return Some(imdb.to_string());
        }
        let name = status["name"].as_str().unwrap_or("");
        let _ = id;
        self.posters.meta(None, name).await.map(|m| m.imdb)
    }

    /// Subtitle streams of a video (reads only the file header, so it works while downloading).
    async fn embedded(&self, id: &str, video: u64) -> Result<EmbeddedInfo> {
        let key = format!("{id}:{video}");
        if let Some(hit) = self.probes.lock().await.get(&key) {
            return Ok(hit.clone());
        }
        let ffprobe = self.media.ffprobe.clone().context("ffprobe is not installed")?;
        let out = tokio::time::timeout(
            Duration::from_secs(30),
            tokio_command(ffprobe)
                .args([
                    "-v", "error", "-select_streams", "s", "-show_entries",
                    "format=duration:stream=codec_name:stream_tags=language,title:stream_disposition=forced",
                    "-of", "json", &self.stream_url(id, video),
                ])
                .stdin(Stdio::null())
                .output(),
        )
        .await
        .context("ffprobe timed out")??;
        let json: Value = serde_json::from_slice(&out.stdout)?;
        let info = EmbeddedInfo {
            streams: json["streams"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .enumerate()
                .map(|(n, s)| Embedded {
                    n,
                    codec: s["codec_name"].as_str().unwrap_or("").to_string(),
                    language: s["tags"]["language"].as_str().map(str::to_string),
                    title: s["tags"]["title"].as_str().map(str::to_string),
                    forced: s["disposition"]["forced"].as_u64() == Some(1),
                })
                .collect(),
            duration: json["format"]["duration"].as_str().and_then(|d| d.parse().ok()),
        };
        self.probes.lock().await.insert(key, info.clone());
        Ok(info)
    }

    /// WebVTT with cues shifted by `offset` seconds (converted streams that started mid-movie).
    pub async fn get(&self, id: &str, key: &str, offset: f64) -> Result<String, ApiError> {
        let mut parts = key.splitn(3, ':');
        let kind = parts.next().unwrap_or("");
        let a = parts.next().unwrap_or("");
        let b = parts.next().unwrap_or("");
        match kind {
            "u" => {
                let name = Path::new(a).file_name().ok_or_else(|| ApiError::not_found("Subtitle not found"))?;
                let text = tokio::fs::read_to_string(self.user_dir(id).join(name)).await.map_err(|_| ApiError::not_found("Subtitle not found"))?;
                Ok(to_vtt(&text, offset))
            }
            "t" => {
                let index: u64 = a.parse().map_err(|_| ApiError::bad_request("bad key"))?;
                let file = self.lib.file(id, index).await.ok_or_else(|| ApiError::not_found("Subtitle not found"))?;
                let size = file.size;
                let mut stream = Box::pin(self.lib.clone().read_range(file, 0, size.saturating_sub(1)));
                let mut buf = Vec::new();
                while let Some(chunk) = stream.next().await {
                    buf.extend_from_slice(&chunk.map_err(|e| anyhow!(e))?);
                }
                Ok(to_vtt(&decode(&buf), offset))
            }
            "e" => {
                let video: u64 = a.parse().map_err(|_| ApiError::bad_request("bad key"))?;
                let n: usize = b.parse().map_err(|_| ApiError::bad_request("bad key"))?;
                let cache = self.embedded_cache(id, video, n);
                if !cache.exists() {
                    if self.lib.local_path(id, video).await.is_none() {
                        return Err(ApiError::new(StatusCode::CONFLICT, "These subtitles become available when the download finishes."));
                    }
                    let mut done = self.extract_all(id, video, true);
                    let result = loop {
                        if let Some(r) = done.borrow().clone() {
                            break r;
                        }
                        if done.changed().await.is_err() {
                            break Err("extraction stopped".into());
                        }
                    };
                    result.map_err(|e| ApiError::from(anyhow!("Could not extract the subtitles: {e}")))?;
                }
                let text = tokio::fs::read_to_string(&cache).await.map_err(|_| ApiError::not_found("This subtitle track is empty"))?;
                Ok(to_vtt(&text, offset))
            }
            "o" => {
                let file_id: u64 = a.parse().map_err(|_| ApiError::bad_request("bad key"))?;
                let os = self.opensubs.lock().unwrap().clone().ok_or_else(|| ApiError::bad_request("OpenSubtitles is not configured"))?;
                let bytes = os.download(file_id).await?;
                let vtt = to_vtt(&decode(&bytes), 0.0);
                if !vtt.contains("-->") {
                    return Err(ApiError::from(anyhow!("OpenSubtitles returned a file without subtitles")));
                }
                // keep it, so it's instant next time (and listed under "Your files")
                let label = self.online_label(id, file_id).await.unwrap_or_else(|| format!("OpenSubtitles {file_id}"));
                let dir = self.user_dir(id);
                tokio::fs::create_dir_all(&dir).await.map_err(anyhow::Error::from)?;
                tokio::fs::write(dir.join(format!("os{file_id}-{}.vtt", sanitize_name(&label))), &vtt).await.map_err(anyhow::Error::from)?;
                Ok(to_vtt(&vtt, offset))
            }
            _ => Err(ApiError::not_found("Subtitle not found")),
        }
    }

    async fn online_label(&self, id: &str, file_id: u64) -> Option<String> {
        let os = self.opensubs.lock().unwrap().clone()?;
        let status = self.lib.entry(id)?;
        let imdb = self.imdb_of(id, &status).await?;
        let hit = os.search(&imdb).await.ok()?.into_iter().find(|r| r.file_id == file_id)?;
        Some(format!("{} {}", hit.language.to_uppercase(), hit.release))
    }

    pub async fn upload(&self, id: &str, filename: &str, bytes: &[u8]) -> Result<String, ApiError> {
        let ext = file_ext(filename);
        if !UPLOAD_EXT.contains(&ext.as_str()) {
            return Err(ApiError::bad_request(format!("Unsupported subtitle format \"{ext}\". Use .srt, .vtt, .ass, .ssa or .sub")));
        }
        if !self.lib.has(id) {
            return Err(ApiError::not_found("Unknown download"));
        }
        let vtt = if ext == ".srt" || ext == ".vtt" {
            to_vtt(&decode(bytes), 0.0)
        } else {
            let ffmpeg = self.media.ffmpeg.clone().ok_or_else(|| ApiError::bad_request(format!("Converting {ext} subtitles needs ffmpeg")))?;
            let tmp = std::env::temp_dir().join(format!("flc-sub-{}{ext}", crate::util::now_ms()));
            tokio::fs::write(&tmp, decode(bytes)).await.map_err(anyhow::Error::from)?;
            let out = tokio::time::timeout(
                Duration::from_secs(30),
                tokio_command(ffmpeg).args(["-v", "error", "-i"]).arg(&tmp).args(["-f", "webvtt", "pipe:1"]).stdin(Stdio::null()).output(),
            )
            .await;
            let _ = tokio::fs::remove_file(&tmp).await;
            match out {
                Ok(Ok(o)) if o.status.success() => to_vtt(&String::from_utf8_lossy(&o.stdout), 0.0),
                _ => return Err(ApiError::bad_request("Could not read that subtitle file")),
            }
        };
        if !vtt.contains("-->") {
            return Err(ApiError::bad_request("No subtitle lines found in that file"));
        }
        let base = Path::new(filename).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let base = sanitize_name(&base);
        let file = format!("{}-{}.vtt", crate::util::now_ms(), if base.is_empty() { "subtitles".into() } else { base });
        let dir = self.user_dir(id);
        tokio::fs::create_dir_all(&dir).await.map_err(anyhow::Error::from)?;
        tokio::fs::write(dir.join(&file), vtt).await.map_err(anyhow::Error::from)?;
        Ok(format!("u:{file}"))
    }

    pub async fn remove(&self, id: &str, key: &str) -> Result<(), ApiError> {
        let name = key.strip_prefix("u:").ok_or_else(|| ApiError::bad_request("Only subtitles you added can be removed"))?;
        let name = Path::new(name).file_name().ok_or_else(|| ApiError::bad_request("bad key"))?;
        let _ = tokio::fs::remove_file(self.user_dir(id).join(name)).await;
        Ok(())
    }

    pub async fn remove_all(&self, id: &str) {
        let _ = tokio::fs::remove_dir_all(self.user_dir(id)).await;
        self.probes.lock().await.retain(|k, _| !k.starts_with(&format!("{id}:")));
    }

    // ---------- extraction queue ----------
    // Text subtitles are interleaved through the whole file, so extraction reads the entire movie. Jobs run one
    // at a time (the disk is the bottleneck); background jobs wait while someone watches or qBittorrent
    // verifies files; a job someone is waiting for in the player goes first and pauses a background job.

    fn extract_all(&self, id: &str, video: u64, urgent: bool) -> watch::Receiver<JobResult> {
        let key = format!("{id}:{video}");
        let mut sched = self.sched.lock().unwrap();
        if !sched.jobs.contains_key(&key) {
            let (tx, _) = watch::channel(None);
            sched.jobs.insert(
                key.clone(),
                Job { id: id.into(), video, urgent, queued: true, progress: Arc::new(StdMutex::new(0.0)), abort: Arc::new(Notify::new()), done: tx },
            );
            sched.queue.push_back(key.clone());
        }
        let running = sched.running.clone();
        let job = sched.jobs.get_mut(&key).expect("just inserted");
        let rx = job.done.subscribe();
        if urgent {
            job.urgent = true;
            if job.queued {
                sched.queue.retain(|k| *k != key);
                sched.queue.push_front(key.clone());
                if let Some(r) = running.filter(|r| *r != key) {
                    if let Some(rj) = sched.jobs.get(&r).filter(|j| !j.urgent) {
                        rj.abort.notify_one(); // resumes after the urgent one
                    }
                }
            }
        }
        drop(sched);
        self.wake.notify_one();
        rx
    }

    /// Someone started watching (or qBittorrent is verifying): pause a background extraction; it resumes later.
    pub fn pause_background(&self) {
        let sched = self.sched.lock().unwrap();
        if let Some(job) = sched.running.as_ref().and_then(|k| sched.jobs.get(k)).filter(|j| !j.urgent) {
            println!("[subs {}] background extraction paused (disk needed elsewhere)", job.id);
            job.abort.notify_one();
        }
    }

    /// Called for finished downloads: extract embedded subtitles in the background so they're ready on Play.
    pub fn prefetch(self: &Arc<Self>, id: &str, video: u64) {
        let key = format!("{id}:{video}");
        if self.media.ffmpeg.is_none() || self.sched.lock().unwrap().jobs.contains_key(&key) {
            return;
        }
        if self.failed.lock().unwrap().get(&key).is_some_and(|t| t.elapsed() < Duration::from_secs(30 * 60)) {
            return; // don't hammer the disk retrying
        }
        let me = self.clone();
        let id = id.to_string();
        tokio::spawn(async move {
            let Ok(info) = me.embedded(&id, video).await else { return };
            let missing = info.streams.iter().any(|s| TEXT_CODECS.contains(&s.codec.as_str()) && !me.embedded_cache(&id, video, s.n).exists());
            if missing && me.lib.local_path(&id, video).await.is_some() {
                let mut rx = me.extract_all(&id, video, false);
                while rx.changed().await.is_ok() {
                    if let Some(Err(err)) = rx.borrow().clone() {
                        me.failed.lock().unwrap().insert(key.clone(), Instant::now());
                        eprintln!("[subs {id}] embedded extraction failed: {err}");
                    }
                    if rx.borrow().is_some() {
                        break;
                    }
                }
            }
        });
    }

    async fn run_worker(self: Arc<Self>) {
        loop {
            let next = {
                let mut sched = self.sched.lock().unwrap();
                let busy = (self.is_busy)();
                let pos = sched.queue.iter().position(|k| sched.jobs.get(k).is_some_and(|j| j.urgent || !busy));
                pos.and_then(|p| sched.queue.remove(p)).map(|key| {
                    sched.running = Some(key.clone());
                    let job = sched.jobs.get_mut(&key).expect("queued job exists");
                    job.queued = false;
                    *job.progress.lock().unwrap() = 0.0;
                    (key, job.id.clone(), job.video, job.progress.clone(), job.abort.clone())
                })
            };
            let Some((key, id, video, progress, abort)) = next else {
                tokio::select! {
                    _ = self.wake.notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                }
                continue;
            };
            let outcome = tokio::select! {
                r = self.run_extraction(&id, video, progress) => Some(r),
                _ = abort.notified() => None,
            };
            let mut sched = self.sched.lock().unwrap();
            sched.running = None;
            match outcome {
                None => {
                    // paused: back in the queue, same waiters
                    if let Some(job) = sched.jobs.get_mut(&key) {
                        job.queued = true;
                    }
                    sched.queue.push_back(key);
                }
                Some(result) => {
                    if let Some(job) = sched.jobs.remove(&key) {
                        let _ = job.done.send(Some(result.map_err(|e| format!("{e:#}"))));
                    }
                }
            }
        }
    }

    async fn run_extraction(&self, id: &str, video: u64, progress: Arc<StdMutex<f64>>) -> Result<()> {
        let info = self.embedded(id, video).await?;
        let text: Vec<&Embedded> = info.streams.iter().filter(|s| TEXT_CODECS.contains(&s.codec.as_str())).collect();
        if text.is_empty() {
            return Ok(());
        }
        let input = self.lib.local_path(id, video).await.ok_or_else(|| anyhow!("the video is not completely downloaded yet"))?;
        tokio::fs::create_dir_all(self.user_dir(id)).await?;
        let size = tokio::fs::metadata(&input).await?.len();
        let ffmpeg = self.media.ffmpeg.clone().context("ffmpeg is not installed")?;
        let mut args: Vec<String> = ["-v", "error", "-nostats", "-progress", "pipe:1", "-i"].map(String::from).to_vec();
        args.push(input.to_string_lossy().into_owned());
        for s in &text {
            args.extend(["-map".into(), format!("0:s:{}", s.n), "-f".into(), "webvtt".into(), "-y".into()]);
            args.push(format!("{}.part", self.embedded_cache(id, video, s.n).to_string_lossy()));
        }
        println!("[subs {id}] extracting {} subtitle track(s) from {:.1} GB…", text.len(), size as f64 / 1e9);
        let mut child = tokio_command(ffmpeg).args(&args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
        let stdout = child.stdout.take().context("no ffmpeg output")?;
        let duration = info.duration;
        let reader = async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let (Some(us), Some(d)) = (line.strip_prefix("out_time_us=").and_then(|v| v.parse::<f64>().ok()), duration) {
                    *progress.lock().unwrap() = (us / 1e6 / d).clamp(0.0, 0.99);
                }
            }
        };
        // about 5 MB/s worst case on a busy hard drive, at least 20 minutes
        let limit = Duration::from_secs((size / 5_000_000).max(20 * 60));
        let status = tokio::time::timeout(limit, async {
            reader.await;
            child.wait().await
        })
        .await
        .map_err(|_| anyhow!("extraction took too long"))??;
        if !status.success() {
            bail!("ffmpeg failed ({status})");
        }
        for s in &text {
            let cache = self.embedded_cache(id, video, s.n);
            tokio::fs::rename(format!("{}.part", cache.to_string_lossy()), &cache).await?;
        }
        println!("[subs {id}] subtitles extracted");
        Ok(())
    }
}

fn sanitize_name(s: &str) -> String {
    let cleaned: String = s.chars().map(|c| if c.is_alphanumeric() || " .-_()[]".contains(c) { c } else { '_' }).collect();
    cleaned.trim().chars().take(120).collect()
}

fn guess_lang(text: &str) -> Option<String> {
    static RO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(ro|ron|rum|romanian|romana|rom)\b").unwrap());
    static EN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(en|eng|english)\b").unwrap());
    let t = text.replace(['.', '_', '-'], " ");
    if RO.is_match(&t) {
        Some("ro".into())
    } else if EN.is_match(&t) {
        Some("en".into())
    } else {
        None
    }
}

/// Subtitle files come in all encodings; Romanian ones are often Windows-1250.
pub fn decode(buf: &[u8]) -> String {
    if buf.starts_with(&[0xFF, 0xFE]) {
        return encoding_rs::UTF_16LE.decode(buf).0.into_owned();
    }
    if buf.starts_with(&[0xFE, 0xFF]) {
        return encoding_rs::UTF_16BE.decode(buf).0.into_owned();
    }
    match std::str::from_utf8(buf) {
        Ok(s) => s.to_string(),
        Err(_) => encoding_rs::WINDOWS_1250.decode(buf).0.into_owned(),
    }
}

/// SRT or VTT text -> WebVTT, shifting every cue by -offset seconds.
pub fn to_vtt(text: &str, offset: f64) -> String {
    static TIME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:(\d{1,2}):)?(\d{1,2}):(\d{2})[,.](\d{1,3})").unwrap());
    static ASS_TAGS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{\\[^}]*\}").unwrap());
    let text = text.trim_start_matches('\u{feff}').replace("\r\n", "\n").replace('\r', "\n");
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.first().is_some_and(|l| l.starts_with("WEBVTT")) {
        lines.remove(0);
    }
    let body: Vec<String> = lines
        .iter()
        .map(|line| {
            if line.contains("-->") {
                TIME.replace_all(line, |c: &regex::Captures| {
                    let h: f64 = c.get(1).map_or(0.0, |m| m.as_str().parse().unwrap_or(0.0));
                    let m: f64 = c[2].parse().unwrap_or(0.0);
                    let s: f64 = c[3].parse().unwrap_or(0.0);
                    let ms: f64 = format!("{:0<3}", &c[4]).parse().unwrap_or(0.0);
                    stamp((h * 3600.0 + m * 60.0 + s + ms / 1000.0 - offset).max(0.0))
                })
                .into_owned()
            } else {
                ASS_TAGS.replace_all(line, "").into_owned() // drop ASS override tags like {\an8} that some SRTs carry
            }
        })
        .collect();
    format!("WEBVTT\n\n{}\n", body.join("\n").trim())
}

fn stamp(t: f64) -> String {
    let total_ms = (t * 1000.0).round() as u64;
    let (h, rest) = (total_ms / 3_600_000, total_ms % 3_600_000);
    format!("{:02}:{:02}:{:02}.{:03}", h, rest / 60_000, (rest % 60_000) / 1000, rest % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srt_to_vtt_with_offset() {
        let srt = "1\r\n00:00:05,000 --> 00:00:07,500\r\n{\\an8}Bună ziua\r\n\r\n2\r\n00:01:00,250 --> 00:01:02,000\r\nSalut\r\n";
        let vtt = to_vtt(srt, 2.0);
        assert!(vtt.starts_with("WEBVTT\n\n1\n00:00:03.000 --> 00:00:05.500\nBună ziua"));
        assert!(vtt.contains("00:00:58.250 --> 00:01:00.000"));
        assert_eq!(to_vtt("WEBVTT\n\n00:01.5 --> 00:02.000\nx", 0.0), "WEBVTT\n\n00:00:01.500 --> 00:00:02.000\nx\n");
    }

    #[test]
    fn decodes_romanian_windows_1250() {
        // "ţ" in Windows-1250 is 0xFE, "ş" is 0xBA
        assert_eq!(decode(&[b'M', b'u', b'l', 0xFE, b'u', b'm', b'e', b's', b'c']), "Mulţumesc");
        assert_eq!(decode("Bună".as_bytes()), "Bună");
    }
}
