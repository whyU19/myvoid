//! clipboard: the Wayland clipboard in one tool (replaces wl-copy and the
//! clipboard script), via wlr-data-control.
//!
//!   clipboard [-t MIME] [text...]   copy the arguments, or stdin when none
//!   clipboard history               pick an entry of the history with the menu tool
//!                                   and put it back on the clipboard
//!   clipboard paste [-t MIME]       print the clipboard (used by neovim)
//!   clipboard watch                 record everything copied into the history
//!                                   (~/.cache/clipboard, 50 slots); runs from `session`
//! Copied data is served by a background copy of this program that exits as
//! soon as something else is copied. Without -t, text is offered as plain text.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use wayland_client::{
    backend::ObjectId,
    event_created_child,
    globals::{registry_queue_init, GlobalListContents},
    protocol::{wl_registry, wl_seat},
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

const MAX_PASTE: u64 = 64 * 1024 * 1024;

const TEXT_MIMES: [&str; 5] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "STRING", "TEXT"];

/* ---- history store: ~/.cache/clipboard (files "0".."49", "head", "index") ----
 * index lines: kind <TAB> slot <TAB> image:PATH|text:PATH <TAB> label, newest first */

const SLOTS: usize = 50;
const MAX_TEXT: u64 = 64 * 1024;
const MAX_IMAGE: u64 = 24 * 1024 * 1024;

struct Clip {
    dir: PathBuf,
    head: usize,
    last_hash: u64,
}

fn fnv(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

impl Clip {
    fn new() -> Clip {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let dir = PathBuf::from(home).join(".cache/clipboard");
        let _ = std::fs::create_dir_all(&dir);
        let head = std::fs::read_to_string(dir.join("head"))
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .map(|h| h % SLOTS)
            .unwrap_or(0);
        Clip { dir, head, last_hash: 0 }
    }

    fn store(&mut self, data: &[u8], image: bool) {
        if data.is_empty() {
            return;
        }
        if !image && data.iter().all(|b| b.is_ascii_whitespace()) {
            return;
        }
        let h = fnv(data);
        if h == self.last_hash {
            return;
        }
        self.last_hash = h;
        let slot = self.head;
        if std::fs::write(self.dir.join(slot.to_string()), data).is_err() {
            return;
        }
        self.head = (self.head + 1) % SLOTS;
        let _ = std::fs::write(self.dir.join("head"), self.head.to_string());
        self.write_index();
    }

    fn write_index(&self) {
        let mut out = String::new();
        for n in 0..SLOTS {
            let slot = (self.head + SLOTS - 1 - n) % SLOTS;
            let path = self.dir.join(slot.to_string());
            let Ok(mut f) = std::fs::File::open(&path) else { continue };
            let mut buf = Vec::new();
            if Read::take(&mut f, 4096).read_to_end(&mut buf).is_err() || buf.is_empty() {
                continue;
            }
            let p = path.display().to_string();
            if buf.starts_with(b"\x89PNG\r\n\x1a\n") && buf.len() >= 24 {
                let w = u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]);
                let hh = u32::from_be_bytes([buf[20], buf[21], buf[22], buf[23]]);
                let kb = std::fs::metadata(&path).map(|m| m.len() / 1024).unwrap_or(0);
                out.push_str(&format!("image\t{}\timage:{}\t[image] {}x{}  {} KB\n", slot, p, w, hh, kb));
                continue;
            }
            let text = String::from_utf8_lossy(&buf).into_owned();
            let trimmed = text.trim_start();
            let first = trimmed.lines().next().unwrap_or("");
            let label: String = first
                .chars()
                .take(200)
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect();
            // a lone path to an image file previews as that image
            let one = text.trim();
            let cand = one.strip_prefix("file://").unwrap_or(one);
            let mut pv = format!("text:{}", p);
            if !one.contains('\n') && cand.starts_with('/') {
                let lower = cand.to_lowercase();
                let is_img = [".png", ".jpg", ".jpeg", ".webp", ".gif", ".bmp"].iter().any(|e| lower.ends_with(e));
                if is_img && std::fs::metadata(cand).map(|m| m.is_file()).unwrap_or(false) {
                    pv = format!("image:{}", cand);
                }
            }
            out.push_str(&format!("text\t{}\t{}\t{}\n", slot, pv, label));
        }
        let _ = std::fs::write(self.dir.join("index"), out);
    }
}


struct PasteJob {
    want: Option<String>,
    result: Vec<u8>,
    done: bool,
}

struct State {
    data: Arc<Vec<u8>>,
    done: bool,
    /// Some = watch mode: record selections into the history
    watch: Option<Arc<Mutex<Clip>>>,
    /// Some = paste mode: read the current selection
    paste: Option<PasteJob>,
    offers: HashMap<ObjectId, Vec<String>>,
}

impl State {
    fn on_paste(&mut self, offer: Option<ZwlrDataControlOfferV1>, conn: &Connection) {
        let Some(offer) = offer else {
            if let Some(j) = &mut self.paste {
                j.done = true;
            }
            return;
        };
        let mimes = self.offers.remove(&offer.id()).unwrap_or_default();
        let want = self.paste.as_ref().and_then(|j| j.want.clone());
        let mime = match want {
            Some(w) => mimes.iter().find(|m| **m == w).cloned(),
            None => TEXT_MIMES.iter().find(|t| mimes.iter().any(|m| m == **t)).map(|t| t.to_string()),
        };
        if let Some(mime) = mime {
            if let Ok((mut rd, wr)) = UnixStream::pair() {
                offer.receive(mime, wr.as_fd());
                let _ = conn.flush();
                drop(wr);
                let mut buf = Vec::new();
                let _ = (&mut rd).take(MAX_PASTE).read_to_end(&mut buf);
                if let Some(j) = &mut self.paste {
                    j.result = buf;
                }
            }
        }
        offer.destroy();
        if let Some(j) = &mut self.paste {
            j.done = true;
        }
    }

    fn on_selection(&mut self, offer: ZwlrDataControlOfferV1, conn: &Connection) {
        let mimes = self.offers.remove(&offer.id()).unwrap_or_default();
        let secret = mimes.iter().any(|m| m == "x-kde-passwordManagerHint");
        let text = TEXT_MIMES.iter().find(|t| mimes.iter().any(|m| m == **t));
        let (mime, image) = if let Some(t) = text {
            (Some(t.to_string()), false)
        } else if mimes.iter().any(|m| m == "image/png") {
            (Some("image/png".to_string()), true)
        } else {
            (None, false)
        };
        if let (Some(mime), false, Some(clip)) = (mime, secret, self.watch.clone()) {
            if let Ok((rd, wr)) = UnixStream::pair() {
                offer.receive(mime, wr.as_fd());
                let _ = conn.flush();
                drop(wr);
                std::thread::spawn(move || {
                    let limit = if image { MAX_IMAGE } else { MAX_TEXT };
                    let mut bytes = Vec::new();
                    let mut rd = rd;
                    if (&mut rd).take(limit + 1).read_to_end(&mut bytes).is_ok() && (bytes.len() as u64) <= limit {
                        if let Ok(mut c) = clip.lock() {
                            c.store(&bytes, image);
                        }
                    }
                });
            }
        }
        offer.destroy();
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(_: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ZwlrDataControlManagerV1, ()> for State {
    fn event(_: &mut Self, _: &ZwlrDataControlManagerV1, _: <ZwlrDataControlManagerV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        conn: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if state.watch.is_none() && state.paste.is_none() {
            return;
        }
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                state.offers.insert(id.id(), Vec::new());
            }
            zwlr_data_control_device_v1::Event::Selection { id } => {
                if state.paste.is_some() {
                    state.on_paste(id, conn);
                } else if let Some(offer) = id {
                    state.on_selection(offer, conn);
                }
            }
            zwlr_data_control_device_v1::Event::PrimarySelection { id } => {
                if let Some(offer) = id {
                    state.offers.remove(&offer.id());
                    offer.destroy();
                }
            }
            _ => {}
        }
    }

    event_created_child!(State, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}
impl Dispatch<ZwlrDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if state.watch.is_some() || state.paste.is_some() {
            if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
                state.offers.entry(offer.id()).or_default().push(mime_type);
            }
        }
    }
}
impl Dispatch<ZwlrDataControlSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { fd, .. } => {
                let data = state.data.clone();
                std::thread::spawn(move || {
                    let mut f = std::fs::File::from(fd);
                    let _ = f.write_all(&data);
                });
            }
            zwlr_data_control_source_v1::Event::Cancelled => state.done = true,
            _ => {}
        }
    }
}

fn bind_all() -> (EventQueue<State>, ZwlrDataControlManagerV1, wl_seat::WlSeat) {
    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(_) => {
            eprintln!("clipboard: cannot connect to the Wayland display");
            std::process::exit(1);
        }
    };
    let (globals, queue) = registry_queue_init::<State>(&conn).expect("registry init");
    let qh = queue.handle();
    let manager: ZwlrDataControlManagerV1 = match globals.bind(&qh, 1..=2, ()) {
        Ok(m) => m,
        Err(_) => {
            eprintln!("clipboard: compositor does not support wlr-data-control");
            std::process::exit(1);
        }
    };
    let seat: wl_seat::WlSeat = match globals.bind(&qh, 1..=1, ()) {
        Ok(s) => s,
        Err(_) => std::process::exit(1),
    };
    (queue, manager, seat)
}

/// Background owner of the selection (started by `set_clipboard`).
fn serve(mime: Option<String>) -> ! {
    let mut data = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut data);
    let (mut queue, manager, seat) = bind_all();
    let qh = queue.handle();
    let device = manager.get_data_device(&seat, &qh, ());
    let source = manager.create_data_source(&qh, ());
    match &mime {
        Some(m) => source.offer(m.clone()),
        None => {
            for m in TEXT_MIMES {
                source.offer(m.to_string());
            }
        }
    }
    device.set_selection(Some(&source));
    let mut state = State { data: Arc::new(data), done: false, watch: None, paste: None, offers: HashMap::new() };
    while !state.done {
        if queue.blocking_dispatch(&mut state).is_err() {
            break;
        }
    }
    std::process::exit(0);
}

/// Print the current clipboard to stdout.
fn paste(want: Option<String>) -> ! {
    let (mut queue, manager, seat) = bind_all();
    let qh = queue.handle();
    let _device = manager.get_data_device(&seat, &qh, ());
    let mut state = State {
        data: Arc::new(Vec::new()),
        done: false,
        watch: None,
        paste: Some(PasteJob { want, result: Vec::new(), done: false }),
        offers: HashMap::new(),
    };
    while !state.paste.as_ref().map(|j| j.done).unwrap_or(true) {
        if queue.blocking_dispatch(&mut state).is_err() {
            break;
        }
    }
    if let Some(j) = state.paste {
        let _ = std::io::stdout().write_all(&j.result);
    }
    std::process::exit(0);
}

/// Record everything that is copied.
fn watch() -> ! {
    let (mut queue, manager, seat) = bind_all();
    let qh = queue.handle();
    let _device = manager.get_data_device(&seat, &qh, ());
    let mut state = State {
        data: Arc::new(Vec::new()),
        done: false,
        watch: Some(Arc::new(Mutex::new(Clip::new()))),
        paste: None,
        offers: HashMap::new(),
    };
    loop {
        if queue.blocking_dispatch(&mut state).is_err() {
            break;
        }
    }
    std::process::exit(0);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut mime: Option<String> = None;
    let mut serve_mode = false;
    let mut words: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-t" if i + 1 < args.len() => {
                i += 1;
                mime = Some(args[i].clone());
            }
            "--serve" => serve_mode = true,
            a => words.push(a.to_string()),
        }
        i += 1;
    }
    if serve_mode {
        serve(mime);
    }
    if words.len() == 1 && mime.is_none() {
        match words[0].as_str() {
            "history" => {
                history();
                return;
            }
            "watch" => watch(),
            "paste" => paste(None),
            _ => {}
        }
    }
    if words.len() == 1 && words[0] == "paste" {
        paste(mime.clone());
    }

    let mut data = Vec::new();
    if words.is_empty() {
        let _ = std::io::stdin().read_to_end(&mut data);
    } else {
        data = words.join(" ").into_bytes();
    }
    set_clipboard(&data, mime.as_deref());
}

/// Start the background process that owns the selection.
fn set_clipboard(data: &[u8], mime: Option<&str>) {
    let exe = std::env::current_exe().unwrap_or_else(|_| "copy".into());
    let mut cmd = Command::new(exe);
    cmd.arg("--serve");
    if let Some(m) = mime {
        cmd.args(["-t", m]);
    }
    let child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
    match child {
        Ok(mut c) => {
            if let Some(mut stdin) = c.stdin.take() {
                let _ = stdin.write_all(data);
            }
        }
        Err(_) => std::process::exit(1),
    }
}

/// Menu over ~/.cache/clipboard/index; the choice goes back on the clipboard.
fn history() {
    let home = std::env::var("HOME").unwrap_or_default();
    let dir = format!("{}/.cache/clipboard", home);
    let Ok(index) = std::fs::read_to_string(format!("{}/index", dir)) else { return };
    // index lines: kind <TAB> slot <TAB> image:PATH|text:PATH <TAB> label
    let mut items = String::new();
    for l in index.lines() {
        let f: Vec<&str> = l.splitn(4, '\t').collect();
        if f.len() == 4 {
            items.push_str(&format!("{}\t{}\t{} {}\n", f[3], f[2], f[0], f[1]));
        }
    }
    if items.is_empty() {
        return;
    }
    let Ok(mut menu) = Command::new(format!("{}/.config/menu/menu", home))
        .args(["-t", "-l", "15", "-p", "Clipboard>"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
    else {
        return;
    };
    if let Some(mut stdin) = menu.stdin.take() {
        let _ = stdin.write_all(items.as_bytes());
    }
    let Ok(out) = menu.wait_with_output() else { return };
    if !out.status.success() {
        return;
    }
    let choice = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
    let Some((_, rest)) = choice.rsplit_once('\t') else { return };
    let Some((kind, slot)) = rest.rsplit_once(' ') else { return };
    if slot.is_empty() || !slot.chars().all(|c| c.is_ascii_digit()) {
        return;
    }
    let Ok(data) = std::fs::read(format!("{}/{}", dir, slot)) else { return };
    set_clipboard(&data, if kind == "image" { Some("image/png") } else { None });
    let _ = Command::new("notify-send").args(["Clipboard", "Copied"]).status();
}
