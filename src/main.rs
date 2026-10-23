//! FileList Cinema: browse FileList movies, download them with qBittorrent and stream them to any browser
//! (PC, phone, TV). A single native executable with the web UI built in.

mod config;
mod filelist;
mod library;
mod livesubs;
mod media;
mod opensubs;
mod posters;
mod qbit;
mod routes;
mod subtitles;
mod torrent;
mod util;

use std::collections::HashSet;
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::Duration;

use serde_json::{Map, Value};

use crate::config::Settings;
use crate::filelist::{FileList, Page};
use crate::library::Library;
use crate::media::Media;
use crate::opensubs::OpenSubs;
use crate::posters::Posters;
use crate::qbit::{Credentials, Qbit};
use crate::subtitles::Subtitles;
use crate::util::TtlCache;

pub struct AppState {
    pub settings: RwLock<Settings>,
    pub filelist: RwLock<Option<Arc<FileList>>>,
    pub listings: TtlCache<Page>,
    pub posters: Arc<Posters>,
    pub qbit: Arc<Qbit>,
    pub lib: Arc<Library>,
    pub media: Arc<Media>,
    pub subs: Arc<Subtitles>,
    pub progress: StdMutex<Map<String, Value>>,
    pub progress_file: PathBuf,
    pub active_streams: Arc<AtomicUsize>,
    pub stream_generation: AtomicU64,
    pub build: String,
    pub media_log: PathBuf,
    pub seen_agents: StdMutex<HashSet<String>>,
    pub noise: bytes::Bytes,
    pub live: Arc<livesubs::LiveCues>,
    /// Matroska header info per "id:index" (subtitle tracks), for picking subtitles out of streamed bytes
    pub mkv: tokio::sync::Mutex<std::collections::HashMap<String, Option<Arc<livesubs::MkvInfo>>>>,
}

impl AppState {
    pub fn filelist(&self) -> Option<Arc<FileList>> {
        self.filelist.read().unwrap().clone()
    }

    pub fn save_progress(&self) {
        let json = serde_json::to_string(&*self.progress.lock().unwrap()).unwrap_or_default();
        let _ = std::fs::write(&self.progress_file, json);
    }

    /// Media access log (data/media.log): helps diagnose players on TVs and other devices.
    pub fn log_media(&self, line: &str) {
        let entry = format!("{} {line}", timestamp());
        println!("{entry}");
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.media_log) {
            let _ = writeln!(f, "{entry}");
        }
    }
}

fn timestamp() -> String {
    // ISO-8601 UTC without pulling in a date crate
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// Howard Hinnant's days-to-date algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// The PC's address on the local network (no packets are sent: connecting a UDP socket only picks a route).
fn lan_ip() -> Option<std::net::IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    socket.local_addr().ok().map(|a| a.ip())
}

fn noise(len: usize) -> bytes::Bytes {
    // incompressible test data for the connection speed test
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15 ^ util::now_ms();
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect::<Vec<u8>>()
        .into()
}

#[tokio::main]
async fn main() {
    let packaged = !cfg!(debug_assertions);
    if packaged {
        // keep the console open long enough to read a crash
        std::panic::set_hook(Box::new(|info| {
            eprintln!("\nFileList Cinema stopped because of an error:\n{info}\n\nThis window closes in 60 seconds.");
            std::thread::sleep(Duration::from_secs(60));
        }));
    }
    let _ = std::process::Command::new("cmd").args(["/c", "title FileList Cinema"]).status();

    let settings = Settings::load();
    let _ = std::fs::create_dir_all(&settings.data_dir);

    let ffmpeg = config::find_tool("ffmpeg", settings.ffmpeg_path.as_deref());
    let ffprobe = config::find_tool("ffprobe", settings.ffprobe_path.as_deref());

    let filelist = match FileList::create(&settings.filelist_username, &settings.filelist_password, &settings.filelist_passkey) {
        Ok(f) => Some(Arc::new(f)),
        Err(err) => {
            println!("Setup needed: {err}");
            None
        }
    };
    let qbit = Arc::new(Qbit::new(
        Credentials {
            url: settings.qbit_url.clone(),
            username: settings.qbit_username.clone(),
            password: settings.qbit_password.clone(),
        },
        settings.qbit_path.clone().or_else(config::default_qbit_exe),
    ));
    let lib = Arc::new(Library::new(qbit.clone(), settings.download_dir.clone(), &settings.data_dir));
    let live = Arc::new(livesubs::LiveCues::default());
    let media = Media::new(ffmpeg.clone(), ffprobe, live.clone());
    let posters = Posters::new(&settings.data_dir);
    let opensubs = OpenSubs::new(
        &settings.opensubtitles_api_key,
        &settings.opensubtitles_username,
        &settings.opensubtitles_password,
        &settings.opensubtitles_languages,
    );

    let active_streams = Arc::new(AtomicUsize::new(0));
    let busy = {
        let active = active_streams.clone();
        let lib = lib.clone();
        // background subtitle extraction waits while someone is watching or qBittorrent is verifying files
        Box::new(move || active.load(Ordering::Relaxed) > 0 || lib.checking_count.load(Ordering::Relaxed) > 0)
    };
    let subs = Subtitles::new(
        lib.clone(),
        media.clone(),
        posters.clone(),
        opensubs,
        &settings.data_dir,
        format!("http://127.0.0.1:{}", settings.port),
        busy,
    );

    let progress_file = settings.data_dir.join("progress.json");
    let progress: Map<String, Value> =
        std::fs::read_to_string(&progress_file).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();

    let state = Arc::new(AppState {
        filelist: RwLock::new(filelist),
        listings: TtlCache::default(),
        posters,
        qbit: qbit.clone(),
        lib: lib.clone(),
        media: media.clone(),
        subs,
        progress: StdMutex::new(progress),
        progress_file,
        active_streams,
        stream_generation: AtomicU64::new(0),
        build: util::base36_now(),
        media_log: settings.data_dir.join("media.log"),
        seen_agents: StdMutex::new(HashSet::new()),
        noise: noise(1 << 20),
        live,
        mkv: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        settings: RwLock::new(settings.clone()),
    });

    // background startup work
    {
        let (qbit, lib, settings) = (qbit.clone(), lib.clone(), settings.clone());
        tokio::spawn(async move {
            qbit.configure_throttle(settings.stream_limit_down_kib, settings.stream_limit_up_kib, settings.data_dir.join("throttle.json"))
                .await;
            qbit.ensure_preferences().await;
            if let Err(err) = lib.restore().await {
                eprintln!("[qbit] {err:#}");
            }
        });
        let media = media.clone();
        tokio::spawn(async move { media.detect_encoder().await });
    }

    let addr: SocketAddr = format!("{}:{}", settings.host, settings.port)
        .parse()
        .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], settings.port)));
    let local_url = format!("http://localhost:{}", settings.port);
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
            // started twice (e.g. double-clicked again): just show the running app
            println!("FileList Cinema is already running on port {}. Opening it in the browser…", settings.port);
            config::open_browser(&local_url);
            tokio::time::sleep(Duration::from_secs(3)).await;
            return;
        }
        Err(err) => panic!("cannot listen on {addr}: {err}"),
    };

    println!("FileList Cinema {} running at {local_url}", env!("CARGO_PKG_VERSION"));
    if settings.host == "0.0.0.0" {
        if let Some(ip) = lan_ip() {
            println!("  on your network: http://{ip}:{}", settings.port);
        }
    }
    let mode = match state.filelist() {
        None => "setup needed",
        Some(f) if f.mode() == "web" => "website login",
        Some(_) => "API (passkey)",
    };
    println!("Mode: {mode} | app folder: {}", settings.root.display());
    match &ffmpeg {
        Some(path) => println!("ffmpeg: {path}"),
        None => println!("ffmpeg not found: only browser-native formats will play"),
    }
    if packaged {
        println!("\nKeep this window open while you use the app. Close it to stop FileList Cinema.");
        if settings.open_browser {
            config::open_browser(&local_url);
        }
    }

    let app = routes::router(state.clone());
    let server = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async { let _ = tokio::signal::ctrl_c().await; });
    if let Err(err) = server.await {
        eprintln!("server error: {err}");
    }
}
