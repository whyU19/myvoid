use std::process::{Child, Command, Stdio};
use std::time::Instant;

use crate::common::{fmt_time, kill_child, run_capture, Playlist};

struct Term {
    old: libc::termios,
}

impl Term {
    fn raw() -> Option<Term> {
        unsafe {
            if libc::isatty(0) == 0 {
                return None;
            }
            let mut old: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut old) != 0 {
                return None;
            }
            let mut raw = old;
            raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
            raw.c_cc[libc::VMIN] = 0;
            raw.c_cc[libc::VTIME] = 0;
            libc::tcsetattr(0, libc::TCSANOW, &raw);
            Some(Term { old })
        }
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.old);
        }
    }
}

fn cols() -> usize {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            return ws.ws_col as usize;
        }
    }
    80
}

struct Player {
    pl: Playlist,
    child: Option<Child>,
    started: Option<Instant>,
    base: f64,
    paused: bool,
    dur: f64,
    title: String,
    artist: String,
}

impl Player {
    fn pos(&self) -> f64 {
        self.base + self.started.map(|s| s.elapsed().as_secs_f64()).unwrap_or(0.0)
    }

    fn probe(&mut self) {
        let f = self.pl.cur_file().to_string();
        self.dur = 0.0;
        self.artist.clear();
        self.title = f.rsplit('/').next().unwrap_or(&f).to_string();
        if let Some(out) = run_capture(
            "ffprobe",
            &["-v", "error", "-show_entries", "format=duration:format_tags=title,artist", "-of", "default=nw=1", &f],
        ) {
            for l in out.lines() {
                if let Some((k, v)) = l.split_once('=') {
                    let k = k.to_lowercase();
                    if k == "duration" {
                        self.dur = v.parse().unwrap_or(0.0);
                    } else if k.ends_with("title") && !v.is_empty() {
                        self.title = v.to_string();
                    } else if k.ends_with("artist") {
                        self.artist = v.to_string();
                    }
                }
            }
        }
    }

    fn start(&mut self, pos: f64) {
        kill_child(&mut self.child);
        let ss = format!("{:.3}", pos.max(0.0));
        self.child = Command::new("ffmpeg")
            .args(["-v", "quiet", "-nostdin", "-ss", &ss, "-i", self.pl.cur_file(), "-vn", "-f", "pulse", "default"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok();
        self.base = pos.max(0.0);
        self.started = Some(Instant::now());
        self.paused = false;
    }

    fn track(&mut self, step: i64) -> bool {
        if !self.pl.advance(step) {
            return false;
        }
        self.probe();
        self.start(0.0);
        true
    }

    fn seek(&mut self, d: f64) {
        let mut p = (self.pos() + d).max(0.0);
        if self.dur > 0.0 && p > self.dur - 1.0 {
            p = (self.dur - 1.0).max(0.0);
        }
        self.start(p);
    }

    fn toggle_pause(&mut self) {
        if self.paused {
            let b = self.base;
            self.start(b);
        } else {
            self.base = self.pos();
            self.started = None;
            kill_child(&mut self.child);
            self.paused = true;
        }
    }

    fn ui(&self) {
        let mut flags = String::new();
        if self.pl.shuffle {
            flags.push_str(" shuffle");
        }
        if self.pl.repeat {
            flags.push_str(" repeat");
        }
        let who = if self.artist.is_empty() { self.title.clone() } else { format!("{} - {}", self.artist, self.title) };
        let tail = format!("  {} / {}{}", fmt_time(self.pos()), if self.dur > 0.0 { fmt_time(self.dur) } else { "?".into() }, flags);
        let head = format!("{} {}/{}  ", if self.paused { "||" } else { ">" }, self.pl.cur + 1, self.pl.files.len());
        let room = cols().saturating_sub(head.chars().count() + tail.chars().count() + 1);
        let who: String = who.chars().take(room).collect();
        print!("\r\x1b[K{}{}{}", head, who, tail);
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
}

fn poll_stdin(ms: i32) -> Vec<u8> {
    unsafe {
        let mut p = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
        if libc::poll(&mut p, 1, ms) > 0 && p.revents & libc::POLLIN != 0 {
            let mut buf = [0u8; 16];
            let n = libc::read(0, buf.as_mut_ptr() as *mut libc::c_void, buf.len());
            if n > 0 {
                return buf[..n as usize].to_vec();
            }
        }
    }
    Vec::new()
}

pub fn run(pl: Playlist) -> i32 {
    let tty = Term::raw();
    let mut p = Player { pl, child: None, started: None, base: 0.0, paused: false, dur: 0.0, title: String::new(), artist: String::new() };
    p.probe();
    p.start(0.0);
    let mut fails = 0usize;
    let mut last_ui = Instant::now();
    let mut code = 0;
    'outer: loop {
        let keys = if tty.is_some() { poll_stdin(300) } else { std::thread::sleep(std::time::Duration::from_millis(300)); Vec::new() };
        let mut i = 0;
        while i < keys.len() {
            match keys[i] {
                b'q' | 3 => break 'outer,
                b' ' => p.toggle_pause(),
                b'n' | b'>' => {
                    if !p.track(1) {
                        break 'outer;
                    }
                }
                b'p' | b'<' => {
                    p.track(-1);
                }
                b's' => {
                    p.pl.shuffle = !p.pl.shuffle;
                    p.pl.build_order();
                }
                b'r' => p.pl.repeat = !p.pl.repeat,
                27 if i + 2 < keys.len() && keys[i + 1] == b'[' => {
                    match keys[i + 2] {
                        b'C' => p.seek(10.0),
                        b'D' => p.seek(-10.0),
                        b'A' => p.seek(60.0),
                        b'B' => p.seek(-60.0),
                        _ => {}
                    }
                    i += 2;
                }
                _ => {}
            }
            i += 1;
        }
        if !p.paused {
            let ended = match p.child.as_mut() {
                Some(c) => matches!(c.try_wait(), Ok(Some(_))),
                None => true,
            };
            if ended {
                let el = p.started.map(|s| s.elapsed().as_secs_f64()).unwrap_or(0.0);
                p.child = None;
                if el < 1.5 && p.base < 0.1 {
                    fails += 1;
                    if fails >= p.pl.files.len() {
                        eprintln!("\nplay: audio output failed");
                        code = 1;
                        break;
                    }
                } else {
                    fails = 0;
                }
                if !p.track(1) {
                    break;
                }
            }
        }
        if tty.is_some() && last_ui.elapsed().as_secs_f64() >= 0.25 {
            p.ui();
            last_ui = Instant::now();
        }
    }
    kill_child(&mut p.child);
    drop(tty);
    println!();
    code
}
