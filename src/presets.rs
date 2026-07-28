use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const X_SPEEDS: [&str; 9] = [
    "ultrafast",
    "superfast",
    "veryfast",
    "faster",
    "fast",
    "medium",
    "slow",
    "slower",
    "veryslow",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoCodec {
    Av1Svt,
    X265,
    X264,
    Vp9,
}

impl VideoCodec {
    pub fn label(self) -> &'static str {
        match self {
            Self::Av1Svt => "AV1 (SVT-AV1)",
            Self::X265 => "H.265 (x265)",
            Self::X264 => "H.264 (x264)",
            Self::Vp9 => "VP9 (libvpx)",
        }
    }

    pub fn crf_range(self) -> (i32, i32) {
        match self {
            Self::Av1Svt | Self::Vp9 => (0, 63),
            Self::X265 | Self::X264 => (0, 51),
        }
    }

    /// Speed knob is an integer for SVT-AV1/VP9 and a named preset for x264/x265.
    pub fn numeric_speed(self) -> Option<(i32, i32)> {
        match self {
            Self::Av1Svt => Some((0, 13)),
            Self::Vp9 => Some((0, 8)),
            Self::X265 | Self::X264 => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioCodec {
    Opus,
    Aac,
    Copy,
    None,
}

impl AudioCodec {
    pub fn label(self) -> &'static str {
        match self {
            Self::Opus => "Opus",
            Self::Aac => "AAC",
            Self::Copy => "Copy",
            Self::None => "None",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RateMode {
    Crf,
    Bitrate,
    TargetSize,
}

impl RateMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Crf => "Quality (CRF)",
            Self::Bitrate => "Bitrate",
            Self::TargetSize => "Target file size",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    pub container: String,
    pub video: VideoCodec,
    pub speed: i32,
    pub x_speed: String,
    pub rate: RateMode,
    pub crf: i32,
    /// Capped-CRF ceiling in kbit/s; 0 disables it.
    pub max_kbps: u32,
    pub target_mib: f64,
    pub bitrate_kbps: u32,
    pub two_pass: bool,
    pub audio: AudioCodec,
    pub audio_kbps: u32,
    /// SVT-AV1 `-svtav1-params` payload, ignored by the other codecs.
    pub svt_params: String,
    pub extra_args: String,
    /// Output height; 0 keeps the source resolution.
    pub scale_height: u32,
    /// Output frame rate cap; 0 keeps the source rate.
    pub fps_cap: f64,
}

impl Default for Preset {
    fn default() -> Self {
        Self {
            name: "AV1 space-saver".into(),
            container: "mp4".into(),
            video: VideoCodec::Av1Svt,
            speed: 7,
            x_speed: "medium".into(),
            rate: RateMode::Crf,
            crf: 48,
            max_kbps: 10_000,
            target_mib: 25.0,
            bitrate_kbps: 2000,
            two_pass: true,
            audio: AudioCodec::Opus,
            audio_kbps: 96,
            svt_params: "lookahead=120:tune=0".into(),
            extra_args: String::new(),
            scale_height: 0,
            fps_cap: 0.0,
        }
    }
}

pub fn builtins() -> Vec<Preset> {
    vec![
        Preset::default(),
        Preset {
            name: "AV1 target size".into(),
            rate: RateMode::TargetSize,
            max_kbps: 0,
            ..Preset::default()
        },
        Preset {
            name: "AV1 archival".into(),
            crf: 32,
            speed: 4,
            max_kbps: 0,
            audio_kbps: 128,
            ..Preset::default()
        },
        Preset {
            name: "H.265 balanced".into(),
            container: "mp4".into(),
            video: VideoCodec::X265,
            crf: 26,
            x_speed: "medium".into(),
            max_kbps: 0,
            audio: AudioCodec::Aac,
            audio_kbps: 128,
            ..Preset::default()
        },
        Preset {
            name: "H.264 compatible".into(),
            container: "mp4".into(),
            video: VideoCodec::X264,
            crf: 21,
            x_speed: "medium".into(),
            max_kbps: 0,
            audio: AudioCodec::Aac,
            audio_kbps: 160,
            ..Preset::default()
        },
        Preset {
            name: "Discord 10 MiB".into(),
            rate: RateMode::TargetSize,
            target_mib: 10.0,
            max_kbps: 0,
            audio_kbps: 64,
            scale_height: 720,
            ..Preset::default()
        },
    ]
}

fn store_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("votrim").join("presets.json"))
}

pub fn load() -> Vec<Preset> {
    let user: Vec<Preset> = store_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let mut all = builtins();
    all.extend(user);
    all
}

/// Persists everything that is not a built-in, matched by name.
pub fn save(all: &[Preset]) -> Result<(), String> {
    let builtin_names: Vec<String> = builtins().into_iter().map(|p| p.name).collect();
    let user: Vec<&Preset> = all
        .iter()
        .filter(|p| !builtin_names.contains(&p.name))
        .collect();
    let path = store_path().ok_or("no config directory")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_vec_pretty(&user).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())
}
