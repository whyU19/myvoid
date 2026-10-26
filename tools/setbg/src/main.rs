//! setbg: background (wallpaper) for Wayland (wlr-layer-shell background surface on every output).
//!
//!   setbg next | prev | restore   cycle through ~/Pictures/Backgrounds
//!   setbg IMAGE                   set one image
//! The chosen image is remembered in ~/.cache/current_background. The image is
//! shown (scaled to cover) by a background process (`setbg --daemon IMAGE`)
//! that this command starts; a new one takes over from the previous one
//! (pid file in $XDG_RUNTIME_DIR) once its first frame is on screen.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use image::RgbaImage;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::EventLoop,
        calloop_wayland_source::WaylandSource,
        client::{
            globals::registry_queue_init,
            protocol::{wl_output, wl_shm, wl_surface},
            Connection, QueueHandle,
        },
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use wlkit::theme;


struct Wall {
    layer: LayerSurface,
    last: (u32, u32),
}

struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    compositor: CompositorState,
    layer_shell: LayerShell,
    img: RgbaImage,
    walls: Vec<Wall>,
    took_over: bool,
}

fn pid_file() -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(dir).join("setbg.pid")
}

impl App {
    fn take_over(&mut self) {
        if self.took_over {
            return;
        }
        self.took_over = true;
        let path = pid_file();
        let me = std::process::id();
        if let Ok(s) = std::fs::read_to_string(&path) {
            if let Ok(old) = s.trim().parse::<u32>() {
                if old != me {
                    // ask the previous instance to quit
                    unsafe {
                        libc::kill(old as i32, libc::SIGTERM);
                    }
                }
            }
        }
        let _ = std::fs::write(&path, me.to_string());
    }

    fn paint(&mut self, idx: usize, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        let scale = wlkit::fit_scale(self.img.width(), self.img.height(), w, h, true);
        let px = wlkit::render(&self.img, w, h, scale, 0, 0, theme::BASE);
        let (buffer, canvas) =
            match self.pool.create_buffer(w as i32, h as i32, w as i32 * 4, wl_shm::Format::Xrgb8888) {
                Ok(b) => b,
                Err(_) => return,
            };
        let n = canvas.len().min(px.len());
        canvas[..n].copy_from_slice(&px[..n]);
        let wall = &self.walls[idx];
        wall.layer.wl_surface().damage_buffer(0, 0, w as i32, h as i32);
        if buffer.attach_to(wall.layer.wl_surface()).is_ok() {
            wall.layer.commit();
        }
        self.walls[idx].last = (w, h);
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
    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Background,
            Some("background"),
            Some(&output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(0, 0);
        layer.commit();
        self.walls.push(Wall { layer, last: (0, 0) });
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        self.walls.retain(|w| w.layer.wl_surface() != layer.wl_surface());
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let (w, h) = configure.new_size;
        if let Some(idx) = self.walls.iter().position(|x| x.layer.wl_surface() == layer.wl_surface()) {
            if self.walls[idx].last != (w, h) {
                self.paint(idx, w, h);
            }
        }
        self.take_over();
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
delegate_layer!(App);
delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())
}

fn notify(args: &[&str]) {
    let _ = Command::new("notify-send")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Start the background process that shows `full` and remember it.
fn show(full: &str) -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let ok = Command::new(exe)
        .arg("--daemon")
        .arg(full)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .is_ok();
    if ok {
        let cache = format!("{}/.cache/current_background", home());
        if let Some(d) = std::path::Path::new(&cache).parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(&cache, format!("{}\n", full));
    }
    ok
}

fn cycle(direction: &str) -> i32 {
    let restore = direction == "restore";
    let dir = format!("{}/Pictures/Backgrounds", home());
    let fail = |msg: &str| -> i32 {
        if restore {
            0
        } else {
            notify(&["-u", "critical", "Background", msg]);
            1
        }
    };
    let Ok(rd) = std::fs::read_dir(&dir) else { return fail("Folder not found") };
    let mut list: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| {
            let l = n.to_lowercase();
            [".jpg", ".jpeg", ".png", ".webp"].iter().any(|x| l.ends_with(x))
        })
        .collect();
    list.sort();
    if list.is_empty() {
        return fail("No images");
    }
    let current = std::fs::read_to_string(format!("{}/.cache/current_background", home()))
        .ok()
        .and_then(|s| {
            std::path::Path::new(s.trim()).file_name().map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_default();

    let target = if restore {
        if list.contains(&current) { current } else { list[0].clone() }
    } else {
        let mut ordered = list.clone();
        if direction == "prev" {
            ordered.reverse();
        }
        match ordered.iter().position(|n| *n == current) {
            Some(i) if i + 1 < ordered.len() => ordered[i + 1].clone(),
            _ => ordered[0].clone(),
        }
    };
    let full = format!("{}/{}", dir, target);
    if !show(&full) {
        return 1;
    }
    if !restore {
        let stem = target.rsplit_once('.').map(|x| x.0).unwrap_or(&target);
        notify(&["-i", &full, "Background", stem]);
    }
    0
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(|s| s.as_str()).collect::<Vec<_>>().as_slice() {
        ["--daemon", path] => daemon(path),
        ["next"] | ["prev"] | ["restore"] => std::process::exit(cycle(&args[0])),
        [path] => {
            if wlkit::open_rgba(path).is_none() {
                eprintln!("setbg: cannot load image");
                std::process::exit(1);
            }
            let full = std::fs::canonicalize(path)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| path.to_string());
            std::process::exit(if show(&full) { 0 } else { 1 });
        }
        _ => {
            eprintln!("usage: setbg next | prev | restore | IMAGE");
            std::process::exit(1);
        }
    }
}

fn daemon(path: &str) {
    let img = match wlkit::open_rgba(path) {
        Some(i) => i,
        None => {
            eprintln!("setbg: cannot load image");
            std::process::exit(1);
        }
    };
    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(_) => {
            eprintln!("setbg: cannot connect to the Wayland display");
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
    let layer_shell = LayerShell::bind(&globals, &qh).expect("wlr-layer-shell missing");
    let shm = Shm::bind(&globals, &qh).expect("wl_shm missing");
    let pool = SlotPool::new(1 << 20, &shm).expect("shm pool");

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm,
        pool,
        compositor,
        layer_shell,
        img,
        walls: Vec::new(),
        took_over: false,
    };

    // SIGTERM from the next instance: default action ends the process, which
    // is all that is needed (the compositor drops our surfaces).
    loop {
        if event_loop.dispatch(None, &mut app).is_err() {
            break;
        }
    }
}
