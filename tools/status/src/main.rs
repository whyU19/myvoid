//! status: top bar for dwl and notification daemon.
//!
//! * Bar: wlr-layer-shell surface on every output. Tag/title/layout state is
//!   read from stdin, which dwl feeds when started as `dwl -s <session>`
//!   (the session script runs this program in the foreground).
//! * Notifications: org.freedesktop.Notifications on the session bus, plus the
//!   /tmp/bar-msg-UID file written by the `notify-send` script.

mod notif;

use std::collections::HashMap;
use std::io::BufRead;
use std::os::unix::fs::MetadataExt;
use std::time::Duration;

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_seat,
    delegate_shm,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            channel::{self, Channel, Event as ChanEvent},
            timer::{TimeoutAction, Timer},
            EventLoop,
        },
        calloop_wayland_source::WaylandSource,
        client::{
            globals::registry_queue_init,
            protocol::{wl_output, wl_seat, wl_shm, wl_surface},
            Connection, QueueHandle,
        },
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{Capability, SeatHandler, SeatState},
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use wlkit::{theme, Canvas, Font, FONT_PT};

const TAGS: [&str; 9] = ["1", "2", "3", "4", "5", "6", "7", "8", "9"];

#[derive(Default, Clone)]
struct OutState {
    title: String,
    layout: String,
    occ: u32,
    tagset: u32,
    sel: u32,
    urg: u32,
    selmon: bool,
}

struct Bar {
    layer: LayerSurface,
    output: wl_output::WlOutput,
    w: u32,
    h: u32,
    configured: bool,
}

pub struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    compositor: CompositorState,
    layer_shell: LayerShell,
    font: Font,
    bar_h: u32,
    bars: Vec<Bar>,
    dwl: HashMap<String, OutState>,
    dirty: bool,
    quit: bool,

    // status text
    status: String,
    // messages
    dmsg: Option<(String, i64)>,
    fmsg: Option<(String, i64)>,
    fmsg_mtime: i64,
    icon: Option<Vec<u8>>,
    icon_exp: i64,
}

fn now_secs() -> i64 {
    chrono::Local::now().timestamp()
}

fn read_line(path: &str) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let l = s.lines().next()?.trim().to_string();
    if l.is_empty() {
        None
    } else {
        Some(l)
    }
}

fn status_text() -> String {
    let time = chrono::Local::now().format("%a, %b %e   %I:%M %p").to_string();
    for bat in ["BAT0", "BAT1"] {
        let base = format!("/sys/class/power_supply/{}", bat);
        if let Some(cap) = read_line(&format!("{}/capacity", base)) {
            let raw = read_line(&format!("{}/status", base)).unwrap_or_default();
            let st = match raw.as_str() {
                "Charging" => "Chg".to_string(),
                "Full" => "Full".to_string(),
                "Discharging" => "Dis".to_string(),
                "Not charging" => "Idle".to_string(),
                "" => "n/a".to_string(),
                other => other.to_string(),
            };
            return format!("BAT {}% ({})   {}", cap, st, time);
        }
    }
    time
}

impl App {
    fn msg_path() -> String {
        let uid = std::fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(0);
        format!("/tmp/bar-msg-{}", uid)
    }

    fn icon_tile(&self, src: &notif::IconSrc) -> Option<Vec<u8>> {
        let size = self.bar_h;
        match src {
            notif::IconSrc::Path(p) => {
                let p = p.strip_prefix("file://").unwrap_or(p);
                if !p.starts_with('/') {
                    return None;
                }
                wlkit::load_fit(p, size, size, false, theme::BASE)
            }
            notif::IconSrc::Raw { data, w, h, stride, ch } => {
                Some(wlkit::raw_to_tile(data, *w, *h, *stride, *ch, size, theme::BASE))
            }
        }
    }

    /// Poll the message file written by the notify-send script.
    fn poll_file_msg(&mut self) {
        let path = Self::msg_path();
        let mtime = std::fs::metadata(&path).map(|m| m.mtime() * 1000 + m.mtime_nsec() / 1_000_000).unwrap_or(0);
        if mtime == self.fmsg_mtime {
            return;
        }
        self.fmsg_mtime = mtime;
        self.dirty = true;
        let Ok(s) = std::fs::read_to_string(&path) else {
            self.fmsg = None;
            return;
        };
        let mut lines = s.lines();
        let exp: i64 = lines.next().and_then(|l| l.trim().parse().ok()).unwrap_or(0);
        let msg = lines.next().unwrap_or("").to_string();
        let icon = lines.next().unwrap_or("").to_string();
        if msg.is_empty() || exp <= now_secs() {
            self.fmsg = None;
            return;
        }
        self.fmsg = Some((msg, exp));
        self.icon = None;
        if !icon.is_empty() {
            self.icon = self.icon_tile(&notif::IconSrc::Path(icon));
            self.icon_exp = exp;
        }
    }

    fn on_notification(&mut self, n: notif::Notif) {
        let exp = now_secs() + n.secs;
        self.dmsg = Some((n.text, exp));
        self.icon = n.icon.as_ref().and_then(|i| self.icon_tile(i));
        self.icon_exp = exp;
        self.dirty = true;
    }

    fn tick(&mut self) {
        let now = now_secs();
        let st = status_text();
        if st != self.status {
            self.status = st;
            self.dirty = true;
        }
        if let Some((_, e)) = &self.dmsg {
            if *e <= now {
                self.dmsg = None;
                self.dirty = true;
            }
        }
        if let Some((_, e)) = &self.fmsg {
            if *e <= now {
                self.fmsg = None;
                self.dirty = true;
            }
        }
        if self.icon.is_some() && self.icon_exp <= now {
            self.icon = None;
            self.dirty = true;
        }
    }

    fn right_text(&self) -> String {
        let now = now_secs();
        let d = self.dmsg.as_ref().filter(|m| m.1 > now);
        let f = self.fmsg.as_ref().filter(|m| m.1 > now);
        match (d, f) {
            (Some(d), Some(f)) => if d.1 >= f.1 { d.0.clone() } else { f.0.clone() },
            (Some(d), None) => d.0.clone(),
            (None, Some(f)) => f.0.clone(),
            _ => self.status.clone(),
        }
    }

    fn parse_dwl(&mut self, line: &str) {
        let mut it = line.splitn(3, ' ');
        let (Some(out), Some(key)) = (it.next(), it.next()) else { return };
        let val = it.next().unwrap_or("");
        let e = self.dwl.entry(out.to_string()).or_default();
        match key {
            "title" => e.title = val.to_string(),
            "layout" => e.layout = val.to_string(),
            "selmon" => e.selmon = val.trim() == "1",
            "tags" => {
                let n: Vec<u32> = val.split_whitespace().filter_map(|x| x.parse().ok()).collect();
                if n.len() >= 4 {
                    e.occ = n[0];
                    e.tagset = n[1];
                    e.sel = n[2];
                    e.urg = n[3];
                }
            }
            _ => return,
        }
        self.dirty = true;
    }

    fn redraw_all(&mut self) {
        self.dirty = false;
        for i in 0..self.bars.len() {
            self.draw_bar(i);
        }
    }

    fn draw_bar(&mut self, idx: usize) {
        let (w, h, configured) = {
            let b = &self.bars[idx];
            (b.w as i32, b.h as i32, b.configured)
        };
        if !configured || w == 0 || h == 0 {
            return;
        }
        let name = self
            .output_state
            .info(&self.bars[idx].output)
            .and_then(|i| i.name)
            .unwrap_or_default();
        let st = self
            .dwl
            .get(&name)
            .cloned()
            .or_else(|| self.dwl.values().next().cloned())
            .unwrap_or_default();
        let right = self.right_text();
        let icon = self.icon.clone();

        let (buffer, canvas) = match self.pool.create_buffer(w, h, w * 4, wl_shm::Format::Xrgb8888) {
            Ok(b) => b,
            Err(_) => return,
        };
        {
            let font = &self.font;
            let fh = font.height();
            let ty = (h - fh) / 2;
            let lr = fh / 2;
            let mut cv = Canvas::new(canvas, w, h);
            cv.clear(theme::BASE);

            let mut x = 0;
            for (i, tag) in TAGS.iter().enumerate() {
                let bit = 1u32 << i;
                let tw = font.width(tag) + 2 * lr;
                let selected = st.tagset & bit != 0;
                let urgent = st.urg & bit != 0;
                let (fg, bg) = if urgent {
                    (theme::BASE, theme::RED)
                } else if selected {
                    (theme::TEXT, theme::SURFACE0)
                } else {
                    (theme::TEXT, theme::BASE)
                };
                cv.rect(x, 0, tw, h, bg);
                font.draw(&mut cv, x + lr, ty, tag, fg, tw);
                if st.occ & bit != 0 {
                    let filled = st.selmon && st.sel & bit != 0;
                    let sq = (fh / 6).max(3);
                    cv.rect(x + 2, 2, sq, sq, fg);
                    if !filled {
                        cv.rect(x + 3, 3, sq - 2, sq - 2, bg);
                    }
                }
                x += tw;
            }
            let lw = font.width(&st.layout) + 2 * lr;
            cv.rect(x, 0, lw, h, theme::BASE);
            font.draw(&mut cv, x + lr, ty, &st.layout, theme::TEXT, lw);
            x += lw;

            // right side: icon + text
            let icon_w = if icon.is_some() { h } else { 0 };
            let rw = (font.width(&right) + 2 * lr).min(w * 6 / 10);
            let rx = w - icon_w - rw;
            cv.rect(rx, 0, rw, h, theme::BASE);
            let shown = font.fit(&right, rw - 2 * lr).to_string();
            font.draw(&mut cv, rx + lr, ty, &shown, theme::TEXT, rw - 2 * lr);
            if let Some(tile) = &icon {
                cv.blit(tile, h, h, w - h, 0);
            }

            // title between layout and right text
            let tw = (rx - x).max(0);
            let bg = if st.selmon { theme::SURFACE0 } else { theme::BASE };
            cv.rect(x, 0, tw, h, bg);
            if tw > 2 * lr {
                let t = font.fit(&st.title, tw - 2 * lr).to_string();
                font.draw(&mut cv, x + lr, ty, &t, theme::TEXT, tw - 2 * lr);
            }
        }
        let b = &self.bars[idx];
        b.layer.wl_surface().damage_buffer(0, 0, w, h);
        if buffer.attach_to(b.layer.wl_surface()).is_ok() {
            b.layer.commit();
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
    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(qh, surface, Layer::Top, Some("bar"), Some(&output));
        layer.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(self.bar_h as i32);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(0, self.bar_h);
        layer.commit();
        self.bars.push(Bar { layer, output, w: 0, h: 0, configured: false });
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.dirty = true;
    }
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.bars.retain(|b| b.output != output);
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        self.bars.retain(|b| b.layer.wl_surface() != layer.wl_surface());
    }
    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface, configure: LayerSurfaceConfigure, _: u32) {
        if let Some(i) = self.bars.iter().position(|b| b.layer.wl_surface() == layer.wl_surface()) {
            self.bars[i].w = configure.new_size.0;
            self.bars[i].h = if configure.new_size.1 > 0 { configure.new_size.1 } else { self.bar_h };
            self.bars[i].configured = true;
            self.draw_bar(i);
        }
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, _: Capability) {}
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, _: Capability) {}
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
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
delegate_layer!(App);
delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

fn main() {
    let font = match Font::load(FONT_PT, true) {
        Some(f) => f,
        None => {
            eprintln!("status: no usable font found");
            std::process::exit(1);
        }
    };
    let bar_h = (font.height() + 8) as u32;

    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(_) => {
            eprintln!("status: cannot connect to the Wayland display");
            std::process::exit(1);
        }
    };
    let (globals, event_queue) = registry_queue_init(&conn).expect("registry init");
    let qh = event_queue.handle();
    let mut event_loop: EventLoop<'static, App> = EventLoop::try_new().expect("event loop");
    let handle = event_loop.handle();
    WaylandSource::new(conn.clone(), event_queue)
        .insert(handle.clone())
        .expect("wayland source");

    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor missing");
    let layer_shell = LayerShell::bind(&globals, &qh).expect("wlr-layer-shell missing");
    let shm = Shm::bind(&globals, &qh).expect("wl_shm missing");
    let pool = SlotPool::new(1 << 20, &shm).expect("shm pool");

    // channels: dwl lines, notifications
    let (line_tx, line_rx): (channel::Sender<Option<String>>, Channel<Option<String>>) = channel::channel();
    let (notif_tx, notif_rx) = channel::channel::<notif::Notif>();

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        pool,
        compositor,
        layer_shell,
        font,
        bar_h,
        bars: Vec::new(),
        dwl: HashMap::new(),
        dirty: true,
        quit: false,
        status: status_text(),
        dmsg: None,
        fmsg: None,
        fmsg_mtime: 0,
        icon: None,
        icon_exp: 0,
    };

    handle
        .insert_source(line_rx, |ev, _, app: &mut App| match ev {
            ChanEvent::Msg(Some(l)) => app.parse_dwl(&l),
            ChanEvent::Msg(None) | ChanEvent::Closed => app.quit = true,
        })
        .expect("stdin source");
    handle
        .insert_source(notif_rx, |ev, _, app: &mut App| {
            if let ChanEvent::Msg(n) = ev {
                app.on_notification(n);
            }
        })
        .expect("notification source");
    handle
        .insert_source(Timer::from_duration(Duration::from_millis(250)), {
            let mut n = 0u32;
            move |_, _, app: &mut App| {
                app.poll_file_msg();
                n += 1;
                if n % 4 == 0 {
                    app.tick();
                }
                TimeoutAction::ToDuration(Duration::from_millis(250))
            }
        })
        .expect("timer");

    // reader thread for dwl's status stream
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(l) => {
                    if line_tx.send(Some(l)).is_err() {
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = line_tx.send(None);
    });
    notif::spawn(notif_tx);

    while !app.quit {
        if event_loop.dispatch(None, &mut app).is_err() {
            break;
        }
        if app.dirty {
            app.redraw_all();
        }
    }
}
