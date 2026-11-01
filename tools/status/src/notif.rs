//! org.freedesktop.Notifications server (runs on its own thread, forwards to the bar).

use std::collections::HashMap;
use std::sync::Mutex;

use smithay_client_toolkit::reexports::calloop::channel::Sender;
use zbus::zvariant::{OwnedValue, Value};

pub enum IconSrc {
    Path(String),
    Raw { data: Vec<u8>, w: i32, h: i32, stride: i32, ch: i32 },
}

pub struct Notif {
    pub text: String,
    pub secs: i64,
    pub icon: Option<IconSrc>,
}

/// Append a line to ~/.local/state/status.log (what the notification daemon saw).
fn log(msg: &str) {
    use std::io::Write;
    let dir = format!("{}/.local/state", std::env::var("HOME").unwrap_or_default());
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(format!("{}/status.log", dir)) {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "{} {}", t, msg);
    }
}

struct Notifier {
    tx: Mutex<Sender<Notif>>,
    next: u32,
}

fn clean(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if in_tag => {}
            '\n' | '\r' | '\t' => out.push(' '),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out.trim().to_string()
}

fn icon_from_hints(hints: &HashMap<String, OwnedValue>, app_icon: &str) -> Option<IconSrc> {
    for key in ["image-data", "image_data", "icon_data"] {
        if let Some(v) = hints.get(key) {
            if let Value::Structure(s) = &**v {
                let f = s.fields();
                if f.len() >= 7 {
                    let int = |v: &Value| if let Value::I32(i) = v { Some(*i) } else { None };
                    let (w, h, stride) = (int(&f[0])?, int(&f[1])?, int(&f[2])?);
                    let ch = int(&f[5])?;
                    if let Value::Array(a) = &f[6] {
                        let data: Vec<u8> = a
                            .iter()
                            .filter_map(|x| if let Value::U8(b) = x { Some(*b) } else { None })
                            .collect();
                        return Some(IconSrc::Raw { data, w, h, stride, ch });
                    }
                }
            }
        }
    }
    for key in ["image-path", "image_path"] {
        if let Some(v) = hints.get(key) {
            if let Value::Str(s) = &**v {
                return Some(IconSrc::Path(s.to_string()));
            }
        }
    }
    if app_icon.starts_with('/') || app_icon.starts_with("file://") {
        return Some(IconSrc::Path(app_icon.to_string()));
    }
    None
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Notifier {
    fn get_capabilities(&self) -> Vec<String> {
        log("GetCapabilities");
        vec!["body".to_string(), "body-markup".to_string(), "icon-static".to_string()]
    }

    #[allow(clippy::too_many_arguments)]
    fn notify(
        &mut self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        log(&format!("Notify app={} summary={:?} expire={}", app_name, summary, expire_timeout));
        let _ = actions;
        let id = if replaces_id != 0 {
            replaces_id
        } else {
            self.next = self.next.wrapping_add(1).max(1);
            self.next
        };
        let s = clean(&summary);
        let b = clean(&body);
        let text = if b.is_empty() { s } else if s.is_empty() { b } else { format!("{}: {}", s, b) };
        let secs = match expire_timeout {
            t if t > 0 => ((t as i64 + 999) / 1000).min(15),
            0 => 10,
            _ => 5,
        };
        if !text.is_empty() {
            let icon = icon_from_hints(&hints, &app_icon);
            if let Ok(tx) = self.tx.lock() {
                let _ = tx.send(Notif { text, secs, icon });
            }
        }
        id
    }

    fn close_notification(&self, _id: u32) {}

    fn get_server_information(&self) -> (String, String, String, String) {
        log("GetServerInformation");
        ("status".to_string(), "void".to_string(), "0.1".to_string(), "1.2".to_string())
    }
}

pub fn spawn(tx: Sender<Notif>) {
    std::thread::spawn(move || {
        let n = Notifier { tx: Mutex::new(tx), next: 0 };
        let built = zbus::blocking::connection::Builder::session()
            .and_then(|b| b.name("org.freedesktop.Notifications"))
            .and_then(|b| b.serve_at("/org/freedesktop/Notifications", n))
            .and_then(|b| b.build());
        match built {
            Ok(_conn) => {
                log("notification daemon running (org.freedesktop.Notifications)");
                loop {
                    std::thread::park();
                }
            }
            Err(e) => {
                log(&format!("notification daemon NOT running: {}", e));
                eprintln!("status: notification daemon disabled: {}", e);
            }
        }
    });
}
