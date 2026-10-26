//! view: small image viewer for Wayland (xdg-toplevel).
//!
//! usage: view FILE...      (one FILE also browses the other images in its directory)
//! keys: q/Esc quit | space,n,Right next | p,BackSpace,Left prev
//!       +/- or Up/Down zoom | 0 fit | 1 actual size | h j k l pan

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

use image::RgbaImage;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_output, delegate_registry, delegate_seat,
    delegate_shm, delegate_xdg_shell, delegate_xdg_window,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::EventLoop,
        calloop_wayland_source::WaylandSource,
        client::{
            globals::registry_queue_init,
            protocol::{wl_keyboard, wl_output, wl_seat, wl_shm, wl_surface},
            Connection, QueueHandle,
        },
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        xdg::{
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
            XdgShell,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use wlkit::theme;

const EXTS: [&str; 7] = ["png", "jpg", "jpeg", "webp", "gif", "bmp", "jfif"];

fn is_image(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| EXTS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    window: Window,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    files: Vec<PathBuf>,
    idx: usize,
    img: Option<RgbaImage>,
    zoom: Option<f64>,
    pan: (i32, i32),
    w: u32,
    h: u32,
    first: bool,
    exit: bool,
}

impl App {
    fn load(&mut self) {
        self.img = None;
        // skip unreadable files in the direction of travel
        let n = self.files.len();
        for _ in 0..n {
            if let Some(i) = wlkit::open_rgba(&self.files[self.idx].to_string_lossy()) {
                self.img = Some(i);
                break;
            }
            self.idx = (self.idx + 1) % n;
        }
        self.zoom = None;
        self.pan = (0, 0);
    }

    fn fit(&self) -> f64 {
        match &self.img {
            Some(i) => wlkit::fit_scale(i.width(), i.height(), self.w, self.h, false),
            None => 1.0,
        }
    }

    fn step(&mut self, d: i64) {
        let n = self.files.len() as i64;
        self.idx = ((self.idx as i64 + d).rem_euclid(n)) as usize;
        self.load();
        self.draw();
    }

    fn zoom_by(&mut self, f: f64) {
        let cur = self.zoom.unwrap_or_else(|| self.fit());
        self.zoom = Some((cur * f).clamp(0.02, 64.0));
        self.draw();
    }

    fn handle_key(&mut self, ev: &KeyEvent, shift: bool) {
        let _ = shift;
        let step = 60;
        match ev.keysym.raw() {
            0x71 | 0xff1b => self.exit = true,
            0x20 | 0x6e | 0xff53 => self.step(1),
            0x70 | 0xff08 | 0xff51 => self.step(-1),
            0x2b | 0x3d | 0xff52 => self.zoom_by(1.25),
            0x2d | 0xff54 => self.zoom_by(0.8),
            0x30 => {
                self.zoom = None;
                self.pan = (0, 0);
                self.draw();
            }
            0x31 => {
                self.zoom = Some(1.0);
                self.pan = (0, 0);
                self.draw();
            }
            0x68 => {
                self.pan.0 += step;
                self.draw();
            }
            0x6c => {
                self.pan.0 -= step;
                self.draw();
            }
            0x6b => {
                self.pan.1 += step;
                self.draw();
            }
            0x6a => {
                self.pan.1 -= step;
                self.draw();
            }
            _ => {}
        }
    }

    fn draw(&mut self) {
        if self.w == 0 || self.h == 0 {
            return;
        }
        let (w, h) = (self.w as i32, self.h as i32);
        let scale = self.zoom.unwrap_or_else(|| self.fit());
        let px = match &self.img {
            Some(i) => wlkit::render(i, self.w, self.h, scale, self.pan.0, self.pan.1, theme::CRUST),
            None => wlkit::solid(self.w, self.h, theme::CRUST),
        };
        let (buffer, canvas) = match self.pool.create_buffer(w, h, w * 4, wl_shm::Format::Xrgb8888) {
            Ok(b) => b,
            Err(_) => return,
        };
        let n = canvas.len().min(px.len());
        canvas[..n].copy_from_slice(&px[..n]);
        let name = self.files[self.idx].file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        self.window.set_title(format!(
            "{} ({}/{}) {:.0}%",
            name,
            self.idx + 1,
            self.files.len(),
            scale * 100.0
        ));
        self.window.wl_surface().damage_buffer(0, 0, w, h);
        if buffer.attach_to(self.window.wl_surface()).is_ok() {
            self.window.commit();
        }
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

impl WindowHandler for App {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.exit = true;
    }
    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window, configure: WindowConfigure, _: u32) {
        let nz = |v: Option<NonZeroU32>, d: u32| v.map(|x| x.get()).unwrap_or(d);
        self.w = nz(configure.new_size.0, if self.w > 0 { self.w } else { 1024 });
        self.h = nz(configure.new_size.1, if self.h > 0 { self.h } else { 768 });
        if self.first {
            self.first = false;
            self.load();
        }
        self.draw();
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
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, cap: Capability) {
        if cap == Capability::Keyboard {
            if let Some(k) = self.keyboard.take() {
                k.release();
            }
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for App {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32, _: &[u32], _: &[Keysym]) {}
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32) {}
    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, event: KeyEvent) {
        self.handle_key(&event, false);
    }
    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}
    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: Modifiers, _: u32) {}
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
delegate_xdg_shell!(App);
delegate_xdg_window!(App);
delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

fn collect(args: &[String]) -> (Vec<PathBuf>, usize) {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut start = 0;
    if args.len() == 1 {
        let p = PathBuf::from(&args[0]);
        if p.is_dir() {
            files = list_dir(&p);
        } else if let Some(dir) = p.parent().map(|d| if d.as_os_str().is_empty() { Path::new(".") } else { d }) {
            files = list_dir(dir);
            let canon = std::fs::canonicalize(&p).ok();
            start = files
                .iter()
                .position(|f| std::fs::canonicalize(f).ok() == canon)
                .unwrap_or_else(|| {
                    files.insert(0, p.clone());
                    0
                });
        }
    } else {
        for a in args {
            let p = PathBuf::from(a);
            if p.is_dir() {
                files.extend(list_dir(&p));
            } else {
                files.push(p);
            }
        }
    }
    (files, start)
}

fn list_dir(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_file() && is_image(p)).collect())
        .unwrap_or_default();
    v.sort();
    v
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: view FILE...");
        std::process::exit(1);
    }
    let (files, start) = collect(&args);
    if files.is_empty() {
        eprintln!("view: no images");
        std::process::exit(1);
    }

    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(_) => {
            eprintln!("view: cannot connect to the Wayland display");
            std::process::exit(1);
        }
    };
    let (globals, event_queue) = registry_queue_init(&conn).expect("registry init");
    let qh = event_queue.handle();
    let mut event_loop: EventLoop<'static, App> = EventLoop::try_new().expect("event loop");
    WaylandSource::new(conn.clone(), event_queue)
        .insert(event_loop.handle())
        .expect("wayland source");

    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor missing");
    let xdg_shell = XdgShell::bind(&globals, &qh).expect("xdg-shell missing");
    let shm = Shm::bind(&globals, &qh).expect("wl_shm missing");
    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::ServerDefault, &qh);
    window.set_title("view");
    window.set_app_id("view");
    window.set_min_size(Some((200, 150)));
    window.commit();
    let pool = SlotPool::new(1 << 20, &shm).expect("shm pool");

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        pool,
        window,
        keyboard: None,
        files,
        idx: start,
        img: None,
        zoom: None,
        pan: (0, 0),
        w: 0,
        h: 0,
        first: true,
        exit: false,
    };
    while !app.exit {
        if event_loop.dispatch(None, &mut app).is_err() {
            break;
        }
    }
}
