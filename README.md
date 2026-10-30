<img src="assets/logo.svg" width="96" alt="CC2 Monitor logo">

# CC2 Monitor

A native desktop monitor for the Elegoo Centauri Carbon 2, laid out like the
device tab in Elegoo Slicer: a live camera, the current job, temperatures, fans,
speed and light, Canvas spools, motion controls, print history, files and a
protocol log. It is a single executable with no server, browser or runtime.

![CC2 Monitor showing a print in progress: live camera, current job, temperatures, fans, Canvas spools and motion controls](docs/screenshot.png)

## Download

Download the Windows x64 ZIP from this fork's
[v1.0.1-beta.1 release](https://github.com/Sceletor/cc2-monitor/releases/tag/v1.0.1-beta.1).
Extract it and run `cc2-monitor.exe`. Checksums and instructions are included.

For the Windows x64 build of this fork, see the
[Actions ZIP download instructions](docs/WINDOWS-DOWNLOAD.md).
The existing PR workflow tests and builds the executable in one cancellable
run per PR update, with artifacts retained for 14 days and no scheduled runs.

### Upstream release

Grab the Windows build from the
[latest release](https://github.com/AlvaroEPena/cc2-monitor/releases/latest):
unzip it and run `CC2 Monitor.exe`. Nothing needs installing. Settings are
stored in `%APPDATA%\cc2-monitor`.

Windows may warn that the app is from an unknown publisher, because the
executable is not code-signed. Choose **More info → Run anyway**.

## Build

```bash
cargo build --release --locked
```

The binary ends up at `target/release/cc2-monitor(.exe)`.

### Linux (planned)

The code is cross-platform, but nothing has been built or tested on Linux yet.
For a Linux build on Debian/Ubuntu, install the windowing and GL libraries first:

```bash
sudo apt install libxkbcommon-dev libwayland-dev libxcursor-dev libxrandr-dev libxi-dev libgl1-mesa-dev
```

## Using it

Open **⚙ Printer**, then either press **Scan network** or type the printer's IP.
The access code is `123456` unless you set one on the printer. The serial number
is found automatically.

The printer accepts at most **two** monitoring clients at once, and Elegoo
Slicer's device tab counts as one. If the app shows *Refused*, close another
client. The app retries every 30 seconds.

The **⚙** button on the camera toolbar sets the frame rate and resolution, and
changes apply live. Frame rate is what saves CPU, because each frame redraws the
window. The stock camera is only 640×360, so lowering the resolution saves very
little. Measured on a live CC2:

| Camera setting | CPU (one core) |
|---|---|
| Max (~20 fps) | ~10% |
| 10 fps (default) | ~5% |
| 5 fps | ~3% |
| Camera off | ~3–7% |

For development, set `CC2_IP=192.168.1.50` to connect at launch without the dialog.

## Sending prints

There are three ways to open the print dialog:

- **📤 Send file** in the header opens a file picker.
- **Drag a `.gcode` file** onto the window.
- **▶ Print** on any file in the Files tab, for files already on the printer or USB drive.

A local file is uploaded first, with a progress bar. The dialog then reads the
file's details from the printer and matches each color in the file to a
Canvas slot: same material first, then the nearest color. You can change any
slot. A warning appears when the material or color doesn't match. Options are
the plate side (textured A or smooth B; the last one you used is remembered),
auto bed leveling (on by default), timelapse, and Canvas auto refill. Auto
refill is a printer setting, so a change stays in effect after the print.

**Start print** only works while the printer is idle. It is disabled while the
printer is printing, so the dialog can't interrupt a running job.

The slot map always covers every color in the file. The printer silently falls
back to slot 1 for any color left out, so a partial map is never sent.

## Timelapses

The **History** tab lists every print with its timelapse:

- **⬇ Download** saves a finished timelapse MP4 over the network, so no USB
  drive is needed.
- **Render video** appears for prints where the printer kept the frames but
  never made the video. The printer builds the MP4 (about half a minute, idle
  printer only), then the row switches to Download. Each print can be rendered
  once, because the printer deletes the frames afterwards.

Timelapse recording can only be turned on when a print starts; the printer has
no command to start one mid-print. Downloads may not work while the printer is
printing, so if one fails, try again when it's idle.

## How it talks to the printer

| What | How |
|---|---|
| Discovery | UDP broadcast `{"method":7000}` to port 52700 |
| Status and commands | MQTT on port 1883, user `elegoo`, password = access code |
| Camera | MJPEG over HTTP on port 8080 |
| Upload | `PUT :80/upload` in 1 MB chunks with `Content-Range`, `X-File-Name`, `X-File-MD5`, `X-Token` |
| Start print | MQTT method 1020 with a Canvas `slot_map` |
| Timelapses | history from method 1036, render with method 1051, `GET :80/download?file_name=video/…` |

The protocol is documented by
[elegoo-homeassistant](https://github.com/danielcherubini/elegoo-homeassistant/blob/main/docs/CC2_PROTOCOL.md)
and [elegoo-web](https://github.com/runnane/elegoo-web).

## Layout

```
src/
  main.rs            window setup
  app.rs             app state; routes printer and camera events to the UI
  config.rs          persisted settings
  print_job.rs       print dialog state and Canvas slot matching
  camera.rs          MJPEG reader and decoder (background thread)
  ai.rs              FrameAnalyzer trait: hook for future failure detection
  printer/
    client.rs        MQTT session: register, heartbeat, reconnect (background threads)
    state.rs         status delta merging and typed accessors
    discovery.rs     UDP discovery
    upload.rs        chunked G-code upload
    download.rs      timelapse download
    codes.rs         status, exception and error code names
  ui/                panels (header, camera + job, controls, tabs, dialogs, theme)
assets/              logo (SVG), small-size logo, .ico and PNG icons
build.rs             embeds the icon and version info into the Windows .exe
```

## AI detection

Not implemented. `src/ai.rs` defines a `FrameAnalyzer` trait, and the camera
thread already calls it on every frame when it is enabled. To add detection,
implement the trait and pass it to `CameraStream::start` in place of
`ai::Disabled`.

## License

MIT. See [LICENSE](LICENSE).
