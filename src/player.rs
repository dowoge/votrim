use eframe::glow::{self, HasContext};
use libmpv2::Mpv;
use libmpv2::render::{OpenGLInitParams, RenderContext, RenderParam, RenderParamApiType};
use std::ffi::{CString, c_char, c_void};
use std::num::NonZeroU32;
use std::path::Path;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

/// Resolves GL entry points for mpv. eframe owns the GL context and does not expose
/// its loader, so the symbols are looked up straight out of the client libraries.
pub struct GlLoader {
    handles: [*mut c_void; 3],
    egl_get_proc: Option<unsafe extern "C" fn(*const c_char) -> *mut c_void>,
}

unsafe impl Send for GlLoader {}
unsafe impl Sync for GlLoader {}

impl GlLoader {
    fn new() -> Self {
        unsafe {
            let gl = libc::dlopen(c"libGL.so.1".as_ptr(), libc::RTLD_LAZY);
            let gles = libc::dlopen(c"libGLESv2.so.2".as_ptr(), libc::RTLD_LAZY);
            let egl = libc::dlopen(c"libEGL.so.1".as_ptr(), libc::RTLD_LAZY);
            let egl_get_proc = (!egl.is_null())
                .then(|| libc::dlsym(egl, c"eglGetProcAddress".as_ptr()))
                .filter(|p| !p.is_null())
                .map(|p| {
                    std::mem::transmute::<
                        *mut c_void,
                        unsafe extern "C" fn(*const c_char) -> *mut c_void,
                    >(p)
                });
            Self {
                handles: [gl, gles, egl],
                egl_get_proc,
            }
        }
    }

    fn resolve(&self, name: &str) -> *mut c_void {
        let Ok(cname) = CString::new(name) else {
            return ptr::null_mut();
        };
        unsafe {
            for handle in self.handles {
                if !handle.is_null() {
                    let sym = libc::dlsym(handle, cname.as_ptr());
                    if !sym.is_null() {
                        return sym;
                    }
                }
            }
            if let Some(get_proc) = self.egl_get_proc {
                return get_proc(cname.as_ptr());
            }
        }
        ptr::null_mut()
    }
}

fn get_proc_address(loader: &GlLoader, name: &str) -> *mut c_void {
    loader.resolve(name)
}

fn round_up(v: i32, step: i32) -> i32 {
    ((v + step - 1) / step) * step
}

/// Every `mpv_command`/`mpv_get_property` call is synchronous: during a seek the
/// core can take tens of milliseconds to answer. Commands are therefore issued
/// from a worker thread and player state is read from a cache, so the UI thread
/// never blocks on mpv.
enum Cmd {
    Load(String),
    Seek { time: f64, exact: bool },
    FrameStep(bool),
    SetPause(bool),
    SetVolume(i64),
    SetSpeed(f64),
}

#[derive(Default)]
struct Cache {
    time_pos: AtomicU64,
    paused: AtomicBool,
    loaded: AtomicBool,
    shutdown: AtomicBool,
}

pub struct Player {
    render: RenderContext<'static>,
    frame_ready: Arc<AtomicBool>,
    cache: Arc<Cache>,
    tx: Sender<Cmd>,
    tex: Option<glow::Texture>,
    fbo: Option<glow::Framebuffer>,
    tex_id: Option<egui::TextureId>,
    tex_size: [i32; 2],
    last_draw: [i32; 2],
}

fn run_cmd(mpv: &Mpv, cmd: &Cmd) {
    let _ = match cmd {
        Cmd::Load(path) => mpv.command("loadfile", &[path, "replace"]),
        Cmd::Seek { time, exact } => {
            let mode = if *exact {
                "absolute+exact"
            } else {
                "absolute+keyframes"
            };
            mpv.command("seek", &[&format!("{time:.6}"), mode])
        }
        Cmd::FrameStep(true) => mpv.command("frame-step", &[]),
        Cmd::FrameStep(false) => mpv.command("frame-back-step", &[]),
        Cmd::SetPause(paused) => mpv.set_property("pause", *paused),
        Cmd::SetVolume(volume) => mpv.set_property("volume", *volume),
        Cmd::SetSpeed(speed) => mpv.set_property("speed", *speed),
    };
}

/// Runs queued commands, keeping only the newest seek: a scrub drag produces
/// them far faster than mpv can service them, and every stale one is wasted work.
fn command_worker(mpv: &'static Mpv, rx: Receiver<Cmd>) {
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        batch.extend(rx.try_iter());
        let last_seek = batch.iter().rposition(|c| matches!(c, Cmd::Seek { .. }));
        for (i, cmd) in batch.iter().enumerate() {
            if matches!(cmd, Cmd::Seek { .. }) && Some(i) != last_seek {
                continue;
            }
            run_cmd(mpv, cmd);
        }
    }
}

impl Player {
    pub fn new(egui_ctx: egui::Context) -> Result<Self, String> {
        let mpv = Mpv::with_initializer(|init| {
            init.set_property("vo", "libmpv")?;
            init.set_property("hwdec", "auto-safe")?;
            init.set_property("terminal", "no")?;
            init.set_property("msg-level", "all=no")?;
            init.set_property("input-default-bindings", "no")?;
            init.set_property("osc", "no")?;
            init.set_property("keep-open", "always")?;
            init.set_property("audio-file-auto", "no")?;
            init.set_property("sub-auto", "no")?;
            init.set_property("hr-seek", "yes")?;
            init.set_property("hr-seek-framedrop", "no")?;
            // Otherwise render() blocks until the frame's presentation time.
            init.set_property("video-timing-offset", "0")?;
            Ok(())
        })
        .map_err(|e| format!("mpv init: {e}"))?;

        let mpv: &'static Mpv = Box::leak(Box::new(mpv));
        let mut render = mpv
            .create_render_context(vec![
                RenderParam::ApiType(RenderParamApiType::OpenGl),
                RenderParam::InitParams(OpenGLInitParams {
                    get_proc_address,
                    ctx: GlLoader::new(),
                }),
            ])
            .map_err(|e| format!("mpv render context: {e}"))?;

        let frame_ready = Arc::new(AtomicBool::new(false));
        let flag = frame_ready.clone();
        render.set_update_callback(move || {
            flag.store(true, Ordering::Release);
            egui_ctx.request_repaint();
        });

        let (tx, rx) = channel();
        std::thread::spawn(move || command_worker(mpv, rx));

        let cache = Arc::new(Cache::default());
        cache.paused.store(true, Ordering::Relaxed);
        let poll = cache.clone();
        std::thread::spawn(move || {
            while !poll.shutdown.load(Ordering::Relaxed) {
                if let Ok(t) = mpv.get_property::<f64>("time-pos") {
                    poll.time_pos.store(t.to_bits(), Ordering::Relaxed);
                }
                if let Ok(p) = mpv.get_property::<bool>("pause") {
                    poll.paused.store(p, Ordering::Relaxed);
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
        });

        Ok(Self {
            render,
            frame_ready,
            cache,
            tx,
            tex: None,
            fbo: None,
            tex_id: None,
            tex_size: [0, 0],
            last_draw: [0, 0],
        })
    }

    fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }

    pub fn load(&mut self, path: &Path) {
        self.cache.time_pos.store(0f64.to_bits(), Ordering::Relaxed);
        self.cache.loaded.store(true, Ordering::Relaxed);
        self.send(Cmd::Load(path.to_string_lossy().into_owned()));
        self.set_paused(true);
    }

    pub fn is_loaded(&self) -> bool {
        self.cache.loaded.load(Ordering::Relaxed)
    }

    pub fn position(&self) -> f64 {
        f64::from_bits(self.cache.time_pos.load(Ordering::Relaxed))
    }

    pub fn paused(&self) -> bool {
        self.cache.paused.load(Ordering::Relaxed)
    }

    pub fn set_paused(&self, paused: bool) {
        self.cache.paused.store(paused, Ordering::Relaxed);
        self.send(Cmd::SetPause(paused));
    }

    pub fn set_volume(&self, volume: i64) {
        self.send(Cmd::SetVolume(volume));
    }

    pub fn set_speed(&self, speed: f64) {
        self.send(Cmd::SetSpeed(speed));
    }

    /// `exact` decodes to the frame; the fast path snaps to a keyframe and is used
    /// while a scrub drag is in flight.
    pub fn seek(&self, time: f64, exact: bool) {
        self.send(Cmd::Seek { time, exact });
    }

    pub fn frame_step(&self, forward: bool) {
        self.send(Cmd::FrameStep(forward));
    }

    fn ensure_target(&mut self, gl: &glow::Context, need: [i32; 2]) {
        let fits = self.tex.is_some()
            && self.tex_size[0] >= need[0]
            && self.tex_size[1] >= need[1]
            && self.tex_size[0] <= need[0] * 2
            && self.tex_size[1] <= need[1] * 2;
        if fits {
            return;
        }
        unsafe {
            if let Some(fbo) = self.fbo.take() {
                gl.delete_framebuffer(fbo);
            }
            if let Some(tex) = self.tex.take() {
                gl.delete_texture(tex);
            }

            let tex = gl.create_texture().expect("create texture");
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                need[0],
                need[1],
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );

            let prev_fbo = gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING);
            let fbo = gl.create_framebuffer().expect("create framebuffer");
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(tex),
                0,
            );
            bind_raw_framebuffer(gl, prev_fbo);
            gl.bind_texture(glow::TEXTURE_2D, None);

            self.tex = Some(tex);
            self.fbo = Some(fbo);
            self.tex_size = need;
            self.tex_id = None;
        }
    }

    /// Renders the current mpv frame and returns the texture plus the sub-rectangle
    /// of it that holds the image.
    pub fn draw(
        &mut self,
        gl: &glow::Context,
        frame: &mut eframe::Frame,
        size_px: [i32; 2],
    ) -> Option<(egui::TextureId, egui::Rect)> {
        if !self.is_loaded() || size_px[0] <= 0 || size_px[1] <= 0 {
            return None;
        }
        let need = [round_up(size_px[0], 64), round_up(size_px[1], 64)];
        self.ensure_target(gl, need);

        if self.tex_id.is_none() {
            self.tex_id = Some(frame.register_native_glow_texture(self.tex?));
        }

        let resized = self.last_draw != size_px;
        if self.frame_ready.swap(false, Ordering::AcqRel) || resized {
            self.last_draw = size_px;
            let fbo = self.fbo?;
            unsafe {
                let prev_fbo = gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING);
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
                // mpv draws into (0,0,w,h) of the framebuffer, so the image lands in the
                // bottom-left of an oversized texture; `flip=false` keeps row 0 at v=0.
                let _ = self.render.render::<GlLoader>(
                    fbo.0.get() as i32,
                    size_px[0],
                    size_px[1],
                    false,
                );
                bind_raw_framebuffer(gl, prev_fbo);
            }
        }

        let uv = egui::Rect::from_min_max(
            egui::pos2(0.0, 0.0),
            egui::pos2(
                size_px[0] as f32 / self.tex_size[0] as f32,
                size_px[1] as f32 / self.tex_size[1] as f32,
            ),
        );
        Some((self.tex_id?, uv))
    }

    pub fn destroy(&mut self, gl: &glow::Context) {
        self.cache.shutdown.store(true, Ordering::Relaxed);
        unsafe {
            if let Some(fbo) = self.fbo.take() {
                gl.delete_framebuffer(fbo);
            }
            if let Some(tex) = self.tex.take() {
                gl.delete_texture(tex);
            }
        }
    }
}

unsafe fn bind_raw_framebuffer(gl: &glow::Context, raw: i32) {
    let fbo = NonZeroU32::new(raw as u32).map(glow::NativeFramebuffer);
    unsafe { gl.bind_framebuffer(glow::FRAMEBUFFER, fbo) };
}
