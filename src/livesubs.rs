//! Subtitles collected *while a movie plays*, instead of reading the whole file up front:
//!   - direct playback: a Matroska parser watches the bytes /stream sends to the player and picks out the subtitle
//!     blocks (no extra disk reads; seeking just resynchronises at the next cluster);
//!   - converted/HLS playback: the ffmpeg already converting the video also writes each text subtitle track to a
//!     WebVTT side file, which is read as it grows.
//! Cues go into one in-memory store per (download, video, subtitle track); the player polls it for new ones.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use regex::Regex;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

#[derive(Default)]
struct Store {
    version: u64,
    cues: Vec<(u64, u64, u64, String)>, // (version, start ms, end ms, text)
    seen: HashSet<(u64, String)>,
}

#[derive(Default)]
pub struct LiveCues {
    stores: Mutex<HashMap<String, Store>>,
}

impl LiveCues {
    pub fn key(id: &str, video: u64, n: usize) -> String {
        format!("{id}:{video}:{n}")
    }

    pub fn add(&self, key: &str, start_ms: u64, end_ms: u64, text: &str) {
        let text = clean_text(text);
        if text.is_empty() {
            return;
        }
        let mut stores = self.stores.lock().unwrap();
        let store = stores.entry(key.to_string()).or_default();
        if store.seen.insert((start_ms, text.clone())) {
            store.version += 1;
            let v = store.version;
            store.cues.push((v, start_ms, end_ms.max(start_ms + 500), text));
        }
    }

    /// Cues added after `since`, as (start s, end s, text), and the latest version.
    pub fn since(&self, key: &str, since: u64) -> (u64, Vec<(f64, f64, String)>) {
        let stores = self.stores.lock().unwrap();
        let Some(store) = stores.get(key) else { return (0, vec![]) };
        let cues = store
            .cues
            .iter()
            .filter(|(v, ..)| *v > since)
            .map(|(_, s, e, t)| (*s as f64 / 1000.0, *e as f64 / 1000.0, t.clone()))
            .collect();
        (store.version, cues)
    }

    pub fn forget(&self, id: &str) {
        self.stores.lock().unwrap().retain(|k, _| !k.starts_with(&format!("{id}:")));
    }

    /// Follow a WebVTT file ffmpeg is writing (times relative to `offset` seconds) until `alive` is cleared and
    /// nothing new has arrived.
    pub fn follow_vtt(self: &Arc<Self>, file: PathBuf, key: String, offset: f64, alive: Arc<AtomicBool>) {
        let me = self.clone();
        tokio::spawn(async move {
            let mut pos = 0u64;
            let mut pending = String::new();
            let mut idle_after_stop = 0;
            loop {
                tokio::time::sleep(Duration::from_millis(1000)).await;
                let mut grew = false;
                if let Ok(mut f) = tokio::fs::File::open(&file).await {
                    if f.seek(std::io::SeekFrom::Start(pos)).await.is_ok() {
                        let mut buf = Vec::new();
                        if let Ok(n) = f.read_to_end(&mut buf).await {
                            if n > 0 {
                                grew = true;
                                pos += n as u64;
                                pending.push_str(&String::from_utf8_lossy(&buf));
                            }
                        }
                    }
                }
                // complete cue blocks end with a blank line; keep the unfinished tail
                let normalized = pending.replace("\r\n", "\n");
                let (complete, rest) = match normalized.rfind("\n\n") {
                    Some(i) => (normalized[..i].to_string(), normalized[i + 2..].to_string()),
                    None => (String::new(), normalized),
                };
                pending = rest;
                for block in complete.split("\n\n") {
                    if let Some((start, end, text)) = parse_vtt_block(block) {
                        me.add(&key, ((start + offset) * 1000.0) as u64, ((end + offset) * 1000.0) as u64, &text);
                    }
                }
                if !alive.load(Ordering::Relaxed) {
                    idle_after_stop = if grew { 0 } else { idle_after_stop + 1 };
                    if idle_after_stop >= 2 {
                        if let Some((start, end, text)) = parse_vtt_block(&pending) {
                            me.add(&key, ((start + offset) * 1000.0) as u64, ((end + offset) * 1000.0) as u64, &text);
                        }
                        let _ = tokio::fs::remove_file(&file).await;
                        return;
                    }
                }
            }
        });
    }
}

static TIME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:(\d{1,2}):)?(\d{1,2}):(\d{2})[,.](\d{1,3})").unwrap());

fn parse_vtt_time(c: &regex::Captures) -> f64 {
    let h: f64 = c.get(1).map_or(0.0, |m| m.as_str().parse().unwrap_or(0.0));
    let m: f64 = c[2].parse().unwrap_or(0.0);
    let s: f64 = c[3].parse().unwrap_or(0.0);
    let ms: f64 = format!("{:0<3}", &c[4]).parse().unwrap_or(0.0);
    h * 3600.0 + m * 60.0 + s + ms / 1000.0
}

fn parse_vtt_block(block: &str) -> Option<(f64, f64, String)> {
    let mut lines = block.lines().skip_while(|l| !l.contains("-->"));
    let timing = lines.next()?;
    let mut times = TIME.captures_iter(timing);
    let start = parse_vtt_time(&times.next()?);
    let end = parse_vtt_time(&times.next()?);
    let text = lines.collect::<Vec<_>>().join("\n");
    Some((start, end, text))
}

fn clean_text(text: &str) -> String {
    static ASS_TAGS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{[^}]*\}").unwrap());
    let t = text.replace("\\N", "\n").replace("\\n", "\n").replace('\r', "");
    ASS_TAGS.replace_all(&t, "").trim().to_string()
}

// ---------- Matroska ----------

const ID_EBML: u32 = 0x1A45DFA3;
const ID_SEGMENT: u32 = 0x18538067;
const ID_INFO: u32 = 0x1549A966;
const ID_TIMECODE_SCALE: u32 = 0x2AD7B1;
const ID_TRACKS: u32 = 0x1654AE6B;
const ID_TRACK_ENTRY: u32 = 0xAE;
const ID_TRACK_NUMBER: u32 = 0xD7;
const ID_TRACK_TYPE: u32 = 0x83;
const ID_CODEC_ID: u32 = 0x86;
const ID_DEFAULT_DURATION: u32 = 0x23E383;
const ID_CLUSTER: u32 = 0x1F43B675;
const ID_TIMECODE: u32 = 0xE7;
const ID_SIMPLE_BLOCK: u32 = 0xA3;
const ID_BLOCK_GROUP: u32 = 0xA0;
const ID_BLOCK: u32 = 0xA1;
const ID_BLOCK_DURATION: u32 = 0x9B;
const TRACK_TYPE_SUBTITLE: u64 = 17;
/// Blocks larger than this can't be subtitles: skip them without buffering (video, audio).
const MAX_SUB_BLOCK: u64 = 256 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SubCodec {
    Srt,
    Ass,
    WebVtt,
}

#[derive(Clone, Debug)]
pub struct MkvSubTrack {
    pub n: usize, // index among subtitle streams (ffmpeg's 0:s:n)
    pub codec: Option<SubCodec>, // None: image-based (PGS, VobSub) — ignored
    pub default_duration_ms: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct MkvInfo {
    pub timecode_scale: u64, // ns per timecode unit
    pub tracks: HashMap<u64, MkvSubTrack>,
}

/// EBML variable-size integer: (value, length). For IDs the marker bit is kept.
fn read_vint(buf: &[u8], keep_marker: bool) -> Option<(u64, usize)> {
    let first = *buf.first()?;
    if first == 0 {
        return None;
    }
    let len = first.leading_zeros() as usize + 1;
    if len > 8 || buf.len() < len {
        return None;
    }
    // the length marker bit is removed from sizes; for 8-byte values the whole first byte is the marker
    let mut value = if keep_marker { first as u64 } else { (first as u64) & (0xFFu64 >> len) };
    for b in &buf[1..len] {
        value = (value << 8) | *b as u64;
    }
    Some((value, len))
}

const UNKNOWN_SIZE: u64 = u64::MAX;

/// Element header at the start of `buf`: (id, size, header length). Size UNKNOWN_SIZE if "unknown".
fn read_header(buf: &[u8]) -> Option<(u32, u64, usize)> {
    let (id, id_len) = read_vint(buf, true)?;
    if id_len > 4 {
        return None;
    }
    let (size, size_len) = read_vint(&buf[id_len..], false)?;
    let all_ones = (1u64 << (7 * size_len)) - 1;
    Some((id as u32, if size == all_ones { UNKNOWN_SIZE } else { size }, id_len + size_len))
}

fn read_uint(data: &[u8]) -> u64 {
    data.iter().take(8).fold(0, |acc, b| (acc << 8) | *b as u64)
}

/// Parse the subtitle tracks and timecode scale from the beginning of a Matroska file.
pub fn parse_mkv_header(buf: &[u8]) -> Option<MkvInfo> {
    let (id, _, hl) = read_header(buf)?;
    if id != ID_EBML {
        return None;
    }
    let (_, ebml_size, _) = read_header(buf)?;
    let mut pos = hl + ebml_size as usize;
    let (id, _, hl) = read_header(buf.get(pos..)?)?;
    if id != ID_SEGMENT {
        return None;
    }
    pos += hl;
    let mut info = MkvInfo { timecode_scale: 1_000_000, tracks: HashMap::new() };
    let mut found_tracks = false;
    while let Some((id, size, hl)) = buf.get(pos..).and_then(read_header) {
        if id == ID_CLUSTER || size == UNKNOWN_SIZE {
            break;
        }
        let body = buf.get(pos + hl..pos + hl + size as usize);
        match (id, body) {
            (ID_INFO, Some(body)) => {
                for_each_child(body, |cid, data| {
                    if cid == ID_TIMECODE_SCALE {
                        info.timecode_scale = read_uint(data).max(1);
                    }
                });
            }
            (ID_TRACKS, Some(body)) => {
                found_tracks = true;
                let mut sub_index = 0;
                for_each_child(body, |cid, entry| {
                    if cid != ID_TRACK_ENTRY {
                        return;
                    }
                    let (mut number, mut kind, mut codec, mut default_duration) = (0, 0, String::new(), None);
                    for_each_child(entry, |fid, data| match fid {
                        ID_TRACK_NUMBER => number = read_uint(data),
                        ID_TRACK_TYPE => kind = read_uint(data),
                        ID_CODEC_ID => codec = String::from_utf8_lossy(data).trim_end_matches('\0').to_string(),
                        ID_DEFAULT_DURATION => default_duration = Some(read_uint(data) / 1_000_000),
                        _ => {}
                    });
                    if kind == TRACK_TYPE_SUBTITLE {
                        let codec = match codec.as_str() {
                            "S_TEXT/UTF8" | "S_TEXT/ASCII" => Some(SubCodec::Srt),
                            "S_TEXT/ASS" | "S_TEXT/SSA" | "S_ASS" | "S_SSA" => Some(SubCodec::Ass),
                            "S_TEXT/WEBVTT" => Some(SubCodec::WebVtt),
                            _ => None,
                        };
                        info.tracks.insert(number, MkvSubTrack { n: sub_index, codec, default_duration_ms: default_duration });
                        sub_index += 1;
                    }
                });
            }
            (_, None) if id == ID_TRACKS || id == ID_INFO => return None, // header not fully read
            _ => {}
        }
        if found_tracks && id == ID_TRACKS {
            break;
        }
        pos += hl + size as usize;
    }
    found_tracks.then_some(info)
}

fn for_each_child(mut data: &[u8], mut f: impl FnMut(u32, &[u8])) {
    while let Some((id, size, hl)) = read_header(data) {
        if size == UNKNOWN_SIZE || hl + size as usize > data.len() {
            return;
        }
        f(id, &data[hl..hl + size as usize]);
        data = &data[hl + size as usize..];
    }
}

/// Watches a Matroska byte stream (from any starting offset) and reports subtitle cues.
pub struct MkvTap {
    info: Arc<MkvInfo>,
    buf: Vec<u8>,
    skip: u64,
    synced: bool,
    cluster_tc: u64,
    /// key prefix "id:video"; cues are reported as (n, start ms, end ms, text)
    emit: Box<dyn FnMut(usize, u64, u64, String) + Send>,
}

impl MkvTap {
    pub fn new(info: Arc<MkvInfo>, emit: Box<dyn FnMut(usize, u64, u64, String) + Send>) -> MkvTap {
        MkvTap { info, buf: Vec::new(), skip: 0, synced: false, cluster_tc: 0, emit }
    }

    pub fn feed(&mut self, mut chunk: &[u8]) {
        // skip the remainder of a large element (video/audio) without buffering it
        if self.skip > 0 {
            let n = (self.skip as usize).min(chunk.len());
            self.skip -= n as u64;
            chunk = &chunk[n..];
        }
        self.buf.extend_from_slice(chunk);
        self.process();
        // never let the buffer grow without bound (e.g. garbage): resync instead
        if self.buf.len() > 4 * MAX_SUB_BLOCK as usize {
            self.buf.drain(..self.buf.len() - 4);
            self.synced = false;
        }
    }

    fn ms(&self, timecode: i64) -> u64 {
        (timecode.max(0) as u128 * self.info.timecode_scale as u128 / 1_000_000) as u64
    }

    fn process(&mut self) {
        loop {
            if !self.synced && !self.resync() {
                return;
            }
            let Some((id, size, hl)) = read_header(&self.buf) else {
                if self.buf.len() >= 12 {
                    self.synced = false; // not a valid element header: lost track
                    continue;
                }
                return; // need more bytes
            };
            match id {
                // containers we walk into
                ID_SEGMENT | ID_CLUSTER => {
                    self.buf.drain(..hl);
                }
                ID_TIMECODE | ID_SIMPLE_BLOCK | ID_BLOCK_GROUP if size != UNKNOWN_SIZE && size <= MAX_SUB_BLOCK => {
                    let total = hl + size as usize;
                    if self.buf.len() < total {
                        return; // wait for the whole (small) element
                    }
                    let body: Vec<u8> = self.buf[hl..total].to_vec();
                    self.buf.drain(..total);
                    match id {
                        ID_TIMECODE => self.cluster_tc = read_uint(&body),
                        ID_SIMPLE_BLOCK => self.block(&body, None),
                        _ => {
                            let (mut block, mut duration) = (None, None);
                            for_each_child(&body, |cid, data| match cid {
                                ID_BLOCK => block = Some(data.to_vec()),
                                ID_BLOCK_DURATION => duration = Some(read_uint(data)),
                                _ => {}
                            });
                            if let Some(block) = block {
                                self.block(&block, duration);
                            }
                        }
                    }
                }
                _ if size == UNKNOWN_SIZE => {
                    self.synced = false; // unexpected unknown-size element
                }
                _ => {
                    // anything else (large blocks, cues, tags, …): skip it
                    let total = hl as u64 + size;
                    if (self.buf.len() as u64) >= total {
                        self.buf.drain(..total as usize);
                    } else {
                        self.skip = total - self.buf.len() as u64;
                        self.buf.clear();
                        return;
                    }
                }
            }
        }
    }

    /// Find the next cluster start (a seek landed mid-file). Returns false if more data is needed.
    fn resync(&mut self) -> bool {
        if self.buf.starts_with(&ID_EBML.to_be_bytes()) {
            self.synced = true;
            return true;
        }
        let pattern = ID_CLUSTER.to_be_bytes();
        let mut from = 0;
        while let Some(i) = self.buf[from..].windows(4).position(|w| w == pattern).map(|i| i + from) {
            match read_header(&self.buf[i..]) {
                Some((_, size, hl)) => {
                    // a real cluster starts with its timecode (or position/prev-size/CRC)
                    match read_header(&self.buf[i + hl..]).map(|(cid, ..)| cid) {
                        Some(ID_TIMECODE | 0xA7 | 0xAB | 0xBF) if size > 8 => {
                            self.buf.drain(..i);
                            self.synced = true;
                            return true;
                        }
                        None if self.buf.len() - i < 24 => {
                            self.buf.drain(..i);
                            return false; // need more bytes to validate
                        }
                        _ => from = i + 1,
                    }
                }
                None => {
                    self.buf.drain(..i);
                    return false;
                }
            }
        }
        let keep = self.buf.len().min(3);
        self.buf.drain(..self.buf.len() - keep);
        false
    }

    fn block(&mut self, data: &[u8], duration: Option<u64>) {
        let Some((track, tl)) = read_vint(data, false) else { return };
        let Some(t) = self.info.tracks.get(&track).cloned() else { return };
        let Some(codec) = t.codec else { return };
        if data.len() < tl + 3 {
            return;
        }
        let rel = i16::from_be_bytes([data[tl], data[tl + 1]]) as i64;
        let flags = data[tl + 2];
        if flags & 0x06 != 0 {
            return; // laced: never used for subtitles
        }
        let payload = String::from_utf8_lossy(&data[tl + 3..]).into_owned();
        let text = match codec {
            SubCodec::Srt | SubCodec::WebVtt => payload,
            // ASS in Matroska: ReadOrder,Layer,Style,Name,MarginL,MarginR,MarginV,Effect,Text
            SubCodec::Ass => payload.splitn(9, ',').nth(8).unwrap_or("").to_string(),
        };
        let start = self.ms(self.cluster_tc as i64 + rel);
        let length = match duration {
            Some(d) => self.ms(d as i64),
            None => t.default_duration_ms.unwrap_or(4000),
        };
        (self.emit)(t.n, start, start + length, text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(id: u32, body: &[u8]) -> Vec<u8> {
        let id_bytes: Vec<u8> = id.to_be_bytes().into_iter().skip_while(|b| *b == 0).collect();
        let mut out = id_bytes;
        out.push(0x01); // 8-byte size: marker byte, then 7 bytes of value
        out.extend_from_slice(&(body.len() as u64).to_be_bytes()[1..]);
        out.extend_from_slice(body);
        out
    }

    fn sample_file() -> Vec<u8> {
        let track_video = el(ID_TRACK_ENTRY, &[el(ID_TRACK_NUMBER, &[1]), el(ID_TRACK_TYPE, &[1]), el(ID_CODEC_ID, b"V_MPEG4/ISO/AVC")].concat());
        let track_sub = el(ID_TRACK_ENTRY, &[el(ID_TRACK_NUMBER, &[3]), el(ID_TRACK_TYPE, &[17]), el(ID_CODEC_ID, b"S_TEXT/UTF8")].concat());
        let track_ass = el(ID_TRACK_ENTRY, &[el(ID_TRACK_NUMBER, &[4]), el(ID_TRACK_TYPE, &[17]), el(ID_CODEC_ID, b"S_TEXT/ASS")].concat());
        let header = [
            el(ID_EBML, &[0x42, 0x82, 0x84, b'w', b'e', b'b', b'm']),
            vec![0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF], // segment, unknown size
            el(ID_INFO, &el(ID_TIMECODE_SCALE, &[0x0F, 0x42, 0x40])),
            el(ID_TRACKS, &[track_video, track_sub, track_ass].concat()),
        ]
        .concat();
        let video_block = el(ID_SIMPLE_BLOCK, &[&[0x81, 0, 0, 0x80][..], &vec![0xAB; 300_000]].concat()); // big: skipped
        let sub = el(ID_BLOCK_GROUP, &[el(ID_BLOCK, &[&[0x83, 0x03, 0xE8, 0x00][..], "Bună ziua".as_bytes()].concat()), el(ID_BLOCK_DURATION, &[0x07, 0xD0])].concat());
        let ass = el(ID_BLOCK_GROUP, &[el(ID_BLOCK, &[&[0x84, 0x07, 0xD0, 0x00][..], b"1,0,Default,,0,0,0,,{\\an8}Line one\\Nline two"].concat()), el(ID_BLOCK_DURATION, &[0x03, 0xE8])].concat());
        let cluster = el(ID_CLUSTER, &[el(ID_TIMECODE, &[0x27, 0x10]), video_block, sub, ass].concat()); // cluster at 10 s
        [header, cluster].concat()
    }

    fn collect(bytes: &[u8], chunk: usize) -> Vec<(usize, u64, u64, String)> {
        let info = Arc::new(parse_mkv_header(bytes).expect("header"));
        let out = Arc::new(Mutex::new(Vec::new()));
        let sink = out.clone();
        let mut tap = MkvTap::new(info, Box::new(move |n, s, e, t| sink.lock().unwrap().push((n, s, e, clean_text(&t)))));
        for c in bytes.chunks(chunk) {
            tap.feed(c);
        }
        let v = out.lock().unwrap().clone();
        v
    }

    #[test]
    fn parses_header_tracks() {
        let info = parse_mkv_header(&sample_file()).unwrap();
        assert_eq!(info.timecode_scale, 1_000_000);
        assert_eq!(info.tracks[&3].n, 0);
        assert_eq!(info.tracks[&3].codec, Some(SubCodec::Srt));
        assert_eq!(info.tracks[&4].codec, Some(SubCodec::Ass));
    }

    #[test]
    fn extracts_cues_from_stream_in_any_chunking() {
        let expected = vec![(0, 11_000, 13_000, "Bună ziua".to_string()), (1, 12_000, 13_000, "Line one\nline two".to_string())];
        for chunk in [7, 1000, 65_536, 1 << 20] {
            assert_eq!(collect(&sample_file(), chunk), expected, "chunk size {chunk}");
        }
    }

    #[test]
    fn resyncs_when_starting_mid_file() {
        let file = sample_file();
        let cluster_at = file.windows(4).position(|w| w == ID_CLUSTER.to_be_bytes()).unwrap();
        let info = Arc::new(parse_mkv_header(&file).unwrap());
        let out = Arc::new(Mutex::new(Vec::new()));
        let sink = out.clone();
        let mut tap = MkvTap::new(info, Box::new(move |n, s, _, _| sink.lock().unwrap().push((n, s))));
        tap.feed(&file[cluster_at - 37..]); // a seek landed a little before the cluster
        assert_eq!(*out.lock().unwrap(), vec![(0, 11_000), (1, 12_000)]);
    }

    #[test]
    fn vtt_blocks() {
        assert_eq!(parse_vtt_block("00:00.914 --> 00:04.083\nCum vrei.\nDar"), Some((0.914, 4.083, "Cum vrei.\nDar".into())));
    }
}
