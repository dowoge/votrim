use crate::presets::{AudioCodec, Preset, RateMode, VideoCodec};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrimMode {
    Reencode,
    Copy,
}

#[derive(Debug, Clone)]
pub struct JobSpec {
    pub input: PathBuf,
    pub segments: Vec<(f64, f64)>,
    pub mode: TrimMode,
    pub preset: Preset,
    pub output: PathBuf,
    pub separate: bool,
    pub has_audio: bool,
}

#[derive(Debug, Clone)]
pub struct Step {
    pub args: Vec<String>,
    pub dur: f64,
    pub label: String,
}

#[derive(Debug)]
pub struct Plan {
    pub steps: Vec<Step>,
    /// Written before the run and deleted after it, in this order.
    pub concat_list: Option<(PathBuf, String)>,
}

pub enum Msg {
    Progress {
        frac: f64,
        label: String,
        speed: String,
    },
    Log(String),
    Done(Result<(), String>),
}

fn s(v: impl ToString) -> String {
    v.to_string()
}

fn out_with_suffix(out: &Path, idx: usize) -> PathBuf {
    let stem = out
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = out
        .extension()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "mp4".into());
    out.with_file_name(format!("{stem}_{:03}.{ext}", idx + 1))
}

fn temp_dir() -> PathBuf {
    std::env::temp_dir().join("votrim")
}

/// Video bitrate in kbit/s once the audio track's share is subtracted.
fn target_video_kbps(p: &Preset, dur: f64, has_audio: bool) -> u32 {
    if dur <= 0.0 {
        return 1000;
    }
    let total_kbps = p.target_mib * 1024.0 * 1024.0 * 8.0 / dur / 1000.0;
    let audio_kbps = match p.audio {
        AudioCodec::Opus | AudioCodec::Aac if has_audio => p.audio_kbps as f64,
        _ => 0.0,
    };
    // Leave a little headroom for container overhead.
    ((total_kbps * 0.97 - audio_kbps).max(50.0)) as u32
}

fn filter_chain(p: &Preset) -> Option<String> {
    let mut parts = Vec::new();
    if p.scale_height > 0 {
        parts.push(format!("scale=-2:{}", p.scale_height));
    }
    if p.fps_cap > 0.0 {
        parts.push(format!("fps={}", p.fps_cap));
    }
    (!parts.is_empty()).then(|| parts.join(","))
}

fn video_args(p: &Preset, dur: f64, has_audio: bool, pass: Option<u8>, log: &Path) -> Vec<String> {
    let mut a = Vec::new();
    let bitrate_kbps = match p.rate {
        RateMode::TargetSize => Some(target_video_kbps(p, dur, has_audio)),
        RateMode::Bitrate => Some(p.bitrate_kbps),
        RateMode::Crf => None,
    };

    match p.video {
        VideoCodec::Av1Svt => {
            a.extend([
                "-c:v".into(),
                "libsvtav1".into(),
                "-preset".into(),
                s(p.speed),
            ]);
            let mut svt = p.svt_params.clone();
            match bitrate_kbps {
                Some(kbps) => a.extend(["-b:v".into(), format!("{kbps}k")]),
                None => {
                    a.extend(["-crf".into(), s(p.crf)]);
                    if p.max_kbps > 0 {
                        if !svt.is_empty() {
                            svt.push(':');
                        }
                        svt.push_str(&format!("mbr={}k", p.max_kbps));
                    }
                }
            }
            if !svt.is_empty() {
                a.extend(["-svtav1-params".into(), svt]);
            }
            a.extend(["-pix_fmt".into(), "yuv420p".into()]);
        }
        VideoCodec::X265 => {
            a.extend([
                "-c:v".into(),
                "libx265".into(),
                "-preset".into(),
                p.x_speed.clone(),
            ]);
            match bitrate_kbps {
                Some(kbps) => a.extend(["-b:v".into(), format!("{kbps}k")]),
                None => a.extend(["-crf".into(), s(p.crf)]),
            }
            a.extend(["-x265-params".into(), "log-level=error".into()]);
            a.extend(["-tag:v".into(), "hvc1".into()]);
        }
        VideoCodec::X264 => {
            a.extend([
                "-c:v".into(),
                "libx264".into(),
                "-preset".into(),
                p.x_speed.clone(),
            ]);
            match bitrate_kbps {
                Some(kbps) => a.extend(["-b:v".into(), format!("{kbps}k")]),
                None => a.extend(["-crf".into(), s(p.crf)]),
            }
            a.extend(["-pix_fmt".into(), "yuv420p".into()]);
        }
        VideoCodec::Vp9 => {
            a.extend([
                "-c:v".into(),
                "libvpx-vp9".into(),
                "-cpu-used".into(),
                s(p.speed),
                "-row-mt".into(),
                "1".into(),
            ]);
            match bitrate_kbps {
                Some(kbps) => a.extend(["-b:v".into(), format!("{kbps}k")]),
                None => a.extend(["-crf".into(), s(p.crf), "-b:v".into(), "0".into()]),
            }
        }
    }

    if let Some(pass) = pass {
        a.extend([
            "-pass".into(),
            s(pass),
            "-passlogfile".into(),
            log.to_string_lossy().to_string(),
        ]);
    }
    a.extend(p.extra_args.split_whitespace().map(s));
    a
}

fn audio_args(p: &Preset, has_audio: bool, force_encode: bool) -> Vec<String> {
    if !has_audio || p.audio == AudioCodec::None {
        return vec!["-an".into()];
    }
    match p.audio {
        AudioCodec::Opus => vec![
            "-c:a".into(),
            "libopus".into(),
            "-b:a".into(),
            format!("{}k", p.audio_kbps),
        ],
        AudioCodec::Aac => vec![
            "-c:a".into(),
            "aac".into(),
            "-b:a".into(),
            format!("{}k", p.audio_kbps),
        ],
        AudioCodec::Copy if force_encode => {
            vec![
                "-c:a".into(),
                "libopus".into(),
                "-b:a".into(),
                format!("{}k", p.audio_kbps),
            ]
        }
        AudioCodec::Copy => vec!["-c:a".into(), "copy".into()],
        AudioCodec::None => vec!["-an".into()],
    }
}

const BASE: [&str; 8] = [
    "-hide_banner",
    "-nostdin",
    "-y",
    "-loglevel",
    "error",
    "-nostats",
    "-progress",
    "pipe:1",
];

fn base_args() -> Vec<String> {
    BASE.iter().map(|a| a.to_string()).collect()
}

/// One input segment, decoded with an input seek, written to its own file.
fn single_segment_steps(
    spec: &JobSpec,
    start: f64,
    end: f64,
    output: &Path,
    label: &str,
    log: &Path,
) -> Vec<Step> {
    let dur = (end - start).max(0.0);
    let p = &spec.preset;

    if spec.mode == TrimMode::Copy {
        let mut args = base_args();
        args.extend([
            "-ss".into(),
            s(start),
            "-i".into(),
            spec.input.to_string_lossy().to_string(),
            "-t".into(),
            s(dur),
            "-map".into(),
            "0:v:0".into(),
            "-map".into(),
            "0:a:0?".into(),
            "-c".into(),
            "copy".into(),
            "-avoid_negative_ts".into(),
            "make_zero".into(),
            output.to_string_lossy().to_string(),
        ]);
        return vec![Step {
            args,
            dur,
            label: label.into(),
        }];
    }

    let input_args = |a: &mut Vec<String>| {
        a.extend([
            "-ss".into(),
            s(start),
            "-i".into(),
            spec.input.to_string_lossy().to_string(),
            "-t".into(),
            s(dur),
            "-map".into(),
            "0:v:0".into(),
        ]);
        if spec.has_audio && p.audio != AudioCodec::None {
            a.extend(["-map".into(), "0:a:0?".into()]);
        }
        if let Some(vf) = filter_chain(p) {
            a.extend(["-vf".into(), vf]);
        }
    };

    let two_pass = p.two_pass && p.rate != RateMode::Crf;
    let mut steps = Vec::new();

    if two_pass {
        let mut args = base_args();
        input_args(&mut args);
        args.extend(video_args(p, dur, spec.has_audio, Some(1), log));
        args.extend(["-an".into(), "-f".into(), "null".into(), "-".into()]);
        steps.push(Step {
            args,
            dur,
            label: format!("{label} (pass 1)"),
        });
    }

    let mut args = base_args();
    input_args(&mut args);
    args.extend(video_args(
        p,
        dur,
        spec.has_audio,
        two_pass.then_some(2),
        log,
    ));
    args.extend(audio_args(p, spec.has_audio, false));
    args.push(output.to_string_lossy().to_string());
    steps.push(Step {
        args,
        dur,
        label: if two_pass {
            format!("{label} (pass 2)")
        } else {
            label.into()
        },
    });
    steps
}

/// Several segments joined into one re-encoded file via trim/concat filters.
fn concat_filter_steps(spec: &JobSpec, log: &Path) -> Vec<Step> {
    let p = &spec.preset;
    let total: f64 = spec.segments.iter().map(|(a, b)| b - a).sum();
    let use_audio = spec.has_audio && p.audio != AudioCodec::None;

    let mut graph = String::new();
    let mut links = String::new();
    for (i, (a, b)) in spec.segments.iter().enumerate() {
        graph.push_str(&format!(
            "[0:v]trim=start={a}:end={b},setpts=PTS-STARTPTS[v{i}];"
        ));
        links.push_str(&format!("[v{i}]"));
        if use_audio {
            graph.push_str(&format!(
                "[0:a]atrim=start={a}:end={b},asetpts=PTS-STARTPTS[a{i}];"
            ));
            links.push_str(&format!("[a{i}]"));
        }
    }
    let n = spec.segments.len();
    match (use_audio, filter_chain(p)) {
        (true, Some(vf)) => {
            graph.push_str(&format!(
                "{links}concat=n={n}:v=1:a=1[cv][outa];[cv]{vf}[outv]"
            ));
        }
        (true, None) => graph.push_str(&format!("{links}concat=n={n}:v=1:a=1[outv][outa]")),
        (false, Some(vf)) => {
            graph.push_str(&format!("{links}concat=n={n}:v=1:a=0[cv];[cv]{vf}[outv]"));
        }
        (false, None) => graph.push_str(&format!("{links}concat=n={n}:v=1:a=0[outv]")),
    }

    let common = |a: &mut Vec<String>| {
        a.extend([
            "-i".into(),
            spec.input.to_string_lossy().to_string(),
            "-filter_complex".into(),
            graph.clone(),
            "-map".into(),
            "[outv]".into(),
        ]);
        if use_audio {
            a.extend(["-map".into(), "[outa]".into()]);
        }
    };

    let two_pass = p.two_pass && p.rate != RateMode::Crf;
    let mut steps = Vec::new();
    if two_pass {
        let mut args = base_args();
        common(&mut args);
        args.extend(video_args(p, total, spec.has_audio, Some(1), log));
        args.extend(["-an".into(), "-f".into(), "null".into(), "-".into()]);
        steps.push(Step {
            args,
            dur: total,
            label: "Encoding (pass 1)".into(),
        });
    }
    let mut args = base_args();
    common(&mut args);
    args.extend(video_args(
        p,
        total,
        spec.has_audio,
        two_pass.then_some(2),
        log,
    ));
    args.extend(audio_args(p, spec.has_audio, true));
    args.push(spec.output.to_string_lossy().to_string());
    steps.push(Step {
        args,
        dur: total,
        label: if two_pass {
            "Encoding (pass 2)".into()
        } else {
            "Encoding".into()
        },
    });
    steps
}

pub fn plan(spec: &JobSpec) -> Result<Plan, String> {
    if spec.segments.is_empty() {
        return Err("no segments to export".into());
    }
    let tmp = temp_dir();
    let log = tmp.join("ffmpeg2pass");

    if spec.separate {
        let mut steps = Vec::new();
        for (i, &(a, b)) in spec.segments.iter().enumerate() {
            let out = out_with_suffix(&spec.output, i);
            let per_log = tmp.join(format!("ffmpeg2pass_{i}"));
            steps.extend(single_segment_steps(
                spec,
                a,
                b,
                &out,
                &format!("Segment {}", i + 1),
                &per_log,
            ));
        }
        return Ok(Plan {
            steps,
            concat_list: None,
        });
    }

    if spec.segments.len() == 1 {
        let (a, b) = spec.segments[0];
        let steps = single_segment_steps(spec, a, b, &spec.output, "Encoding", &log);
        return Ok(Plan {
            steps,
            concat_list: None,
        });
    }

    if spec.mode == TrimMode::Reencode {
        return Ok(Plan {
            steps: concat_filter_steps(spec, &log),
            concat_list: None,
        });
    }

    // Stream copy of several segments: cut each to a temp file, then concat-demux them.
    let ext = spec
        .output
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_else(|| "mkv".into());
    let mut steps = Vec::new();
    let mut list = String::new();
    for (i, &(a, b)) in spec.segments.iter().enumerate() {
        let part = tmp.join(format!("part_{i:03}.{ext}"));
        steps.extend(single_segment_steps(
            spec,
            a,
            b,
            &part,
            &format!("Cutting {}", i + 1),
            &log,
        ));
        list.push_str(&format!(
            "file '{}'\n",
            part.to_string_lossy().replace('\'', r"'\''")
        ));
    }
    let list_path = tmp.join("concat.txt");
    let mut args = base_args();
    args.extend([
        "-f".into(),
        "concat".into(),
        "-safe".into(),
        "0".into(),
        "-i".into(),
        list_path.to_string_lossy().to_string(),
        "-c".into(),
        "copy".into(),
        spec.output.to_string_lossy().to_string(),
    ]);
    steps.push(Step {
        args,
        dur: spec.segments.iter().map(|(a, b)| b - a).sum(),
        label: "Joining".into(),
    });
    Ok(Plan {
        steps,
        concat_list: Some((list_path, list)),
    })
}

pub struct Job {
    pub rx: Receiver<Msg>,
    pub cancel: Arc<AtomicBool>,
    pid: Arc<AtomicU32>,
}

impl Job {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        let pid = self.pid.load(Ordering::SeqCst);
        if pid != 0 {
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        }
    }
}

fn run_step(
    step: &Step,
    tx: &Sender<Msg>,
    pid: &AtomicU32,
    cancel: &AtomicBool,
    done_before: f64,
    total: f64,
) -> Result<(), String> {
    let mut child = Command::new("ffmpeg")
        .args(&step.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn ffmpeg: {e}"))?;
    pid.store(child.id(), Ordering::SeqCst);

    let stderr = child.stderr.take().expect("stderr piped");
    let err_tx = tx.clone();
    let err_thread = std::thread::spawn(move || {
        let mut collected = String::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if collected.len() < 8192 {
                collected.push_str(&line);
                collected.push('\n');
            }
            let _ = err_tx.send(Msg::Log(line));
        }
        collected
    });

    let stdout = child.stdout.take().expect("stdout piped");
    let mut speed = String::new();
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        match key {
            "speed" => speed = val.trim().to_string(),
            "out_time_us" => {
                let secs = val.trim().parse::<f64>().unwrap_or(0.0) / 1e6;
                let frac = if total > 0.0 {
                    (done_before + secs.max(0.0)) / total
                } else {
                    0.0
                };
                let _ = tx.send(Msg::Progress {
                    frac: frac.clamp(0.0, 1.0),
                    label: step.label.clone(),
                    speed: speed.clone(),
                });
            }
            _ => {}
        }
    }

    let status = child.wait().map_err(|e| e.to_string())?;
    pid.store(0, Ordering::SeqCst);
    let stderr_text = err_thread.join().unwrap_or_default();

    if cancel.load(Ordering::SeqCst) {
        return Err("cancelled".into());
    }
    if !status.success() {
        let detail = stderr_text
            .lines()
            .last()
            .unwrap_or("unknown error")
            .to_string();
        return Err(format!("{}: {detail}", step.label));
    }
    Ok(())
}

pub fn spawn(plan: Plan) -> Job {
    let (tx, rx) = channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let pid = Arc::new(AtomicU32::new(0));
    let job = Job {
        rx,
        cancel: cancel.clone(),
        pid: pid.clone(),
    };

    std::thread::spawn(move || {
        let result = (|| -> Result<(), String> {
            std::fs::create_dir_all(temp_dir()).map_err(|e| e.to_string())?;
            if let Some((path, body)) = &plan.concat_list {
                std::fs::write(path, body).map_err(|e| e.to_string())?;
            }
            let total: f64 = plan.steps.iter().map(|s| s.dur).sum();
            let mut done = 0.0;
            for step in &plan.steps {
                if cancel.load(Ordering::SeqCst) {
                    return Err("cancelled".into());
                }
                run_step(step, &tx, &pid, &cancel, done, total)?;
                done += step.dur;
            }
            Ok(())
        })();

        let _ = std::fs::remove_dir_all(temp_dir());
        let _ = tx.send(Msg::Done(result));
    });

    job
}

pub fn command_preview(plan: &Plan) -> String {
    plan.steps
        .iter()
        .map(|st| {
            let args = st
                .args
                .iter()
                .map(|a| {
                    if a.contains(' ') || a.contains('[') {
                        format!("'{a}'")
                    } else {
                        a.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            format!("ffmpeg {args}")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presets::{AudioCodec, Preset, RateMode};

    fn fixture() -> PathBuf {
        let path = std::env::temp_dir().join("votrim_fixture.mp4");
        if !path.exists() {
            let status = Command::new("ffmpeg")
                .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
                .arg("testsrc2=size=320x240:rate=30:duration=30")
                .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=30"])
                .args(["-c:v", "libx264", "-g", "60", "-pix_fmt", "yuv420p"])
                .args(["-c:a", "aac", "-shortest", "-y"])
                .arg(&path)
                .status()
                .expect("ffmpeg");
            assert!(status.success());
        }
        path
    }

    fn duration_of(path: &Path) -> f64 {
        let out = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "csv=p=0",
            ])
            .arg(path)
            .output()
            .expect("ffprobe");
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("duration")
    }

    fn run(spec: &JobSpec) {
        let plan = plan(spec).expect("plan");
        let job = spawn(plan);
        loop {
            match job.rx.recv().expect("channel") {
                Msg::Done(result) => {
                    result.expect("ffmpeg run");
                    return;
                }
                Msg::Log(_) | Msg::Progress { .. } => {}
            }
        }
    }

    fn spec(segments: Vec<(f64, f64)>, mode: TrimMode, preset: Preset, name: &str) -> JobSpec {
        JobSpec {
            input: fixture(),
            segments,
            mode,
            preset,
            output: std::env::temp_dir().join(name),
            separate: false,
            has_audio: true,
        }
    }

    fn fast_av1() -> Preset {
        Preset {
            speed: 12,
            crf: 50,
            ..Preset::default()
        }
    }

    #[test]
    fn encodes_are_frame_accurate_and_hit_their_size_budget() {
        // Single segment, re-encoded.
        let one = spec(
            vec![(3.0, 8.0)],
            TrimMode::Reencode,
            fast_av1(),
            "votrim_one.mp4",
        );
        run(&one);
        assert!(
            (duration_of(&one.output) - 5.0).abs() < 0.15,
            "{}",
            duration_of(&one.output)
        );

        // Several segments joined by the trim/concat filter graph.
        let many = spec(
            vec![(1.0, 3.0), (10.0, 14.0), (20.0, 23.0)],
            TrimMode::Reencode,
            fast_av1(),
            "votrim_many.mp4",
        );
        run(&many);
        assert!(
            (duration_of(&many.output) - 9.0).abs() < 0.2,
            "{}",
            duration_of(&many.output)
        );

        // Several segments stream-copied through temp files and the concat demuxer.
        let copied = spec(
            vec![(2.0, 6.0), (12.0, 16.0)],
            TrimMode::Copy,
            Preset {
                container: "mkv".into(),
                ..Preset::default()
            },
            "votrim_copy.mkv",
        );
        run(&copied);
        assert!(
            (duration_of(&copied.output) - 8.0).abs() < 0.5,
            "{}",
            duration_of(&copied.output)
        );

        // Two-pass target size.
        let sized = spec(
            vec![(0.0, 20.0)],
            TrimMode::Reencode,
            Preset {
                rate: RateMode::TargetSize,
                target_mib: 1.0,
                two_pass: true,
                audio: AudioCodec::Opus,
                audio_kbps: 64,
                ..fast_av1()
            },
            "votrim_sized.mp4",
        );
        run(&sized);
        let bytes = std::fs::metadata(&sized.output).expect("output").len();
        let target = 1024.0 * 1024.0;
        assert!(
            (bytes as f64) < target * 1.15 && (bytes as f64) > target * 0.5,
            "{bytes} bytes vs {target} target"
        );
    }
}
