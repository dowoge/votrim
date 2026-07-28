use crate::media::{self, MediaInfo};
use egui::{ColorImage, TextureHandle, TextureId, TextureOptions};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};

const MARGIN: i64 = 8;

/// Seeks spend as much time waiting on the file as decoding, so the pool runs
/// wider than the core count would suggest but stops paying off well before it.
fn workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(1).clamp(2, 8))
        .unwrap_or(3)
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    level: i32,
    idx: i64,
}

struct Job {
    generation: u64,
    key: Key,
    path: PathBuf,
    time: f64,
    w: u32,
    h: u32,
    coarse: bool,
}

pub struct Tile {
    pub t0: f64,
    pub t1: f64,
    pub id: TextureId,
    pub aspect: f32,
    pub dim: bool,
}

struct Source {
    path: PathBuf,
    duration: f64,
    frame_dur: f64,
    w: u32,
    h: u32,
}

/// Whichever worker is free takes the next job, so one slow seek cannot hold up
/// tiles the others could be decoding. Taken from the back: during a zoom the
/// tiles just asked for are the ones on screen, and older requests are usually
/// for a level that has already been left behind.
#[derive(Default)]
struct Queue {
    jobs: Mutex<VecDeque<Job>>,
    ready: Condvar,
}

impl Queue {
    fn take(&self) -> Job {
        let mut jobs = self.jobs.lock().unwrap();
        loop {
            match jobs.pop_back() {
                Some(job) => return job,
                None => jobs = self.ready.wait(jobs).unwrap(),
            }
        }
    }
}

/// Frames of the open clip, drawn as a strip behind the scrub bar. A coarse
/// level spanning the whole file is extracted once and kept, so zooming always
/// has something to show while the sharper tiles for that zoom are decoding.
pub struct Thumbs {
    enabled: bool,
    source: Option<Source>,
    tex: HashMap<Key, TextureHandle>,
    /// Keys already queued. A tile whose extraction fails stays here, so a file
    /// ffmpeg cannot seek is attempted once instead of on every frame.
    pending: HashSet<Key>,
    draw: Vec<Tile>,
    level: i32,
    shared_level: Arc<AtomicI64>,
    generation: Arc<AtomicU64>,
    children: Arc<Mutex<HashMap<u32, Child>>>,
    queue: Arc<Queue>,
    done: Receiver<(u64, Key, Vec<u8>)>,
}

impl Thumbs {
    pub fn new(ctx: egui::Context) -> Self {
        let generation = Arc::new(AtomicU64::new(0));
        let shared_level = Arc::new(AtomicI64::new(0));
        let children: Arc<Mutex<HashMap<u32, Child>>> = Arc::default();
        let queue: Arc<Queue> = Arc::default();
        let (results, done) = channel();
        for _ in 0..workers() {
            let queue = queue.clone();
            let generation = generation.clone();
            let level = shared_level.clone();
            let children = children.clone();
            let results = results.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || work(queue, results, children, generation, level, ctx));
        }
        Self {
            enabled: true,
            source: None,
            tex: HashMap::new(),
            pending: HashSet::new(),
            draw: Vec::new(),
            level: 0,
            shared_level,
            generation,
            children,
            queue,
            done,
        }
    }

    pub fn open(&mut self, info: &MediaInfo) {
        self.discard();
        self.source = info
            .thumb_size()
            .filter(|_| info.duration > 0.0)
            .map(|(w, h)| Source {
                path: info.path.clone(),
                duration: info.duration,
                frame_dur: info.frame_dur(),
                w,
                h,
            });
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        if enabled != self.enabled {
            self.enabled = enabled;
            if !enabled {
                self.discard();
            }
        }
    }

    pub fn tiles(&self) -> &[Tile] {
        &self.draw
    }

    /// Drops every extraction still in flight; the bumped generation makes any
    /// result that slips through land after the kill get thrown away.
    fn discard(&mut self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        for (_, mut child) in self.children.lock().unwrap().drain() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.tex.clear();
        self.pending.clear();
        self.draw.clear();
    }

    pub fn prepare(&mut self, ctx: &egui::Context, view_start: f64, view_dur: f64, width: f32) {
        self.collect(ctx);
        self.draw.clear();
        if !self.enabled || width <= 0.0 || view_dur <= 0.0 {
            return;
        }
        let Some(src) = self.source.as_ref() else {
            return;
        };

        let duration = src.duration;
        let (w, h) = (src.w, src.h);
        let aspect = w as f32 / h as f32;
        // A tile is as wide as one frame drawn at the strip's height, so the
        // coarse level is whatever spans the whole clip in a single screenful.
        let tile_frac = (crate::timeline::SCRUB_H * aspect) as f64 / width as f64;
        let coarse_level = level_for(tile_frac * duration, src.frame_dur);
        let level = level_for(tile_frac * view_dur, src.frame_dur).min(coarse_level);
        let dt = level_dt(src.frame_dur, level);
        let coarse_dt = level_dt(src.frame_dur, coarse_level);
        let last = |step: f64| ((duration / step).ceil() as i64 - 1).max(0);

        let lo = ((view_start / dt).floor() as i64 - MARGIN).max(0);
        let hi = (((view_start + view_dur) / dt).floor() as i64 + MARGIN).min(last(dt));
        self.tex.retain(|k, _| {
            k.level == coarse_level || (k.level == level && (lo..=hi).contains(&k.idx))
        });
        if level != self.level {
            self.pending.retain(|k| k.level == coarse_level);
            let mut jobs = self.queue.jobs.lock().unwrap();
            jobs.retain(|j| j.coarse || j.key.level == level);
            drop(jobs);
            self.level = level;
            self.shared_level.store(level as i64, Ordering::SeqCst);
        }

        let mut wanted = Vec::new();
        let view_end = view_start + view_dur;
        for idx in 0..=last(coarse_dt) {
            let key = Key {
                level: coarse_level,
                idx,
            };
            let t0 = idx as f64 * coarse_dt;
            let t1 = (t0 + coarse_dt).min(duration);
            match self.tex.get(&key) {
                Some(tex) if t1 > view_start && t0 < view_end => self.draw.push(Tile {
                    t0,
                    t1,
                    id: tex.id(),
                    aspect,
                    dim: level != coarse_level,
                }),
                None if !self.pending.contains(&key) => wanted.push((key, t0, true)),
                _ => {}
            }
        }
        if level != coarse_level {
            for idx in lo..=hi {
                let key = Key { level, idx };
                let t0 = idx as f64 * dt;
                match self.tex.get(&key) {
                    Some(tex) => self.draw.push(Tile {
                        t0,
                        t1: (t0 + dt).min(duration),
                        id: tex.id(),
                        aspect,
                        dim: false,
                    }),
                    None if !self.pending.contains(&key) => wanted.push((key, t0, false)),
                    None => {}
                }
            }
        }

        if !wanted.is_empty() {
            let path = src.path.clone();
            let generation = self.generation.load(Ordering::SeqCst);
            let mut jobs = self.queue.jobs.lock().unwrap();
            for (key, time, coarse) in wanted {
                jobs.push_back(Job {
                    generation,
                    key,
                    path: path.clone(),
                    time,
                    w,
                    h,
                    coarse,
                });
                self.pending.insert(key);
            }
            drop(jobs);
            self.queue.ready.notify_all();
        }
    }

    fn collect(&mut self, ctx: &egui::Context) {
        let generation = self.generation.load(Ordering::SeqCst);
        while let Ok((sent, key, rgba)) = self.done.try_recv() {
            if sent != generation {
                continue;
            }
            self.pending.remove(&key);
            let Some(src) = self.source.as_ref() else {
                continue;
            };
            let image = ColorImage::from_rgba_unmultiplied([src.w as usize, src.h as usize], &rgba);
            self.tex.insert(
                key,
                ctx.load_texture("thumb", image, TextureOptions::LINEAR),
            );
        }
    }
}

fn work(
    queue: Arc<Queue>,
    results: Sender<(u64, Key, Vec<u8>)>,
    children: Arc<Mutex<HashMap<u32, Child>>>,
    generation: Arc<AtomicU64>,
    level: Arc<AtomicI64>,
    ctx: egui::Context,
) {
    loop {
        let job = queue.take();
        if job.generation != generation.load(Ordering::SeqCst)
            || (!job.coarse && i64::from(job.key.level) != level.load(Ordering::SeqCst))
        {
            continue;
        }
        let Ok(mut child) = media::thumb_cmd(&job.path, job.time, job.w, job.h, job.coarse).spawn()
        else {
            continue;
        };
        let Some(stdout) = child.stdout.take() else {
            continue;
        };
        let id = child.id();
        children.lock().unwrap().insert(id, child);
        let frame = media::read_frame(stdout, job.w, job.h);
        if let Some(mut child) = children.lock().unwrap().remove(&id) {
            let _ = child.wait();
        }
        if let Ok(rgba) = frame
            && job.generation == generation.load(Ordering::SeqCst)
        {
            if results.send((job.generation, job.key, rgba)).is_err() {
                return;
            }
            ctx.request_repaint();
        }
    }
}

fn level_dt(frame_dur: f64, level: i32) -> f64 {
    frame_dur * f64::from(level).exp2()
}

/// Nearest spacing to `ideal` on the frame-doubling ladder, so zooming
/// re-extracts in steps instead of on every scroll tick.
fn level_for(ideal: f64, frame_dur: f64) -> i32 {
    if frame_dur <= 0.0 || ideal <= frame_dur {
        return 0;
    }
    (ideal / frame_dur).log2().round() as i32
}
