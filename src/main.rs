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

// Black canvas, one raised surface, off-white type. Color is reserved for errors.
const BG: Color32 = Color32::from_rgb(0, 0, 0);
const ELEVATED: Color32 = Color32::from_rgb(22, 24, 28);
const BORDER: Color32 = Color32::from_rgb(42, 45, 49);
const TEXT: Color32 = Color32::from_rgb(236, 237, 238);
const TEXT_DIM: Color32 = Color32::from_rgb(125, 130, 136);
const BUTTON_FILL: Color32 = Color32::from_rgb(239, 243, 244);
const INK: Color32 = Color32::from_rgb(15, 20, 25);
const ERROR: Color32 = Color32::from_rgb(255, 140, 130);
const WARN: Color32 = Color32::from_rgb(196, 168, 120);
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
    window_hidden_to_tray: bool,
    applied_viewport_size: bool,
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
        configure_theme(&cc.egui_ctx);

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
            window_hidden_to_tray: false,
            applied_viewport_size: false,
            allow_quit: false,
            pending_auto_mirror: false,
            qr_service_name,
            qr_password,
            qr_payload,
            qr_texture: None,
            show_code_pairing: false,
            pair_address_input: String::new(),
            pair_code_input: String::new(),
            status_text: String::new(),
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
            self.set_status(LogLevel::Info, format!("Connecting to {}", svc.address));
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
        // Mirror-window shortcuts: Ctrl+Alt home, Ctrl+Esc stop, Ctrl+Backspace back, Ctrl+Tab recents.
        while let Ok(hk) = self.hotkeys.rx.try_recv() {
            match hk {
                HotkeyAction::StopMirroring => {
                    if let Some(dev) = self.active_device().cloned() {
                        self.scrcpy_mgr.stop_for_device(&dev.serial);
                    }
                    self.set_status(LogLevel::Info, "Mirroring stopped");
                }
                HotkeyAction::GoHome | HotkeyAction::GoBack | HotkeyAction::AppSwitcher => {}
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
                AdbUpdate::QrAutoPaired { address: _ } => {
                    self.set_status(LogLevel::Success, "Paired");
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
                self.set_status(LogLevel::Warning, "Connect a phone before sending files");
            }
        }

        self.sync_tray_state();
    }
}

impl eframe::App for ScreenrApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_quit {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.window_hidden_to_tray = true;
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            self.set_status(LogLevel::Info, "Running in the tray");
        }

        self.poll_updates(ctx);
        ctx.request_repaint_after(std::time::Duration::from_millis(450));
        if !self.applied_viewport_size {
            self.applied_viewport_size = true;
            ctx.send_viewport_cmd(ViewportCommand::InnerSize(Vec2::new(400.0, 424.0)));
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(BG)
                    .inner_margin(egui::Margin::symmetric(20, 18)),
            )
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                match self.view_mode {
                    ViewMode::MainCard => self.render_main(ui),
                    ViewMode::PairingSheet => self.render_pairing(ui, ctx),
                }
            });

        self.paint_drop_target(ctx);
    }
}

impl ScreenrApp {
    fn cycle_device(&mut self) {
        if self.devices.is_empty() {
            return;
        }
        let current = self
            .selected_serial
            .as_ref()
            .and_then(|serial| self.devices.iter().position(|d| &d.serial == serial))
            .unwrap_or(0);
        let next = (current + 1) % self.devices.len();
        self.selected_serial = Some(self.devices[next].serial.clone());
    }

    fn send_key(&self, serial: &str, keycode: &'static str) {
        let _ = self.adb_worker.action_tx.send(AdbAction::SendKeyEvent {
            serial: serial.to_string(),
            keycode,
        });
    }

    fn submit_pairing(&mut self) {
        let addr = self.pair_address_input.trim().to_string();
        if addr.is_empty() {
            self.set_status(LogLevel::Warning, "Enter the phone's pairing address");
            return;
        }
        let code = self.pair_code_input.trim().to_string();
        if code.is_empty() {
            let _ = self
                .adb_worker
                .action_tx
                .send(AdbAction::ConnectDirect { addr });
        } else {
            let _ = self
                .adb_worker
                .action_tx
                .send(AdbAction::PairWithCode { addr, code });
        }
    }

    fn paint_drop_target(&self, ctx: &egui::Context) {
        let dragging = ctx.input(|i| !i.raw.hovered_files.is_empty());
        if !dragging {
            return;
        }
        let message = if self.active_device().is_some() {
            "Drop to send"
        } else {
            "Connect a phone first"
        };
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("file_drop"),
        ));
        let rect = ctx.screen_rect().shrink(12.0);
        painter.rect(
            rect,
            CornerRadius::same(12),
            Color32::from_rgba_unmultiplied(0, 0, 0, 210),
            Stroke::new(1.0, BUTTON_FILL),
            egui::StrokeKind::Inside,
        );
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            message,
            FontId::proportional(15.0),
            TEXT,
        );
    }

    fn render_status(&self, ui: &mut egui::Ui) {
        if self.status_text.is_empty() {
            return;
        }
        ui.add_space(16.0);
        let color = match self.status_level {
            LogLevel::Error => ERROR,
            LogLevel::Warning => WARN,
            _ => TEXT_DIM,
        };
        ui.add(
            egui::Label::new(RichText::new(&self.status_text).size(12.0).color(color)).truncate(),
        )
        .on_hover_text(&self.status_text);
    }

    fn render_main(&mut self, ui: &mut egui::Ui) {
        let dev_opt = self.active_device().cloned();
        if let Some(dev) = dev_opt {
            self.render_connected(ui, &dev);
        } else {
            self.render_empty(ui);
        }
    }

    fn render_empty(&mut self, ui: &mut egui::Ui) {
        let extra = (ui.available_height() - 110.0).max(0.0);
        ui.add_space(extra * 0.5);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("No phone").size(20.0).strong().color(TEXT));
            ui.add_space(6.0);
            ui.label(
                RichText::new("USB, or wireless debugging on this network.")
                    .size(13.0)
                    .color(TEXT_DIM),
            );
        });
        ui.add_space(18.0);
        if primary_button(ui, "Pair wirelessly").clicked() {
            self.view_mode = ViewMode::PairingSheet;
        }
        self.render_status(ui);
    }

    fn render_connected(&mut self, ui: &mut egui::Ui, dev: &AdbDevice) {
        let online = dev.state == "device";
        let mirroring = self.scrcpy_mgr.is_mirroring_device(&dev.serial);
        let serial = dev.serial.clone();

        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if text_button(ui, "Pair")
                    .on_hover_text("Pair another phone")
                    .clicked()
                {
                    self.view_mode = ViewMode::PairingSheet;
                }
                if self.devices.len() > 1
                    && text_button(ui, "Switch")
                        .on_hover_text("Show another phone")
                        .clicked()
                {
                    self.cycle_device();
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(device_title(dev))
                                .size(18.0)
                                .strong()
                                .color(TEXT),
                        )
                        .truncate(),
                    );
                });
            });
        });

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if online && dev.kind == ConnectionKind::Usb {
                    if text_button(ui, "Go wireless")
                        .on_hover_text("Stay connected after unplugging")
                        .clicked()
                    {
                        let _ = self
                            .adb_worker
                            .action_tx
                            .send(AdbAction::SwitchUsbToWireless {
                                serial: serial.clone(),
                            });
                    }
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    presence_dot(ui, if online { TEXT } else { TEXT_DIM });
                    ui.add_space(6.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new(device_meta(dev, mirroring))
                                .size(13.0)
                                .color(TEXT_DIM),
                        )
                        .truncate(),
                    );
                });
            });
        });

        ui.add_space(16.0);
        if mirroring {
            if stop_button(ui, "Stop mirroring").clicked() {
                self.scrcpy_mgr.stop_for_device(&serial);
                self.set_status(LogLevel::Info, "Mirroring stopped");
            }
        } else {
            ui.add_enabled_ui(online, |ui| {
                if primary_button(ui, "Mirror").clicked() {
                    match self
                        .scrcpy_mgr
                        .start_screen_mirror(&serial, &dev.model, &self.settings)
                    {
                        Ok(msg) => self.set_status(LogLevel::Success, msg),
                        Err(err) => self.set_status(LogLevel::Error, err),
                    }
                }
            });
        }

        ui.add_space(18.0);
        ui.add_enabled_ui(online, |ui| {
            surface(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                ui.columns(5, |cols| {
                    if command_button(&mut cols[0], "Back", "Ctrl+Backspace").clicked() {
                        self.send_key(&serial, "KEYCODE_BACK");
                    }
                    if command_button(&mut cols[1], "Home", "Ctrl+Alt").clicked() {
                        self.send_key(&serial, "KEYCODE_HOME");
                    }
                    if command_button(&mut cols[2], "Recents", "Ctrl+Tab").clicked() {
                        self.send_key(&serial, "KEYCODE_APP_SWITCH");
                    }
                    if command_button(&mut cols[3], "Lock", "Power").clicked() {
                        self.send_key(&serial, "KEYCODE_POWER");
                    }
                    if command_button(&mut cols[4], "Shot", "Save a screenshot").clicked() {
                        let _ = self.adb_worker.action_tx.send(AdbAction::TakeScreenshot {
                            serial: serial.clone(),
                        });
                    }
                });
            });

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 14.0;
                if quiet_button(ui, "Vol −").clicked() {
                    self.send_key(&serial, "KEYCODE_VOLUME_DOWN");
                }
                if quiet_button(ui, "Vol +").clicked() {
                    self.send_key(&serial, "KEYCODE_VOLUME_UP");
                }
                if quiet_button(ui, "Notifications").clicked() {
                    let _ = self
                        .adb_worker
                        .action_tx
                        .send(AdbAction::ExpandNotifications {
                            serial: serial.clone(),
                        });
                }
                if quiet_button(ui, "Camera").clicked() {
                    let _ =
                        self.scrcpy_mgr
                            .start_camera_stream(&serial, &dev.model, &self.settings);
                }
            });
        });

        ui.add_space(16.0);
        hairline(ui);
        ui.add_space(14.0);

        let mut changed = false;
        let presets = [
            MirrorPreset::Pixel10Ultra120Hz,
            MirrorPreset::WirelessBalanced,
            MirrorPreset::BatterySaver,
            MirrorPreset::VirtualDesktopDisplay,
        ];
        let labels: [&str; 4] = presets.map(|preset| preset.label());
        let selected = presets
            .iter()
            .position(|preset| *preset == self.settings.preset)
            .unwrap_or(0);
        if let Some(index) = segmented(ui, &labels, selected) {
            self.settings.apply_preset(presets[index]);
            changed = true;
        }

        ui.add_space(6.0);
        ui.label(
            RichText::new(self.settings.preset.detail())
                .size(12.0)
                .color(TEXT_DIM),
        );

        ui.add_space(14.0);
        changed |= toggle_row(
            ui,
            "Screen off",
            "The phone display stays dark while mirroring.",
            &mut self.settings.turn_screen_off,
        );
        ui.add_space(8.0);

        let mut audio_on = self.settings.audio_mode != AudioMode::Muted;
        if toggle_row(
            ui,
            "Play audio here",
            "Media and notification sounds play on this computer.",
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
        changed |= toggle_row(
            ui,
            "Keep window on top",
            "The mirror stays above other windows.",
            &mut self.settings.always_on_top,
        );

        if changed {
            self.settings.save();
        }

        self.render_status(ui);
    }

    fn render_pairing(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if text_button(ui, "Back").clicked() {
                    self.view_mode = ViewMode::MainCard;
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.label(RichText::new("Pair").size(18.0).strong().color(TEXT));
                });
            });
        });
        ui.add_space(8.0);
        ui.label(
            RichText::new("Developer options  ·  Wireless debugging  ·  Pair with QR code")
                .size(12.5)
                .color(TEXT_DIM),
        );

        ui.add_space(18.0);
        ui.vertical_centered(|ui| {
            if let Some(tex) = &self.qr_texture {
                let side = 184.0;
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(side), egui::Sense::hover());
                ui.painter()
                    .rect_filled(rect, CornerRadius::same(12), Color32::WHITE);
                let image_rect = rect.shrink(10.0);
                egui::Image::new((tex.id(), image_rect.size())).paint_at(ui, image_rect);
            }
        });
        ui.add_space(8.0);
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new("Same Wi-Fi network")
                    .size(12.0)
                    .color(TEXT_DIM),
            );
        });

        ui.add_space(16.0);
        hairline(ui);
        ui.add_space(10.0);

        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if text_button(ui, "New code")
                    .on_hover_text("Generate a new QR code")
                    .clicked()
                {
                    let (service, password, payload) = generate_qr_credentials();
                    self.qr_service_name = service.clone();
                    self.qr_password = password.clone();
                    self.qr_payload = payload;
                    self.rebuild_qr_texture(ctx);
                    let _ = self
                        .adb_worker
                        .action_tx
                        .send(AdbAction::RegenerateQrCredentials {
                            service_name: service,
                            password,
                        });
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    let label = if self.show_code_pairing {
                        "Hide code"
                    } else {
                        "Pairing code"
                    };
                    if text_button(ui, label).clicked() {
                        self.show_code_pairing = !self.show_code_pairing;
                    }
                });
            });
        });

        if self.show_code_pairing {
            ui.add_space(10.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.pair_address_input)
                    .hint_text("192.168.1.50:37123")
                    .desired_width(f32::INFINITY),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let pair = inline_button(ui, "Pair");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.pair_code_input)
                            .hint_text("6-digit code")
                            .desired_width(ui.available_width()),
                    );
                    if pair.clicked() {
                        self.submit_pairing();
                    }
                });
            });
        }

        self.render_status(ui);
    }
}

fn device_title(dev: &AdbDevice) -> &str {
    if dev.model.trim().is_empty() {
        dev.serial.as_str()
    } else {
        dev.model.trim()
    }
}

fn device_meta(dev: &AdbDevice, mirroring: bool) -> String {
    let state = if mirroring {
        "Mirroring"
    } else if dev.state == "device" {
        "Ready"
    } else if dev.state == "unauthorized" {
        "Unlock to allow debugging"
    } else {
        "Offline"
    };
    let connection = match dev.kind {
        ConnectionKind::Wifi => "Wi-Fi",
        ConnectionKind::Usb => "USB",
    };
    let mut parts = vec![state.to_string(), connection.to_string()];
    if let Some(level) = dev.battery_level {
        if dev.battery_charging {
            parts.push(format!("{level}% charging"));
        } else {
            parts.push(format!("{level}%"));
        }
    }
    parts.join("  ·  ")
}

fn surface<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::NONE
        .fill(ELEVATED)
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(3))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

fn hairline(ui: &mut egui::Ui) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 1.0), egui::Sense::hover());
    ui.painter()
        .hline(rect.x_range(), rect.center().y, Stroke::new(1.0, BORDER));
}

fn presence_dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.0, color);
}

fn text_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(label).size(13.0).color(TEXT))
            .frame(false)
            .corner_radius(CornerRadius::same(6)),
    )
}

fn quiet_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(label).size(12.5).color(TEXT_DIM))
            .frame(false)
            .corner_radius(CornerRadius::same(6)),
    )
}

fn primary_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add_sized(
        [ui.available_width(), 36.0],
        egui::Button::new(RichText::new(label).size(14.0).strong().color(INK))
            .fill(BUTTON_FILL)
            .corner_radius(CornerRadius::same(8)),
    )
}

fn stop_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add_sized(
        [ui.available_width(), 36.0],
        egui::Button::new(RichText::new(label).size(14.0).strong().color(TEXT))
            .fill(BG)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(8)),
    )
}

fn inline_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add_sized(
        [64.0, 28.0],
        egui::Button::new(RichText::new(label).size(13.0).strong().color(INK))
            .fill(BUTTON_FILL)
            .corner_radius(CornerRadius::same(8)),
    )
}

fn command_button(ui: &mut egui::Ui, label: &str, tip: &str) -> egui::Response {
    ui.add_sized(
        [ui.available_width(), 28.0],
        egui::Button::new(RichText::new(label).size(12.5).color(TEXT))
            .frame(false)
            .corner_radius(CornerRadius::same(6)),
    )
    .on_hover_text(tip)
}

fn segmented(ui: &mut egui::Ui, labels: &[&str], selected: usize) -> Option<usize> {
    let mut picked = None;
    surface(ui, |ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        ui.columns(labels.len(), |cols| {
            for (index, col) in cols.iter_mut().enumerate() {
                let on = index == selected;
                let color = if on { INK } else { TEXT_DIM };
                let mut button =
                    egui::Button::new(RichText::new(labels[index]).size(12.5).color(color))
                        .frame(false)
                        .corner_radius(CornerRadius::same(6));
                if on {
                    button = button.fill(BUTTON_FILL);
                }
                if col
                    .add_sized([col.available_width(), 26.0], button)
                    .clicked()
                    && !on
                {
                    picked = Some(index);
                }
            }
        });
    });
    picked
}

fn toggle_row(ui: &mut egui::Ui, title: &str, tip: &str, value: &mut bool) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ios_switch(ui, value).changed() {
                changed = true;
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add(egui::Label::new(RichText::new(title).size(13.5).color(TEXT)).truncate());
            });
        });
    })
    .response
    .on_hover_text(tip);
    changed
}

fn ios_switch(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let (rect, mut response) = ui.allocate_exact_size(Vec2::new(36.0, 20.0), egui::Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *on, "")
    });

    if ui.is_rect_visible(rect) {
        let how_on = ui.ctx().animate_bool_responsive(response.id, *on);
        let track = if *on {
            BUTTON_FILL
        } else {
            Color32::from_rgb(48, 51, 56)
        };
        let knob = if *on { INK } else { TEXT };
        let radius = rect.height() * 0.5;
        ui.painter()
            .rect_filled(rect, CornerRadius::same(radius as u8), track);
        let x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), how_on);
        ui.painter()
            .circle_filled(egui::pos2(x, rect.center().y), radius - 2.5, knob);
    }

    response
}

fn configure_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.override_text_color = Some(TEXT);
    visuals.panel_fill = BG;
    visuals.window_fill = BG;
    visuals.extreme_bg_color = ELEVATED;
    visuals.faint_bg_color = ELEVATED;
    visuals.code_bg_color = ELEVATED;
    visuals.hyperlink_color = TEXT;
    visuals.warn_fg_color = WARN;
    visuals.error_fg_color = ERROR;
    visuals.window_stroke = Stroke::NONE;
    visuals.menu_corner_radius = CornerRadius::same(8);
    visuals.selection.bg_fill = ELEVATED;
    visuals.selection.stroke = Stroke::new(1.0, TEXT);

    let hairline = Stroke::new(1.0, BORDER);
    visuals.widgets.noninteractive.bg_fill = BG;
    visuals.widgets.noninteractive.weak_bg_fill = BG;
    visuals.widgets.noninteractive.bg_stroke = hairline;
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_DIM);

    visuals.widgets.inactive.bg_fill = BG;
    visuals.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
    visuals.widgets.inactive.bg_stroke = hairline;
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    visuals.widgets.inactive.corner_radius = CornerRadius::same(8);

    visuals.widgets.hovered.bg_fill = ELEVATED;
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgba_unmultiplied(236, 237, 238, 18);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(90, 96, 102));
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    visuals.widgets.hovered.corner_radius = CornerRadius::same(8);

    visuals.widgets.active.bg_fill = BUTTON_FILL;
    visuals.widgets.active.weak_bg_fill = Color32::from_rgba_unmultiplied(236, 237, 238, 28);
    visuals.widgets.active.bg_stroke = Stroke::NONE;
    visuals.widgets.active.fg_stroke = Stroke::new(1.0, INK);
    visuals.widgets.active.corner_radius = CornerRadius::same(8);

    visuals.widgets.open.bg_fill = ELEVATED;
    visuals.widgets.open.bg_stroke = hairline;
    visuals.widgets.open.fg_stroke = Stroke::new(1.0, TEXT);
    visuals.widgets.open.corner_radius = CornerRadius::same(8);

    ctx.set_visuals(visuals);
    ctx.style_mut(|style| {
        style.spacing.item_spacing = Vec2::new(8.0, 8.0);
        style.spacing.button_padding = Vec2::new(8.0, 4.0);
        style.spacing.menu_margin = egui::Margin::same(8);
    });
}

fn main() -> eframe::Result<()> {
    let start_in_tray =
        std::env::args().any(|arg| arg == "--tray" || arg == "--startup" || arg == "--minimized");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Screenr")
            .with_inner_size([400.0, 424.0])
            .with_min_inner_size([380.0, 400.0])
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
