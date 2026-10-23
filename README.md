# FileList Cinema

Browse the movies on [filelist.io](https://filelist.io) in a Netflix-style interface, download them with qBittorrent, and watch them in any browser on your network (PC, phone, TV), even while they're still downloading.

The app is a single native Windows executable, `FileListCinema.exe` (~7 MB). The server is written in Rust, and the web UI is built into the exe.

## Running it

1. Install **qBittorrent** (`winget install qBittorrent.qBittorrent`) and enable its Web UI: *Tools → Options → Web UI*, with a username and password. FileList's tracker only accepts whitelisted clients, so every download goes through qBittorrent.
2. Optional: install **ffmpeg** (`winget install ffmpeg`). It's needed to convert what a device can't play, for lower-quality streaming, and for built-in subtitles.
3. Double-click `FileListCinema.exe`. The browser opens on http://localhost:3000. On the first run, a setup screen asks for your FileList and qBittorrent logins and saves them to `.env` next to the exe. Change them later with the ⚙ button.

How the exe behaves:
- **Everything lives next to the exe:** `.env`, `data/` (library, posters, watch progress, subtitles, logs) and `downloads/`.
- **The console window is the app.** Close it to stop. Starting it a second time just opens the browser.
- **qBittorrent starts automatically** if it's installed but not running. At startup the app turns on qBittorrent's *pre-allocate disk space* option, which keeps big movie files from fragmenting on a hard drive.

## Watching

- **Auto quality** (the default) checks what the device can decode and how fast the connection is (measured once every 15 minutes):
  - **Original:** the file is played as-is, with normal seeking.
  - **Original (converted):** used when the device can't decode the audio (TrueHD, DTS, AC3…). Only the audio is converted; the video is copied untouched, including HEVC/4K on devices that support it.
  - **Reduced quality:** 1080p 8 Mbit/s, 720p 4 or 2 Mbit/s, or 480p 1 Mbit/s, for slow Wi-Fi or devices that can't decode the video. It's served as **HLS**, so seeking is normal, and segments are produced just ahead of playback. It uses **Intel Quick Sync** on the GPU when available (HDR is tone-mapped to SDR), with libx264 on the CPU as the fallback.
- **Resume:** the position is saved on the server, so you can stop on the PC and continue on the TV.
- **While anyone is watching,** qBittorrent is throttled (`STREAM_LIMIT_DOWN_KIB` / `STREAM_LIMIT_UP_KIB`, default 1024/256 KiB/s) and background work pauses, because they compete for Wi-Fi airtime and the disk.
- **TV remote:**
  - Arrow keys move between cards and buttons; Back/Esc closes things.
  - In the player, ←/→ seek 10 s, OK pauses, and the media keys work.
  - G/H adjust subtitle sync.
- **Other devices:** set `HOST=0.0.0.0` and open the "on your network" address printed at startup. The app has no password, so only do this on a network you trust.

## Subtitles

The subtitle menu combines four sources, all shown as WebVTT:
- **Your files:** *+ Add subtitles* or drag a `.srt`/`.vtt`/`.ass`/`.ssa`/`.sub` file onto the player. Romanian Windows-1250 files are decoded correctly.
- **In the torrent:** `.srt`/`.vtt` files, downloaded first.
- **In the video file:** text tracks embedded in the MKV. They work right away, with no extraction wait:
  - In **Original** quality, the server picks the subtitle lines out of the video data as it streams it to the player (`src/livesubs.rs`, a small Matroska parser). There are no extra disk reads, and after a seek it picks up again at the new position.
  - In **converted/HLS** playback, the ffmpeg converting the video also writes the subtitle tracks in the same pass.
  - The player shows new lines as playback moves along. The complete track is still extracted once in the background when the download finishes (one movie at a time, paused while anyone watches) and replaces the live track next time.
- **Online (OpenSubtitles):** found by IMDb id in your languages (`OPENSUBTITLES_LANGUAGES`, default `ro,en`) in seconds. Add a free API key in ⚙ Settings to enable it.

Romanian is picked automatically when available. *Sync −/+* fixes timing; the choice and delay are remembered per movie.

## Building

You need Rust (rustup, GNU toolchain) and MinGW-w64 binutils, for `dlltool`:

```powershell
winget install Rustlang.Rustup
rustup toolchain install 1.99.0-x86_64-pc-windows-gnu --profile minimal
rustup default 1.99.0-x86_64-pc-windows-gnu
# MinGW-w64 from WinLibs, unpacked to %LOCALAPPDATA%\flc-tools\mingw64 (provides dlltool)
powershell -ExecutionPolicy Bypass -File build.ps1   # → FileListCinema.exe with icon and version info
```

- Run the tests with `cargo test`.
- Build output goes to `%LOCALAPPDATA%\flc-target` (override with `CARGO_TARGET_DIR`).
- The UI files in `public/` (including the vendored hls.js 1.7.3, MIT licence) are compiled into the exe, so rebuild after changing them.

### Layout
| Path | Contents |
| --- | --- |
| `src/main.rs` | Startup, shared state, single instance, browser launch |
| `src/routes.rs` | HTTP API, media endpoints, viewer tracking (throttle, media log) |
| `src/filelist.rs` | FileList website login + scraping, or the passkey API; movie-only filter |
| `src/qbit.rs` | qBittorrent Web API client, throttling, settings |
| `src/library.rs`, `src/torrent.rs` | Downloads, progress from verified pieces, piece-aware streaming |
| `src/media.rs` | ffprobe (cached), Quick Sync/libx264, converted streams, HLS sessions |
| `src/subtitles.rs`, `src/opensubs.rs` | Subtitle sources, extraction queue, OpenSubtitles |
| `src/posters.rs` | IMDb posters and title info (cached) |
| `public/` | Web UI (HTML/CSS/JS) |

## Notes
- **Diagnostics:** playback problems from any device are logged to `data/media.log`.
- **qBittorrent settings:** connection settings (port, limits, queueing) are configured in qBittorrent itself.
