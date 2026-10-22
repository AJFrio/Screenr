use crate::config::{AudioMode, MirrorPreset, StreamSettings};
use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SessionKind {
    ScreenMirror,
    VirtualDisplay,
    AppStream(String),
    CameraStream,
}

impl SessionKind {
    pub fn label(&self) -> String {
        match self {
            Self::ScreenMirror => "Main Display Mirror".to_string(),
            Self::VirtualDisplay => "Virtual Desktop Display".to_string(),
            Self::AppStream(app) => format!("App Stream ({})", app),
            Self::CameraStream => "Continuity Camera".to_string(),
        }
    }
}

#[allow(dead_code)]
pub struct ActiveScrcpySession {
    pub id: u64,
    pub serial: String,
    pub model: String,
    pub kind: SessionKind,
    pub child: Child,
}

#[allow(dead_code)]
pub struct ScrcpyManager {
    pub scrcpy_bin: PathBuf,
    pub scrcpy_server: PathBuf,
    pub adb_bin: PathBuf,
    pub sessions: Vec<ActiveScrcpySession>,
    next_id: u64,
}

#[allow(dead_code)]
impl ScrcpyManager {
    pub fn new(adb_bin: PathBuf) -> Self {
        let (scrcpy_bin, scrcpy_server) = resolve_scrcpy_paths();
        Self {
            scrcpy_bin,
            scrcpy_server,
            adb_bin,
            sessions: Vec::new(),
            next_id: 1,
        }
    }

    pub fn poll_running(&mut self) -> Vec<String> {
        let mut messages = Vec::new();
        let mut ended_serials = Vec::new();
        self.sessions.retain_mut(|sess| match sess.child.try_wait() {
            Ok(Some(status)) => {
                ended_serials.push(sess.serial.clone());
                if !status.success() && status.code() != Some(0) {
                    messages.push(format!(
                        "{} on {} exited (code {:?}).",
                        sess.kind.label(),
                        sess.model,
                        status.code()
                    ));
                } else {
                    messages.push(format!("{} session ended · Phone locked", sess.kind.label()));
                }
                false
            }
            Ok(None) => true,
            Err(_) => false,
        });
        for serial in ended_serials {
            lock_device_screen(&self.adb_bin, &serial);
        }
        messages
    }

    pub fn is_mirroring_device(&self, serial: &str) -> bool {
        self.sessions.iter().any(|s| s.serial == serial)
    }

    pub fn stop_for_device(&mut self, serial: &str) {
        let mut had_session = false;
        for sess in &mut self.sessions {
            if sess.serial == serial {
                had_session = true;
                let _ = sess.child.kill();
                let _ = sess.child.wait();
            }
        }
        self.sessions.retain(|s| s.serial != serial);
        if had_session {
            lock_device_screen(&self.adb_bin, serial);
        }
    }

    pub fn stop_session(&mut self, id: u64) {
        if let Some(pos) = self.sessions.iter().position(|s| s.id == id) {
            let mut s = self.sessions.remove(pos);
            let _ = s.child.kill();
            let _ = s.child.wait();
            lock_device_screen(&self.adb_bin, &s.serial);
        }
    }

    pub fn start_screen_mirror(
        &mut self,
        serial: &str,
        model: &str,
        settings: &StreamSettings,
    ) -> Result<String, String> {
        let mut args: Vec<String> = Vec::new();
        args.push(format!("--serial={}", serial));

        let title = if settings.preset == MirrorPreset::VirtualDesktopDisplay {
            format!("{} — Virtual Display (Screenr)", model)
        } else {
            format!("{} — Screenr", model)
        };
        args.push(format!("--window-title={}", title));

        args.push(format!("--video-codec={}", settings.video_codec.as_arg()));
        args.push(format!("--video-bit-rate={}M", settings.bitrate_mbps));
        args.push(format!("--max-fps={}", settings.max_fps));

        if settings.max_size > 0 && settings.preset != MirrorPreset::VirtualDesktopDisplay {
            args.push(format!("--max-size={}", settings.max_size));
        }

        if settings.preset == MirrorPreset::VirtualDesktopDisplay {
            args.push(format!("--new-display={}", settings.virtual_display_res));
            args.push("--window-width=1600".to_string());
            args.push("--window-height=900".to_string());
        }

        if settings.turn_screen_off {
            args.push("--turn-screen-off".to_string());
        }
        args.push("--power-off-on-close".to_string());

        if settings.stay_awake {
            args.push("--stay-awake".to_string());
        }
        if settings.always_on_top {
            args.push("--always-on-top".to_string());
        }
        if settings.window_borderless {
            args.push("--window-borderless".to_string());
        }
        if settings.show_touches {
            args.push("--show-touches".to_string());
        }
        if settings.uhid_keyboard {
            args.push("--keyboard=uhid".to_string());
        }

        match settings.audio_mode {
            AudioMode::OutputToPc => {
                args.push("--audio-source=output".to_string());
            }
            AudioMode::DuplicateOnBoth => {
                args.push("--audio-source=playback".to_string());
                args.push("--audio-dup".to_string());
            }
            AudioMode::Muted => {
                args.push("--no-audio".to_string());
            }
        }

        if settings.record_session {
            let mut dir = dirs::video_dir()
                .or_else(dirs::home_dir)
                .unwrap_or_else(|| PathBuf::from("."));
            dir.push("Screenr");
            let _ = fs::create_dir_all(&dir);
            let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
            let rec_path = dir.join(format!("Pixel_Recording_{}.mp4", ts));
            args.push(format!("--record={}", rec_path.display()));
        }

        let kind = if settings.preset == MirrorPreset::VirtualDesktopDisplay {
            SessionKind::VirtualDisplay
        } else {
            SessionKind::ScreenMirror
        };

        self.spawn_scrcpy(serial, model, kind, &args)
    }

    pub fn start_app_stream(
        &mut self,
        serial: &str,
        model: &str,
        package: &str,
        display_name: &str,
        use_virtual_display: bool,
        settings: &StreamSettings,
    ) -> Result<String, String> {
        let mut args: Vec<String> = Vec::new();
        args.push(format!("--serial={}", serial));
        args.push(format!("--window-title={} — {}", display_name, model));
        args.push(format!("--video-codec={}", settings.video_codec.as_arg()));
        args.push(format!("--video-bit-rate={}M", settings.bitrate_mbps));
        args.push(format!("--max-fps={}", settings.max_fps));

        if use_virtual_display {
            args.push(format!("--new-display={}", settings.virtual_display_res));
            args.push("--flex-display".to_string());
        }
        args.push(format!("--start-app={}", package));

        if settings.uhid_keyboard {
            args.push("--keyboard=uhid".to_string());
        }
        if settings.always_on_top {
            args.push("--always-on-top".to_string());
        }

        match settings.audio_mode {
            AudioMode::OutputToPc => args.push("--audio-source=output".to_string()),
            AudioMode::DuplicateOnBoth => {
                args.push("--audio-source=playback".to_string());
                args.push("--audio-dup".to_string());
            }
            AudioMode::Muted => args.push("--no-audio".to_string()),
        }

        self.spawn_scrcpy(
            serial,
            model,
            SessionKind::AppStream(display_name.to_string()),
            &args,
        )
    }

    pub fn start_camera_stream(
        &mut self,
        serial: &str,
        model: &str,
        settings: &StreamSettings,
    ) -> Result<String, String> {
        let mut args: Vec<String> = Vec::new();
        args.push(format!("--serial={}", serial));
        args.push(format!(
            "--window-title={} — Continuity Camera ({})",
            model,
            settings.camera_facing.as_arg()
        ));
        args.push("--video-source=camera".to_string());
        args.push(format!(
            "--camera-facing={}",
            settings.camera_facing.as_arg()
        ));
        args.push(format!("--camera-size={}", settings.camera_size));
        args.push(format!("--camera-fps={}", settings.camera_fps));
        args.push(format!("--video-codec={}", settings.video_codec.as_arg()));
        args.push(format!("--video-bit-rate={}M", settings.bitrate_mbps));

        if settings.always_on_top {
            args.push("--always-on-top".to_string());
        }
        if settings. audio_mode == AudioMode::Muted {
            args.push("--no-audio".to_string());
        } else {
            args.push("--audio-source=mic-camcorder".to_string());
        }

        self.spawn_scrcpy(serial, model, SessionKind::CameraStream, &args)
    }

    fn spawn_scrcpy(
        &mut self,
        serial: &str,
        model: &str,
        kind: SessionKind,
        args: &[String],
    ) -> Result<String, String> {
        let mut cmd = Command::new(&self.scrcpy_bin);
        cmd.args(args);
        cmd.env("ADB", &self.adb_bin);
        if self.scrcpy_server.exists() {
            cmd.env("SCRCPY_SERVER_PATH", &self.scrcpy_server);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let child = cmd
            .spawn()
            .map_err(|e| format!("Failed to launch scrcpy ({}): {}", self.scrcpy_bin.display(), e))?;

        let id = self.next_id;
        self.next_id += 1;
        let label = kind.label();

        self.sessions.push(ActiveScrcpySession {
            id,
            serial: serial.to_string(),
            model: model.to_string(),
            kind,
            child,
        });

        Ok(format!("Started {} on {}", label, model))
    }
}

impl Drop for ScrcpyManager {
    fn drop(&mut self) {
        for sess in &mut self.sessions {
            let _ = sess.child.kill();
            let _ = Command::new(&self.adb_bin)
                .args(["-s", &sess.serial, "shell", "input", "keyevent", "KEYCODE_SLEEP"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
    }
}

fn lock_device_screen(adb_bin: &PathBuf, serial: &str) {
    let adb = adb_bin.clone();
    let ser = serial.to_string();
    std::thread::spawn(move || {
        let _ = Command::new(&adb)
            .args(["-s", &ser, "shell", "input", "keyevent", "KEYCODE_SLEEP"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        // Send a second KEYCODE_SLEEP after 300ms in case scrcpy-server socket teardown briefly woke the display
        std::thread::sleep(std::time::Duration::from_millis(300));
        let _ = Command::new(&adb)
            .args(["-s", &ser, "shell", "input", "keyevent", "KEYCODE_SLEEP"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
}

fn resolve_scrcpy_paths() -> (PathBuf, PathBuf) {
    let mut dir_candidates = Vec::new();
    if let Some(root) = crate::adb::project_root_dir() {
        dir_candidates.push(root.join("vendor/scrcpy"));
    }
    dir_candidates.push(PathBuf::from("./vendor/scrcpy"));

    for dir in &dir_candidates {
        let bin = dir.join("scrcpy");
        let server = dir.join("scrcpy-server");
        if bin.exists() {
            return (bin, server);
        }
    }
    (PathBuf::from("scrcpy"), PathBuf::from("scrcpy-server"))
}
