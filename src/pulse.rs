use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use pulsectl::controllers::{DeviceControl, SourceController};
use std::time::Duration;

use crate::audio::{AudioMessage, MuteDeviceSelector};
use crate::muteme::ControlMessage;

pub use crate::audio::DeviceSettings as PulseSettings;

pub trait Mute {
    fn is_muted(&mut self) -> bool;
    fn set_muted(&mut self, muted: bool) -> ();
}
pub struct PulseControl {
    handler: SourceController,
    settings: PulseSettings,
}

impl PulseControl {
    pub fn new(settings: PulseSettings) -> Self {
        let handler = SourceController::create().expect("Failed to get handler");
        Self { handler, settings }
    }
}
impl Mute for PulseControl {
    fn is_muted(&mut self) -> bool {
        let device = match &self.settings.unmute_device {
            Some(dev) => dev,
            None => &self.settings.mute_device,
        };
        match device {
            MuteDeviceSelector::All => {
                let devices_res = &self.handler.list_devices();
                match devices_res {
                    Ok(devices) => {
                        for dev in devices {
                            if !dev.mute {
                                return false;
                            }
                        }
                    },
                    Err(_) => {
                        println!("Could not get list of recording devices");
                        return false;
                    },
                }
                true
            },
            MuteDeviceSelector::Default => match self.handler.get_server_info() {
                Ok(server_info) => match server_info.default_source_name {
                    Some(device_name) => {
                        return match &self.handler.get_device_by_name(&device_name) {
                            Ok(dev) => dev.mute,
                            Err(_) => {
                                println!("Failed to find device with default source name");
                                false
                            },
                        };
                    },
                    None => {
                        println!("No default device selected");
                        false
                    },
                },
                Err(_) => {
                    println!("Failed to get server info");
                    false
                },
            },
            MuteDeviceSelector::Selected => {
                return match &self
                    .handler
                    .get_device_by_name(&self.settings.selected_device_name)
                {
                    Ok(dev) => dev.mute,
                    Err(_) => {
                        println!("Failed to find device with selected source name");
                        false
                    },
                };
            },
        }
    }

    fn set_muted(&mut self, muted: bool) -> () {
        let device;
        if muted {
            device = &self.settings.mute_device;
        } else {
            device = match &self.settings.unmute_device {
                Some(dev) => dev,
                None => &self.settings.mute_device,
            };
        }
        match device {
            MuteDeviceSelector::All => {
                let devices_res = &self.handler.list_devices();
                match devices_res {
                    Ok(devices) => {
                        for dev in devices {
                            let _ = self.handler.set_device_mute_by_index(dev.index, muted);
                        }
                    },
                    Err(_) => {
                        println!("Could not get list of recording devices")
                    },
                }
            },
            MuteDeviceSelector::Default => match self.handler.get_server_info() {
                Ok(server_info) => match server_info.default_source_name {
                    Some(device_name) => {
                        let _ = self.handler.set_device_mute_by_name(&device_name, muted);
                    },
                    None => {
                        println!("No default device selected");
                    },
                },
                Err(_) => {
                    println!("Failed to get server info");
                },
            },
            MuteDeviceSelector::Selected => {
                let _ = self
                    .handler
                    .set_device_mute_by_name(&self.settings.selected_device_name, muted);
            },
        }
    }
}

/// Runs the PulseAudio backend thread. PulseAudio has no push-based mute change
/// notifications available here, so this polls for changes made outside this app
/// (e.g. system tray, another app) and reports them upstream, instead of only
/// reacting to explicit requests - and without ever re-asserting a stale cached
/// state onto the server.
pub fn run(
    settings: PulseSettings,
    mute_on_startup: Option<bool>,
    audio_receiver: Receiver<AudioMessage>,
    audio_ctrl_sender: Sender<ControlMessage>,
) {
    let mut terminated = false;
    let mut pulse_control = PulseControl::new(settings);
    let mut last_known_muted = None;
    if let Some(muted) = mute_on_startup {
        pulse_control.set_muted(muted);
        last_known_muted = Some(muted);
    }
    while !terminated {
        let res = audio_receiver.recv_timeout(Duration::from_millis(300));
        match res {
            Ok(AudioMessage::GetMuteStatus) => {
                let is_muted = pulse_control.is_muted();
                last_known_muted = Some(is_muted);
                audio_ctrl_sender
                    .send(ControlMessage::PublishMuteStatus(is_muted))
                    .unwrap_or(());
            },
            Ok(AudioMessage::SetMuteStatus(new_state)) => {
                pulse_control.set_muted(new_state);
                last_known_muted = Some(new_state);
            },
            Ok(AudioMessage::Terminate) => terminated = true,
            Err(RecvTimeoutError::Timeout) => {
                let is_muted = pulse_control.is_muted();
                if last_known_muted != Some(is_muted) {
                    last_known_muted = Some(is_muted);
                    audio_ctrl_sender
                        .send(ControlMessage::PublishMuteStatus(is_muted))
                        .unwrap_or(());
                }
            },
            Err(RecvTimeoutError::Disconnected) => terminated = true,
        }
    }
}
