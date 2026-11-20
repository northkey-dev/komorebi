//! komotype widget: typing stats from the komotype daemon (`komotyped`).
//!
//! The daemon does all the tracking; this widget only reads [`Status`] lines from its named pipe
//! and renders them. See <https://github.com/northkey-dev/komotype>.

use crate::render::RenderConfig;
use crate::selected_frame::SelectableFrame;
use crate::widgets::widget::BarWidget;
use eframe::egui::Align;
use eframe::egui::CentralPanel;
use eframe::egui::Color32;
use eframe::egui::Context;
use eframe::egui::Frame;
use eframe::egui::Grid;
use eframe::egui::Label;
use eframe::egui::Margin;
use eframe::egui::Pos2;
use eframe::egui::RichText;
use eframe::egui::TextFormat;
use eframe::egui::Ui;
use eframe::egui::ViewportBuilder;
use eframe::egui::ViewportCommand;
use eframe::egui::ViewportId;
use eframe::egui::WindowLevel;
use eframe::egui::text::LayoutJob;
use komotype_protocol::ipc::IPC_VERSION;
use komotype_protocol::ipc::PIPE_NAME;
use komotype_protocol::ipc::Status;
use komotype_protocol::ipc::Transport;
use parking_lot::Mutex;
use serde::Deserialize;
use serde::Serialize;
use std::io::BufRead;
use std::io::BufReader;
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

/// The daemon sends a line at least every 5 s; older data means it is gone.
const STALE_AFTER: Duration = Duration::from_secs(12);
const RECONNECT_INTERVAL: Duration = Duration::from_secs(2);
const AUTOSTART_INTERVAL: Duration = Duration::from_secs(30);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const POPUP_SIZE: [f32; 2] = [380.0, 330.0];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
/// komotype widget configuration (typing stats from the komotype daemon)
pub struct KomotypeConfig {
    /// Enable the komotype widget
    pub enable: bool,
    /// Show live words per minute
    #[cfg_attr(feature = "schemars", schemars(extend("default" = true)))]
    pub show_wpm: Option<bool>,
    /// Show words typed today
    #[cfg_attr(feature = "schemars", schemars(extend("default" = true)))]
    pub show_words_today: Option<bool>,
    /// Show the active keyboard layer (needs the komotype ZMK module)
    #[cfg_attr(feature = "schemars", schemars(extend("default" = true)))]
    pub show_layer: Option<bool>,
    /// Show the keyboard battery (needs the komotype ZMK module)
    #[cfg_attr(feature = "schemars", schemars(extend("default" = true)))]
    pub show_battery: Option<bool>,
    /// Names for keyboard layers, by index. Layers without a name are shown as "L<n>".
    pub layer_names: Option<Vec<String>>,
    /// Start komotyped automatically if it is not running
    #[cfg_attr(feature = "schemars", schemars(extend("default" = true)))]
    pub autostart: Option<bool>,
    /// Path to komotyped.exe (default: found via PATH)
    pub daemon_path: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Today,
    Keyboard,
    Achievements,
}

#[derive(Default)]
struct Shared {
    status: Option<Status>,
    updated: Option<Instant>,
    error: Option<String>,
}

pub struct Komotype {
    config: KomotypeConfig,
    shared: Arc<Mutex<Shared>>,
    started: bool,
    popup_open: bool,
    popup_anchor: Pos2,
    popup_was_focused: bool,
    tab: Tab,
}

impl From<&KomotypeConfig> for Komotype {
    fn from(config: &KomotypeConfig) -> Self {
        Self {
            config: config.clone(),
            shared: Arc::default(),
            started: false,
            popup_open: false,
            popup_anchor: Pos2::ZERO,
            popup_was_focused: false,
            tab: Tab::Today,
        }
    }
}

fn client_loop(shared: Arc<Mutex<Shared>>, ctx: Context, autostart: bool, daemon: String) {
    let mut last_spawn: Option<Instant> = None;
    loop {
        match std::fs::OpenOptions::new().read(true).open(PIPE_NAME) {
            Ok(pipe) => {
                for line in BufReader::new(pipe).lines() {
                    let Ok(line) = line else { break };
                    let mut s = shared.lock();
                    match serde_json::from_str::<Status>(&line) {
                        Ok(status) if status.ipc_version == IPC_VERSION => {
                            s.status = Some(status);
                            s.updated = Some(Instant::now());
                            s.error = None;
                        }
                        Ok(status) => {
                            s.error = Some(format!(
                                "komotyped speaks IPC v{}, widget expects v{IPC_VERSION}. Update both.",
                                status.ipc_version
                            ));
                        }
                        Err(e) => s.error = Some(format!("unreadable status: {e}")),
                    }
                    drop(s);
                    ctx.request_repaint();
                }
                shared.lock().status = None;
                ctx.request_repaint();
            }
            Err(_) => {
                if autostart && last_spawn.is_none_or(|t| t.elapsed() >= AUTOSTART_INTERVAL) {
                    last_spawn = Some(Instant::now());
                    if let Err(e) = Command::new(&daemon)
                        .creation_flags(CREATE_NO_WINDOW)
                        .spawn()
                    {
                        shared.lock().error = Some(format!("cannot start {daemon}: {e}"));
                        ctx.request_repaint();
                    }
                }
            }
        }
        std::thread::sleep(RECONNECT_INTERVAL);
    }
}

fn pct(value: Option<f32>) -> String {
    value.map_or("–".into(), |v| format!("{:.0}%", v * 100.0))
}

fn wpm_text(value: Option<f32>) -> String {
    value.map_or("–".into(), |v| format!("{v:.0}"))
}

fn duration_text(seconds: u32) -> String {
    let (h, m) = (seconds / 3600, seconds / 60 % 60);
    if h > 0 {
        format!("{h} h {m:02} min")
    } else {
        format!("{m} min")
    }
}

fn battery_icon(level: u8) -> &'static str {
    match level {
        76.. => egui_phosphor::regular::BATTERY_FULL,
        51.. => egui_phosphor::regular::BATTERY_HIGH,
        26.. => egui_phosphor::regular::BATTERY_MEDIUM,
        11.. => egui_phosphor::regular::BATTERY_LOW,
        _ => egui_phosphor::regular::BATTERY_WARNING,
    }
}

impl Komotype {
    fn ensure_started(&mut self, ctx: &Context) {
        if self.started {
            return;
        }
        self.started = true;
        let shared = self.shared.clone();
        let ctx = ctx.clone();
        let autostart = self.config.autostart.unwrap_or(true);
        let daemon = self
            .config
            .daemon_path
            .clone()
            .unwrap_or_else(|| "komotyped.exe".into());
        std::thread::Builder::new()
            .name("komotype-client".into())
            .spawn(move || client_loop(shared, ctx, autostart, daemon))
            .ok();
    }

    /// Current status, or `None` when the daemon is not reachable.
    fn status(&self) -> (Option<Status>, Option<String>) {
        let s = self.shared.lock();
        let fresh = s.updated.is_some_and(|t| t.elapsed() < STALE_AFTER);
        (s.status.clone().filter(|_| fresh), s.error.clone())
    }

    fn layer_name(&self, layer: u8) -> String {
        self.config
            .layer_names
            .as_ref()
            .and_then(|names| names.get(layer as usize))
            .cloned()
            .unwrap_or_else(|| format!("L{layer}"))
    }

    fn bar_label(&self, ctx: &Context, config: &RenderConfig, status: Option<&Status>) -> LayoutJob {
        let visuals = &ctx.style().visuals;
        let text = visuals.text_color();
        let weak = visuals.weak_text_color();
        let accent = visuals.selection.stroke.color;

        let mut job = LayoutJob::simple(
            egui_phosphor::regular::KEYBOARD.to_string(),
            config.icon_font_id.clone(),
            if status.is_some() { accent } else { weak },
            100.0,
        );
        let mut append = |s: &str, color: Color32| {
            job.append(
                s,
                8.0,
                TextFormat {
                    font_id: config.text_font_id.clone(),
                    color,
                    valign: Align::Center,
                    ..Default::default()
                },
            );
        };

        let Some(status) = status else {
            append("–", weak);
            return job;
        };
        let t = &status.typing;

        if self.config.show_wpm.unwrap_or(true) {
            // Highlight when typing near today's best: a nudge to push for a new record.
            let near_best = t.best_wpm_today.is_some_and(|b| t.wpm_live >= b * 0.9);
            let color = match t.wpm_live {
                w if w < 1.0 => weak,
                _ if near_best => accent,
                _ => text,
            };
            append(&format!("{:.0} WPM", t.wpm_live), color);
        }
        let firmware = status.firmware.as_ref();
        if let Some(fw) = firmware
            && self.config.show_layer.unwrap_or(true)
        {
            // Base layer in a quiet color, any other layer stands out.
            let color = if fw.report.layer == 0 { text } else { accent };
            append(
                &format!("{} {}", egui_phosphor::regular::STACK, self.layer_name(fw.report.layer)),
                color,
            );
        }
        if self.config.show_words_today.unwrap_or(true) {
            append(&format!("{} words", t.words_today), weak);
        }
        if let Some(fw) = firmware {
            if self.config.show_battery.unwrap_or(true) {
                let levels: Vec<u8> = std::iter::once(fw.report.battery_central)
                    .chain(fw.report.battery_peripherals.iter().copied())
                    .flatten()
                    .collect();
                if let Some(&min) = levels.iter().min() {
                    let joined = levels.iter().map(|l| format!("{l}")).collect::<Vec<_>>().join("·");
                    let color = if min <= 20 { visuals.warn_fg_color } else { weak };
                    append(&format!("{} {joined}%", battery_icon(min)), color);
                }
            }
        }
        job
    }

    fn tooltip(&self, ui: &mut Ui, status: Option<&Status>, error: Option<&str>) {
        let Some(s) = status else {
            ui.label("komotyped is not running.");
            if let Some(e) = error {
                ui.label(RichText::new(e).weak());
            }
            return;
        };
        let t = &s.typing;
        Grid::new("komotype-tooltip").num_columns(2).spacing([16.0, 2.0]).show(ui, |ui| {
            ui.label("WPM now");
            ui.label(format!("{:.0}", t.wpm_live));
            ui.end_row();
            ui.label("Best today");
            ui.label(wpm_text(t.best_wpm_today));
            ui.end_row();
            ui.label("Words today");
            ui.label(t.words_today.to_string());
            ui.end_row();
            ui.label("Accuracy");
            ui.label(pct(t.accuracy_today));
            ui.end_row();
        });
        ui.label(RichText::new("Click for details").weak().small());
    }

    fn popup_contents(&mut self, ui: &mut Ui, status: Option<&Status>, error: Option<&str>) {
        ui.heading("komotype");
        let Some(s) = status else {
            ui.label("komotyped is not running.");
            if let Some(e) = error {
                ui.label(RichText::new(e).weak());
            }
            return;
        };
        let keyboard = s.main_keyboard.as_deref().unwrap_or("no main keyboard (run `komotyped pick`)");
        let connection = if s.main_connected { "connected" } else { "not connected" };
        ui.label(RichText::new(format!("{keyboard} · {connection}")).weak());
        ui.add_space(6.0);

        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Today, "Today");
            ui.selectable_value(&mut self.tab, Tab::Keyboard, "Keyboard");
            ui.selectable_value(&mut self.tab, Tab::Achievements, "Achievements");
        });
        ui.separator();
        ui.add_space(4.0);

        match self.tab {
            Tab::Today => Self::today_tab(ui, s),
            Tab::Keyboard => self.keyboard_tab(ui, s),
            Tab::Achievements => {
                ui.label(RichText::new("0 / ?? discovered").strong());
                ui.add_space(6.0);
                ui.label("Achievements are not built yet.");
                ui.label(RichText::new("They arrive with levels and quests in a later update.").weak());
            }
        }
    }

    fn today_tab(ui: &mut Ui, s: &Status) {
        let t = &s.typing;
        Grid::new("komotype-today").num_columns(2).spacing([24.0, 4.0]).show(ui, |ui| {
            let mut row = |k: &str, v: String| {
                ui.label(k);
                ui.label(v);
                ui.end_row();
            };
            row("WPM now", format!("{:.0}", t.wpm_live));
            row("Current burst", wpm_text(t.wpm_burst));
            row("Best burst today", wpm_text(t.best_wpm_today));
            row("Best ever", wpm_text(t.best_wpm_ever));
            row("Words", t.words_today.to_string());
            row("Accuracy", pct(t.accuracy_today));
            row("Active typing", duration_text(t.active_seconds_today));
            row("Main keyboard share", pct(s.main_share_today));
        });
    }

    fn keyboard_tab(&self, ui: &mut Ui, s: &Status) {
        let Some(fw) = &s.firmware else {
            ui.label("No data from the keyboard firmware.");
            ui.label(
                RichText::new("Layer and battery need the komotype ZMK module on the keyboard.").weak(),
            );
            return;
        };
        {
            Grid::new("komotype-keyboard").num_columns(2).spacing([24.0, 4.0]).show(ui, |ui| {
                let bat = |b: Option<u8>| b.map_or("?".into(), |b| format!("{b}%"));
                ui.label("Layer");
                ui.label(self.layer_name(fw.report.layer));
                ui.end_row();
                ui.label("Battery");
                let mut levels = vec![bat(fw.report.battery_central)];
                levels.extend(fw.report.battery_peripherals.iter().map(|b| bat(*b)));
                ui.label(levels.join(" / "));
                ui.end_row();
                ui.label("Connection");
                ui.label(match fw.transport {
                    Transport::Usb => "USB",
                    Transport::Bluetooth => "Bluetooth",
                    Transport::Other => "other",
                });
                ui.end_row();
            });
        }
    }

    fn show_popup(&mut self, ctx: &Context, status: Option<&Status>, error: Option<&str>) {
        let mut close = false;
        let builder = ViewportBuilder::default()
            .with_title("komotype")
            .with_decorations(false)
            .with_taskbar(false)
            .with_resizable(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_inner_size(POPUP_SIZE)
            .with_position(self.popup_anchor);
        ctx.show_viewport_immediate(ViewportId::from_hash_of("komotype-popup"), builder, |ctx, _| {
            CentralPanel::default()
                .frame(Frame::window(&ctx.style()).inner_margin(Margin::same(14)))
                .show(ctx, |ui| self.popup_contents(ui, status, error));
            let (focused, escape, close_requested) = ctx.input(|i| {
                (
                    i.viewport().focused,
                    i.key_pressed(eframe::egui::Key::Escape),
                    i.viewport().close_requested(),
                )
            });
            // Close when the user clicks elsewhere, like a menu.
            if focused == Some(true) {
                self.popup_was_focused = true;
            } else if focused == Some(false) && self.popup_was_focused {
                close = true;
            }
            if escape || close_requested {
                close = true;
            }
        });
        if close {
            self.popup_open = false;
        }
    }

    /// Opens the popup below the widget, or above it if the bar sits at the bottom of the screen.
    fn place_popup(&mut self, ctx: &Context, widget: eframe::egui::Rect) {
        let (outer, monitor) = ctx.input(|i| (i.viewport().outer_rect, i.viewport().monitor_size));
        let origin = outer.map_or(Pos2::ZERO, |r| r.min);
        let mut x = origin.x + widget.right() - POPUP_SIZE[0];
        let mut y = origin.y + widget.bottom() + 6.0;
        if let Some(screen) = monitor {
            if y + POPUP_SIZE[1] > screen.y {
                y = origin.y + widget.top() - POPUP_SIZE[1] - 6.0;
            }
            x = x.clamp(0.0, (screen.x - POPUP_SIZE[0]).max(0.0));
        }
        self.popup_anchor = Pos2::new(x.max(0.0), y.max(0.0));
        self.popup_was_focused = false;
    }
}

impl BarWidget for Komotype {
    fn render(&mut self, ctx: &Context, ui: &mut Ui, config: &mut RenderConfig) {
        if !self.config.enable {
            return;
        }
        self.ensure_started(ctx);
        let (status, error) = self.status();
        let label = self.bar_label(ctx, config, status.as_ref());

        let response = config.apply_on_widget(false, ui, |ui| {
            SelectableFrame::new(self.popup_open)
                .show(ui, |ui| {
                    // Breathing room so the highlight frame does not hug the text.
                    Frame::NONE
                        .inner_margin(Margin::symmetric(4, 3))
                        .show(ui, |ui| ui.add(Label::new(label).selectable(false)))
                })
                .on_hover_ui(|ui| self.tooltip(ui, status.as_ref(), error.as_deref()))
        });

        if response.inner.clicked() {
            self.popup_open = !self.popup_open;
            if self.popup_open {
                self.place_popup(ctx, response.inner.rect);
            }
        }
        if self.popup_open {
            self.show_popup(ctx, status.as_ref(), error.as_deref());
            if self.popup_open && !self.popup_was_focused {
                ctx.send_viewport_cmd_to(ViewportId::from_hash_of("komotype-popup"), ViewportCommand::Focus);
            }
        }
    }
}
