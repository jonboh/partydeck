use std::error::Error;
use std::io::BufRead;
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Handle for the thread that performs the hyprland window management (size,
/// position, decorations).
/// The thread listens on the Hyprland IPC socket and float-positions any
/// gamescope window that opens.
/// Call stop() for a clean explicit shutdown (sets flag that stops the thread
/// and joins it).
/// Dropping the handle sets the stop flag but does not join it, so it
/// will not block in error conditions.
pub struct HyprlandManager {
    stop_flag: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl HyprlandManager {
    pub fn stop(mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        // Thread is blocked on a socket read, closing should happen when the games
        // exit (the thread loop will end on the next wakeup / error).
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for HyprlandManager {
    fn drop(&mut self) {
        // Signal the thread to stop.
        // We dont join to prevent blocking on error conditions
        self.stop_flag.store(true, Ordering::Relaxed);
    }
}

pub fn hyprland_start(vertical: bool) -> Result<HyprlandManager, Box<dyn Error>> {
    // Resolve socket path from environment variables.
    let xdg = std::env::var("XDG_RUNTIME_DIR").map_err(|_| "XDG_RUNTIME_DIR is not set")?;
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
        .map_err(|_| "HYPRLAND_INSTANCE_SIGNATURE is not set — is Hyprland running?")?;
    let socket_path = format!("{}/hypr/{}/.socket2.sock", xdg, sig);

    apply_decoration_rules();

    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_flag_thread = Arc::clone(&stop_flag);

    let thread = std::thread::spawn(move || {
        event_loop(&socket_path, vertical, stop_flag_thread);
    });

    Ok(HyprlandManager {
        stop_flag,
        thread: Some(thread),
    })
}

/// Strips all decorations from gamescope windows with class-based windowrulev2 rules.
fn apply_decoration_rules() {
    let classes = ["gamescope", "gamescope-kbm", r"\.gamescope-wrapped"];
    let rules = ["noborder", "rounding 0", "noshadow", "nodim"];

    for class in &classes {
        let selector = format!("class:^({})$", class);
        for rule in &rules {
            let rule_str = format!("{},{}", rule, selector);
            println!(
                "[partydeck] hyprland::apply_decoration_rules: windowrulev2 {}",
                rule_str
            );
            let out = Command::new("hyprctl")
                .args(["keyword", "windowrulev2", &rule_str])
                .output();
            match out {
                Ok(o) => {
                    let response = String::from_utf8_lossy(&o.stdout);
                    println!(
                        "[partydeck] hyprland::apply_decoration_rules: response: {}",
                        response.trim()
                    );
                }
                Err(e) => {
                    println!("[partydeck] hyprland::apply_decoration_rules: error: {}", e);
                }
            }
        }
    }
}

fn event_loop(socket_path: &str, vertical: bool, stop: Arc<AtomicBool>) {
    let stream = match UnixStream::connect(socket_path) {
        Ok(s) => {
            println!(
                "[partydeck] hyprland::event_loop: listening on {}",
                socket_path
            );
            s
        }
        Err(e) => {
            println!(
                "[partydeck] hyprland::event_loop: failed to connect to socket: {}",
                e
            );
            return;
        }
    };

    let reader = std::io::BufReader::new(stream);
    for line in reader.lines() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };

        // Events look like: openwindow>>addr,workspace,class,title
        if let Some(rest) = line.strip_prefix("openwindow>>") {
            let addr = rest.split(',').next().unwrap_or("").to_string();
            println!("[partydeck] hyprland::event_loop: openwindow addr={}", addr);
            // Small delay to allow Hyprland to finish placing the window in its
            // tiling layout before we override the geometry.
            std::thread::sleep(std::time::Duration::from_millis(300));
            apply_splitscreen(vertical);
        }
    }

    println!("[partydeck] hyprland::event_loop: exiting");
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct MonitorId(i64);

impl std::fmt::Display for MonitorId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy)]
struct MonitorRect {
    x: i64,
    y: i64,
    width: i64,
    height: i64,
}

#[derive(Debug, Clone, Copy)]
struct WindowRect {
    x: i64,
    y: i64,
    width: i64,
    height: i64,
}

impl MonitorRect {
    /// Compute the window rectangle for a given player slot using fractional
    /// layout tables. `xf`, `yf`, `wf`, `hf` are slices of fractions for the
    /// current player count; `idx` is the zero-based player index.
    fn window_rect(&self, layout: &Layout, idx: usize) -> WindowRect {
        WindowRect {
            x: self.x + (layout.x[idx] * self.width as f64) as i64,
            y: self.y + (layout.y[idx] * self.height as f64) as i64,
            width: (layout.width[idx] * self.width as f64) as i64,
            height: (layout.height[idx] * self.height as f64) as i64,
        }
    }
}

fn get_monitors() -> std::collections::HashMap<MonitorId, MonitorRect> {
    let mut monitors = std::collections::HashMap::new();
    let out = match Command::new("hyprctl").args(["-j", "monitors"]).output() {
        Ok(o) => o.stdout,
        Err(e) => {
            println!("[partydeck] hyprland::get_monitors: hyprctl failed: {}", e);
            return monitors;
        }
    };
    let json: serde_json::Value = match serde_json::from_slice(&out) {
        Ok(v) => v,
        Err(e) => {
            println!("[partydeck] hyprland::get_monitors: parse failed: {}", e);
            return monitors;
        }
    };
    if let Some(arr) = json.as_array() {
        for m in arr {
            let id = MonitorId(m["id"].as_i64().unwrap_or(0));
            let rect = MonitorRect {
                x: m["x"].as_i64().unwrap_or(0),
                y: m["y"].as_i64().unwrap_or(0),
                width: m["width"].as_i64().unwrap_or(1920),
                height: m["height"].as_i64().unwrap_or(1080),
            };
            monitors.insert(id, rect);
        }
    }
    monitors
}

fn get_gamescope_by_monitor() -> std::collections::HashMap<MonitorId, Vec<String>> {
    let mut by_monitor: std::collections::HashMap<MonitorId, Vec<String>> =
        std::collections::HashMap::new();
    let out = match Command::new("hyprctl").args(["-j", "clients"]).output() {
        Ok(o) => o.stdout,
        Err(e) => {
            println!(
                "[partydeck] hyprland::get_gamescope_by_monitor: hyprctl failed: {}",
                e
            );
            return by_monitor;
        }
    };
    let json: serde_json::Value = match serde_json::from_slice(&out) {
        Ok(v) => v,
        Err(e) => {
            println!(
                "[partydeck] hyprland::get_gamescope_by_monitor: parse failed: {}",
                e
            );
            return by_monitor;
        }
    };
    if let Some(arr) = json.as_array() {
        for window_entry in arr {
            let class = window_entry["class"].as_str().unwrap_or("");
            let is_gamescope =
                class == "gamescope" || class == "gamescope-kbm" || class.starts_with(".gamescope");
            if !is_gamescope {
                continue;
            }
            let addr = window_entry["address"].as_str().unwrap_or("").to_string();
            let mon = MonitorId(window_entry["monitor"].as_i64().unwrap_or(0));
            by_monitor.entry(mon).or_default().push(addr);
        }
    }
    by_monitor
}

/// Fractional x/y/w/h positions for each player in a given player count.
struct Layout {
    x: Vec<f64>,
    y: Vec<f64>,
    width: Vec<f64>,
    height: Vec<f64>,
}

/// Full set of layouts for 1–4 players, for a given split orientation.
struct SplitLayout {
    one: Layout,
    two: Layout,
    three: Layout,
    four: Layout,
}

impl SplitLayout {
    fn for_count(&self, count: usize) -> Option<&Layout> {
        match count {
            1 => Some(&self.one),
            2 => Some(&self.two),
            3 => Some(&self.three),
            4 => Some(&self.four),
            _ => None,
        }
    }
}

fn horizontal_layout() -> SplitLayout {
    SplitLayout {
        one: Layout {
            x: vec![0.0],
            y: vec![0.0],
            width: vec![1.0],
            height: vec![1.0],
        },
        two: Layout {
            x: vec![0.0, 0.0],
            y: vec![0.0, 0.5],
            width: vec![1.0, 1.0],
            height: vec![0.5, 0.5],
        },
        three: Layout {
            x: vec![0.0, 0.0, 0.5],
            y: vec![0.0, 0.5, 0.5],
            width: vec![1.0, 0.5, 0.5],
            height: vec![0.5, 0.5, 0.5],
        },
        four: Layout {
            x: vec![0.0, 0.5, 0.0, 0.5],
            y: vec![0.0, 0.0, 0.5, 0.5],
            width: vec![0.5, 0.5, 0.5, 0.5],
            height: vec![0.5, 0.5, 0.5, 0.5],
        },
    }
}

fn vertical_layout() -> SplitLayout {
    SplitLayout {
        one: Layout {
            x: vec![0.0],
            y: vec![0.0],
            width: vec![1.0],
            height: vec![1.0],
        },
        two: Layout {
            x: vec![0.0, 0.5],
            y: vec![0.0, 0.0],
            width: vec![0.5, 0.5],
            height: vec![1.0, 1.0],
        },
        three: Layout {
            x: vec![0.0, 0.0, 0.5],
            y: vec![0.0, 0.5, 0.5],
            width: vec![1.0, 0.5, 0.5],
            height: vec![0.5, 0.5, 0.5],
        },
        four: Layout {
            x: vec![0.0, 0.5, 0.0, 0.5],
            y: vec![0.0, 0.0, 0.5, 0.5],
            width: vec![0.5, 0.5, 0.5, 0.5],
            height: vec![0.5, 0.5, 0.5, 0.5],
        },
    }
}

fn apply_splitscreen(vertical: bool) {
    let split = if vertical {
        vertical_layout()
    } else {
        horizontal_layout()
    };

    let monitors = get_monitors();
    let gamescope_by_monitor = get_gamescope_by_monitor();

    if gamescope_by_monitor.is_empty() {
        println!("[partydeck] hyprland::apply_splitscreen: no gamescope windows found");
        return;
    }

    for (monitor_id, window_addresses) in &gamescope_by_monitor {
        let player_count = window_addresses.len();
        let Some(layout) = split.for_count(player_count) else {
            println!(
                "[partydeck] hyprland::apply_splitscreen: skipping monitor {} count={}",
                monitor_id, player_count
            );
            continue;
        };
        let rect = monitors.get(monitor_id).copied().unwrap_or(MonitorRect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        });

        for (idx, address) in window_addresses.iter().enumerate() {
            let win = rect.window_rect(layout, idx);

            println!(
                "[partydeck] hyprland::apply_splitscreen: {} mon={} idx={}/{} pos={},{} size={}x{}",
                address, monitor_id, idx, player_count, win.x, win.y, win.width, win.height
            );

            let _ = Command::new("hyprctl")
                .args(["dispatch", "setfloating", &format!("address:{}", address)])
                .status();
            std::thread::sleep(std::time::Duration::from_millis(50));
            let _ = Command::new("hyprctl")
                .args([
                    "dispatch",
                    "movewindowpixel",
                    &format!("exact {} {},address:{}", win.x, win.y, address),
                ])
                .status();
            let _ = Command::new("hyprctl")
                .args([
                    "dispatch",
                    "resizewindowpixel",
                    &format!("exact {} {},address:{}", win.width, win.height, address),
                ])
                .status();
        }
    }
}
