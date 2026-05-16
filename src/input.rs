use crate::app::PadFilterType;

use evdev::uinput::VirtualDevice;
use evdev::*;
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token, Waker};
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

#[derive(Clone, PartialEq, Copy)]
pub enum DeviceType {
    Gamepad,
    Keyboard,
    Mouse,
    Other,
}

pub enum PadButton {
    Left,
    Right,
    Up,
    Down,
    ABtn,
    BBtn,
    XBtn,
    YBtn,
    StartBtn,
    SelectBtn,

    AKey,
    RKey,
    XKey,
    ZKey,

    RightClick,
}

#[derive(Clone)]
pub struct DeviceInfo {
    pub path: String,
    pub _enabled: bool,
    pub device_type: DeviceType,
}

pub struct InputDevice {
    path: String,
    dev: Device,
    enabled: bool,
    device_type: DeviceType,
    has_button_held: bool,
}
impl InputDevice {
    pub fn name(&self) -> &str {
        self.dev.name().unwrap_or_else(|| "")
    }
    pub fn emoji(&self) -> &str {
        match self.device_type() {
            DeviceType::Gamepad => "🎮",
            DeviceType::Keyboard => "🖮",
            DeviceType::Mouse => "🖱",
            DeviceType::Other => "",
        }
    }
    pub fn fancyname(&self) -> &str {
        match self.dev.input_id().vendor() {
            0x045e => "Xbox Controller",
            0x054c => "PS Controller",
            0x057e => "NT Pro Controller",
            0x28de => "Steam Input",
            _ => self.name(),
        }
    }
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn enabled(&self) -> bool {
        self.enabled
    }
    pub fn device_type(&self) -> DeviceType {
        self.device_type
    }
    pub fn has_button_held(&self) -> bool {
        self.has_button_held
    }
    pub fn info(&self) -> DeviceInfo {
        DeviceInfo {
            path: self.path().to_string(),
            _enabled: self.enabled(),
            device_type: self.device_type(),
        }
    }
    pub fn poll(&mut self) -> Option<PadButton> {
        let mut btn: Option<PadButton> = None;
        if let Ok(events) = self.dev.fetch_events() {
            for event in events {
                let summary = event.destructure();

                match summary {
                    EventSummary::Key(_, _, 1) => {
                        self.has_button_held = true;
                    }
                    EventSummary::Key(_, _, 0) => {
                        self.has_button_held = false;
                    }
                    _ => {}
                }

                btn = match summary {
                    EventSummary::Key(_, KeyCode::BTN_SOUTH, 1) => Some(PadButton::ABtn),
                    EventSummary::Key(_, KeyCode::BTN_EAST, 1) => Some(PadButton::BBtn),
                    EventSummary::Key(_, KeyCode::BTN_NORTH, 1) => Some(PadButton::XBtn),
                    EventSummary::Key(_, KeyCode::BTN_WEST, 1) => Some(PadButton::YBtn),
                    EventSummary::Key(_, KeyCode::BTN_START, 1) => Some(PadButton::StartBtn),
                    EventSummary::Key(_, KeyCode::BTN_SELECT, 1) => Some(PadButton::SelectBtn),
                    EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_HAT0X, -1) => {
                        Some(PadButton::Left)
                    }
                    EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_HAT0X, 1) => {
                        Some(PadButton::Right)
                    }
                    EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_HAT0Y, -1) => {
                        Some(PadButton::Up)
                    }
                    EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_HAT0Y, 1) => {
                        Some(PadButton::Down)
                    }
                    EventSummary::Key(_, KeyCode::KEY_A, 1) => Some(PadButton::AKey),
                    EventSummary::Key(_, KeyCode::KEY_R, 1) => Some(PadButton::RKey),
                    EventSummary::Key(_, KeyCode::KEY_X, 1) => Some(PadButton::XKey),
                    EventSummary::Key(_, KeyCode::KEY_Z, 1) => Some(PadButton::ZKey),
                    EventSummary::Key(_, KeyCode::BTN_RIGHT, 1) => Some(PadButton::RightClick),
                    _ => btn,
                };
            }
        }
        btn
    }
}

pub fn scan_input_devices(filter: &PadFilterType) -> Vec<InputDevice> {
    let mut pads: Vec<InputDevice> = Vec::new();
    for dev in evdev::enumerate() {
        // Skip virtual/loopback devices (no physical path = uinput-created, including our own)
        if let Some(phys) = dev.1.physical_path() {
            if phys.is_empty() {
                continue;
            }
        } else {
            continue;
        }

        let enabled = match filter {
            PadFilterType::All => true,
            PadFilterType::NoSteamInput => dev.1.input_id().vendor() != 0x28de,
            PadFilterType::OnlySteamInput => dev.1.input_id().vendor() == 0x28de,
        };

        let device_type = if dev
            .1
            .supported_keys()
            .map_or(false, |keys| keys.contains(KeyCode::BTN_SOUTH))
        {
            DeviceType::Gamepad
        } else if dev
            .1
            .supported_keys()
            .map_or(false, |keys| keys.contains(KeyCode::BTN_LEFT))
        {
            DeviceType::Mouse
        } else if dev
            .1
            .supported_keys()
            .map_or(false, |keys| keys.contains(KeyCode::KEY_SPACE))
        {
            DeviceType::Keyboard
        } else {
            DeviceType::Other
        };

        if device_type != DeviceType::Other {
            let _ = dev.1.set_nonblocking(true);
            pads.push(InputDevice {
                path: dev.0.to_str().unwrap().to_string(),
                dev: dev.1,
                enabled,
                device_type,
                has_button_held: false,
            });
        }
    }
    pads.sort_by_key(|pad| pad.path().to_string());
    pads
}

const WAKER_TOKEN: Token = Token(usize::MAX); // REVIEW: can this collide with other apps?

pub struct RouterSlot {
    pub virtual_device: VirtualDevice,
    pub physical_path: Option<String>,
    pub last_log_time: Instant,
}

pub struct InputRouter {
    pub slots: Arc<Mutex<Vec<RouterSlot>>>,
    waker: Arc<Mutex<Option<Arc<Waker>>>>,
    thread_handle: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
    // REVIEW: 3 mutex, possible deadlock?
}

impl Clone for InputRouter {
    fn clone(&self) -> Self {
        Self {
            slots: self.slots.clone(),
            waker: self.waker.clone(),
            thread_handle: self.thread_handle.clone(),
        }
    }
}

impl InputRouter {
    pub fn new() -> Self {
        Self {
            slots: Arc::new(Mutex::new(Vec::new())),
            waker: Arc::new(Mutex::new(None)),
            thread_handle: Arc::new(Mutex::new(None)),
        }
    }

    /// Signal the routing thread to stop and block until it has fully exited,
    /// releasing all exclusive device grabs.
    pub fn stop_routing(&self) {
        // Wake the poll loop — it will exit on the WAKER_TOKEN event.
        if let Some(w) = self.waker.lock().unwrap().take() {
            let _ = w.wake();
        }
        if let Some(handle) = self.thread_handle.lock().unwrap().take() {
            let _ = handle.join();
        }
    }

    pub fn start_routing(&self) {
        let slots = self.slots.clone();
        let waker_slot = self.waker.clone();
        let thread_handle = self.thread_handle.clone();

        let handle = thread::spawn(move || {
            // Build the Poll instance and Waker inside the thread so their
            // lifetimes are fully owned here.
            let mut poll = match Poll::new() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("[partydeck] Failed to create mio Poll: {}", e);
                    return;
                }
            };
            let waker = match Waker::new(poll.registry(), WAKER_TOKEN) {
                Ok(w) => Arc::new(w),
                Err(e) => {
                    eprintln!("[partydeck] Failed to create mio Waker: {}", e);
                    return;
                }
            };
            // Publish the Waker so stop_routing() can reach it.
            *waker_slot.lock().unwrap() = Some(waker.clone());

            let mut physical_devices: std::collections::HashMap<String, Device> =
                std::collections::HashMap::new();
            // Maps mio Token → physical device path
            let mut token_to_path: std::collections::HashMap<Token, String> =
                std::collections::HashMap::new();
            let mut next_token = 0usize;
            // Paths that disconnected and are awaiting reconnection
            let mut pending_reconnect: std::collections::HashSet<String> =
                std::collections::HashSet::new();

            // Helper: open, grab, and register a single device path.
            // Returns true on success.
            let register_device = |path: &str,
                                   poll: &mut Poll,
                                   physical_devices: &mut std::collections::HashMap<
                String,
                Device,
            >,
                                   token_to_path: &mut std::collections::HashMap<Token, String>,
                                   next_token: &mut usize|
             -> bool {
                match Device::open(path) {
                    Ok(mut dev) => {
                        let _ = dev.grab();
                        let token = Token(*next_token);
                        *next_token += 1;
                        if let Err(e) = poll.registry().register(
                            &mut SourceFd(&dev.as_raw_fd()),
                            token,
                            Interest::READABLE,
                        ) {
                            eprintln!("[partydeck] Failed to register {} with mio: {}", path, e);
                            false
                        } else {
                            println!("[partydeck] Grabbed physical device: {}", path);
                            token_to_path.insert(token, path.to_string());
                            physical_devices.insert(path.to_string(), dev);
                            true
                        }
                    }
                    Err(_) => false,
                }
            };

            // Open, grab, and register every physical device assigned to a slot.
            {
                let slots_lock = slots.lock().unwrap();
                for slot in slots_lock.iter() {
                    if let Some(path) = &slot.physical_path {
                        if physical_devices.contains_key(path) {
                            continue;
                        }
                        if !register_device(
                            path,
                            &mut poll,
                            &mut physical_devices,
                            &mut token_to_path,
                            &mut next_token,
                        ) {
                            eprintln!("[partydeck] Failed to open {} at startup", path);
                            pending_reconnect.insert(path.clone());
                        }
                    }
                }
            }

            println!("[partydeck] Input routing thread started");

            let mut events = Events::with_capacity(32);

            loop {
                // Block until an fd is readable or the reconnect interval elapses.
                // The 1s timeout drives reconnection attempts for dead devices.
                if let Err(e) = poll.poll(&mut events, Some(std::time::Duration::from_secs(1))) {
                    if e.kind() == std::io::ErrorKind::Interrupted {
                        continue; // EINTR — retry
                    }
                    eprintln!("[partydeck] mio poll error: {}", e);
                    break;
                }

                let mut dead_tokens: Vec<Token> = Vec::new();

                for event in &events {
                    let token = event.token();

                    // Waker fired → stop_routing() was called.
                    if token == WAKER_TOKEN {
                        println!("[partydeck] Input routing thread stopped");
                        return;
                    }

                    if let Some(path) = token_to_path.get(&token) {
                        if let Some(dev) = physical_devices.get_mut(path) {
                            match dev.fetch_events() {
                                Ok(raw_events) => {
                                    let ev_vec: Vec<InputEvent> = raw_events.collect();
                                    if ev_vec.is_empty() {
                                        continue;
                                    }

                                    // Fan out to every slot mapped to this device.
                                    let mut slots_lock = slots.lock().unwrap();
                                    for (idx, slot) in slots_lock.iter_mut().enumerate() {
                                        if slot.physical_path.as_deref() == Some(path) {
                                            let now = Instant::now();
                                            if now.duration_since(slot.last_log_time)
                                                >= std::time::Duration::from_secs(2)
                                            {
                                                slot.last_log_time = now;
                                                println!(
                                                    "[partydeck] Routing events to slot {}",
                                                    idx
                                                );
                                            }
                                            emit_batched(&mut slot.virtual_device, &ev_vec, idx);
                                        }
                                    }
                                }
                                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                                Err(e) => {
                                    eprintln!(
                                        "[partydeck] Physical device disconnected: {} - {}",
                                        path, e
                                    );
                                    dead_tokens.push(token);
                                }
                            }
                        }
                    }
                }

                // Deregister disconnected devices and queue them for reconnection.
                for token in dead_tokens {
                    if let Some(path) = token_to_path.remove(&token) {
                        if let Some(dev) = physical_devices.remove(&path) {
                            let _ = poll.registry().deregister(&mut SourceFd(&dev.as_raw_fd()));
                        }
                        println!("[partydeck] Queued {} for reconnection", path);
                        pending_reconnect.insert(path);
                    }
                }

                // Attempt to reconnect any pending devices.
                if !pending_reconnect.is_empty() {
                    let reconnected: Vec<String> = pending_reconnect
                        .iter()
                        .filter(|path| {
                            register_device(
                                path,
                                &mut poll,
                                &mut physical_devices,
                                &mut token_to_path,
                                &mut next_token,
                            )
                        })
                        .cloned()
                        .collect();
                    for path in reconnected {
                        pending_reconnect.remove(&path);
                        println!("[partydeck] Reconnected device: {}", path);
                    }
                }
            }

            println!("[partydeck] Input routing thread stopped (loop exited)");
        });
        *thread_handle.lock().unwrap() = Some(handle);
    }

    /// Create a virtual device mirroring the given physical device, register it as a slot,
    /// and return all dev node paths (eventX + legacy jsX) to be bind-mounted into the sandbox.
    pub fn add_slot(&self, phys_path: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let phys_dev = Device::open(phys_path)?;
        let mut builder = uinput::VirtualDevice::builder()?;

        builder = builder.name(phys_dev.name().unwrap_or("Virtual Gamepad"));
        builder = builder.input_id(phys_dev.input_id());

        if let Some(keys) = phys_dev.supported_keys() {
            builder = builder.with_keys(keys)?;
        }
        if let Some(rel) = phys_dev.supported_relative_axes() {
            builder = builder.with_relative_axes(rel)?;
        }
        if let Ok(abs_infos) = phys_dev.get_absinfo() {
            for (code, info) in abs_infos {
                builder = builder.with_absolute_axis(&evdev::UinputAbsSetup::new(code, info))?;
            }
        }
        if let Some(sw) = phys_dev.supported_switches() {
            builder = builder.with_switches(sw)?;
        }

        let mut vdev = builder.build()?;

        // Wait for udev/joydev to stabilise permissions and create legacy nodes.
        std::thread::sleep(std::time::Duration::from_millis(1000));

        let mut nodes = Vec::new();
        for node_res in vdev.enumerate_dev_nodes_blocking()? {
            if let Ok(node) = node_res {
                let path_str = node.to_string_lossy().to_string();
                nodes.push(path_str.clone());

                // Also collect legacy /dev/input/jsX nodes via sysfs.
                if let Some(event_name) = std::path::Path::new(&path_str)
                    .file_name()
                    .and_then(|n| n.to_str())
                {
                    let sysfs_dir = format!("/sys/class/input/{}/device", event_name);
                    if let Ok(entries) = std::fs::read_dir(sysfs_dir) {
                        for entry in entries.flatten() {
                            if let Ok(name) = entry.file_name().into_string() {
                                if name.starts_with("js") {
                                    let js_path = format!("/dev/input/{}", name);
                                    nodes.push(js_path.clone());
                                    println!("[partydeck] Bound legacy joystick node: {}", js_path);
                                }
                            }
                        }
                    }
                }
            }
        }

        self.slots.lock().unwrap().push(RouterSlot {
            virtual_device: vdev,
            physical_path: Some(phys_path.to_string()),
            last_log_time: Instant::now(),
        });

        Ok(nodes)
    }
}

/// Emit a batch of input events to a virtual device, flushing on each SYN_REPORT.
fn emit_batched(vdev: &mut VirtualDevice, events: &[InputEvent], slot_idx: usize) {
    let mut batch: Vec<InputEvent> = Vec::new();
    for event in events {
        batch.push(*event);
        if event.event_type() == EventType::SYNCHRONIZATION && event.code() == 0 {
            if let Err(e) = vdev.emit(&batch) {
                eprintln!("[partydeck] Emit error on slot {}: {}", slot_idx, e);
            }
            batch.clear();
        }
    }
    if !batch.is_empty() {
        let _ = vdev.emit(&batch);
    }
}
