//! capture: screenshots and screen recording for Wayland, no grim/slurp/wf-recorder.
//!
//!   capture shot [area]     screenshot of all outputs, or of a region you drag
//!                           (saved to ~/Pictures/Screenshots and copied)
//!   capture rec [area|OUT]  start recording (first output, a dragged region,
//!                           or the named output); run again to stop
//! Uses wlr-screencopy (frames), wlr-layer-shell (region picker) and ffmpeg
//! (H.264 encoding, fed raw frames through a pipe). Copying uses the `clipboard` tool.

use std::fs::File;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    reexports::{
        client::{
            globals::registry_queue_init,
            protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
            Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
        },
        protocols_wlr::screencopy::v1::client::{
            zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
            zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
        },
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::{Buffer, SlotPool}, Shm, ShmHandler},
};

const FPS: u64 = 30;
const BORDER: u32 = 0xff89_b4fa; // Catppuccin blue, 0xAARRGGBB

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_stop(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())
}

fn uid() -> u32 {
    unsafe { libc::getuid() }
}

fn stamp() -> String {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        let mut buf = [0u8; 64];
        let n = libc::strftime(
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            b"%Y-%m-%d_%H-%M-%S\0".as_ptr() as *const libc::c_char,
            &tm,
        );
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }
}

fn notify(args: &[&str]) {
    let _ = Command::new("notify-send")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn die(msg: &str) -> ! {
    eprintln!("capture: {}", msg);
    notify(&["-u", "critical", "Capture", msg]);
    std::process::exit(1);
}

/* ---- raw frames ---- */

struct Raw {
    w: u32,
    h: u32,
    stride: u32,
    format: wl_shm::Format,
    y_invert: bool,
    data: Vec<u8>,
}

impl Raw {
    fn rgb(&self, p: &[u8]) -> [u8; 3] {
        use wl_shm::Format as F;
        match self.format {
            F::Xrgb8888 | F::Argb8888 => [p[2], p[1], p[0]],
            F::Xbgr8888 | F::Abgr8888 => [p[0], p[1], p[2]],
            F::Xrgb2101010 | F::Argb2101010 => {
                let v = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
                [((v >> 22) & 0xff) as u8, ((v >> 12) & 0xff) as u8, ((v >> 2) & 0xff) as u8]
            }
            F::Xbgr2101010 | F::Abgr2101010 => {
                let v = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
                [((v >> 2) & 0xff) as u8, ((v >> 12) & 0xff) as u8, ((v >> 22) & 0xff) as u8]
            }
            _ => [0, 0, 0],
        }
    }

    fn supported(&self) -> bool {
        use wl_shm::Format as F;
        matches!(
            self.format,
            F::Xrgb8888 | F::Argb8888 | F::Xbgr8888 | F::Abgr8888 | F::Xrgb2101010 | F::Argb2101010 | F::Xbgr2101010 | F::Abgr2101010
        )
    }

    fn row(&self, y: u32) -> &[u8] {
        let sy = if self.y_invert { self.h - 1 - y } else { y } as usize;
        let start = sy * self.stride as usize;
        &self.data[start..start + self.w as usize * 4]
    }

    /// Opaque RGBA, w*h*4 bytes.
    fn to_rgba(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.w as usize * self.h as usize * 4);
        for y in 0..self.h {
            for p in self.row(y).chunks_exact(4) {
                let [r, g, b] = self.rgb(p);
                out.extend_from_slice(&[r, g, b, 255]);
            }
        }
        out
    }

    /// B,G,R,0 rows (ffmpeg "bgr0") for the top-left w x h area.
    fn to_bgr0(&self, w: usize, h: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(w * h * 4);
        let fast = matches!(self.format, wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888);
        for y in 0..h as u32 {
            let row = self.row(y);
            if fast {
                out.extend_from_slice(&row[..w * 4]);
            } else {
                for p in row[..w * 4].chunks_exact(4) {
                    let [r, g, b] = self.rgb(p);
                    out.extend_from_slice(&[b, g, r, 0]);
                }
            }
        }
        out
    }
}

#[derive(Default)]
struct Job {
    buffer: Option<Buffer>,
    w: u32,
    h: u32,
    stride: u32,
    format: Option<wl_shm::Format>,
    y_invert: bool,
    ready: bool,
    failed: bool,
}

struct Out {
    output: wl_output::WlOutput,
    name: String,
    lx: i32,
    ly: i32,
    scale: i32,
}

struct SelSurf {
    layer: LayerSurface,
    idx: usize,
    w: u32,
    h: u32,
    base: Vec<u8>,
    dim: Vec<u8>,
}

struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    compositor: CompositorState,
    layer_shell: LayerShell,
    screencopy: ZwlrScreencopyManagerV1,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,

    job: Job,

    // region picker
    shots: Vec<Raw>,
    surfs: Vec<SelSurf>,
    drag: Option<(usize, f64, f64)>,
    cur: (f64, f64),
    /// Some(None) = cancelled, Some(Some((surface, x, y, w, h))) = chosen
    picked: Option<Option<(usize, i32, i32, i32, i32)>>,
}

impl App {
    fn start_copy(&mut self, frame: &ZwlrScreencopyFrameV1) {
        if self.job.buffer.is_some() {
            return;
        }
        let Some(fmt) = self.job.format else {
            self.job.failed = true;
            return;
        };
        match self.pool.create_buffer(self.job.w as i32, self.job.h as i32, self.job.stride as i32, fmt) {
            Ok((buffer, _)) => {
                frame.copy(buffer.wl_buffer());
                self.job.buffer = Some(buffer);
            }
            Err(_) => self.job.failed = true,
        }
    }

    fn outs(&self) -> Vec<Out> {
        let mut v: Vec<Out> = self
            .output_state
            .outputs()
            .map(|o| {
                let info = self.output_state.info(&o);
                Out {
                    name: info.as_ref().and_then(|i| i.name.clone()).unwrap_or_default(),
                    lx: info.as_ref().and_then(|i| i.logical_position).map(|p| p.0).unwrap_or(0),
                    ly: info.as_ref().and_then(|i| i.logical_position).map(|p| p.1).unwrap_or(0),
                    scale: info.as_ref().map(|i| i.scale_factor).unwrap_or(1).max(1),
                    output: o,
                }
            })
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    fn draw_sel(&mut self, i: usize) {
        let (w, h) = (self.surfs[i].w as i32, self.surfs[i].h as i32);
        if w == 0 || h == 0 {
            return;
        }
        let sel = match self.drag {
            Some((si, sx, sy)) if si == i => {
                let x0 = sx.min(self.cur.0).max(0.0) as i32;
                let y0 = sy.min(self.cur.1).max(0.0) as i32;
                let x1 = (sx.max(self.cur.0) as i32).min(w);
                let y1 = (sy.max(self.cur.1) as i32).min(h);
                Some((x0, y0, (x1 - x0).max(0), (y1 - y0).max(0)))
            }
            _ => None,
        };
        let Ok((buffer, canvas)) = self.pool.create_buffer(w, h, w * 4, wl_shm::Format::Xrgb8888) else {
            return;
        };
        let s = &self.surfs[i];
        canvas.copy_from_slice(&s.dim);
        if let Some((x, y, rw, rh)) = sel {
            for row in y..y + rh {
                let a = ((row * w + x) * 4) as usize;
                let b = a + (rw * 4) as usize;
                canvas[a..b].copy_from_slice(&s.base[a..b]);
            }
            let px = BORDER.to_le_bytes();
            let mut put = |xx: i32, yy: i32| {
                if xx >= 0 && yy >= 0 && xx < w && yy < h {
                    let o = ((yy * w + xx) * 4) as usize;
                    canvas[o..o + 4].copy_from_slice(&px);
                }
            };
            for xx in x..x + rw {
                for t in 0..2 {
                    put(xx, y + t);
                    put(xx, y + rh - 1 - t);
                }
            }
            for yy in y..y + rh {
                for t in 0..2 {
                    put(x + t, yy);
                    put(x + rw - 1 - t, yy);
                }
            }
        }
        let wl = s.layer.wl_surface();
        wl.damage_buffer(0, 0, w, h);
        if buffer.attach_to(wl).is_ok() {
            s.layer.commit();
        }
    }

    fn surf_index(&self, wl: &wl_surface::WlSurface) -> Option<usize> {
        self.surfs.iter().position(|s| s.layer.wl_surface() == wl)
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for App {
    fn event(
        state: &mut Self,
        frame: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event;
        match event {
            Event::Buffer { format, width, height, stride } => {
                state.job.format = format.into_result().ok();
                state.job.w = width;
                state.job.h = height;
                state.job.stride = stride;
                if frame.version() < 3 {
                    state.start_copy(frame);
                }
            }
            Event::Flags { flags } => {
                if let WEnum::Value(f) = flags {
                    state.job.y_invert = f.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
                }
            }
            Event::BufferDone => state.start_copy(frame),
            Event::Ready { .. } => state.job.ready = true,
            Event::Failed => state.job.failed = true,
            _ => {}
        }
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ZwlrScreencopyManagerV1,
        _: <ZwlrScreencopyManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.picked = Some(None);
    }
    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface, configure: LayerSurfaceConfigure, _: u32) {
        let Some(i) = self.surf_index(layer.wl_surface()) else { return };
        let (w, h) = configure.new_size;
        if w == 0 || h == 0 {
            return;
        }
        // base = frozen screenshot at surface size, dim = darkened copy
        let shot = &self.shots[self.surfs[i].idx];
        let rgba = shot.to_rgba();
        let mut base = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            let sy = (y as u64 * shot.h as u64 / h as u64) as usize;
            for x in 0..w {
                let sx = (x as u64 * shot.w as u64 / w as u64) as usize;
                let s = (sy * shot.w as usize + sx) * 4;
                let d = ((y * w + x) * 4) as usize;
                base[d] = rgba[s + 2];
                base[d + 1] = rgba[s + 1];
                base[d + 2] = rgba[s];
                base[d + 3] = 255;
            }
        }
        let mut dim = base.clone();
        for px in dim.chunks_exact_mut(4) {
            px[0] /= 2;
            px[1] /= 2;
            px[2] /= 2;
        }
        let s = &mut self.surfs[i];
        s.w = w;
        s.h = h;
        s.base = base;
        s.dim = dim;
        self.draw_sel(i);
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, cap: Capability) {
        if cap == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seat_state.get_keyboard(qh, &seat, None).ok();
        }
        if cap == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, _: Capability) {}
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for App {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32, _: &[u32], _: &[Keysym]) {}
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32) {}
    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, event: KeyEvent) {
        if event.keysym.raw() == 0xff1b || event.keysym.raw() == 0x71 {
            self.picked = Some(None); // Escape / q
        }
    }
    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}
    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: Modifiers, _: u32) {}
}

impl PointerHandler for App {
    fn pointer_frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for e in events {
            let Some(i) = self.surf_index(&e.surface) else { continue };
            match &e.kind {
                PointerEventKind::Press { button, .. } => {
                    if *button == 0x110 {
                        self.drag = Some((i, e.position.0, e.position.1));
                        self.cur = e.position;
                    } else if *button == 0x111 {
                        self.picked = Some(None);
                    }
                }
                PointerEventKind::Motion { .. } => {
                    self.cur = e.position;
                    if let Some((si, _, _)) = self.drag {
                        if si == i {
                            self.draw_sel(i);
                        }
                    }
                }
                PointerEventKind::Release { button, .. } => {
                    if *button == 0x110 {
                        if let Some((si, sx, sy)) = self.drag.take() {
                            let (w, h) = (self.surfs[si].w as i32, self.surfs[si].h as i32);
                            let x0 = sx.min(e.position.0).max(0.0) as i32;
                            let y0 = sy.min(e.position.1).max(0.0) as i32;
                            let x1 = (sx.max(e.position.0) as i32).min(w);
                            let y1 = (sy.max(e.position.1) as i32).min(h);
                            self.picked = Some(if x1 - x0 >= 2 && y1 - y0 >= 2 {
                                Some((si, x0, y0, x1 - x0, y1 - y0))
                            } else {
                                None
                            });
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_compositor!(App);
delegate_output!(App);
delegate_shm!(App);
delegate_seat!(App);
delegate_keyboard!(App);
delegate_pointer!(App);
delegate_layer!(App);
delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

/* ---- capturing ---- */

fn connect() -> (EventQueue<App>, App) {
    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(_) => die("cannot connect to the Wayland display"),
    };
    let (globals, mut queue) = registry_queue_init::<App>(&conn).expect("registry init");
    let qh = queue.handle();
    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor missing");
    let layer_shell = LayerShell::bind(&globals, &qh).expect("wlr-layer-shell missing");
    let shm = Shm::bind(&globals, &qh).expect("wl_shm missing");
    let screencopy: ZwlrScreencopyManagerV1 = match globals.bind(&qh, 1..=3, ()) {
        Ok(s) => s,
        Err(_) => die("compositor does not support wlr-screencopy"),
    };
    let pool = SlotPool::new(4 << 20, &shm).expect("shm pool");
    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        pool,
        compositor,
        layer_shell,
        screencopy,
        keyboard: None,
        pointer: None,
        job: Job::default(),
        shots: Vec::new(),
        surfs: Vec::new(),
        drag: None,
        cur: (0.0, 0.0),
        picked: None,
    };
    // outputs, their info and the seat capabilities arrive over a few round trips
    for _ in 0..3 {
        let _ = queue.roundtrip(&mut app);
    }
    (queue, app)
}

fn grab(
    app: &mut App,
    queue: &mut EventQueue<App>,
    output: &wl_output::WlOutput,
    region: Option<(i32, i32, i32, i32)>,
    cursor: bool,
) -> Option<Raw> {
    app.job = Job::default();
    let qh = queue.handle();
    let frame = match region {
        None => app.screencopy.capture_output(cursor as i32, output, &qh, ()),
        Some((x, y, w, h)) => app.screencopy.capture_output_region(cursor as i32, output, x, y, w, h, &qh, ()),
    };
    while !app.job.ready && !app.job.failed {
        queue.blocking_dispatch(app).ok()?;
    }
    frame.destroy();
    if app.job.failed {
        return None;
    }
    let buffer = app.job.buffer.take()?;
    let (w, h, stride) = (app.job.w, app.job.h, app.job.stride);
    let format = app.job.format?;
    let y_invert = app.job.y_invert;
    let canvas = app.pool.canvas(&buffer)?;
    let need = (stride * h) as usize;
    if canvas.len() < need {
        return None;
    }
    let raw = Raw { w, h, stride, format, y_invert, data: canvas[..need].to_vec() };
    if raw.supported() {
        Some(raw)
    } else {
        None
    }
}

fn save_png(rgba: Vec<u8>, w: u32, h: u32) -> Option<String> {
    let dir = format!("{}/Pictures/Screenshots", home());
    std::fs::create_dir_all(&dir).ok()?;
    let path = format!("{}/screenshot_{}.png", dir, stamp());
    image::RgbaImage::from_raw(w, h, rgba)?.save(&path).ok()?;
    Some(path)
}

fn copy_png(path: &str) {
    if let Ok(f) = File::open(path) {
        let _ = Command::new(format!("{}/.config/clipboard/clipboard", home()))
            .args(["-t", "image/png"])
            .stdin(Stdio::from(f))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Freeze every output, let the user drag a rectangle.
/// Returns (output index, x, y, w, h) in that output's logical coordinates.
fn pick_area(queue: &mut EventQueue<App>, app: &mut App, outs: &[Out]) -> Option<(usize, i32, i32, i32, i32)> {
    let qh = queue.handle();
    for o in outs {
        let shot = grab(app, queue, &o.output, None, false)?;
        app.shots.push(shot);
    }
    for (idx, o) in outs.iter().enumerate() {
        let surface = app.compositor.create_surface(&qh);
        let layer = app.layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("capture"), Some(&o.output));
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.set_size(0, 0);
        layer.commit();
        app.surfs.push(SelSurf { layer, idx, w: 0, h: 0, base: Vec::new(), dim: Vec::new() });
    }
    while app.picked.is_none() {
        queue.blocking_dispatch(app).ok()?;
    }
    let result = app.picked.take().flatten();
    result.map(|(si, x, y, w, h)| (app.surfs[si].idx, x, y, w, h))
}

fn shot(area: bool) {
    let (mut queue, mut app) = connect();
    let outs = app.outs();
    if outs.is_empty() {
        die("no outputs");
    }
    let (rgba, w, h);
    if area {
        let Some((oi, x, y, rw, rh)) = pick_area(&mut queue, &mut app, &outs) else { return };
        // surface size == logical output size; scale the region to frame pixels
        let shot = &app.shots[oi];
        let surf_w = app.surfs.iter().find(|s| s.idx == oi).map(|s| s.w).unwrap_or(shot.w).max(1);
        let surf_h = app.surfs.iter().find(|s| s.idx == oi).map(|s| s.h).unwrap_or(shot.h).max(1);
        let fx = shot.w as f64 / surf_w as f64;
        let fy = shot.h as f64 / surf_h as f64;
        let (cx, cy) = ((x as f64 * fx) as usize, (y as f64 * fy) as usize);
        let cw = (((rw as f64 * fx) as usize).max(1)).min(shot.w as usize - cx.min(shot.w as usize - 1));
        let ch = (((rh as f64 * fy) as usize).max(1)).min(shot.h as usize - cy.min(shot.h as usize - 1));
        let full = shot.to_rgba();
        let mut out = Vec::with_capacity(cw * ch * 4);
        for yy in cy..cy + ch {
            let a = (yy * shot.w as usize + cx) * 4;
            out.extend_from_slice(&full[a..a + cw * 4]);
        }
        rgba = out;
        w = cw as u32;
        h = ch as u32;
        app.surfs.clear(); // close the overlay
        let _ = queue.roundtrip(&mut app);
    } else {
        let mut shots = Vec::new();
        for o in &outs {
            match grab(&mut app, &mut queue, &o.output, None, false) {
                Some(r) => shots.push((o.lx * o.scale, o.ly * o.scale, r)),
                None => die("screen capture failed"),
            }
        }
        let min_x = shots.iter().map(|s| s.0).min().unwrap_or(0);
        let min_y = shots.iter().map(|s| s.1).min().unwrap_or(0);
        let max_x = shots.iter().map(|s| s.0 + s.2.w as i32).max().unwrap_or(0);
        let max_y = shots.iter().map(|s| s.1 + s.2.h as i32).max().unwrap_or(0);
        let (tw, th) = ((max_x - min_x) as usize, (max_y - min_y) as usize);
        let mut canvas = vec![0u8; tw * th * 4];
        for (sx, sy, r) in &shots {
            let src = r.to_rgba();
            for y in 0..r.h as usize {
                let d = (((*sy - min_y) as usize + y) * tw + (*sx - min_x) as usize) * 4;
                let s = y * r.w as usize * 4;
                canvas[d..d + r.w as usize * 4].copy_from_slice(&src[s..s + r.w as usize * 4]);
            }
        }
        rgba = canvas;
        w = tw as u32;
        h = th as u32;
    }
    match save_png(rgba, w, h) {
        Some(path) => {
            copy_png(&path);
            notify(&["-i", &path, "Screenshot", "Saved and copied"]);
        }
        None => die("cannot save screenshot"),
    }
}

/* ---- recording ---- */

fn pid_file() -> String {
    format!("/tmp/capture-rec-{}.pid", uid())
}

/// Background recorder: OUT FILE [x y w h]
fn rec_run(out_name: &str, file: &str, region: Option<(i32, i32, i32, i32)>) {
    unsafe {
        libc::signal(libc::SIGINT, on_stop as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_stop as libc::sighandler_t);
    }
    let (mut queue, mut app) = connect();
    let outs = app.outs();
    let Some(out) = outs.iter().find(|o| o.name == out_name) else { std::process::exit(1) };
    let output = out.output.clone();

    let Some(first) = grab(&mut app, &mut queue, &output, region, true) else { std::process::exit(1) };
    let (w, h) = ((first.w & !1) as usize, (first.h & !1) as usize);
    if w == 0 || h == 0 {
        std::process::exit(1);
    }
    let size = format!("{}x{}", w, h);
    let rate = FPS.to_string();
    let mut ffmpeg = match Command::new("ffmpeg")
        .args([
            "-v", "quiet", "-y", "-f", "rawvideo", "-pix_fmt", "bgr0", "-s", &size, "-r", &rate, "-i", "-",
            "-c:v", "libx264", "-preset", "ultrafast", "-crf", "23", "-pix_fmt", "yuv420p", file,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => std::process::exit(1),
    };
    let mut pipe = ffmpeg.stdin.take().expect("ffmpeg stdin");
    let _ = pipe.write_all(&first.to_bgr0(w, h));

    let tick = Duration::from_millis(1000 / FPS);
    let mut failures = 0;
    while !STOP.load(Ordering::SeqCst) {
        let t0 = Instant::now();
        match grab(&mut app, &mut queue, &output, region, true) {
            Some(r) if (r.w & !1) as usize == w && (r.h & !1) as usize == h => {
                failures = 0;
                if pipe.write_all(&r.to_bgr0(w, h)).is_err() {
                    break;
                }
            }
            _ => {
                failures += 1;
                if failures > 30 {
                    break;
                }
            }
        }
        let spent = t0.elapsed();
        if spent < tick {
            std::thread::sleep(tick - spent);
        }
    }
    drop(pipe);
    let _ = ffmpeg.wait();
    let _ = std::fs::remove_file(pid_file());
}

fn rec(arg: Option<&str>) {
    // second call stops a running recording
    if let Ok(text) = std::fs::read_to_string(pid_file()) {
        let mut lines = text.lines();
        if let Some(pid) = lines.next().and_then(|l| l.parse::<i32>().ok()) {
            if unsafe { libc::kill(pid, 0) } == 0 {
                unsafe { libc::kill(pid, libc::SIGINT) };
                let _ = std::fs::remove_file(pid_file());
                notify(&["Recording", "Saved"]);
                return;
            }
        }
        let _ = std::fs::remove_file(pid_file());
    }

    let (mut queue, mut app) = connect();
    let outs = app.outs();
    if outs.is_empty() {
        die("no outputs");
    }
    let (name, region) = match arg {
        Some("area") => {
            let Some((oi, x, y, w, h)) = pick_area(&mut queue, &mut app, &outs) else { return };
            app.surfs.clear();
            let _ = queue.roundtrip(&mut app);
            std::thread::sleep(Duration::from_millis(150)); // overlay must be gone before the first frame
            (outs[oi].name.clone(), Some((x, y, w, h)))
        }
        Some(n) => match outs.iter().find(|o| o.name == n) {
            Some(o) => (o.name.clone(), None),
            None => die("unknown output"),
        },
        None => (outs[0].name.clone(), None),
    };
    drop(queue);
    drop(app);

    let dir = format!("{}/Videos/Recordings", home());
    let _ = std::fs::create_dir_all(&dir);
    let file = format!("{}/recording_{}.mp4", dir, stamp());
    let exe = std::env::current_exe().unwrap_or_else(|_| "capture".into());
    let mut cmd = Command::new(exe);
    cmd.arg("--rec-run").arg(&name).arg(&file);
    if let Some((x, y, w, h)) = region {
        cmd.args([x.to_string(), y.to_string(), w.to_string(), h.to_string()]);
    }
    match cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0).spawn() {
        Ok(child) => {
            let _ = std::fs::write(pid_file(), format!("{}\n{}\n", child.id(), file));
            notify(&["Recording", "Started"]);
        }
        Err(_) => die("cannot start the recorder"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    match a.as_slice() {
        ["shot"] => shot(false),
        ["shot", "area"] => shot(true),
        ["rec"] => rec(None),
        ["rec", x] => rec(Some(x)),
        ["--rec-run", out, file] => rec_run(out, file, None),
        ["--rec-run", out, file, x, y, w, h] => {
            let n = |s: &str| s.parse::<i32>().unwrap_or(0);
            rec_run(out, file, Some((n(x), n(y), n(w), n(h))));
        }
        _ => {
            eprintln!("usage: capture shot [area] | capture rec [area|OUTPUT]");
            std::process::exit(1);
        }
    }
}
