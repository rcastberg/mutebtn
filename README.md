# MuteBtn

Connects mute controllers such as MuteMe™ to apps

## Supported environments

Currently the only supported device is [MuteMe™](https://muteme.com/), because this is the only one I have available for testing. However, there is no reason why this should remain the only supported device. Even DIY devices could be added.

This app was developed on and for Linux. It supports PulseAudio and, natively, PipeWire (talking directly to `libpipewire`, not through PipeWire's PulseAudio-compatibility layer). It is written in Rust, so it should be possible to adapt it to any environment. More audio servers will be added.

## Why?

The vendor-provided app of MuteMe™ worked for me, but instead of the heavy closed-source Electron app I wanted something lighter with the possibility to run as a system service.

Also with the closed-source app my confidence that Linux features will be developed much further is quite low. Especially Linux-specific FOSS apps will likely not get much more support.

# Additional features

Besides the vendor-provided app features (color setting, push-to-talk or toggle mode), the following is supported:
* Native PipeWire backend (default): talks directly to `libpipewire`, reacting immediately to mute-state changes made elsewhere (system tray, `wpctl`, another app) instead of polling. The app only ever pushes a mute change to the audio server in response to an actual button press (or `mute_on_startup`) - it never re-asserts its own cached state, so muting/unmuting a device outside the app sticks.
* PulseAudio backend: works against a real PulseAudio server, or PipeWire's PulseAudio-compatibility layer. Since there's no push notification for external changes on this path, it polls a few times a second to detect them.
* Selecting the audio device: Select a specific audio-device or the selected default device separately for mute and unmute. The default is to mute/unmute all sources.
* Hybrid mode: If you prefer push-to-talk, but sometimes get tired of holding the button, you can double-tap, and it will leave the mic open until you touch once again, similar to toggle mode.

# Missing features

* There is no GUI yet.
* Currently settings cannot be changed at run-time. The app has to be restarted. This will change soon.

# Configuration

The app will look for configuration files in the following places:
* Any command line option provided with `-c <config_file>` or `--config <config_file>`.
* `mutebtn.toml` in the current working directory.
* `/etc/mutebtn.toml`

Format: Several formats such as JSON, YAML etc are supported, but TOML is the recommended option.
If any entry in the configuration file is invalid, all defaults apply.

Example with all options:

```toml
[main]
# Optional. If set to true, mutes selected devices on app start; if set to false, unmutes
# selected devices on app start. If not present, does nothing (default).
mute_on_startup = true

# Which audio backend to use. Valid choices are "pipewire" (default, talks to PipeWire
# natively) and "pulseaudio" (works against a real PulseAudio server, or PipeWire's
# PulseAudio-compatibility layer).
backend = "pipewire"

[muteme]
# Color when muted (default: red) or unmuted (default: green).
# Valid choices are "red", "green", "blue", "yelllow", "cyan", "purple", "white", and "nocolor".
muted_color = "red"
unmuted_color = "green"

# Operation mode. Valid choices are "toggle" (default), "pushtotalk", and "hybrid".
operation_mode = "hybrid"

# Only applies to "hybrid" mode: Maximum duration in milliseconds to detect a double-tap
# (1), and the following release (2). Defaults to values below:
double_tap_duration_1 = 300
double_tap_duration_2 = 250

# [pipewire] settings are used when backend = "pipewire" (the default); [pulse]
# settings are used when backend = "pulseaudio". Both sections have the same shape.
[pipewire]
# Device to mute. Choices are "all" (default setting), "default", and "selected". On
# "default", the current default audio source is re-detected on each mute/unmute operation.
mute_device = "all"
# Optional, separate selection of which device to unmute. Choices are the same as for
# mute_device; if not set unmutes the same as in mute_device. This example shows that you
# can always mute all audio sources, and only unmute the default device on-demand.
unmute_device = "default"

# Only applies if mute_device or unmute_device is set to "selected": Defines the specific
# device name. Available names can e.g. be listed using "wpctl status" (look under Sources).
selected_device_name = "my_device"

[pulse]
# Same options as [pipewire] above. Available device names can e.g. be listed using
# "pactl list sources".
mute_device = "all"
unmute_device = "default"
selected_device_name = "my_device"
```

## Running as a service (autostart)

A sample systemd **user** unit is provided at `contrib/systemd/mutebtn.service`. It's a user unit
rather than a system one because PipeWire/PulseAudio and WirePlumber are themselves user-session
services - mutebtn needs to run alongside them, not before or outside the session.

To install it:

```sh
cargo build --release
sudo install -m 755 target/release/mutebtn /usr/local/bin/mutebtn
mkdir -p ~/.config/systemd/user
cp contrib/systemd/mutebtn.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now mutebtn.service
```

Logs are available via `journalctl --user -u mutebtn.service -f`. The device also needs a udev
rule granting your user access to it (e.g. `KERNEL=="hidraw*", ATTRS{idVendor}=="20a0", ATTRS{idProduct}=="42da", MODE="0666"`
in a file under `/etc/udev/rules.d/`), or the service will fail to open the USB device.

## Development plans

Next planned steps in development are:
* Provide some sort of interface to change settings comfortably at run-time.
* Support more apps (e.g. Mumble)

Contributions welcome, also for more devices!

## Disclaimer

Note that this app is not associated with or endorsed by MuteMe™ or any other vendor.
