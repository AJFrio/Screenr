use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MirrorPreset {
    Pixel10Ultra120Hz,
    WirelessBalanced,
    BatterySaver,
    VirtualDesktopDisplay,
}

impl MirrorPreset {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Pixel10Ultra120Hz => "Pixel 10 Ultra · 120Hz",
            Self::WirelessBalanced => "Balanced · 60Hz",
            Self::BatterySaver => "Battery Saver · 60Hz",
            Self::VirtualDesktopDisplay => "Virtual Second Screen",
        }
    }

    pub fn short_badge(&self) -> &'static str {
        match self {
            Self::Pixel10Ultra120Hz => "120Hz · H.265 · Native",
            Self::WirelessBalanced => "60Hz · H.265 · 1080p",
            Self::BatterySaver => "60Hz · H.265 · 720p",
            Self::VirtualDesktopDisplay => "1920×1080 Desktop · 90Hz",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoCodec {
    H264,
    H265,
    AV1,
}

impl VideoCodec {
    pub fn as_arg(&self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::H265 => "h265",
            Self::AV1 => "av1",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioMode {
    OutputToPc,
    DuplicateOnBoth,
    Muted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CameraFacing {
    Back,
    Front,
}

impl CameraFacing {
    pub fn as_arg(&self) -> &'static str {
        match self {
            Self::Back => "back",
            Self::Front => "front",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamSettings {
    pub preset: MirrorPreset,
    pub video_codec: VideoCodec,
    pub max_size: u32,
    pub max_fps: u32,
    pub bitrate_mbps: u32,
    pub turn_screen_off: bool,
    pub stay_awake: bool,
    pub always_on_top: bool,
    pub window_borderless: bool,
    pub audio_mode: AudioMode,
    pub uhid_keyboard: bool,
    pub show_touches: bool,
    pub record_session: bool,
    pub virtual_display_res: String,
    pub camera_facing: CameraFacing,
    pub camera_size: String,
    pub camera_fps: u32,
    pub last_known_wifi_addr: Option<String>,
}

impl Default for StreamSettings {
    fn default() -> Self {
        let mut s = Self {
            preset: MirrorPreset::Pixel10Ultra120Hz,
            video_codec: VideoCodec::H265,
            max_size: 0,
            max_fps: 120,
            bitrate_mbps: 16,
            turn_screen_off: true, // Default ON: Keep physical Pixel 10 screen off while mirroring
            stay_awake: true,
            always_on_top: false,
            window_borderless: false,
            audio_mode: AudioMode::OutputToPc,
            uhid_keyboard: false,
            show_touches: false,
            record_session: false,
            virtual_display_res: "1920x1080/180".to_string(),
            camera_facing: CameraFacing::Back,
            camera_size: "1920x1080".to_string(),
            camera_fps: 60,
            last_known_wifi_addr: None,
        };
        s.apply_preset(MirrorPreset::Pixel10Ultra120Hz);
        s
    }
}

impl StreamSettings {
    pub fn apply_preset(&mut self, preset: MirrorPreset) {
        self.preset = preset;
        match preset {
            MirrorPreset::Pixel10Ultra120Hz => {
                self.video_codec = VideoCodec::H265;
                self.max_size = 0; // Native resolution
                self.max_fps = 120;
                self.bitrate_mbps = 16;
                self.turn_screen_off = true; // Keep phone screen dark by default!
                self.stay_awake = true;
            }
            MirrorPreset::WirelessBalanced => {
                self.video_codec = VideoCodec::H265;
                self.max_size = 1600;
                self.max_fps = 60;
                self.bitrate_mbps = 8;
                self.turn_screen_off = true;
                self.stay_awake = true;
            }
            MirrorPreset::BatterySaver => {
                self.video_codec = VideoCodec::H265;
                self.max_size = 1280;
                self.max_fps = 60;
                self.bitrate_mbps = 4;
                self.turn_screen_off = true;
                self.stay_awake = false;
            }
            MirrorPreset::VirtualDesktopDisplay => {
                self.video_codec = VideoCodec::H265;
                self.max_size = 0;
                self.max_fps = 90;
                self.bitrate_mbps = 16;
                self.virtual_display_res = "1920x1080/180".to_string();
                self.turn_screen_off = true;
                self.stay_awake = true;
            }
        }
    }

    pub fn config_path() -> PathBuf {
        let mut dir = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
        dir.push("screenr");
        let _ = fs::create_dir_all(&dir);
        dir.push("settings_v2.json");
        dir
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        if let Ok(data) = fs::read_to_string(path) {
            if let Ok(mut parsed) = serde_json::from_str::<Self>(&data) {
                if parsed.virtual_display_res == "1080x2400/420"
                    || parsed.virtual_display_res == "1920x1080/160"
                {
                    parsed.virtual_display_res = "1920x1080/180".to_string();
                    parsed.save();
                }
                return parsed;
            }
        }
        let default = Self::default();
        default.save();
        default
    }

    pub fn save(&self) {
        let path = Self::config_path();
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = fs::write(path, json);
        }
    }
}
