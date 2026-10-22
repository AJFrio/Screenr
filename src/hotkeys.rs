use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConnectionExt, EventMask, GrabMode, ModMask, Window,
};
use x11rb::protocol::Event;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyAction {
    GoHome,        // Ctrl + Alt
    StopMirroring, // Ctrl + Escape
    GoBack,        // Ctrl + Backspace
    AppSwitcher,   // Ctrl + Tab
}

// X11 hardware keycodes (evdev standard on Linux)
const KEYCODE_ESCAPE: u8 = 9;
const KEYCODE_BACKSPACE: u8 = 22;
const KEYCODE_TAB: u8 = 23;
const KEYCODE_CTRL_L: u8 = 37;
const KEYCODE_ALT_L: u8 = 64;
const KEYCODE_CTRL_R: u8 = 105;
const KEYCODE_ALT_R: u8 = 108;

const CTRL_COMBO_KEYCODES: [u8; 5] = [
    KEYCODE_ALT_L,
    KEYCODE_ALT_R,
    KEYCODE_ESCAPE,
    KEYCODE_BACKSPACE,
    KEYCODE_TAB,
];

const ALT_COMBO_KEYCODES: [u8; 2] = [KEYCODE_CTRL_L, KEYCODE_CTRL_R];

pub struct HotkeyWatcher {
    pub rx: Receiver<HotkeyAction>,
}

impl HotkeyWatcher {
    pub fn spawn(ctx: eframe::egui::Context, adb_bin: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = run_x11_hotkey_loop(tx, ctx, adb_bin);
        });
        Self { rx }
    }
}

fn run_x11_hotkey_loop(
    tx: Sender<HotkeyAction>,
    ctx: eframe::egui::Context,
    adb_bin: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    let (conn, screen_num) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen_num].root;

    let net_active_window = conn
        .intern_atom(false, b"_NET_ACTIVE_WINDOW")?
        .reply()?
        .atom;
    let net_wm_name = conn.intern_atom(false, b"_NET_WM_NAME")?.reply()?.atom;
    let utf8_string = conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;

    conn.change_window_attributes(
        root,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?;
    conn.flush()?;

    let mut is_grabbed = false;
    let mut last_home_trigger = Instant::now() - Duration::from_secs(5);

    update_grab_state(
        &conn,
        root,
        net_active_window,
        net_wm_name,
        utf8_string,
        &mut is_grabbed,
    );

    loop {
        while let Some(event) = conn.poll_for_event()? {
            match event {
                Event::PropertyNotify(ev) if ev.atom == net_active_window => {
                    update_grab_state(
                        &conn,
                        root,
                        net_active_window,
                        net_wm_name,
                        utf8_string,
                        &mut is_grabbed,
                    );
                }
                Event::KeyPress(ev) => match ev.detail {
                    KEYCODE_ALT_L | KEYCODE_ALT_R | KEYCODE_CTRL_L | KEYCODE_CTRL_R => {
                        // Debounce slightly in case both modifier key events fire rapidly
                        if last_home_trigger.elapsed() >= Duration::from_millis(250) {
                            last_home_trigger = Instant::now();
                            send_adb_keyevent_async(&adb_bin, "KEYCODE_HOME");
                            let _ = tx.send(HotkeyAction::GoHome);
                            ctx.request_repaint();
                        }
                    }
                    KEYCODE_ESCAPE => {
                        if is_grabbed {
                            ungrab_shortcuts(&conn, root);
                            is_grabbed = false;
                        }
                        if let Some(active_win) = get_active_window(&conn, root, net_active_window)
                        {
                            let _ = conn.destroy_window(active_win);
                            let _ = conn.flush();
                        }
                        lock_phone_async(&adb_bin);
                        let _ = tx.send(HotkeyAction::StopMirroring);
                        ctx.request_repaint();
                    }
                    KEYCODE_BACKSPACE => {
                        send_adb_keyevent_async(&adb_bin, "KEYCODE_BACK");
                        let _ = tx.send(HotkeyAction::GoBack);
                        ctx.request_repaint();
                    }
                    KEYCODE_TAB => {
                        send_adb_keyevent_async(&adb_bin, "KEYCODE_APP_SWITCH");
                        let _ = tx.send(HotkeyAction::AppSwitcher);
                        ctx.request_repaint();
                    }
                    _ => {}
                },
                _ => {}
            }
        }

        update_grab_state(
            &conn,
            root,
            net_active_window,
            net_wm_name,
            utf8_string,
            &mut is_grabbed,
        );

        thread::sleep(Duration::from_millis(80));
    }
}

fn send_adb_keyevent_async(adb_bin: &PathBuf, keycode: &'static str) {
    let adb = adb_bin.clone();
    thread::spawn(move || {
        let _ = Command::new(&adb)
            .args(["shell", "input", "keyevent", keycode])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
}

fn lock_phone_async(adb_bin: &PathBuf) {
    let adb = adb_bin.clone();
    thread::spawn(move || {
        let _ = Command::new(&adb)
            .args(["shell", "input", "keyevent", "KEYCODE_SLEEP"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        thread::sleep(Duration::from_millis(300));
        let _ = Command::new(&adb)
            .args(["shell", "input", "keyevent", "KEYCODE_SLEEP"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
}

fn update_grab_state<C: Connection>(
    conn: &C,
    root: Window,
    net_active_window: u32,
    net_wm_name: u32,
    utf8_string: u32,
    is_grabbed: &mut bool,
) {
    let should_grab =
        is_mirror_window_active(conn, root, net_active_window, net_wm_name, utf8_string);
    if should_grab && !*is_grabbed {
        grab_shortcuts(conn, root);
        *is_grabbed = true;
    } else if !should_grab && *is_grabbed {
        ungrab_shortcuts(conn, root);
        *is_grabbed = false;
    }
}

fn get_active_window<C: Connection>(
    conn: &C,
    root: Window,
    net_active_window: u32,
) -> Option<Window> {
    let cookie = conn
        .get_property(false, root, net_active_window, AtomEnum::WINDOW, 0, 1)
        .ok()?;
    let reply = cookie.reply().ok()?;
    let mut vals = reply.value32()?;
    let win = vals.next()?;
    if win == 0 {
        None
    } else {
        Some(win)
    }
}

fn is_mirror_window_active<C: Connection>(
    conn: &C,
    root: Window,
    net_active_window: u32,
    net_wm_name: u32,
    utf8_string: u32,
) -> bool {
    let Some(active_win) = get_active_window(conn, root, net_active_window) else {
        return false;
    };

    let title = get_window_title(conn, active_win, net_wm_name, utf8_string);
    title.contains("— Screenr")
        || title.contains("(Screenr)")
        || title.contains("Continuity Camera")
}

fn get_window_title<C: Connection>(
    conn: &C,
    win: Window,
    net_wm_name: u32,
    utf8_string: u32,
) -> String {
    if let Ok(cookie) = conn.get_property(false, win, net_wm_name, utf8_string, 0, 256) {
        if let Ok(reply) = cookie.reply() {
            if !reply.value.is_empty() {
                return String::from_utf8_lossy(&reply.value).to_string();
            }
        }
    }
    if let Ok(cookie) = conn.get_property(false, win, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 256) {
        if let Ok(reply) = cookie.reply() {
            if !reply.value.is_empty() {
                return String::from_utf8_lossy(&reply.value).to_string();
            }
        }
    }
    String::new()
}

fn grab_shortcuts<C: Connection>(conn: &C, root: Window) {
    // 1. Holding Control + pressing Alt / Esc / Backspace / Tab
    let ctrl_modifiers = [
        ModMask::CONTROL,
        ModMask::CONTROL | ModMask::M2,
        ModMask::CONTROL | ModMask::LOCK,
        ModMask::CONTROL | ModMask::M2 | ModMask::LOCK,
    ];

    for &key in &CTRL_COMBO_KEYCODES {
        for &mods in &ctrl_modifiers {
            let _ = conn.grab_key(false, root, mods, key, GrabMode::ASYNC, GrabMode::ASYNC);
        }
    }

    // 2. Holding Alt (M1) + pressing Control (so Ctrl+Alt works in either order!)
    let alt_modifiers = [
        ModMask::M1,
        ModMask::M1 | ModMask::M2,
        ModMask::M1 | ModMask::LOCK,
        ModMask::M1 | ModMask::M2 | ModMask::LOCK,
    ];

    for &key in &ALT_COMBO_KEYCODES {
        for &mods in &alt_modifiers {
            let _ = conn.grab_key(false, root, mods, key, GrabMode::ASYNC, GrabMode::ASYNC);
        }
    }

    let _ = conn.flush();
}

fn ungrab_shortcuts<C: Connection>(conn: &C, root: Window) {
    for &key in &CTRL_COMBO_KEYCODES {
        let _ = conn.ungrab_key(key, root, ModMask::ANY);
    }
    for &key in &ALT_COMBO_KEYCODES {
        let _ = conn.ungrab_key(key, root, ModMask::ANY);
    }
    let _ = conn.flush();
}
