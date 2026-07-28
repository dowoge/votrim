use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct MediaInfo {
    pub path: PathBuf,
    pub duration: f64,
    pub fps: f64,
    pub width: u32,
    pub height: u32,
    pub video_codec: String,
    pub audio_codec: Option<String>,
    pub size_bytes: u64,
}

impl MediaInfo {
    pub fn has_audio(&self) -> bool {
        self.audio_codec.is_some()
    }

    pub fn frame_dur(&self) -> f64 {
        if self.fps > 0.0 {
            1.0 / self.fps
        } else {
            1.0 / 30.0
        }
    }
}

#[derive(Deserialize)]
struct ProbeOut {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    format: ProbeFormat,
}

#[derive(Deserialize)]
struct ProbeStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    avg_frame_rate: Option<String>,
    r_frame_rate: Option<String>,
}

#[derive(Deserialize)]
struct ProbeFormat {
    duration: Option<String>,
    size: Option<String>,
}

fn rational(s: &str) -> Option<f64> {
    let (n, d) = s.split_once('/')?;
    let n: f64 = n.parse().ok()?;
    let d: f64 = d.parse().ok()?;
    (d != 0.0 && n != 0.0).then_some(n / d)
}

pub fn probe(path: &Path) -> Result<MediaInfo, String> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries"])
        .arg("format=duration,size:stream=codec_type,codec_name,width,height,r_frame_rate,avg_frame_rate")
        .args(["-of", "json"])
        .arg(path)
        .output()
        .map_err(|e| format!("ffprobe: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let probe: ProbeOut = serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;

    let video = probe
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("video"))
        .ok_or("no video stream")?;
    let audio = probe
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("audio"));

    let fps = video
        .avg_frame_rate
        .as_deref()
        .and_then(rational)
        .or_else(|| video.r_frame_rate.as_deref().and_then(rational))
        .unwrap_or(30.0);

    Ok(MediaInfo {
        path: path.to_path_buf(),
        duration: probe
            .format
            .duration
            .as_deref()
            .and_then(|d| d.parse().ok())
            .unwrap_or(0.0),
        fps,
        width: video.width.unwrap_or(0),
        height: video.height.unwrap_or(0),
        video_codec: video.codec_name.clone().unwrap_or_default(),
        audio_codec: audio.and_then(|a| a.codec_name.clone()),
        size_bytes: probe
            .format
            .size
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
    })
}

/// Keyframe timestamps, read from packet flags so no decoding happens.
pub fn keyframes(path: &Path) -> Result<Vec<f64>, String> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "packet=pts_time,flags",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .map_err(|e| format!("ffprobe: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let mut times: Vec<f64> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (t, flags) = line.split_once(',')?;
            flags.starts_with('K').then(|| t.parse().ok())?
        })
        .collect();
    times.sort_by(|a, b| a.total_cmp(b));
    Ok(times)
}

pub fn fmt_time(t: f64) -> String {
    let t = t.max(0.0);
    let total_ms = (t * 1000.0).round() as u64;
    let (ms, s) = (total_ms % 1000, total_ms / 1000);
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        s / 3600,
        (s / 60) % 60,
        s % 60,
        ms
    )
}

pub fn fmt_short(t: f64) -> String {
    let s = t.max(0.0);
    if s >= 3600.0 {
        format!(
            "{}:{:02}:{:02}",
            s as u64 / 3600,
            (s as u64 / 60) % 60,
            s as u64 % 60
        )
    } else if s >= 60.0 {
        format!("{}:{:02}", s as u64 / 60, s as u64 % 60)
    } else if s >= 10.0 {
        format!("{s:.1}s")
    } else {
        format!("{s:.2}s")
    }
}

pub fn fmt_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", UNITS[i])
}
