mod audio;
mod muteme;
mod pipewire_backend;
mod pulse;

use clap::{clap_app, ArgMatches};
use config::{Config, ConfigError, File};
use crossbeam_channel::{unbounded, Receiver, RecvError, RecvTimeoutError};
use hidapi::{HidApi, HidDevice, HidError};
use serde::{Deserialize, Serialize};
use signal_hook::{
    consts::{SIGINT, SIGTERM},
    iterator::Signals,
};
use std::{
    path::Path,
    thread,
    time::{Duration, Instant},
};

use crate::audio::{AudioBackendKind, AudioMessage, DeviceSettings};
use crate::muteme::{
    ControlMessage, DeviceEvent, ExecMessage, IntMessage, MuteMeSettings, OperationMode,
};

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
struct MainSettings {
    mute_on_startup: Option<bool>,
    backend: AudioBackendKind,
}
impl Default for MainSettings {
    fn default() -> Self {
        Self {
            mute_on_startup: None,
            backend: AudioBackendKind::default(),
        }
    }
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    main: MainSettings,
    muteme: MuteMeSettings,
    pulse: DeviceSettings,
    pipewire: DeviceSettings,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            main: MainSettings::default(),
            muteme: MuteMeSettings::default(),
            pulse: DeviceSettings::default(),
            pipewire: DeviceSettings::default(),
        }
    }
}
impl Settings {
    pub fn new(arg_matches: &ArgMatches) -> Result<Self, ConfigError> {
        let mut s = Config::default();
        let defaults = Config::try_from(&Settings::default())?;
        s.merge(defaults)?;
        let config_file = match arg_matches.value_of("config_file") {
            Some(file_name) => Some(file_name),
            None => {
                if Path::new("mutebtn.toml").is_file() {
                    Some("mutebtn.toml")
                } else if Path::new("/etc/mutebtn.toml").is_file() {
                    Some("/etc/mutebtn.toml")
                } else {
                    None
                }
            },
        };
        if let Some(file_name) = config_file {
            println!("Using configuration file {}", file_name);
            s.merge(File::with_name(file_name))?;
        }
        for settings_key in vec!["muted_color", "unmuted_color", "operation_mode"] {
            if arg_matches.occurrences_of(&settings_key) > 0 {
                let config_key = format!("muteme.{}", &settings_key);
                s.set(&config_key, arg_matches.value_of(&settings_key).unwrap())?;
            }
        }
        s.try_into()
    }
}

fn main() -> Result<(), HidError> {
    let app = clap_app!(mutebtn =>
        (version: "0.2.0")
        (author: "Matthias Erll <matthias@erll.de>")
        (about: "Connects the MuteMe Button")
        (@arg config_file: -c --config +takes_value
         "Sets a configuration file name (optional - default is ./mutebtn or /etc/mutebtn)")
        (@arg muted_color: --("muted-color") +takes_value
         default_value[red] possible_value[red green blue yellow cyan purple white nocolor]
         "Sets the color when muted")
        (@arg unmuted_color: --("unmuted-color") +takes_value
         default_value[green] possible_value[red green blue yellow cyan purple white nocolor]
         "Sets the color when not muted")
        (@arg operation_mode: -m --mode +takes_value
         default_value[toggle] possible_value[toggle pushtotalk]
         "Sets the operation mode")
    );
    let matches = app.get_matches();
    let settings;
    match Settings::new(&matches) {
        Ok(s) => settings = s,
        Err(err) => {
            println!("{}", err);
            settings = Settings::default();
        },
    }
    println!("{:?}", &settings);

    let (ctrl_sender, ctrl_receiver) = unbounded();
    let (int_sender, int_receiver) = unbounded();
    let (exec_sender, exec_receiver) = unbounded();
    let (audio_sender, audio_receiver) = unbounded();

    let backend = settings.main.backend;
    let pulse_settings = settings.pulse;
    let pipewire_settings = settings.pipewire;
    let mute_on_startup = settings.main.mute_on_startup.clone();
    let audio_ctrl_sender = ctrl_sender.clone();
    let audio_thread = thread::spawn(move || -> () {
        match backend {
            AudioBackendKind::Pulseaudio => {
                pulse::run(pulse_settings, mute_on_startup, audio_receiver, audio_ctrl_sender)
            },
            AudioBackendKind::Pipewire => pipewire_backend::run(
                pipewire_settings,
                mute_on_startup,
                audio_receiver,
                audio_ctrl_sender,
            ),
        }
    });

    let mut muteme_settings = settings.muteme;
    let ctrl_exec_sender = exec_sender.clone();
    let ctrl_audio_sender = audio_sender.clone();
    let ctrl_self_sender = ctrl_sender.clone();
    let ctrl_thread = thread::spawn(move || -> () {
        let mut terminated = false;
        let mut is_muted = false;
        let mut transition = false;
        let mut pending_audio_update = false;
        ctrl_audio_sender
            .send(AudioMessage::GetMuteStatus)
            .unwrap_or(());

        let mut last_touch: Option<Instant> = None;
        let mut second_touch = false;
        let double_tap_duration_1 =
            Duration::from_millis(muteme_settings.double_tap_duration_1.into());
        let double_tap_duration_2 =
            Duration::from_millis(muteme_settings.double_tap_duration_2.into());
        while !terminated {
            let res = ctrl_receiver.recv_timeout(Duration::from_secs(5));
            match res {
                Ok(ControlMessage::PublishMuteStatus(state)) => {
                    // State reported by the audio backend, e.g. after an external change
                    // (system tray, another app). Only reflect it in the LED - do not
                    // echo it back with SetMuteStatus, or we'd fight the user's change.
                    if state != is_muted {
                        println!("Audio server reports muted={}", state);
                        is_muted = state;
                        transition = false;
                    }
                },
                Ok(ControlMessage::SetColor(mute_state, color)) => {
                    if mute_state {
                        muteme_settings.muted_color = color;
                    } else {
                        muteme_settings.unmuted_color = color;
                    }
                    transition = false;
                },
                Ok(ControlMessage::SetMode(new_mode)) => {
                    muteme_settings.operation_mode = new_mode;
                    is_muted = true;
                    pending_audio_update = true;
                    transition = false;
                },
                Ok(ControlMessage::Event(event)) => {
                    let new_state;
                    match event {
                        DeviceEvent::Touch => {
                            println!("Touch event");
                            match muteme_settings.operation_mode {
                                OperationMode::PushToTalk => new_state = false,
                                OperationMode::Toggle => new_state = is_muted,
                                OperationMode::Hybrid => {
                                    if is_muted {
                                        new_state = false;
                                    } else {
                                        new_state = is_muted;
                                    }
                                    match last_touch {
                                        Some(t) => {
                                            let duration = Instant::now().duration_since(t);
                                            println!(
                                                "Intitial - Duration since last touch: {:?}",
                                                duration
                                            );
                                            second_touch = duration < double_tap_duration_1;
                                        },
                                        None => {
                                            second_touch = false;
                                        },
                                    }
                                    last_touch = Some(Instant::now());
                                },
                            }
                        },
                        DeviceEvent::Release => {
                            println!("Release event");
                            match muteme_settings.operation_mode {
                                OperationMode::PushToTalk => new_state = true,
                                OperationMode::Toggle => new_state = !is_muted,
                                OperationMode::Hybrid => {
                                    if second_touch {
                                        match last_touch {
                                            Some(t) => {
                                                let duration = Instant::now().duration_since(t);
                                                println!("Release on 2nd touch - Duration since last touch: {:?}", duration);
                                                if duration < double_tap_duration_2 {
                                                    new_state = false;
                                                } else {
                                                    new_state = true;
                                                    second_touch = false;
                                                }
                                            },
                                            None => {
                                                new_state = true;
                                            },
                                        }
                                    } else {
                                        new_state = true;
                                    }
                                },
                            }
                        },
                    };
                    if is_muted != new_state {
                        is_muted = new_state;
                        pending_audio_update = true;
                        transition = false;
                    }
                },
                Ok(ControlMessage::Continue) => {},
                Ok(ControlMessage::Terminate) => terminated = true,
                Err(RecvTimeoutError::Timeout) => {
                    println!("Sending keepalive");
                    transition = false;
                },
                Err(RecvTimeoutError::Disconnected) => terminated = true,
            }

            if pending_audio_update {
                ctrl_audio_sender
                    .send(AudioMessage::SetMuteStatus(is_muted))
                    .unwrap_or(());
                pending_audio_update = false;
            }

            let current_color = if is_muted {
                &muteme_settings.muted_color
            } else {
                &muteme_settings.unmuted_color
            };
            let effect: u8;
            if transition {
                effect = 0x40;
                transition = false;
            } else {
                effect = 0x00;
                let sub_thread_sender = ctrl_self_sender.clone();
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(100));
                    sub_thread_sender
                        .send(ControlMessage::Continue)
                        .unwrap_or(());
                });
                transition = true;
            }
            let color_value = current_color.get_byte_value() + effect;
            ctrl_exec_sender
                .send(ExecMessage::SetReport(color_value))
                .unwrap_or(());
        }
    });
    let int_exec_sender = exec_sender.clone();
    let int_thread = thread::spawn(move || {
        let mut terminated = false;
        while !terminated {
            int_exec_sender
                .send(ExecMessage::ReadInterrupt)
                .unwrap_or(());
            let res = int_receiver.recv_timeout(Duration::from_millis(50));
            match res {
                Ok(IntMessage::Terminate) => terminated = true,
                Err(RecvTimeoutError::Disconnected) => terminated = true,
                Err(RecvTimeoutError::Timeout) => continue,
            }
        }
    });
    let exec_ctrl_sender = ctrl_sender.clone();
    let exec_thread = thread::spawn(move || {
        let api = hidapi::HidApi::new().expect("Failed to initialise hidapi");
        let mut terminated = false;

        while !terminated {
            // The device may be absent at startup or disappear at any time (e.g. when it
            // sits behind a KVM switch), so never give up: keep retrying the open, and
            // fall back here whenever a read or write fails.
            let device = match open_device(&api) {
                Some(device) => device,
                None => {
                    terminated = wait_for_retry(&exec_receiver);
                    continue;
                },
            };
            println!("Connected to MuteMe device");
            // Prompt the control thread to push the current LED state to the fresh device.
            exec_ctrl_sender.send(ControlMessage::Continue).unwrap_or(());

            let mut connected = true;
            let mut state = 0;
            while !terminated && connected {
                loop {
                    let data = read_interrupt(&device);
                    match data {
                        Ok(Some(new_state @ 1..=2)) if state != new_state => {
                            state = new_state;
                            if state == 1 {
                                exec_ctrl_sender
                                    .send(ControlMessage::Event(DeviceEvent::Touch))
                                    .unwrap_or(());
                            } else {
                                exec_ctrl_sender
                                    .send(ControlMessage::Event(DeviceEvent::Release))
                                    .unwrap_or(());
                            }
                        },
                        Ok(Some(_)) => {},
                        Ok(None) => break,
                        Err(()) => {
                            connected = false;
                            break;
                        },
                    }
                    thread::yield_now();
                }
                if !connected {
                    break;
                }

                let res = exec_receiver.recv();
                match res {
                    Ok(ExecMessage::SetReport(value)) => connected = write_value(&device, value),
                    Ok(ExecMessage::ReadInterrupt) => continue,
                    Ok(ExecMessage::Terminate) => terminated = true,
                    Err(RecvError) => terminated = true,
                }
            }
            if !terminated {
                println!("MuteMe device disconnected");
            }
        }
    });

    let mut signals = Signals::new(&[SIGINT, SIGTERM]).unwrap();
    let handle = signals.handle();
    thread::spawn(move || {
        for sig in signals.forever() {
            println!("Received signal {:?}", sig);
            int_sender.send(IntMessage::Terminate).unwrap_or(());
            ctrl_sender.send(ControlMessage::Terminate).unwrap_or(());
            exec_sender.send(ExecMessage::Terminate).unwrap_or(());
            audio_sender.send(AudioMessage::Terminate).unwrap_or(());
        }
    });

    int_thread.join().unwrap();
    ctrl_thread.join().unwrap();
    exec_thread.join().unwrap();
    audio_thread.join().unwrap();
    handle.close();

    Ok(())
}

/// How long to wait between attempts to open the device while it is absent.
const RECONNECT_INTERVAL: Duration = Duration::from_secs(2);

fn open_device(api: &HidApi) -> Option<HidDevice> {
    let device = api.open(muteme::DEVICE_VID, muteme::DEVICE_PID).ok()?;
    match device.set_blocking_mode(false) {
        Ok(()) => Some(device),
        Err(err) => {
            println!("Failed to set device to non-blocking mode: {}", err);
            None
        },
    }
}

/// Sleeps for RECONNECT_INTERVAL while draining (and discarding) queued messages, so the
/// other threads don't pile up work while the device is absent. Returns true if the
/// thread should terminate.
fn wait_for_retry(receiver: &Receiver<ExecMessage>) -> bool {
    let deadline = Instant::now() + RECONNECT_INTERVAL;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        match receiver.recv_timeout(deadline - now) {
            Ok(ExecMessage::Terminate) | Err(RecvTimeoutError::Disconnected) => return true,
            Ok(_) => {},
            Err(RecvTimeoutError::Timeout) => return false,
        }
    }
}

/// Writes a report to the device. Returns false if the device appears to be gone.
fn write_value(device: &HidDevice, value: u8) -> bool {
    let data = [0x00, value];
    let mut attempts = 3u8;
    loop {
        attempts -= 1;
        let res = device.write(&data);
        match res {
            Ok(i) => {
                println!("Wrote {} bytes", i);
                return true;
            },
            Err(err) => println!("{}", err),
        };
        if attempts > 0 {
            thread::sleep(Duration::from_millis(10));
        } else {
            return false;
        }
    }
}

/// Reads one interrupt report. Ok(None) means nothing pending; Err(()) means the device
/// appears to be gone.
fn read_interrupt(device: &HidDevice) -> Result<Option<u8>, ()> {
    let mut buf = [0u8; 8];
    let mut attempts = 3u8;
    loop {
        attempts -= 1;
        let res = device.read(&mut buf);
        match res {
            Ok(_i @ 0) => return Ok(None),
            Ok(_) => return Ok(Some(buf[3])),
            Err(err) => {
                println!("{}", err);
            },
        }
        if attempts > 0 {
            thread::sleep(Duration::from_millis(10));
        } else {
            return Err(());
        }
    }
}
