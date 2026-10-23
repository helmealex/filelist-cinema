//! The download library: torrents handed to qBittorrent, their status (from *verified* pieces), and
//! piece-aware reading of files that may still be downloading.

use std::collections::HashMap;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use futures_util::Stream;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::qbit::Qbit;
use crate::torrent::{parse_layout, Layout};
use crate::util::{base_name, file_ext, now_ms};

pub const VIDEO_EXT: &[&str] = &[".mp4", ".m4v", ".mkv", ".webm", ".mov", ".avi", ".ts", ".m2ts", ".wmv"];
pub const SUB_EXT: &[&str] = &[".srt", ".vtt"];
const TAG: &str = "filelist-cinema";
const CHUNK: u64 = 1024 * 1024;
const PIECE_DONE: u64 = 2;

pub struct Library {
    pub qbit: Arc<Qbit>,
    download_dir: PathBuf,
    torrent_dir: PathBuf,
    library_file: PathBuf,
    layouts: RwLock<HashMap<String, Layout>>,
    entries: RwLock<Map<String, Value>>,
    pub checking_count: AtomicUsize,
}

/// A file inside a torrent, as needed for streaming.
#[derive(Clone)]
pub struct TorrentFile {
    pub id: String,
    pub hash: String,
    pub name: String, // qBittorrent's relative path
    pub size: u64,
    pub offset: Option<u64>,
    pub piece_length: u64,
}

fn is_checking(state: &str) -> bool {
    let s = state.to_ascii_lowercase();
    s.contains("checking") || s.contains("moving")
}

impl Library {
    pub fn new(qbit: Arc<Qbit>, download_dir: PathBuf, data_dir: &Path) -> Library {
        let torrent_dir = data_dir.join("torrents");
        let _ = std::fs::create_dir_all(&torrent_dir);
        let _ = std::fs::create_dir_all(&download_dir);
        let library_file = data_dir.join("library.json");
        let entries: Map<String, Value> = std::fs::read_to_string(&library_file)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let mut layouts = HashMap::new();
        for id in entries.keys() {
            if let Ok(bytes) = std::fs::read(torrent_dir.join(format!("{id}.torrent"))) {
                if let Ok(layout) = parse_layout(&bytes) {
                    layouts.insert(id.clone(), layout);
                }
            }
        }
        Library {
            qbit,
            download_dir,
            torrent_dir,
            library_file,
            layouts: RwLock::new(layouts),
            entries: RwLock::new(entries),
            checking_count: AtomicUsize::new(0),
        }
    }

    fn torrent_path(&self, id: &str) -> PathBuf {
        self.torrent_dir.join(format!("{id}.torrent"))
    }

    fn save(&self) {
        let json = serde_json::to_string_pretty(&*self.entries.read().unwrap()).unwrap_or_default();
        let _ = std::fs::write(&self.library_file, json);
    }

    pub fn has(&self, id: &str) -> bool {
        self.entries.read().unwrap().contains_key(id)
    }

    pub fn entry(&self, id: &str) -> Option<Value> {
        self.entries.read().unwrap().get(id).cloned()
    }

    pub fn layout(&self, id: &str) -> Option<Layout> {
        self.layouts.read().unwrap().get(id).cloned()
    }

    /// Re-add library torrents that qBittorrent no longer knows about; make sure streaming flags are set.
    pub async fn restore(&self) -> Result<()> {
        let layouts: Vec<(String, Layout)> = self.layouts.read().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let hashes: Vec<String> = layouts.iter().map(|(_, l)| l.hash.clone()).collect();
        let infos = self.qbit.infos(&hashes).await?;
        for (id, layout) in layouts {
            match infos.iter().find(|i| i["hash"] == layout.hash.as_str()) {
                Some(info) => self.ensure_streaming_flags(info).await?,
                None => {
                    let bytes = std::fs::read(self.torrent_path(&id))?;
                    self.send(&id, &bytes).await?;
                }
            }
        }
        Ok(())
    }

    pub async fn add(&self, id: &str, bytes: &[u8], name: &str, imdb: Option<String>, poster: Option<String>) -> Result<()> {
        if self.has(id) {
            return Ok(());
        }
        let layout = parse_layout(bytes)?;
        self.send(id, bytes).await?;
        std::fs::write(self.torrent_path(id), bytes)?;
        self.layouts.write().unwrap().insert(id.to_string(), layout);
        self.entries.write().unwrap().insert(
            id.to_string(),
            json!({ "id": id, "name": name, "imdb": imdb, "poster": poster, "addedAt": now_ms() }),
        );
        self.save();
        Ok(())
    }

    async fn send(&self, id: &str, bytes: &[u8]) -> Result<()> {
        let hash = parse_layout(bytes)?.hash;
        let fields = vec![
            ("savepath".to_string(), self.download_dir.to_string_lossy().into_owned()),
            ("tags".to_string(), TAG.to_string()),
            ("sequentialDownload".to_string(), "true".to_string()),
            ("firstLastPiecePrio".to_string(), "true".to_string()),
        ];
        self.qbit.add_torrent(bytes, &format!("{id}.torrent"), fields).await?;
        // qBittorrent answers "Fails." for duplicates, so confirm by hash instead of trusting the response text
        let mut info = None;
        for _ in 0..10 {
            info = self.qbit.infos(&[hash.clone()]).await?.into_iter().next();
            if info.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        let info = info.ok_or_else(|| anyhow!("qBittorrent did not accept the torrent"))?;
        self.ensure_streaming_flags(&info).await?;
        // subtitles are tiny: fetch them first so they work while the movie is still downloading
        self.qbit.forget(&hash);
        let subs: Vec<String> = self
            .qbit
            .files(&hash)
            .await?
            .iter()
            .filter(|f| SUB_EXT.contains(&file_ext(f["name"].as_str().unwrap_or("")).as_str()))
            .filter_map(|f| f["index"].as_u64().map(|i| i.to_string()))
            .collect();
        if !subs.is_empty() {
            let _ = self.qbit.post("torrents/filePrio", &[("hash", &hash), ("id", &subs.join("|")), ("priority", "7")]).await;
        }
        Ok(())
    }

    /// Sequential download + first/last pieces first (MKV/MP4 keep their index at the end).
    /// The add flags aren't always honoured, so check and toggle.
    async fn ensure_streaming_flags(&self, info: &Value) -> Result<()> {
        let hash = info["hash"].as_str().unwrap_or_default();
        if info["seq_dl"] == json!(false) {
            self.qbit.post("torrents/toggleSequentialDownload", &[("hashes", hash)]).await?;
        }
        if info["f_l_piece_prio"] == json!(false) {
            self.qbit.post("torrents/toggleFirstLastPiecePrio", &[("hashes", hash)]).await?;
        }
        Ok(())
    }

    pub async fn remove(&self, id: &str, delete_files: bool) -> Result<()> {
        if let Some(layout) = self.layout(id) {
            self.qbit
                .post("torrents/delete", &[("hashes", &layout.hash), ("deleteFiles", if delete_files { "true" } else { "false" })])
                .await?;
            self.qbit.forget(&layout.hash);
        }
        self.layouts.write().unwrap().remove(id);
        self.entries.write().unwrap().remove(id);
        self.save();
        let _ = std::fs::remove_file(self.torrent_path(id));
        Ok(())
    }

    pub async fn list(&self) -> Vec<Value> {
        let ids: Vec<(String, String)> = {
            let layouts = self.layouts.read().unwrap();
            self.entries.read().unwrap().keys().filter_map(|id| layouts.get(id).map(|l| (id.clone(), l.hash.clone()))).collect()
        };
        let hashes: Vec<String> = ids.iter().map(|(_, h)| h.clone()).collect();
        let (infos, error) = match self.qbit.infos(&hashes).await {
            Ok(i) => (i, None),
            Err(e) => (vec![], Some(format!("{e:#}"))),
        };
        self.checking_count
            .store(infos.iter().filter(|i| is_checking(i["state"].as_str().unwrap_or(""))).count(), Ordering::Relaxed);
        // ask qBittorrent about all torrents at once: one slow answer shouldn't delay the others
        let statuses = ids.iter().map(|(id, hash)| {
            let info = infos.iter().find(|i| i["hash"] == hash.as_str());
            self.status(id, info, error.as_deref())
        });
        let mut out = futures_util::future::join_all(statuses).await;
        out.sort_by_key(|d| std::cmp::Reverse(d["addedAt"].as_u64().unwrap_or(0)));
        out
    }

    /// The library as known from library.json, before qBittorrent has answered (shown instantly at startup).
    pub fn placeholders(&self) -> Vec<Value> {
        let mut out: Vec<Value> = self
            .entries
            .read()
            .unwrap()
            .values()
            .map(|e| {
                let mut v = e.clone();
                if let Some(obj) = v.as_object_mut() {
                    obj.extend([
                        ("connecting".into(), json!(true)),
                        ("ready".into(), json!(false)),
                        ("progress".into(), json!(0)),
                        ("done".into(), json!(false)),
                        ("peers".into(), json!(0)),
                        ("videos".into(), json!([])),
                        ("subtitles".into(), json!([])),
                    ]);
                }
                v
            })
            .collect();
        out.sort_by_key(|d| std::cmp::Reverse(d["addedAt"].as_u64().unwrap_or(0)));
        out
    }

    async fn status(&self, id: &str, info: Option<&Value>, error: Option<&str>) -> Value {
        let mut base = self.entry(id).unwrap_or_else(|| json!({ "id": id }));
        let obj = base.as_object_mut().expect("library entry is an object");
        let Some(info) = info else {
            obj.extend([
                ("ready".into(), json!(false)),
                ("progress".into(), json!(0)),
                ("done".into(), json!(false)),
                ("peers".into(), json!(0)),
                ("videos".into(), json!([])),
                ("subtitles".into(), json!([])),
                ("error".into(), json!(error.unwrap_or("Not found in qBittorrent"))),
            ]);
            return base;
        };
        let hash = info["hash"].as_str().unwrap_or_default();
        let raw = self.qbit.files(hash).await.unwrap_or_default();
        let files: Vec<Value> = raw
            .iter()
            .map(|f| {
                let path = f["name"].as_str().unwrap_or("");
                json!({
                    "index": f["index"], "name": base_name(path), "path": path, "length": f["size"],
                    "progress": f["progress"], "ext": file_ext(path),
                })
            })
            .collect();
        // Progress from the files' *verified* pieces. qBittorrent's torrent-level progress also counts data that
        // was received but not yet written/hash-checked, so it can claim 100% while the movie is unusable.
        let wanted: Vec<&Value> = raw.iter().filter(|f| f["priority"].as_u64().unwrap_or(1) > 0).collect();
        let wanted_size: f64 = wanted.iter().map(|f| f["size"].as_f64().unwrap_or(0.0)).sum();
        let progress = if wanted_size > 0.0 {
            wanted.iter().map(|f| f["size"].as_f64().unwrap_or(0.0) * f["progress"].as_f64().unwrap_or(0.0)).sum::<f64>() / wanted_size
        } else {
            info["progress"].as_f64().unwrap_or(0.0)
        };
        let done = progress >= 1.0;
        let state = info["state"].as_str().unwrap_or("");
        let checking = is_checking(state);
        let peers = info["num_seeds"].as_u64().unwrap_or(0) + info["num_leechs"].as_u64().unwrap_or(0);
        let mut problem: Option<String> = None;
        if state.contains("error") || state == "missingFiles" {
            problem = Some(format!("qBittorrent: {state}"));
        } else if !done && !checking && info["progress"].as_f64().unwrap_or(0.0) >= 0.999 && info["dlspeed"].as_u64() == Some(0) {
            problem = Some(
                "qBittorrent received all the data but has not written/verified it (its disk writer is stuck). Restart qBittorrent; if that does not help, use \"Force recheck\" on this torrent.".into(),
            );
        } else if !done && peers == 0 {
            problem = self.tracker_message(hash).await;
        }
        let eta = info["eta"].as_u64().unwrap_or(0);
        let mut videos: Vec<Value> = files
            .iter()
            .filter(|f| {
                let ext = f["ext"].as_str().unwrap_or("");
                let path = f["path"].as_str().unwrap_or("").to_lowercase();
                VIDEO_EXT.contains(&ext) && !path.split(|c: char| !c.is_alphanumeric()).any(|w| w == "sample")
            })
            .cloned()
            .collect();
        videos.sort_by_key(|v| std::cmp::Reverse(v["length"].as_u64().unwrap_or(0)));
        let subtitles: Vec<Value> =
            files.iter().filter(|f| SUB_EXT.contains(&f["ext"].as_str().unwrap_or(""))).cloned().collect();
        obj.extend([
            ("name".into(), info["name"].clone()),
            ("ready".into(), json!(!files.is_empty())),
            ("state".into(), json!(state)),
            ("progress".into(), json!(progress)),
            ("done".into(), json!(done)),
            ("checking".into(), json!(checking)),
            ("checkProgress".into(), if checking { info["progress"].clone() } else { Value::Null }),
            ("downloaded".into(), info["downloaded"].clone()),
            ("length".into(), info["size"].clone()),
            ("downloadSpeed".into(), info["dlspeed"].clone()),
            ("uploadSpeed".into(), info["upspeed"].clone()),
            ("uploaded".into(), info["uploaded"].clone()),
            ("ratio".into(), info["ratio"].clone()),
            ("peers".into(), json!(peers)),
            ("timeRemaining".into(), if eta > 0 && eta < 8_640_000 { json!(eta * 1000) } else { Value::Null }),
            ("error".into(), json!(problem)),
            ("videos".into(), json!(videos)),
            ("subtitles".into(), json!(subtitles)),
        ]);
        base
    }

    /// Surface tracker complaints such as "client not whitelisted" or "unregistered torrent".
    async fn tracker_message(&self, hash: &str) -> Option<String> {
        let trackers = self.qbit.trackers(hash).await.ok()?;
        trackers
            .iter()
            .find(|t| {
                let url = t["url"].as_str().unwrap_or("");
                (url.starts_with("http") || url.starts_with("udp:")) && !t["msg"].as_str().unwrap_or("").is_empty()
            })
            .and_then(|t| t["msg"].as_str().map(str::to_string))
    }

    // ---------- files & streaming ----------

    pub async fn file(&self, id: &str, index: u64) -> Option<TorrentFile> {
        let layout = self.layout(id)?;
        let files = self.qbit.files(&layout.hash).await.ok()?;
        let f = files.iter().find(|f| f["index"].as_u64() == Some(index))?;
        let name = f["name"].as_str()?.to_string();
        Some(TorrentFile {
            id: id.to_string(),
            offset: layout.offsets.get(&name).copied(),
            hash: layout.hash.clone(),
            size: f["size"].as_u64().unwrap_or(0),
            piece_length: layout.piece_length,
            name,
        })
    }

    /// Path on disk of a fully downloaded *and verified* file, or None.
    pub async fn local_path(&self, id: &str, index: u64) -> Option<PathBuf> {
        let file = self.file(id, index).await?;
        let info = self.qbit.info(&file.hash).await.ok()??;
        // while qBittorrent (re)checks a torrent, the file progress it reports is not trustworthy
        if is_checking(info["state"].as_str().unwrap_or("")) {
            return None;
        }
        let files = self.qbit.files(&file.hash).await.ok()?;
        let progress = files.iter().find(|f| f["index"].as_u64() == Some(index))?["progress"].as_f64()?;
        if progress < 1.0 {
            return None;
        }
        resolve_disk_path(&info, &file.name).await
    }

    /// Wait until qBittorrent has verified every piece covering [start, end] of the file.
    async fn wait_for_range(&self, file: &TorrentFile, start: u64, end: u64) -> Result<Value> {
        loop {
            let info = self.qbit.info(&file.hash).await?.ok_or_else(|| anyhow!("Torrent was removed"))?;
            if is_checking(info["state"].as_str().unwrap_or("")) {
                tokio::time::sleep(Duration::from_secs(2)).await; // being verified: nothing can be trusted yet
                continue;
            }
            let files = self.qbit.files(&file.hash).await?;
            let progress = files.iter().find(|f| f["name"] == file.name.as_str()).and_then(|f| f["progress"].as_f64());
            if progress.is_some_and(|p| p >= 1.0) {
                return Ok(info);
            }
            if let Some(offset) = file.offset {
                let first = (offset + start) / file.piece_length;
                let last = (offset + end) / file.piece_length;
                let states = self.qbit.piece_states(&file.hash).await?;
                let ready = (first..=last).all(|p| states.get(p as usize).and_then(Value::as_u64) == Some(PIECE_DONE));
                if ready {
                    return Ok(info);
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    /// Bytes [start, end] of a file, read as soon as the pieces are verified. Dropping the stream (viewer gone)
    /// stops everything and closes the file.
    pub fn read_range(self: Arc<Self>, file: TorrentFile, start: u64, end: u64) -> impl Stream<Item = std::io::Result<Bytes>> + Send {
        struct State {
            lib: Arc<Library>,
            file: TorrentFile,
            pos: u64,
            end: u64,
            handle: Option<tokio::fs::File>,
        }
        let state = State { lib: self, file, pos: start, end, handle: None };
        futures_util::stream::unfold(state, |mut s| async move {
            if s.pos > s.end {
                return None;
            }
            let chunk_end = s.end.min(s.pos + CHUNK - 1);
            let result: Result<Bytes> = async {
                let info = s.lib.wait_for_range(&s.file, s.pos, chunk_end).await?;
                if s.handle.is_none() {
                    let path = resolve_disk_path(&info, &s.file.name)
                        .await
                        .with_context(|| format!("File not found on disk yet: {}", s.file.name))?;
                    let mut f = tokio::fs::File::open(path).await?;
                    f.seek(SeekFrom::Start(s.pos)).await?;
                    s.handle = Some(f);
                }
                let f = s.handle.as_mut().expect("opened above");
                let mut buf = vec![0u8; (chunk_end - s.pos + 1) as usize];
                let mut filled = 0;
                while filled < buf.len() {
                    let n = f.read(&mut buf[filled..]).await?;
                    if n == 0 {
                        break;
                    }
                    filled += n;
                }
                if filled == 0 {
                    return Err(anyhow!("Unexpected end of file"));
                }
                buf.truncate(filled);
                Ok(Bytes::from(buf))
            }
            .await;
            match result {
                Ok(bytes) => {
                    s.pos += bytes.len() as u64;
                    Some((Ok(bytes), s))
                }
                Err(err) => {
                    eprintln!("[stream {}] {err:#}", s.file.id);
                    s.pos = s.end + 1; // end the stream after reporting the error
                    Some((Err(std::io::Error::other(format!("{err:#}"))), s))
                }
            }
        })
    }
}

async fn resolve_disk_path(info: &Value, name: &str) -> Option<PathBuf> {
    for dir in [info["save_path"].as_str(), info["download_path"].as_str()].into_iter().flatten().filter(|d| !d.is_empty()) {
        for candidate in [name.to_string(), format!("{name}.!qB")] {
            let path = Path::new(dir).join(candidate);
            if tokio::fs::metadata(&path).await.is_ok() {
                return Some(path);
            }
        }
    }
    None
}
