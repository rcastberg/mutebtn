use crossbeam_channel::{Receiver, Sender};
use pipewire as pw;
use pw::{
    device::Device,
    metadata::Metadata,
    node::Node,
    proxy::{Listener, ProxyT},
    spa,
    types::ObjectType,
};
use spa::param::ParamType;
use spa::pod::{
    deserialize::PodDeserializer, serialize::PodSerializer, Object, Pod, Property, Value,
};
use spa::utils::SpaTypes;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Cursor;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use crate::audio::{AudioMessage, DeviceSettings, MuteDeviceSelector};
use crate::muteme::ControlMessage;

pub use crate::audio::DeviceSettings as PipewireSettings;

/// How long a requested mute change is retried/trusted before falling back to
/// whatever the server actually reports (see `State::effective_mute` and
/// `State::retry_pending`).
const CONVERGE_WINDOW: Duration = Duration::from_secs(5);

/// Commands routed into the dedicated PipeWire mainloop thread.
enum Command {
    SetMuteStatus(bool),
    GetMuteStatus,
    Terminate,
}

struct SourceNode {
    name: String,
    // Software mute state, straight from this node's own Props. Authoritative
    // only for nodes with no hardware route (see `DeviceEntry`/`RouteState`
    // below) - many real capture devices instead mute via a hardware route on
    // their parent Device, which WirePlumber treats as the source of truth and
    // does not mirror into this node's Props.
    muted: Option<bool>,
    node: Node,
    // Kept alive only for its Drop impl (unregisters the listener); never read.
    _listener: pw::node::NodeListener,
    // Parent Device global id and the route's "device" sub-index
    // (`card.profile.device`), if this node is backed by a hardware route.
    device_id: Option<u32>,
    route_device: Option<i32>,
    // The last mute state explicitly requested for this node (via the button,
    // mute_on_startup, etc.) and when that request was made. Some hardware
    // routes silently drop a mute write made before the device has fully
    // settled after being (re)discovered, so requests are retried against the
    // node/route until an incoming event confirms they took effect, or this
    // deadline passes.
    desired: Option<(bool, Instant)>,
}

struct RouteState {
    route_index: i32,
    muted: Option<bool>,
}

struct DeviceEntry {
    device: Device,
    // Keyed by route "device" sub-index (see `SourceNode::route_device`).
    routes: HashMap<i32, RouteState>,
    // Kept alive only for its Drop impl; never read.
    _listener: pw::device::DeviceListener,
}

struct State {
    settings: DeviceSettings,
    ctrl_sender: Sender<ControlMessage>,
    nodes: HashMap<u32, SourceNode>,
    devices: HashMap<u32, DeviceEntry>,
    default_source_name: Option<String>,
    last_published: Option<bool>,
    mute_on_startup: Option<bool>,
    // Source nodes (and their routes) are discovered asynchronously and don't
    // all appear at once (e.g. a USB mic can take noticeably longer to
    // enumerate than a built-in one), so mute_on_startup is (re-)applied
    // whenever new information arrives until this deadline, instead of a
    // single fixed-delay attempt that could race a slow-to-appear device.
    startup_deadline: Instant,
}

impl State {
    /// The mute state that actually determines whether this node is audible.
    /// Prefers the hardware route's mute (what WirePlumber and tools like
    /// wpctl/pavucontrol treat as authoritative for hardware-mute-capable
    /// devices) and falls back to the node's own software Props.mute only when
    /// there's no (yet) known route for it.
    fn observed_mute(&self, node: &SourceNode) -> Option<bool> {
        if let (Some(device_id), Some(route_device)) = (node.device_id, node.route_device) {
            if let Some(route_muted) = self
                .devices
                .get(&device_id)
                .and_then(|dev| dev.routes.get(&route_device))
                .and_then(|route| route.muted)
            {
                return Some(route_muted);
            }
        }
        node.muted
    }

    /// Like `observed_mute`, but while a request we made for this node is
    /// still converging (see `retry_pending`), reports what we asked for
    /// instead. Several real devices (esp. Bluetooth) confirm a mute change
    /// far slower than others; with `mute_device = "all"` spanning multiple
    /// devices of different speeds, reporting raw per-device confirmations as
    /// they trickle in makes the aggregate "all muted?" answer flap between
    /// true and false for a moment after every button press - which the LED
    /// and audio thread would otherwise see as real, external toggles. Once a
    /// request is confirmed or its window lapses, this always matches reality
    /// again, so genuine external changes are still picked up normally.
    fn effective_mute(&self, node: &SourceNode) -> Option<bool> {
        let observed = self.observed_mute(node);
        if let Some((desired, since)) = node.desired {
            if observed != Some(desired) && Instant::now().duration_since(since) < CONVERGE_WINDOW
            {
                return Some(desired);
            }
        }
        observed
    }

    fn is_muted_for(&self, selector: &MuteDeviceSelector) -> bool {
        match selector {
            MuteDeviceSelector::All => {
                for node in self.nodes.values() {
                    if self.effective_mute(node) != Some(true) {
                        return false;
                    }
                }
                true
            },
            MuteDeviceSelector::Default => self
                .default_source_name
                .as_ref()
                .and_then(|name| self.nodes.values().find(|n| &n.name == name))
                .and_then(|n| self.effective_mute(n))
                .unwrap_or(false),
            MuteDeviceSelector::Selected => self
                .nodes
                .values()
                .find(|n| n.name == self.settings.selected_device_name)
                .and_then(|n| self.effective_mute(n))
                .unwrap_or(false),
        }
    }

    fn current_mute_status(&self) -> bool {
        let selector = self
            .settings
            .unmute_device
            .as_ref()
            .unwrap_or(&self.settings.mute_device);
        self.is_muted_for(selector)
    }

    fn publish_if_changed(&mut self) {
        let current = self.current_mute_status();
        if self.last_published != Some(current) {
            self.last_published = Some(current);
            self.ctrl_sender
                .send(ControlMessage::PublishMuteStatus(current))
                .unwrap_or(());
        }
    }

    fn route_for(&self, node: &SourceNode) -> Option<(&Device, i32, i32)> {
        let device_id = node.device_id?;
        let route_device = node.route_device?;
        let dev = self.devices.get(&device_id)?;
        let route = dev.routes.get(&route_device)?;
        Some((&dev.device, route.route_index, route_device))
    }

    fn set_muted(&mut self, muted: bool) {
        let selector = if muted {
            self.settings.mute_device.clone()
        } else {
            self.settings
                .unmute_device
                .clone()
                .unwrap_or_else(|| self.settings.mute_device.clone())
        };
        let mut matched_any = false;
        let now = Instant::now();
        let default_source_name = self.default_source_name.clone();
        let selected_device_name = self.settings.selected_device_name.clone();
        let devices = &self.devices;
        for node in self.nodes.values_mut() {
            let matches = match &selector {
                MuteDeviceSelector::All => true,
                MuteDeviceSelector::Default => default_source_name.as_deref() == Some(&node.name),
                MuteDeviceSelector::Selected => selected_device_name == node.name,
            };
            if !matches {
                continue;
            }
            matched_any = true;
            node.desired = Some((muted, now));

            let mut route = None;
            if let (Some(device_id), Some(route_device)) = (node.device_id, node.route_device) {
                if let Some(dev) = devices.get(&device_id) {
                    if let Some(r) = dev.routes.get(&route_device) {
                        route = Some((&dev.device, r.route_index, route_device));
                    }
                }
            }
            match route {
                Some((device, route_index, route_device)) => {
                    set_route_mute(device, route_index, route_device, muted)
                },
                None => set_node_mute(&node.node, muted),
            }
        }
        // Optimistically reflect the intended state; the real param-changed event
        // from the server will confirm (or correct) it shortly after. Only do this
        // when a node was actually asked to change, so an unmatched request (e.g.
        // the selected device hasn't been discovered yet) doesn't paper over the
        // real state once it does arrive.
        if matched_any {
            self.last_published = Some(muted);
        }
    }

    /// Re-sends mute writes that haven't yet been confirmed by an incoming
    /// node/route event. Some hardware routes silently drop a write made
    /// before the device has fully settled after being (re)discovered, so a
    /// single request isn't reliable; this is polled on a short interval to
    /// paper over that without the caller needing to know about it.
    fn retry_pending(&self) {
        let now = Instant::now();
        for node in self.nodes.values() {
            let Some((desired, since)) = node.desired else {
                continue;
            };
            if now.duration_since(since) > CONVERGE_WINDOW {
                continue;
            }
            if self.observed_mute(node) == Some(desired) {
                continue;
            }
            match self.route_for(node) {
                Some((device, route_index, route_device)) => {
                    set_route_mute(device, route_index, route_device, desired)
                },
                None => set_node_mute(&node.node, desired),
            }
        }
    }
}

fn set_node_mute(node: &Node, muted: bool) {
    let value = Value::Object(Object {
        type_: SpaTypes::ObjectParamProps.as_raw(),
        id: ParamType::Props.as_raw(),
        properties: vec![Property::new(spa::sys::SPA_PROP_mute, Value::Bool(muted))],
    });
    if let Ok((cursor, _)) = PodSerializer::serialize(Cursor::new(Vec::<u8>::new()), &value) {
        let bytes = cursor.into_inner();
        if let Some(pod) = Pod::from_bytes(&bytes) {
            node.set_param(ParamType::Props, 0, pod);
        }
    }
}

fn parse_mute_from_props(param: &Pod) -> Option<bool> {
    let (_rest, value) = PodDeserializer::deserialize_from::<Value>(param.as_bytes()).ok()?;
    if let Value::Object(obj) = value {
        for prop in obj.properties {
            if prop.key == spa::sys::SPA_PROP_mute {
                if let Value::Bool(muted) = prop.value {
                    return Some(muted);
                }
            }
        }
    }
    None
}

/// Sets mute via a Device's hardware Route - the actual control point for
/// devices with a hardware mute/volume switch (`route.hw-mute`). WirePlumber
/// does not mirror a plain Node Props.mute write into such a route, so setting
/// it there is a no-op for these devices; the route itself must be targeted.
fn set_route_mute(device: &Device, route_index: i32, route_device: i32, muted: bool) {
    let props = Value::Object(Object {
        type_: SpaTypes::ObjectParamProps.as_raw(),
        id: ParamType::Props.as_raw(),
        properties: vec![Property::new(spa::sys::SPA_PROP_mute, Value::Bool(muted))],
    });
    let value = Value::Object(Object {
        type_: SpaTypes::ObjectParamRoute.as_raw(),
        id: ParamType::Route.as_raw(),
        properties: vec![
            Property::new(spa::sys::SPA_PARAM_ROUTE_index, Value::Int(route_index)),
            Property::new(spa::sys::SPA_PARAM_ROUTE_device, Value::Int(route_device)),
            Property::new(spa::sys::SPA_PARAM_ROUTE_props, props),
        ],
    });
    if let Ok((cursor, _)) = PodSerializer::serialize(Cursor::new(Vec::<u8>::new()), &value) {
        let bytes = cursor.into_inner();
        if let Some(pod) = Pod::from_bytes(&bytes) {
            device.set_param(ParamType::Route, 0, pod);
        }
    }
}

/// Parses a Device's Route param event, returning (route_index, route_device,
/// mute) when the route carries a mute-capable Props sub-object.
fn parse_route(param: &Pod) -> Option<(i32, i32, bool)> {
    let (_rest, value) = PodDeserializer::deserialize_from::<Value>(param.as_bytes()).ok()?;
    let Value::Object(obj) = value else { return None };
    let mut index = None;
    let mut device = None;
    let mut mute = None;
    for prop in obj.properties {
        if prop.key == spa::sys::SPA_PARAM_ROUTE_index {
            if let Value::Int(i) = prop.value {
                index = Some(i);
            }
        } else if prop.key == spa::sys::SPA_PARAM_ROUTE_device {
            if let Value::Int(i) = prop.value {
                device = Some(i);
            }
        } else if prop.key == spa::sys::SPA_PARAM_ROUTE_props {
            if let Value::Object(props_obj) = prop.value {
                for p in props_obj.properties {
                    if p.key == spa::sys::SPA_PROP_mute {
                        if let Value::Bool(m) = p.value {
                            mute = Some(m);
                        }
                    }
                }
            }
        }
    }
    Some((index?, device?, mute?))
}

/// The "default.audio.source" metadata value is a JSON object like
/// `{"name":"alsa_input.xxx"}` - pull out just the name.
fn parse_default_node_name(value: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(value).ok()?;
    parsed.get("name")?.as_str().map(String::from)
}

/// Runs the native PipeWire backend thread.
///
/// Unlike the PulseAudio backend, this reacts to mute-state changes pushed by the
/// server the instant any client (system tray, wireplumber, another app) changes
/// them - no polling, and no re-asserting a stale cached state.
pub fn run(
    settings: PipewireSettings,
    mute_on_startup: Option<bool>,
    audio_receiver: Receiver<AudioMessage>,
    ctrl_sender: Sender<ControlMessage>,
) {
    pw::init();

    let main_loop = match pw::main_loop::MainLoopRc::new(None) {
        Ok(m) => m,
        Err(err) => {
            println!("Failed to create PipeWire main loop: {}", err);
            return;
        },
    };
    let context = match pw::context::ContextRc::new(&main_loop, None) {
        Ok(c) => c,
        Err(err) => {
            println!("Failed to create PipeWire context: {}", err);
            return;
        },
    };
    let core = match context.connect_rc(None) {
        Ok(c) => c,
        Err(err) => {
            println!("Failed to connect to PipeWire: {}", err);
            return;
        },
    };
    let registry = match core.get_registry_rc() {
        Ok(r) => r,
        Err(err) => {
            println!("Failed to get PipeWire registry: {}", err);
            return;
        },
    };

    let state = Rc::new(RefCell::new(State {
        settings,
        ctrl_sender,
        nodes: HashMap::new(),
        devices: HashMap::new(),
        default_source_name: None,
        last_published: None,
        mute_on_startup,
        startup_deadline: Instant::now() + Duration::from_secs(2),
    }));

    // Bridge: the rest of the app (a plain OS thread) -> this mainloop thread.
    let (pw_sender, pw_receiver) = pw::channel::channel::<Command>();
    {
        let pw_sender = pw_sender.clone();
        thread_pump(audio_receiver, pw_sender);
    }

    let main_loop_for_cmds = main_loop.clone();
    let state_for_cmds = state.clone();
    let _cmd_receiver = pw_receiver.attach(main_loop.loop_(), move |cmd| match cmd {
        Command::SetMuteStatus(muted) => state_for_cmds.borrow_mut().set_muted(muted),
        Command::GetMuteStatus => {
            let mut state = state_for_cmds.borrow_mut();
            let current = state.current_mute_status();
            state.last_published = Some(current);
            state
                .ctrl_sender
                .send(ControlMessage::PublishMuteStatus(current))
                .unwrap_or(());
        },
        Command::Terminate => main_loop_for_cmds.quit(),
    });

    // Non-Node proxies (currently just the "default" Metadata object) and their
    // listeners, kept alive here since State only tracks Nodes.
    let other_proxies: Rc<RefCell<HashMap<u32, (Box<dyn ProxyT>, Vec<Box<dyn Listener>>)>>> =
        Rc::new(RefCell::new(HashMap::new()));

    let registry_for_global = registry.clone();
    let state_for_global = state.clone();
    let other_proxies_for_global = other_proxies.clone();
    let state_for_remove = state.clone();
    let other_proxies_for_remove = other_proxies.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |obj| {
            match obj.type_ {
                ObjectType::Node => {
                    let is_source = obj
                        .props
                        .and_then(|p| p.get("media.class"))
                        .map(|c| c == "Audio/Source")
                        .unwrap_or(false);
                    if !is_source {
                        return;
                    }
                    let name = obj
                        .props
                        .and_then(|p| p.get("node.name"))
                        .unwrap_or("")
                        .to_string();
                    let device_id = obj
                        .props
                        .and_then(|p| p.get("device.id"))
                        .and_then(|v| v.parse::<u32>().ok());
                    let node: Node = match registry_for_global.bind(obj) {
                        Ok(n) => n,
                        Err(_) => return,
                    };
                    node.subscribe_params(&[ParamType::Props]);
                    node.enum_params(0, Some(ParamType::Props), 0, u32::MAX);

                    let node_id = obj.id;
                    // Weak, so the node's own listener (stored inside `state`) doesn't
                    // keep `state` alive forever through a reference cycle.
                    let state_weak: Weak<RefCell<State>> = Rc::downgrade(&state_for_global);
                    let state_weak_info = state_weak.clone();
                    let listener = node
                        .add_listener_local()
                        .info(move |info| {
                            // The registry's global-object props are a reduced set;
                            // "card.profile.device" (needed to correlate this node to
                            // its parent Device's hardware route) is only present on
                            // the node's own full info props.
                            let route_device = info
                                .props()
                                .and_then(|p| p.get("card.profile.device"))
                                .and_then(|v| v.parse::<i32>().ok());
                            if let (Some(route_device), Some(state)) =
                                (route_device, state_weak_info.upgrade())
                            {
                                let mut state = state.borrow_mut();
                                if let Some(n) = state.nodes.get_mut(&node_id) {
                                    n.route_device = Some(route_device);
                                }
                                state.publish_if_changed();
                            }
                        })
                        .param(move |_seq, id, _index, _next, param| {
                            if id != ParamType::Props {
                                return;
                            }
                            let Some(param) = param else { return };
                            let Some(muted) = parse_mute_from_props(param) else {
                                return;
                            };
                            if let Some(state) = state_weak.upgrade() {
                                let mut state = state.borrow_mut();
                                if let Some(n) = state.nodes.get_mut(&node_id) {
                                    n.muted = Some(muted);
                                }
                                state.publish_if_changed();
                            }
                        })
                        .register();

                    let mut state = state_for_global.borrow_mut();
                    state.nodes.insert(
                        node_id,
                        SourceNode {
                            name,
                            muted: None,
                            node,
                            _listener: listener,
                            device_id,
                            route_device: None,
                            desired: None,
                        },
                    );
                    // Applying mute_on_startup here (rather than waiting for this
                    // node to report its actual state) means it races a node that
                    // needs a hardware route not yet known for it; retry_pending()
                    // is what makes that eventually consistent instead of a no-op.
                    if let Some(desired) = state.mute_on_startup {
                        if Instant::now() < state.startup_deadline {
                            state.set_muted(desired);
                        }
                    }
                },
                ObjectType::Device => {
                    let device: Device = match registry_for_global.bind(obj) {
                        Ok(d) => d,
                        Err(_) => return,
                    };
                    device.subscribe_params(&[ParamType::Route]);
                    device.enum_params(0, Some(ParamType::Route), 0, u32::MAX);

                    let device_id = obj.id;
                    let state_weak: Weak<RefCell<State>> = Rc::downgrade(&state_for_global);
                    let listener = device
                        .add_listener_local()
                        .param(move |_seq, id, _index, _next, param| {
                            if id != ParamType::Route {
                                return;
                            }
                            let Some(param) = param else { return };
                            let Some((route_index, route_device, muted)) = parse_route(param)
                            else {
                                return;
                            };
                            if let Some(state) = state_weak.upgrade() {
                                let mut state = state.borrow_mut();
                                if let Some(dev) = state.devices.get_mut(&device_id) {
                                    dev.routes.insert(
                                        route_device,
                                        RouteState {
                                            route_index,
                                            muted: Some(muted),
                                        },
                                    );
                                }
                                state.publish_if_changed();
                            }
                        })
                        .register();

                    let mut state = state_for_global.borrow_mut();
                    state.devices.insert(
                        device_id,
                        DeviceEntry {
                            device,
                            routes: HashMap::new(),
                            _listener: listener,
                        },
                    );
                    if let Some(desired) = state.mute_on_startup {
                        if Instant::now() < state.startup_deadline {
                            state.set_muted(desired);
                        }
                    }
                },
                ObjectType::Metadata => {
                    let is_default = obj
                        .props
                        .and_then(|p| p.get("metadata.name"))
                        .map(|n| n == "default")
                        .unwrap_or(false);
                    if !is_default {
                        return;
                    }
                    let metadata: Metadata = match registry_for_global.bind(obj) {
                        Ok(m) => m,
                        Err(_) => return,
                    };
                    let state_for_meta = state_for_global.clone();
                    let listener = metadata
                        .add_listener_local()
                        .property(move |_subject, key, _type_, value| {
                            if key == Some("default.audio.source") {
                                let name = value.and_then(parse_default_node_name);
                                let mut state = state_for_meta.borrow_mut();
                                state.default_source_name = name;
                                state.publish_if_changed();
                            }
                            0
                        })
                        .register();
                    other_proxies_for_global
                        .borrow_mut()
                        .insert(obj.id, (Box::new(metadata), vec![Box::new(listener)]));
                },
                _ => {},
            }
        })
        .global_remove(move |id| {
            let mut state = state_for_remove.borrow_mut();
            state.nodes.remove(&id);
            state.devices.remove(&id);
            other_proxies_for_remove.borrow_mut().remove(&id);
        })
        .register();

    let state_for_retry = state.clone();
    let retry_timer = main_loop.loop_().add_timer(move |_| {
        state_for_retry.borrow().retry_pending();
    });
    retry_timer.update_timer(Some(Duration::from_millis(300)), Some(Duration::from_millis(300)));

    main_loop.run();
}

/// Pumps the app's generic (crossbeam) audio command channel into PipeWire's own
/// cross-thread channel, so the rest of the app doesn't need to know which audio
/// backend is active.
fn thread_pump(audio_receiver: Receiver<AudioMessage>, pw_sender: pw::channel::Sender<Command>) {
    std::thread::spawn(move || loop {
        match audio_receiver.recv() {
            Ok(AudioMessage::GetMuteStatus) => {
                if pw_sender.send(Command::GetMuteStatus).is_err() {
                    break;
                }
            },
            Ok(AudioMessage::SetMuteStatus(muted)) => {
                if pw_sender.send(Command::SetMuteStatus(muted)).is_err() {
                    break;
                }
            },
            Ok(AudioMessage::Terminate) => {
                pw_sender.send(Command::Terminate).unwrap_or(());
                break;
            },
            Err(_) => {
                pw_sender.send(Command::Terminate).unwrap_or(());
                break;
            },
        }
    });
}
