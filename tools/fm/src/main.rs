// fm: minimal terminal file manager (everything compiled in, config below).
// Picker mode for other programs: fm -f|-m|-d OUTFILE [dir]  (file / files / directory;
// the choice is written to OUTFILE, exit status 1 when cancelled).

use std::env;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use app::{App, Outcome, PickMode};
use term::Terminal;

fn main() {
    match run() {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(e) => {
            eprintln!("fm: {e}");
            std::process::exit(1);
        }
    }
}

const USAGE: &str = "usage: fm [-f|-m|-d OUTFILE] [directory|file]";

/// Ok(true) normally or when something was chosen; Ok(false) when a picker was cancelled.
fn run() -> io::Result<bool> {
    let mut args = env::args_os().skip(1).peekable();
    let mut pick = PickMode::Off;
    let mut out_file: Option<PathBuf> = None;
    while let Some(arg) = args.peek() {
        let mode = match arg.to_str() {
            Some("-f") => PickMode::File,
            Some("-m") => PickMode::Files,
            Some("-d") => PickMode::Dir,
            _ => break,
        };
        args.next();
        pick = mode;
        out_file = Some(
            args.next()
                .map(PathBuf::from)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, USAGE))?,
        );
    }
    let target = args.next().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    if args.next().is_some() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, USAGE));
    }

    let target = target.canonicalize()?;
    let (cwd, select) = if target.is_dir() {
        (target, None)
    } else {
        let parent = target.parent().unwrap_or_else(|| Path::new("/")).to_path_buf();
        let name = target.file_name().map(|name| name.to_string_lossy().into_owned());
        (parent, name)
    };

    let mut term = Terminal::new()?;
    term.enter()?;
    let mut app = App::new(cwd, select, pick)?;
    let mut picked = false;
    let mut reader = keys::Reader::new();
    let mut size = term.size();
    let mut dirty = true;

    while !app.quit {
        if dirty {
            ui::draw(&mut app, size)?;
            dirty = false;
        }

        let pressed = match reader.read(libc::STDIN_FILENO, Duration::from_millis(100)) {
            Ok(keys) => keys,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        };

        for key in pressed {
            let page = size.1.saturating_sub(2) as usize;
            match app.handle_key(key, page) {
                Outcome::None => {}
                Outcome::Quit => app.quit = true,
                Outcome::Pick(paths) => {
                    if let Some(file) = &out_file {
                        let mut data: Vec<u8> = Vec::new();
                        for (i, p) in paths.iter().enumerate() {
                            if i > 0 {
                                data.push(b'\n');
                            }
                            data.extend_from_slice(p.as_os_str().as_bytes());
                        }
                        match std::fs::write(file, data) {
                            Ok(()) => {
                                picked = true;
                                app.quit = true;
                            }
                            Err(e) => app.set_error(format!("write: {e}")),
                        }
                    }
                }
                Outcome::Open(path) => {
                    if let Err(e) = open(&mut term, &path) {
                        app.set_error(format!("open: {e}"));
                    }
                    app.preview_cache = None;
                }
                Outcome::Edit(path) => {
                    if let Err(e) = edit(&mut term, &path) {
                        app.set_error(format!("edit: {e}"));
                    }
                    app.preview_cache = None;
                }
            }

            dirty = true;
            if app.quit {
                break;
            }
        }

        let resized = term.size();
        if resized != size {
            size = resized;
            dirty = true;
        }
    }

    Ok(pick == PickMode::Off || picked)
}

/// Edit with $EDITOR (nvim by default) in this terminal.
fn edit(term: &mut Terminal, path: &Path) -> io::Result<()> {
    term.suspend()?;
    let result = Command::new("sh")
        .arg("-c")
        .arg("${EDITOR:-nvim} \"$1\"")
        .arg("fm-edit")
        .arg(path)
        .status()
        .map(|_| ());
    term.resume()?;
    result
}

fn open(term: &mut Terminal, path: &Path) -> io::Result<()> {
    let mut command = if config::OPENER.is_empty() {
        Command::new(if cfg!(target_os = "macos") { "open" } else { "xdg-open" })
    } else {
        let mut command = Command::new(config::OPENER[0]);
        command.args(&config::OPENER[1..]);
        command
    };
    command.arg(path);

    if !config::OPEN_WAIT {
        command.stdin(Stdio::null());
        command.stdout(Stdio::null());
        command.stderr(Stdio::null());
    }

    if config::OPEN_WAIT {
        term.suspend()?;
        let result = command.status().map(|_| ());
        term.resume()?;
        result
    } else {
        command.spawn().map(|_| ())
    }
}

mod config {
    // fm configuration. Edit this file, then rebuild with `make`.
    //
    // This is the equivalent of suckless' config.h: everything is compiled in.

    use crate::app::{Entry, Sort};
    use crate::keys::{Action, Key};

    // Behavior

    pub const SHOW_HIDDEN: bool = false;
    pub const DIRS_FIRST: bool = true;
    pub const SORT: Sort = Sort::Name;
    pub const PREVIEW: bool = true;
    pub const PREVIEW_MIN_WIDTH: u16 = 72;
    pub const PREVIEW_BYTES: usize = 256 * 1024;

    // Opener: empty uses `xdg-open` (Linux) or `open` (macOS).
    // For a terminal editor: &["nvim"] with OPEN_WAIT = true.
    pub const OPENER: &[&str] = &["sh", "-c", OPEN_SCRIPT, "fm-open"];
    pub const OPEN_WAIT: bool = true;

    // Run by OPENER with the file as $1: images and video open in their own
    // Wayland window (view / play), everything else in the terminal.
    const OPEN_SCRIPT: &str = r#"
    f=$1
    case "$(printf '%s' "${f##*.}" | tr 'A-Z' 'a-z')" in
      png|jpg|jpeg|webp|gif|bmp|jfif) setsid -f view "$f" >/dev/null 2>&1 ;;
      mp4|mkv|webm|avi|mov|wmv|flv|m4v|mpg|mpeg|ogv) setsid -f play "$f" >/dev/null 2>&1 ;;
      mp3|flac|ogg|oga|opus|m4a|aac|wav|wma|aiff|ape|mka) play "$f" ;;
      *) ${EDITOR:-nvim} "$f" ;;
    esac
    "#;

    // Rendering (ANSI SGR parameters)

    pub const STYLE_HEADER: &str = "1";
    pub const STYLE_STATUS: &str = "30;46";
    pub const STYLE_PROMPT: &str = "30;46";
    pub const STYLE_ERROR: &str = "31";
    pub const STYLE_DIR: &str = "1;34";
    pub const STYLE_LINK: &str = "36";
    pub const STYLE_SELECTED: &str = "7";
    pub const STYLE_SELECTED_DIR: &str = "1;7";
    pub const STYLE_BORDER: &str = "90";
    pub const STYLE_PREVIEW_HEAD: &str = "1";
    pub const STYLE_DIM: &str = "90";
    pub const STYLE_MARK: &str = "35";

    pub const HINTS: &str =
        "j/k move  h/l back/open  e edit  / filter  . hidden  s sort  p preview  n/N new  r rename  d delete  q quit";

    pub const HINTS_PICK: &str =
        "j/k move  h/l back/open  enter/o choose  space mark  / find  . hidden  s sort  q cancel";

    // Key bindings. Remove or change entries freely.

    pub const BINDINGS: &[(Key, Action)] = &[
        (Key::Char('q'), Action::Quit),
        (Key::Ctrl('c'), Action::Quit),
        (Key::Char('j'), Action::Down),
        (Key::Down, Action::Down),
        (Key::Char('k'), Action::Up),
        (Key::Up, Action::Up),
        (Key::Char('h'), Action::Parent),
        (Key::Left, Action::Parent),
        (Key::Char('l'), Action::Open),
        (Key::Right, Action::Open),
        (Key::Enter, Action::Open),
        (Key::Char('g'), Action::Top),
        (Key::Home, Action::Top),
        (Key::Char('G'), Action::Bottom),
        (Key::End, Action::Bottom),
        (Key::Ctrl('d'), Action::HalfDown),
        (Key::Ctrl('u'), Action::HalfUp),
        (Key::PageDown, Action::PageDown),
        (Key::PageUp, Action::PageUp),
        (Key::Char('.'), Action::ToggleHidden),
        (Key::Char('s'), Action::Sort),
        (Key::Char('P'), Action::TogglePreview),
        (Key::Char('/'), Action::Search),
        (Key::Char('n'), Action::NewFile),
        (Key::Char('N'), Action::NewDir),
        (Key::Char('r'), Action::Rename),
        (Key::Char('d'), Action::Delete),
        (Key::Char('R'), Action::Refresh),
        (Key::Ctrl('r'), Action::Refresh),
        (Key::Esc, Action::ClearFilter),
        (Key::Char(' '), Action::Mark),
        (Key::Char('o'), Action::Pick),
        (Key::Char('e'), Action::Edit),
        (Key::Char('a'), Action::SelectAll),
        (Key::Char('u'), Action::ClearMarks),
        (Key::Char('y'), Action::Copy),
        (Key::Char('x'), Action::Cut),
        (Key::Char('p'), Action::Paste),
        (Key::Char('z'), Action::Archive),
        (Key::Char('X'), Action::Extract),
        (Key::Char('f'), Action::Find),
        (Key::Char('?'), Action::Help),
    ];

    pub const STYLE_INFO: &str = "30;42";

    /// Shown by `?`.
    pub const HELP: &[&str] = &[
        " fm keys",
        "",
        " move        j/k up/down   h/l back/open   g/G top/bottom   ctrl-d/u half page",
        " select      space select/unselect (moves down)   a select all   u unselect all",
        " copy/move   y copy   x move (cut)   p paste in this folder",
        "             if a name already exists you are asked: overwrite / skip / rename",
        " delete      d delete the selected items (asks y/N)",
        " rename      r rename   n new file   N new folder",
        " archive     z   then: z zip   g tar.gz   x tar.xz   s tar.zst   b tar.bz2   t tar",
        " extract     X   then: h here   f into a new folder named after the archive",
        "             (zip, tar, tar.gz/xz/zst/bz2, gz, xz, zst, bz2, 7z, rar)",
        " find        f search in ALL subfolders (empty = list everything); enter = go there",
        " filter      / filter this folder   esc clear",
        " view        e edit with $EDITOR   P preview on/off   . hidden   s sort   R refresh",
        " preview     text and code in colour (bat), folders (eza), images and PDF (chafa,",
        "             pdftoppm), archive contents",
        " quit        q",
        "",
        " press any key to close",
    ];

    /// ANSI colour per file type, like eza: LS_COLORS wins when set, otherwise
    /// built-in defaults (directories blue, executables green, symlinks cyan,
    /// archives red, images/video magenta, audio cyan, documents yellow, code green).
    pub fn entry_style(e: &Entry) -> String {
        use std::collections::HashMap;
        use std::sync::OnceLock;
        static LS: OnceLock<HashMap<String, String>> = OnceLock::new();
        let ls = LS.get_or_init(|| {
            let mut m = HashMap::new();
            if let Ok(v) = std::env::var("LS_COLORS") {
                for part in v.split(':') {
                    if let Some((k, val)) = part.split_once('=') {
                        m.insert(k.to_string(), val.to_string());
                    }
                }
            }
            m
        });
        if e.is_dir {
            return ls.get("di").cloned().unwrap_or_else(|| "1;34".into());
        }
        if e.is_symlink {
            return ls.get("ln").cloned().unwrap_or_else(|| "36".into());
        }
        if e.exec {
            return ls.get("ex").cloned().unwrap_or_else(|| "1;32".into());
        }
        let ext = e.name.rsplit_once('.').filter(|(base, _)| !base.is_empty()).map(|(_, x)| x.to_lowercase());
        if let Some(ext) = &ext {
            if let Some(v) = ls.get(&format!("*.{ext}")) {
                return v.clone();
            }
            let group = match ext.as_str() {
                "zip" | "tar" | "gz" | "tgz" | "xz" | "bz2" | "zst" | "7z" | "rar" | "deb" | "rpm" | "iso" | "jar" | "apk" => "1;31",
                "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg" | "ico" | "tiff" | "xcf" | "jfif" => "35",
                "mp4" | "mkv" | "webm" | "avi" | "mov" | "flv" | "wmv" | "m4v" | "mpg" | "mpeg" | "ogv" => "1;35",
                "mp3" | "flac" | "ogg" | "oga" | "opus" | "wav" | "m4a" | "aac" | "wma" | "mka" => "36",
                "pdf" | "doc" | "docx" | "odt" | "xls" | "xlsx" | "ods" | "ppt" | "pptx" | "epub" | "rtf" | "txt" | "md" => "33",
                "rs" | "c" | "h" | "cpp" | "hpp" | "py" | "js" | "ts" | "go" | "lua" | "sh" | "bash" | "java" | "rb" | "php" | "zig" => "92",
                "toml" | "yaml" | "yml" | "json" | "ini" | "conf" | "cfg" | "lock" => "38;5;180",
                "log" | "tmp" | "bak" | "swp" | "old" => "90",
                _ => "",
            };
            if !group.is_empty() {
                return group.to_string();
            }
        }
        if e.name.starts_with('.') {
            return "90".into();
        }
        ls.get("fi").cloned().unwrap_or_default()
    }
}

mod keys {
    use std::io;
    use std::os::fd::RawFd;
    use std::time::Duration;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Key {
        Char(char),
        Ctrl(char),
        Enter,
        Esc,
        Backspace,
        Tab,
        BackTab,
        Up,
        Down,
        Left,
        Right,
        Home,
        End,
        PageUp,
        PageDown,
        Delete,
        Insert,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Action {
        Quit,
        Up,
        Down,
        Top,
        Bottom,
        HalfUp,
        HalfDown,
        PageUp,
        PageDown,
        Open,
        Parent,
        ToggleHidden,
        Sort,
        TogglePreview,
        Refresh,
        Search,
        NewFile,
        NewDir,
        Rename,
        Delete,
        ClearFilter,
        Mark,
        Pick,
        Edit,
        SelectAll,
        ClearMarks,
        Copy,
        Cut,
        Paste,
        Archive,
        Extract,
        Find,
        Help,
    }

    pub struct Reader {
        buf: Vec<u8>,
    }

    impl Reader {
        pub fn new() -> Self {
            Self { buf: Vec::with_capacity(64) }
        }

        pub fn read(&mut self, fd: RawFd, timeout: Duration) -> io::Result<Vec<Key>> {
            let mut keys = Vec::new();

            if !wait_fd(fd, timeout)? {
                return Ok(keys);
            }
            self.read_available(fd)?;

            loop {
                let before = self.buf.len();
                parse(&mut self.buf, &mut keys);
                if self.buf.is_empty() {
                    break;
                }
                if !wait_fd(fd, Duration::from_millis(15))? {
                    break;
                }
                self.read_available(fd)?;
                if self.buf.len() == before {
                    break;
                }
            }

            if !self.buf.is_empty() {
                if self.buf.len() == 1 && self.buf[0] == 0x1b {
                    keys.push(Key::Esc);
                }
                self.buf.clear();
            }

            Ok(keys)
        }

        fn read_available(&mut self, fd: RawFd) -> io::Result<()> {
            let mut tmp = [0u8; 256];

            loop {
                let n = unsafe { libc::read(fd, tmp.as_mut_ptr().cast(), tmp.len()) };
                if n < 0 {
                    let err = io::Error::last_os_error();
                    match err.kind() {
                        io::ErrorKind::Interrupted => continue,
                        io::ErrorKind::WouldBlock => return Ok(()),
                        _ => return Err(err),
                    }
                }
                if n == 0 {
                    return Ok(());
                }

                self.buf.extend_from_slice(&tmp[..n as usize]);
                if (n as usize) < tmp.len() {
                    return Ok(());
                }
            }
        }
    }

    fn wait_fd(fd: RawFd, timeout: Duration) -> io::Result<bool> {
        let mut pollfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        let millis = timeout.as_millis().min(i32::MAX as u128) as i32;

        loop {
            let rc = unsafe { libc::poll(&mut pollfd, 1, millis) };
            if rc > 0 {
                if pollfd.revents & libc::POLLIN != 0 {
                    return Ok(true);
                }
                if pollfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "terminal closed"));
                }
                return Ok(false);
            }
            if rc == 0 {
                return Ok(false);
            }

            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }

    fn parse(buf: &mut Vec<u8>, keys: &mut Vec<Key>) {
        while !buf.is_empty() {
            match parse_one(buf) {
                Ok(Some((key, len))) => {
                    keys.extend(key);
                    buf.drain(..len);
                }
                Ok(None) => return,
                Err(()) => {
                    buf.drain(..1);
                }
            }
        }
    }

    fn parse_one(buf: &[u8]) -> Result<Option<(Option<Key>, usize)>, ()> {
        let byte = buf[0];

        match byte {
            0x1b => {
                if buf.len() < 2 {
                    return Ok(None);
                }
                match buf[1] {
                    b'[' => parse_csi(buf),
                    b'O' => {
                        if buf.len() < 3 {
                            return Ok(None);
                        }
                        let key = match buf[2] {
                            b'A' => Some(Key::Up),
                            b'B' => Some(Key::Down),
                            b'C' => Some(Key::Right),
                            b'D' => Some(Key::Left),
                            b'H' => Some(Key::Home),
                            b'F' => Some(Key::End),
                            _ => None,
                        };
                        Ok(Some((key, 3)))
                    }
                    _ => Ok(Some((None, 2))),
                }
            }
            0x0d | 0x0a => Ok(Some((Some(Key::Enter), 1))),
            0x7f | 0x08 => Ok(Some((Some(Key::Backspace), 1))),
            0x09 => Ok(Some((Some(Key::Tab), 1))),
            0x01..=0x1a => {
                let c = (b'a' + byte - 1) as char;
                Ok(Some((Some(Key::Ctrl(c)), 1)))
            }
            0x1c..=0x1f => Ok(Some((None, 1))),
            _ => parse_utf8(buf),
        }
    }

    fn parse_csi(buf: &[u8]) -> Result<Option<(Option<Key>, usize)>, ()> {
        let mut i = 2;

        while i < buf.len() {
            let byte = buf[i];
            if (0x40..=0x7e).contains(&byte) {
                let key = match byte {
                    b'A' => Some(Key::Up),
                    b'B' => Some(Key::Down),
                    b'C' => Some(Key::Right),
                    b'D' => Some(Key::Left),
                    b'H' => Some(Key::Home),
                    b'F' => Some(Key::End),
                    b'Z' => Some(Key::BackTab),
                    b'~' => tilde(&buf[2..i]),
                    _ => None,
                };
                return Ok(Some((key, i + 1)));
            }
            i += 1;
        }

        Ok(None)
    }

    fn tilde(params: &[u8]) -> Option<Key> {
        let first = params.split(|&b| b == b';').next()?;
        let code = std::str::from_utf8(first).ok()?.parse::<u8>().ok()?;

        Some(match code {
            1 | 7 => Key::Home,
            2 => Key::Insert,
            3 => Key::Delete,
            4 | 8 => Key::End,
            5 => Key::PageUp,
            6 => Key::PageDown,
            _ => return None,
        })
    }

    fn parse_utf8(buf: &[u8]) -> Result<Option<(Option<Key>, usize)>, ()> {
        let byte = buf[0];
        let len = match byte {
            0x00..=0x7f => 1,
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => return Err(()),
        };

        if buf.len() < len {
            return Ok(None);
        }

        let text = std::str::from_utf8(&buf[..len]).map_err(|_| ())?;
        let c = text.chars().next().ok_or(())?;
        Ok(Some((Some(Key::Char(c)), len)))
    }
}

mod term {
    use std::io::{self, Write};

    pub struct Terminal {
        orig: libc::termios,
        active: bool,
    }

    impl Terminal {
        pub fn new() -> io::Result<Self> {
            let stdin = unsafe { libc::isatty(libc::STDIN_FILENO) };
            let stdout = unsafe { libc::isatty(libc::STDOUT_FILENO) };
            if stdin != 1 || stdout != 1 {
                return Err(io::Error::new(io::ErrorKind::NotConnected, "not a terminal"));
            }

            let mut orig: libc::termios = unsafe { std::mem::zeroed() };
            if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut orig) } != 0 {
                return Err(io::Error::last_os_error());
            }

            Ok(Self { orig, active: false })
        }

        pub fn enter(&mut self) -> io::Result<()> {
            self.raw()?;
            self.active = true;
            write_out("\x1b[?1049h\x1b[2J\x1b[H\x1b[?25l")
        }

        pub fn suspend(&mut self) -> io::Result<()> {
            if !self.active {
                return Ok(());
            }

            write_out("\x1b[0m\x1b[?25h\x1b[?1049l")?;
            self.restore()?;
            self.active = false;
            Ok(())
        }

        pub fn resume(&mut self) -> io::Result<()> {
            if self.active {
                return Ok(());
            }

            self.raw()?;
            self.active = true;
            write_out("\x1b[?1049h\x1b[2J\x1b[H\x1b[?25l")
        }

        pub fn size(&self) -> (u16, u16) {
            let mut size: libc::winsize = unsafe { std::mem::zeroed() };
            let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) };

            if rc == 0 && size.ws_col > 0 && size.ws_row > 0 {
                (size.ws_col, size.ws_row)
            } else {
                (80, 24)
            }
        }

        fn raw(&self) -> io::Result<()> {
            let mut raw = self.orig;

            raw.c_iflag &= !(libc::IGNBRK
                | libc::BRKINT
                | libc::PARMRK
                | libc::ISTRIP
                | libc::INLCR
                | libc::IGNCR
                | libc::ICRNL
                | libc::IXON);
            raw.c_oflag &= !libc::OPOST;
            raw.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG | libc::IEXTEN);
            raw.c_cflag &= !(libc::CSIZE | libc::PARENB);
            raw.c_cflag |= libc::CS8;
            raw.c_cc[libc::VMIN] = 0;
            raw.c_cc[libc::VTIME] = 0;

            set_termios(&raw)
        }

        fn restore(&self) -> io::Result<()> {
            set_termios(&self.orig)
        }
    }

    impl Drop for Terminal {
        fn drop(&mut self) {
            let _ = self.suspend();
        }
    }

    fn set_termios(termios: &libc::termios) -> io::Result<()> {
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, termios) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn write_out(data: &str) -> io::Result<()> {
        let mut stdout = io::stdout().lock();
        stdout.write_all(data.as_bytes())?;
        stdout.flush()
    }
}

mod app {
    use std::collections::VecDeque;
    use std::ffi::OsString;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::SystemTime;

    use crate::config;
    use crate::keys::{Action, Key};

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub enum Sort {
        Name,
        NameDesc,
        Modified,
        Size,
        SizeDesc,
    }

    impl Sort {
        pub fn next(self) -> Self {
            match self {
                Self::Name => Self::NameDesc,
                Self::NameDesc => Self::Modified,
                Self::Modified => Self::Size,
                Self::Size => Self::SizeDesc,
                Self::SizeDesc => Self::Name,
            }
        }

        pub fn label(self) -> &'static str {
            match self {
                Self::Name => "name",
                Self::NameDesc => "name-desc",
                Self::Modified => "mtime",
                Self::Size => "size",
                Self::SizeDesc => "size-desc",
            }
        }
    }

    pub struct Entry {
        pub name: String,
        pub path: PathBuf,
        pub is_dir: bool,
        pub is_symlink: bool,
        pub size: u64,
        pub modified: Option<SystemTime>,
        pub exec: bool,
        pub mode: u32,
    }

    impl Entry {
        pub fn label(&self) -> String {
            let mut label = self.name.clone();
            if self.is_symlink {
                label.push('@');
            }
            label
        }
    }

    pub enum Outcome {
        None,
        Quit,
        Open(PathBuf),
        Pick(Vec<PathBuf>),
        Edit(PathBuf),
    }

    /// Picker mode (used by other programs as a file chooser): the choice is written to a file.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub enum PickMode {
        Off,
        File,
        Files,
        Dir,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub enum PromptKind {
        Search,
        NewFile,
        NewDir,
        Rename,
        ConfirmDelete,
        Find,
    }

    pub struct Prompt {
        pub kind: PromptKind,
        pub label: &'static str,
        pub input: String,
        previous: String,
        pub target: Option<PathBuf>,
    }

    /// A question with single-key answers; the options are always shown in the status bar.
    pub struct Choice {
        pub kind: ChoiceKind,
        pub text: String,
    }

    pub enum ChoiceKind {
        Conflict,
        Archive(Vec<PathBuf>),
        Extract(Vec<PathBuf>),
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Policy {
        Overwrite,
        Skip,
        Rename,
    }

    struct PasteJob {
        items: VecDeque<(PathBuf, PathBuf)>,
        cut: bool,
        policy: Option<Policy>,
        once: Option<Policy>,
        done: usize,
        skipped: usize,
    }

    pub struct FindState {
        pub origin: PathBuf,
        pub pattern: String,
    }

    const FIND_LIMIT: usize = 100_000;

    #[derive(Clone, Copy, PartialEq)]
    pub enum ArchiveKind {
        Zip,
        Tar,
        Gz,
        Xz,
        Zst,
        Bz2,
        Other7z,
    }

    pub fn archive_kind(name: &str) -> Option<ArchiveKind> {
        let l = name.to_lowercase();
        let tar = [".tar", ".tar.gz", ".tgz", ".tar.xz", ".txz", ".tar.zst", ".tzst", ".tar.bz2", ".tbz2", ".tbz"];
        if tar.iter().any(|e| l.ends_with(e)) {
            return Some(ArchiveKind::Tar);
        }
        match l.rsplit_once('.').map(|(_, e)| e) {
            Some("zip") | Some("jar") | Some("apk") => Some(ArchiveKind::Zip),
            Some("gz") => Some(ArchiveKind::Gz),
            Some("xz") => Some(ArchiveKind::Xz),
            Some("zst") => Some(ArchiveKind::Zst),
            Some("bz2") => Some(ArchiveKind::Bz2),
            Some("7z") | Some("rar") => Some(ArchiveKind::Other7z),
            _ => None,
        }
    }

    /// "photos.tar.gz" -> "photos", "notes.txt.gz" -> "notes.txt"
    fn archive_stem(name: &str) -> String {
        let l = name.to_lowercase();
        for e in [".tar.gz", ".tar.xz", ".tar.zst", ".tar.bz2", ".tgz", ".txz", ".tzst", ".tbz2", ".tbz", ".tar"] {
            if l.ends_with(e) {
                return name[..name.len() - e.len()].to_string();
            }
        }
        name.rsplit_once('.').map(|(b, _)| b.to_string()).filter(|b| !b.is_empty()).unwrap_or_else(|| name.to_string())
    }

    fn unique_path(path: &Path) -> PathBuf {
        if fs::symlink_metadata(path).is_err() {
            return path.to_path_buf();
        }
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let (stem, ext) = match name.split_once('.') {
            Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
            _ => (name.clone(), String::new()),
        };
        for n in 1.. {
            let candidate = dir.join(format!("{stem}_{n}{ext}"));
            if fs::symlink_metadata(&candidate).is_err() {
                return candidate;
            }
        }
        unreachable!()
    }

    fn remove_path(path: &Path) -> io::Result<()> {
        match fs::symlink_metadata(path) {
            Ok(m) if m.is_dir() => fs::remove_dir_all(path),
            Ok(_) => fs::remove_file(path),
            Err(_) => Ok(()),
        }
    }

    fn copy_recursive(src: &Path, dst: &Path) -> io::Result<()> {
        let meta = fs::symlink_metadata(src)?;
        if meta.file_type().is_symlink() {
            let target = fs::read_link(src)?;
            std::os::unix::fs::symlink(target, dst)
        } else if meta.is_dir() {
            fs::create_dir_all(dst)?;
            for entry in fs::read_dir(src)? {
                let entry = entry?;
                copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
            }
            Ok(())
        } else {
            fs::copy(src, dst).map(|_| ())
        }
    }

    /// Copy or move one item. Moving across file systems falls back to copy + delete.
    fn transfer(src: &Path, dst: &Path, cut: bool) -> io::Result<()> {
        if src.is_dir() && dst.starts_with(src) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "cannot put a folder inside itself"));
        }
        if cut {
            if fs::rename(src, dst).is_ok() {
                return Ok(());
            }
            copy_recursive(src, dst)?;
            remove_path(src)
        } else {
            copy_recursive(src, dst)
        }
    }

    fn build_entry(name: String, dir_entry: &fs::DirEntry, file_type: fs::FileType) -> Entry {
        let path = dir_entry.path();
        let is_symlink = file_type.is_symlink();
        let is_dir = file_type.is_dir() || (is_symlink && path.is_dir());
        let (size, modified, exec, mode) = match dir_entry.metadata() {
            Ok(metadata) => {
                let mode = std::os::unix::fs::PermissionsExt::mode(&metadata.permissions());
                (metadata.len(), metadata.modified().ok(), !metadata.is_dir() && mode & 0o111 != 0, mode & 0o7777)
            }
            Err(_) => (0, None, false, 0),
        };
        Entry { name, path, is_dir, is_symlink, size, modified, exec, mode }
    }

    fn tool_error(program: &str, e: &io::Error, package: &str) -> String {
        if e.kind() == io::ErrorKind::NotFound {
            format!("{program} is not installed (xbps-install {package})")
        } else {
            format!("{program}: {e}")
        }
    }

    enum Select {
        First,
        Index(usize),
        Name(String),
    }

    pub struct App {
        pub cwd: PathBuf,
        pub entries: Vec<Entry>,
        pub selected: usize,
        pub offset: usize,
        pub show_hidden: bool,
        pub sort: Sort,
        pub preview: bool,
        pub filter: String,
        pub prompt: Option<Prompt>,
        pub message: Option<String>,
        pub quit: bool,
        pub pick: PickMode,
        pub marked: Vec<PathBuf>,
        /// (path, width, height, hidden, lines) of the last tool-rendered preview
        pub preview_cache: Option<(PathBuf, usize, usize, bool, Vec<String>)>,
        /// sixel image for the preview pane (cache and what to draw now)
        pub sixel_cache: Option<(PathBuf, usize, usize, String)>,
        pub sixel_out: Option<String>,
        pub register: Vec<PathBuf>,
        pub register_cut: bool,
        pub choice: Option<Choice>,
        paste_job: Option<PasteJob>,
        pub find: Option<FindState>,
        pub help: bool,
        pub info: bool,
    }

    impl App {
        pub fn new(cwd: PathBuf, select: Option<String>, pick: PickMode) -> io::Result<Self> {
            let mut app = Self {
                cwd,
                entries: Vec::new(),
                selected: 0,
                offset: 0,
                show_hidden: config::SHOW_HIDDEN,
                sort: config::SORT,
                preview: config::PREVIEW,
                filter: String::new(),
                prompt: None,
                message: None,
                quit: false,
                pick,
                marked: Vec::new(),
                preview_cache: None,
                sixel_cache: None,
                sixel_out: None,
                register: Vec::new(),
                register_cut: false,
                choice: None,
                paste_job: None,
                find: None,
                help: false,
                info: false,
            };

            app.entries = app.load_entries()?;
            app.select(select.map(Select::Name).unwrap_or(Select::First));
            Ok(app)
        }

        pub fn handle_key(&mut self, key: Key, page: usize) -> Outcome {
            self.message = None;
            self.info = false;

            if self.help {
                self.help = false;
                return Outcome::None;
            }
            if self.choice.is_some() {
                self.choice_key(key);
                return Outcome::None;
            }

            if self.prompt.is_some() {
                self.prompt_key(key);
                return Outcome::None;
            }

            let action = config::BINDINGS.iter().find(|(k, _)| *k == key).map(|(_, a)| *a);
            match action {
                Some(action) => self.act(action, page),
                None => Outcome::None,
            }
        }

        pub fn act(&mut self, action: Action, page: usize) -> Outcome {
            let page = page.max(1) as isize;

            match action {
                Action::Quit => return Outcome::Quit,
                Action::Up => self.move_by(-1),
                Action::Down => self.move_by(1),
                Action::Top => self.selected = 0,
                Action::Bottom => self.selected = self.entries.len().saturating_sub(1),
                Action::HalfUp => self.move_by(-(page / 2).max(1)),
                Action::HalfDown => self.move_by((page / 2).max(1)),
                Action::PageUp => self.move_by(-page),
                Action::PageDown => self.move_by(page),
                Action::Open => return self.open(),
                Action::Parent => self.parent(),
                Action::ToggleHidden => {
                    self.show_hidden = !self.show_hidden;
                    let selection = self.selection();
                    self.reload(selection);
                }
                Action::Sort => {
                    self.sort = self.sort.next();
                    let selection = self.selection();
                    self.reload(selection);
                }
                Action::TogglePreview => self.preview = !self.preview,
                Action::Refresh => {
                    self.preview_cache = None;
                    let selection = self.selection();
                    self.reload(selection);
                }
                Action::Edit => {
                    if let Some(entry) = self.current() {
                        if !entry.is_dir {
                            return Outcome::Edit(entry.path.clone());
                        }
                    }
                }
                Action::Search => self.start_prompt(PromptKind::Search),
                Action::NewFile => self.start_prompt(PromptKind::NewFile),
                Action::NewDir => self.start_prompt(PromptKind::NewDir),
                Action::Rename => self.start_prompt(PromptKind::Rename),
                Action::Delete => self.start_prompt(PromptKind::ConfirmDelete),
                Action::Mark => self.mark(),
                Action::Pick => return self.pick_now(),
                Action::ClearFilter => {
                    if self.find.is_some() {
                        self.exit_find();
                    } else if !self.filter.is_empty() {
                        self.filter.clear();
                        self.reload(Select::First);
                    } else if !self.marked.is_empty() {
                        self.marked.clear();
                    }
                }
                Action::SelectAll => self.select_all(),
                Action::ClearMarks => self.marked.clear(),
                Action::Copy => self.stash(false),
                Action::Cut => self.stash(true),
                Action::Paste => self.paste(),
                Action::Archive => self.start_archive(),
                Action::Extract => self.start_extract(),
                Action::Find => self.start_prompt(PromptKind::Find),
                Action::Help => self.help = true,
            }

            Outcome::None
        }

        pub fn current(&self) -> Option<&Entry> {
            self.entries.get(self.selected)
        }

        pub fn set_error(&mut self, message: String) {
            self.message = Some(message);
        }

        pub fn scroll(&mut self, page: usize) {
            let page = page.max(1);

            if self.entries.is_empty() {
                self.offset = 0;
                self.selected = 0;
                return;
            }

            self.selected = self.selected.min(self.entries.len() - 1);
            if self.selected < self.offset {
                self.offset = self.selected;
            }
            if self.selected >= self.offset + page {
                self.offset = self.selected + 1 - page;
            }

            self.offset = self.offset.min(self.entries.len().saturating_sub(page));
        }

        fn open(&mut self) -> Outcome {
            let Some(entry) = self.current() else {
                return Outcome::None;
            };

            if self.find.is_some() && (entry.is_dir || self.pick == PickMode::Off) {
                let path = entry.path.clone();
                self.goto(&path);
                return Outcome::None;
            }

            if entry.is_dir {
                let path = entry.path.clone();
                self.enter_dir(&path);
                Outcome::None
            } else if self.pick != PickMode::Off {
                self.pick_now()
            } else {
                Outcome::Open(entry.path.clone())
            }
        }

        fn mark(&mut self) {
            if self.pick == PickMode::File || self.pick == PickMode::Dir {
                return;
            }
            let Some(entry) = self.current() else {
                return;
            };
            if entry.is_dir && self.pick == PickMode::Files {
                return;
            }
            let path = entry.path.clone();
            match self.marked.iter().position(|p| *p == path) {
                Some(i) => {
                    self.marked.remove(i);
                }
                None => self.marked.push(path),
            }
            self.move_by(1);
        }

        fn pick_now(&mut self) -> Outcome {
            match self.pick {
                PickMode::Off => Outcome::None,
                PickMode::Dir => Outcome::Pick(vec![self.cwd.clone()]),
                PickMode::File | PickMode::Files => {
                    if self.pick == PickMode::Files && !self.marked.is_empty() {
                        return Outcome::Pick(self.marked.clone());
                    }
                    match self.current() {
                        Some(entry) if !entry.is_dir => Outcome::Pick(vec![entry.path.clone()]),
                        _ => {
                            self.message = Some("select a file".into());
                            Outcome::None
                        }
                    }
                }
            }
        }

        /// Jump to a path: a directory is entered, a file selected in its directory.
        pub fn goto(&mut self, target: &Path) {
            self.find = None;
            if target.is_dir() {
                self.enter_dir(target);
            } else if let Some(parent) = target.parent() {
                let name = target.file_name().map(|n| n.to_string_lossy().into_owned());
                self.cwd = parent.canonicalize().unwrap_or_else(|_| parent.to_path_buf());
                self.filter.clear();
                self.reload(name.map(Select::Name).unwrap_or(Select::First));
            }
        }

        fn enter_dir(&mut self, path: &Path) {
            self.find = None;
            match path.canonicalize() {
                Ok(cwd) if cwd.is_dir() => {
                    self.cwd = cwd;
                    self.filter.clear();
                    self.reload(Select::First);
                }
                Ok(_) => self.message = Some("not a directory".into()),
                Err(e) => self.message = Some(format!("{}: {e}", path.display())),
            }
        }

        fn parent(&mut self) {
            if self.find.is_some() {
                self.exit_find();
                return;
            }
            let Some(parent) = self.cwd.parent() else {
                self.message = Some("already at root".into());
                return;
            };

            let previous = self.cwd.file_name().map(|name| name.to_string_lossy().into_owned());
            let parent = parent.to_path_buf();
            self.cwd = parent;
            self.filter.clear();
            self.reload(previous.map(Select::Name).unwrap_or(Select::First));
        }

        fn move_by(&mut self, delta: isize) {
            if self.entries.is_empty() {
                self.selected = 0;
                return;
            }

            let last = self.entries.len() - 1;
            let next = self.selected as isize + delta;
            self.selected = next.clamp(0, last as isize) as usize;
        }

        fn load_entries(&self) -> io::Result<Vec<Entry>> {
            if let Some(f) = &self.find {
                return Ok(self.find_entries(f));
            }
            let needle = self.filter.to_lowercase();
            let mut entries = Vec::new();

            for result in fs::read_dir(&self.cwd)? {
                let Ok(entry) = result else {
                    continue;
                };

                let name = entry.file_name().to_string_lossy().into_owned();
                if name.is_empty() {
                    continue;
                }
                if !self.show_hidden && name.starts_with('.') {
                    continue;
                }
                if !needle.is_empty() && !name.to_lowercase().contains(&needle) {
                    continue;
                }

                let Ok(file_type) = entry.file_type() else {
                    continue;
                };

                entries.push(build_entry(name, &entry, file_type));
            }

            entries.sort_by(|a, b| {
                if config::DIRS_FIRST && a.is_dir != b.is_dir {
                    return b.is_dir.cmp(&a.is_dir);
                }

                let order = match self.sort {
                    Sort::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                    Sort::NameDesc => b.name.to_lowercase().cmp(&a.name.to_lowercase()),
                    Sort::Modified => b.modified.cmp(&a.modified),
                    Sort::Size => a.size.cmp(&b.size),
                    Sort::SizeDesc => b.size.cmp(&a.size),
                };

                order.then_with(|| a.name.cmp(&b.name))
            });

            Ok(entries)
        }

        fn reload(&mut self, select: Select) {
            match self.load_entries() {
                Ok(entries) => {
                    self.entries = entries;
                    self.select(select);
                }
                Err(e) => self.message = Some(format!("{}: {e}", self.cwd.display())),
            }
        }

        fn select(&mut self, select: Select) {
            let index = match select {
                Select::First => 0,
                Select::Index(index) => index,
                Select::Name(name) => self.entries.iter().position(|e| e.name == name).unwrap_or(0),
            };

            self.selected = index.min(self.entries.len().saturating_sub(1));
        }

        fn selection(&self) -> Select {
            self.current()
                .map(|entry| Select::Name(entry.name.clone()))
                .unwrap_or(Select::First)
        }

        fn start_prompt(&mut self, kind: PromptKind) {
            let (label, input, target) = match kind {
                PromptKind::Search => ("/", self.filter.clone(), None),
                PromptKind::NewFile => ("new file: ", String::new(), None),
                PromptKind::NewDir => ("new dir: ", String::new(), None),
                PromptKind::Find => ("find (all subfolders): ", String::new(), None),
                PromptKind::Rename => {
                    let Some(entry) = self.current() else {
                        return;
                    };
                    ("rename: ", entry.name.clone(), None)
                }
                PromptKind::ConfirmDelete => {
                    if self.marked.is_empty() {
                        let Some(entry) = self.current() else {
                            return;
                        };
                        ("delete: ", String::new(), Some(entry.path.clone()))
                    } else {
                        ("delete: ", String::new(), None)
                    }
                }
            };

            let previous = self.filter.clone();
            self.prompt = Some(Prompt { kind, label, input, previous, target });
        }

        fn prompt_key(&mut self, key: Key) {
            let Some(mut prompt) = self.prompt.take() else {
                return;
            };

            if prompt.kind == PromptKind::ConfirmDelete {
                match key {
                    Key::Char('y') | Key::Char('Y') => self.delete(prompt.target.clone()),
                    Key::Esc | Key::Char('n') | Key::Char('N') => {}
                    _ => self.prompt = Some(prompt),
                }
                return;
            }

            match key {
                Key::Enter => self.submit(prompt),
                Key::Esc => {
                    if prompt.kind == PromptKind::Search {
                        self.filter = prompt.previous;
                        self.reload(Select::First);
                    }
                }
                Key::Backspace => {
                    prompt.input.pop();
                    self.search(&prompt);
                    self.prompt = Some(prompt);
                }
                Key::Ctrl('u') => {
                    prompt.input.clear();
                    self.search(&prompt);
                    self.prompt = Some(prompt);
                }
                Key::Ctrl('w') => {
                    pop_word(&mut prompt.input);
                    self.search(&prompt);
                    self.prompt = Some(prompt);
                }
                Key::Char(c) if !c.is_control() => {
                    prompt.input.push(c);
                    self.search(&prompt);
                    self.prompt = Some(prompt);
                }
                _ => self.prompt = Some(prompt),
            }
        }

        fn search(&mut self, prompt: &Prompt) {
            if prompt.kind == PromptKind::Search {
                self.filter = prompt.input.clone();
                self.reload(Select::First);
            }
        }

        fn submit(&mut self, prompt: Prompt) {
            match prompt.kind {
                PromptKind::Search => {
                    self.filter = prompt.input;
                    self.reload(Select::First);
                }
                PromptKind::NewFile => self.create(&prompt.input, false),
                PromptKind::NewDir => self.create(&prompt.input, true),
                PromptKind::Rename => self.rename(&prompt.input),
                PromptKind::Find => self.run_find(prompt.input),
                PromptKind::ConfirmDelete => {}
            }
        }

        fn create(&mut self, name: &str, dir: bool) {
            if !valid_name(name) {
                self.message = Some("invalid name".into());
                return;
            }

            let path = self.cwd.join(name);
            if fs::symlink_metadata(&path).is_ok() {
                self.message = Some(format!("{name}: already exists"));
                return;
            }

            let result = if dir {
                fs::create_dir(&path)
            } else {
                fs::File::create(&path).map(|_| ())
            };

            match result {
                Ok(()) => {
                    self.filter.clear();
                    self.reload(Select::Name(name.to_string()));
                }
                Err(e) => self.message = Some(format!("create: {e}")),
            }
        }

        fn rename(&mut self, name: &str) {
            if !valid_name(name) {
                self.message = Some("invalid name".into());
                return;
            }

            let Some(entry) = self.current() else {
                return;
            };
            if entry.name == name {
                return;
            }

            let from = entry.path.clone();
            let to = self.cwd.join(name);
            if fs::symlink_metadata(&to).is_ok() {
                self.message = Some(format!("{name}: already exists"));
                return;
            }

            match fs::rename(&from, &to) {
                Ok(()) => self.reload(Select::Name(name.to_string())),
                Err(e) => self.message = Some(format!("rename: {e}")),
            }
        }

        fn delete(&mut self, target: Option<PathBuf>) {
            let paths: Vec<PathBuf> = match target {
                Some(p) => vec![p],
                None => std::mem::take(&mut self.marked),
            };
            let mut failed = None;
            for path in &paths {
                if let Err(e) = remove_path(path) {
                    failed = Some(format!("delete {}: {e}", path.display()));
                }
            }
            let index = self.selected;
            self.reload(Select::Index(index));
            match failed {
                Some(m) => self.message = Some(m),
                None => self.notice(format!("deleted {} item(s)", paths.len())),
            }
        }

        fn notice(&mut self, text: String) {
            self.message = Some(text);
            self.info = true;
        }

        /// What an action works on: the selected items, or the item under the cursor.
        fn targets(&self) -> Vec<PathBuf> {
            if !self.marked.is_empty() {
                self.marked.clone()
            } else {
                self.current().map(|e| vec![e.path.clone()]).unwrap_or_default()
            }
        }

        fn select_all(&mut self) {
            let files_only = self.pick == PickMode::Files;
            for e in &self.entries {
                if (files_only && e.is_dir) || self.marked.contains(&e.path) {
                    continue;
                }
                self.marked.push(e.path.clone());
            }
            self.notice(format!("{} selected - y copy, x move, d delete, z archive, u unselect", self.marked.len()));
        }

        // ---- copy / move / paste ----

        fn stash(&mut self, cut: bool) {
            let t = self.targets();
            if t.is_empty() {
                return;
            }
            let n = t.len();
            self.register = t;
            self.register_cut = cut;
            self.marked.clear();
            self.notice(format!(
                "{} {n} item(s) - open the target folder and press p to paste",
                if cut { "cut" } else { "copied" }
            ));
        }

        fn paste(&mut self) {
            if self.register.is_empty() {
                self.message = Some("nothing to paste - press y (copy) or x (move) first".into());
                return;
            }
            if self.find.is_some() {
                self.message = Some("leave find results first (h or esc) and open the target folder".into());
                return;
            }
            let mut items = VecDeque::new();
            for src in &self.register {
                if let Some(name) = src.file_name() {
                    items.push_back((src.clone(), self.cwd.join(name)));
                }
            }
            self.paste_job = Some(PasteJob { items, cut: self.register_cut, policy: None, once: None, done: 0, skipped: 0 });
            self.paste_step();
        }

        fn paste_step(&mut self) {
            loop {
                let Some(job) = self.paste_job.as_mut() else {
                    return;
                };
                let Some((src, dst)) = job.items.front().cloned() else {
                    let (cut, done, skipped) = (job.cut, job.done, job.skipped);
                    self.paste_job = None;
                    if cut {
                        self.register.clear();
                    }
                    self.reload(Select::First);
                    self.notice(format!("{} {done} item(s){}", if cut { "moved" } else { "pasted" }, if skipped > 0 { format!(", skipped {skipped}") } else { String::new() }));
                    return;
                };

                let exists = fs::symlink_metadata(&dst).is_ok();
                let mut target = dst.clone();
                let mut overwrite = false;
                if exists {
                    if src == dst {
                        // pasting where it already is: a copy gets a new name, a move is a no-op
                        if job.cut {
                            job.items.pop_front();
                            continue;
                        }
                        target = unique_path(&dst);
                    } else {
                        let decision = job.once.take().or(job.policy);
                        match decision {
                            None => {
                                let name = dst.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                                self.choice = Some(Choice {
                                    kind: ChoiceKind::Conflict,
                                    text: format!(" '{name}' exists:  [o]verwrite  [s]kip  [r]ename  |  all: [O]verwrite  [S]kip  [R]ename  |  [esc] stop"),
                                });
                                return;
                            }
                            Some(Policy::Skip) => {
                                job.items.pop_front();
                                job.skipped += 1;
                                continue;
                            }
                            Some(Policy::Overwrite) => overwrite = true,
                            Some(Policy::Rename) => target = unique_path(&dst),
                        }
                    }
                }

                let cut = job.cut;
                job.items.pop_front();
                if overwrite {
                    if let Err(e) = remove_path(&target) {
                        self.paste_job = None;
                        self.message = Some(format!("overwrite: {e}"));
                        return;
                    }
                }
                match transfer(&src, &target, cut) {
                    Ok(()) => {
                        if let Some(job) = self.paste_job.as_mut() {
                            job.done += 1;
                        }
                    }
                    Err(e) => {
                        self.paste_job = None;
                        self.message = Some(format!("{}: {e}", src.display()));
                        self.reload(Select::First);
                        return;
                    }
                }
            }
        }

        fn choice_key(&mut self, key: Key) {
            let Some(choice) = self.choice.take() else {
                return;
            };
            match choice.kind {
                ChoiceKind::Conflict => {
                    let Some(job) = self.paste_job.as_mut() else {
                        return;
                    };
                    match key {
                        Key::Char('o') => job.once = Some(Policy::Overwrite),
                        Key::Char('s') => job.once = Some(Policy::Skip),
                        Key::Char('r') => job.once = Some(Policy::Rename),
                        Key::Char('O') => job.policy = Some(Policy::Overwrite),
                        Key::Char('S') => job.policy = Some(Policy::Skip),
                        Key::Char('R') => job.policy = Some(Policy::Rename),
                        Key::Esc | Key::Char('q') => {
                            let done = job.done;
                            self.paste_job = None;
                            self.reload(Select::First);
                            self.notice(format!("stopped after {done} item(s)"));
                            return;
                        }
                        _ => {
                            self.choice = Some(choice);
                            return;
                        }
                    }
                    self.paste_step();
                }
                ChoiceKind::Archive(sources) => match key {
                    Key::Char(c) if "zgxsbt".contains(c) => self.do_archive(sources, c),
                    Key::Esc | Key::Char('q') => {}
                    _ => self.choice = Some(Choice { kind: ChoiceKind::Archive(sources), text: choice.text }),
                },
                ChoiceKind::Extract(archives) => match key {
                    Key::Char('h') => self.do_extract(archives, false),
                    Key::Char('f') => self.do_extract(archives, true),
                    Key::Esc | Key::Char('q') => {}
                    _ => self.choice = Some(Choice { kind: ChoiceKind::Extract(archives), text: choice.text }),
                },
            }
        }

        // ---- archive / extract ----

        fn start_archive(&mut self) {
            let t = self.targets();
            if t.is_empty() {
                return;
            }
            self.choice = Some(Choice {
                kind: ChoiceKind::Archive(t),
                text: " archive as:  [z] zip   [g] tar.gz   [x] tar.xz   [s] tar.zst   [b] tar.bz2   [t] tar   |  [esc] cancel".into(),
            });
        }

        fn do_archive(&mut self, sources: Vec<PathBuf>, fmt: char) {
            let Some(base) = sources.first().and_then(|p| p.parent()).map(Path::to_path_buf) else {
                return;
            };
            if sources.iter().any(|p| p.parent() != Some(base.as_path())) {
                self.message = Some("archive: select items from one folder only".into());
                return;
            }
            let names: Vec<OsString> = sources.iter().filter_map(|p| p.file_name().map(|n| n.to_os_string())).collect();
            let ext = match fmt {
                'z' => "zip",
                'g' => "tar.gz",
                'x' => "tar.xz",
                's' => "tar.zst",
                'b' => "tar.bz2",
                _ => "tar",
            };
            let stem = if names.len() == 1 { names[0].to_string_lossy().into_owned() } else { "archive".to_string() };
            let out = unique_path(&base.join(format!("{stem}.{ext}")));

            let mut cmd;
            let (program, package);
            if fmt == 'z' {
                program = "zip";
                package = "zip";
                cmd = Command::new("zip");
                cmd.args(["-r", "-q"]).arg(&out);
            } else {
                program = "tar";
                package = "tar";
                cmd = Command::new("tar");
                match fmt {
                    'g' => cmd.arg("-czf"),
                    'x' => cmd.arg("-cJf"),
                    'b' => cmd.arg("-cjf"),
                    's' => cmd.args(["--zstd", "-cf"]),
                    _ => cmd.arg("-cf"),
                };
                cmd.arg(&out);
            }
            cmd.arg("--").args(&names).current_dir(&base).stdin(Stdio::null());

            match cmd.output() {
                Ok(o) if o.status.success() => {
                    self.marked.clear();
                    let name = out.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    self.reload(Select::Name(name.clone()));
                    self.notice(format!("archived -> {name}"));
                }
                Ok(o) => {
                    let _ = remove_path(&out);
                    let err = String::from_utf8_lossy(&o.stderr);
                    self.message = Some(format!("{program}: {}", err.lines().next().unwrap_or("failed")));
                }
                Err(e) => self.message = Some(tool_error(program, &e, package)),
            }
        }

        fn start_extract(&mut self) {
            let archives: Vec<PathBuf> = self
                .targets()
                .into_iter()
                .filter(|p| p.file_name().and_then(|n| archive_kind(&n.to_string_lossy())).is_some())
                .collect();
            if archives.is_empty() {
                self.message = Some("not an archive (zip, tar, tar.gz/xz/zst/bz2, gz, xz, zst, bz2, 7z, rar)".into());
                return;
            }
            self.choice = Some(Choice {
                kind: ChoiceKind::Extract(archives),
                text: " extract:  [h] here (this folder)   [f] into a new folder named after the archive   |  [esc] cancel".into(),
            });
        }

        fn do_extract(&mut self, archives: Vec<PathBuf>, into_folder: bool) {
            let mut count = 0;
            for archive in &archives {
                let name = archive.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let Some(kind) = archive_kind(&name) else {
                    continue;
                };
                let dest = if into_folder { unique_path(&self.cwd.join(archive_stem(&name))) } else { self.cwd.clone() };
                if let Err(e) = fs::create_dir_all(&dest) {
                    self.message = Some(format!("extract: {e}"));
                    return;
                }
                let result = match kind {
                    ArchiveKind::Zip => Command::new("unzip")
                        .args(["-n", "-q"])
                        .arg(archive)
                        .arg("-d")
                        .arg(&dest)
                        .stdin(Stdio::null())
                        .output()
                        .map_err(|e| tool_error("unzip", &e, "unzip")),
                    ArchiveKind::Tar => Command::new("tar")
                        .arg("-xf")
                        .arg(archive)
                        .arg("-C")
                        .arg(&dest)
                        .stdin(Stdio::null())
                        .output()
                        .map_err(|e| tool_error("tar", &e, "tar")),
                    ArchiveKind::Gz | ArchiveKind::Xz | ArchiveKind::Zst | ArchiveKind::Bz2 => {
                        let (program, package) = match kind {
                            ArchiveKind::Gz => ("gzip", "gzip"),
                            ArchiveKind::Xz => ("xz", "xz"),
                            ArchiveKind::Zst => ("zstd", "zstd"),
                            _ => ("bzip2", "bzip2"),
                        };
                        let out_path = unique_path(&dest.join(archive_stem(&name)));
                        match fs::File::create(&out_path) {
                            Ok(f) => Command::new(program)
                                .arg("-dc")
                                .arg(archive)
                                .stdin(Stdio::null())
                                .stdout(Stdio::from(f))
                                .output()
                                .map_err(|e| tool_error(program, &e, package)),
                            Err(e) => Err(format!("extract: {e}")),
                        }
                    }
                    ArchiveKind::Other7z => {
                        let mut found = None;
                        for program in ["7z", "7zz", "7za", "7zr"] {
                            match Command::new(program)
                                .arg("x")
                                .arg("-y")
                                .arg(format!("-o{}", dest.display()))
                                .arg(archive)
                                .stdin(Stdio::null())
                                .output()
                            {
                                Ok(o) => {
                                    found = Some(o);
                                    break;
                                }
                                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                                Err(e) => {
                                    self.message = Some(format!("{program}: {e}"));
                                    return;
                                }
                            }
                        }
                        found.ok_or_else(|| "7z is not installed (xbps-install 7zip)".to_string())
                    }
                };
                match result {
                    Ok(o) if o.status.success() => count += 1,
                    Ok(o) => {
                        let err = String::from_utf8_lossy(&o.stderr);
                        self.message = Some(format!("{name}: {}", err.lines().next().unwrap_or("extract failed")));
                        self.reload(Select::First);
                        return;
                    }
                    Err(m) => {
                        self.message = Some(m);
                        return;
                    }
                }
            }
            self.marked.clear();
            self.reload(Select::First);
            self.notice(format!("extracted {count} archive(s){}", if into_folder { " into new folder(s)" } else { "" }));
        }

        // ---- recursive find ----

        fn run_find(&mut self, pattern: String) {
            self.find = Some(FindState { origin: self.cwd.clone(), pattern });
            self.reload(Select::First);
            let n = self.entries.len();
            let limit = if n >= FIND_LIMIT { format!(" (stopped at {FIND_LIMIT})") } else { String::new() };
            self.notice(format!("{n} result(s){limit} - enter go to it, h/esc back"));
        }

        fn exit_find(&mut self) {
            self.find = None;
            self.reload(Select::First);
        }

        fn find_entries(&self, f: &FindState) -> Vec<Entry> {
            let needle = f.pattern.to_lowercase();
            let mut out: Vec<Entry> = Vec::new();
            let mut stack = vec![f.origin.clone()];
            while let Some(dir) = stack.pop() {
                let Ok(rd) = fs::read_dir(&dir) else {
                    continue;
                };
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if !self.show_hidden && name.starts_with('.') {
                        continue;
                    }
                    let Ok(ft) = e.file_type() else {
                        continue;
                    };
                    let path = e.path();
                    if ft.is_dir() {
                        stack.push(path.clone());
                    }
                    let rel = path.strip_prefix(&f.origin).unwrap_or(&path).to_string_lossy().into_owned();
                    if needle.is_empty() || rel.to_lowercase().contains(&needle) {
                        out.push(build_entry(rel, &e, ft));
                        if out.len() >= FIND_LIMIT {
                            out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                            return out;
                        }
                    }
                }
            }
            out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            out
        }
    }

    fn valid_name(name: &str) -> bool {
        !name.is_empty()
            && name != "."
            && name != ".."
            && !name.contains('/')
            && !name.contains('\0')
    }

    fn pop_word(input: &mut String) {
        while let Some(c) = input.chars().last() {
            if c.is_whitespace() {
                input.pop();
            } else {
                break;
            }
        }
        while let Some(c) = input.chars().last() {
            if c.is_whitespace() {
                break;
            }
            input.pop();
        }
    }
}

mod ui {
    use std::fs;
    use std::io::{self, Read, Write};
    use std::path::Path;
    use std::process::{Command, Stdio};

    use crate::app::{archive_kind, App, ArchiveKind, PickMode, PromptKind};
    use crate::config;

    pub fn draw(app: &mut App, size: (u16, u16)) -> io::Result<()> {
        let width = size.0.max(1) as usize;
        let height = size.1.max(2) as usize;
        let content_height = height - 2;
        if app.help {
            return draw_help(width, height);
        }
        app.scroll(content_height);

        let show_preview =
            app.preview && app.current().is_some() && width >= config::PREVIEW_MIN_WIDTH as usize;
        let list_width = if show_preview {
            (width * 2 / 5).clamp(16, width - 16)
        } else {
            width
        };
        let preview_width = width.saturating_sub(list_width + 1);
        let preview = if show_preview {
            preview_lines(app, preview_width, content_height)
        } else {
            Vec::new()
        };

        let mut out = String::with_capacity(width * height + 256);
        out.push_str("\x1b[H");

        let header = match &app.find {
            Some(f) => format!(" find: {}   ({} results in {})", f.pattern, app.entries.len(), short_path(&f.origin)),
            None => format!(" {}", short_path(&app.cwd)),
        };
        out.push_str(&paint(config::STYLE_HEADER, &fit(&header, width)));
        out.push_str("\x1b[0m\x1b[K\r\n");

        for row in 0..content_height {
            let index = app.offset + row;
            let mut line = match app.entries.get(index) {
                Some(entry) => {
                    let marker = if app.marked.contains(&entry.path) { "+" } else { " " };
                    let text = fit(&format!("{marker}{}", entry.label()), list_width);
                    let base = config::entry_style(entry);
                    let style = if index == app.selected {
                        if base.is_empty() { "7".to_string() } else { format!("{base};7") }
                    } else if app.marked.contains(&entry.path) {
                        config::STYLE_MARK.to_string()
                    } else {
                        base
                    };
                    paint(&style, &text)
                }
                None => spaces(list_width),
            };

            if show_preview {
                line.push_str(&paint(config::STYLE_BORDER, "|"));
                line.push_str(preview.get(row).map(String::as_str).unwrap_or(""));
            }

            out.push_str(&line);
            out.push_str("\x1b[0m\x1b[K\r\n");
        }

        let (status, cursor) = status_line(app, width);
        out.push_str(&status);
        out.push_str("\x1b[0m\x1b[K");

        // sixel image on top of the (blank) preview pane
        if show_preview {
            if let Some(sixel) = &app.sixel_out {
                out.push_str(&format!("\x1b[2;{}H", list_width + 2));
                out.push_str(sixel);
            }
        }

        match cursor {
            Some(column) => out.push_str(&format!(
                "\x1b[{};{}H\x1b[?25h",
                height,
                column.clamp(1, width)
            )),
            None => out.push_str("\x1b[?25l"),
        }

        let mut stdout = io::stdout().lock();
        stdout.write_all(out.as_bytes())?;
        stdout.flush()
    }

    fn draw_help(width: usize, height: usize) -> io::Result<()> {
        let mut out = String::from("\x1b[H\x1b[?25l");
        for row in 0..height {
            let line = config::HELP.get(row).copied().unwrap_or("");
            let style = if row == 0 { config::STYLE_HEADER } else { "" };
            out.push_str(&paint(style, &fit(line, width)));
            out.push_str("\x1b[0m\x1b[K");
            if row + 1 < height {
                out.push_str("\r\n");
            }
        }
        let mut stdout = io::stdout().lock();
        stdout.write_all(out.as_bytes())?;
        stdout.flush()
    }

    fn status_line(app: &App, width: usize) -> (String, Option<usize>) {
        if let Some(choice) = &app.choice {
            return (paint(config::STYLE_PROMPT, &fit(&choice.text, width)), None);
        }
        if let Some(prompt) = &app.prompt {
            if prompt.kind == PromptKind::ConfirmDelete && prompt.target.is_none() {
                let text = format!(" delete {} selected item(s)? [y/N]", app.marked.len());
                return (paint(config::STYLE_PROMPT, &fit(&text, width)), None);
            }
            if prompt.kind == PromptKind::ConfirmDelete {
                let name = prompt
                    .target
                    .as_deref()
                    .and_then(Path::file_name)
                    .map(|name| name.to_string_lossy())
                    .unwrap_or_default();
                let text = format!(" delete '{name}'? [y/N]");
                return (paint(config::STYLE_PROMPT, &fit(&text, width)), None);
            }

            let full = format!("{}{}", prompt.label, prompt.input);
            let shown = if display_width(&full) >= width {
                tail(&full, width.saturating_sub(1))
            } else {
                full
            };
            let column = display_width(&shown) + 1;

            return (paint(config::STYLE_PROMPT, &fit(&shown, width)), Some(column));
        }

        if let Some(message) = &app.message {
            let style = if app.info { config::STYLE_INFO } else { config::STYLE_ERROR };
            return (paint(style, &fit(&format!(" {message}"), width)), None);
        }

        let position = if app.entries.is_empty() {
            "0/0".to_string()
        } else {
            format!("{}/{}", app.selected + 1, app.entries.len())
        };

        let mut text = format!(" {} {} ", position, app.sort.label());
        if !app.filter.is_empty() {
            text.push_str(&format!("[{}] ", app.filter));
        }

        if !app.marked.is_empty() {
            text.push_str(&format!("[{} selected] ", app.marked.len()));
        }
        if !app.register.is_empty() {
            text.push_str(&format!("[{} to {}] ", app.register.len(), if app.register_cut { "move" } else { "copy" }));
        }
        if let Some(entry) = app.current() {
            text.push_str(&entry_details(entry));
        }
        if app.pick != PickMode::Off {
            text.push_str("   ");
            text.push_str(config::HINTS_PICK);
        }

        (paint(config::STYLE_STATUS, &fit(&text, width)), None)
    }

    /// "drwxr-xr-x  4.0 K  2026-10-25 14:03  directory (12 items)" for the status bar.
    fn entry_details(entry: &crate::app::Entry) -> String {
        let kind_char = if entry.is_symlink {
            'l'
        } else if entry.is_dir {
            'd'
        } else {
            '-'
        };
        let mut perms = String::from(kind_char);
        for shift in [6u32, 3, 0] {
            let bits = (entry.mode >> shift) & 7;
            perms.push(if bits & 4 != 0 { 'r' } else { '-' });
            perms.push(if bits & 2 != 0 { 'w' } else { '-' });
            perms.push(if bits & 1 != 0 { 'x' } else { '-' });
        }

        let size = if entry.is_dir {
            match fs::read_dir(&entry.path) {
                Ok(rd) => format!("{} items", rd.count()),
                Err(_) => "-".to_string(),
            }
        } else {
            human_size(entry.size)
        };

        let kind = if entry.is_symlink {
            match fs::read_link(&entry.path) {
                Ok(target) => format!("symlink -> {}", target.display()),
                Err(_) => "symlink".to_string(),
            }
        } else if entry.is_dir {
            "directory".to_string()
        } else if entry.exec {
            "executable".to_string()
        } else {
            match entry.name.rsplit_once('.') {
                Some((base, ext)) if !base.is_empty() => format!("{} file", ext.to_lowercase()),
                _ => "file".to_string(),
            }
        };

        format!("{}  {}  {}  {}", perms, size, format_time(entry.modified), kind)
    }

    /// Local time as "2026-10-25 14:03".
    fn format_time(time: Option<std::time::SystemTime>) -> String {
        let Some(secs) = time
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as libc::time_t)
        else {
            return "-".to_string();
        };
        unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            if libc::localtime_r(&secs, &mut tm).is_null() {
                return "-".to_string();
            }
            let mut buf = [0u8; 32];
            let n = libc::strftime(
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
                b"%Y-%m-%d %H:%M\0".as_ptr() as *const libc::c_char,
                &tm,
            );
            String::from_utf8_lossy(&buf[..n]).into_owned()
        }
    }

    fn preview_lines(app: &mut App, width: usize, height: usize) -> Vec<String> {
        let Some(entry) = app.current() else {
            return vec![spaces(width); height];
        };
        let (path, is_dir) = (entry.path.clone(), entry.is_dir);
        let hidden = app.show_hidden;

        app.sixel_out = None;
        if !is_dir && width >= 8 && height >= 4 {
            let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
            let image = matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg" | "ico" | "tiff" | "jfif");
            if image || ext == "pdf" {
                if let Some(sixel) = sixel_for(app, &path, ext == "pdf", width, height) {
                    app.sixel_out = Some(sixel);
                    return vec![spaces(width); height];
                }
            }
        }

        let mut lines = Vec::with_capacity(height);
        if height > 0 {
            let cached = match &app.preview_cache {
                Some((p, w, h, hd, l)) if *p == path && *w == width && *h == height && *hd == hidden => Some(l.clone()),
                _ => None,
            };
            let body = match cached {
                Some(l) => l,
                None => {
                    let l = build_preview(&path, is_dir, hidden, width, height);
                    app.preview_cache = Some((path.clone(), width, height, hidden, l.clone()));
                    l
                }
            };
            lines.extend(body);
        }

        while lines.len() < height {
            lines.push(spaces(width));
        }
        lines.truncate(height);
        lines
    }

    /// Directory: eza. Text file: bat. Falls back to the built-in preview when the
    /// tool is missing; binary files are never handed to bat.
    fn build_preview(path: &Path, is_dir: bool, hidden: bool, width: usize, height: usize) -> Vec<String> {
        if is_dir {
            let mut cmd = Command::new("eza");
            cmd.args(["-1", "--color=always", "--group-directories-first"]);
            if hidden {
                cmd.arg("-a");
            }
            cmd.arg(path);
            if let Some(lines) = tool_lines(cmd, width, height) {
                return lines;
            }
            return vec![paint(config::STYLE_DIM, &fit(" directory", width))];
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        if ext == "pdf" {
            let mut cmd = Command::new("pdftotext");
            cmd.args(["-l", "1", "-layout"]).arg(path).arg("-");
            return tool_lines(cmd, width, height).unwrap_or_else(|| {
                vec![paint(config::STYLE_DIM, &fit(" PDF preview needs poppler-utils (xbps-install poppler-utils) and chafa", width))]
            });
        }
        if matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg" | "ico" | "tiff" | "jfif") {
            return vec![paint(config::STYLE_DIM, &fit(" image preview needs chafa (xbps-install chafa) and a sixel terminal", width))];
        }
        match archive_kind(&name) {
            Some(ArchiveKind::Tar) => {
                let mut cmd = Command::new("tar");
                cmd.arg("-tf").arg(path);
                if let Some(lines) = tool_lines(cmd, width, height) {
                    return lines;
                }
            }
            Some(ArchiveKind::Zip) => {
                let mut cmd = Command::new("unzip");
                cmd.arg("-l").arg(path);
                if let Some(lines) = tool_lines(cmd, width, height) {
                    return lines;
                }
            }
            Some(_) => return vec![paint(config::STYLE_DIM, &fit(" compressed file - press X to extract", width))],
            None => {}
        }
        let text = fs::File::open(path)
            .ok()
            .map(|f| {
                let mut head = Vec::new();
                let _ = f.take(8192).read_to_end(&mut head);
                !head.contains(&0)
            })
            .unwrap_or(false);
        if text {
            let mut cmd = Command::new("bat");
            cmd.args(["--color=always", "--style=plain", "--paging=never", "--tabs=4", "--theme=ansi"]);
            cmd.arg(format!("--line-range=:{}", height));
            cmd.arg(path);
            if let Some(lines) = tool_lines(cmd, width, height) {
                return lines;
            }
        }
        preview_file(path, width, height)
    }

    /// Sixel data (via chafa) for an image, or for page 1 of a PDF (via pdftoppm).
    fn sixel_for(app: &mut App, path: &Path, pdf: bool, width: usize, height: usize) -> Option<String> {
        if let Some((p, w, h, s)) = &app.sixel_cache {
            if p == path && *w == width && *h == height {
                return Some(s.clone());
            }
        }
        let mut temp = None;
        let source = if pdf {
            let base = format!("/tmp/fm-pdf-{}", std::process::id());
            let ok = Command::new("pdftoppm")
                .args(["-png", "-r", "80", "-f", "1", "-l", "1", "-singlefile"])
                .arg(path)
                .arg(&base)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !ok {
                return None;
            }
            let png = std::path::PathBuf::from(format!("{base}.png"));
            temp = Some(png.clone());
            png
        } else {
            path.to_path_buf()
        };
        let out = Command::new("chafa")
            .args(["-f", "sixels", "--animate=off", "-s", &format!("{width}x{height}")])
            .arg(&source)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok();
        if let Some(t) = temp {
            let _ = fs::remove_file(t);
        }
        let out = out?;
        if !out.status.success() || out.stdout.is_empty() {
            return None;
        }
        let data = String::from_utf8_lossy(&out.stdout).trim_end_matches(['\n', '\r']).to_string();
        app.sixel_cache = Some((path.to_path_buf(), width, height, data.clone()));
        Some(data)
    }

    fn tool_lines(mut cmd: Command, width: usize, height: usize) -> Option<Vec<String>> {
        let out = cmd.stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        Some(text.lines().take(height).map(|l| fit_ansi(l, width)).collect())
    }

    /// Like fit(), but keeps ANSI colour sequences (they take no width).
    fn fit_ansi(text: &str, width: usize) -> String {
        let mut out = String::new();
        let mut used = 0;
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                out.push(c);
                if chars.peek() == Some(&'[') {
                    out.push('[');
                    chars.next();
                    while let Some(&n) = chars.peek() {
                        out.push(n);
                        chars.next();
                        if ('@'..='~').contains(&n) {
                            break;
                        }
                    }
                }
                continue;
            }
            let c = if c == '\t' {
                ' '
            } else if c.is_control() {
                continue;
            } else {
                c
            };
            let cell = char_width(c);
            if used + cell > width {
                break;
            }
            out.push(c);
            used += cell;
        }
        out.push_str("\x1b[0m");
        out.push_str(&spaces(width.saturating_sub(used)));
        out
    }

    fn preview_file(path: &Path, width: usize, height: usize) -> Vec<String> {
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(e) => return vec![paint(config::STYLE_ERROR, &fit(&format!(" {e}"), width))],
        };
        if !metadata.is_file() {
            return vec![paint(config::STYLE_DIM, &fit(" not a regular file", width))];
        }

        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(e) => return vec![paint(config::STYLE_ERROR, &fit(&format!(" {e}"), width))],
        };

        let mut data = Vec::new();
        if let Err(e) = file.take(config::PREVIEW_BYTES as u64).read_to_end(&mut data) {
            return vec![paint(config::STYLE_ERROR, &fit(&format!(" {e}"), width))];
        }

        if data.iter().take(8192).any(|&byte| byte == 0) {
            return vec![paint(config::STYLE_DIM, &fit(" binary file", width))];
        }

        let text = String::from_utf8_lossy(&data);
        let mut lines = Vec::new();

        for line in text.lines().take(height) {
            let mut expanded = String::with_capacity(line.len());
            for c in line.chars() {
                if c == '\t' {
                    expanded.push_str("    ");
                } else if c.is_control() {
                    expanded.push(' ');
                } else {
                    expanded.push(c);
                }
            }
            lines.push(fit(&expanded, width));
        }

        if lines.is_empty() {
            lines.push(paint(config::STYLE_DIM, &fit(" empty", width)));
        }

        lines
    }

    fn short_path(path: &Path) -> String {
        let Some(home) = std::env::var_os("HOME") else {
            return path.display().to_string();
        };

        match path.strip_prefix(&home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        }
    }

    fn human_size(bytes: u64) -> String {
        const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
        let mut value = bytes as f64;
        let mut unit = 0;

        while value >= 1024.0 && unit + 1 < UNITS.len() {
            value /= 1024.0;
            unit += 1;
        }

        if unit == 0 {
            format!("{bytes} B")
        } else {
            format!("{value:.1} {}", UNITS[unit])
        }
    }

    fn paint(style: &str, text: &str) -> String {
        if style.is_empty() {
            text.to_string()
        } else {
            format!("\x1b[{style}m{text}\x1b[0m")
        }
    }

    fn spaces(width: usize) -> String {
        " ".repeat(width)
    }

    fn fit(text: &str, width: usize) -> String {
        let mut out = truncate(text, width);
        let used = display_width(&out);
        out.push_str(&spaces(width.saturating_sub(used)));
        out
    }

    fn truncate(text: &str, width: usize) -> String {
        let mut out = String::new();
        let mut used = 0;

        for c in text.chars() {
            let c = if c.is_control() { ' ' } else { c };
            let cell = char_width(c);
            if used + cell > width {
                break;
            }
            out.push(c);
            used += cell;
        }

        out
    }

    fn tail(text: &str, width: usize) -> String {
        let chars: Vec<char> = text.chars().collect();
        let mut used = 0;
        let mut start = chars.len();

        while start > 0 {
            let c = chars[start - 1];
            let cell = char_width(if c.is_control() { ' ' } else { c });
            if used + cell > width {
                break;
            }
            used += cell;
            start -= 1;
        }

        chars[start..].iter().collect()
    }

    fn display_width(text: &str) -> usize {
        text.chars()
            .map(|c| char_width(if c.is_control() { ' ' } else { c }))
            .sum()
    }

    fn char_width(c: char) -> usize {
        match c as u32 {
            0x1100..=0x115f
            | 0x2e80..=0xa4cf
            | 0xac00..=0xd7a3
            | 0xf900..=0xfaff
            | 0xfe10..=0xfe19
            | 0xfe30..=0xfe6f
            | 0xff00..=0xff60
            | 0xffe0..=0xffe6
            | 0x1f300..=0x1faff
            | 0x20000..=0x3fffd => 2,
            _ => 1,
        }
    }
}
