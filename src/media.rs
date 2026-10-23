//! ffmpeg/ffprobe: probing (cached), choosing an encoder (Intel Quick Sync or libx264), converted streams
//! (fragmented MP4) and HLS for reduced-quality playback with normal seeking.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::Mutex;
use tokio_util::io::ReaderStream;

use crate::config::tokio_command;
use crate::livesubs::LiveCues;

pub const HLS_SEGMENT_SECONDS: u32 = 6;

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Preset {
    pub height: u32,
    pub mbps: u32,
}

/// Reduced-quality presets for slow networks.
pub const PRESETS: &[(&str, Preset)] = &[
    ("8m", Preset { height: 1080, mbps: 8 }),
    ("4m", Preset { height: 720, mbps: 4 }),
    ("2m", Preset { height: 720, mbps: 2 }),
    ("1m", Preset { height: 480, mbps: 1 }),
];

pub fn preset(name: &str) -> Option<Preset> {
    PRESETS.iter().find(|(n, _)| *n == name).map(|(_, p)| *p)
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioStream {
    pub index: usize,
    pub codec: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub channels: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubtitleStream {
    pub n: usize, // ffmpeg's 0:s:n
    pub codec: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Probe {
    pub duration: Option<f64>,
    pub bitrate: Option<f64>,
    pub video_codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub hdr: bool,
    pub audio: Vec<AudioStream>,
    pub subtitles: Vec<SubtitleStream>,
}

/// Text subtitle codecs ffmpeg can write as WebVTT (image-based PGS/VobSub can't be).
const TEXT_SUBTITLES: &[&str] = &["subrip", "srt", "ass", "ssa", "webvtt", "mov_text", "text"];

/// Cleared when the ffmpeg writing subtitle side files stops, so their readers finish up.
struct AliveGuard(Arc<AtomicBool>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Encoder {
    Qsv,
    X264,
}

pub struct Media {
    pub ffmpeg: Option<String>,
    pub ffprobe: Option<String>,
    probes: StdMutex<HashMap<String, Probe>>,
    qsv: AtomicBool,
    qsv_failed: StdMutex<std::collections::HashSet<String>>, // files Quick Sync could not handle
    hls_root: PathBuf,
    sessions: Mutex<HashMap<String, Arc<HlsSession>>>,
    live: Arc<LiveCues>,
}

struct HlsSession {
    dir: PathBuf,
    start: u32,
    encoder: Encoder,
    child: Mutex<Child>,
    last_access: StdMutex<Instant>,
    _subtitles: AliveGuard,
}

/// What the viewer's device can play (sent by the page, detected with MediaSource.isTypeSupported).
#[derive(Clone, Debug, Default)]
pub struct Caps {
    pub video: Vec<String>,
    pub audio: Vec<String>,
}

impl Caps {
    pub fn parse(video: Option<&str>, audio: Option<&str>) -> Caps {
        let split = |s: Option<&str>| s.unwrap_or("").split(',').map(|x| x.trim().to_lowercase()).filter(|x| !x.is_empty()).collect();
        Caps { video: split(video), audio: split(audio) }
    }
}

/// Audio codecs that may be copied into fragmented MP4 (if the device can decode them).
const MP4_AUDIO: &[&str] = &["aac", "mp3", "ac3", "eac3", "flac", "opus"];

impl Media {
    pub fn new(ffmpeg: Option<String>, ffprobe: Option<String>, live: Arc<LiveCues>) -> Arc<Media> {
        let hls_root = std::env::temp_dir().join("filelist-cinema-hls");
        let _ = std::fs::remove_dir_all(&hls_root); // leftovers from a previous run
        let _ = std::fs::create_dir_all(&hls_root);
        let media = Arc::new(Media {
            ffmpeg,
            ffprobe,
            probes: StdMutex::new(HashMap::new()),
            qsv: AtomicBool::new(false),
            qsv_failed: StdMutex::new(Default::default()),
            hls_root,
            sessions: Mutex::new(HashMap::new()),
            live,
        });
        // idle HLS sessions: stop ffmpeg and delete their segments
        let m = media.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let mut sessions = m.sessions.lock().await;
                let idle: Vec<String> = sessions
                    .iter()
                    .filter(|(_, s)| s.last_access.lock().unwrap().elapsed() > Duration::from_secs(120))
                    .map(|(k, _)| k.clone())
                    .collect();
                for key in idle {
                    if let Some(s) = sessions.remove(&key) {
                        let _ = s.child.lock().await.start_kill();
                        let _ = tokio::fs::remove_dir_all(&s.dir).await;
                    }
                }
            }
        });
        media
    }

    pub fn encoder(&self, file_key: &str) -> Encoder {
        if self.qsv.load(Ordering::Relaxed) && !self.qsv_failed.lock().unwrap().contains(file_key) {
            Encoder::Qsv
        } else {
            Encoder::X264
        }
    }

    /// Check once whether Intel Quick Sync works here (decode → resize → encode on the GPU). On this kind of
    /// laptop (Intel + NVIDIA) it works through DXVA2 but not D3D11, so that's the device type used.
    pub async fn detect_encoder(&self) {
        let Some(ffmpeg) = &self.ffmpeg else { return };
        let args = [
            "-hide_banner", "-loglevel", "error",
            "-init_hw_device", "qsv=qs:hw_any,child_device_type=dxva2", "-filter_hw_device", "qs",
            "-f", "lavfi", "-i", "testsrc2=size=1920x1080:rate=24", "-t", "1",
            "-vf", "format=nv12,hwupload=extra_hw_frames=16,scale_qsv=w=1280:h=720",
            "-c:v", "h264_qsv", "-f", "null", "-",
        ];
        let ok = tokio_command(ffmpeg)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        self.qsv.store(ok, Ordering::Relaxed);
        println!("Video encoder: {}", if ok { "Intel Quick Sync (GPU)" } else { "libx264 (CPU)" });
    }

    pub fn encoder_name(&self) -> &'static str {
        if self.qsv.load(Ordering::Relaxed) { "qsv" } else { "x264" }
    }

    // ---------- probing (#8: cached per file) ----------

    pub async fn probe(&self, key: &str, input: &str) -> Result<Probe> {
        if let Some(p) = self.probes.lock().unwrap().get(key) {
            return Ok(p.clone());
        }
        let ffprobe = self.ffprobe.as_ref().context("ffprobe is not installed")?;
        let out = tokio::time::timeout(
            Duration::from_secs(60),
            tokio_command(ffprobe)
                .args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams", input])
                .stdin(Stdio::null())
                .output(),
        )
        .await
        .context("ffprobe timed out")??;
        let json: Value = serde_json::from_slice(&out.stdout).context("ffprobe returned no information")?;
        let streams = json["streams"].as_array().cloned().unwrap_or_default();
        let video = streams.iter().find(|s| s["codec_type"] == "video" && s["disposition"]["attached_pic"] != 1);
        let transfer = video.and_then(|v| v["color_transfer"].as_str()).unwrap_or("");
        let probe = Probe {
            duration: json["format"]["duration"].as_str().and_then(|d| d.parse().ok()),
            bitrate: json["format"]["bit_rate"].as_str().and_then(|d| d.parse().ok()),
            video_codec: video.and_then(|v| v["codec_name"].as_str()).map(str::to_string),
            width: video.and_then(|v| v["width"].as_u64()).map(|w| w as u32),
            height: video.and_then(|v| v["height"].as_u64()).map(|h| h as u32),
            hdr: transfer == "smpte2084" || transfer == "arib-std-b67",
            audio: streams
                .iter()
                .filter(|s| s["codec_type"] == "audio")
                .enumerate()
                .map(|(i, s)| AudioStream {
                    index: i,
                    codec: s["codec_name"].as_str().unwrap_or("").to_string(),
                    language: s["tags"]["language"].as_str().map(str::to_string),
                    title: s["tags"]["title"].as_str().map(str::to_string),
                    channels: s["channels"].as_u64(),
                })
                .collect(),
            subtitles: streams
                .iter()
                .filter(|s| s["codec_type"] == "subtitle")
                .enumerate()
                .map(|(n, s)| SubtitleStream { n, codec: s["codec_name"].as_str().unwrap_or("").to_string() })
                .collect(),
        };
        if probe.duration.is_some() {
            self.probes.lock().unwrap().insert(key.to_string(), probe.clone());
        }
        Ok(probe)
    }

    pub fn forget(&self, id: &str) {
        self.probes.lock().unwrap().retain(|k, _| !k.starts_with(&format!("{id}:")));
    }

    // ---------- shared argument builders ----------

    fn hw_input_args(encoder: Encoder) -> Vec<String> {
        match encoder {
            Encoder::Qsv => [
                "-init_hw_device", "qsv=qs:hw_any,child_device_type=dxva2", "-filter_hw_device", "qs",
                "-hwaccel", "qsv", "-hwaccel_device", "qs", "-hwaccel_output_format", "qsv",
            ]
            .map(String::from)
            .to_vec(),
            Encoder::X264 => vec![],
        }
    }

    /// Video encoding to H.264 at `target_height` (None = source size) and `mbps` (None = quality-based).
    fn encode_args(encoder: Encoder, probe: &Probe, target_height: Option<u32>, mbps: Option<u32>) -> Vec<String> {
        let (sw, sh) = (probe.width.unwrap_or(1920), probe.height.unwrap_or(1080));
        let (w, h) = match target_height {
            Some(t) if t < sh => (((sw as f64 * t as f64 / sh as f64) / 2.0).round() as u32 * 2, t),
            _ => (sw, sh),
        };
        let mut args: Vec<String> = Vec::new();
        match encoder {
            Encoder::Qsv => {
                // always pass through the GPU scaler: it also converts 10-bit (HEVC/HDR) to 8-bit for H.264
                let filter = if probe.hdr {
                    format!("vpp_qsv=w={w}:h={h}:format=nv12:tonemap=1")
                } else {
                    format!("scale_qsv=w={w}:h={h}:format=nv12")
                };
                args.extend(["-vf".into(), filter, "-c:v".into(), "h264_qsv".into(), "-preset".into(), "veryfast".into()]);
            }
            Encoder::X264 => {
                let mut chain = Vec::new();
                if probe.hdr {
                    chain.push(
                        "zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv"
                            .to_string(),
                    );
                }
                if h != sh {
                    chain.push(format!("scale={w}:{h}"));
                }
                chain.push("format=yuv420p".into());
                args.extend(["-vf".into(), chain.join(","), "-c:v".into(), "libx264".into(), "-preset".into(), "veryfast".into()]);
            }
        }
        match mbps {
            Some(m) => args.extend([
                "-b:v".into(), format!("{m}M"), "-maxrate".into(), format!("{m}M"), "-bufsize".into(), format!("{}M", m * 2),
            ]),
            None if encoder == Encoder::X264 => args.extend(["-crf".into(), "21".into()]),
            None => args.extend(["-global_quality".into(), "23".into()]),
        }
        args
    }

    fn audio_args(probe: &Probe, audio_index: usize, caps: &Caps, allow_copy: bool, low: bool) -> Vec<String> {
        let codec = probe.audio.get(audio_index).map(|a| a.codec.as_str()).unwrap_or("");
        if allow_copy && MP4_AUDIO.contains(&codec) && caps.audio.iter().any(|c| c == codec) {
            return vec!["-c:a".into(), "copy".into()];
        }
        ["-c:a", "aac", "-ac", "2", "-b:a", if low { "128k" } else { "192k" }].map(String::from).to_vec()
    }

    /// Extra ffmpeg outputs writing every text subtitle track to a WebVTT side file in the same pass (the
    /// subtitles are in the data ffmpeg reads anyway). Returns the arguments and (file, cue store key) pairs.
    fn subtitle_outputs(file_key: &str, probe: &Probe, dir: &std::path::Path) -> (Vec<String>, Vec<(PathBuf, String)>) {
        let mut args = Vec::new();
        let mut files = Vec::new();
        for s in probe.subtitles.iter().filter(|s| TEXT_SUBTITLES.contains(&s.codec.as_str())) {
            let file = dir.join(format!("subs{}.vtt", s.n));
            args.extend([
                "-map".into(), format!("0:s:{}", s.n), "-c:s".into(), "webvtt".into(),
                "-flush_packets".into(), "1".into(), "-f".into(), "webvtt".into(), "-y".into(),
                file.to_string_lossy().into_owned(),
            ]);
            files.push((file, format!("{file_key}:{}", s.n)));
        }
        (args, files)
    }

    /// Read the side files as ffmpeg writes them; cue times are relative to where ffmpeg started (`offset`).
    fn follow_subtitles(&self, files: Vec<(PathBuf, String)>, offset: f64) -> AliveGuard {
        let alive = Arc::new(AtomicBool::new(true));
        for (file, key) in files {
            self.live.follow_vtt(file, key, offset, alive.clone());
        }
        AliveGuard(alive)
    }

    // ---------- converted stream (fragmented MP4, restarts on seek) ----------

    /// Full-quality conversion for devices that can't play the file directly. #2: the video is copied untouched
    /// whenever the device can decode its codec (e.g. HEVC/4K on the TV); usually only the audio is converted.
    pub async fn converted_stream(
        self: &Arc<Self>,
        file_key: &str,
        input: &str,
        start: f64,
        audio_index: usize,
        caps: &Caps,
    ) -> Result<impl Stream<Item = std::io::Result<Bytes>> + Send + 'static> {
        let ffmpeg = self.ffmpeg.clone().context("ffmpeg is not installed")?;
        let probe = self.probe(file_key, input).await.unwrap_or(Probe {
            duration: None, bitrate: None, video_codec: None, width: None, height: None, hdr: false, audio: vec![], subtitles: vec![],
        });
        let sub_dir = self.hls_root.join(format!("convert-{}-{}", sanitize(file_key), crate::util::now_ms()));
        let _ = tokio::fs::create_dir_all(&sub_dir).await;
        let codec = probe.video_codec.clone().unwrap_or_default();
        let copy_video = caps.video.iter().any(|c| *c == codec) && matches!(codec.as_str(), "h264" | "hevc" | "av1" | "vp9");
        let mut encoder = self.encoder(file_key);
        loop {
            let mut args: Vec<String> = ["-hide_banner", "-loglevel", "error"].map(String::from).to_vec();
            if !copy_video {
                args.extend(Self::hw_input_args(encoder));
            }
            args.extend(["-ss".into(), format!("{start:.3}"), "-i".into(), input.to_string()]);
            args.extend(["-map", "0:v:0", "-map"].map(String::from));
            args.push(format!("0:a:{audio_index}?"));
            args.push("-sn".into());
            if copy_video {
                args.extend(["-c:v".into(), "copy".into()]);
                if codec == "hevc" {
                    args.extend(["-tag:v".into(), "hvc1".into()]);
                }
            } else {
                args.extend(Self::encode_args(encoder, &probe, None, None));
            }
            args.extend(Self::audio_args(&probe, audio_index, caps, true, false));
            args.extend(["-movflags", "frag_keyframe+empty_moov+default_base_moof", "-f", "mp4", "pipe:1"].map(String::from));
            let (sub_args, sub_files) = Self::subtitle_outputs(file_key, &probe, &sub_dir);
            args.extend(sub_args);

            let mut child = tokio_command(&ffmpeg)
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            let stderr = log_stderr(child.stderr.take(), file_key);
            let mut stdout = child.stdout.take().context("no ffmpeg output")?;
            let mut first = vec![0u8; 256 * 1024];
            let n = tokio::time::timeout(Duration::from_secs(60), stdout.read(&mut first)).await.unwrap_or(Ok(0)).unwrap_or(0);
            if n == 0 {
                let _ = child.start_kill();
                let detail = stderr.lock().unwrap().clone();
                if encoder == Encoder::Qsv && !copy_video {
                    eprintln!("[ffmpeg] Quick Sync could not convert {file_key}, using the CPU encoder: {detail}");
                    self.qsv_failed.lock().unwrap().insert(file_key.to_string());
                    encoder = Encoder::X264;
                    continue;
                }
                bail!("ffmpeg produced no output: {detail}");
            }
            first.truncate(n);
            let head = futures_util::stream::once(async move { Ok(Bytes::from(first)) });
            let subtitles = self.follow_subtitles(sub_files, start);
            // the child moves into the stream: when the viewer disconnects, the stream is dropped and ffmpeg killed
            let body = ReaderStream::new(stdout).map(move |chunk| {
                let _keep_alive = (&child, &subtitles);
                chunk
            });
            return Ok(head.chain(body));
        }
    }

    // ---------- HLS (#5): reduced quality with real seeking ----------

    pub fn hls_playlist(duration: f64, query: &str) -> String {
        let seg = HLS_SEGMENT_SECONDS as f64;
        let count = (duration / seg).ceil().max(1.0) as u32;
        let mut out = format!(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:{}\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-INDEPENDENT-SEGMENTS\n",
            HLS_SEGMENT_SECONDS
        );
        for n in 0..count {
            let len = if n + 1 == count { duration - seg * n as f64 } else { seg };
            out.push_str(&format!("#EXTINF:{:.3},\nseg{n}.ts?{query}\n", len.max(0.1)));
        }
        out.push_str("#EXT-X-ENDLIST\n");
        out
    }

    /// Path of segment `n`, producing it if needed. One ffmpeg per session writes segments ahead of playback;
    /// a jump outside what it has (or will soon have) restarts it at the requested segment.
    pub async fn hls_segment(
        self: &Arc<Self>,
        session_key: &str,
        file_key: &str,
        input: &str,
        preset: Preset,
        audio_index: usize,
        n: u32,
    ) -> Result<PathBuf> {
        let probe = self.probe(file_key, input).await?;
        let mut encoder = self.encoder(file_key);
        for _attempt in 0..2 {
            let session = {
                let mut sessions = self.sessions.lock().await;
                let reuse = match sessions.get(session_key) {
                    Some(s) if n >= s.start && n <= self.produced(s).await.unwrap_or(s.start) + 4 && s.encoder == encoder => {
                        Some(s.clone())
                    }
                    _ => None,
                };
                match reuse {
                    Some(s) => s,
                    None => {
                        if let Some(old) = sessions.remove(session_key) {
                            let _ = old.child.lock().await.start_kill();
                            let _ = tokio::fs::remove_dir_all(&old.dir).await;
                        }
                        let s = Arc::new(self.start_hls(session_key, file_key, input, &probe, preset, audio_index, n, encoder).await?);
                        sessions.insert(session_key.to_string(), s.clone());
                        s
                    }
                }
            };
            *session.last_access.lock().unwrap() = Instant::now();
            let segment = session.dir.join(format!("seg{n}.ts"));
            let deadline = Instant::now() + Duration::from_secs(120);
            loop {
                if tokio::fs::metadata(&segment).await.is_ok() {
                    return Ok(segment); // written as .tmp and renamed when complete
                }
                let exited = session.child.lock().await.try_wait()?.is_some();
                if exited {
                    // ffmpeg finished or failed without producing this segment
                    if tokio::fs::metadata(&segment).await.is_ok() {
                        return Ok(segment);
                    }
                    if encoder == Encoder::Qsv && self.produced(&session).await.is_none() {
                        eprintln!("[ffmpeg] Quick Sync could not convert {file_key}, using the CPU encoder");
                        self.qsv_failed.lock().unwrap().insert(file_key.to_string());
                        encoder = Encoder::X264;
                        break; // retry with libx264
                    }
                    bail!("ffmpeg stopped before producing segment {n}");
                }
                if Instant::now() > deadline {
                    bail!("timed out waiting for segment {n}");
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
        Err(anyhow!("could not convert this video"))
    }

    /// Highest segment number already complete in a session (None if none yet).
    async fn produced(&self, s: &HlsSession) -> Option<u32> {
        let mut highest: Option<u32> = None;
        if let Ok(mut dir) = tokio::fs::read_dir(&s.dir).await {
            while let Ok(Some(entry)) = dir.next_entry().await {
                let name = entry.file_name().to_string_lossy().into_owned();
                if let Some(num) = name.strip_prefix("seg").and_then(|r| r.strip_suffix(".ts")).and_then(|r| r.parse::<u32>().ok()) {
                    highest = Some(highest.map_or(num, |h| h.max(num)));
                }
            }
        }
        highest
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_hls(
        &self,
        session_key: &str,
        file_key: &str,
        input: &str,
        probe: &Probe,
        preset: Preset,
        audio_index: usize,
        start_segment: u32,
        encoder: Encoder,
    ) -> Result<HlsSession> {
        let ffmpeg = self.ffmpeg.clone().context("ffmpeg is not installed")?;
        let dir = self.hls_root.join(sanitize(session_key)).join(format!("{start_segment}-{}", crate::util::now_ms()));
        tokio::fs::create_dir_all(&dir).await?;
        let offset = (start_segment * HLS_SEGMENT_SECONDS) as f64;
        let seg = HLS_SEGMENT_SECONDS;
        let mut args: Vec<String> = ["-hide_banner", "-loglevel", "error"].map(String::from).to_vec();
        args.extend(Self::hw_input_args(encoder));
        args.extend(["-ss".into(), format!("{offset:.3}"), "-i".into(), input.to_string()]);
        args.extend(["-map".into(), "0:v:0".into(), "-map".into(), format!("0:a:{audio_index}?"), "-sn".into()]);
        args.extend(Self::encode_args(encoder, probe, Some(preset.height), Some(preset.mbps)));
        // a keyframe exactly at every segment boundary, so segments are exactly `seg` seconds long
        args.extend(["-force_key_frames".into(), format!("expr:gte(t,n_forced*{seg})")]);
        if encoder == Encoder::Qsv {
            args.extend(["-forced_idr".into(), "1".into()]);
        }
        let caps = Caps::default(); // HLS audio is always AAC stereo
        args.extend(Self::audio_args(probe, audio_index, &caps, false, preset.mbps <= 2));
        args.extend([
            "-output_ts_offset".into(), format!("{offset:.3}"),
            "-f".into(), "hls".into(),
            "-hls_time".into(), seg.to_string(),
            "-hls_list_size".into(), "0".into(),
            "-hls_segment_type".into(), "mpegts".into(),
            "-hls_flags".into(), "independent_segments+temp_file".into(),
            "-start_number".into(), start_segment.to_string(),
            "-hls_segment_filename".into(), dir.join("seg%d.ts").to_string_lossy().into_owned(),
            dir.join("out.m3u8").to_string_lossy().into_owned(),
        ]);
        let (sub_args, sub_files) = Self::subtitle_outputs(file_key, probe, &dir);
        args.extend(sub_args);
        let mut child = tokio_command(&ffmpeg).args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()?;
        log_stderr(child.stderr.take(), session_key);
        Ok(HlsSession {
            dir,
            start: start_segment,
            encoder,
            child: Mutex::new(child),
            last_access: StdMutex::new(Instant::now()),
            _subtitles: self.follow_subtitles(sub_files, offset),
        })
    }

    /// Stop all HLS sessions of a download (it was removed).
    pub async fn stop_sessions(&self, id: &str) {
        let mut sessions = self.sessions.lock().await;
        let keys: Vec<String> = sessions.keys().filter(|k| k.starts_with(&format!("{id}:"))).cloned().collect();
        for k in keys {
            if let Some(s) = sessions.remove(&k) {
                let _ = s.child.lock().await.start_kill();
                let _ = tokio::fs::remove_dir_all(&s.dir).await;
            }
        }
    }
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect()
}

/// Log ffmpeg's stderr (it only prints errors with -loglevel error); keeps the last message for diagnostics.
fn log_stderr(stderr: Option<tokio::process::ChildStderr>, tag: &str) -> Arc<StdMutex<String>> {
    let last = Arc::new(StdMutex::new(String::new()));
    if let Some(stderr) = stderr {
        let last2 = last.clone();
        let tag = tag.to_string();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[ffmpeg {tag}] {line}");
                *last2.lock().unwrap() = line;
            }
        });
    }
    last
}
