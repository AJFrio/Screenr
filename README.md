# 📱 Screenr — macOS-Style Phone Mirroring & Control Hub for Linux

**Screenr** is a fast, native Rust (`eframe`/`egui` + OpenGL) desktop and system tray app for Linux that pairs with `scrcpy v5.0.1` and `adb` to deliver a sleek **iPhone Mirroring** experience for **Pixel / Android** devices.

## ✨ Highlights

- **Ultra-Lightweight Native Rust UI**: Compiles to a single stripped native binary with hardware-accelerated OpenGL rendering and non-blocking background ADB/mDNS worker threads.
- **Native System Tray AppIndicator**: Lives quietly in your Linux top bar (`org.kde.StatusNotifierItem` via `ksni`). Click the tray icon anytime and select **📱 Connect & Mirror** to pull your phone right up onto your desktop.
- **Pixel 10 Ultra 120Hz + Screen-Off Privacy Mode**:
  - Defaults to **120Hz H.265 (HEVC)** hardware encoding at native resolution.
  - **Keep Phone Screen Off (`--turn-screen-off`)** is enabled by default so your physical phone screen stays dark while you control it on your computer.
  - **Auto-Lock on Stop**: Automatically locks your phone (`KEYCODE_SLEEP`) the moment you close the mirror window or stop mirroring.
- **Virtual Desktop Display Mode (`1920×1080`)**: Launch a widescreen 16:9 desktop-scaled (`180 DPI`) virtual second screen on your Android device while keeping the physical screen independent.
- **Focus-Aware Keyboard Shortcuts (X11)**:
  - `Ctrl + Alt` → **Home Screen** (`KEYCODE_HOME`)
  - `Ctrl + Esc` → **Close Mirroring & Lock Phone** (`KEYCODE_SLEEP`)
  - `Ctrl + Backspace` → **Back** (`KEYCODE_BACK`)
  - `Ctrl + Tab` → **App Switcher / Recents** (`KEYCODE_APP_SWITCH`)
  - *Shortcuts are dynamically grabbed only when the phone mirror window is focused, so they never interfere with other desktop apps.*
- **Zero-Cable Wireless Debugging Pairing**:
  - **Live QR Code Pairing**: Scan the QR code directly from *Settings → Developer options → Wireless debugging → Pair device with QR code*.
  - **6-Digit Pairing Code & mDNS Auto-Discovery**: Automatically detects `_adb-tls-pairing._tcp` and `_adb-tls-connect._tcp` endpoints on your LAN.
  - **1-Click USB-to-Wireless Switch**: Plug in via USB once and click **Switch to Wireless** to enable TCP/IP mode and unplug.
- **AirDrop-Style File Push & APK Install**: Drag and drop any file onto Screenr to push it straight to `/sdcard/Download/`, or drop an `.apk` to install it automatically.

## 🚀 Quick Start

```bash
git clone https://github.com/AJFrio/Screenr.git
cd Screenr
./run.sh
```

`./run.sh` automatically downloads the official standalone `scrcpy v5.0.1` Linux bundle into `./vendor/scrcpy/` (if not already present) and builds the optimized release binary.

### Start Quietly in the System Tray (Autostart)
Pass `--tray` (or `--startup`) to launch Screenr directly into the system tray without opening the control window on boot:
```bash
./run.sh --tray
```
