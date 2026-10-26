use std::path::Path;
use std::process::{Command, Stdio};

const AUDIO_EXT: [&str; 14] = [
    "mp3", "flac", "ogg", "oga", "opus", "m4a", "aac", "wav", "wma", "aiff", "aif", "ape", "mka", "wv",
];
const VIDEO_EXT: [&str; 14] = [
    "mp4", "mkv", "webm", "avi", "mov", "wmv", "flv", "m4v", "mpg", "mpeg", "ts", "m2ts", "ogv", "3gp",
];

pub fn ext_in(path: &str, list: &[&str]) -> bool {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| list.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

pub struct Playlist {
    pub files: Vec<String>,
    pub ord: Vec<usize>,
    pub cur: usize,
    pub shuffle: bool,
    pub repeat: bool,
}

impl Playlist {
    pub fn new(shuffle: bool, repeat: bool) -> Self {
        Playlist { files: Vec::new(), ord: Vec::new(), cur: 0, shuffle, repeat }
    }

    pub fn cur_file(&self) -> &str {
        &self.files[self.ord[self.cur]]
    }

    pub fn add_path(&mut self, p: &str) {
        if Path::new(p).is_dir() {
            self.add_dir(Path::new(p), 4);
        } else {
            self.files.push(p.to_string());
        }
    }

    fn add_dir(&mut self, dir: &Path, depth: u32) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        let mut paths: Vec<_> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or(".");
                let s = p.to_string_lossy();
                !name.starts_with('.') && (p.is_dir() || ext_in(&s, &AUDIO_EXT) || ext_in(&s, &VIDEO_EXT))
            })
            .collect();
        paths.sort();
        for p in paths {
            if p.is_dir() {
                if depth > 0 {
                    self.add_dir(&p, depth - 1);
                }
            } else {
                self.files.push(p.to_string_lossy().into_owned());
            }
        }
    }

    pub fn build_order(&mut self) {
        let n = self.files.len();
        let current = self.ord.get(self.cur).copied();
        self.ord = (0..n).collect();
        if self.shuffle {
            let mut x = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(88172645463325252)
                | 1;
            for i in (1..n).rev() {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let j = (x % (i as u64 + 1)) as usize;
                self.ord.swap(i, j);
            }
        }
        self.cur = match current {
            Some(c) => self.ord.iter().position(|&o| o == c).unwrap_or(0),
            None => 0,
        };
    }

    /// Move by `step` tracks; false when the end is reached without repeat.
    pub fn advance(&mut self, step: i64) -> bool {
        let n = self.files.len() as i64;
        let next = self.cur as i64 + step;
        if next >= 0 && next < n {
            self.cur = next as usize;
            true
        } else if self.repeat {
            self.cur = next.rem_euclid(n) as usize;
            true
        } else {
            false
        }
    }
}

pub fn run_capture(prog: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(prog).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// True when the file has no real video stream (cover art does not count).
pub fn audio_only(path: &str) -> bool {
    if ext_in(path, &AUDIO_EXT) {
        return true;
    }
    if ext_in(path, &VIDEO_EXT) {
        return false;
    }
    let Some(out) = run_capture(
        "ffprobe",
        &[
            "-v", "error", "-select_streams", "v", "-show_entries",
            "stream=codec_name:stream_disposition=attached_pic", "-of", "csv=p=0", path,
        ],
    ) else {
        return false;
    };
    let real_video = out.lines().any(|l| l.rsplit(',').next() != Some("1") && !l.is_empty());
    !real_video
}

pub fn fmt_time(secs: f64) -> String {
    let t = secs.max(0.0) as i64;
    if t >= 3600 {
        format!("{}:{:02}:{:02}", t / 3600, (t / 60) % 60, t % 60)
    } else {
        format!("{}:{:02}", t / 60, t % 60)
    }
}

pub fn kill_child(child: &mut Option<std::process::Child>) {
    if let Some(mut c) = child.take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}
