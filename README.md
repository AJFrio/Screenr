# Screenr

Native Linux app for mirroring and controlling an Android phone. Built with Rust (`eframe` / `egui`) on top of `scrcpy` and `adb`.

## Features

- Single native binary, OpenGL rendering, and background ADB / mDNS workers
- System tray. Connect and mirror from the tray menu
- 120Hz H.265 at native resolution by default
- Phone screen stays off while mirroring, and locks when mirroring stops
- Optional 1920x1080 virtual display
- Shortcuts apply only while the mirror window is focused:
  - `Ctrl+Alt` home
  - `Ctrl+Esc` stop mirroring and lock
  - `Ctrl+Backspace` back
  - `Ctrl+Tab` recents
- Wireless debugging via QR code, 6-digit code, or mDNS, plus a USB-to-wireless switch
- Drag a file onto the window to copy it to `/sdcard/Download/`, or drop an `.apk` to install it

## Quick start

```bash
git clone https://github.com/AJFrio/Screenr.git
cd Screenr
./run.sh
```

`./run.sh` downloads scrcpy v5.0.1 into `./vendor/scrcpy/` if needed and builds the release binary.

Start in the tray without opening the window:

```bash
./run.sh --tray
```
