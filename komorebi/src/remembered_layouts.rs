//! Remembers layouts changed at runtime and restores them on the next start.
//!
//! Enabled with `"remember_layouts": true` in komorebi.json. Layouts are stored per monitor
//! (device id) and workspace (name, or index for unnamed workspaces) in
//! `%LOCALAPPDATA%\komorebi\remembered_layouts.json`. Delete that file to forget them.
//! komorebi.json itself is never modified.
//!
//! Added in the komotype fork (https://github.com/northkey-dev/komorebi).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use crate::DATA_DIR;
use crate::core::DefaultLayout;
use crate::core::Layout;
use crate::window_manager::WindowManager;

pub static ENABLED: AtomicBool = AtomicBool::new(false);

/// Monitor device id → workspace key → layout.
type Remembered = HashMap<String, HashMap<String, DefaultLayout>>;

fn path() -> PathBuf {
    DATA_DIR.join("remembered_layouts.json")
}

fn workspace_key(name: Option<&String>, idx: usize) -> String {
    name.cloned().unwrap_or_else(|| format!("#{idx}"))
}

fn read() -> Remembered {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

/// Stores the current default layouts of all workspaces. Call after a layout change.
pub fn save(wm: &WindowManager) {
    if !ENABLED.load(Ordering::SeqCst) {
        return;
    }
    let mut remembered = read();
    for monitor in wm.monitors() {
        let entry = remembered.entry(monitor.device_id.clone()).or_default();
        for (idx, ws) in monitor.workspaces().iter().enumerate() {
            if let Layout::Default(layout) = ws.layout {
                entry.insert(workspace_key(ws.name.as_ref(), idx), layout);
            }
        }
    }
    match serde_json::to_string_pretty(&remembered) {
        Ok(json) => {
            if let Err(error) = std::fs::write(path(), json) {
                tracing::warn!("cannot save remembered layouts: {error}");
            }
        }
        Err(error) => tracing::warn!("cannot serialize remembered layouts: {error}"),
    }
}

/// Applies remembered layouts over those from the static config.
pub fn apply(wm: &mut WindowManager) {
    if !ENABLED.load(Ordering::SeqCst) {
        return;
    }
    let remembered = read();
    for monitor in wm.monitors_mut() {
        let Some(layouts) = remembered.get(&monitor.device_id) else {
            continue;
        };
        for (idx, ws) in monitor.workspaces_mut().iter_mut().enumerate() {
            if let Some(layout) = layouts.get(&workspace_key(ws.name.as_ref(), idx))
                && matches!(ws.layout, Layout::Default(_))
            {
                ws.layout = Layout::Default(*layout);
            }
        }
    }
    tracing::info!("applied remembered layouts");
}
