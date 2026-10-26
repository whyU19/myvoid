//! display: monitor modes on Wayland (wlr-output-management), with menus.
//!
//!   display             menu with every resolution and refresh rate of the monitor(s)
//!   display restore     re-apply the saved modes (run at session start)
//!   display list [all]  print modes (current resolution only, or all)
//!   display set OUT WxH RATE
//! Chosen modes are saved in ~/.local/state/refreshrate. Menus use
//! ~/.config/menu/menu, messages go through notify-send.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

use wayland_client::{
    backend::ObjectId,
    event_created_child,
    globals::{registry_queue_init, GlobalListContents},
    protocol::wl_registry,
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};
use wayland_protocols_wlr::output_management::v1::client::{
    zwlr_output_configuration_head_v1::{self, ZwlrOutputConfigurationHeadV1},
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};

#[derive(Default, Clone, Copy)]
struct ModeInfo {
    w: i32,
    h: i32,
    /// millihertz
    refresh: i32,
}

struct HeadInfo {
    head: ZwlrOutputHeadV1,
    name: String,
    enabled: bool,
    modes: Vec<ZwlrOutputModeV1>,
    current: Option<ZwlrOutputModeV1>,
}

#[derive(Default)]
struct State {
    heads: Vec<HeadInfo>,
    modes: HashMap<ObjectId, ModeInfo>,
    serial: Option<u32>,
    result: Option<bool>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrOutputManagerV1,
        event: zwlr_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_manager_v1::Event::Head { head } => state.heads.push(HeadInfo {
                head,
                name: String::new(),
                enabled: false,
                modes: Vec::new(),
                current: None,
            }),
            zwlr_output_manager_v1::Event::Done { serial } => state.serial = Some(serial),
            _ => {}
        }
    }

    event_created_child!(State, ZwlrOutputManagerV1, [
        zwlr_output_manager_v1::EVT_HEAD_OPCODE => (ZwlrOutputHeadV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputHeadV1, ()> for State {
    fn event(
        state: &mut Self,
        head: &ZwlrOutputHeadV1,
        event: zwlr_output_head_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_output_head_v1::Event::Finished = event {
            state.heads.retain(|h| &h.head != head);
            return;
        }
        let Some(h) = state.heads.iter_mut().find(|h| &h.head == head) else {
            return;
        };
        match event {
            zwlr_output_head_v1::Event::Name { name } => h.name = name,
            zwlr_output_head_v1::Event::Mode { mode } => h.modes.push(mode),
            zwlr_output_head_v1::Event::Enabled { enabled } => h.enabled = enabled != 0,
            zwlr_output_head_v1::Event::CurrentMode { mode } => h.current = Some(mode),
            _ => {}
        }
    }

    event_created_child!(State, ZwlrOutputHeadV1, [
        zwlr_output_head_v1::EVT_MODE_OPCODE => (ZwlrOutputModeV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputModeV1, ()> for State {
    fn event(
        state: &mut Self,
        mode: &ZwlrOutputModeV1,
        event: zwlr_output_mode_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let m = state.modes.entry(mode.id()).or_default();
        match event {
            zwlr_output_mode_v1::Event::Size { width, height } => {
                m.w = width;
                m.h = height;
            }
            zwlr_output_mode_v1::Event::Refresh { refresh } => m.refresh = refresh,
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputConfigurationV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrOutputConfigurationV1,
        event: zwlr_output_configuration_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_configuration_v1::Event::Succeeded => state.result = Some(true),
            zwlr_output_configuration_v1::Event::Failed
            | zwlr_output_configuration_v1::Event::Cancelled => state.result = Some(false),
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputConfigurationHeadV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrOutputConfigurationHeadV1,
        _: zwlr_output_configuration_head_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

fn mode_info(state: &State, m: &ZwlrOutputModeV1) -> ModeInfo {
    state.modes.get(&m.id()).copied().unwrap_or_default()
}

struct Row {
    out: String,
    w: i32,
    h: i32,
    refresh: i32,
    cur: bool,
}

fn rows(state: &State, all: bool) -> Vec<Row> {
    let mut res: Vec<Row> = Vec::new();
    for h in state.heads.iter().filter(|h| h.enabled) {
        let cur = h.current.as_ref().map(|m| mode_info(state, m));
        let mut mine: Vec<Row> = Vec::new();
        for m in &h.modes {
            let mi = mode_info(state, m);
            if mi.w == 0 || mi.h == 0 {
                continue;
            }
            if !all {
                match cur {
                    Some(c) if c.w == mi.w && c.h == mi.h => {}
                    _ => continue,
                }
            }
            let key = (mi.w, mi.h, (mi.refresh + 5) / 10);
            if mine.iter().any(|r| (r.w, r.h, (r.refresh + 5) / 10) == key) {
                continue;
            }
            mine.push(Row {
                out: h.name.clone(),
                w: mi.w,
                h: mi.h,
                refresh: mi.refresh,
                cur: h.current.as_ref() == Some(m),
            });
        }
        mine.sort_by(|a, b| (b.w, b.h, b.refresh).cmp(&(a.w, a.h, a.refresh)));
        res.extend(mine);
    }
    res
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())
}

fn state_path() -> String {
    format!("{}/.local/state/refreshrate", home())
}

fn notify(title: &str, msg: &str, critical: bool) {
    let mut c = Command::new("notify-send");
    if critical {
        c.args(["-u", "critical"]);
    }
    let _ = c.arg(title).arg(msg).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// Show `items` in the menu tool; None when cancelled.
fn pick(prompt: &str, items: &[String]) -> Option<String> {
    let mut child = Command::new(format!("{}/.config/menu/menu", home()))
        .args(["-l", "15", "-p", prompt])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(items.join("\n").as_bytes());
    }
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

type Target = (String, i32, i32, f64);

/// Apply modes to one or more outputs in a single configuration.
fn apply(
    state: &mut State,
    manager: &ZwlrOutputManagerV1,
    qh: &QueueHandle<State>,
    queue: &mut EventQueue<State>,
    targets: &[Target],
) -> bool {
    let mut chosen: Vec<(String, ZwlrOutputModeV1)> = Vec::new();
    for (out, w, h, want) in targets {
        let found = state.heads.iter().find(|x| &x.name == out).and_then(|x| {
            x.modes
                .iter()
                .find(|m| {
                    let mi = mode_info(state, m);
                    mi.w == *w && mi.h == *h && (mi.refresh as f64 / 1000.0 - want).abs() < 0.02
                })
                .cloned()
        });
        if let Some(m) = found {
            chosen.push((out.clone(), m));
        }
    }
    if chosen.is_empty() {
        return false;
    }
    let config = manager.create_configuration(state.serial.unwrap_or(0), qh, ());
    for hd in &state.heads {
        if hd.enabled {
            let ch = config.enable_head(&hd.head, qh, ());
            if let Some((_, m)) = chosen.iter().find(|(o, _)| o == &hd.name) {
                ch.set_mode(m);
            }
        } else {
            config.disable_head(&hd.head);
        }
    }
    state.result = None;
    config.apply();
    while state.result.is_none() {
        if queue.blocking_dispatch(state).is_err() {
            break;
        }
    }
    state.result == Some(true)
}

fn save(out: &str, size: &str, rate: &str) {
    let path = state_path();
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let prefix = format!("{} ", out);
    let mut text: String = old.lines().filter(|l| !l.starts_with(&prefix)).map(|l| format!("{}\n", l)).collect();
    text.push_str(&format!("{} {} {}\n", out, size, rate));
    if let Some(dir) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, text);
}

fn parse_size(s: &str) -> Option<(i32, i32)> {
    let (a, b) = s.split_once('x')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

fn usage() -> ! {
    eprintln!("usage: display [restore | list [all] | set OUTPUT WxH RATE]");
    std::process::exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(_) => {
            eprintln!("display: cannot connect to the Wayland display");
            std::process::exit(1);
        }
    };
    let (globals, mut queue) = registry_queue_init::<State>(&conn).expect("registry init");
    let qh = queue.handle();
    let manager: ZwlrOutputManagerV1 = match globals.bind(&qh, 1..=4, ()) {
        Ok(m) => m,
        Err(_) => {
            eprintln!("display: compositor does not support wlr-output-management");
            std::process::exit(1);
        }
    };
    let mut state = State::default();
    while state.serial.is_none() {
        if queue.blocking_dispatch(&mut state).is_err() {
            std::process::exit(1);
        }
    }

    let code = match args.first().map(|s| s.as_str()) {
        Some("list") => {
            let r = rows(&state, args.get(1).map(|s| s.as_str()) == Some("all"));
            for x in &r {
                println!("{} {}x{} {:.2}{}", x.out, x.w, x.h, x.refresh as f64 / 1000.0, if x.cur { " *" } else { "" });
            }
            if r.is_empty() { 1 } else { 0 }
        }
        Some("set") if args.len() == 4 => {
            let Some((w, h)) = parse_size(&args[2]) else { usage() };
            let rate: f64 = args[3].parse().unwrap_or(0.0);
            if apply(&mut state, &manager, &qh, &mut queue, &[(args[1].clone(), w, h, rate)]) { 0 } else { 1 }
        }
        Some("restore") => {
            let text = std::fs::read_to_string(state_path()).unwrap_or_default();
            let mut targets: Vec<Target> = Vec::new();
            for l in text.lines() {
                let f: Vec<&str> = l.split_whitespace().collect();
                if f.len() == 3 {
                    if let (Some((w, h)), Ok(r)) = (parse_size(f[1]), f[2].parse::<f64>()) {
                        targets.push((f[0].to_string(), w, h, r));
                    }
                }
            }
            if !targets.is_empty() {
                apply(&mut state, &manager, &qh, &mut queue, &targets);
            }
            0
        }
        None => {
            let r = rows(&state, true);
            if r.is_empty() {
                notify("Display", "No monitor found", true);
                std::process::exit(1);
            }
            let mut outs: Vec<String> = Vec::new();
            for x in &r {
                if !outs.contains(&x.out) {
                    outs.push(x.out.clone());
                }
            }
            let multi = outs.len() > 1;
            // one list: every resolution and refresh rate (monitor name first when there are several)
            let items: Vec<String> = r
                .iter()
                .map(|x| {
                    let mode = format!("{}x{}  {:.2} Hz{}", x.w, x.h, x.refresh as f64 / 1000.0, if x.cur { "  *" } else { "" });
                    if multi { format!("{}  {}", x.out, mode) } else { mode }
                })
                .collect();
            match pick("Display>", &items) {
                None => 0,
                Some(c) => {
                    let mut f: Vec<&str> = c.split_whitespace().collect();
                    let out = if multi {
                        if f.is_empty() { String::new() } else { f.remove(0).to_string() }
                    } else {
                        outs[0].clone()
                    };
                    let ok = f.len() >= 2
                        && parse_size(f[0])
                            .map(|(w, h)| apply(&mut state, &manager, &qh, &mut queue, &[(out.clone(), w, h, f[1].parse().unwrap_or(0.0))]))
                            .unwrap_or(false);
                    if ok {
                        save(&out, f[0], f[1]);
                        notify("Display", &format!("{} {} {} Hz", out, f[0], f[1]), false);
                        0
                    } else {
                        notify("Display", &format!("Failed: {}", c), true);
                        1
                    }
                }
            }
        }
        _ => usage(),
    };
    std::process::exit(code);
}
