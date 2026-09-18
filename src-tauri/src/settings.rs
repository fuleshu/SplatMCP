//! Window geometry persistence, following the reference app's `settings.json` pattern.
//!
//! The file lives next to `bridge.json` in the app data directory. Only the window's
//! normal geometry and its mode are stored: a maximized or fullscreen window keeps the
//! bounds it should return to, and a window left on a monitor that has since been
//! unplugged falls back to the configured default instead of restoring off-screen.

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::{Manager, WebviewWindow, WindowEvent};

use crate::paths;

/// Default window size, matching `tauri.conf.json`.
pub const DEFAULT_WIDTH: f64 = 1280.0;
pub const DEFAULT_HEIGHT: f64 = 820.0;
/// Bounds smaller than this are treated as unusable and replaced by the default.
const MIN_USABLE_WIDTH: f64 = 320.0;
const MIN_USABLE_HEIGHT: f64 = 240.0;
/// Modes a window can be restored into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowMode {
    #[default]
    Normal,
    Maximized,
    Fullscreen,
}

/// Geometry of a normal (neither maximized nor fullscreen) window.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Default for Bounds {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
        }
    }
}

impl Bounds {
    /// True when the values describe a window a user could actually see and use.
    ///
    /// A window saved while it straddled a monitor boundary, or on a monitor that is no
    /// longer attached, can end up with absurd or negative geometry.
    pub fn is_usable(&self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && self.width.is_finite()
            && self.height.is_finite()
            && self.width >= MIN_USABLE_WIDTH
            && self.height >= MIN_USABLE_HEIGHT
            && self.x.abs() < 32_000.0
            && self.y.abs() < 32_000.0
    }
}

/// Everything persisted between runs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// Schema version, so a future change can migrate instead of guessing.
    #[serde(default = "current_version")]
    pub version: u32,
    /// Bounds to restore a normal window to.
    #[serde(default)]
    pub bounds: Bounds,
    /// Mode the window was last in.
    #[serde(default)]
    pub mode: WindowMode,
}

fn current_version() -> u32 {
    1
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: current_version(),
            bounds: Bounds::default(),
            mode: WindowMode::Normal,
        }
    }
}

impl Settings {
    /// Replaces unusable values with the defaults and normalises the version.
    pub fn sanitized(mut self) -> Self {
        self.version = current_version();
        if !self.bounds.is_usable() {
            self.bounds = Bounds::default();
            // Off-screen bounds are only meaningful for a normal window.
            self.mode = WindowMode::Normal;
        }
        self
    }
}

/// Reads settings, falling back to the defaults for a missing or unreadable file.
pub fn load_file(path: &Path) -> Settings {
    let Ok(text) = fs::read_to_string(path) else {
        return Settings::default();
    };
    serde_json::from_str::<Settings>(&text)
        .unwrap_or_default()
        .sanitized()
}

/// Writes settings, creating the directory when needed.
pub fn save_file(path: &Path, settings: &Settings) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(&settings.sanitized())
        .map_err(|error| format!("could not encode settings: {error}"))?;
    fs::write(path, text).map_err(|error| format!("could not write {}: {error}", path.display()))
}

/// Shared settings plus the clock that keeps a resize burst from writing on every event.
pub struct WindowSettings {
    settings: Mutex<Settings>,
    path: std::path::PathBuf,
    last_saved: Mutex<Option<std::time::Instant>>,
}

/// Writes at most this often while the user drags or resizes the window.
const SAVE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(400);

impl WindowSettings {
    pub fn new(path: std::path::PathBuf) -> Self {
        Self {
            settings: Mutex::new(load_file(&path)),
            path,
            last_saved: Mutex::new(None),
        }
    }

    /// The settings as written on disk.
    pub fn snapshot(&self) -> Settings {
        self.settings.lock().map(|it| *it).unwrap_or_default()
    }

    /// The window state to restore at startup.
    pub fn startup(&self) -> Settings {
        self.snapshot().sanitized()
    }

    /// Records a new geometry, throttled while the window is being dragged.
    pub fn remember(&self, settings: Settings, force: bool) {
        {
            let Ok(mut last) = self.last_saved.lock() else {
                return;
            };
            if !force {
                if let Some(last) = *last {
                    if last.elapsed() < SAVE_INTERVAL {
                        return;
                    }
                }
            }
            *last = Some(std::time::Instant::now());
        }
        if let Ok(mut current) = self.settings.lock() {
            *current = settings.sanitized();
        }
        if let Err(error) = save_file(&self.path, &settings) {
            // Settings are a convenience: failing to write them must not break the app.
            eprintln!("splatmcp: could not save window settings: {error}");
        }
    }
}

/// Reads the current geometry from a window.
pub fn capture(window: &WebviewWindow) -> Settings {
    let mode = if window.is_fullscreen().unwrap_or(false) {
        WindowMode::Fullscreen
    } else if window.is_maximized().unwrap_or(false) {
        WindowMode::Maximized
    } else {
        WindowMode::Normal
    };

    let bounds = if mode == WindowMode::Normal {
        normal_bounds(window).unwrap_or_default()
    } else {
        // Keep the last normal bounds so leaving fullscreen has somewhere to go.
        normal_bounds(window).unwrap_or_default()
    };

    Settings {
        version: current_version(),
        bounds,
        mode,
    }
}

fn normal_bounds(window: &WebviewWindow) -> Option<Bounds> {
    let scale = window.scale_factor().unwrap_or(1.0);
    // Position and size are captured in different spaces on purpose: Tauri's
    // `set_position` places the window's outer top-left corner, while `set_size` sets the
    // *inner* size. Capturing both the same way they will be applied is what makes the
    // restored window land exactly where the user left it.
    let position = window.outer_position().ok()?;
    let size = window.inner_size().ok()?;
    Some(Bounds {
        x: position.x as f64 / scale,
        y: position.y as f64 / scale,
        width: size.width as f64 / scale,
        height: size.height as f64 / scale,
    })
}

/// Applies saved geometry to the window, if it describes something usable.
pub fn apply(window: &WebviewWindow, settings: &Settings) -> Result<(), String> {
    let settings = settings.sanitized();
    if settings.bounds != Bounds::default() || settings.mode != WindowMode::Normal {
        let bounds = settings.bounds;
        window
            .set_size(tauri::Size::Logical(tauri::LogicalSize {
                width: bounds.width,
                height: bounds.height,
            }))
            .map_err(|error| error.to_string())?;
        window
            .set_position(tauri::Position::Logical(tauri::LogicalPosition {
                x: bounds.x,
                y: bounds.y,
            }))
            .map_err(|error| error.to_string())?;
    }

    match settings.mode {
        WindowMode::Normal => {}
        WindowMode::Maximized => window.maximize().map_err(|error| error.to_string())?,
        WindowMode::Fullscreen => window
            .set_fullscreen(true)
            .map_err(|error| error.to_string())?,
    }
    Ok(())
}

/// Restores geometry at startup and follows the window from then on.
pub fn track(app: &tauri::AppHandle) -> Arc<WindowSettings> {
    let path = paths::settings_path()
        .unwrap_or_else(|_| std::env::temp_dir().join("splatmcp-settings.json"));
    let settings = Arc::new(WindowSettings::new(path));

    if let Some(window) = app.get_webview_window("main") {
        let startup = settings.startup();
        if let Err(error) = apply(&window, &startup) {
            eprintln!("splatmcp: could not restore the window geometry: {error}");
        }

        let tracked = window.clone();
        let settings_for_events = settings.clone();
        window.on_window_event(move |event| match event {
            WindowEvent::Moved(_) | WindowEvent::Resized(_) => {
                settings_for_events.remember(capture(&tracked), false);
            }
            WindowEvent::CloseRequested { .. } => {
                // The final geometry, written immediately.
                settings_for_events.remember(capture(&tracked), true);
            }
            _ => {}
        });
    }

    settings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("splatmcp-settings-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn settings_round_trip_through_the_file() {
        let path = temp_path("round-trip.json");
        let settings = Settings {
            version: 1,
            bounds: Bounds {
                x: 120.0,
                y: 80.0,
                width: 1024.0,
                height: 700.0,
            },
            mode: WindowMode::Maximized,
        };
        save_file(&path, &settings).unwrap();
        let loaded = load_file(&path);
        assert_eq!(loaded.bounds, settings.bounds);
        assert_eq!(loaded.mode, WindowMode::Maximized);

        // The file is human-readable JSON, like the reference app's.
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"width\": 1024.0"), "{text}");
        assert!(text.contains("\"mode\": \"maximized\""), "{text}");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn a_missing_or_broken_file_falls_back_to_the_default() {
        let missing = temp_path("missing.json");
        fs::remove_file(&missing).ok();
        assert_eq!(load_file(&missing), Settings::default());

        let broken = temp_path("broken.json");
        fs::write(&broken, "{ not json at all").unwrap();
        assert_eq!(load_file(&broken), Settings::default());

        // A file from an older schema still loads; missing fields take defaults.
        let partial = temp_path("partial.json");
        fs::write(
            &partial,
            "{\"bounds\":{\"x\":10,\"y\":10,\"width\":800,\"height\":600}}",
        )
        .unwrap();
        let loaded = load_file(&partial);
        assert_eq!(loaded.mode, WindowMode::Normal);
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.bounds.width, 800.0);
        fs::remove_file(&partial).ok();
        fs::remove_file(&broken).ok();
    }

    #[test]
    fn implausible_bounds_are_replaced() {
        let cases = [
            Bounds {
                x: 0.0,
                y: 0.0,
                width: 10.0,
                height: 10.0,
            },
            Bounds {
                x: 0.0,
                y: 0.0,
                width: f64::NEG_INFINITY,
                height: 600.0,
            },
            Bounds {
                x: 40_000.0,
                y: 0.0,
                width: 800.0,
                height: 600.0,
            },
        ];
        for bounds in cases {
            let settings = Settings {
                version: 1,
                bounds,
                mode: WindowMode::Fullscreen,
            }
            .sanitized();
            assert_eq!(settings.bounds, Bounds::default(), "{bounds:?}");
            // A window that cannot be placed must come back in normal mode, otherwise
            // the user would see nothing at all.
            assert_eq!(settings.mode, WindowMode::Normal, "{bounds:?}");
        }

        let good = Bounds {
            x: -1200.0,
            y: 40.0,
            width: 640.0,
            height: 480.0,
        };
        assert!(good.is_usable());
        assert_eq!(
            Settings {
                version: 0,
                bounds: good,
                mode: WindowMode::Maximized
            }
            .sanitized()
            .bounds,
            good
        );
    }

    #[test]
    fn the_version_is_normalised_on_load() {
        let path = temp_path("old-version.json");
        fs::write(
            &path,
            "{\"version\":0,\"bounds\":{\"x\":1,\"y\":2,\"width\":900,\"height\":700},\"mode\":\"normal\"}",
        )
        .unwrap();
        assert_eq!(load_file(&path).version, 1);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn setting_a_geometry_is_throttled_unless_forced() {
        let path = temp_path("throttle.json");
        let settings = WindowSettings::new(path.clone());
        let first = Settings {
            version: 1,
            bounds: Bounds {
                x: 10.0,
                y: 10.0,
                width: 800.0,
                height: 600.0,
            },
            mode: WindowMode::Normal,
        };
        settings.remember(first, false);
        assert_eq!(settings.snapshot().bounds.x, 10.0);

        // A second unforced write inside the interval is dropped...
        let second = Settings {
            bounds: Bounds {
                x: 500.0,
                ..first.bounds
            },
            ..first
        };
        settings.remember(second, false);
        assert_eq!(
            settings.snapshot().bounds.x,
            10.0,
            "throttled write should be skipped"
        );

        // ...but a forced one (window closed) always lands.
        settings.remember(second, true);
        assert_eq!(settings.snapshot().bounds.x, 500.0);
        assert_eq!(load_file(&path).bounds.x, 500.0);
        fs::remove_file(&path).ok();
    }
}
