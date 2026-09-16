use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub enum AudioMessage {
    GetMuteStatus,
    SetMuteStatus(bool),
    Terminate,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MuteDeviceSelector {
    All,
    Default,
    Selected,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceSettings {
    pub mute_device: MuteDeviceSelector,
    pub unmute_device: Option<MuteDeviceSelector>,
    pub selected_device_name: String,
}
impl Default for DeviceSettings {
    fn default() -> Self {
        Self {
            mute_device: MuteDeviceSelector::All,
            unmute_device: Some(MuteDeviceSelector::All),
            selected_device_name: String::from(""),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum AudioBackendKind {
    Pulseaudio,
    Pipewire,
}
impl Default for AudioBackendKind {
    fn default() -> Self {
        AudioBackendKind::Pipewire
    }
}
