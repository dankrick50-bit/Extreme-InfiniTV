// Which graphics card the app's window draws with (Windows laptops with two).
//
// The window normally lands on the power-saving chip. NVIDIA's RTX Video Super Resolution, which
// upscales video on the card itself, only works for an app drawing on the NVIDIA card, so the
// player can be asked to use it. The choice has to be known before the window exists, so it lives
// in a small file read at startup and takes effect on the next start.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::Manager;

const FILE_NAME: &str = "kd-graphics.json";
const WEBVIEW_ARGS_VAR: &str = "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS";
const HIGH_PERFORMANCE_ARG: &str = "--force_high_performance_gpu";

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Preference {
    #[serde(default)]
    high_performance: bool,
}

/// Off unless the file clearly says otherwise: a missing or damaged file changes nothing.
fn read(config_dir: &Path) -> bool {
    std::fs::read_to_string(config_dir.join(FILE_NAME))
        .ok()
        .and_then(|contents| serde_json::from_str::<Preference>(&contents).ok())
        .is_some_and(|preference| preference.high_performance)
}

fn write(config_dir: &Path, high_performance: bool) -> std::io::Result<()> {
    std::fs::create_dir_all(config_dir)?;
    let contents = serde_json::to_string(&Preference { high_performance })
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    std::fs::write(config_dir.join(FILE_NAME), contents)
}

/// Adds our switch to whatever is already being passed to the WebView, once.
fn with_high_performance(existing: Option<String>) -> String {
    match existing.map(|value| value.trim().to_string()).filter(|value| !value.is_empty()) {
        Some(value) if value.split_whitespace().any(|argument| argument == HIGH_PERFORMANCE_ARG) => value,
        Some(value) => format!("{value} {HIGH_PERFORMANCE_ARG}"),
        None => HIGH_PERFORMANCE_ARG.to_string(),
    }
}

/// Where Tauri keeps this app's config on Windows, worked out without an app handle.
fn startup_config_dir(identifier: &str) -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|roaming| PathBuf::from(roaming).join(identifier))
}

/// Must run before `tauri::Builder`: the WebView reads its switches when the window is created.
pub fn initialize(identifier: &str) {
    if !cfg!(windows) {
        return;
    }
    let Some(config_dir) = startup_config_dir(identifier) else {
        return;
    };
    if !read(&config_dir) {
        return;
    }
    std::env::set_var(WEBVIEW_ARGS_VAR, with_high_performance(std::env::var(WEBVIEW_ARGS_VAR).ok()));
    log::info!("[graphics] the window is asked to draw with the high-performance graphics card");
}

/// Whether the high-performance card is asked for (from the next start on).
#[tauri::command]
pub fn graphics_pref_get(app: tauri::AppHandle) -> bool {
    app.path().app_config_dir().map(|config_dir| read(&config_dir)).unwrap_or(false)
}

#[tauri::command]
pub fn graphics_pref_set(app: tauri::AppHandle, high_performance: bool) -> Result<(), String> {
    let config_dir = app.path().app_config_dir().map_err(|error| format!("OTHER:{error}"))?;
    write(&config_dir, high_performance).map_err(|error| format!("OTHER:{error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("kd-graphics-{tag}-{}", std::process::id()))
    }

    #[test]
    fn the_choice_round_trips_and_defaults_to_off() {
        let dir = temp_dir("roundtrip");
        assert!(!read(&dir), "no file means the power-saving default");
        write(&dir, true).unwrap();
        assert!(read(&dir));
        write(&dir, false).unwrap();
        assert!(!read(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_file_changes_nothing() {
        let dir = temp_dir("damaged");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(FILE_NAME), "not json").unwrap();
        assert!(!read(&dir));
        std::fs::write(dir.join(FILE_NAME), "{}").unwrap();
        assert!(!read(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_switch_is_added_once_and_keeps_other_switches() {
        assert_eq!(with_high_performance(None), HIGH_PERFORMANCE_ARG);
        assert_eq!(with_high_performance(Some("  ".to_string())), HIGH_PERFORMANCE_ARG);
        assert_eq!(
            with_high_performance(Some("--remote-debugging-port=9333".to_string())),
            format!("--remote-debugging-port=9333 {HIGH_PERFORMANCE_ARG}")
        );
        let already = format!("--x {HIGH_PERFORMANCE_ARG}");
        assert_eq!(with_high_performance(Some(already.clone())), already);
    }
}
