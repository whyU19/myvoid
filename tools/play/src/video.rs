use std::io::Read;
use std::num::NonZeroU32;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_output, delegate_registry, delegate_seat,
    delegate_shm, delegate_xdg_shell, delegate_xdg_window,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            channel::{self, Event as ChanEvent},
            timer::{TimeoutAction, Timer},
            EventLoop,
        },
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

use crate::common::{fmt_time, kill_child, Playlist};

enum Msg {
    Frame(u64, Vec<u8>),
    Ended(u64),
}

struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    window: Window,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pl: Playlist,
    tx: channel::Sender<Msg>,
    child: Option<Child>,
    gen: u64,
    base: f64,
    started: Option<Instant>,
    paused: bool,
    w: u32,
    h: u32,
    vw: u32,
    vh: u32,
    frame: Option<Vec<u8>>,
    configured: bool,
    exit: bool,
    code: i32,
    fails: usize,
}

impl App {
    fn pos(&self) -> f64 {
        self.base + self.started.map(|s| s.elapsed().as_secs_f64()).unwrap_or(0.0)
    }

    fn update_title(&self) {
        let f = self.pl.cur_file();
        let name = f.rsplit('/').next().unwrap_or(f);
        let n = self.pl.files.len();
        let idx = if n > 1 { format!(" ({}/{})", self.pl.cur + 1, n) } else { String::new() };
        self.window.set_title(format!("{}{}  {}{}", name, idx, fmt_time(self.pos()), if self.paused { "  [paused]" } else { "" }));
    }

    fn start_video(&mut self, pos: f64) {
        kill_child(&mut self.child);
        self.gen += 1;
        self.frame = None;
        self.vw = self.w & !1;
        self.vh = self.h & !1;
        if self.vw < 16 || self.vh < 16 {
            return;
        }
        let pos = pos.max(0.0);
        let ss = format!("{:.3}", pos);
        let vf = format!(
            "scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black",
            w = self.vw,
            h = self.vh
        );
        let spawned = Command::new("ffmpeg")
            .args([
                "-v", "quiet", "-nostdin", "-ss", &ss, "-re", "-i", self.pl.cur_file(), "-map", "0:v:0", "-vf", &vf,
                "-f", "rawvideo", "-pix_fmt", "bgr0", "pipe:1", "-map", "0:a:0?", "-f", "pulse", "default",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(_) => {
                eprintln!("play: cannot run ffmpeg");
                self.exit = true;
                self.code = 1;
                return;
            }
        };
        let mut out = child.stdout.take().expect("piped stdout");
        let gen = self.gen;
        let tx = self.tx.clone();
        let size = self.vw as usize * self.vh as usize * 4;
        std::thread::spawn(move || loop {
            let mut buf = vec![0u8; size];
            if out.read_exact(&mut buf).is_err() {
                let _ = tx.send(Msg::Ended(gen));
                break;
            }
            if tx.send(Msg::Frame(gen, buf)).is_err() {
                break;
            }
        });
        self.child = Some(child);
        self.base = pos;
        self.started = Some(Instant::now());
        self.paused = false;
        self.update_title();
    }

    fn track(&mut self, step: i64) {
        if self.pl.advance(step) {
            self.start_video(0.0);
        } else {
            self.exit = true;
        }
    }

    fn seek(&mut self, d: f64) {
        let p = self.pos() + d;
        self.start_video(p);
    }

    fn toggle_pause(&mut self) {
        if self.paused {
            let b = self.base;
            self.start_video(b);
        } else {
            self.base = self.pos();
            self.started = None;
            self.gen += 1;
            kill_child(&mut self.child);
            self.paused = true;
            self.update_title();
        }
    }

    fn on_msg(&mut self, m: Msg) {
        match m {
            Msg::Frame(g, data) if g == self.gen => {
                self.frame = Some(data);
                self.draw();
            }
            Msg::Ended(g) if g == self.gen => {
                let el = self.started.map(|s| s.elapsed().as_secs_f64()).unwrap_or(0.0);
                kill_child(&mut self.child);
                if el < 1.5 && self.base < 0.1 {
                    self.fails += 1;
                    if self.fails >= self.pl.files.len() {
                        eprintln!("play: cannot play (ffmpeg/audio output failed)");
                        self.code = 1;
                        self.exit = true;
                        return;
                    }
                } else {
                    self.fails = 0;
                }
                self.track(1);
            }
            _ => {}
        }
    }

    fn handle_key(&mut self, ev: &KeyEvent) {
        match ev.keysym.raw() {
            0x71 | 0xff1b => self.exit = true,
            0x20 => self.toggle_pause(),
            0xff53 => self.seek(10.0),
            0xff51 => self.seek(-10.0),
            0xff52 => self.seek(60.0),
            0xff54 => self.seek(-60.0),
            0x6e => self.track(1),
            0x70 => self.track(-1),
            _ => {}
        }
    }

    fn draw(&mut self) {
        if !self.configured || self.w == 0 || self.h == 0 {
            return;
        }
        let (w, h) = (self.w as i32, self.h as i32);
        let (buffer, canvas) = match self.pool.create_buffer(w, h, w * 4, wl_shm::Format::Xrgb8888) {
            Ok(b) => b,
            Err(_) => return,
        };
        match &self.frame {
            Some(f) if self.vw == (self.w & !1) && self.vh == (self.h & !1) => {
                // frame is vw x vh; copy row by row into the w x h buffer
                let (vw, vh) = (self.vw as usize, self.vh as usize);
                let stride = self.w as usize * 4;
                for b in canvas.iter_mut() {
                    *b = 0;
                }
                for y in 0..vh.min(self.h as usize) {
                    let src = &f[y * vw * 4..(y + 1) * vw * 4];
                    canvas[y * stride..y * stride + vw * 4].copy_from_slice(src);
                }
            }
            _ => {
                for b in canvas.iter_mut() {
                    *b = 0;
                }
            }
        }
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
        let nw = nz(configure.new_size.0, if self.w > 0 { self.w } else { 1280 });
        let nh = nz(configure.new_size.1, if self.h > 0 { self.h } else { 720 });
        let changed = nw != self.w || nh != self.h;
        self.w = nw;
        self.h = nh;
        let first = !self.configured;
        self.configured = true;
        if first {
            self.start_video(0.0);
        } else if changed && !self.paused {
            let p = self.pos();
            self.start_video(p);
        } else if changed {
            let b = self.base;
            self.vw = 0;
            self.base = b;
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
        self.handle_key(&event);
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

pub fn run(pl: Playlist) -> i32 {
    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(_) => {
            eprintln!("play: cannot connect to the Wayland display");
            return 1;
        }
    };
    let (globals, event_queue) = registry_queue_init(&conn).expect("registry init");
    let qh = event_queue.handle();
    let mut event_loop: EventLoop<'static, App> = EventLoop::try_new().expect("event loop");
    let handle = event_loop.handle();
    WaylandSource::new(conn.clone(), event_queue).insert(handle.clone()).expect("wayland source");

    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor missing");
    let xdg_shell = XdgShell::bind(&globals, &qh).expect("xdg-shell missing");
    let shm = Shm::bind(&globals, &qh).expect("wl_shm missing");
    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::ServerDefault, &qh);
    window.set_title("play");
    window.set_app_id("play");
    window.set_min_size(Some((160, 90)));
    window.commit();
    let pool = SlotPool::new(4 << 20, &shm).expect("shm pool");

    let (tx, rx) = channel::channel::<Msg>();
    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        pool,
        window,
        keyboard: None,
        pl,
        tx,
        child: None,
        gen: 0,
        base: 0.0,
        started: None,
        paused: false,
        w: 0,
        h: 0,
        vw: 0,
        vh: 0,
        frame: None,
        configured: false,
        exit: false,
        code: 0,
        fails: 0,
    };
    handle
        .insert_source(rx, |ev, _, app: &mut App| {
            if let ChanEvent::Msg(m) = ev {
                app.on_msg(m);
            }
        })
        .expect("frame channel");
    handle
        .insert_source(Timer::from_duration(Duration::from_secs(1)), |_, _, app: &mut App| {
            if !app.paused {
                app.update_title();
            }
            TimeoutAction::ToDuration(Duration::from_secs(1))
        })
        .expect("timer");

    while !app.exit {
        if event_loop.dispatch(None, &mut app).is_err() {
            break;
        }
    }
    kill_child(&mut app.child);
    app.code
}
