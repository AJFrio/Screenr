use rand::Rng;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionKind {
    Usb,
    Wifi,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AdbDevice {
    pub serial: String,
    pub state: String, // "device", "unauthorized", "offline"
    pub model: String,
    pub android_version: String,
    pub battery_level: Option<u8>,
    pub battery_charging: bool,
    pub wifi_ip: Option<String>,
    pub resolution: Option<String>,
    pub kind: ConnectionKind,
}

#[derive(Debug, Clone)]
pub struct MdnsService {
    pub instance_name: String,
    pub service_type: String, // "_adb-tls-pairing._tcp" or "_adb-tls-connect._tcp"
    pub address: String,      // "192.168.1.50:38123"
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct InstalledApp {
    pub package: String,
    pub display_name: String,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum AdbAction {
    RefreshNow,
    SwitchUsbToWireless {
        serial: String,
    },
    PairWithCode {
        addr: String,
        code: String,
    },
    ConnectDirect {
        addr: String,
    },
    DisconnectDevice {
        serial: String,
    },
    SendKeyEvent {
        serial: String,
        keycode: &'static str,
    },
    ExpandNotifications {
        serial: String,
    },
    ExpandQuickSettings {
        serial: String,
    },
    CollapseStatusBar {
        serial: String,
    },
    SendText {
        serial: String,
        text: String,
    },
    TakeScreenshot {
        serial: String,
    },
    PushFileOrApk {
        serial: String,
        path: PathBuf,
    },
    LoadInstalledApps {
        serial: String,
    },
    RegenerateQrCredentials {
        service_name: String,
        password: String,
    },
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum AdbUpdate {
    StateSnapshot {
        devices: Vec<AdbDevice>,
        mdns_services: Vec<MdnsService>,
    },
    LogMessage {
        level: LogLevel,
        message: String,
    },
    AppsLoaded {
        serial: String,
        apps: Vec<InstalledApp>,
    },
    QrAutoPaired {
        address: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Success,
    Warning,
    Error,
}

pub fn project_root_dir() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        // If running from <repo>/target/release/screenr or <repo>/target/debug/screenr
        if let Some(root) = exe
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
        {
            if root.join("vendor").exists() || root.join("Cargo.toml").exists() {
                return Some(root.to_path_buf());
            }
        }
        if let Some(dir) = exe.parent() {
            if dir.join("vendor").exists() {
                return Some(dir.to_path_buf());
            }
        }
    }
    std::env::current_dir().ok()
}

pub fn resolve_adb_binary() -> PathBuf {
    let mut candidates = Vec::new();
    if let Some(root) = project_root_dir() {
        candidates.push(root.join("vendor/scrcpy/adb"));
    }
    candidates.push(PathBuf::from("./vendor/scrcpy/adb"));
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join("Android/Sdk/platform-tools/adb"));
    }

    for c in &candidates {
        if c.exists() {
            return c.clone();
        }
    }
    PathBuf::from("adb")
}

pub fn generate_qr_credentials() -> (String, String, String) {
    let mut rng = rand::thread_rng();
    let suffix: u32 = rng.gen_range(1000..9999);
    let pass: u32 = rng.gen_range(100000..999999);
    let service_name = format!("studio-screenr-{}", suffix);
    let password = format!("{}", pass);
    let qr_payload = format!("WIFI:T:ADB;S:{};P:{};;", service_name, password);
    (service_name, password, qr_payload)
}

pub struct AdbWorkerHandle {
    pub action_tx: Sender<AdbAction>,
    pub update_rx: Receiver<AdbUpdate>,
    pub adb_path: PathBuf,
}

impl AdbWorkerHandle {
    pub fn spawn(
        ctx: eframe::egui::Context,
        initial_qr_service: String,
        initial_qr_pass: String,
        initial_last_wifi_addr: Option<String>,
    ) -> Self {
        let (action_tx, action_rx) = mpsc::channel::<AdbAction>();
        let (update_tx, update_rx) = mpsc::channel::<AdbUpdate>();
        let adb_path = resolve_adb_binary();
        let worker_adb = adb_path.clone();

        thread::spawn(move || {
            run_adb_worker(
                ctx,
                worker_adb,
                action_rx,
                update_tx,
                initial_qr_service,
                initial_qr_pass,
                initial_last_wifi_addr,
            );
        });

        Self {
            action_tx,
            update_rx,
            adb_path,
        }
    }
}

fn run_adb_cmd(adb: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new(adb)
        .args(args)
        .output()
        .map_err(|e| format!("Failed to execute adb: {}", e))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if out.status.success() {
        Ok(stdout)
    } else {
        Err(if stderr.trim().is_empty() {
            stdout
        } else {
            stderr
        })
    }
}

fn extract_ipv4(addr_or_serial: &str) -> Option<Ipv4Addr> {
    let host = addr_or_serial.split(':').next()?.trim();
    host.parse::<Ipv4Addr>().ok()
}

fn run_adb_worker(
    ctx: eframe::egui::Context,
    adb: PathBuf,
    action_rx: Receiver<AdbAction>,
    update_tx: Sender<AdbUpdate>,
    mut qr_service_name: String,
    mut qr_password: String,
    initial_last_wifi_addr: Option<String>,
) {
    // Ensure ADB server is started
    let _ = run_adb_cmd(&adb, &["start-server"]);

    let mut known_wifi_ip: Option<Ipv4Addr> = initial_last_wifi_addr
        .as_deref()
        .and_then(extract_ipv4);

    let mut telemetry_cache: HashMap<String, (Instant, AdbDevice)> = HashMap::new();
    let mut attempted_qr_pairings: HashSet<String> = HashSet::new();
    let mut auto_connected_endpoints: HashMap<String, Instant> = HashMap::new();
    let mut last_poll = Instant::now() - Duration::from_secs(10);
    let mut last_udp_mdns_probe = Instant::now() - Duration::from_secs(10);

    // If we have a saved Wi-Fi endpoint, try a quick direct connect right away in case the port is unchanged
    if let Some(ref addr) = initial_last_wifi_addr {
        let trimmed = addr.trim();
        if !trimmed.is_empty() {
            auto_connected_endpoints.insert(trimmed.to_string(), Instant::now());
            let _ = run_adb_cmd(&adb, &["connect", trimmed]);
        }
    }

    loop {
        // Process any queued user actions immediately
        while let Ok(action) = action_rx.try_recv() {
            match action {
                AdbAction::RefreshNow => {
                    last_poll = Instant::now() - Duration::from_secs(10);
                }
                AdbAction::RegenerateQrCredentials {
                    service_name,
                    password,
                } => {
                    qr_service_name = service_name;
                    qr_password = password;
                    attempted_qr_pairings.clear();
                }
                AdbAction::SwitchUsbToWireless { serial } => {
                    let _ = update_tx.send(AdbUpdate::LogMessage {
                        level: LogLevel::Info,
                        message: format!("Enabling Wireless TCP/IP mode on {}...", serial),
                    });
                    // First detect the phone's Wi-Fi IP address
                    let ip_opt = query_device_wifi_ip(&adb, &serial);
                    match run_adb_cmd(&adb, &["-s", &serial, "tcpip", "5555"]) {
                        Ok(_) => {
                            thread::sleep(Duration::from_millis(1200));
                            if let Some(ip) = ip_opt {
                                let target = format!("{}:5555", ip);
                                match run_adb_cmd(&adb, &["connect", &target]) {
                                    Ok(out) => {
                                        let _ = update_tx.send(AdbUpdate::LogMessage {
                                            level: LogLevel::Success,
                                            message: format!(
                                                "Connected wirelessly to {}! ({}) — You can now unplug the USB cable.",
                                                target,
                                                out.trim()
                                            ),
                                        });
                                    }
                                    Err(e) => {
                                        let _ = update_tx.send(AdbUpdate::LogMessage {
                                            level: LogLevel::Warning,
                                            message: format!(
                                                "TCP/IP port 5555 enabled, but auto-connect to {} failed: {}",
                                                target,
                                                e.trim()
                                            ),
                                        });
                                    }
                                }
                            } else {
                                let _ = update_tx.send(AdbUpdate::LogMessage {
                                    level: LogLevel::Warning,
                                    message: "TCP/IP port 5555 enabled on phone, but could not detect phone Wi-Fi IP. Check that Pixel 10 is on the same Wi-Fi network.".to_string(),
                                });
                            }
                        }
                        Err(e) => {
                            let _ = update_tx.send(AdbUpdate::LogMessage {
                                level: LogLevel::Error,
                                message: format!("Failed to enable TCP/IP mode: {}", e.trim()),
                            });
                        }
                    }
                    last_poll = Instant::now() - Duration::from_secs(10);
                }
                AdbAction::PairWithCode { addr, code } => {
                    let _ = update_tx.send(AdbUpdate::LogMessage {
                        level: LogLevel::Info,
                        message: format!("Pairing with {}...", addr),
                    });
                    match run_adb_cmd(&adb, &["pair", addr.trim(), code.trim()]) {
                        Ok(out) => {
                            let _ = update_tx.send(AdbUpdate::LogMessage {
                                level: LogLevel::Success,
                                message: format!("Pairing succeeded: {}", out.trim()),
                            });
                            // Clear auto-connect cache so any newly advertised connect service is picked up
                            auto_connected_endpoints.clear();
                        }
                        Err(e) => {
                            let _ = update_tx.send(AdbUpdate::LogMessage {
                                level: LogLevel::Error,
                                message: format!("Pairing failed: {}", e.trim()),
                            });
                        }
                    }
                    last_poll = Instant::now() - Duration::from_secs(10);
                }
                AdbAction::ConnectDirect { addr } => {
                    let _ = update_tx.send(AdbUpdate::LogMessage {
                        level: LogLevel::Info,
                        message: format!("Connecting to {}...", addr),
                    });
                    match run_adb_cmd(&adb, &["connect", addr.trim()]) {
                        Ok(out) => {
                            let ok = out.contains("connected to");
                            let _ = update_tx.send(AdbUpdate::LogMessage {
                                level: if ok {
                                    LogLevel::Success
                                } else {
                                    LogLevel::Warning
                                },
                                message: out.trim().to_string(),
                            });
                        }
                        Err(e) => {
                            let _ = update_tx.send(AdbUpdate::LogMessage {
                                level: LogLevel::Error,
                                message: format!("Connection error: {}", e.trim()),
                            });
                        }
                    }
                    last_poll = Instant::now() - Duration::from_secs(10);
                }
                AdbAction::DisconnectDevice { serial } => {
                    let _ = run_adb_cmd(&adb, &["disconnect", &serial]);
                    let _ = update_tx.send(AdbUpdate::LogMessage {
                        level: LogLevel::Info,
                        message: format!("Disconnected {}", serial),
                    });
                    last_poll = Instant::now() - Duration::from_secs(10);
                }
                AdbAction::SendKeyEvent { serial, keycode } => {
                    let _ = run_adb_cmd(
                        &adb,
                        &["-s", &serial, "shell", "input", "keyevent", keycode],
                    );
                }
                AdbAction::ExpandNotifications { serial } => {
                    let _ = run_adb_cmd(
                        &adb,
                        &[
                            "-s",
                            &serial,
                            "shell",
                            "cmd",
                            "statusbar",
                            "expand-notifications",
                        ],
                    );
                }
                AdbAction::ExpandQuickSettings { serial } => {
                    let _ = run_adb_cmd(
                        &adb,
                        &[
                            "-s",
                            &serial,
                            "shell",
                            "cmd",
                            "statusbar",
                            "expand-settings",
                        ],
                    );
                }
                AdbAction::CollapseStatusBar { serial } => {
                    let _ = run_adb_cmd(
                        &adb,
                        &["-s", &serial, "shell", "cmd", "statusbar", "collapse"],
                    );
                }
                AdbAction::SendText { serial, text } => {
                    // Escape spaces for Android `input text`
                    let escaped = text
                        .replace(' ', "%s")
                        .replace('&', "\\&")
                        .replace(';', "\\;")
                        .replace('(', "\\(")
                        .replace(')', "\\)")
                        .replace('"', "\\\"")
                        .replace('\'', "\\'");
                    if !escaped.is_empty() {
                        let _ =
                            run_adb_cmd(&adb, &["-s", &serial, "shell", "input", "text", &escaped]);
                        let _ = update_tx.send(AdbUpdate::LogMessage {
                            level: LogLevel::Info,
                            message: "Sent text to device.".to_string(),
                        });
                    }
                }
                AdbAction::TakeScreenshot { serial } => {
                    let mut dir = dirs::picture_dir().unwrap_or_else(|| PathBuf::from("."));
                    dir.push("Screenr");
                    let _ = fs::create_dir_all(&dir);
                    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
                    let filename = format!("Pixel_Screenshot_{}.png", timestamp);
                    let filepath = dir.join(&filename);

                    match Command::new(&adb)
                        .args(["-s", &serial, "exec-out", "screencap", "-p"])
                        .output()
                    {
                        Ok(out) if out.status.success() && !out.stdout.is_empty() => {
                            if let Err(e) = fs::write(&filepath, &out.stdout) {
                                let _ = update_tx.send(AdbUpdate::LogMessage {
                                    level: LogLevel::Error,
                                    message: format!("Failed to save screenshot: {}", e),
                                });
                            } else {
                                let _ = update_tx.send(AdbUpdate::LogMessage {
                                    level: LogLevel::Success,
                                    message: format!("Saved screenshot to {}", filepath.display()),
                                });
                            }
                        }
                        Ok(_) | Err(_) => {
                            let _ = update_tx.send(AdbUpdate::LogMessage {
                                level: LogLevel::Error,
                                message: "Failed to capture screenshot from device.".to_string(),
                            });
                        }
                    }
                }
                AdbAction::PushFileOrApk { serial, path } => {
                    let path_str = path.to_string_lossy().to_string();
                    let is_apk = path
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.eq_ignore_ascii_case("apk"))
                        .unwrap_or(false);

                    if is_apk {
                        let _ = update_tx.send(AdbUpdate::LogMessage {
                            level: LogLevel::Info,
                            message: format!("Installing APK {} on device...", path.display()),
                        });
                        match run_adb_cmd(&adb, &["-s", &serial, "install", "-r", &path_str]) {
                            Ok(out) => {
                                let _ = update_tx.send(AdbUpdate::LogMessage {
                                    level: LogLevel::Success,
                                    message: format!("Installed APK: {}", out.trim()),
                                });
                            }
                            Err(e) => {
                                let _ = update_tx.send(AdbUpdate::LogMessage {
                                    level: LogLevel::Error,
                                    message: format!("APK install failed: {}", e.trim()),
                                });
                            }
                        }
                    } else {
                        let _ = update_tx.send(AdbUpdate::LogMessage {
                            level: LogLevel::Info,
                            message: format!(
                                "Pushing {} to /sdcard/Download/...",
                                path.file_name().unwrap_or_default().to_string_lossy()
                            ),
                        });
                        match run_adb_cmd(
                            &adb,
                            &["-s", &serial, "push", &path_str, "/sdcard/Download/"],
                        ) {
                            Ok(out) => {
                                let _ = update_tx.send(AdbUpdate::LogMessage {
                                    level: LogLevel::Success,
                                    message: format!(
                                        "File transferred to Downloads: {}",
                                        out.trim()
                                    ),
                                });
                            }
                            Err(e) => {
                                let _ = update_tx.send(AdbUpdate::LogMessage {
                                    level: LogLevel::Error,
                                    message: format!("File push failed: {}", e.trim()),
                                });
                            }
                        }
                    }
                }
                AdbAction::LoadInstalledApps { serial } => {
                    if let Ok(out) = run_adb_cmd(
                        &adb,
                        &["-s", &serial, "shell", "pm", "list", "packages", "-3"],
                    ) {
                        let mut apps: Vec<InstalledApp> = out
                            .lines()
                            .filter_map(|line| {
                                let pkg = line.trim().strip_prefix("package:")?.trim().to_string();
                                if pkg.is_empty() {
                                    return None;
                                }
                                let display_name = humanize_package_name(&pkg);
                                Some(InstalledApp {
                                    package: pkg,
                                    display_name,
                                })
                            })
                            .collect();
                        apps.sort_by(|a, b| {
                            a.display_name
                                .to_lowercase()
                                .cmp(&b.display_name.to_lowercase())
                        });
                        let _ = update_tx.send(AdbUpdate::AppsLoaded { serial, apps });
                    }
                }
            }
        }

        // Poll ADB devices and mDNS services every 1.5s
        if last_poll.elapsed() >= Duration::from_millis(1500) {
            last_poll = Instant::now();

            let mut devices = poll_devices(&adb, &mut telemetry_cache);

            // Clean up any stale offline Wi-Fi transports so they can cleanly reconnect
            let mut cleaned_offline = false;
            for dev in &devices {
                if dev.kind == ConnectionKind::Wifi && dev.state == "offline" {
                    let _ = run_adb_cmd(&adb, &["disconnect", &dev.serial]);
                    auto_connected_endpoints.remove(&dev.serial);
                    cleaned_offline = true;
                }
            }
            if cleaned_offline {
                devices = poll_devices(&adb, &mut telemetry_cache);
            }

            for dev in &devices {
                if dev.state == "device" {
                    if let Some(ip) = extract_ipv4(&dev.serial)
                        .or_else(|| dev.wifi_ip.as_deref().and_then(extract_ipv4))
                    {
                        known_wifi_ip = Some(ip);
                    }
                }
            }

            let has_online_wifi = devices
                .iter()
                .any(|d| d.state == "device" && d.kind == ConnectionKind::Wifi);

            let mut mdns_services = poll_mdns_services(&adb);

            // If no Wi-Fi device is connected (or adb mdns services hasn't seen the phone yet),
            // actively nudge the phone's mDNS responder via unicast + multicast UDP 5353 and parse replies.
            let has_connect_svc = mdns_services
                .iter()
                .any(|s| s.service_type.contains("_adb-tls-connect._tcp"));
            if (!has_online_wifi || !has_connect_svc)
                && last_udp_mdns_probe.elapsed() >= Duration::from_millis(2500)
            {
                last_udp_mdns_probe = Instant::now();
                let probed = probe_mdns_services_udp(known_wifi_ip);
                for svc in probed {
                    if !mdns_services
                        .iter()
                        .any(|s| s.address == svc.address && s.service_type == svc.service_type)
                    {
                        mdns_services.push(svc);
                    }
                }
                if !mdns_services
                    .iter()
                    .any(|s| s.service_type.contains("_adb-tls-connect._tcp"))
                {
                    for svc in poll_mdns_services(&adb) {
                        if !mdns_services
                            .iter()
                            .any(|s| s.address == svc.address && s.service_type == svc.service_type)
                        {
                            mdns_services.push(svc);
                        }
                    }
                }
            }

            let mut connected_new = false;

            // Check if any mDNS service matches our QR code pairing session or is an active connect service
            for svc in &mdns_services {
                if let Some(ip) = extract_ipv4(&svc.address) {
                    known_wifi_ip = Some(ip);
                }

                if svc.service_type.contains("_adb-tls-pairing._tcp") {
                    // If the phone scanned our QR code, its instance_name matches `qr_service_name`
                    // (or starts with "studio-screenr-")
                    if (svc.instance_name == qr_service_name
                        || svc.instance_name.starts_with("studio-screenr-"))
                        && !attempted_qr_pairings.contains(&svc.address)
                    {
                        attempted_qr_pairings.insert(svc.address.clone());
                        let _ = update_tx.send(AdbUpdate::LogMessage {
                            level: LogLevel::Info,
                            message: format!(
                                "Detected Pixel QR Code scan at {}! Auto-pairing...",
                                svc.address
                            ),
                        });
                        ctx.request_repaint();
                        if let Ok(out) = run_adb_cmd(&adb, &["pair", &svc.address, &qr_password]) {
                            let _ = update_tx.send(AdbUpdate::LogMessage {
                                level: LogLevel::Success,
                                message: format!("QR Auto-Pairing complete: {}", out.trim()),
                            });
                            let _ = update_tx.send(AdbUpdate::QrAutoPaired {
                                address: svc.address.clone(),
                            });
                            ctx.request_repaint();
                            auto_connected_endpoints.clear();
                        }
                    }
                } else if svc.service_type.contains("_adb-tls-connect._tcp") {
                    // Automatically connect to discovered Wireless Debugging endpoints whenever not already online
                    let already_online = devices.iter().any(|d| {
                        d.state == "device"
                            && (d.serial == svc.address
                                || (!svc.instance_name.is_empty()
                                    && d.serial.starts_with(&svc.instance_name)))
                    });

                    if !already_online {
                        let should_try = match auto_connected_endpoints.get(&svc.address) {
                            Some(last_try) => last_try.elapsed() >= Duration::from_secs(4),
                            None => true,
                        };
                        if should_try {
                            auto_connected_endpoints.insert(svc.address.clone(), Instant::now());
                            if let Ok(out) = run_adb_cmd(&adb, &["connect", &svc.address]) {
                                if out.contains("connected to") {
                                    connected_new = true;
                                }
                            }
                        }
                    }
                }
            }

            if connected_new {
                devices = poll_devices(&adb, &mut telemetry_cache);
            }

            let _ = update_tx.send(AdbUpdate::StateSnapshot {
                devices,
                mdns_services,
            });
            ctx.request_repaint();
        }

        thread::sleep(Duration::from_millis(80));
    }
}

fn poll_mdns_services(adb: &Path) -> Vec<MdnsService> {
    let mut services = Vec::new();
    if let Ok(out) = run_adb_cmd(adb, &["mdns", "services"]) {
        for line in out.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with("List of discovered") {
                continue;
            }
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if parts.len() >= 3 {
                // Prefer IPv4 address if adb outputs an address token
                let addr = parts[2].to_string();
                services.push(MdnsService {
                    instance_name: parts[0].to_string(),
                    service_type: parts[1].to_string(),
                    address: addr,
                });
            }
        }
    }
    services
}

fn detect_local_lan_ipv4(target_hint: Option<Ipv4Addr>) -> Option<Ipv4Addr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    let target = target_hint.unwrap_or(Ipv4Addr::new(8, 8, 8, 8));
    sock.connect(SocketAddrV4::new(target, 5353)).ok()?;
    match sock.local_addr().ok()? {
        SocketAddr::V4(v4) if !v4.ip().is_loopback() && !v4.ip().is_unspecified() => Some(*v4.ip()),
        _ => None,
    }
}

fn build_mdns_ptr_query(qname: &str, unicast_response: bool) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(64);
    // Transaction ID = 0, Flags = 0, QDCOUNT = 1, ANCOUNT = 0, NSCOUNT = 0, ARCOUNT = 0
    pkt.extend_from_slice(&[0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
    for label in qname.split('.') {
        let bytes = label.as_bytes();
        if !bytes.is_empty() && bytes.len() < 64 {
            pkt.push(bytes.len() as u8);
            pkt.extend_from_slice(bytes);
        }
    }
    pkt.push(0); // Root label
    pkt.extend_from_slice(&12u16.to_be_bytes()); // QTYPE = PTR (12)
    let qclass: u16 = if unicast_response { 0x8001 } else { 0x0001 };
    pkt.extend_from_slice(&qclass.to_be_bytes());
    pkt
}

fn read_dns_name(pkt: &[u8], mut pos: usize) -> Option<(String, usize)> {
    let mut labels = Vec::new();
    let mut jumped = false;
    let mut end_pos = pos;
    let mut hops = 0;

    loop {
        if pos >= pkt.len() || hops > 32 {
            return None;
        }
        let len = pkt[pos];
        if len == 0 {
            if !jumped {
                end_pos = pos + 1;
            }
            break;
        } else if (len & 0xC0) == 0xC0 {
            if pos + 1 >= pkt.len() {
                return None;
            }
            let offset = (((len & 0x3F) as usize) << 8) | (pkt[pos + 1] as usize);
            if !jumped {
                end_pos = pos + 2;
                jumped = true;
            }
            pos = offset;
            hops += 1;
        } else {
            let label_len = (len & 0x3F) as usize;
            pos += 1;
            if pos + label_len > pkt.len() {
                return None;
            }
            let label = String::from_utf8_lossy(&pkt[pos..pos + label_len]).to_string();
            labels.push(label);
            pos += label_len;
            if !jumped {
                end_pos = pos;
            }
        }
    }

    Some((labels.join("."), end_pos))
}

fn parse_mdns_packet(pkt: &[u8], src_ip: Ipv4Addr) -> Vec<MdnsService> {
    let mut discovered = Vec::new();
    if pkt.len() < 12 {
        return discovered;
    }

    let qdcount = u16::from_be_bytes([pkt[4], pkt[5]]) as usize;
    let ancount = u16::from_be_bytes([pkt[6], pkt[7]]) as usize;
    let nscount = u16::from_be_bytes([pkt[8], pkt[9]]) as usize;
    let arcount = u16::from_be_bytes([pkt[10], pkt[11]]) as usize;
    let total_rr = ancount.saturating_add(nscount).saturating_add(arcount);
    if total_rr == 0 {
        return discovered;
    }

    let mut pos = 12;
    for _ in 0..qdcount {
        let Some((_, next_pos)) = read_dns_name(pkt, pos) else {
            return discovered;
        };
        if next_pos + 4 > pkt.len() {
            return discovered;
        }
        pos = next_pos + 4;
    }

    let mut srv_records: Vec<(String, u16, String)> = Vec::new();
    let mut a_records: HashMap<String, Ipv4Addr> = HashMap::new();

    for _ in 0..total_rr {
        let Some((rr_name, next_pos)) = read_dns_name(pkt, pos) else {
            break;
        };
        if next_pos + 10 > pkt.len() {
            break;
        }
        let rtype = u16::from_be_bytes([pkt[next_pos], pkt[next_pos + 1]]);
        let rdlength = u16::from_be_bytes([pkt[next_pos + 8], pkt[next_pos + 9]]) as usize;
        let rdata_start = next_pos + 10;
        let rdata_end = rdata_start.saturating_add(rdlength);
        if rdata_end > pkt.len() {
            break;
        }

        if rtype == 33 && rdlength >= 6 {
            // SRV record
            let port = u16::from_be_bytes([pkt[rdata_start + 4], pkt[rdata_start + 5]]);
            let target = read_dns_name(pkt, rdata_start + 6)
                .map(|(t, _)| t)
                .unwrap_or_default();
            srv_records.push((rr_name, port, target));
        } else if rtype == 1 && rdlength == 4 {
            // A record
            let ip = Ipv4Addr::new(
                pkt[rdata_start],
                pkt[rdata_start + 1],
                pkt[rdata_start + 2],
                pkt[rdata_start + 3],
            );
            a_records.insert(rr_name, ip);
        }

        pos = rdata_end;
    }

    for (srv_name, port, target) in srv_records {
        let ip = a_records.get(&target).copied().unwrap_or(src_ip);
        for svc_type in ["_adb-tls-connect._tcp", "_adb-tls-pairing._tcp"] {
            let needle = format!(".{}", svc_type);
            if let Some(idx) = srv_name.find(&needle) {
                let instance_name = srv_name[..idx].to_string();
                if !instance_name.is_empty() && port > 0 {
                    discovered.push(MdnsService {
                        instance_name,
                        service_type: svc_type.to_string(),
                        address: format!("{}:{}", ip, port),
                    });
                }
            }
        }
    }

    discovered
}

fn bind_shared_mdns_socket(local_ip: Option<Ipv4Addr>) -> Option<UdpSocket> {
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return None;
        }
        let yes: libc::c_int = 1;
        let _ = libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            &yes as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
        let _ = libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEPORT,
            &yes as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );

        let mut addr: libc::sockaddr_in = std::mem::zeroed();
        addr.sin_family = libc::AF_INET as libc::sa_family_t;
        addr.sin_port = 5353u16.to_be();
        addr.sin_addr.s_addr = 0; // INADDR_ANY

        if libc::bind(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        ) < 0
        {
            addr.sin_port = 0;
            if libc::bind(
                fd,
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            ) < 0
            {
                libc::close(fd);
                return None;
            }
        }

        use std::os::unix::io::FromRawFd;
        let sock = UdpSocket::from_raw_fd(fd);
        let iface = local_ip.unwrap_or(Ipv4Addr::UNSPECIFIED);
        let _ = sock.join_multicast_v4(&Ipv4Addr::new(224, 0, 0, 251), &iface);
        if let Some(ip) = local_ip {
            let in_addr = libc::in_addr {
                s_addr: u32::from_ne_bytes(ip.octets()),
            };
            let _ = libc::setsockopt(
                sock.as_raw_fd(),
                libc::IPPROTO_IP,
                libc::IP_MULTICAST_IF,
                &in_addr as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::in_addr>() as libc::socklen_t,
            );
        }
        Some(sock)
    }
}

fn probe_mdns_services_udp(known_wifi_ip: Option<Ipv4Addr>) -> Vec<MdnsService> {
    let mut results = Vec::new();
    let local_ip = detect_local_lan_ipv4(known_wifi_ip);
    let Some(sock) = bind_shared_mdns_socket(local_ip) else {
        return results;
    };
    let _ = sock.set_read_timeout(Some(Duration::from_millis(90)));
    let _ = sock.set_write_timeout(Some(Duration::from_millis(90)));

    let multicast_dest = SocketAddrV4::new(Ipv4Addr::new(224, 0, 0, 251), 5353);
    for qname in ["_adb-tls-connect._tcp.local", "_adb-tls-pairing._tcp.local"] {
        let q_multi = build_mdns_ptr_query(qname, false);
        let q_uni = build_mdns_ptr_query(qname, true);
        let _ = sock.send_to(&q_multi, multicast_dest);
        let _ = sock.send_to(&q_uni, multicast_dest);
        if let Some(phone_ip) = known_wifi_ip {
            let phone_dest = SocketAddrV4::new(phone_ip, 5353);
            let _ = sock.send_to(&q_uni, phone_dest);
            let _ = sock.send_to(&q_multi, phone_dest);
        }
    }

    let start = Instant::now();
    let mut buf = [0u8; 2048];
    while start.elapsed() < Duration::from_millis(220) {
        match sock.recv_from(&mut buf) {
            Ok((len, SocketAddr::V4(src))) => {
                if Some(*src.ip()) == local_ip {
                    continue;
                }
                for svc in parse_mdns_packet(&buf[..len], *src.ip()) {
                    if !results
                        .iter()
                        .any(|r: &MdnsService| r.address == svc.address && r.service_type == svc.service_type)
                    {
                        results.push(svc);
                    }
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    results
}

fn poll_devices(
    adb: &Path,
    telemetry_cache: &mut HashMap<String, (Instant, AdbDevice)>,
) -> Vec<AdbDevice> {
    let mut list = Vec::new();
    let Ok(out) = run_adb_cmd(adb, &["devices", "-l"]) else {
        return list;
    };

    for line in out.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("List of devices") || trimmed.starts_with('*')
        {
            continue;
        }
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.len() < 2 {
            continue;
        }
        let serial = parts[0].to_string();
        let state = parts[1].to_string();

        let kind = if serial.contains(':') || serial.contains("._adb-tls-") {
            ConnectionKind::Wifi
        } else {
            ConnectionKind::Usb
        };

        // Check if we have fresh telemetry (< 8 seconds old) for this online device
        if state == "device" {
            if let Some((ts, cached)) = telemetry_cache.get(&serial) {
                if ts.elapsed() < Duration::from_secs(8) {
                    list.push(cached.clone());
                    continue;
                }
            }
        }

        let mut model = parts
            .iter()
            .find_map(|p| p.strip_prefix("model:"))
            .unwrap_or("Android Device")
            .replace('_', " ");

        let mut android_version = String::from("Android");
        let mut battery_level = None;
        let mut battery_charging = false;
        let mut wifi_ip = None;
        let mut resolution = None;

        if state == "device" {
            if let Ok(m) = run_adb_cmd(
                adb,
                &["-s", &serial, "shell", "getprop", "ro.product.model"],
            ) {
                let m_clean = m.trim();
                if !m_clean.is_empty() {
                    model = m_clean.to_string();
                }
            }
            if let Ok(v) = run_adb_cmd(
                adb,
                &[
                    "-s",
                    &serial,
                    "shell",
                    "getprop",
                    "ro.build.version.release",
                ],
            ) {
                let v_clean = v.trim();
                if !v_clean.is_empty() {
                    android_version = format!("Android {}", v_clean);
                }
            }
            if let Ok(bat) = run_adb_cmd(&adb, &["-s", &serial, "shell", "dumpsys", "battery"]) {
                for bline in bat.lines() {
                    let b = bline.trim();
                    if let Some(lvl) = b.strip_prefix("level:") {
                        battery_level = lvl.trim().parse::<u8>().ok();
                    } else if b.starts_with("AC powered: true")
                        || b.starts_with("USB powered: true")
                        || b.starts_with("Wireless powered: true")
                    {
                        battery_charging = true;
                    }
                }
            }
            wifi_ip = query_device_wifi_ip(adb, &serial);
            if let Ok(wm) = run_adb_cmd(adb, &["-s", &serial, "shell", "wm", "size"]) {
                for wline in wm.lines() {
                    if let Some(res) = wline.strip_prefix("Physical size:") {
                        resolution = Some(res.trim().to_string());
                    }
                }
            }
        }

        let dev = AdbDevice {
            serial: serial.clone(),
            state: state.clone(),
            model,
            android_version,
            battery_level,
            battery_charging,
            wifi_ip,
            resolution,
            kind,
        };

        if state == "device" {
            telemetry_cache.insert(serial, (Instant::now(), dev.clone()));
        }
        list.push(dev);
    }

    list
}

fn query_device_wifi_ip(adb: &Path, serial: &str) -> Option<String> {
    if let Ok(out) = run_adb_cmd(
        adb,
        &[
            "-s", serial, "shell", "ip", "-o", "-4", "addr", "show", "wlan0",
        ],
    ) {
        for token in out.split_whitespace() {
            if token.contains('/')
                && token
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_digit())
                    .unwrap_or(false)
            {
                return token.split('/').next().map(|s| s.to_string());
            }
        }
    }
    None
}

fn humanize_package_name(pkg: &str) -> String {
    let parts: Vec<&str> = pkg.split('.').collect();
    let last = parts.last().copied().unwrap_or(pkg);
    let second_last = if parts.len() >= 2 {
        parts[parts.len() - 2]
    } else {
        ""
    };

    let raw = if ["android", "app", "mobile", "client", "apps"].contains(&last)
        && !second_last.is_empty()
    {
        second_last
    } else {
        last
    };

    let mut chars = raw.chars();
    let capitalized = match chars.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
    };
    format!("{} ({})", capitalized, pkg)
}
