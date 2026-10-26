/* See LICENSE file for license details. */
/*
 * lock - simple Wayland screen locker (ext-session-lock-v1)
 *
 * A Wayland port of the slock-rs based `lock`: every output is blanked, the
 * screen is black after initialization, #005577 while a password is typed
 * and #CC3333 after a wrong password. The password is checked with PAM
 * (service "lock", /etc/pam.d/lock), so no setuid bit is needed.
 *
 *   lock              lock now, exit after unlocking
 *   lock -a [LOCK [OFF]]
 *                     stay running: lock after LOCK seconds of inactivity and
 *                     turn the screens off after OFF seconds (0 = never; defaults
 *                     AUTOLOCK_SECS / SCREENOFF_SECS). Uses ext-idle-notify-v1 and
 *                     wlr-output-power-management-v1 (this replaces screenctl).
 *   lock -v           print version
 */

use std::collections::HashMap;
use std::ffi::CStr;
use std::ptr;

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_output, delegate_registry, delegate_seat,
    delegate_session_lock, delegate_shm,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            channel::{self, Event as ChanEvent},
            EventLoop,
        },
        calloop_wayland_source::WaylandSource,
        client::{
            globals::registry_queue_init,
            protocol::{wl_keyboard, wl_output, wl_seat, wl_shm, wl_surface},
            backend::ObjectId,
            Connection, Dispatch, Proxy, QueueHandle,
        },
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
        Capability, SeatHandler, SeatState,
    },
    session_lock::{
        SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface,
        SessionLockSurfaceConfigure,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1::{self, ExtIdleNotificationV1},
    ext_idle_notifier_v1::ExtIdleNotifierV1,
};
use wayland_protocols_wlr::output_power_management::v1::client::{
    zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1,
    zwlr_output_power_v1::{self, Mode as PowerMode, ZwlrOutputPowerV1},
};

const VERSION: &str = "1.8";

/* configuration (compiled in, like config.h) */

/* index into COLORS */
const INIT: usize = 0;
const INPUT: usize = 1;
const FAILED: usize = 2;

/* 0xAARRGGBB: black after initialization, #005577 during input, #CC3333 wrong password */
const COLORS: [u32; 3] = [0xff00_0000, 0xff00_5577, 0xffcc_3333];

/* treat a cleared input like a wrong password (color) */
const FAILONCLEAR: bool = true;

/* `lock -a` defaults (seconds): lock after, screens off after */
const AUTOLOCK_SECS: u32 = 300;
const SCREENOFF_SECS: u32 = 600;

/* PAM service name, see /etc/pam.d/lock */
const PAM_SERVICE: &str = "lock";

/* ---- PAM (libpam, direct FFI) ---- */

mod pam {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int, c_void};

    #[repr(C)]
    struct Message {
        style: c_int,
        msg: *const c_char,
    }

    #[repr(C)]
    struct Response {
        resp: *mut c_char,
        retcode: c_int,
    }

    #[repr(C)]
    struct Conv {
        conv: extern "C" fn(c_int, *mut *const Message, *mut *mut Response, *mut c_void) -> c_int,
        data: *mut c_void,
    }

    #[link(name = "pam")]
    extern "C" {
        fn pam_start(
            service: *const c_char,
            user: *const c_char,
            conv: *const Conv,
            handle: *mut *mut c_void,
        ) -> c_int;
        fn pam_authenticate(handle: *mut c_void, flags: c_int) -> c_int;
        fn pam_end(handle: *mut c_void, status: c_int) -> c_int;
    }

    const PAM_PROMPT_ECHO_OFF: c_int = 1;
    const PAM_PROMPT_ECHO_ON: c_int = 2;
    const PAM_CONV_ERR: c_int = 19;

    extern "C" fn converse(
        n: c_int,
        msg: *mut *const Message,
        resp: *mut *mut Response,
        data: *mut c_void,
    ) -> c_int {
        unsafe {
            if n <= 0 || n > 32 || data.is_null() {
                return PAM_CONV_ERR;
            }
            let replies = libc::calloc(n as usize, std::mem::size_of::<Response>()) as *mut Response;
            if replies.is_null() {
                return PAM_CONV_ERR;
            }
            for i in 0..n as usize {
                let m = *msg.add(i);
                let style = (*m).style;
                if style == PAM_PROMPT_ECHO_OFF || style == PAM_PROMPT_ECHO_ON {
                    let dup = libc::strdup(data as *const c_char);
                    if dup.is_null() {
                        for j in 0..i {
                            libc::free((*replies.add(j)).resp as *mut c_void);
                        }
                        libc::free(replies as *mut c_void);
                        return PAM_CONV_ERR;
                    }
                    (*replies.add(i)).resp = dup;
                }
            }
            *resp = replies;
            0
        }
    }

    /// True when `password` is correct for `user`.
    pub fn authenticate(service: &str, user: &str, password: &[u8]) -> bool {
        let (Ok(service), Ok(user), Ok(pw)) =
            (CString::new(service), CString::new(user), CString::new(password))
        else {
            return false;
        };
        let conv = Conv { conv: converse, data: pw.as_ptr() as *mut c_void };
        let mut handle: *mut c_void = std::ptr::null_mut();
        unsafe {
            if pam_start(service.as_ptr(), user.as_ptr(), &conv, &mut handle) != 0 {
                return false;
            }
            let rc = pam_authenticate(handle, 0);
            pam_end(handle, rc);
            rc == 0
        }
    }
}

/* memset(3) that the compiler may not optimize away. */
fn explicit_bzero(buf: &mut [u8]) {
    for byte in buf.iter_mut() {
        unsafe { ptr::write_volatile(byte, 0) };
    }
}

fn current_user() -> String {
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if !pw.is_null() {
            return CStr::from_ptr((*pw).pw_name).to_string_lossy().into_owned();
        }
    }
    std::env::var("USER").unwrap_or_default()
}

fn die(msg: &str) -> ! {
    eprintln!("lock: {}", msg);
    std::process::exit(1);
}

fn usage() -> ! {
    die("usage: lock [-v] [-a [lock-seconds [screen-off-seconds]]]")
}

struct Surf {
    surface: SessionLockSurface,
    output: wl_output::WlOutput,
    w: u32,
    h: u32,
}

struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor: CompositorState,
    shm: Shm,
    pool: SlotPool,
    lock_state: SessionLockState,
    qh: QueueHandle<App>,
    keyboard: Option<wl_keyboard::WlKeyboard>,

    session: Option<SessionLock>,
    surfaces: Vec<Surf>,
    locked: bool,

    password: Vec<u8>,
    color: usize,
    failure: bool,
    verifying: bool,
    user: String,
    auth_tx: channel::Sender<bool>,

    daemon: bool,
    lock_secs: u32,
    off_secs: u32,
    notifier: Option<ExtIdleNotifierV1>,
    idle: Vec<ExtIdleNotificationV1>,
    power_mgr: Option<ZwlrOutputPowerManagerV1>,
    powers: HashMap<ObjectId, ZwlrOutputPowerV1>,
    blanked: bool,

    exit: Option<i32>,
}

impl App {
    fn start_lock(&mut self) {
        if self.session.is_some() {
            return;
        }
        let session = match self.lock_state.lock(&self.qh) {
            Ok(s) => s,
            Err(_) => {
                eprintln!("lock: compositor does not support ext-session-lock-v1");
                self.exit = Some(1);
                return;
            }
        };
        self.session = Some(session);
        self.password.clear();
        self.color = INIT;
        self.failure = false;
        self.verifying = false;
        let outputs: Vec<wl_output::WlOutput> = self.output_state.outputs().collect();
        for o in outputs {
            self.add_surface(o);
        }
    }

    fn add_surface(&mut self, output: wl_output::WlOutput) {
        // one lock surface per output: a second one is a protocol error that kills us
        if self.surfaces.iter().any(|s| s.output == output) {
            return;
        }
        let wl_surface = self.compositor.create_surface(&self.qh);
        let Some(session) = &self.session else { return };
        let surface = session.create_lock_surface(wl_surface, &output, &self.qh);
        self.surfaces.push(Surf { surface, output, w: 0, h: 0 });
    }

    fn draw(&mut self, i: usize) {
        let (w, h) = (self.surfaces[i].w, self.surfaces[i].h);
        if w == 0 || h == 0 {
            return;
        }
        let color = COLORS[self.color].to_le_bytes();
        let (buffer, canvas) =
            match self.pool.create_buffer(w as i32, h as i32, w as i32 * 4, wl_shm::Format::Xrgb8888) {
                Ok(b) => b,
                Err(_) => return,
            };
        for px in canvas.chunks_exact_mut(4) {
            px.copy_from_slice(&color);
        }
        let wl = self.surfaces[i].surface.wl_surface();
        wl.damage_buffer(0, 0, w as i32, h as i32);
        if buffer.attach_to(wl).is_ok() {
            wl.commit();
        }
    }

    fn draw_all(&mut self) {
        for i in 0..self.surfaces.len() {
            self.draw(i);
        }
    }

    fn update_color(&mut self) {
        let color = if !self.password.is_empty() {
            INPUT
        } else if self.failure || FAILONCLEAR {
            FAILED
        } else {
            INIT
        };
        if color != self.color {
            self.color = color;
            self.draw_all();
        }
    }

    fn clear_password(&mut self) {
        explicit_bzero(&mut self.password);
        self.password.clear();
    }

    fn handle_key(&mut self, ev: &KeyEvent) {
        if self.verifying || !self.locked {
            return;
        }
        match ev.keysym.raw() {
            0xff0d | 0xff8d => {
                // Return, KP_Enter
                if self.password.is_empty() {
                    self.failure = true;
                    self.update_color();
                    return;
                }
                self.verifying = true;
                let mut pw = std::mem::take(&mut self.password);
                let tx = self.auth_tx.clone();
                let user = self.user.clone();
                std::thread::spawn(move || {
                    let ok = pam::authenticate(PAM_SERVICE, &user, &pw);
                    explicit_bzero(&mut pw);
                    let _ = tx.send(ok);
                });
            }
            0xff1b => {
                // Escape
                self.clear_password();
                self.update_color();
            }
            0xff08 => {
                // BackSpace
                if let Some(b) = self.password.last_mut() {
                    *b = 0;
                }
                // drop one whole UTF-8 character
                while let Some(b) = self.password.pop() {
                    if b & 0xc0 != 0x80 {
                        break;
                    }
                }
                self.update_color();
            }
            _ => {
                if let Some(t) = &ev.utf8 {
                    if t == "\u{15}" {
                        // ctrl-u clears input
                        self.clear_password();
                    } else {
                        let clean: String = t.chars().filter(|c| !c.is_control()).collect();
                        if !clean.is_empty() && self.password.len() + clean.len() < 255 {
                            self.password.extend_from_slice(clean.as_bytes());
                        }
                    }
                    self.update_color();
                }
            }
        }
    }

    fn on_auth(&mut self, ok: bool) {
        self.verifying = false;
        if ok {
            self.unlock();
        } else {
            self.failure = true;
            self.color = FAILED;
            self.draw_all();
        }
    }

    fn unlock(&mut self) {
        if let Some(s) = self.session.take() {
            s.unlock();
        }
        self.surfaces.clear();
        self.locked = false;
        self.failure = false;
        self.color = INIT;
        if !self.daemon {
            self.exit = Some(0);
        }
    }

    fn set_blank(&mut self, off: bool) {
        self.blanked = off;
        let mode = if off { PowerMode::Off } else { PowerMode::On };
        for p in self.powers.values() {
            p.set_mode(mode);
        }
    }
}

impl SessionLockHandler for App {
    fn locked(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        self.locked = true;
        self.draw_all();
    }

    fn finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        // denied (already locked) or ended by the compositor
        self.session = None;
        self.surfaces.clear();
        self.locked = false;
        if !self.daemon {
            eprintln!("lock: cannot lock (already locked, or not supported)");
            self.exit = Some(1);
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: SessionLockSurface,
        configure: SessionLockSurfaceConfigure,
        _: u32,
    ) {
        if let Some(i) = self
            .surfaces
            .iter()
            .position(|s| s.surface.wl_surface() == surface.wl_surface())
        {
            self.surfaces[i].w = configure.new_size.0;
            self.surfaces[i].h = configure.new_size.1;
            self.draw(i);
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
        if let Some(m) = &self.power_mgr {
            let p = m.get_output_power(&output, qh, ());
            if self.blanked {
                p.set_mode(PowerMode::Off);
            }
            self.powers.insert(output.id(), p);
        }
        if self.session.is_some() {
            self.add_surface(output);
        }
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.powers.remove(&output.id());
        self.surfaces.retain(|s| s.output != output);
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        if self.idle.is_empty() {
            if let Some(n) = &self.notifier {
                // user data tells the two timers apart: 0 = lock, 1 = screens off
                if self.lock_secs > 0 {
                    self.idle.push(n.get_idle_notification(self.lock_secs.saturating_mul(1000), &seat, qh, 0u8));
                }
                if self.off_secs > 0 && self.power_mgr.is_some() {
                    self.idle.push(n.get_idle_notification(self.off_secs.saturating_mul(1000), &seat, qh, 1u8));
                }
            }
        }
    }
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

impl Dispatch<ExtIdleNotifierV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ExtIdleNotifierV1,
        _: <ExtIdleNotifierV1 as smithay_client_toolkit::reexports::client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtIdleNotificationV1, u8> for App {
    fn event(
        state: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        which: &u8,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match (event, *which) {
            (ext_idle_notification_v1::Event::Idled, 0) => state.start_lock(),
            (ext_idle_notification_v1::Event::Idled, _) => state.set_blank(true),
            (ext_idle_notification_v1::Event::Resumed, 1) => state.set_blank(false),
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputPowerManagerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ZwlrOutputPowerManagerV1,
        _: <ZwlrOutputPowerManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrOutputPowerV1, ()> for App {
    fn event(
        state: &mut Self,
        power: &ZwlrOutputPowerV1,
        event: zwlr_output_power_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_output_power_v1::Event::Failed = event {
            state.powers.retain(|_, p| p != power);
        }
    }
}

delegate_compositor!(App);
delegate_output!(App);
delegate_shm!(App);
delegate_seat!(App);
delegate_keyboard!(App);
delegate_session_lock!(App);
delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mut daemon, mut lock_secs, mut off_secs) = (false, 0u32, 0u32);
    let num = |s: &str| -> u32 { s.parse().unwrap_or_else(|_| usage()) };
    match args.iter().map(|s| s.as_str()).collect::<Vec<_>>().as_slice() {
        [] => {}
        ["-v"] => {
            println!("lock-{}", VERSION);
            return;
        }
        ["-a"] => (daemon, lock_secs, off_secs) = (true, AUTOLOCK_SECS, SCREENOFF_SECS),
        ["-a", l] => (daemon, lock_secs, off_secs) = (true, num(l), SCREENOFF_SECS),
        ["-a", l, o] => (daemon, lock_secs, off_secs) = (true, num(l), num(o)),
        _ => usage(),
    }
    if daemon && lock_secs == 0 && off_secs == 0 {
        return; /* both disabled */
    }

    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(_) => die("cannot connect to the Wayland display"),
    };
    let (globals, event_queue) = registry_queue_init(&conn).expect("registry init");
    let qh = event_queue.handle();
    let mut event_loop: EventLoop<'static, App> = EventLoop::try_new().expect("event loop");
    let handle = event_loop.handle();
    WaylandSource::new(conn.clone(), event_queue)
        .insert(handle.clone())
        .expect("wayland source");

    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor missing");
    let shm = Shm::bind(&globals, &qh).expect("wl_shm missing");
    let pool = SlotPool::new(1 << 20, &shm).expect("shm pool");
    let power_mgr: Option<ZwlrOutputPowerManagerV1> = if daemon && off_secs > 0 {
        globals.bind(&qh, 1..=1, ()).ok()
    } else {
        None
    };
    let notifier: Option<ExtIdleNotifierV1> = if daemon {
        match globals.bind(&qh, 1..=2, ()) {
            Ok(n) => Some(n),
            Err(_) => die("compositor does not support ext-idle-notify-v1 (needed for -a)"),
        }
    } else {
        None
    };
    let (auth_tx, auth_rx) = channel::channel::<bool>();

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        compositor,
        shm,
        pool,
        lock_state: SessionLockState::new(&globals, &qh),
        qh: qh.clone(),
        keyboard: None,
        session: None,
        surfaces: Vec::new(),
        locked: false,
        password: Vec::new(),
        color: INIT,
        failure: false,
        verifying: false,
        user: current_user(),
        auth_tx,
        daemon,
        lock_secs,
        off_secs,
        notifier,
        idle: Vec::new(),
        power_mgr,
        powers: HashMap::new(),
        blanked: false,
        exit: None,
    };
    handle
        .insert_source(auth_rx, |ev, _, app: &mut App| {
            if let ChanEvent::Msg(ok) = ev {
                app.on_auth(ok);
            }
        })
        .expect("auth channel");

    if !daemon {
        app.start_lock();
    }
    while app.exit.is_none() {
        if let Err(e) = event_loop.dispatch(None, &mut app) {
            eprintln!("lock: connection lost: {}", e);
            break;
        }
    }
    let _ = conn.roundtrip();
    std::process::exit(app.exit.unwrap_or(1));
}
