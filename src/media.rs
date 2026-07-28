use serde::Deserialize;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const PEAKS_PER_SEC: usize = 200;

/// Thumbnails are decoded at twice the height of the strip they are drawn in so
/// they stay sharp on a HiDPI display.
pub const THUMB_H: u32 = 80;

const PEAK_RATE: usize = 16_000;

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

    /// Thumbnail dimensions at `THUMB_H`, or `None` when the probe reported no
    /// usable frame size.
    pub fn thumb_size(&self) -> Option<(u32, u32)> {
        (self.width > 0 && self.height > 0).then(|| {
            let w = (THUMB_H as f64 * self.width as f64 / self.height as f64).round() as u32;
            (w.max(1), THUMB_H)
        })
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

fn fill(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// Min/max sample pairs, `PEAKS_PER_SEC` of them per second, reduced as ffmpeg
/// streams so the raw audio of a long file is never held whole. Amplitudes are
/// scaled to the loudest peak, otherwise a quiet clip draws as a flat line.
pub fn peaks(path: &Path) -> Result<Vec<(f32, f32)>, String> {
    let mut child = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-vn", "-ac", "1", "-ar"])
        .arg(PEAK_RATE.to_string())
        .args(["-f", "f32le", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn ffmpeg: {e}"))?;

    let stderr = child.stderr.take().expect("stderr piped");
    let err_thread = std::thread::spawn(move || {
        let mut collected = String::new();
        let _ = BufReader::new(stderr).read_to_string(&mut collected);
        collected
    });

    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut buf = vec![0u8; PEAK_RATE / PEAKS_PER_SEC * 4];
    let mut out = Vec::new();
    loop {
        let n = fill(&mut stdout, &mut buf).map_err(|e| format!("ffmpeg: {e}"))?;
        if n == 0 {
            break;
        }
        let (mut lo, mut hi) = (0.0f32, 0.0f32);
        for s in buf[..n].chunks_exact(4) {
            let v = f32::from_le_bytes([s[0], s[1], s[2], s[3]]);
            lo = lo.min(v);
            hi = hi.max(v);
        }
        out.push((lo, hi));
        if n < buf.len() {
            break;
        }
    }

    let status = child.wait().map_err(|e| format!("ffmpeg: {e}"))?;
    let err = err_thread.join().unwrap_or_default();
    if !status.success() {
        return Err(err.trim().to_string());
    }

    let loudest = out.iter().fold(0.0f32, |m, &(lo, hi)| m.max(-lo).max(hi));
    if loudest > 0.0 {
        for (lo, hi) in &mut out {
            *lo /= loudest;
            *hi /= loudest;
        }
    }
    Ok(out)
}

/// Decodes one `w`x`h` RGBA frame to stdout. `fast` drops the decode from the
/// preceding keyframe, which returns almost immediately but lands on whatever
/// frame that keyframe holds instead of the one at `time`.
pub fn thumb_cmd(path: &Path, time: f64, w: u32, h: u32, fast: bool) -> Command {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error"]);
    if fast {
        cmd.arg("-noaccurate_seek");
    }
    cmd.arg("-ss")
        .arg(format!("{time:.3}"))
        .arg("-i")
        .arg(path)
        .args(["-frames:v", "1", "-vf"])
        .arg(format!("scale={w}:{h}"))
        .args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    cmd
}

/// The single frame written by `thumb_cmd`; a short read means ffmpeg reached
/// the end of the file without decoding one.
pub fn read_frame(mut r: impl Read, w: u32, h: u32) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; w as usize * h as usize * 4];
    let n = fill(&mut r, &mut buf).map_err(|e| format!("ffmpeg: {e}"))?;
    if n < buf.len() {
        return Err("no frame decoded".into());
    }
    Ok(buf)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PathBuf {
        let path = std::env::temp_dir().join("votrim_peaks.wav");
        if !path.exists() {
            let status = Command::new("ffmpeg")
                .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
                .arg("sine=frequency=440:duration=3")
                .args(["-af", "volume=t/3:eval=frame", "-y"])
                .arg(&path)
                .status()
                .expect("ffmpeg");
            assert!(status.success());
        }
        path
    }

    fn video_fixture() -> PathBuf {
        let path = std::env::temp_dir().join("votrim_thumbs.mp4");
        if !path.exists() {
            let status = Command::new("ffmpeg")
                .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
                .arg("testsrc2=size=320x180:rate=10:duration=3")
                .args(["-c:v", "mpeg4", "-pix_fmt", "yuv420p", "-y"])
                .arg(&path)
                .status()
                .expect("ffmpeg");
            assert!(status.success());
        }
        path
    }

    fn thumb(path: &Path, time: f64, w: u32, h: u32) -> Result<Vec<u8>, String> {
        let mut child = thumb_cmd(path, time, w, h, false).spawn().expect("ffmpeg");
        let stdout = child.stdout.take().expect("stdout piped");
        let frame = read_frame(stdout, w, h);
        child.wait().expect("ffmpeg");
        frame
    }

    fn loudest(peaks: &[(f32, f32)]) -> f32 {
        peaks.iter().fold(0.0f32, |m, &(lo, hi)| m.max(-lo).max(hi))
    }

    #[test]
    fn peaks_track_the_audio_envelope() {
        let p = peaks(&fixture()).expect("peaks");

        // One bucket per 1/PEAKS_PER_SEC across three seconds.
        assert!(
            p.len().abs_diff(3 * PEAKS_PER_SEC) < PEAKS_PER_SEC / 10,
            "{} buckets",
            p.len()
        );

        // Normalised against the loudest peak, so the ramp ends at full scale.
        assert!((loudest(&p) - 1.0).abs() < 1e-6, "{}", loudest(&p));

        // volume=t/3 ramps up, so the opening is far quieter than the tail.
        let head = loudest(&p[..PEAKS_PER_SEC / 2]);
        let tail = loudest(&p[p.len() - PEAKS_PER_SEC / 2..]);
        assert!(head < tail / 4.0, "head {head}, tail {tail}");

        // A sine swings both ways: every loud bucket has a negative half.
        let lo = p.iter().fold(0.0f32, |m, &(lo, _)| m.min(lo));
        assert!(lo < -0.99, "{lo}");
    }

    #[test]
    fn thumbs_decode_the_frame_at_each_timestamp() {
        let path = video_fixture();
        let info = probe(&path).expect("probe");
        let (w, h) = info.thumb_size().expect("thumb size");

        // 320x180 scaled to THUMB_H keeps the source aspect.
        assert_eq!((w, h), (142, THUMB_H));

        let head = thumb(&path, 0.2, w, h).expect("head");
        let tail = thumb(&path, 2.5, w, h).expect("tail");
        assert_eq!(head.len(), w as usize * h as usize * 4);
        assert_ne!(head, tail);

        // Past the end nothing is decoded, rather than the last frame again.
        assert!(thumb(&path, 60.0, w, h).is_err());
    }
}
