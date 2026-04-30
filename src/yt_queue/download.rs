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

/// File extensions we treat as direct audio URLs (no yt-dlp probe needed).
/// Anything that ends with one of these (after stripping the query string)
/// is fetched verbatim with reqwest and decoded by symphonia.
const DIRECT_AUDIO_EXTENSIONS: &[&str] = &["mp3", "ogg", "wav", "m4a", "flac", "opus", "aac"];

/// True if `url` looks like a direct link to an audio file.
///
/// We only inspect the URL's path extension; if reqwest later fails we'll
/// still surface a clean error, but the goal is to keep yt-dlp's noisy
/// extractor errors out of chat for the obvious case.
#[must_use]
pub fn is_direct_audio_url(url: &str) -> Option<&'static str> {
    let parsed = url::Url::parse(url).ok()?;
    let path = parsed.path();
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    DIRECT_AUDIO_EXTENSIONS
        .iter()
        .copied()
        .find(|candidate| *candidate == ext)
}

/// Build a human-friendly title from a direct audio URL.
///
/// Falls back to the host or `"audio"` when the path has no usable last
/// segment.
#[must_use]
pub fn direct_audio_title(url: &str) -> String {
    if let Ok(parsed) = url::Url::parse(url) {
        if let Some(last) = parsed.path_segments().and_then(Iterator::last) {
            let decoded = percent_decode(last);
            if !decoded.is_empty() {
                return decoded;
            }
        }
        if let Some(host) = parsed.host_str() {
            return host.to_string();
        }
    }
    "audio".to_string()
}

/// Minimal percent-decoder good enough for filenames in URL paths.
fn percent_decode(input: &str) -> String {
    let mut out = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                // Both h and l fit in 4 bits, so h*16+l fits in 8 bits.
                let byte = u8::try_from(h * 16 + l).expect("hex digit pair always fits in u8");
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

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

/// Download a direct audio URL (e.g. `https://example.com/song.mp3`) into
/// `dir` without going through yt-dlp.
///
/// Keeps the original extension so symphonia can detect the codec via the
/// rodio decoder hint.
pub async fn download_direct(url: &str, dir: &Path) -> Result<PathBuf> {
    tokio::fs::create_dir_all(dir).await?;
    let ext = is_direct_audio_url(url).unwrap_or("mp3");
    let target = dir.join(format!("track.{ext}"));

    let response = reqwest::get(url)
        .await
        .map_err(|err| Error::config(format!("direct audio fetch failed: {err}")))?;
    if !response.status().is_success() {
        return Err(Error::config(format!(
            "direct audio fetch returned HTTP {}",
            response.status()
        )));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|err| Error::config(format!("direct audio body read failed: {err}")))?;
    tokio::fs::write(&target, &bytes).await?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_common_audio_extensions() {
        assert_eq!(
            is_direct_audio_url("https://uplg.xyz/music/song.mp3"),
            Some("mp3")
        );
        assert_eq!(
            is_direct_audio_url("https://x.test/p/A.OGG?token=xyz"),
            Some("ogg"),
        );
        assert_eq!(is_direct_audio_url("https://x.test/path"), None);
        assert_eq!(
            is_direct_audio_url("https://www.youtube.com/watch?v=abc"),
            None,
        );
    }

    #[test]
    fn extracts_filename_as_title() {
        assert_eq!(
            direct_audio_title("https://uplg.xyz/music/missing-stream.mp3"),
            "missing-stream.mp3"
        );
        assert_eq!(
            direct_audio_title("https://uplg.xyz/music/Hello%20World.mp3"),
            "Hello World.mp3"
        );
    }
}
