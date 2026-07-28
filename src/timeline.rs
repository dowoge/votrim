use egui::{
    Align2, Color32, CornerRadius, CursorIcon, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2,
};

const RULER_H: f32 = 16.0;
pub const SCRUB_H: f32 = 40.0;
const WAVE_H: f32 = 32.0;
const SEG_H: f32 = 16.0;
const OVERVIEW_H: f32 = 12.0;
const GAP: f32 = 3.0;
const HANDLE_PX: f32 = 5.0;

const NICE_STEPS: [f64; 20] = [
    0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0,
    900.0, 1800.0, 3600.0, 7200.0,
];

#[derive(Clone, Copy)]
enum Drag {
    Playhead,
    Pan { grab_time: f64 },
    SegStart(usize),
    SegEnd(usize),
    SegBody { idx: usize, grab: f64 },
    Overview,
}

pub struct Timeline {
    pub view_start: f64,
    pub view_dur: f64,
    drag: Option<Drag>,
}

impl Default for Timeline {
    fn default() -> Self {
        Self {
            view_start: 0.0,
            view_dur: 1.0,
            drag: None,
        }
    }
}

#[derive(Default)]
pub struct Output {
    pub seek: Option<f64>,
    pub seek_exact: bool,
}

pub struct View<'a> {
    pub duration: f64,
    pub position: f64,
    pub fps: f64,
    pub keyframes: &'a [f64],
    pub show_keyframes: bool,
    pub snap_keyframes: bool,
    pub pending_in: Option<f64>,
    pub peaks: &'a [(f32, f32)],
    pub has_audio: bool,
    pub thumbs: &'a [crate::thumbs::Tile],
}

impl Timeline {
    pub fn reset(&mut self, duration: f64) {
        self.view_start = 0.0;
        self.view_dur = duration.max(0.001);
        self.drag = None;
    }

    pub fn zoom_to_fit(&mut self, duration: f64) {
        self.reset(duration);
    }

    pub fn zoom(&mut self, factor: f64, around: f64, duration: f64) {
        let min_dur = 0.05_f64.min(duration.max(0.001));
        let new_dur = (self.view_dur * factor).clamp(min_dur, duration.max(0.001));
        let ratio = if self.view_dur > 0.0 {
            (around - self.view_start) / self.view_dur
        } else {
            0.5
        };
        self.view_start = around - ratio * new_dur;
        self.view_dur = new_dur;
        self.clamp_view(duration);
    }

    fn clamp_view(&mut self, duration: f64) {
        self.view_dur = self.view_dur.min(duration.max(0.001));
        self.view_start = self
            .view_start
            .clamp(0.0, (duration - self.view_dur).max(0.0));
    }

    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }

    fn x_of(&self, t: f64, rect: Rect) -> f32 {
        rect.left() + ((t - self.view_start) / self.view_dur) as f32 * rect.width()
    }

    fn t_of(&self, x: f32, rect: Rect) -> f64 {
        self.view_start + ((x - rect.left()) / rect.width()) as f64 * self.view_dur
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        v: &View<'_>,
        segments: &mut [(f64, f64)],
        selected: &mut Option<usize>,
    ) -> Output {
        let mut out = Output::default();
        let wave_h = if v.has_audio { WAVE_H + GAP } else { 0.0 };
        let height = RULER_H + SCRUB_H + wave_h + SEG_H + OVERVIEW_H + GAP * 3.0;
        let (rect, response) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), height),
            Sense::click_and_drag(),
        );
        if v.duration <= 0.0 {
            ui.painter_at(rect)
                .rect_filled(rect, 4.0, Color32::from_gray(24));
            return out;
        }
        self.clamp_view(v.duration);

        let ruler_rect = Rect::from_min_size(rect.min, Vec2::new(rect.width(), RULER_H));
        let scrub_rect = Rect::from_min_size(
            Pos2::new(rect.left(), ruler_rect.bottom() + GAP),
            Vec2::new(rect.width(), SCRUB_H),
        );
        let wave_rect = Rect::from_min_size(
            Pos2::new(rect.left(), scrub_rect.bottom() + GAP),
            Vec2::new(rect.width(), WAVE_H),
        );
        let seg_rect = Rect::from_min_size(
            Pos2::new(rect.left(), scrub_rect.bottom() + wave_h + GAP),
            Vec2::new(rect.width(), SEG_H),
        );
        let overview_rect = Rect::from_min_size(
            Pos2::new(rect.left(), seg_rect.bottom() + GAP),
            Vec2::new(rect.width(), OVERVIEW_H),
        );

        let snap = |t: f64| -> f64 {
            let t = t.clamp(0.0, v.duration);
            if v.snap_keyframes && !v.keyframes.is_empty() {
                return nearest(v.keyframes, t);
            }
            if v.fps > 0.0 {
                (t * v.fps).round() / v.fps
            } else {
                t
            }
        };

        let modifiers = ui.input(|i| i.modifiers);
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if response.hovered() && scroll != 0.0 {
            let anchor = ui
                .input(|i| i.pointer.hover_pos())
                .map(|p| self.t_of(p.x, rect))
                .unwrap_or(v.position);
            self.zoom(
                if scroll > 0.0 { 1.0 / 1.2 } else { 1.2 },
                anchor,
                v.duration,
            );
        }

        if response.drag_started()
            && let Some(p) = response.interact_pointer_pos()
        {
            self.drag = Some(self.begin_drag(
                p,
                rect,
                seg_rect,
                overview_rect,
                segments,
                modifiers,
            ));
            if let Some(Drag::SegStart(i) | Drag::SegEnd(i) | Drag::SegBody { idx: i, .. }) =
                self.drag
            {
                *selected = Some(i);
            }
        }

        if let (Some(drag), Some(p)) = (self.drag, response.interact_pointer_pos()) {
            match drag {
                Drag::Playhead => {
                    out.seek = Some(snap(self.t_of(p.x, rect)));
                }
                Drag::Pan { grab_time } => {
                    let ratio = ((p.x - rect.left()) / rect.width()) as f64;
                    self.view_start = grab_time - ratio * self.view_dur;
                    self.clamp_view(v.duration);
                }
                Drag::Overview => {
                    let center = (p.x - rect.left()) as f64 / rect.width() as f64 * v.duration;
                    self.view_start = center - self.view_dur / 2.0;
                    self.clamp_view(v.duration);
                }
                Drag::SegStart(i) => {
                    let t = snap(self.t_of(p.x, rect));
                    if let Some(seg) = segments.get_mut(i) {
                        seg.0 = t.min(seg.1 - 0.001);
                        out.seek = Some(seg.0);
                    }
                }
                Drag::SegEnd(i) => {
                    let t = snap(self.t_of(p.x, rect));
                    if let Some(seg) = segments.get_mut(i) {
                        seg.1 = t.max(seg.0 + 0.001);
                        out.seek = Some(seg.1);
                    }
                }
                Drag::SegBody { idx, grab } => {
                    let t = self.t_of(p.x, rect);
                    if let Some(seg) = segments.get_mut(idx) {
                        let len = seg.1 - seg.0;
                        let start = (t - grab).clamp(0.0, v.duration - len);
                        seg.0 = start;
                        seg.1 = start + len;
                    }
                }
            }
        }

        if response.drag_stopped() {
            if matches!(
                self.drag,
                Some(Drag::Playhead | Drag::SegStart(_) | Drag::SegEnd(_))
            ) {
                out.seek = Some(out.seek.unwrap_or(v.position));
                out.seek_exact = true;
            }
            self.drag = None;
        }

        if response.clicked()
            && let Some(p) = response.interact_pointer_pos()
        {
            if seg_rect.contains(p) {
                *selected = segments.iter().position(|&(a, b)| {
                    p.x >= self.x_of(a, rect) - HANDLE_PX && p.x <= self.x_of(b, rect) + HANDLE_PX
                });
            } else if !overview_rect.contains(p) {
                out.seek = Some(snap(self.t_of(p.x, rect)));
                out.seek_exact = true;
            }
        }

        if let Some(p) = response.hover_pos()
            && !overview_rect.contains(p)
            && (!seg_rect.contains(p) || self.edge_at(segments, p.x, rect).is_some())
        {
            ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
        }

        self.paint(
            ui,
            rect,
            ruler_rect,
            scrub_rect,
            wave_rect,
            seg_rect,
            overview_rect,
            v,
            segments,
            *selected,
        );
        out
    }

    fn edge_at(&self, segments: &[(f64, f64)], x: f32, rect: Rect) -> Option<Drag> {
        for (i, &(a, b)) in segments.iter().enumerate() {
            if (x - self.x_of(a, rect)).abs() <= HANDLE_PX {
                return Some(Drag::SegStart(i));
            }
            if (x - self.x_of(b, rect)).abs() <= HANDLE_PX {
                return Some(Drag::SegEnd(i));
            }
        }
        None
    }

    fn begin_drag(
        &self,
        p: Pos2,
        rect: Rect,
        seg_rect: Rect,
        overview_rect: Rect,
        segments: &[(f64, f64)],
        modifiers: egui::Modifiers,
    ) -> Drag {
        if overview_rect.contains(p) {
            return Drag::Overview;
        }
        if modifiers.shift {
            return Drag::Pan {
                grab_time: self.t_of(p.x, rect),
            };
        }
        if seg_rect.contains(p) {
            if let Some(drag) = self.edge_at(segments, p.x, rect) {
                return drag;
            }
            let time = self.t_of(p.x, rect);
            if let Some(idx) = segments.iter().position(|&(a, b)| time >= a && time <= b) {
                return Drag::SegBody {
                    idx,
                    grab: time - segments[idx].0,
                };
            }
        }
        Drag::Playhead
    }

    #[allow(clippy::too_many_arguments)]
    fn paint(
        &self,
        ui: &egui::Ui,
        rect: Rect,
        ruler_rect: Rect,
        scrub_rect: Rect,
        wave_rect: Rect,
        seg_rect: Rect,
        overview_rect: Rect,
        v: &View<'_>,
        segments: &[(f64, f64)],
        selected: Option<usize>,
    ) {
        let p = ui.painter_at(rect);
        let no_round = CornerRadius::ZERO;
        let ov_x_of = |t: f64| rect.left() + (t / v.duration) as f32 * rect.width();

        p.rect_filled(scrub_rect, 3.0, Color32::from_gray(28));
        p.rect_filled(seg_rect, 3.0, Color32::from_gray(22));
        p.rect_filled(overview_rect, 3.0, Color32::from_gray(22));

        if !v.thumbs.is_empty() {
            let strip = ui.painter_at(scrub_rect);
            for t in v.thumbs {
                let slot = Rect::from_x_y_ranges(
                    self.x_of(t.t0, rect)..=self.x_of(t.t1, rect),
                    scrub_rect.y_range(),
                );
                if slot.width() < 0.5
                    || slot.right() < scrub_rect.left()
                    || slot.left() > scrub_rect.right()
                {
                    continue;
                }
                let tint = if t.dim {
                    Color32::from_gray(110)
                } else {
                    Color32::WHITE
                };
                let uv = cover_uv(slot.width() / slot.height(), t.aspect);
                strip.image(t.id, slot, uv, tint);
            }
        }

        if v.has_audio {
            p.rect_filled(wave_rect, 3.0, Color32::from_gray(18));
            let mid = wave_rect.center().y;
            p.hline(
                wave_rect.x_range(),
                mid,
                Stroke::new(1.0, Color32::from_gray(45)),
            );
            let half = wave_rect.height() / 2.0 - 2.0;
            let stroke = Stroke::new(1.0, Color32::from_rgb(85, 125, 160));
            let rate = crate::media::PEAKS_PER_SEC as f64;
            let mut x = wave_rect.left();
            while x <= wave_rect.right() {
                let i0 = (self.t_of(x, rect) * rate).floor().max(0.0) as usize;
                let i1 =
                    ((self.t_of(x + 1.0, rect) * rate).ceil().max(0.0) as usize).min(v.peaks.len());
                if i0 < i1 {
                    let (lo, hi) = v.peaks[i0..i1]
                        .iter()
                        .fold((0.0f32, 0.0f32), |(l, h), &(a, b)| (l.min(a), h.max(b)));
                    p.vline(x, mid - hi * half..=mid - lo * half, stroke);
                }
                x += 1.0;
            }
        }

        let px_per_sec = rect.width() as f64 / self.view_dur;
        let step = *NICE_STEPS
            .iter()
            .find(|s| *s * px_per_sec >= 80.0)
            .unwrap_or(&NICE_STEPS[NICE_STEPS.len() - 1]);
        let mut t = (self.view_start / step).floor() * step;
        while t <= self.view_start + self.view_dur {
            if t >= 0.0 {
                let x = self.x_of(t, rect);
                p.vline(
                    x,
                    ruler_rect.bottom() - 5.0..=ruler_rect.bottom(),
                    Stroke::new(1.0, Color32::from_gray(90)),
                );
                p.text(
                    Pos2::new(x + 2.0, ruler_rect.top()),
                    Align2::LEFT_TOP,
                    tick_label(t, step),
                    FontId::monospace(10.0),
                    Color32::from_gray(150),
                );
            }
            t += step;
        }

        if v.show_keyframes && !v.keyframes.is_empty() {
            let stroke = Stroke::new(1.0, Color32::from_rgb(95, 115, 160));
            let y = scrub_rect.bottom() - 7.0..=scrub_rect.bottom() - 1.0;
            for &k in v.keyframes {
                if k >= self.view_start && k <= self.view_start + self.view_dur {
                    p.vline(self.x_of(k, rect), y.clone(), stroke);
                }
            }
        }

        for (i, &(a, b)) in segments.iter().enumerate() {
            let x0 = self.x_of(a, rect).max(rect.left());
            let x1 = self.x_of(b, rect).min(rect.right());
            if x1 < rect.left() || x0 > rect.right() || x1 < x0 {
                continue;
            }
            let is_sel = selected == Some(i);
            let shade = if is_sel {
                Color32::from_rgba_unmultiplied(80, 200, 120, 60)
            } else {
                Color32::from_rgba_unmultiplied(80, 160, 200, 40)
            };
            p.rect_filled(
                Rect::from_x_y_ranges(x0..=x1, scrub_rect.y_range()),
                no_round,
                shade,
            );

            let bar =
                Rect::from_x_y_ranges(x0..=x1, seg_rect.y_range()).shrink2(Vec2::new(0.0, 2.0));
            let solid = if is_sel {
                Color32::from_rgb(80, 200, 120)
            } else {
                Color32::from_rgb(70, 130, 170)
            };
            p.rect_filled(bar, 2.0, solid);
            let edge = Stroke::new(2.0, Color32::from_gray(235));
            p.vline(x0, bar.y_range(), edge);
            p.vline(x1, bar.y_range(), edge);
            if bar.width() > 34.0 {
                p.text(
                    bar.center(),
                    Align2::CENTER_CENTER,
                    format!("{}", i + 1),
                    FontId::monospace(10.0),
                    Color32::from_gray(20),
                );
            }
        }

        if let Some(start) = v.pending_in {
            let x0 = self.x_of(start, rect).max(rect.left());
            let x1 = self.x_of(v.position, rect).min(rect.right());
            if x1 > x0 {
                p.rect_filled(
                    Rect::from_x_y_ranges(x0..=x1, scrub_rect.y_range()),
                    no_round,
                    Color32::from_rgba_unmultiplied(220, 180, 60, 40),
                );
            }
            p.vline(
                self.x_of(start, rect),
                scrub_rect.y_range(),
                Stroke::new(1.5, Color32::from_rgb(230, 190, 70)),
            );
        }

        let px = self.x_of(v.position, rect);
        if px >= rect.left() - 1.0 && px <= rect.right() + 1.0 {
            let bottom = if v.has_audio {
                wave_rect.bottom()
            } else {
                scrub_rect.bottom()
            };
            p.vline(
                px,
                scrub_rect.top()..=bottom,
                Stroke::new(1.5, Color32::from_rgb(240, 70, 70)),
            );
            p.rect_filled(
                Rect::from_center_size(Pos2::new(px, scrub_rect.top() + 3.0), Vec2::splat(7.0)),
                2.0,
                Color32::from_rgb(240, 70, 70),
            );
        }

        for &(a, b) in segments {
            p.rect_filled(
                Rect::from_x_y_ranges(ov_x_of(a)..=ov_x_of(b), overview_rect.y_range()),
                no_round,
                Color32::from_rgb(70, 130, 170),
            );
        }
        p.vline(
            ov_x_of(v.position),
            overview_rect.y_range(),
            Stroke::new(1.0, Color32::from_rgb(240, 70, 70)),
        );
        let window = Rect::from_x_y_ranges(
            ov_x_of(self.view_start)..=ov_x_of(self.view_start + self.view_dur),
            overview_rect.y_range(),
        );
        p.rect_stroke(
            window,
            2.0,
            Stroke::new(1.0, Color32::from_gray(200)),
            StrokeKind::Inside,
        );
    }
}

pub fn nearest(sorted: &[f64], t: f64) -> f64 {
    match sorted.binary_search_by(|p| p.total_cmp(&t)) {
        Ok(i) => sorted[i],
        Err(0) => sorted[0],
        Err(i) if i >= sorted.len() => sorted[sorted.len() - 1],
        Err(i) => {
            let (lo, hi) = (sorted[i - 1], sorted[i]);
            if t - lo <= hi - t { lo } else { hi }
        }
    }
}

/// Keyframe at or before `t`, which is where a stream copy will actually cut.
pub fn keyframe_before(sorted: &[f64], t: f64) -> Option<f64> {
    let i = sorted.partition_point(|&k| k <= t + 1e-6);
    (i > 0).then(|| sorted[i - 1])
}

/// Centre-crops a frame so it keeps the clip's aspect instead of stretching to
/// whatever width the zoom level gives its tile.
fn cover_uv(slot: f32, src: f32) -> Rect {
    let size = if slot < src {
        Vec2::new(slot / src, 1.0)
    } else {
        Vec2::new(1.0, src / slot)
    };
    Rect::from_center_size(Pos2::new(0.5, 0.5), size)
}

fn tick_label(t: f64, step: f64) -> String {
    let h = (t / 3600.0).floor() as u64;
    let m = ((t / 60.0).floor() as u64) % 60;
    let s = t - (h * 3600 + m * 60) as f64;
    if step < 1.0 {
        format!("{m:02}:{s:06.3}")
    } else if h > 0 {
        format!("{h}:{m:02}:{:02}", s as u64)
    } else {
        format!("{m:02}:{:02}", s as u64)
    }
}
