mod encode;
mod media;
mod player;
mod presets;
mod settings;
mod thumbs;
mod timeline;

use eframe::egui::{self, Color32, RichText};
use encode::{Job, JobSpec, Msg, TrimMode};
use media::{MediaInfo, fmt_short, fmt_size, fmt_time};
use player::Player;
use presets::{AudioCodec, Preset, RateMode, VideoCodec, X_SPEEDS};
use settings::Settings;
use std::path::PathBuf;
use std::sync::mpsc::TryRecvError;
use std::sync::{Arc, Mutex};
use thumbs::Thumbs;
use timeline::{Timeline, View};

fn main() -> eframe::Result<()> {
    let arg = std::env::args().nth(1);
    if let Some("-V" | "--version") = arg.as_deref() {
        println!("votrim {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([900.0, 600.0])
            .with_title("votrim"),
        ..Default::default()
    };
    let initial = arg.map(PathBuf::from);
    eframe::run_native(
        "votrim",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, initial)))),
    )
}

struct App {
    player: Option<Player>,
    player_err: Option<String>,
    media: Option<MediaInfo>,
    keyframes: Arc<Mutex<Vec<f64>>>,
    waveform: Arc<Mutex<Vec<(f32, f32)>>>,
    thumbs: Thumbs,

    segments: Vec<(f64, f64)>,
    selected: Option<usize>,
    pending_in: Option<f64>,
    timeline: Timeline,
    position: f64,
    volume: i64,
    speed: f64,
    show_keyframes: bool,
    snap_keyframes: bool,
    settings: Settings,

    mode: TrimMode,
    presets: Vec<Preset>,
    preset_idx: usize,
    preset: Preset,
    save_name: String,
    output: String,
    separate: bool,
    show_command: bool,

    job: Option<Job>,
    progress: f64,
    progress_label: String,
    progress_speed: String,
    log: Vec<String>,
    status: String,
    confirm_overwrite: Option<JobSpec>,

    video_px: [i32; 2],
    video: Option<(egui::TextureId, egui::Rect)>,
    pending_open: Option<PathBuf>,
    seek_target: Option<(f64, std::time::Instant)>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, initial: Option<PathBuf>) -> Self {
        let presets = presets::load();
        let preset = presets[0].clone();
        Self {
            player: None,
            player_err: None,
            media: None,
            keyframes: Arc::new(Mutex::new(Vec::new())),
            waveform: Arc::new(Mutex::new(Vec::new())),
            thumbs: Thumbs::new(cc.egui_ctx.clone()),
            segments: Vec::new(),
            selected: None,
            pending_in: None,
            timeline: Timeline::default(),
            position: 0.0,
            volume: 80,
            speed: 1.0,
            show_keyframes: false,
            snap_keyframes: false,
            settings: settings::load(),
            mode: TrimMode::Reencode,
            presets,
            preset_idx: 0,
            preset,
            save_name: String::new(),
            output: String::new(),
            separate: false,
            show_command: false,
            job: None,
            progress: 0.0,
            progress_label: String::new(),
            progress_speed: String::new(),
            log: Vec::new(),
            status: "Open a video to begin.".into(),
            confirm_overwrite: None,
            video_px: [0, 0],
            video: None,
            pending_open: initial,
            seek_target: None,
        }
    }

    fn duration(&self) -> f64 {
        self.media.as_ref().map(|m| m.duration).unwrap_or(0.0)
    }

    fn frame_dur(&self) -> f64 {
        self.media
            .as_ref()
            .map(|m| m.frame_dur())
            .unwrap_or(1.0 / 30.0)
    }

    fn open(&mut self, path: PathBuf, ctx: &egui::Context) {
        match media::probe(&path) {
            Ok(info) => {
                self.status = format!(
                    "{} — {}x{} {:.3} fps, {}, {}",
                    path.file_name().unwrap_or_default().to_string_lossy(),
                    info.width,
                    info.height,
                    info.fps,
                    info.video_codec,
                    fmt_size(info.size_bytes)
                );
                self.timeline.reset(info.duration);
                self.segments.clear();
                self.selected = None;
                self.pending_in = None;
                self.position = 0.0;
                self.output = default_output(&path, &self.preset.container);
                self.keyframes.lock().unwrap().clear();
                self.waveform.lock().unwrap().clear();
                self.thumbs.open(&info);

                let store = self.keyframes.clone();
                let probe_path = path.clone();
                let probe_ctx = ctx.clone();
                std::thread::spawn(move || {
                    if let Ok(k) = media::keyframes(&probe_path) {
                        *store.lock().unwrap() = k;
                    }
                    probe_ctx.request_repaint();
                });

                if info.has_audio() {
                    let store = self.waveform.clone();
                    let peak_path = path.clone();
                    let ctx = ctx.clone();
                    std::thread::spawn(move || {
                        if let Ok(p) = media::peaks(&peak_path) {
                            *store.lock().unwrap() = p;
                        }
                        ctx.request_repaint();
                    });
                }

                if let Some(p) = &mut self.player {
                    p.load(&path);
                    p.set_volume(self.volume);
                    p.set_speed(self.speed);
                }
                self.media = Some(info);
            }
            Err(e) => self.status = format!("Cannot open: {e}"),
        }
    }

    fn seek(&mut self, time: f64, exact: bool) {
        let time = time.clamp(0.0, self.duration());
        self.position = time;
        self.seek_target = Some((time, std::time::Instant::now()));
        if let Some(p) = &self.player {
            p.seek(time, exact);
        }
    }

    /// mpv is the source of truth for the playhead, but it lags a seek, so the
    /// requested time is held until mpv lands on that frame. The tolerance has to
    /// stay below one frame or repeated single-frame steps collapse back onto
    /// mpv's stale position instead of accumulating.
    fn poll_position(&mut self) {
        let Some(player) = &self.player else { return };
        if !player.is_loaded() || self.timeline.is_dragging() {
            return;
        }
        let actual = player.position();
        if let Some((target, since)) = self.seek_target {
            if (actual - target).abs() > self.frame_dur() * 0.6
                && since.elapsed() < std::time::Duration::from_millis(500)
            {
                return;
            }
            self.seek_target = None;
        }
        self.position = actual;
    }

    /// mpv silently drops a `frame-back-step` that arrives while one is still
    /// running, and repeated steps queue up instead of collapsing. An absolute
    /// seek does neither, so stepping is driven from an optimistic playhead and
    /// only takes the cheap `frame-step` path when moving forward and mpv has
    /// kept up -- seeking forward would otherwise decode from the last keyframe.
    fn step_frame(&mut self, forward: bool) {
        let (fd, dur) = (self.frame_dur(), self.duration());
        let Some(actual) = self.player.as_ref().map(|p| p.position()) else {
            return;
        };
        let target = (self.position + if forward { fd } else { -fd }).clamp(0.0, dur);
        let idle = self.seek_target.is_none() && (self.position - actual).abs() < fd * 0.6;
        self.position = target;
        self.seek_target = Some((target, std::time::Instant::now()));
        if let Some(p) = &self.player {
            if forward && idle {
                p.frame_step(true);
            } else {
                p.seek(target, true);
            }
        }
    }

    fn mark_out(&mut self) {
        let Some(start) = self.pending_in else {
            self.status = "Set an in-point first (I).".into();
            return;
        };
        let (a, b) = (start.min(self.position), start.max(self.position));
        if b - a < self.frame_dur() {
            self.status = "Segment is shorter than one frame.".into();
            return;
        }
        self.segments.push((a, b));
        self.selected = Some(self.segments.len() - 1);
        self.pending_in = None;
    }

    fn total_selected(&self) -> f64 {
        self.segments.iter().map(|(a, b)| b - a).sum()
    }

    fn build_spec(&self) -> Result<JobSpec, String> {
        let media = self.media.as_ref().ok_or("no file open")?;
        if self.segments.is_empty() {
            return Err("no segments marked".into());
        }
        if self.output.trim().is_empty() {
            return Err("no output path".into());
        }
        let mut segments = self.segments.clone();
        segments.sort_by(|a, b| a.0.total_cmp(&b.0));
        Ok(JobSpec {
            input: media.path.clone(),
            segments,
            mode: self.mode,
            preset: self.preset.clone(),
            output: PathBuf::from(self.output.trim()),
            separate: self.separate,
            has_audio: media.has_audio(),
        })
    }

    fn start(&mut self, spec: JobSpec) {
        match encode::plan(&spec) {
            Ok(plan) => {
                self.log.clear();
                self.progress = 0.0;
                self.progress_label = "Starting…".into();
                self.progress_speed.clear();
                self.status = "Encoding…".into();
                self.job = Some(encode::spawn(plan));
            }
            Err(e) => self.status = format!("Cannot start: {e}"),
        }
    }

    fn poll_job(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.job else { return };
        loop {
            match job.rx.try_recv() {
                Ok(Msg::Progress { frac, label, speed }) => {
                    self.progress = frac;
                    self.progress_label = label;
                    self.progress_speed = speed;
                }
                Ok(Msg::Log(line)) => {
                    self.log.push(line);
                    if self.log.len() > 400 {
                        self.log.drain(..100);
                    }
                }
                Ok(Msg::Done(result)) => {
                    self.status = match result {
                        Ok(()) => {
                            self.progress = 1.0;
                            let size = std::fs::metadata(self.output.trim())
                                .map(|m| fmt_size(m.len()))
                                .unwrap_or_else(|_| "done".into());
                            format!("Finished — {size}")
                        }
                        Err(e) => format!("Failed: {e}"),
                    };
                    self.job = None;
                    return;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.job = None;
                    return;
                }
            }
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let (keys, modifiers) = ctx.input(|i| {
            let pressed: Vec<egui::Key> = i
                .events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Key {
                        key, pressed: true, ..
                    } => Some(*key),
                    _ => None,
                })
                .collect();
            (pressed, i.modifiers)
        });
        let dur = self.duration();
        for key in keys {
            match key {
                egui::Key::Space => {
                    if let Some(p) = &self.player {
                        p.set_paused(!p.paused());
                    }
                }
                egui::Key::ArrowLeft | egui::Key::ArrowRight => {
                    let forward = key == egui::Key::ArrowRight;
                    if modifiers.ctrl {
                        self.seek(self.position + if forward { 10.0 } else { -10.0 }, true);
                    } else if modifiers.shift {
                        self.seek(self.position + if forward { 1.0 } else { -1.0 }, true);
                    } else {
                        self.step_frame(forward);
                    }
                }
                egui::Key::I => self.pending_in = Some(self.position),
                egui::Key::O => self.mark_out(),
                egui::Key::Delete | egui::Key::Backspace => {
                    if let Some(i) = self.selected.take()
                        && i < self.segments.len()
                    {
                        self.segments.remove(i);
                    }
                }
                egui::Key::Home => self.seek(0.0, true),
                egui::Key::End => self.seek(dur, true),
                egui::Key::Plus | egui::Key::Equals => {
                    self.timeline.zoom(1.0 / 1.5, self.position, dur)
                }
                egui::Key::Minus => self.timeline.zoom(1.5, self.position, dur),
                egui::Key::F => self.timeline.zoom_to_fit(dur),
                _ => {}
            }
        }
    }
}

fn default_output(input: &std::path::Path, container: &str) -> String {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "output".into());
    input
        .with_file_name(format!("{stem}_trim.{container}"))
        .to_string_lossy()
        .to_string()
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if self.player.is_none() && self.player_err.is_none() {
            match Player::new(ctx.clone()) {
                Ok(p) => self.player = Some(p),
                Err(e) => self.player_err = Some(e),
            }
        }

        if let Some(path) = self.pending_open.take() {
            self.open(path, ctx);
        }

        self.poll_job(ctx);
        self.shortcuts(ctx);

        self.poll_position();

        let gl = frame.gl().cloned();
        self.video = match (&mut self.player, &gl) {
            (Some(p), Some(gl)) => p.draw(gl, frame, self.video_px),
            _ => None,
        };
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("top").show(ui, |ui| self.ui_top(ui));
        egui::Panel::right("encode")
            .default_size(370.0)
            .size_range(330.0..=560.0)
            .show(ui, |ui| self.ui_encode(ui));
        egui::Panel::bottom("timeline").show(ui, |ui| self.ui_timeline(ui));
        let video = self.video;
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::BLACK))
            .show(ui, |ui| self.ui_video(ui, video));

        if let Some(spec) = self.confirm_overwrite.clone() {
            let mut open = true;
            egui::Window::new("Overwrite file?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .open(&mut open)
                .show(ui.ctx(), |ui| {
                    ui.label(format!("{} already exists.", spec.output.display()));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Overwrite").clicked() {
                            self.confirm_overwrite = None;
                            self.start(spec.clone());
                        }
                        if ui.button("Cancel").clicked() {
                            self.confirm_overwrite = None;
                        }
                    });
                });
            if !open {
                self.confirm_overwrite = None;
            }
        }
    }

    fn on_exit(&mut self, gl: Option<&eframe::glow::Context>) {
        if let (Some(p), Some(gl)) = (&mut self.player, gl) {
            p.destroy(gl);
        }
        self.player = None;
    }
}

impl App {
    fn ui_top(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("Open video…").clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .add_filter(
                        "Video",
                        &[
                            "mp4", "mkv", "mov", "webm", "avi", "m4v", "ts", "flv", "wmv",
                        ],
                    )
                    .pick_file()
            {
                let ctx = ui.ctx().clone();
                self.open(path, &ctx);
            }
            ui.separator();
            ui.label(RichText::new(&self.status).color(Color32::from_gray(190)));
            if let Some(err) = &self.player_err {
                ui.separator();
                ui.colored_label(
                    Color32::from_rgb(230, 120, 120),
                    format!("Preview unavailable: {err}"),
                );
            }
        });
    }

    fn ui_video(&mut self, ui: &mut egui::Ui, video: Option<(egui::TextureId, egui::Rect)>) {
        let avail = ui.available_size();
        let ppp = ui.ctx().pixels_per_point();
        let px = [(avail.x * ppp) as i32, (avail.y * ppp) as i32];
        if px != self.video_px {
            self.video_px = px;
            ui.ctx().request_repaint();
        }
        let (rect, _) = ui.allocate_exact_size(avail, egui::Sense::hover());
        match video {
            Some((id, uv)) => {
                let mut mesh = egui::Mesh::with_texture(id);
                mesh.add_rect_with_uv(rect, uv, Color32::WHITE);
                ui.painter().add(mesh);
            }
            None => {
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "No video loaded",
                    egui::FontId::proportional(16.0),
                    Color32::from_gray(90),
                );
            }
        }
    }

    fn ui_timeline(&mut self, ui: &mut egui::Ui) {
        let dur = self.duration();
        let fps = self.media.as_ref().map(|m| m.fps).unwrap_or(30.0);
        let paused = self.player.as_ref().map(|p| p.paused()).unwrap_or(true);

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button("⏮").on_hover_text("Start (Home)").clicked() {
                self.seek(0.0, true);
            }
            if ui.button("◀◀").on_hover_text("-1 s (Shift+←)").clicked() {
                self.seek(self.position - 1.0, true);
            }
            if ui
                .button("◀|")
                .on_hover_text("Previous frame (←)")
                .clicked()
            {
                self.step_frame(false);
            }
            let play_label = if paused { "▶" } else { "⏸" };
            if ui
                .button(RichText::new(play_label).size(15.0))
                .on_hover_text("Play/pause (Space)")
                .clicked()
                && let Some(p) = &self.player
            {
                p.set_paused(!paused);
            }
            if ui.button("|▶").on_hover_text("Next frame (→)").clicked() {
                self.step_frame(true);
            }
            if ui.button("▶▶").on_hover_text("+1 s (Shift+→)").clicked() {
                self.seek(self.position + 1.0, true);
            }
            if ui.button("⏭").on_hover_text("End (End)").clicked() {
                self.seek(dur, true);
            }

            ui.separator();
            ui.monospace(format!("{} / {}", fmt_time(self.position), fmt_time(dur)));
            ui.monospace(format!("frame {}", (self.position * fps).round() as i64));
        });

        ui.horizontal(|ui| {
            if ui.button("Mark In").on_hover_text("I").clicked() {
                self.pending_in = Some(self.position);
            }
            if ui.button("Mark Out").on_hover_text("O").clicked() {
                self.mark_out();
            }
            if ui
                .button("+ Segment")
                .on_hover_text("Add 5 s from playhead")
                .clicked()
                && dur > 0.0
            {
                let a = self.position;
                let b = (a + 5.0).min(dur);
                if b > a {
                    self.segments.push((a, b));
                    self.selected = Some(self.segments.len() - 1);
                }
            }

            ui.separator();
            ui.checkbox(&mut self.show_keyframes, "keyframes");
            ui.checkbox(&mut self.snap_keyframes, "snap")
                .on_hover_text("Snap edits to keyframes instead of frames");
            if ui
                .checkbox(&mut self.settings.thumbnails, "thumbs")
                .on_hover_text("Draw video frames in the scrub strip")
                .changed()
                && let Err(e) = settings::save(&self.settings)
            {
                self.status = format!("Settings not saved: {e}");
            }
            if ui.button("Fit").on_hover_text("Zoom to fit (F)").clicked() {
                self.timeline.zoom_to_fit(dur);
            }

            ui.separator();
            ui.spacing_mut().slider_width = 80.0;
            if ui
                .add(
                    egui::Slider::new(&mut self.speed, 0.25..=4.0)
                        .logarithmic(true)
                        .text("speed"),
                )
                .changed()
                && let Some(p) = &self.player
            {
                p.set_speed(self.speed);
            }
            if ui
                .add(egui::Slider::new(&mut self.volume, 0..=130).text("vol"))
                .changed()
                && let Some(p) = &self.player
            {
                p.set_volume(self.volume);
            }
        });

        ui.add_space(2.0);
        self.thumbs.set_enabled(self.settings.thumbnails);
        self.thumbs.prepare(
            ui.ctx(),
            self.timeline.view_start,
            self.timeline.view_dur,
            ui.available_width(),
        );
        let keyframes = self.keyframes.lock().unwrap().clone();
        let peaks = self.waveform.lock().unwrap();
        let view = View {
            duration: dur,
            position: self.position,
            fps,
            keyframes: &keyframes,
            show_keyframes: self.show_keyframes || self.mode == TrimMode::Copy,
            snap_keyframes: self.snap_keyframes,
            pending_in: self.pending_in,
            peaks: &peaks,
            has_audio: self.media.as_ref().is_some_and(|m| m.has_audio()),
            thumbs: self.thumbs.tiles(),
        };
        let mut segments = std::mem::take(&mut self.segments);
        let mut selected = self.selected;
        let out = self.timeline.show(ui, &view, &mut segments, &mut selected);
        drop(peaks);
        self.segments = segments;
        self.selected = selected;
        if let Some(t) = out.seek {
            self.seek(t, out.seek_exact);
        }
        ui.add_space(4.0);
    }

    fn ui_encode(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("Segments");
            self.ui_segments(ui);

            ui.add_space(8.0);
            ui.separator();
            ui.heading("Output");
            self.ui_output(ui);

            ui.add_space(8.0);
            ui.separator();
            ui.heading("Preset");
            self.ui_preset(ui);

            ui.add_space(8.0);
            ui.separator();
            self.ui_run(ui);
        });
    }

    fn ui_segments(&mut self, ui: &mut egui::Ui) {
        if self.segments.is_empty() {
            ui.label(RichText::new("Mark In (I) then Mark Out (O) to add one.").weak());
        }
        let dur = self.duration();
        let keyframes = self.keyframes.lock().unwrap().clone();
        let mut remove = None;
        let mut seek_to = None;
        for i in 0..self.segments.len() {
            let selected = self.selected == Some(i);
            let frame = egui::Frame::new()
                .fill(if selected {
                    Color32::from_gray(46)
                } else {
                    Color32::from_gray(32)
                })
                .inner_margin(4.0)
                .corner_radius(3.0);
            frame.show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .selectable_label(selected, format!("#{}", i + 1))
                        .clicked()
                    {
                        self.selected = Some(i);
                    }
                    let (mut a, mut b) = self.segments[i];
                    let changed_a = ui
                        .add(
                            egui::DragValue::new(&mut a)
                                .speed(0.05)
                                .range(0.0..=dur)
                                .fixed_decimals(3),
                        )
                        .changed();
                    ui.label("-");
                    let changed_b = ui
                        .add(
                            egui::DragValue::new(&mut b)
                                .speed(0.05)
                                .range(0.0..=dur)
                                .fixed_decimals(3),
                        )
                        .changed();
                    if changed_a || changed_b {
                        self.segments[i] = (a.min(b), a.max(b));
                    }
                    ui.label(RichText::new(fmt_short(b - a)).weak());
                    if ui.small_button("x").on_hover_text("Delete").clicked() {
                        remove = Some(i);
                    }
                });
                ui.horizontal(|ui| {
                    if ui
                        .small_button("set in")
                        .on_hover_text("Start = playhead")
                        .clicked()
                    {
                        let end = self.segments[i].1;
                        self.segments[i].0 = self.position.min(end);
                    }
                    if ui
                        .small_button("set out")
                        .on_hover_text("End = playhead")
                        .clicked()
                    {
                        let start = self.segments[i].0;
                        self.segments[i].1 = self.position.max(start);
                    }
                    if ui.small_button("go").clicked() {
                        seek_to = Some(self.segments[i].0);
                    }
                    if self.mode == TrimMode::Copy && !keyframes.is_empty() {
                        let a = self.segments[i].0;
                        if let Some(k) = timeline::keyframe_before(&keyframes, a) {
                            let delta = a - k;
                            let color = if delta > 0.05 {
                                Color32::from_rgb(230, 180, 80)
                            } else {
                                Color32::from_gray(150)
                            };
                            ui.label(
                                RichText::new(format!("cuts at {} (−{delta:.3}s)", fmt_time(k)))
                                    .small()
                                    .color(color),
                            );
                            if delta > 0.001 && ui.small_button("snap").clicked() {
                                self.segments[i].0 = k;
                            }
                        }
                    }
                });
            });
        }
        if let Some(i) = remove {
            self.segments.remove(i);
            self.selected = None;
        }
        if let Some(t) = seek_to {
            self.seek(t, true);
        }
        if !self.segments.is_empty() {
            ui.horizontal(|ui| {
                ui.label(format!(
                    "{} segment(s), {} total",
                    self.segments.len(),
                    fmt_time(self.total_selected())
                ));
                if ui.small_button("Clear all").clicked() {
                    self.segments.clear();
                    self.selected = None;
                }
            });
        }
    }

    fn ui_output(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.mode, TrimMode::Reencode, "Re-encode")
                .on_hover_text("Frame-exact cuts, uses the preset below");
            ui.selectable_value(&mut self.mode, TrimMode::Copy, "Stream copy")
                .on_hover_text("Instant, but each cut snaps back to the nearest keyframe");
        });
        ui.checkbox(&mut self.separate, "One file per segment");

        ui.horizontal(|ui| {
            if ui.button("…").clicked() {
                let mut dialog = rfd::FileDialog::new().set_file_name(
                    PathBuf::from(&self.output)
                        .file_name()
                        .map(|f| f.to_string_lossy().to_string())
                        .unwrap_or_else(|| format!("output.{}", self.preset.container)),
                );
                if let Some(dir) = PathBuf::from(&self.output).parent()
                    && dir.is_dir()
                {
                    dialog = dialog.set_directory(dir);
                }
                if let Some(path) = dialog.save_file() {
                    self.output = path.to_string_lossy().to_string();
                }
            }
            ui.add(egui::TextEdit::singleline(&mut self.output).desired_width(f32::INFINITY));
        });
    }

    fn ui_preset(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("preset")
                .selected_text(self.presets[self.preset_idx].name.clone())
                .width(200.0)
                .show_ui(ui, |ui| {
                    for (i, p) in self.presets.iter().enumerate() {
                        if ui.selectable_label(self.preset_idx == i, &p.name).clicked() {
                            self.preset_idx = i;
                        }
                    }
                });
            if ui.button("Load").clicked() {
                self.preset = self.presets[self.preset_idx].clone();
                self.retarget_output();
            }
        });
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.save_name)
                    .hint_text("save current as…")
                    .desired_width(200.0),
            );
            if ui.button("Save").clicked() && !self.save_name.trim().is_empty() {
                let mut p = self.preset.clone();
                p.name = self.save_name.trim().to_string();
                match self.presets.iter().position(|e| e.name == p.name) {
                    Some(i) => self.presets[i] = p,
                    None => self.presets.push(p),
                }
                self.save_name.clear();
                if let Err(e) = presets::save(&self.presets) {
                    self.status = format!("Preset not saved: {e}");
                }
            }
        });

        ui.add_space(4.0);
        let enabled = self.mode == TrimMode::Reencode;
        ui.add_enabled_ui(enabled, |ui| {
            egui::Grid::new("preset_grid")
                .num_columns(2)
                .spacing([8.0, 4.0])
                .show(ui, |ui| {
                    ui.label("Container");
                    ui.horizontal(|ui| {
                        for c in ["mp4", "mkv", "webm"] {
                            if ui.selectable_label(self.preset.container == c, c).clicked() {
                                self.preset.container = c.into();
                                self.retarget_output();
                            }
                        }
                    });
                    ui.end_row();

                    ui.label("Video codec");
                    egui::ComboBox::from_id_salt("vcodec")
                        .selected_text(self.preset.video.label())
                        .show_ui(ui, |ui| {
                            for c in [
                                VideoCodec::Av1Svt,
                                VideoCodec::X265,
                                VideoCodec::X264,
                                VideoCodec::Vp9,
                            ] {
                                ui.selectable_value(&mut self.preset.video, c, c.label());
                            }
                        });
                    ui.end_row();

                    ui.label("Speed");
                    match self.preset.video.numeric_speed() {
                        Some((lo, hi)) => {
                            ui.add(egui::Slider::new(&mut self.preset.speed, lo..=hi))
                                .on_hover_text("Higher is faster and bigger");
                        }
                        None => {
                            egui::ComboBox::from_id_salt("xspeed")
                                .selected_text(self.preset.x_speed.clone())
                                .show_ui(ui, |ui| {
                                    for name in X_SPEEDS {
                                        ui.selectable_value(
                                            &mut self.preset.x_speed,
                                            name.into(),
                                            name,
                                        );
                                    }
                                });
                        }
                    }
                    ui.end_row();

                    ui.label("Rate control");
                    egui::ComboBox::from_id_salt("rate")
                        .selected_text(self.preset.rate.label())
                        .show_ui(ui, |ui| {
                            for r in [RateMode::Crf, RateMode::TargetSize, RateMode::Bitrate] {
                                ui.selectable_value(&mut self.preset.rate, r, r.label());
                            }
                        });
                    ui.end_row();

                    match self.preset.rate {
                        RateMode::Crf => {
                            let (lo, hi) = self.preset.video.crf_range();
                            ui.label("CRF");
                            ui.add(egui::Slider::new(&mut self.preset.crf, lo..=hi))
                                .on_hover_text("Lower is better quality and bigger");
                            ui.end_row();

                            ui.label("Bitrate cap");
                            ui.horizontal(|ui| {
                                let mut capped = self.preset.max_kbps > 0;
                                if ui.checkbox(&mut capped, "").changed() {
                                    self.preset.max_kbps = if capped { 10_000 } else { 0 };
                                }
                                ui.add_enabled(
                                    capped,
                                    egui::DragValue::new(&mut self.preset.max_kbps)
                                        .suffix(" kbps")
                                        .speed(100.0),
                                );
                            })
                            .response
                            .on_hover_text("Capped CRF, only honoured by SVT-AV1");
                            ui.end_row();
                        }
                        RateMode::TargetSize => {
                            ui.label("Target size");
                            ui.add(
                                egui::DragValue::new(&mut self.preset.target_mib)
                                    .suffix(" MiB")
                                    .speed(0.5)
                                    .range(0.1..=100_000.0),
                            );
                            ui.end_row();
                        }
                        RateMode::Bitrate => {
                            ui.label("Video bitrate");
                            ui.add(
                                egui::DragValue::new(&mut self.preset.bitrate_kbps)
                                    .suffix(" kbps")
                                    .speed(50.0),
                            );
                            ui.end_row();
                        }
                    }

                    if self.preset.rate != RateMode::Crf {
                        ui.label("Two-pass");
                        ui.checkbox(&mut self.preset.two_pass, "accurate, twice as slow");
                        ui.end_row();
                    }

                    ui.label("Audio");
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("acodec")
                            .selected_text(self.preset.audio.label())
                            .width(80.0)
                            .show_ui(ui, |ui| {
                                for c in [
                                    AudioCodec::Opus,
                                    AudioCodec::Aac,
                                    AudioCodec::Copy,
                                    AudioCodec::None,
                                ] {
                                    ui.selectable_value(&mut self.preset.audio, c, c.label());
                                }
                            });
                        let uses_bitrate =
                            matches!(self.preset.audio, AudioCodec::Opus | AudioCodec::Aac);
                        ui.add_enabled(
                            uses_bitrate,
                            egui::DragValue::new(&mut self.preset.audio_kbps)
                                .suffix(" kbps")
                                .speed(8.0),
                        );
                    });
                    ui.end_row();

                    ui.label("Scale height");
                    ui.horizontal(|ui| {
                        let mut scaling = self.preset.scale_height > 0;
                        if ui.checkbox(&mut scaling, "").changed() {
                            self.preset.scale_height = if scaling { 1080 } else { 0 };
                        }
                        ui.add_enabled(
                            scaling,
                            egui::DragValue::new(&mut self.preset.scale_height)
                                .suffix(" px")
                                .speed(10.0),
                        );
                    });
                    ui.end_row();

                    ui.label("FPS cap");
                    ui.horizontal(|ui| {
                        let mut capped = self.preset.fps_cap > 0.0;
                        if ui.checkbox(&mut capped, "").changed() {
                            self.preset.fps_cap = if capped { 30.0 } else { 0.0 };
                        }
                        ui.add_enabled(
                            capped,
                            egui::DragValue::new(&mut self.preset.fps_cap).speed(1.0),
                        );
                    });
                    ui.end_row();

                    if self.preset.video == VideoCodec::Av1Svt {
                        ui.label("svtav1-params");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.preset.svt_params)
                                .desired_width(190.0),
                        );
                        ui.end_row();
                    }

                    ui.label("Extra args");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.preset.extra_args)
                            .hint_text("-g 240")
                            .desired_width(190.0),
                    );
                    ui.end_row();
                });
        });
    }

    fn retarget_output(&mut self) {
        if let Some(m) = &self.media {
            self.output = default_output(&m.path, &self.preset.container);
        }
    }

    fn ui_run(&mut self, ui: &mut egui::Ui) {
        let running = self.job.is_some();
        ui.horizontal(|ui| {
            let can_run = !running && !self.segments.is_empty() && self.media.is_some();
            if ui
                .add_enabled(can_run, egui::Button::new(RichText::new("Encode").strong()))
                .clicked()
            {
                match self.build_spec() {
                    Ok(spec) => {
                        if spec.output.exists() {
                            self.confirm_overwrite = Some(spec);
                        } else {
                            self.start(spec);
                        }
                    }
                    Err(e) => self.status = format!("Cannot start: {e}"),
                }
            }
            if ui
                .add_enabled(running, egui::Button::new("Cancel"))
                .clicked()
                && let Some(job) = &self.job
            {
                job.cancel();
            }
            ui.checkbox(&mut self.show_command, "show command");
        });

        if running {
            ui.add(
                egui::ProgressBar::new(self.progress as f32)
                    .show_percentage()
                    .animate(true),
            );
            ui.label(
                RichText::new(format!("{} {}", self.progress_label, self.progress_speed))
                    .small()
                    .weak(),
            );
        }

        if self.preset.rate == RateMode::TargetSize && self.mode == TrimMode::Reencode {
            let total = self.total_selected();
            if total > 0.0 {
                let kbps = self.preset.target_mib * 1024.0 * 1024.0 * 8.0 / total / 1000.0;
                ui.label(
                    RichText::new(format!("≈ {kbps:.0} kbps over {}", fmt_time(total)))
                        .small()
                        .weak(),
                );
            }
        }

        if self.show_command
            && let Ok(plan) = self.build_spec().and_then(|s| encode::plan(&s))
        {
            ui.add_space(4.0);
            let mut text = encode::command_preview(&plan);
            ui.add(
                egui::TextEdit::multiline(&mut text)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY),
            );
        }

        if !self.log.is_empty() {
            ui.add_space(6.0);
            ui.collapsing("ffmpeg output", |ui| {
                egui::ScrollArea::vertical()
                    .max_height(160.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in &self.log {
                            ui.label(RichText::new(line).small().monospace());
                        }
                    });
            });
        }
    }
}
