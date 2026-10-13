//! Closes the HTTP requests a page left behind when it was navigated away from.
//!
//! The app's pages are separate documents. A stream opened through the HTTP plugin lives in the
//! webview's resource table on the Rust side, and nothing removes it when its page goes away: the
//! provider connection stays open with nobody reading it. An account that allows one connection
//! then has none left for whatever is played next.
//!
//! The page asks for a cancel as it unloads, but that message does not always get out, so this
//! settles it. What the plugin holds when a navigation starts belongs to the page being left;
//! it is closed once the next page has finished loading, when the old one is certainly gone.
//! Closing any earlier fails the old page's reads while it can still react by reconnecting.
//!
//! Known gap: a read that was waiting for data at the moment its page died can stay suspended
//! inside the plugin and keep the response alive after it is closed here. That connection then
//! sits unread until the provider drops it, as every left-behind stream did before this module.

use std::collections::HashMap;
use std::sync::Mutex;

use tauri::webview::PageLoadEvent;
use tauri::{Manager, ResourceId, Runtime, Webview};

const HTTP_PLUGIN_PREFIX: &str = "tauri_plugin_http::";

/// What each webview's outgoing page still had open, by webview label.
#[derive(Default)]
pub struct LeftBehind(Mutex<HashMap<String, Vec<ResourceId>>>);

fn belongs_to_http_plugin(resource_name: &str) -> bool {
    resource_name.starts_with(HTTP_PLUGIN_PREFIX)
}

fn open_http_resources<R: Runtime>(webview: &Webview<R>) -> Vec<ResourceId> {
    webview
        .resources_table()
        .names()
        .filter(|(_, name)| belongs_to_http_plugin(name))
        .map(|(rid, _)| rid)
        .collect()
}

pub fn on_page_load<R: Runtime>(webview: &Webview<R>, event: PageLoadEvent) {
    let state = webview.state::<LeftBehind>();
    let label = webview.label().to_string();
    match event {
        PageLoadEvent::Started => {
            let open = open_http_resources(webview);
            if let Ok(mut left_behind) = state.0.lock() {
                left_behind.insert(label, open);
            }
        }
        PageLoadEvent::Finished => {
            let stale = match state.0.lock() {
                Ok(mut left_behind) => left_behind.remove(&label).unwrap_or_default(),
                Err(_) => return,
            };
            // Checked again by name: an id that was closed meanwhile may have been handed out anew.
            let still_open = open_http_resources(webview);
            let mut table = webview.resources_table();
            let closed = stale
                .into_iter()
                .filter(|rid| still_open.contains(rid) && table.close(*rid).is_ok())
                .count();
            if closed > 0 {
                log::info!("[page-streams] closed {closed} request(s) the previous page left open");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_http_plugins_resources_are_closed() {
        assert!(belongs_to_http_plugin(
            "tauri_plugin_http::commands::ReqwestResponse"
        ));
        assert!(belongs_to_http_plugin(
            "tauri_plugin_http::commands::FetchRequest"
        ));
        assert!(!belongs_to_http_plugin("tauri_plugin_fs::file::StdFileResource"));
        assert!(!belongs_to_http_plugin("tauri::image::Image"));
        assert!(!belongs_to_http_plugin("my_tauri_plugin_http::thing"));
    }
}
