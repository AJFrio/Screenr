mod adb;
mod config;
mod hotkeys;
mod scrcpy;
mod tray;

use adb::{
    generate_qr_credentials, AdbAction, AdbDevice, AdbUpdate, AdbWorkerHandle, ConnectionKind,
    LogLevel, MdnsService,
};
use config::{AudioMode, MirrorPreset, StreamSettings};
use eframe::egui::{
    self, Color32, ColorImage, CornerRadius, FontId, RichText, Stroke, TextureHandle,
    TextureOptions, Vec2, ViewportCommand,
};

// X.com dark palette: pure black canvas, off-white type, gray secondary, white actions.
const BG: Color32 = Color32::from_rgb(0, 0, 0);
const HOVER: Color32 = Color32::from_rgb(22, 24, 28);
const BORDER: Color32 = Color32::from_rgb(47, 51, 54);
const TEXT: Color32 = Color32::from_rgb(231, 233, 234);
const TEXT_DIM: Color32 = Color32::from_rgb(113, 118, 123);
const BUTTON_FILL: Color32 = Color32::from_rgb(239, 243, 244);
const INK: Color32 = Color32::from_rgb(15, 20, 25);
use hotkeys::{HotkeyAction, HotkeyWatcher};
use qrcode::{Color as QrColor, QrCode};
use scrcpy::ScrcpyManager;
use tray::{TrayCommand, TrayController};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    MainCard,
    PairingSheet,
}

struct ScreenrApp {
    settings: StreamSettings,
    adb_worker: AdbWorkerHandle,
    scrcpy_mgr: ScrcpyManager,
    tray: TrayController,
    hotkeys: HotkeyWatcher,

    devices: Vec<AdbDevice>,
    mdns_services: Vec<MdnsService>,
    selected_serial: Option<String>,

    view_mode: ViewMode,
    show_more_drawer: bool,
    window_hidden_to_tray: bool,
    allow_quit: bool,
    pending_auto_mirror: bool,

    // QR Code pairing state
    qr_service_name: String,
    qr_password: String,
    qr_payload: String,
    qr_texture: Option<TextureHandle>,

    // Manual pairing inputs (inside PairingSheet)
    show_code_pairing: bool,
    pair_address_input: String,
    pair_code_input: String,

    // Single status line
    status_text: String,
    status_level: LogLevel,
}

impl ScreenrApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        configure_sleek_theme(&cc.egui_ctx);

        let settings = StreamSettings::load();
        let (qr_service_name, qr_password, qr_payload) = generate_qr_credentials();
        let adb_worker = AdbWorkerHandle::spawn(qr_service_name.clone(), qr_password.clone());
        let scrcpy_mgr = ScrcpyManager::new(adb_worker.adb_path.clone());
        let tray = TrayController::spawn(cc.egui_ctx.clone());
        let hotkeys = HotkeyWatcher::spawn(cc.egui_ctx.clone(), adb_worker.adb_path.clone());

        let mut app = Self {
            settings,
            adb_worker,
            scrcpy_mgr,
            tray,
            hotkeys,
            devices: Vec::new(),
            mdns_services: Vec::new(),
            selected_serial: None,
            view_mode: ViewMode::MainCard,
            show_more_drawer: false,
            window_hidden_to_tray: false,
            allow_quit: false,
            pending_auto_mirror: false,
            qr_service_name,
            qr_password,
            qr_payload,
            qr_texture: None,
            show_code_pairing: false,
            pair_address_input: String::new(),
            pair_code_input: String::new(),
            status_text: "Ready · Tray icon active in top bar".to_string(),
            status_level: LogLevel::Info,
        };

        app.rebuild_qr_texture(&cc.egui_ctx);
        app
    }

    fn set_status(&mut self, level: LogLevel, msg: impl Into<String>) {
        self.status_level = level;
        self.status_text = msg.into();
    }

    fn rebuild_qr_texture(&mut self, ctx: &egui::Context) {
        if let Ok(code) = QrCode::new(self.qr_payload.as_bytes()) {
            let width = code.width();
            let quiet_zone = 3;
            let scale = 6;
            let total_modules = width + quiet_zone * 2;
            let img_size = total_modules * scale;

            let mut pixels = vec![Color32::WHITE; img_size * img_size];
            for y in 0..width {
                for x in 0..width {
                    if code[(x, y)] == QrColor::Dark {
                        for dy in 0..scale {
                            for dx in 0..scale {
                                let px = (x + quiet_zone) * scale + dx;
                                let py = (y + quiet_zone) * scale + dy;
                                pixels[py * img_size + px] = Color32::BLACK;
                            }
                        }
                    }
                }
            }

            let color_image = ColorImage {
                size: [img_size, img_size],
                pixels,
            };
            self.qr_texture =
                Some(ctx.load_texture("adb_qr_code", color_image, TextureOptions::NEAREST));
        }
    }

    fn active_device(&self) -> Option<&AdbDevice> {
        if let Some(ref sel) = self.selected_serial {
            if let Some(dev) = self.devices.iter().find(|d| &d.serial == sel) {
                return Some(dev);
            }
        }
        self.devices.first()
    }

    fn trigger_connect_and_mirror(&mut self, ctx: &egui::Context) {
        if let Some(dev) = self.active_device().cloned() {
            if dev.state == "device" {
                if !self.scrcpy_mgr.is_mirroring_device(&dev.serial) {
                    match self.scrcpy_mgr.start_screen_mirror(
                        &dev.serial,
                        &dev.model,
                        &self.settings,
                    ) {
                        Ok(msg) => self.set_status(LogLevel::Success, msg),
                        Err(err) => self.set_status(LogLevel::Error, err),
                    }
                }
                return;
            }
        }

        // No active online device yet: try connecting to discovered mDNS connect service or last known Wi-Fi address
        if let Some(svc) = self
            .mdns_services
            .iter()
            .find(|s| s.service_type.contains("_adb-tls-connect._tcp"))
            .cloned()
        {
            self.pending_auto_mirror = true;
            self.set_status(
                LogLevel::Info,
                format!("Connecting to {} & launching mirror...", svc.address),
            );
            let _ = self
                .adb_worker
                .action_tx
                .send(AdbAction::ConnectDirect { addr: svc.address });
            return;
        }

        if let Some(ref addr) = self.settings.last_known_wifi_addr.clone() {
            self.pending_auto_mirror = true;
            self.set_status(LogLevel::Info, format!("Reconnecting to {}...", addr));
            let _ = self
                .adb_worker
                .action_tx
                .send(AdbAction::ConnectDirect { addr: addr.clone() });
        }

        // Also show the window on the Pairing sheet in case the phone isn't paired yet
        self.window_hidden_to_tray = false;
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(ViewportCommand::Focus);
        if self.devices.is_empty() {
            self.view_mode = ViewMode::PairingSheet;
        }
    }

    fn sync_tray_state(&self) {
        let dev = self.active_device();
        let label = dev.map(|d| d.model.clone());
        let is_usb = dev.map(|d| d.kind == ConnectionKind::Usb).unwrap_or(false);
        let is_mirroring = dev
            .map(|d| self.scrcpy_mgr.is_mirroring_device(&d.serial))
            .unwrap_or(false);
        self.tray
            .update_status(label, is_usb, is_mirroring, self.settings.turn_screen_off);
    }

    fn poll_updates(&mut self, ctx: &egui::Context) {
        // 0. Handle Focus-Aware Phone Window Hotkeys (Ctrl+Space = Home, Ctrl+Esc = Close & Lock, etc.)
        while let Ok(hk) = self.hotkeys.rx.try_recv() {
            match hk {
                HotkeyAction::GoHome => {
                    self.set_status(LogLevel::Info, "Shortcut: Home (Ctrl+Alt)");
                }
                HotkeyAction::StopMirroring => {
                    if let Some(dev) = self.active_device().cloned() {
                        self.scrcpy_mgr.stop_for_device(&dev.serial);
                    }
                    self.set_status(
                        LogLevel::Info,
                        "Mirroring stopped via Ctrl+Esc · Phone locked",
                    );
                }
                HotkeyAction::GoBack => {
                    self.set_status(LogLevel::Info, "Shortcut: Back (Ctrl+Backspace)");
                }
                HotkeyAction::AppSwitcher => {
                    self.set_status(LogLevel::Info, "Shortcut: App Switcher (Ctrl+Tab)");
                }
            }
        }

        // 1. Handle System Tray clicks
        while let Ok(cmd) = self.tray.rx.try_recv() {
            match cmd {
                TrayCommand::ConnectAndMirror => {
                    self.trigger_connect_and_mirror(ctx);
                }
                TrayCommand::StopMirroring => {
                    if let Some(dev) = self.active_device().cloned() {
                        self.scrcpy_mgr.stop_for_device(&dev.serial);
                        self.set_status(LogLevel::Info, "Mirroring stopped");
                    }
                }
                TrayCommand::ToggleWindow => {
                    self.window_hidden_to_tray = false;
                    ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(ViewportCommand::Focus);
                }
                TrayCommand::SwitchUsbToWireless => {
                    if let Some(dev) = self.active_device().cloned() {
                        let _ = self
                            .adb_worker
                            .action_tx
                            .send(AdbAction::SwitchUsbToWireless { serial: dev.serial });
                    }
                }
                TrayCommand::TakeScreenshot => {
                    if let Some(dev) = self.active_device().cloned() {
                        let _ = self
                            .adb_worker
                            .action_tx
                            .send(AdbAction::TakeScreenshot { serial: dev.serial });
                    }
                }
                TrayCommand::QuitApp => {
                    self.allow_quit = true;
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            }
        }

        // 2. Handle ADB worker updates
        while let Ok(update) = self.adb_worker.update_rx.try_recv() {
            match update {
                AdbUpdate::StateSnapshot {
                    devices,
                    mdns_services,
                } => {
                    if self.pair_address_input.trim().is_empty() {
                        if let Some(pair_svc) = mdns_services
                            .iter()
                            .find(|s| s.service_type.contains("_adb-tls-pairing._tcp"))
                        {
                            self.pair_address_input = pair_svc.address.clone();
                        }
                    }

                    // Prefer Wi-Fi connected device when selecting default
                    if self.selected_serial.is_none()
                        || !devices
                            .iter()
                            .any(|d| Some(&d.serial) == self.selected_serial.as_ref())
                    {
                        let preferred = devices
                            .iter()
                            .find(|d| d.state == "device" && d.kind == ConnectionKind::Wifi)
                            .or_else(|| devices.iter().find(|d| d.state == "device"))
                            .or_else(|| devices.first());
                        self.selected_serial = preferred.map(|d| d.serial.clone());
                    }

                    // Remember last known Wi-Fi address for 1-click tray reconnects
                    if let Some(wifi_dev) = devices
                        .iter()
                        .find(|d| d.state == "device" && d.kind == ConnectionKind::Wifi)
                    {
                        if wifi_dev.serial.contains(':')
                            && self.settings.last_known_wifi_addr.as_ref() != Some(&wifi_dev.serial)
                        {
                            self.settings.last_known_wifi_addr = Some(wifi_dev.serial.clone());
                            self.settings.save();
                        }
                    }

                    self.devices = devices;
                    self.mdns_services = mdns_services;

                    // If user clicked "Connect & Mirror" from tray while phone was connecting, launch mirror now!
                    if self.pending_auto_mirror {
                        if let Some(dev) = self.active_device().cloned() {
                            if dev.state == "device" {
                                self.pending_auto_mirror = false;
                                self.view_mode = ViewMode::MainCard;
                                match self.scrcpy_mgr.start_screen_mirror(
                                    &dev.serial,
                                    &dev.model,
                                    &self.settings,
                                ) {
                                    Ok(msg) => self.set_status(LogLevel::Success, msg),
                                    Err(err) => self.set_status(LogLevel::Error, err),
                                }
                            }
                        }
                    }
                }
                AdbUpdate::LogMessage { level, message } => {
                    self.set_status(level, message);
                }
                AdbUpdate::AppsLoaded { .. } => {}
                AdbUpdate::QrAutoPaired { address } => {
                    self.set_status(
                        LogLevel::Success,
                        format!("Paired with Pixel ({})!", address),
                    );
                    self.view_mode = ViewMode::MainCard;
                    self.pending_auto_mirror = true;
                }
            }
        }

        for msg in self.scrcpy_mgr.poll_running() {
            self.set_status(LogLevel::Info, msg);
        }

        // 3. Handle file drag & drop anywhere on the window
        let dropped_files: Vec<std::path::PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !dropped_files.is_empty() {
            if let Some(dev) = self.active_device().cloned() {
                for path in dropped_files {
                    let _ = self.adb_worker.action_tx.send(AdbAction::PushFileOrApk {
                        serial: dev.serial.clone(),
                        path,
                    });
                }
            } else {
                self.set_status(
                    LogLevel::Warning,
                    "Connect your Pixel 10 first to drop files",
                );
            }
        }

        self.sync_tray_state();
    }
}

impl eframe::App for ScreenrApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Intercept window close button (X) to minimize/hide to tray unless Quit was explicitly clicked
        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_quit {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.window_hidden_to_tray = true;
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            self.set_status(
                LogLevel::Info,
                "Running in system tray · Click tray icon anytime to pull up Pixel 10",
            );
        }

        self.poll_updates(ctx);
        ctx.request_repaint_after(std::time::Duration::from_millis(450));

        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(BG)
                    .inner_margin(egui::Margin::symmetric(18, 16)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Screenr")
                            .font(FontId::proportional(20.0))
                            .strong()
                            .color(TEXT),
                    );
                    ui.add_space(6.0);
                    pill(ui, "Pixel");

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.view_mode == ViewMode::PairingSheet {
                            if header_button(ui, "Back").clicked() {
                                self.view_mode = ViewMode::MainCard;
                            }
                        } else if header_button(ui, "Pair")
                            .on_hover_text("Pair wirelessly with a QR code or pairing code")
                            .clicked()
                        {
                            self.view_mode = ViewMode::PairingSheet;
                        }
                    });
                });

                ui.add_space(12.0);

                match self.view_mode {
                    ViewMode::MainCard => self.render_sleek_main_view(ui, ctx),
                    ViewMode::PairingSheet => self.render_sleek_pairing_sheet(ui, ctx),
                }
            });
    }
}

impl ScreenrApp {
    fn render_sleek_main_view(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let dev_opt = self.active_device().cloned();

        // 1. HERO PHONE CARD
        sleek_card(ui, |ui| {
            if let Some(ref dev) = dev_opt {
                let is_online = dev.state == "device";
                let is_mirroring = self.scrcpy_mgr.is_mirroring_device(&dev.serial);

                ui.horizontal(|ui| {
                    let dot_color = if is_online { TEXT } else { TEXT_DIM };
                    ui.label(RichText::new("●").size(12.0).color(dot_color));
                    ui.label(RichText::new(&dev.model).size(17.0).strong().color(TEXT));

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let conn = match dev.kind {
                            ConnectionKind::Wifi => "Wi-Fi",
                            ConnectionKind::Usb => "USB",
                        };
                        let bat_str = dev
                            .battery_level
                            .map(|b| format!(" · {}%", b))
                            .unwrap_or_default();
                        let state = if is_mirroring {
                            "Mirroring"
                        } else if is_online {
                            "Ready"
                        } else {
                            "Locked"
                        };
                        pill(ui, &format!("{} · {}{}", state, conn, bat_str));
                    });
                });

                if dev.kind == ConnectionKind::Usb && is_online {
                    ui.add_space(2.0);
                    if text_button(ui, "Switch to wireless").clicked() {
                        let _ = self
                            .adb_worker
                            .action_tx
                            .send(AdbAction::SwitchUsbToWireless {
                                serial: dev.serial.clone(),
                            });
                    }
                } else if !is_online {
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("Unlock the phone and allow debugging")
                            .size(13.0)
                            .color(TEXT_DIM),
                    );
                }

                ui.add_space(12.0);

                if is_mirroring {
                    if outline_button(ui, "Stop mirroring").clicked() {
                        self.scrcpy_mgr.stop_for_device(&dev.serial);
                        self.set_status(LogLevel::Info, "Mirroring stopped");
                    }
                } else if primary_button(ui, "Mirror").clicked() {
                    match self.scrcpy_mgr.start_screen_mirror(
                        &dev.serial,
                        &dev.model,
                        &self.settings,
                    ) {
                        Ok(msg) => self.set_status(LogLevel::Success, msg),
                        Err(err) => self.set_status(LogLevel::Error, err),
                    }
                }
            } else {
                // No phone connected yet
                ui.horizontal(|ui| {
                    ui.label(RichText::new("●").size(12.0).color(TEXT_DIM));
                    ui.label(
                        RichText::new("No phone connected")
                            .size(17.0)
                            .strong()
                            .color(TEXT),
                    );
                });
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Scan the QR code in Wireless debugging, or plug in over USB.")
                        .size(13.0)
                        .color(TEXT_DIM),
                );
                ui.add_space(12.0);

                if primary_button(ui, "Pair wirelessly").clicked() {
                    self.view_mode = ViewMode::PairingSheet;
                }
            }
        });

        ui.add_space(10.0);

        // 2. SLEEK 1-ROW QUICK REMOTE PILL (5 Clean Buttons)
        sleek_card(ui, |ui| {
            let serial_opt = dev_opt.as_ref().map(|d| d.serial.clone());
            ui.add_enabled_ui(serial_opt.is_some(), |ui| {
                let serial = serial_opt.unwrap_or_default();
                ui.columns(5, |cols| {
                    if mini_remote_btn(&mut cols[0], "◀", "Back").clicked() {
                        let _ = self.adb_worker.action_tx.send(AdbAction::SendKeyEvent {
                            serial: serial.clone(),
                            keycode: "KEYCODE_BACK",
                        });
                    }
                    if mini_remote_btn(&mut cols[1], "⏺", "Home").clicked() {
                        let _ = self.adb_worker.action_tx.send(AdbAction::SendKeyEvent {
                            serial: serial.clone(),
                            keycode: "KEYCODE_HOME",
                        });
                    }
                    if mini_remote_btn(&mut cols[2], "▣", "Apps").clicked() {
                        let _ = self.adb_worker.action_tx.send(AdbAction::SendKeyEvent {
                            serial: serial.clone(),
                            keycode: "KEYCODE_APP_SWITCH",
                        });
                    }
                    if mini_remote_btn(&mut cols[3], "⏻", "Lock").clicked() {
                        let _ = self.adb_worker.action_tx.send(AdbAction::SendKeyEvent {
                            serial: serial.clone(),
                            keycode: "KEYCODE_POWER",
                        });
                    }
                    if mini_remote_btn(&mut cols[4], "◻", "Shot").clicked() {
                        let _ = self.adb_worker.action_tx.send(AdbAction::TakeScreenshot {
                            serial: serial.clone(),
                        });
                    }
                });
            });
            ui.add_space(6.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new(
                        "Ctrl+Alt Home  ·  Ctrl+Esc Stop  ·  Ctrl+Bksp Back  ·  Ctrl+Tab Apps",
                    )
                    .size(11.0)
                    .color(TEXT_DIM),
                );
            });
        });

        ui.add_space(10.0);

        // 3. CLEAN ESSENTIAL SETTINGS CARD
        sleek_card(ui, |ui| {
            let mut changed = false;

            // Quality Profile Row
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new("Performance").size(15.0).strong().color(TEXT));
                    ui.label(
                        RichText::new(self.settings.preset.short_badge())
                            .size(13.0)
                            .color(TEXT_DIM),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    egui::ComboBox::from_id_salt("sleek_preset_cb")
                        .selected_text(
                            RichText::new(self.settings.preset.label())
                                .size(12.5)
                                .strong(),
                        )
                        .show_ui(ui, |ui| {
                            for p in [
                                MirrorPreset::Pixel10Ultra120Hz,
                                MirrorPreset::WirelessBalanced,
                                MirrorPreset::BatterySaver,
                                MirrorPreset::VirtualDesktopDisplay,
                            ] {
                                if ui
                                    .selectable_value(&mut self.settings.preset, p, p.label())
                                    .changed()
                                {
                                    self.settings.apply_preset(p);
                                    changed = true;
                                }
                            }
                        });
                });
            });

            ui.add_space(8.0);
            ui.separator();
            ui.add_space(8.0);

            // Toggle 1: Keep Phone Screen Off (Default ON)
            changed |= sleek_toggle_row(
                ui,
                "Keep Phone Screen Off",
                "Physical Pixel 10 screen stays dark while mirroring",
                &mut self.settings.turn_screen_off,
            );

            ui.add_space(8.0);

            // Toggle 2: Forward Audio to PC
            let mut audio_on = self.settings.audio_mode != AudioMode::Muted;
            if sleek_toggle_row(
                ui,
                "Stream Phone Audio to PC",
                "Play media & notification sounds on your computer",
                &mut audio_on,
            ) {
                self.settings.audio_mode = if audio_on {
                    AudioMode::OutputToPc
                } else {
                    AudioMode::Muted
                };
                changed = true;
            }

            ui.add_space(8.0);

            // Toggle 3: Always on Top
            changed |= sleek_toggle_row(
                ui,
                "Pin Phone Window on Top",
                "Keep mirror window floating above other desktop apps",
                &mut self.settings.always_on_top,
            );

            // Optional collapsible "More Tools" (Camera, Volume, Shade)
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let arrow = if self.show_more_drawer { "▴" } else { "▾" };
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new(format!("More  {}", arrow))
                                .size(13.0)
                                .color(TEXT_DIM),
                        )
                        .frame(false),
                    )
                    .clicked()
                {
                    self.show_more_drawer = !self.show_more_drawer;
                }
            });

            if self.show_more_drawer {
                ui.add_space(6.0);
                let serial_opt = dev_opt.as_ref().map(|d| d.serial.clone());
                ui.add_enabled_ui(serial_opt.is_some(), |ui| {
                    let serial = serial_opt.unwrap_or_default();
                    ui.columns(4, |c| {
                        if c[0].button("Vol −").clicked() {
                            let _ = self.adb_worker.action_tx.send(AdbAction::SendKeyEvent {
                                serial: serial.clone(),
                                keycode: "KEYCODE_VOLUME_DOWN",
                            });
                        }
                        if c[1].button("Vol +").clicked() {
                            let _ = self.adb_worker.action_tx.send(AdbAction::SendKeyEvent {
                                serial: serial.clone(),
                                keycode: "KEYCODE_VOLUME_UP",
                            });
                        }
                        if c[2].button("Shade").clicked() {
                            let _ =
                                self.adb_worker
                                    .action_tx
                                    .send(AdbAction::ExpandNotifications {
                                        serial: serial.clone(),
                                    });
                        }
                        if c[3].button("Camera").clicked() {
                            if let Some(ref d) = dev_opt {
                                let _ = self.scrcpy_mgr.start_camera_stream(
                                    &d.serial,
                                    &d.model,
                                    &self.settings,
                                );
                            }
                        }
                    });
                });
            }

            if changed {
                self.settings.save();
            }
        });

        ui.add_space(10.0);

        // 4. SUBTLE DROP FOOTER + TRAY HINT
        egui::Frame::NONE
            .fill(BG)
            .corner_radius(CornerRadius::same(16))
            .inner_margin(egui::Margin::symmetric(14, 10))
            .stroke(Stroke::new(1.0, BORDER))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Drop a file or .apk to send it to the phone")
                            .size(13.0)
                            .color(TEXT_DIM),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new("Hide").size(13.0).strong().color(TEXT),
                                )
                                .frame(false),
                            )
                            .clicked()
                        {
                            self.window_hidden_to_tray = true;
                            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
                        }
                    });
                });
            });

        ui.add_space(6.0);
        let status_col = match self.status_level {
            LogLevel::Info | LogLevel::Warning => TEXT_DIM,
            LogLevel::Success | LogLevel::Error => TEXT,
        };
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new(&self.status_text)
                    .size(11.5)
                    .color(status_col),
            );
        });
    }

    fn render_sleek_pairing_sheet(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        sleek_card(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new("Scan with your phone")
                        .size(17.0)
                        .strong()
                        .color(TEXT),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Settings → Developer options → Wireless debugging\nPair device with QR code")
                        .size(13.0)
                        .color(TEXT_DIM),
                );
                ui.add_space(12.0);

                if let Some(ref tex) = self.qr_texture {
                    egui::Frame::NONE
                        .fill(Color32::WHITE)
                        .corner_radius(CornerRadius::same(14))
                        .inner_margin(egui::Margin::same(10))
                        .show(ui, |ui| {
                            ui.image((tex.id(), Vec2::new(190.0, 190.0)));
                        });
                }

                ui.add_space(10.0);
                ui.label(
                    RichText::new("Waiting for a scan on this Wi-Fi network")
                        .size(13.0)
                        .color(TEXT_DIM),
                );
            });

            ui.add_space(10.0);
            ui.separator();
            ui.add_space(6.0);

            ui.horizontal(|ui| {
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new(if self.show_code_pairing {
                                "Hide pairing code  ▴"
                            } else {
                                "Use a pairing code  ▾"
                            })
                            .size(13.0)
                            .color(TEXT),
                        )
                        .frame(false),
                    )
                    .clicked()
                {
                    self.show_code_pairing = !self.show_code_pairing;
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if header_button(ui, "New QR").clicked() {
                        let (s, p, payload) = generate_qr_credentials();
                        self.qr_service_name = s.clone();
                        self.qr_password = p.clone();
                        self.qr_payload = payload;
                        self.rebuild_qr_texture(ctx);
                        let _ =
                            self.adb_worker
                                .action_tx
                                .send(AdbAction::RegenerateQrCredentials {
                                    service_name: s,
                                    password: p,
                                });
                    }
                });
            });

            if self.show_code_pairing {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.pair_address_input)
                            .hint_text("IP:Port (e.g. 192.168.1.50:39123)")
                            .desired_width(195.0),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut self.pair_code_input)
                            .hint_text("6-Digit Code")
                            .desired_width(85.0),
                    );
                    if ui.button("Pair").clicked() && !self.pair_address_input.trim().is_empty() {
                        if self.pair_code_input.trim().is_empty() {
                            let _ = self.adb_worker.action_tx.send(AdbAction::ConnectDirect {
                                addr: self.pair_address_input.clone(),
                            });
                        } else {
                            let _ = self.adb_worker.action_tx.send(AdbAction::PairWithCode {
                                addr: self.pair_address_input.clone(),
                                code: self.pair_code_input.clone(),
                            });
                        }
                    }
                });
            }
        });
    }
}

fn sleek_card(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::NONE
        .fill(BG)
        .corner_radius(CornerRadius::same(16))
        .inner_margin(egui::Margin::symmetric(16, 14))
        .stroke(Stroke::new(1.0, BORDER))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add_contents(ui);
        });
}

fn pill(ui: &mut egui::Ui, label: &str) {
    egui::Frame::NONE
        .fill(BG)
        .corner_radius(CornerRadius::same(20))
        .inner_margin(egui::Margin::symmetric(10, 3))
        .stroke(Stroke::new(1.0, BORDER))
        .show(ui, |ui| {
            ui.label(RichText::new(label).size(12.0).color(TEXT_DIM));
        });
}

fn text_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(label).size(13.0).color(TEXT)).frame(false))
}

fn header_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(label).size(13.0).strong().color(TEXT))
            .fill(BG)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(16)),
    )
}

fn primary_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add_sized(
        [ui.available_width(), 44.0],
        egui::Button::new(RichText::new(label).size(15.0).strong().color(INK))
            .fill(BUTTON_FILL)
            .corner_radius(CornerRadius::same(22)),
    )
}

fn outline_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add_sized(
        [ui.available_width(), 40.0],
        egui::Button::new(RichText::new(label).size(14.0).strong().color(TEXT))
            .fill(BG)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(20)),
    )
}

fn mini_remote_btn(ui: &mut egui::Ui, icon: &str, label: &str) -> egui::Response {
    ui.add_sized(
        [ui.available_width(), 44.0],
        egui::Button::new(
            RichText::new(format!("{}\n{}", icon, label))
                .size(12.0)
                .strong()
                .color(TEXT),
        )
        .fill(BG)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(12)),
    )
}

fn sleek_toggle_row(ui: &mut egui::Ui, title: &str, subtitle: &str, value: &mut bool) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(RichText::new(title).size(15.0).strong().color(TEXT));
            ui.label(RichText::new(subtitle).size(13.0).color(TEXT_DIM));
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ios_switch(ui, value).changed() {
                changed = true;
            }
        });
    });
    changed
}

/// Renders a clean iOS/macOS-style pill toggle switch
fn ios_switch(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let desired_size = ui.spacing().interact_size.y * egui::vec2(1.9, 1.05);
    let (rect, mut response) = ui.allocate_exact_size(desired_size, egui::Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *on, "")
    });

    if ui.is_rect_visible(rect) {
        let how_on = ui.ctx().animate_bool_responsive(response.id, *on);
        let bg_color = if *on {
            BUTTON_FILL
        } else {
            Color32::from_rgb(62, 65, 68)
        };
        let knob = if *on { INK } else { TEXT };
        let radius = 0.5 * rect.height();
        ui.painter()
            .rect_filled(rect, CornerRadius::same(radius as u8), bg_color);
        let circle_x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), how_on);
        let center = egui::pos2(circle_x, rect.center().y);
        ui.painter().circle_filled(center, 0.72 * radius, knob);
    }

    response
}

fn configure_sleek_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.override_text_color = Some(TEXT);
    visuals.panel_fill = BG;
    visuals.window_fill = BG;
    visuals.extreme_bg_color = BG;
    visuals.faint_bg_color = HOVER;
    visuals.code_bg_color = HOVER;
    visuals.hyperlink_color = TEXT;
    visuals.warn_fg_color = TEXT;
    visuals.error_fg_color = TEXT;
    visuals.window_stroke = Stroke::new(1.0, BORDER);
    visuals.selection.bg_fill = HOVER;
    visuals.selection.stroke = Stroke::new(1.0, TEXT);

    let hairline = Stroke::new(1.0, BORDER);
    visuals.widgets.noninteractive.bg_fill = BG;
    visuals.widgets.noninteractive.weak_bg_fill = BG;
    visuals.widgets.noninteractive.bg_stroke = hairline;
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_DIM);

    visuals.widgets.inactive.bg_fill = BG;
    visuals.widgets.inactive.weak_bg_fill = Color32::from_rgba_unmultiplied(231, 233, 234, 14);
    visuals.widgets.inactive.bg_stroke = hairline;
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);

    visuals.widgets.hovered.bg_fill = HOVER;
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgba_unmultiplied(231, 233, 234, 28);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(83, 100, 113));
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, Color32::WHITE);

    visuals.widgets.active.bg_fill = BUTTON_FILL;
    visuals.widgets.active.weak_bg_fill = Color32::from_rgba_unmultiplied(231, 233, 234, 40);
    visuals.widgets.active.bg_stroke = Stroke::NONE;
    visuals.widgets.active.fg_stroke = Stroke::new(1.0, INK);

    visuals.widgets.open.bg_fill = HOVER;
    visuals.widgets.open.bg_stroke = hairline;
    visuals.widgets.open.fg_stroke = Stroke::new(1.0, TEXT);

    ctx.set_visuals(visuals);
}

fn main() -> eframe::Result<()> {
    let start_in_tray =
        std::env::args().any(|arg| arg == "--tray" || arg == "--startup" || arg == "--minimized");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Screenr")
            .with_inner_size([400.0, 600.0])
            .with_min_inner_size([360.0, 520.0])
            .with_resizable(false)
            .with_visible(!start_in_tray),
        ..Default::default()
    };

    eframe::run_native(
        "Screenr",
        options,
        Box::new(|cc| Ok(Box::new(ScreenrApp::new(cc)))),
    )
}
