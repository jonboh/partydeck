use std::error::Error;
use std::io::BufRead;
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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

/// `instance_monitors` is the list of SDL display indices (one per instance,
/// in launch order) so that each gamescope window can be moved to its intended
/// Hyprland monitor as it opens.
pub fn hyprland_start(
    vertical: bool,
    instance_monitors: Vec<usize>,
) -> Result<HyprlandManager, Box<dyn Error>> {
    // Resolve socket path from environment variables.
    let xdg = std::env::var("XDG_RUNTIME_DIR").map_err(|_| "XDG_RUNTIME_DIR is not set")?;
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
        .map_err(|_| "HYPRLAND_INSTANCE_SIGNATURE is not set — is Hyprland running?")?;
    let socket_path = format!("{}/hypr/{}/.socket2.sock", xdg, sig);

    apply_decoration_rules();

    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_flag_thread = Arc::clone(&stop_flag);

    let thread = std::thread::spawn(move || {
        event_loop(&socket_path, vertical, instance_monitors, stop_flag_thread);
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

fn event_loop(
    socket_path: &str,
    vertical: bool,
    instance_monitors: Vec<usize>,
    stop: Arc<AtomicBool>,
) {
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

    // Counter for gamescope windows seen so far; used to map each new window
    // to the corresponding instance's target monitor in launch order.
    let mut gamescope_count: usize = 0;

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
            let parts: Vec<&str> = rest.splitn(4, ',').collect();
            // The openwindow event gives the address without a leading "0x",
            // but hyprctl's address: selector requires it.
            let raw_addr = parts.first().copied().unwrap_or("");
            let addr = if raw_addr.starts_with("0x") {
                raw_addr.to_string()
            } else {
                format!("0x{}", raw_addr)
            };
            let class = parts.get(2).copied().unwrap_or("");

            // Only react to gamescope windows; ignore all other openwindow events.
            let is_gamescope =
                class == "gamescope" || class == "gamescope-kbm" || class.starts_with(".gamescope");
            if !is_gamescope {
                continue;
            }

            println!(
                "[partydeck] hyprland::event_loop: openwindow gamescope addr={} class={}",
                addr, class
            );

            // Move this window to its intended Hyprland monitor before tiling.
            if let Some(&sdl_idx) = instance_monitors.get(gamescope_count) {
                move_window_to_sdl_monitor(&addr, sdl_idx);
            }
            gamescope_count += 1;

            // Small delay to allow Hyprland to finish placing the window in its
            // tiling layout (and complete the monitor move) before we override
            // the geometry.
            std::thread::sleep(std::time::Duration::from_millis(300));
            apply_splitscreen(vertical);
        }
    }

    println!("[partydeck] hyprland::event_loop: exiting");
}

/// Move a window (identified by `addr`) to the active workspace of the
/// Hyprland monitor that corresponds to `sdl_index` in SDL display order.
fn move_window_to_sdl_monitor(addr: &str, sdl_index: usize) {
    let hypr_monitors = get_hypr_monitors();
    let sdl_monitors = crate::monitor::get_monitors_errorless();

    let sdl_name = match sdl_monitors.get(sdl_index) {
        Some(m) => m.name().to_string(),
        None => {
            println!(
                "[partydeck] hyprland::move_window_to_sdl_monitor: SDL monitor index {} out of range",
                sdl_index
            );
            return;
        }
    };

    let hypr_mon = match hypr_monitors.iter().find(|m| m.name == sdl_name) {
        Some(m) => m,
        None => {
            println!(
                "[partydeck] hyprland::move_window_to_sdl_monitor: no Hyprland monitor named '{}' (sdl_index={})",
                sdl_name, sdl_index
            );
            return;
        }
    };

    // movetoworkspacesilent keeps the workspace invisible (no viewport jump)
    // and supports the address: window selector.
    let arg = format!("{},address:{}", hypr_mon.active_workspace_id, addr);
    println!(
        "[partydeck] hyprland::move_window_to_sdl_monitor: {} -> monitor '{}' workspace {}",
        addr, hypr_mon.name, hypr_mon.active_workspace_id
    );
    let _ = Command::new("hyprctl")
        .args(["dispatch", "movetoworkspacesilent", &arg])
        .status();
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

/// Rich monitor description as returned by `hyprctl -j monitors`.
#[derive(Debug, Clone)]
struct HyprMonitor {
    id: MonitorId,
    name: String,
    rect: MonitorRect,
    /// ID of the workspace currently displayed on this monitor.
    active_workspace_id: i64,
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

/// Returns all Hyprland monitors with their geometry and active workspace.
fn get_hypr_monitors() -> Vec<HyprMonitor> {
    let out = match Command::new("hyprctl").args(["-j", "monitors"]).output() {
        Ok(o) => o.stdout,
        Err(e) => {
            println!(
                "[partydeck] hyprland::get_hypr_monitors: hyprctl failed: {}",
                e
            );
            return vec![];
        }
    };
    let json: serde_json::Value = match serde_json::from_slice(&out) {
        Ok(v) => v,
        Err(e) => {
            println!(
                "[partydeck] hyprland::get_hypr_monitors: parse failed: {}",
                e
            );
            return vec![];
        }
    };
    let mut monitors = vec![];
    if let Some(arr) = json.as_array() {
        for m in arr {
            let id = MonitorId(m["id"].as_i64().unwrap_or(0));
            let name = m["name"].as_str().unwrap_or("").to_string();
            let rect = MonitorRect {
                x: m["x"].as_i64().unwrap_or(0),
                y: m["y"].as_i64().unwrap_or(0),
                width: m["width"].as_i64().unwrap_or(1920),
                height: m["height"].as_i64().unwrap_or(1080),
            };
            let active_workspace_id = m["activeWorkspace"]["id"].as_i64().unwrap_or(1);
            monitors.push(HyprMonitor {
                id,
                name,
                rect,
                active_workspace_id,
            });
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
            let mon_raw = window_entry["monitor"].as_i64().unwrap_or(-1);
            // Hyprland reports -1 for windows on workspaces not currently
            // assigned to any monitor (e.g. during startup).  Skip them — they
            // are not ready to be positioned yet and calling apply_splitscreen
            // for them would place them at wrong coordinates.
            if mon_raw < 0 {
                println!(
                    "[partydeck] hyprland::get_gamescope_by_monitor: skipping {} (monitor=-1, not yet mapped)",
                    addr
                );
                continue;
            }
            let mon = MonitorId(mon_raw);
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

    let hypr_monitors = get_hypr_monitors();
    let monitor_map: std::collections::HashMap<MonitorId, MonitorRect> =
        hypr_monitors.iter().map(|m| (m.id, m.rect)).collect();

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
        let rect = monitor_map.get(monitor_id).copied().unwrap_or(MonitorRect {
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
