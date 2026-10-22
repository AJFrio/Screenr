use ksni::{menu::StandardItem, Icon, MenuItem, Tray, TrayService};
use std::sync::mpsc::{self, Receiver, Sender};

#[derive(Debug, Clone)]
pub enum TrayCommand {
    ConnectAndMirror,
    StopMirroring,
    ToggleWindow,
    SwitchUsbToWireless,
    TakeScreenshot,
    QuitApp,
}

#[derive(Clone)]
pub struct TrayState {
    pub device_label: Option<String>,
    pub is_usb: bool,
    pub is_mirroring: bool,
    pub turn_screen_off: bool,
    pub tx: Sender<TrayCommand>,
    pub ctx: eframe::egui::Context,
}

impl Tray for TrayState {
    fn id(&self) -> String {
        "screenr-pixel-hub".into()
    }

    fn title(&self) -> String {
        match &self.device_label {
            Some(dev) if self.is_mirroring => format!("Screenr — Mirroring {}", dev),
            Some(dev) => format!("Screenr — {} Ready", dev),
            None => "Screenr — Pixel Mirror".into(),
        }
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        vec![build_phone_tray_icon(
            self.device_label.is_some(),
            self.is_mirroring,
        )]
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.send(TrayCommand::ConnectAndMirror);
        self.ctx.request_repaint();
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items: Vec<MenuItem<Self>> = Vec::new();

        // Primary 1-Click Action at the very top of the tray menu!
        if self.is_mirroring {
            let label = match &self.device_label {
                Some(d) => format!("⏹  Stop Mirroring ({})", d),
                None => "⏹  Stop Mirroring".to_string(),
            };
            items.push(
                StandardItem {
                    label,
                    activate: Box::new(|this: &mut Self| {
                        let _ = this.tx.send(TrayCommand::StopMirroring);
                        this.ctx.request_repaint();
                    }),
                    ..Default::default()
                }
                .into(),
            );
        } else {
            let label = match &self.device_label {
                Some(d) => format!("📱  Connect & Mirror {}", d),
                None => "📱  Connect & Pull Up Phone".to_string(),
            };
            items.push(
                StandardItem {
                    label,
                    activate: Box::new(|this: &mut Self| {
                        let _ = this.tx.send(TrayCommand::ConnectAndMirror);
                        this.ctx.request_repaint();
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }

        items.push(MenuItem::Separator);

        if self.device_label.is_some() {
            if self.is_usb {
                items.push(
                    StandardItem {
                        label: "📶  Switch USB to Wireless".into(),
                        activate: Box::new(|this: &mut Self| {
                            let _ = this.tx.send(TrayCommand::SwitchUsbToWireless);
                            this.ctx.request_repaint();
                        }),
                        ..Default::default()
                    }
                    .into(),
                );
            }

            items.push(
                StandardItem {
                    label: "📸  Take Phone Screenshot".into(),
                    activate: Box::new(|this: &mut Self| {
                        let _ = this.tx.send(TrayCommand::TakeScreenshot);
                        this.ctx.request_repaint();
                    }),
                    ..Default::default()
                }
                .into(),
            );

            items.push(MenuItem::Separator);
        }

        items.push(
            StandardItem {
                label: "🎛  Open Screenr Controls".into(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.tx.send(TrayCommand::ToggleWindow);
                    this.ctx.request_repaint();
                }),
                ..Default::default()
            }
            .into(),
        );

        items.push(
            StandardItem {
                label: "✕  Quit Screenr".into(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.tx.send(TrayCommand::QuitApp);
                    this.ctx.request_repaint();
                }),
                ..Default::default()
            }
            .into(),
        );

        items
    }
}

pub struct TrayController {
    pub handle: ksni::Handle<TrayState>,
    pub rx: Receiver<TrayCommand>,
}

impl TrayController {
    pub fn spawn(ctx: eframe::egui::Context) -> Self {
        let (tx, rx) = mpsc::channel();
        let initial = TrayState {
            device_label: None,
            is_usb: false,
            is_mirroring: false,
            turn_screen_off: true,
            tx,
            ctx,
        };
        let service = TrayService::new(initial);
        let handle = service.handle();
        service.spawn();
        Self { handle, rx }
    }

    pub fn update_status(
        &self,
        device_label: Option<String>,
        is_usb: bool,
        is_mirroring: bool,
        turn_screen_off: bool,
    ) {
        self.handle.update(|state: &mut TrayState| {
            state.device_label = device_label;
            state.is_usb = is_usb;
            state.is_mirroring = is_mirroring;
            state.turn_screen_off = turn_screen_off;
        });
    }
}

/// Generates a clean 24x24 ARGB32 smartphone icon for the system tray
fn build_phone_tray_icon(connected: bool, mirroring: bool) -> Icon {
    let w = 24;
    let h = 24;
    let mut data = vec![0u8; w * h * 4];

    let (screen_r, screen_g, screen_b) = if mirroring {
        (52u8, 211u8, 153u8) // Emerald green when actively mirroring
    } else if connected {
        (59u8, 130u8, 246u8) // Pixel Blue when phone is connected & ready
    } else {
        (100u8, 116u8, 139u8) // Subtle slate when waiting
    };

    for y in 0..h {
        for x in 0..w {
            let idx = (y * w + x) * 4;
            // Phone outer body: x in 6..18, y in 2..22
            if (6..18).contains(&x) && (2..22).contains(&y) {
                // Rounded corners check
                let corner = (x == 6 && y == 2)
                    || (x == 17 && y == 2)
                    || (x == 6 && y == 21)
                    || (x == 17 && y == 21);
                if corner {
                    continue;
                }

                let is_border = x == 6 || x == 17 || y == 2 || y == 21 || y == 3 || y == 20;
                let is_camera_dot = y == 5 && (11..=12).contains(&x);
                let is_home_bar = y == 19 && (10..=13).contains(&x);

                if is_border || is_home_bar {
                    // Crisp white/silver frame (ARGB network byte order)
                    data[idx] = 255;
                    data[idx + 1] = 241;
                    data[idx + 2] = 245;
                    data[idx + 3] = 249;
                } else if is_camera_dot {
                    data[idx] = 255;
                    data[idx + 1] = 15;
                    data[idx + 2] = 23;
                    data[idx + 3] = 42;
                } else {
                    // Inner screen glow
                    data[idx] = 240;
                    data[idx + 1] = screen_r;
                    data[idx + 2] = screen_g;
                    data[idx + 3] = screen_b;
                }
            }
        }
    }

    Icon {
        width: w as i32,
        height: h as i32,
        data,
    }
}
