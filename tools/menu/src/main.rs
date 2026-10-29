//! menu: dmenu-style launcher/picker for Wayland (wlr-layer-shell overlay).
//!
//! usage: menu [-l lines] [-p prompt] [-P] [-t] [-r]
//!   -r  application launcher: lists the .desktop applications itself and
//!       runs the choice; text that is not an application is evaluated as a
//!       calculation (result is copied and shown with notify-send).
//!   -P  password mode (typed text is masked and printed on Enter)
//!   -t  items are "label<TAB>image:PATH|text:PATH<TAB>..." with a preview pane;
//!       the whole selected line is printed.
//! Reads items from stdin, prints the choice on stdout. Exit 1 on cancel.

use std::collections::HashMap;
use std::io::{BufRead, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::Duration;

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            timer::{TimeoutAction, Timer},
            EventLoop, LoopHandle, RegistrationToken,
        },
        calloop_wayland_source::WaylandSource,
        client::{
            globals::registry_queue_init,
            protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
            Connection, QueueHandle,
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
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use wlkit::{theme, Canvas, Font, FONT_PT};

const PAD: i32 = 10;
const BTN_LEFT: u32 = 0x110;

struct Item {
    line: String,
    label: String,
    lower: String,
    /// (is_image, path)
    pv: Option<(bool, String)>,
}

struct Geom {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    lw: i32,
    hh: i32,
    rowh: i32,
    rows: usize,
}

struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    loop_handle: LoopHandle<'static, App>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    font: Font,
    mods: Modifiers,
    repeat_ev: Option<KeyEvent>,
    repeat_tok: Option<RegistrationToken>,

    sw: u32,
    sh: u32,
    configured: bool,

    items: Vec<Item>,
    vis: Vec<usize>,
    sel: usize,
    top: usize,
    query: String,
    password: bool,
    haspv: bool,
    maxlines: usize,
    prompt: String,
    scroll_acc: f64,

    pw: u32,
    ph: u32,
    preview_img: Option<Vec<u8>>,
    preview_text: Option<String>,

    /// Some(Some(text)) = chosen, Some(None) = cancelled
    result: Option<Option<String>>,
}

impl App {
    fn geom(&self) -> Geom {
        let fh = self.font.height();
        let rowh = fh + 8;
        let hh = rowh + PAD;
        let sw = self.sw as i32;
        let sh = self.sh as i32;
        let (w, h, lw, rows);
        if self.haspv {
            w = (sw * 8 / 10).min(1460);
            h = (sh * 7 / 10).min(800);
            lw = w * 4 / 10;
            rows = (((h - hh - PAD / 2) / rowh).max(1)) as usize;
        } else {
            w = (sw * 6 / 10).min(960);
            rows = self.items.len().min(self.maxlines);
            h = hh + rows as i32 * rowh + if rows > 0 { PAD / 2 } else { 0 };
            lw = w;
        }
        Geom { x: (sw - w) / 2, y: (sh - h) / 2, w, h, lw, hh, rowh, rows }
    }

    fn refilter(&mut self) {
        let toks: Vec<String> = self
            .query
            .to_lowercase()
            .split_whitespace()
            .map(|s| s.to_string())
            .collect();
        self.vis = (0..self.items.len())
            .filter(|&i| toks.iter().all(|t| self.items[i].lower.contains(t.as_str())))
            .collect();
        if self.sel >= self.vis.len() {
            self.sel = self.vis.len().saturating_sub(1);
        }
        if self.sel < self.top {
            self.top = self.sel;
        }
    }

    fn load_preview(&mut self) {
        self.preview_img = None;
        self.preview_text = None;
        if !self.haspv || self.vis.is_empty() {
            return;
        }
        let it = &self.items[self.vis[self.sel]];
        if let Some((is_img, path)) = &it.pv {
            if *is_img {
                if self.pw > 0 && self.ph > 0 {
                    self.preview_img = wlkit::load_fit(path, self.pw, self.ph, false, theme::BASE);
                }
            } else if let Ok(f) = std::fs::File::open(path) {
                let mut buf = Vec::new();
                if f.take(65536).read_to_end(&mut buf).is_ok() {
                    self.preview_text = Some(String::from_utf8_lossy(&buf).into_owned());
                }
            }
        }
    }

    fn finish(&mut self, use_text: bool) {
        let out = if !self.vis.is_empty() && !use_text {
            self.items[self.vis[self.sel]].line.clone()
        } else {
            self.query.clone()
        };
        self.result = Some(Some(out));
    }

    fn cancel(&mut self) {
        self.result = Some(None);
    }

    fn move_sel(&mut self, delta: i64) {
        if self.vis.is_empty() {
            return;
        }
        let n = self.vis.len() as i64;
        let v = (self.sel as i64 + delta).clamp(0, n - 1);
        self.sel = v as usize;
        self.load_preview();
        self.draw();
    }

    fn query_changed(&mut self) {
        self.sel = 0;
        self.top = 0;
        self.refilter();
        self.load_preview();
        self.draw();
    }

    fn handle_key(&mut self, ev: &KeyEvent) {
        let ks = ev.keysym.raw();
        let ctrl = self.mods.ctrl;
        let shift = self.mods.shift;
        match ks {
            0xff1b => self.cancel(),                       // Escape
            0x63 if ctrl => self.cancel(),                 // Ctrl-C
            0xff0d | 0xff8d => self.finish(shift || self.password), // Return, KP_Enter
            0xff52 | 0xff51 => self.move_sel(-1),          // Up, Left
            0x70 if ctrl => self.move_sel(-1),             // Ctrl-P
            0xff54 | 0xff53 | 0xff09 => self.move_sel(1), // Down, Right, Tab
            0x6e if ctrl => self.move_sel(1),              // Ctrl-N
            0xff55 => self.move_sel(-10),                  // Page Up
            0xff56 => self.move_sel(10),                   // Page Down
            0xff50 => self.move_sel(i64::MIN / 2),         // Home
            0xff57 => self.move_sel(i64::MAX / 2),         // End
            0xff08 => {
                if self.query.pop().is_some() {
                    self.query_changed();
                }
            }
            0x75 if ctrl => {
                self.query.clear();
                self.query_changed();
            }
            _ => {
                if ctrl {
                    return;
                }
                if let Some(t) = &ev.utf8 {
                    let clean: String = t.chars().filter(|c| !c.is_control()).collect();
                    if !clean.is_empty() && self.query.len() + clean.len() < 255 {
                        self.query.push_str(&clean);
                        self.query_changed();
                    }
                }
            }
        }
    }

    fn start_repeat(&mut self, ev: KeyEvent) {
        self.stop_repeat();
        self.repeat_ev = Some(ev);
        let tok = self.loop_handle.insert_source(
            Timer::from_duration(Duration::from_millis(400)),
            |_, _, app: &mut App| {
                if let Some(ev) = app.repeat_ev.clone() {
                    app.handle_key(&ev);
                    TimeoutAction::ToDuration(Duration::from_millis(35))
                } else {
                    TimeoutAction::Drop
                }
            },
        );
        self.repeat_tok = tok.ok();
    }

    fn stop_repeat(&mut self) {
        if let Some(t) = self.repeat_tok.take() {
            self.loop_handle.remove(t);
        }
        self.repeat_ev = None;
    }

    fn click(&mut self, pos: (f64, f64)) {
        let g = self.geom();
        let (x, y) = (pos.0 as i32, pos.1 as i32);
        if x < g.x || x >= g.x + g.w || y < g.y || y >= g.y + g.h {
            self.cancel();
            return;
        }
        let listw = if self.haspv { g.lw } else { g.w };
        if x < g.x + listw && y >= g.y + g.hh {
            let idx = self.top + ((y - (g.y + g.hh)) / g.rowh) as usize;
            if idx < self.vis.len() {
                self.sel = idx;
                self.finish(false);
            }
        }
    }

    fn draw(&mut self) {
        if !self.configured || self.sw == 0 || self.sh == 0 {
            return;
        }
        let g = self.geom();
        // keep the selection inside the visible window of rows
        if g.rows > 0 {
            if self.sel >= self.top + g.rows {
                self.top = self.sel + 1 - g.rows;
            }
            if self.sel < self.top {
                self.top = self.sel;
            }
        }
        let (sw, sh) = (self.sw as i32, self.sh as i32);
        let (buffer, canvas) =
            match self.pool.create_buffer(sw, sh, sw * 4, wl_shm::Format::Argb8888) {
                Ok(b) => b,
                Err(_) => return,
            };
        {
            let mut cv = Canvas::new(canvas, sw, sh);
            cv.clear(0x0000_0000);
            let fh = self.font.height();
            let listw = if self.haspv { g.lw } else { g.w };

            // one solid colour, like a terminal: no panels, borders or bars
            cv.rect(g.x, g.y, g.w, g.h, theme::BASE);

            // prompt + query
            let shown: String = if self.password {
                "*".repeat(self.query.chars().count())
            } else {
                self.query.clone()
            };
            let line = if self.prompt.is_empty() {
                shown
            } else {
                format!("{} {}", self.prompt, shown)
            };
            let ty = g.y + (g.hh - fh) / 2;
            let fitted = self.font.fit(&line, g.w - 2 * PAD).to_string();
            let used = self.font.draw(&mut cv, g.x + PAD, ty, &fitted, theme::TEXT, g.w - 2 * PAD);
            cv.rect(g.x + PAD + used, ty, self.font.width("m"), fh, 0xfff5_e0dc); // block cursor

            // rows
            for i in 0..g.rows {
                let idx = self.top + i;
                if idx >= self.vis.len() {
                    break;
                }
                let it = &self.items[self.vis[idx]];
                let y = g.y + g.hh + i as i32 * g.rowh;
                if idx == self.sel {
                    cv.rect(g.x, y, listw, g.rowh, theme::SURFACE0);
                }
                let col = match &it.pv {
                    Some((true, _)) => theme::MAUVE,
                    _ => theme::TEXT,
                };
                self.font.draw(&mut cv, g.x + PAD, y + (g.rowh - fh) / 2, &it.label, col, listw - 2 * PAD);
            }
            if self.vis.is_empty() && !self.items.is_empty() {
                self.font.draw(
                    &mut cv,
                    g.x + PAD,
                    g.y + g.hh + (g.rowh - fh) / 2,
                    "no matches",
                    theme::OVERLAY0,
                    listw - 2 * PAD,
                );
            }

            // preview pane
            if self.haspv {
                let px = g.x + g.lw + PAD;
                let py = g.y + g.hh + PAD;
                if let Some(img) = &self.preview_img {
                    cv.blit(img, self.pw as i32, self.ph as i32, px, py);
                } else if let Some(text) = &self.preview_text {
                    let prows = (g.h - g.hh - 2 * PAD) / fh;
                    for (i, l) in text.lines().take(prows.max(0) as usize).enumerate() {
                        let l = l.replace('\t', "  ");
                        self.font.draw(&mut cv, px, py + i as i32 * fh, &l, theme::TEXT, g.w - g.lw - 2 * PAD);
                    }
                }
            }
        }
        self.layer.wl_surface().damage_buffer(0, 0, sw, sh);
        if buffer.attach_to(self.layer.wl_surface()).is_ok() {
            self.layer.commit();
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

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.cancel();
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        self.sw = configure.new_size.0;
        self.sh = configure.new_size.1;
        self.configured = true;
        let g = self.geom();
        self.pw = (g.w - g.lw - 2 * PAD).max(0) as u32;
        self.ph = (g.h - g.hh - 2 * PAD).max(0) as u32;
        self.load_preview();
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
        if cap == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, cap: Capability) {
        if cap == Capability::Keyboard {
            if let Some(k) = self.keyboard.take() {
                k.release();
            }
        }
        if cap == Capability::Pointer {
            if let Some(p) = self.pointer.take() {
                p.release();
            }
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for App {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32, _: &[u32], _: &[Keysym]) {}
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32) {
        self.stop_repeat();
    }
    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, event: KeyEvent) {
        self.handle_key(&event);
        if self.result.is_none() {
            self.start_repeat(event);
        }
    }
    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, event: KeyEvent) {
        if let Some(r) = &self.repeat_ev {
            if r.raw_code == event.raw_code {
                self.stop_repeat();
            }
        }
    }
    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, modifiers: Modifiers, _: u32) {
        self.mods = modifiers;
    }
}

impl PointerHandler for App {
    fn pointer_frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for e in events {
            match &e.kind {
                PointerEventKind::Press { button, .. } => {
                    if *button == BTN_LEFT {
                        self.click(e.position);
                    }
                }
                PointerEventKind::Axis { vertical, .. } => {
                    self.scroll_acc += vertical.absolute;
                    while self.scroll_acc >= 10.0 {
                        self.scroll_acc -= 10.0;
                        self.move_sel(1);
                    }
                    while self.scroll_acc <= -10.0 {
                        self.scroll_acc += 10.0;
                        self.move_sel(-1);
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

fn usage() -> ! {
    eprintln!("usage: menu [-l lines] [-p prompt] [-P] [-t] [-r]");
    std::process::exit(2);
}

fn read_items(tabs: bool) -> (Vec<Item>, bool) {
    let mut items = Vec::new();
    let mut haspv = false;
    let stdin = std::io::stdin();
    let mut raw = Vec::new();
    let mut lock = stdin.lock();
    loop {
        raw.clear();
        match lock.read_until(b'\n', &mut raw) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&raw).trim_end_matches(['\n', '\r']).to_string();
        if line.is_empty() {
            continue;
        }
        let mut label = line.clone();
        let mut pv = None;
        if tabs {
            if let Some((l, rest)) = line.split_once('\t') {
                label = l.to_string();
                let field = rest.split('\t').next().unwrap_or("");
                if let Some(p) = field.strip_prefix("image:") {
                    pv = Some((true, p.to_string()));
                    haspv = true;
                } else if let Some(p) = field.strip_prefix("text:") {
                    pv = Some((false, p.to_string()));
                    haspv = true;
                }
            }
        }
        items.push(Item { lower: label.to_lowercase(), label, line, pv });
    }
    (items, haspv)
}

/* ---- launcher mode (-r) ---- */

/// Applications from the .desktop files: items for the menu plus name -> command.
fn load_apps() -> (Vec<Item>, HashMap<String, String>) {
    let home = std::env::var("HOME").unwrap_or_default();
    let dirs = ["/usr/share/applications".to_string(), format!("{}/.local/share/applications", home)];
    let mut apps: HashMap<String, String> = HashMap::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut paths: Vec<_> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "desktop").unwrap_or(false) && p.is_file())
            .collect();
        paths.sort();
        for p in paths {
            let Ok(text) = std::fs::read_to_string(&p) else { continue };
            let (mut name, mut exec, mut hidden, mut in_entry) = (None::<String>, None::<String>, false, false);
            for line in text.lines() {
                let line = line.trim();
                if line.starts_with('[') {
                    if in_entry {
                        break; // only the [Desktop Entry] group counts
                    }
                    in_entry = line == "[Desktop Entry]";
                    continue;
                }
                if !in_entry {
                    continue;
                }
                if line == "NoDisplay=true" || line == "Hidden=true" {
                    hidden = true;
                } else if let Some(v) = line.strip_prefix("Name=") {
                    name.get_or_insert_with(|| v.to_string());
                } else if let Some(v) = line.strip_prefix("Exec=") {
                    exec.get_or_insert_with(|| v.to_string());
                }
            }
            if let (false, Some(n), Some(e)) = (hidden, name, exec) {
                // drop field codes such as %U %f
                let mut cmd = String::new();
                let mut it = e.chars().peekable();
                while let Some(c) = it.next() {
                    if c == '%' {
                        if let Some(n) = it.peek() {
                            if "fFuUdDnNickvm".contains(*n) {
                                it.next();
                                continue;
                            }
                        }
                    }
                    cmd.push(c);
                }
                let cmd = cmd.trim().to_string();
                if !n.is_empty() && !cmd.is_empty() {
                    apps.entry(n).or_insert(cmd);
                }
            }
        }
    }
    let mut names: Vec<String> = apps.keys().cloned().collect();
    names.sort();
    let items = names
        .into_iter()
        .map(|n| Item { lower: n.to_lowercase(), line: n.clone(), label: n, pv: None })
        .collect();
    (items, apps)
}

fn calc_expr(s: &[u8], i: &mut usize) -> Option<f64> {
    let mut v = calc_term(s, i)?;
    loop {
        skip_ws(s, i);
        match s.get(*i) {
            Some(b'+') => {
                *i += 1;
                v += calc_term(s, i)?;
            }
            Some(b'-') => {
                *i += 1;
                v -= calc_term(s, i)?;
            }
            _ => return Some(v),
        }
    }
}

fn calc_term(s: &[u8], i: &mut usize) -> Option<f64> {
    let mut v = calc_unary(s, i)?;
    loop {
        skip_ws(s, i);
        match s.get(*i) {
            Some(b'*') => {
                *i += 1;
                v *= calc_unary(s, i)?;
            }
            Some(b'/') => {
                *i += 1;
                v /= calc_unary(s, i)?;
            }
            _ => return Some(v),
        }
    }
}

fn calc_unary(s: &[u8], i: &mut usize) -> Option<f64> {
    skip_ws(s, i);
    match s.get(*i) {
        Some(b'-') => {
            *i += 1;
            Some(-calc_unary(s, i)?)
        }
        Some(b'+') => {
            *i += 1;
            calc_unary(s, i)
        }
        _ => {
            let base = calc_atom(s, i)?;
            skip_ws(s, i);
            if s.get(*i) == Some(&b'^') {
                *i += 1;
                let exp = calc_unary(s, i)?;
                Some(base.powf(exp))
            } else {
                Some(base)
            }
        }
    }
}

fn calc_atom(s: &[u8], i: &mut usize) -> Option<f64> {
    skip_ws(s, i);
    if s.get(*i) == Some(&b'(') {
        *i += 1;
        let v = calc_expr(s, i)?;
        skip_ws(s, i);
        if s.get(*i) != Some(&b')') {
            return None;
        }
        *i += 1;
        return Some(v);
    }
    let start = *i;
    while *i < s.len() && (s[*i].is_ascii_digit() || s[*i] == b'.') {
        *i += 1;
    }
    std::str::from_utf8(&s[start..*i]).ok()?.parse().ok()
}

fn skip_ws(s: &[u8], i: &mut usize) {
    while *i < s.len() && s[*i] == b' ' {
        *i += 1;
    }
}

/// Evaluate "1+2*3" style input; None when it is not plain arithmetic.
fn calc(q: &str) -> Option<String> {
    if !q.chars().any(|c| c.is_ascii_digit()) || q.chars().any(|c| !"0123456789+-*/^(). ".contains(c)) {
        return None;
    }
    let s = q.as_bytes();
    let mut i = 0;
    let v = calc_expr(s, &mut i)?;
    skip_ws(s, &mut i);
    if i != s.len() || !v.is_finite() {
        return None;
    }
    let rounded: f64 = format!("{:.9e}", v).parse().ok()?; // ~10 significant digits
    Some(format!("{}", rounded))
}

fn launch(chosen: &str, apps: &HashMap<String, String>) {
    if chosen.is_empty() {
        return;
    }
    if let Some(cmd) = apps.get(chosen) {
        let _ = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn();
        return;
    }
    if let Some(res) = calc(chosen) {
        if let Ok(mut c) = Command::new(format!("{}/.config/clipboard/clipboard", std::env::var("HOME").unwrap_or_default())).stdin(Stdio::piped()).spawn() {
            if let Some(mut stdin) = c.stdin.take() {
                let _ = stdin.write_all(res.as_bytes());
            }
            let _ = c.wait();
        }
        let _ = Command::new("notify-send")
            .arg("Calc")
            .arg(format!("{} = {}", chosen, res))
            .status();
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mut maxlines, mut prompt, mut password, mut tabs) = (10usize, String::new(), false, false);
    let (mut run_apps, mut lines_set) = (false, false);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-l" if i + 1 < args.len() => {
                i += 1;
                maxlines = args[i].parse().unwrap_or(10);
                lines_set = true;
            }
            "-p" if i + 1 < args.len() => {
                i += 1;
                prompt = args[i].clone();
            }
            "-P" => password = true,
            "-t" => tabs = true,
            "-r" => run_apps = true,
            // dmenu options accepted and ignored
            "-m" | "-fn" | "-nb" | "-nf" | "-sb" | "-sf" | "-w" if i + 1 < args.len() => i += 1,
            a if a.starts_with('-') => {}
            _ => usage(),
        }
        i += 1;
    }
    if run_apps {
        if prompt.is_empty() {
            prompt = "Launch>".to_string();
        }
        if !lines_set {
            maxlines = 15;
        }
    }
    maxlines = maxlines.max(1);

    let (items, haspv, apps) = if run_apps {
        let (items, apps) = load_apps();
        (items, false, apps)
    } else {
        let (items, haspv) = read_items(tabs);
        (items, haspv, HashMap::new())
    };
    let font = match Font::load(FONT_PT, true) {
        Some(f) => f,
        None => {
            eprintln!("menu: no usable font found");
            std::process::exit(1);
        }
    };

    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(_) => {
            eprintln!("menu: cannot connect to the Wayland display");
            std::process::exit(1);
        }
    };
    let (globals, event_queue) = registry_queue_init(&conn).expect("registry init");
    let qh = event_queue.handle();
    let mut event_loop: EventLoop<'static, App> = EventLoop::try_new().expect("event loop");
    let loop_handle = event_loop.handle();
    WaylandSource::new(conn.clone(), event_queue)
        .insert(loop_handle.clone())
        .expect("wayland source");

    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor missing");
    let layer_shell = LayerShell::bind(&globals, &qh).expect("wlr-layer-shell missing");
    let shm = Shm::bind(&globals, &qh).expect("wl_shm missing");
    let surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("menu"), None);
    layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    layer.set_exclusive_zone(-1);
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    layer.set_size(0, 0);
    layer.commit();
    let pool = SlotPool::new(1 << 20, &shm).expect("shm pool");

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        pool,
        layer,
        loop_handle,
        keyboard: None,
        pointer: None,
        font,
        mods: Modifiers::default(),
        repeat_ev: None,
        repeat_tok: None,
        sw: 0,
        sh: 0,
        configured: false,
        items,
        vis: Vec::new(),
        sel: 0,
        top: 0,
        query: String::new(),
        password,
        haspv,
        maxlines,
        prompt,
        scroll_acc: 0.0,
        pw: 0,
        ph: 0,
        preview_img: None,
        preview_text: None,
        result: None,
    };
    app.refilter();

    while app.result.is_none() {
        if event_loop.dispatch(None, &mut app).is_err() {
            std::process::exit(1);
        }
    }
    match app.result.take().flatten() {
        Some(out) => {
            if run_apps {
                launch(&out, &apps);
            } else {
                println!("{}", out);
            }
            std::process::exit(0);
        }
        None => std::process::exit(1),
    }
}
