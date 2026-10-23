//! Pixel-style body around the scrcpy mirror window.
//!
//! scrcpy stays the video and input surface. This thread reparents that window
//! into a borderless phone body: obsidian metal, squircle glass, centered
//! punch-hole, volume rocker, and power button.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use tiny_skia::{
    BlendMode, Color, FillRule, GradientStop, LinearGradient, Paint, Path, PathBuilder, Pixmap,
    Point, SpreadMode, Stroke, Transform,
};
use x11rb::connection::Connection;
use x11rb::protocol::shape::{ConnectionExt as _, SK, SO};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageEvent, ColormapAlloc, ConfigureWindowAux, ConnectionExt as _,
    CreateGCAux, CreateWindowAux, EventMask, Gcontext, GrabMode, ImageFormat, InputFocus, PropMode,
    Rectangle, StackMode, Visualid, Window, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

type FrameResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub struct PhoneFrame {
    stop: Arc<AtomicBool>,
}

impl PhoneFrame {
    pub fn attach(title: String, adb: PathBuf, serial: String, above: bool) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let _ = thread::Builder::new()
            .name("phone-frame".into())
            .spawn(move || {
                if let Err(err) = run(flag, title, adb, serial, above) {
                    eprintln!("screenr: phone frame unavailable ({err})");
                }
            });
        Self { stop }
    }
}

impl Drop for PhoneFrame {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn run(
    stop: Arc<AtomicBool>,
    title: String,
    adb: PathBuf,
    serial: String,
    above: bool,
) -> FrameResult<()> {
    let (conn, screen_num) = x11rb::connect(None)?;
    conn.shape_query_version()?.reply()?;
    let root = conn.setup().roots[screen_num].root;
    let child = wait_for_window(&conn, root, &stop, &title)?;
    let mut chrome = Chrome::wrap(conn, root, child, title, adb, serial, above)?;
    chrome.event_loop(&stop)?;
    chrome.destroy();
    Ok(())
}

struct Chrome {
    conn: RustConnection,
    root: Window,
    frame: Window,
    child: Window,
    gc: Gcontext,
    colormap: u32,
    layout: PhoneLayout,
    pixels: Vec<u8>,
    adb: PathBuf,
    serial: String,
    protocols: Atom,
    delete: Atom,
    last_child: (i32, i32),
    last_frame: (i32, i32),
    dragging: bool,
}

impl Chrome {
    fn wrap(
        conn: RustConnection,
        root: Window,
        child: Window,
        title: String,
        adb: PathBuf,
        serial: String,
        above: bool,
    ) -> FrameResult<Self> {
        let geo = conn.get_geometry(child)?.reply()?;
        let origin = root_origin(&conn, child, root)?;
        let layout = layout_for(geo.width as u32, geo.height as u32);
        let (pixels, bounds, input) = render(&layout, None);
        let visual = argb_visual(&conn, root).ok_or("no 32-bit visual for the phone body")?;
        let colormap = conn.generate_id()?;
        conn.create_colormap(ColormapAlloc::NONE, colormap, root, visual)?
            .check()?;

        let frame = conn.generate_id()?;
        let frame_x = origin.0 - layout.screen_x;
        let frame_y = origin.1 - layout.screen_y;
        conn.create_window(
            32,
            frame,
            root,
            clamp_i16(frame_x),
            clamp_i16(frame_y),
            layout.frame_w as u16,
            layout.frame_h as u16,
            0,
            WindowClass::INPUT_OUTPUT,
            visual,
            &CreateWindowAux::new()
                .background_pixel(0)
                .border_pixel(0)
                .colormap(colormap)
                .override_redirect(0u32)
                .event_mask(
                    EventMask::EXPOSURE
                        | EventMask::BUTTON_PRESS
                        | EventMask::BUTTON_RELEASE
                        | EventMask::POINTER_MOTION
                        | EventMask::STRUCTURE_NOTIFY,
                ),
        )?
        .check()?;

        let gc = conn.generate_id()?;
        conn.create_gc(gc, frame, &CreateGCAux::new())?.check()?;

        let mut chrome = Self {
            conn,
            root,
            frame,
            child,
            gc,
            colormap,
            layout,
            pixels,
            adb,
            serial,
            protocols: 0,
            delete: 0,
            last_child: origin,
            last_frame: (frame_x, frame_y),
            dragging: false,
        };
        chrome.install_hints(&title)?;
        chrome.put_pixels()?;
        apply_shape(&chrome.conn, frame, SK::BOUNDING, &bounds)?;
        apply_shape(&chrome.conn, frame, SK::INPUT, &input)?;
        // Stay a separate top-level window. Reparenting the mirror is undone by
        // the compositor, so the body is stacked over the video with the glass cut out.
        chrome.shape_child()?;
        chrome.conn.map_window(frame)?.check()?;
        chrome.skip_taskbar()?;
        chrome.raise_above_child()?;
        chrome.sync_to_child()?;
        if above {
            chrome.request_above()?;
        }
        chrome.focus_child()?;
        chrome.conn.flush()?;
        Ok(chrome)
    }

    fn install_hints(&mut self, title: &str) -> FrameResult<()> {
        let conn = &self.conn;
        let utf8 = intern(conn, b"UTF8_STRING")?;
        let net_name = intern(conn, b"_NET_WM_NAME")?;
        conn.change_property8(
            PropMode::REPLACE,
            self.frame,
            net_name,
            utf8,
            title.as_bytes(),
        )?;
        conn.change_property8(
            PropMode::REPLACE,
            self.frame,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            b"Screenr",
        )?;
        conn.change_property8(
            PropMode::REPLACE,
            self.frame,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            b"screenr\0Screenr\0",
        )?;

        // Input hint + NormalState. The mirror window should be focusable so the
        // existing shortcuts keep working, while keystrokes are handed to scrcpy.
        let wm_hints: [u32; 9] = [1 | 2, 1, 1, 0, 0, 0, 0, 0, 0];
        conn.change_property32(
            PropMode::REPLACE,
            self.frame,
            AtomEnum::WM_HINTS,
            AtomEnum::WM_HINTS,
            &wm_hints,
        )?;

        let motif = intern(conn, b"_MOTIF_WM_HINTS")?;
        // functions (move + close) and no decorations, so the body is the window.
        let motif_hints: [u32; 5] = [1 | 2, 4 | 32, 0, 0, 0];
        conn.change_property32(PropMode::REPLACE, self.frame, motif, motif, &motif_hints)?;

        let win_type = intern(conn, b"_NET_WM_WINDOW_TYPE")?;
        let normal = intern(conn, b"_NET_WM_WINDOW_TYPE_NORMAL")?;
        conn.change_property32(
            PropMode::REPLACE,
            self.frame,
            win_type,
            AtomEnum::ATOM,
            &[normal],
        )?;

        let skip_taskbar = intern(conn, b"_NET_WM_STATE_SKIP_TASKBAR")?;
        let skip_pager = intern(conn, b"_NET_WM_STATE_SKIP_PAGER")?;
        let state = intern(conn, b"_NET_WM_STATE")?;
        conn.change_property32(
            PropMode::REPLACE,
            self.frame,
            state,
            AtomEnum::ATOM,
            &[skip_taskbar, skip_pager],
        )?;

        self.protocols = intern(conn, b"WM_PROTOCOLS")?;
        self.delete = intern(conn, b"WM_DELETE_WINDOW")?;
        conn.change_property32(
            PropMode::REPLACE,
            self.frame,
            self.protocols,
            AtomEnum::ATOM,
            &[self.delete],
        )?;
        self.lock_size()?;
        Ok(())
    }

    fn lock_size(&self) -> FrameResult<()> {
        let wm_size = intern(&self.conn, b"WM_SIZE_HINTS")?;
        let w = self.layout.frame_w;
        let h = self.layout.frame_h;
        let hints = [
            (1 << 4) | (1 << 5),
            0,
            0,
            w,
            h,
            w,
            h,
            w,
            h,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        self.conn.change_property32(
            PropMode::REPLACE,
            self.frame,
            AtomEnum::WM_NORMAL_HINTS,
            wm_size,
            &hints,
        )?;
        Ok(())
    }

    fn skip_taskbar(&self) -> FrameResult<()> {
        let state = intern(&self.conn, b"_NET_WM_STATE")?;
        let skip_taskbar = intern(&self.conn, b"_NET_WM_STATE_SKIP_TASKBAR")?;
        let skip_pager = intern(&self.conn, b"_NET_WM_STATE_SKIP_PAGER")?;
        let event =
            ClientMessageEvent::new(32, self.frame, state, [1, skip_taskbar, skip_pager, 1, 0]);
        self.conn.send_event(
            false,
            self.root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )?;
        Ok(())
    }

    fn raise_above_child(&self) -> FrameResult<()> {
        if let Ok(cookie) = self.conn.configure_window(
            self.frame,
            &ConfigureWindowAux::new()
                .sibling(self.child)
                .stack_mode(StackMode::ABOVE),
        ) {
            let _ = cookie.check();
        }
        Ok(())
    }

    fn move_root(&self, window: Window, x: i32, y: i32) -> FrameResult<()> {
        let parent = self.conn.query_tree(window)?.reply()?.parent;
        let (parent_x, parent_y) = if parent == self.root {
            (0, 0)
        } else {
            root_origin(&self.conn, parent, self.root)?
        };
        self.conn.configure_window(
            window,
            &ConfigureWindowAux::new().x(x - parent_x).y(y - parent_y),
        )?;
        Ok(())
    }

    fn sync_to_child(&mut self) -> FrameResult<()> {
        let child_at = root_origin(&self.conn, self.child, self.root)?;
        let frame_at = (
            child_at.0 - self.layout.screen_x,
            child_at.1 - self.layout.screen_y,
        );
        self.move_root(self.frame, frame_at.0, frame_at.1)?;
        self.last_child = child_at;
        self.last_frame = frame_at;
        Ok(())
    }

    fn shape_child(&self) -> FrameResult<()> {
        let rects = screen_mask(&self.layout);
        apply_shape(&self.conn, self.child, SK::BOUNDING, &rects)?;
        apply_shape(&self.conn, self.child, SK::INPUT, &rects)?;
        Ok(())
    }

    fn put_pixels(&self) -> FrameResult<()> {
        let width = self.layout.frame_w;
        let stride = width as usize * 4;
        let max_bytes = self.conn.setup().maximum_request_length as usize * 4;
        let rows = ((max_bytes.saturating_sub(256)) / stride).max(1);
        let mut y = 0u32;
        while y < self.layout.frame_h {
            let h = ((self.layout.frame_h - y) as usize).min(rows) as u32;
            let start = y as usize * stride;
            let end = start + h as usize * stride;
            self.conn
                .put_image(
                    ImageFormat::Z_PIXMAP,
                    self.frame,
                    self.gc,
                    width as u16,
                    h as u16,
                    0,
                    y as i16,
                    0,
                    32,
                    &self.pixels[start..end],
                )?
                .check()?;
            y += h;
        }
        Ok(())
    }

    fn request_above(&self) -> FrameResult<()> {
        let state = intern(&self.conn, b"_NET_WM_STATE")?;
        let above = intern(&self.conn, b"_NET_WM_STATE_ABOVE")?;
        let event = ClientMessageEvent::new(32, self.frame, state, [1, above, 0, 1, 0]);
        self.conn.send_event(
            false,
            self.root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )?;
        Ok(())
    }

    fn focus_child(&self) -> FrameResult<()> {
        self.conn
            .set_input_focus(InputFocus::PARENT, self.child, x11rb::CURRENT_TIME)?;
        Ok(())
    }

    fn event_loop(&mut self, stop: &AtomicBool) -> FrameResult<()> {
        let mut drag: Option<Drag> = None;
        let mut pressed: Option<SideButton> = None;
        while !stop.load(Ordering::SeqCst) {
            self.conn.flush()?;
            while let Some(event) = self.conn.poll_for_event()? {
                match event {
                    Event::Expose(expose) if expose.window == self.frame && expose.count == 0 => {
                        self.put_pixels()?;
                    }
                    Event::ButtonPress(press) if press.event == self.frame && press.detail == 1 => {
                        let x = press.event_x as f32;
                        let y = press.event_y as f32;
                        if !self.layout.screen.contains(x, y) {
                            let frame_at = root_origin(&self.conn, self.frame, self.root)?;
                            let child_at = root_origin(&self.conn, self.child, self.root)?;
                            let button = self.layout.hit_button(x, y);
                            self.dragging = true;
                            drag = Some(Drag {
                                root_x: press.root_x as i32,
                                root_y: press.root_y as i32,
                                frame_x: frame_at.0,
                                frame_y: frame_at.1,
                                child_x: child_at.0,
                                child_y: child_at.1,
                                moved: false,
                                button,
                            });
                            if button != pressed {
                                pressed = button;
                                self.redraw(pressed)?;
                            }
                            let _ = self.conn.grab_pointer(
                                true,
                                self.frame,
                                EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
                                GrabMode::ASYNC,
                                GrabMode::ASYNC,
                                x11rb::NONE,
                                x11rb::NONE,
                                x11rb::CURRENT_TIME,
                            );
                        }
                    }
                    Event::MotionNotify(motion) => {
                        if let Some(drag) = drag.as_mut() {
                            let dx = motion.root_x as i32 - drag.root_x;
                            let dy = motion.root_y as i32 - drag.root_y;
                            if dx.abs() + dy.abs() > 4 {
                                drag.moved = true;
                                if pressed.take().is_some() {
                                    self.redraw(None)?;
                                }
                                self.move_root(self.frame, drag.frame_x + dx, drag.frame_y + dy)?;
                                self.move_root(self.child, drag.child_x + dx, drag.child_y + dy)?;
                                self.last_frame = (drag.frame_x + dx, drag.frame_y + dy);
                                self.last_child = (drag.child_x + dx, drag.child_y + dy);
                            }
                        }
                    }
                    Event::ButtonRelease(release) if release.detail == 1 => {
                        self.dragging = false;
                        let _ = self.conn.ungrab_pointer(x11rb::CURRENT_TIME);
                        if let Some(drag) = drag.take() {
                            if let Some(button) = drag.button {
                                if !drag.moved {
                                    send_key(&self.adb, &self.serial, button.keycode());
                                }
                            }
                        }
                        if pressed.take().is_some() {
                            self.redraw(None)?;
                        }
                        self.focus_child()?;
                    }
                    Event::ClientMessage(message)
                        if message.type_ == self.protocols
                            && message.data.as_data32()[0] == self.delete =>
                    {
                        let _ = self.conn.destroy_window(self.child);
                        return Ok(());
                    }
                    Event::DestroyNotify(destroyed) if destroyed.window == self.frame => {
                        let _ = self.conn.destroy_window(self.child);
                        return Ok(());
                    }
                    _ => {}
                }
            }

            if !self.follow_child()? {
                break;
            }
            thread::sleep(Duration::from_millis(16));
        }
        Ok(())
    }

    /// Keep the glass aligned if scrcpy resizes (rotation) or slides its window.
    /// Returns false when the mirror window is gone.
    fn follow_child(&mut self) -> FrameResult<bool> {
        if self.dragging {
            return Ok(true);
        }
        let geo = match self.conn.get_geometry(self.child) {
            Ok(cookie) => match cookie.reply() {
                Ok(geo) => geo,
                Err(_) => return Ok(false),
            },
            Err(_) => return Ok(false),
        };
        let width = geo.width as u32;
        let height = geo.height as u32;
        if width < 32 || height < 32 {
            return Ok(true);
        }
        if width != self.layout.screen_w || height != self.layout.screen_h {
            self.layout = layout_for(width, height);
            self.redraw(None)?;
            self.lock_size()?;
            self.conn.configure_window(
                self.frame,
                &ConfigureWindowAux::new()
                    .width(self.layout.frame_w)
                    .height(self.layout.frame_h),
            )?;
            self.shape_child()?;
            self.sync_to_child()?;
            self.raise_above_child()?;
            return Ok(true);
        }

        let child_at = root_origin(&self.conn, self.child, self.root)?;
        let frame_at = root_origin(&self.conn, self.frame, self.root)?;
        let child_moved = (child_at.0 - self.last_child.0).abs() > 2
            || (child_at.1 - self.last_child.1).abs() > 2;
        let frame_moved = (frame_at.0 - self.last_frame.0).abs() > 2
            || (frame_at.1 - self.last_frame.1).abs() > 2;
        if child_moved {
            self.sync_to_child()?;
        } else if frame_moved {
            let dx = frame_at.0 - self.last_frame.0;
            let dy = frame_at.1 - self.last_frame.1;
            let next = (self.last_child.0 + dx, self.last_child.1 + dy);
            self.move_root(self.child, next.0, next.1)?;
            self.last_frame = frame_at;
            self.last_child = next;
        }
        Ok(true)
    }

    fn redraw(&mut self, pressed: Option<SideButton>) -> FrameResult<()> {
        let (pixels, bounds, input) = render(&self.layout, pressed);
        self.pixels = pixels;
        self.put_pixels()?;
        apply_shape(&self.conn, self.frame, SK::BOUNDING, &bounds)?;
        apply_shape(&self.conn, self.frame, SK::INPUT, &input)?;
        Ok(())
    }

    fn destroy(&self) {
        let _ = self.conn.destroy_window(self.frame);
        let _ = self.conn.free_gc(self.gc);
        let _ = self.conn.free_colormap(self.colormap);
        let _ = self.conn.flush();
    }
}

struct Drag {
    root_x: i32,
    root_y: i32,
    frame_x: i32,
    frame_y: i32,
    child_x: i32,
    child_y: i32,
    moved: bool,
    button: Option<SideButton>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SideButton {
    VolumeUp,
    VolumeDown,
    Power,
}

impl SideButton {
    fn keycode(self) -> &'static str {
        match self {
            Self::VolumeUp => "KEYCODE_VOLUME_UP",
            Self::VolumeDown => "KEYCODE_VOLUME_DOWN",
            Self::Power => "KEYCODE_POWER",
        }
    }
}

fn send_key(adb: &PathBuf, serial: &str, keycode: &str) {
    let adb = adb.clone();
    let serial = serial.to_string();
    let keycode = keycode.to_string();
    let _ = thread::Builder::new()
        .name("phone-key".into())
        .spawn(move || {
            let _ = Command::new(adb)
                .args(["-s", &serial, "shell", "input", "keyevent", &keycode])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        });
}

fn wait_for_window(
    conn: &RustConnection,
    root: Window,
    stop: &AtomicBool,
    title: &str,
) -> FrameResult<Window> {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && !stop.load(Ordering::SeqCst) {
        if let Some(window) = find_window(conn, root, title)? {
            return Ok(window);
        }
        thread::sleep(Duration::from_millis(40));
    }
    Err("mirror window did not appear".into())
}

fn find_window(conn: &RustConnection, root: Window, title: &str) -> FrameResult<Option<Window>> {
    let list_atom = intern(conn, b"_NET_CLIENT_LIST")?;
    let reply = conn
        .get_property(false, root, list_atom, AtomEnum::WINDOW, 0, 2048)?
        .reply()?;
    if let Some(windows) = reply.value32() {
        for window in windows {
            if window_title(conn, window)?.as_deref() == Some(title) {
                return Ok(Some(window));
            }
        }
    }
    let tree = conn.query_tree(root)?.reply()?;
    for window in tree.children {
        if window_title(conn, window)?.as_deref() == Some(title) {
            return Ok(Some(window));
        }
    }
    Ok(None)
}

fn window_title(conn: &RustConnection, window: Window) -> FrameResult<Option<String>> {
    let utf8 = intern(conn, b"UTF8_STRING")?;
    let net_name = intern(conn, b"_NET_WM_NAME")?;
    if let Some(title) = read_title(conn, window, net_name, utf8)? {
        return Ok(Some(title));
    }
    read_title(
        conn,
        window,
        AtomEnum::WM_NAME.into(),
        AtomEnum::STRING.into(),
    )
}

fn read_title(
    conn: &RustConnection,
    window: Window,
    atom: Atom,
    kind: Atom,
) -> FrameResult<Option<String>> {
    let reply = match conn.get_property(false, window, atom, kind, 0, 512) {
        Ok(cookie) => match cookie.reply() {
            Ok(reply) => reply,
            Err(_) => return Ok(None),
        },
        Err(_) => return Ok(None),
    };
    if reply.value.is_empty() {
        return Ok(None);
    }
    let title = String::from_utf8_lossy(&reply.value)
        .trim_matches('\u{0}')
        .trim()
        .to_string();
    Ok(Some(title))
}

fn root_origin(conn: &RustConnection, window: Window, root: Window) -> FrameResult<(i32, i32)> {
    let point = conn.translate_coordinates(window, root, 0, 0)?.reply()?;
    Ok((point.dst_x as i32, point.dst_y as i32))
}

fn intern(conn: &RustConnection, name: &[u8]) -> FrameResult<Atom> {
    Ok(conn.intern_atom(false, name)?.reply()?.atom)
}

fn argb_visual(conn: &RustConnection, root: Window) -> Option<Visualid> {
    let screen = conn
        .setup()
        .roots
        .iter()
        .find(|screen| screen.root == root)?;
    for depth in &screen.allowed_depths {
        if depth.depth != 32 {
            continue;
        }
        for visual in &depth.visuals {
            if visual.red_mask == 0x00ff_0000
                && visual.green_mask == 0x0000_ff00
                && visual.blue_mask == 0x0000_00ff
            {
                return Some(visual.visual_id);
            }
        }
    }
    None
}

fn clamp_i16(value: i32) -> i16 {
    value.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

fn apply_shape(
    conn: &RustConnection,
    window: Window,
    kind: SK,
    rects: &[Rectangle],
) -> FrameResult<()> {
    if rects.is_empty() {
        return Ok(());
    }
    let mut operation = SO::SET;
    for chunk in rects.chunks(480) {
        conn.shape_rectangles(
            operation,
            kind,
            x11rb::protocol::xproto::ClipOrdering::Y_SORTED,
            window,
            0,
            0,
            chunk,
        )?
        .check()?;
        operation = SO::UNION;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct GfxRect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl GfxRect {
    fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }

    fn expand(self, amount: f32) -> Self {
        Self {
            x: self.x - amount,
            y: self.y - amount,
            w: self.w + amount * 2.0,
            h: self.h + amount * 2.0,
        }
    }
}

struct PhoneLayout {
    frame_w: u32,
    frame_h: u32,
    screen_x: i32,
    screen_y: i32,
    screen_w: u32,
    screen_h: u32,
    body: GfxRect,
    screen: GfxRect,
    body_radius: f32,
    screen_radius: f32,
    landscape: bool,
    camera: (f32, f32, f32),
    rocker: GfxRect,
    volume_up: GfxRect,
    volume_down: GfxRect,
    power: GfxRect,
    port: GfxRect,
}

impl PhoneLayout {
    fn hit_button(&self, x: f32, y: f32) -> Option<SideButton> {
        let slop = 5.0;
        if self.volume_up.expand(slop).contains(x, y) {
            Some(SideButton::VolumeUp)
        } else if self.volume_down.expand(slop).contains(x, y) {
            Some(SideButton::VolumeDown)
        } else if self.power.expand(slop).contains(x, y) {
            Some(SideButton::Power)
        } else {
            None
        }
    }
}

fn layout_for(screen_w: u32, screen_h: u32) -> PhoneLayout {
    let short = (screen_w.min(screen_h) as f32).max(1.0);
    let bezel = (short * 0.046).clamp(18.0, 32.0);
    let body_radius = (short * 0.162).clamp(44.0, 74.0).min(bezel * 3.15);
    let screen_radius = (body_radius - bezel * 0.72).max(16.0);
    let shadow = 46.0;
    let body = GfxRect {
        x: shadow,
        y: shadow,
        w: screen_w as f32 + bezel * 2.0,
        h: screen_h as f32 + bezel * 2.0,
    };
    let screen = GfxRect {
        x: shadow + bezel,
        y: shadow + bezel,
        w: screen_w as f32,
        h: screen_h as f32,
    };
    let landscape = screen.w > screen.h * 1.08;
    let protrude = (bezel * 0.2).clamp(3.2, 5.0);
    let thick = protrude + 2.4;
    let (rocker, volume_up, volume_down, power, port) = if landscape {
        let length = body.w * 0.16;
        let rocker = GfxRect {
            x: body.x + body.w * 0.2,
            y: body.y - protrude,
            w: length,
            h: thick,
        };
        let up = GfxRect {
            x: rocker.x,
            y: rocker.y,
            w: rocker.w * 0.48,
            h: rocker.h,
        };
        let down = GfxRect {
            x: rocker.x + rocker.w * 0.52,
            y: rocker.y,
            w: rocker.w * 0.48,
            h: rocker.h,
        };
        let power = GfxRect {
            x: body.x + body.w * 0.22,
            y: body.y + body.h - 2.2,
            w: body.w * 0.075,
            h: thick,
        };
        let port = GfxRect {
            x: body.x + body.w - bezel * 0.72,
            y: body.y + body.h * 0.5 - screen.w * 0.07,
            w: bezel * 0.34,
            h: screen.w * 0.14,
        };
        (rocker, up, down, power, port)
    } else {
        let length = body.h * 0.155;
        let rocker = GfxRect {
            x: body.x - protrude,
            y: body.y + body.h * 0.2,
            w: thick,
            h: length,
        };
        let up = GfxRect {
            x: rocker.x,
            y: rocker.y,
            w: rocker.w,
            h: rocker.h * 0.48,
        };
        let down = GfxRect {
            x: rocker.x,
            y: rocker.y + rocker.h * 0.52,
            w: rocker.w,
            h: rocker.h * 0.48,
        };
        let power_h = body.h * 0.074;
        let power = GfxRect {
            x: body.x + body.w - 2.2,
            y: rocker.y + (rocker.h - power_h) * 0.42,
            w: thick,
            h: power_h,
        };
        let port_w = screen.w * 0.15;
        let port_h = (bezel * 0.3).clamp(4.5, 7.5);
        let port = GfxRect {
            x: body.x + (body.w - port_w) * 0.5,
            y: body.y + body.h - bezel * 0.68,
            w: port_w,
            h: port_h,
        };
        (rocker, up, down, power, port)
    };
    let cam_r = (short * 0.023).clamp(7.5, 15.0);
    let camera = if landscape {
        (screen.x + cam_r * 2.2, screen.y + screen.h * 0.5, cam_r)
    } else {
        (screen.x + screen.w * 0.5, screen.y + cam_r * 2.25, cam_r)
    };

    PhoneLayout {
        frame_w: (body.w + shadow * 2.0).ceil() as u32,
        frame_h: (body.h + shadow * 2.0).ceil() as u32,
        screen_x: screen.x.round() as i32,
        screen_y: screen.y.round() as i32,
        screen_w,
        screen_h,
        body,
        screen,
        body_radius,
        screen_radius,
        landscape,
        camera,
        rocker,
        volume_up,
        volume_down,
        power,
        port,
    }
}

fn render(
    layout: &PhoneLayout,
    pressed: Option<SideButton>,
) -> (Vec<u8>, Vec<Rectangle>, Vec<Rectangle>) {
    let mut pixmap = Pixmap::new(layout.frame_w, layout.frame_h).expect("phone frame size");
    draw_shadow(&mut pixmap, layout);
    draw_body(&mut pixmap, layout);
    draw_glass_lip(&mut pixmap, layout);
    draw_buttons(&mut pixmap, layout, pressed);
    draw_port(&mut pixmap, layout);
    // The mirror window shows through this opening. Corner wedges stay painted
    // so the video's square corners disappear under the metal.
    if let Some(path) = squircle(layout.screen, layout.screen_radius) {
        let mut clear = Paint::default();
        clear.blend_mode = BlendMode::Clear;
        clear.anti_alias = true;
        clear.force_hq_pipeline = true;
        clear.set_color_rgba8(0, 0, 0, 255);
        pixmap.fill_path(
            &path,
            &clear,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    draw_camera(&mut pixmap, layout.camera);

    let bounds = alpha_rects(pixmap.data(), layout.frame_w, layout.frame_h, 12);
    let input = alpha_rects(pixmap.data(), layout.frame_w, layout.frame_h, 180);
    rgba_to_bgra(pixmap.data_mut());
    (pixmap.take(), bounds, input)
}

fn draw_shadow(pixmap: &mut Pixmap, layout: &PhoneLayout) {
    let layers = 16;
    for index in (1..=layers).rev() {
        let t = index as f32 / layers as f32;
        let grow = t * 30.0;
        let alpha = (22.0 * (1.0 - t).powf(1.7)) as u8;
        if alpha == 0 {
            continue;
        }
        let rect = layout.body.expand(grow);
        let Some(path) = squircle(rect, layout.body_radius + grow * 0.65) else {
            continue;
        };
        let mut paint = Paint::default();
        paint.anti_alias = true;
        // Sit the shadow slightly below the body so the phone reads as resting on the desk.
        let shift = Transform::from_translate(0.0, 8.0);
        paint.set_color_rgba8(0, 0, 0, alpha);
        pixmap.fill_path(&path, &paint, FillRule::Winding, shift, None);
    }
}

fn draw_body(pixmap: &mut Pixmap, layout: &PhoneLayout) {
    let Some(path) = squircle(layout.body, layout.body_radius) else {
        return;
    };
    let mut paint = Paint::default();
    paint.anti_alias = true;
    paint.force_hq_pipeline = true;
    if let Some(shader) = LinearGradient::new(
        Point::from_xy(layout.body.x, layout.body.y),
        Point::from_xy(
            layout.body.x + layout.body.w,
            layout.body.y + layout.body.h * 0.85,
        ),
        vec![
            GradientStop::new(0.0, Color::from_rgba8(96, 99, 105, 255)),
            GradientStop::new(0.16, Color::from_rgba8(58, 60, 66, 255)),
            GradientStop::new(0.48, Color::from_rgba8(36, 37, 41, 255)),
            GradientStop::new(1.0, Color::from_rgba8(16, 17, 19, 255)),
        ],
        SpreadMode::Pad,
        Transform::identity(),
    ) {
        paint.shader = shader;
    }
    pixmap.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    let mut rim = Paint::default();
    rim.anti_alias = true;
    rim.set_color_rgba8(255, 255, 255, 42);
    let stroke = Stroke {
        width: 1.25,
        ..Stroke::default()
    };
    pixmap.stroke_path(&path, &rim, &stroke, Transform::identity(), None);
}

fn draw_glass_lip(pixmap: &mut Pixmap, layout: &PhoneLayout) {
    let Some(path) = squircle(layout.screen, layout.screen_radius) else {
        return;
    };
    let mut paint = Paint::default();
    paint.anti_alias = true;
    paint.set_color_rgba8(0, 0, 0, 210);
    let stroke = Stroke {
        width: 2.4,
        ..Stroke::default()
    };
    pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
}

fn draw_buttons(pixmap: &mut Pixmap, layout: &PhoneLayout, pressed: Option<SideButton>) {
    draw_pill(pixmap, layout.rocker, false);
    // Groove that splits the volume rocker into up and down.
    let mut groove = Paint::default();
    groove.anti_alias = true;
    groove.set_color_rgba8(8, 8, 10, 200);
    if layout.landscape {
        let y = layout.rocker.y + 1.2;
        let x = layout.rocker.x + layout.rocker.w * 0.5;
        if let Some(path) = pill_path(GfxRect {
            x: x - 0.7,
            y,
            w: 1.4,
            h: layout.rocker.h - 2.4,
        }) {
            pixmap.fill_path(
                &path,
                &groove,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
    } else {
        let y = layout.rocker.y + layout.rocker.h * 0.5;
        if let Some(path) = pill_path(GfxRect {
            x: layout.rocker.x + 1.2,
            y: y - 0.7,
            w: layout.rocker.w - 2.4,
            h: 1.4,
        }) {
            pixmap.fill_path(
                &path,
                &groove,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
    }
    if pressed == Some(SideButton::VolumeUp) {
        draw_pill(pixmap, layout.volume_up, true);
    }
    if pressed == Some(SideButton::VolumeDown) {
        draw_pill(pixmap, layout.volume_down, true);
    }
    draw_pill(pixmap, layout.power, pressed == Some(SideButton::Power));
}

fn draw_pill(pixmap: &mut Pixmap, rect: GfxRect, pressed: bool) {
    let Some(path) = pill_path(rect) else {
        return;
    };
    let mut paint = Paint::default();
    paint.anti_alias = true;
    if pressed {
        paint.set_color_rgba8(24, 25, 28, 255);
    } else if let Some(shader) = LinearGradient::new(
        Point::from_xy(rect.x, rect.y),
        Point::from_xy(rect.x + rect.w, rect.y + rect.h),
        vec![
            GradientStop::new(0.0, Color::from_rgba8(132, 136, 142, 255)),
            GradientStop::new(0.45, Color::from_rgba8(74, 76, 82, 255)),
            GradientStop::new(1.0, Color::from_rgba8(34, 35, 38, 255)),
        ],
        SpreadMode::Pad,
        Transform::identity(),
    ) {
        paint.shader = shader;
    } else {
        paint.set_color_rgba8(70, 72, 78, 255);
    }
    pixmap.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
}

fn draw_port(pixmap: &mut Pixmap, layout: &PhoneLayout) {
    let Some(path) = pill_path(layout.port) else {
        return;
    };
    let mut paint = Paint::default();
    paint.anti_alias = true;
    paint.set_color_rgba8(6, 6, 8, 255);
    pixmap.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
    let mut rim = Paint::default();
    rim.anti_alias = true;
    rim.set_color_rgba8(150, 154, 160, 90);
    let stroke = Stroke {
        width: 0.8,
        ..Stroke::default()
    };
    pixmap.stroke_path(&path, &rim, &stroke, Transform::identity(), None);
}

fn draw_camera(pixmap: &mut Pixmap, camera: (f32, f32, f32)) {
    let (cx, cy, radius) = camera;
    fill_circle(pixmap, cx, cy, radius, [4, 4, 5, 255]);
    fill_circle(pixmap, cx, cy, radius * 0.74, [22, 26, 32, 255]);
    fill_circle(pixmap, cx, cy, radius * 0.56, [6, 8, 12, 255]);
    fill_circle(
        pixmap,
        cx - radius * 0.24,
        cy - radius * 0.28,
        radius * 0.16,
        [214, 220, 228, 170],
    );
}

fn fill_circle(pixmap: &mut Pixmap, cx: f32, cy: f32, radius: f32, color: [u8; 4]) {
    let mut builder = PathBuilder::new();
    builder.push_circle(cx, cy, radius);
    let Some(path) = builder.finish() else {
        return;
    };
    let mut paint = Paint::default();
    paint.anti_alias = true;
    paint.set_color_rgba8(color[0], color[1], color[2], color[3]);
    pixmap.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
}

/// Screen mask in the video window's own coordinates. The punch-hole is left
/// out so the camera painted on the body shows through.
fn screen_mask(layout: &PhoneLayout) -> Vec<Rectangle> {
    let width = layout.screen_w;
    let height = layout.screen_h;
    let Some(mut pixmap) = Pixmap::new(width, height) else {
        return Vec::new();
    };
    let screen = GfxRect {
        x: 0.0,
        y: 0.0,
        w: width as f32,
        h: height as f32,
    };
    if let Some(path) = squircle(screen, layout.screen_radius) {
        let mut paint = Paint::default();
        paint.anti_alias = false;
        paint.set_color_rgba8(255, 255, 255, 255);
        pixmap.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    let (cx, cy, radius) = layout.camera;
    let mut builder = PathBuilder::new();
    builder.push_circle(cx - layout.screen.x, cy - layout.screen.y, radius + 0.4);
    if let Some(path) = builder.finish() {
        let mut paint = Paint::default();
        paint.blend_mode = BlendMode::Clear;
        paint.set_color_rgba8(0, 0, 0, 255);
        pixmap.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    alpha_rects(pixmap.data(), width, height, 128)
}

fn squircle(rect: GfxRect, radius: f32) -> Option<Path> {
    if rect.w < 2.0 || rect.h < 2.0 {
        return None;
    }
    let radius = radius.min(rect.w * 0.5).min(rect.h * 0.5).max(0.0);
    if radius < 1.0 {
        return Some(PathBuilder::from_rect(tiny_skia::Rect::from_xywh(
            rect.x, rect.y, rect.w, rect.h,
        )?));
    }
    // Superellipse corners (n = 5) stay fuller than a plain circular radius,
    // which is the Pixel glass silhouette rather than a stadium shape.
    let n = 5.0;
    let steps = 14;
    let corners = [
        (rect.x + rect.w - radius, rect.y + radius, -90.0_f32, 0.0),
        (
            rect.x + rect.w - radius,
            rect.y + rect.h - radius,
            0.0,
            90.0,
        ),
        (rect.x + radius, rect.y + rect.h - radius, 90.0, 180.0),
        (rect.x + radius, rect.y + radius, 180.0, 270.0),
    ];
    let mut builder = PathBuilder::new();
    let mut started = false;
    for (cx, cy, start, end) in corners {
        for step in 0..=steps {
            let angle = start + (end - start) * (step as f32 / steps as f32);
            let (px, py) = squircle_point(cx, cy, radius, angle, n);
            if !started {
                builder.move_to(px, py);
                started = true;
            } else {
                builder.line_to(px, py);
            }
        }
    }
    builder.close();
    builder.finish()
}

fn squircle_point(cx: f32, cy: f32, radius: f32, degrees: f32, n: f32) -> (f32, f32) {
    let theta = degrees.to_radians();
    let (cos, sin) = (theta.cos(), theta.sin());
    let exponent = 2.0 / n;
    (
        cx + radius * signed_pow(cos, exponent),
        cy + radius * signed_pow(sin, exponent),
    )
}

fn signed_pow(value: f32, exponent: f32) -> f32 {
    if value == 0.0 {
        0.0
    } else {
        value.signum() * value.abs().powf(exponent)
    }
}

fn pill_path(rect: GfxRect) -> Option<Path> {
    let radius = (rect.w.min(rect.h) * 0.5).max(0.0);
    squircle(rect, radius)
}

fn alpha_rects(data: &[u8], width: u32, height: u32, threshold: u8) -> Vec<Rectangle> {
    let mut rects: Vec<Rectangle> = Vec::new();
    for y in 0..height {
        let mut spans = Vec::new();
        let mut x = 0u32;
        while x < width {
            while x < width && pixel_alpha(data, width, x, y) < threshold {
                x += 1;
            }
            let start = x;
            while x < width && pixel_alpha(data, width, x, y) >= threshold {
                x += 1;
            }
            if start < x {
                spans.push((start as i16, (x - start) as u16));
            }
        }
        let y = y as i16;
        let merge = rects.len() >= spans.len()
            && !spans.is_empty()
            && rects[rects.len() - spans.len()..]
                .iter()
                .zip(spans.iter())
                .all(|(rect, span)| {
                    rect.x == span.0 && rect.width == span.1 && rect.y + rect.height as i16 == y
                });
        if merge {
            let start = rects.len() - spans.len();
            for rect in &mut rects[start..] {
                rect.height = rect.height.saturating_add(1);
            }
        } else {
            for (x, w) in spans {
                rects.push(Rectangle {
                    x,
                    y,
                    width: w,
                    height: 1,
                });
            }
        }
    }
    rects
}

fn pixel_alpha(data: &[u8], width: u32, x: u32, y: u32) -> u8 {
    data[((y * width + x) * 4 + 3) as usize]
}

fn rgba_to_bgra(data: &mut [u8]) {
    for pixel in data.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
}
