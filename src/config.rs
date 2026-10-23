//! Where the app lives, its settings (.env next to the executable) and external tools (ffmpeg, qBittorrent).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// Hide the console window of helper processes (ffmpeg, cmd) on Windows.
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Start a process outside the app's job object, so it keeps running after the app exits.
pub const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

/// Put the app in a Windows job object that kills every process it started (ffmpeg, ffprobe) when the app
/// exits, however it exits: closing the console window or killing the process skips all cleanup code, and
/// would otherwise leave ffmpeg reading tens of gigabytes in the background. qBittorrent and the browser are
/// started with CREATE_BREAKAWAY_FROM_JOB, so they stay.
#[cfg(windows)]
pub fn kill_children_on_exit() {
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: plain Win32 calls with valid arguments; the job handle is deliberately never closed, so Windows
    // closes it when the app exits, which is what triggers killing the remaining child processes.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok != 0 {
            AssignProcessToJobObject(job, GetCurrentProcess());
        }
    }
}

#[cfg(not(windows))]
pub fn kill_children_on_exit() {}

/// The app folder: next to the .exe, or the project folder under `cargo run`. Holds .env, data/ and downloads/.
pub fn app_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("FLC_ROOT") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("CARGO_MANIFEST_DIR") {
        return PathBuf::from(dir);
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub root: PathBuf,
    pub port: u16,
    pub host: String,
    pub data_dir: PathBuf,
    pub download_dir: PathBuf,
    pub open_browser: bool,
    pub filelist_username: String,
    pub filelist_password: String,
    pub filelist_passkey: String,
    pub qbit_url: String,
    pub qbit_username: String,
    pub qbit_password: String,
    pub qbit_path: Option<PathBuf>,
    pub ffmpeg_path: Option<String>,
    pub ffprobe_path: Option<String>,
    pub opensubtitles_api_key: String,
    pub opensubtitles_username: String,
    pub opensubtitles_password: String,
    pub opensubtitles_languages: String,
}

impl Settings {
    pub fn env_file(&self) -> PathBuf {
        self.root.join(".env")
    }

    /// Read the settings: real environment variables first, then .env.
    pub fn load() -> Settings {
        let root = app_root();
        let file = std::fs::read_to_string(root.join(".env")).map(|t| parse_env(&t)).unwrap_or_default();
        let var = |k: &str| std::env::var(k).ok().or_else(|| file.get(k).cloned()).filter(|v| !v.trim().is_empty());
        let get = |k: &str, default: &str| var(k).unwrap_or_else(|| default.to_string());
        let path_in_root = |k: &str, default: &str| {
            let p = PathBuf::from(get(k, default));
            if p.is_absolute() { p } else { root.join(p) }
        };
        Settings {
            port: get("PORT", "3000").parse().unwrap_or(3000),
            host: get("HOST", "127.0.0.1"),
            data_dir: path_in_root("DATA_DIR", "data"),
            download_dir: path_in_root("DOWNLOAD_DIR", "downloads"),
            open_browser: get("OPEN_BROWSER", "1") != "0",
            filelist_username: get("FILELIST_USERNAME", ""),
            filelist_password: get("FILELIST_PASSWORD", ""),
            filelist_passkey: get("FILELIST_PASSKEY", ""),
            qbit_url: get("QBIT_URL", "http://127.0.0.1:8080"),
            qbit_username: get("QBIT_USERNAME", ""),
            qbit_password: get("QBIT_PASSWORD", ""),
            qbit_path: var("QBIT_PATH").map(PathBuf::from),
            ffmpeg_path: var("FFMPEG_PATH"),
            ffprobe_path: var("FFPROBE_PATH"),
            opensubtitles_api_key: get("OPENSUBTITLES_API_KEY", ""),
            opensubtitles_username: get("OPENSUBTITLES_USERNAME", ""),
            opensubtitles_password: get("OPENSUBTITLES_PASSWORD", ""),
            opensubtitles_languages: get("OPENSUBTITLES_LANGUAGES", "ro,en"),
            root,
        }
    }
}

/// Parse a .env file the way Node's dotenv does: unquoted values are literal (Windows paths keep their
/// backslashes) with ` #` starting a comment; '...' and `...` are literal; "..." expands \n.
pub fn parse_env(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let text = text.replace("\r\n", "\n");
    let mut lines = text.split('\n');
    while let Some(line) = lines.next() {
        let line = line.trim_start();
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, rest)) = line.split_once('=') else { continue };
        let key = key.trim();
        if key.is_empty() || key.starts_with('#') || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-') {
            continue;
        }
        let rest = rest.trim_start();
        let value = match rest.chars().next() {
            Some(q @ ('\'' | '"' | '`')) => {
                // quoted values may span lines
                let mut buf = rest[1..].to_string();
                while !buf.contains(q) {
                    match lines.next() {
                        Some(next) => {
                            buf.push('\n');
                            buf.push_str(next);
                        }
                        None => break,
                    }
                }
                let inner = buf.split(q).next().unwrap_or("").to_string();
                if q == '"' { inner.replace("\\n", "\n").replace("\\r", "\r") } else { inner }
            }
            _ => match rest.find(" #") {
                Some(i) => rest[..i].trim().to_string(),
                None => rest.trim().to_string(),
            },
        };
        out.insert(key.to_string(), value);
    }
    out
}

/// Update (or add) keys in the .env file, keeping comments and everything else as it is.
pub fn write_env(file: &Path, updates: &[(&str, String)]) -> std::io::Result<()> {
    let existing = std::fs::read_to_string(file).unwrap_or_default();
    let mut pending: HashMap<&str, &String> = updates.iter().map(|(k, v)| (*k, v)).collect();
    let mut out: Vec<String> = Vec::new();
    for line in existing.lines() {
        let key = line.split_once('=').map(|(k, _)| k.trim()).filter(|k| {
            !k.is_empty() && k.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        });
        match key.and_then(|k| pending.remove_entry(k)) {
            Some((k, v)) => out.push(format!("{k}={}", quote_env(v))),
            None => out.push(line.to_string()),
        }
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    // keep the order of `updates` for new keys
    for (k, v) in updates {
        if pending.contains_key(k) {
            out.push(format!("{k}={}", quote_env(v)));
        }
    }
    std::fs::write(file, format!("{}\r\n", out.join("\r\n")))
}

/// dotenv takes single- and backtick-quoted values literally.
fn quote_env(v: &str) -> String {
    if !v.chars().any(|c| c.is_whitespace() || "#'\"`\\".contains(c)) {
        return v.to_string();
    }
    if !v.contains('\'') {
        return format!("'{v}'");
    }
    if !v.contains('`') {
        return format!("`{v}`");
    }
    format!("\"{v}\"")
}

/// A std::process::Command that doesn't flash a console window.
pub fn quiet_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// A tokio Command that doesn't flash a console window and is killed when dropped.
pub fn tokio_command(program: impl AsRef<std::ffi::OsStr>) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd.kill_on_drop(true);
    cmd
}

fn works(cmd: &str) -> bool {
    quiet_command(cmd)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Locate ffmpeg/ffprobe: the configured path, then PATH, then a winget install (its folder name contains the
/// version, so it changes with every upgrade), then common folders. Newest wins.
pub fn find_tool(name: &str, configured: Option<&str>) -> Option<String> {
    if let Some(c) = configured {
        if works(c) {
            return Some(c.to_string());
        }
    }
    if works(name) {
        return Some(name.to_string());
    }
    let mut found: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    let mut consider = |exe: PathBuf| {
        if let Ok(meta) = std::fs::metadata(&exe) {
            found.push((exe, meta.modified().unwrap_or(std::time::UNIX_EPOCH)));
        }
    };
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let packages = PathBuf::from(local).join("Microsoft").join("WinGet").join("Packages");
        for pkg in read_dir_names(&packages).into_iter().filter(|d| d.to_lowercase().contains("ffmpeg")) {
            for build in read_dir_names(&packages.join(&pkg)) {
                consider(packages.join(&pkg).join(build).join("bin").join(format!("{name}.exe")));
            }
        }
    }
    consider(PathBuf::from(r"C:\ffmpeg\bin").join(format!("{name}.exe")));
    if let Some(pf) = std::env::var_os("ProgramFiles") {
        consider(PathBuf::from(pf).join("ffmpeg").join("bin").join(format!("{name}.exe")));
    }
    found.sort_by(|a, b| b.1.cmp(&a.1));
    found
        .into_iter()
        .map(|(p, _)| p.to_string_lossy().into_owned())
        .find(|p| works(p))
}

pub fn read_dir_names(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|it| it.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default()
}

pub fn default_qbit_exe() -> Option<PathBuf> {
    let pf = std::env::var_os("ProgramFiles")?;
    let exe = PathBuf::from(pf).join("qBittorrent").join("qbittorrent.exe");
    exe.exists().then_some(exe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_like_node_dotenv() {
        let env = parse_env(
            "# comment\r\nPORT=3000\r\nFFMPEG_PATH=C:\\Users\\user\\ffmpeg.exe\r\nA='p@ss #1 \"q\"'\r\nB=`it's`\r\nC=\"x\\ny\"\r\nD=value # note\r\nexport E= spaced \r\n",
        );
        assert_eq!(env["PORT"], "3000");
        assert_eq!(env["FFMPEG_PATH"], r"C:\Users\user\ffmpeg.exe");
        assert_eq!(env["A"], "p@ss #1 \"q\"");
        assert_eq!(env["B"], "it's");
        assert_eq!(env["C"], "x\ny");
        assert_eq!(env["D"], "value");
        assert_eq!(env["E"], "spaced");
    }

    #[test]
    fn write_env_round_trips() {
        let path = std::env::temp_dir().join(format!("flc-env-test-{}", std::process::id()));
        std::fs::write(&path, "# keep me\r\nPORT=3000\r\nQBIT_PASSWORD=old\r\n").unwrap();
        write_env(&path, &[("QBIT_PASSWORD", "it's a #secret".into()), ("NEW_KEY", r"C:\x y".into())]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let env = parse_env(&text);
        assert!(text.starts_with("# keep me"));
        assert_eq!(env["QBIT_PASSWORD"], "it's a #secret");
        assert_eq!(env["NEW_KEY"], r"C:\x y");
        assert_eq!(env["PORT"], "3000");
        let _ = std::fs::remove_file(path);
    }
}

pub fn open_browser(url: &str) {
    if cfg!(windows) {
        let mut cmd = quiet_command("cmd");
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB); // the browser must outlive the app
        let _ = cmd.args(["/c", "start", "", url]).spawn();
    }
}

/// Start a program that must keep running after the app exits (qBittorrent).
pub fn spawn_detached(program: &Path) -> std::io::Result<std::process::Child> {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_BREAKAWAY_FROM_JOB);
    cmd.spawn()
}
