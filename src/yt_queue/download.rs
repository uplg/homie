//! yt-dlp invocations: metadata probe + audio download.
//!
//! We use the `yt-dlp` crate's typed `Downloader` for metadata probes
//! (it gives us a strongly typed `Video` struct), and shell out to the
//! `yt-dlp` binary directly for downloads with `-x --audio-format mp3`.
//! The crate's typed download API does not expose the post-process flag
//! cleanly, and forcing mp3 on the way out means rodio's `symphonia-mp3`
//! decoder always plays the result — no codec guessing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::OnceCell;

use yt_dlp::{Downloader, client::deps::Libraries};

use crate::error::{Error, Result};

/// Lightweight metadata pulled from yt-dlp.
#[derive(Debug, Clone)]
pub struct VideoMeta {
    pub title: String,
    pub duration_secs: u32,
    pub is_live: bool,
}

/// Build (once) a `Downloader` pointing at the system-installed yt-dlp and
/// ffmpeg binaries (`brew install yt-dlp ffmpeg` is enough on macOS). The
/// `Downloader` is only used for metadata probes — the download itself
/// happens via a direct subprocess call.
async fn probe_downloader() -> Result<Arc<Downloader>> {
    static CELL: OnceCell<Arc<Downloader>> = OnceCell::const_new();
    if let Some(d) = CELL.get() {
        return Ok(d.clone());
    }

    let yt = which("yt-dlp").await?;
    let ff = which("ffmpeg").await?;
    let libs = Libraries::new(yt, ff);
    let scratch = std::env::temp_dir().join("twitchy-yt-probe");
    let dl = Downloader::builder(libs, scratch)
        .build()
        .await
        .map_err(|err| Error::config(format!("yt-dlp builder failed: {err}")))?;
    let arc = Arc::new(dl);
    let _ = CELL.set(arc.clone());
    Ok(arc)
}

async fn which(name: &str) -> Result<PathBuf> {
    let output = tokio::process::Command::new("which")
        .arg(name)
        .output()
        .await
        .map_err(|err| Error::config(format!("`which {name}` failed: {err}")))?;
    if !output.status.success() {
        return Err(Error::config(format!(
            "{name} not found on PATH (try `brew install {name}`)"
        )));
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if raw.is_empty() {
        return Err(Error::config(format!("{name} binary path is empty")));
    }
    Ok(PathBuf::from(raw))
}

/// Verify yt-dlp + ffmpeg are installed.
pub async fn check_yt_dlp_available() -> Result<String> {
    let yt = which("yt-dlp").await?;
    let _ff = which("ffmpeg").await?;
    let output = tokio::process::Command::new(&yt)
        .arg("--version")
        .output()
        .await
        .map_err(|err| Error::config(format!("yt-dlp --version: {err}")))?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Probe a URL for title, duration, live status.
pub async fn probe(url: &str) -> Result<VideoMeta> {
    let dl = probe_downloader().await?;
    let video = dl
        .fetch_video_infos(url.to_string())
        .await
        .map_err(|err| Error::config(format!("yt-dlp probe: {err}")))?;

    Ok(VideoMeta {
        title: video.title.clone(),
        duration_secs: u32::try_from(video.duration.unwrap_or(0).max(0)).unwrap_or(u32::MAX),
        is_live: video.is_live.unwrap_or(false),
    })
}

/// Download the audio of `url` into `dir`, transcoded to mp3.
///
/// Shells out to `yt-dlp -x --audio-format mp3 -o dir/track.%(ext)s URL`.
/// `%(ext)s` is substituted to `mp3` once the audio extraction is done,
/// so the final file is always `dir/track.mp3`.
pub async fn download_audio(url: &str, dir: &Path) -> Result<PathBuf> {
    tokio::fs::create_dir_all(dir).await?;
    let yt = which("yt-dlp").await?;
    let template = dir.join("track.%(ext)s");
    let target = dir.join("track.mp3");

    let output = tokio::process::Command::new(&yt)
        .args([
            "--no-playlist",
            "--no-progress",
            "--no-warnings",
            "-x",
            "--audio-format",
            "mp3",
            "-o",
        ])
        .arg(&template)
        .arg(url)
        .output()
        .await
        .map_err(|err| Error::config(format!("yt-dlp download spawn failed: {err}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::config(format!(
            "yt-dlp download failed (exit {}): {}",
            output.status,
            stderr.trim()
        )));
    }

    if !tokio::fs::try_exists(&target).await.unwrap_or(false) {
        return Err(Error::config(format!(
            "yt-dlp succeeded but {} is missing",
            target.display()
        )));
    }

    Ok(target)
}
