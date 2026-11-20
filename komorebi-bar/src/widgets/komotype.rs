//! komotype widget: typing stats and progression from the komotype daemon (`komotyped`).
//!
//! The daemon does all the tracking; this widget only reads [`Status`] lines from its named pipe
//! and renders them. See <https://github.com/northkey-dev/komotype>.

use crate::render::RenderConfig;
use crate::selected_frame::SelectableFrame;
use crate::widgets::widget::BarWidget;
use eframe::egui::Align;
use eframe::egui::Button;
use eframe::egui::CentralPanel;
use eframe::egui::Color32;
use eframe::egui::Context;
use eframe::egui::CornerRadius;
use eframe::egui::Frame;
use eframe::egui::Grid;
use eframe::egui::Label;
use eframe::egui::Margin;
use eframe::egui::Pos2;
use eframe::egui::ProgressBar;
use eframe::egui::Rect;
use eframe::egui::RichText;
use eframe::egui::ScrollArea;
use eframe::egui::Sense;
use eframe::egui::TextFormat;
use eframe::egui::Ui;
use eframe::egui::Vec2;
use eframe::egui::ViewportBuilder;
use eframe::egui::ViewportCommand;
use eframe::egui::ViewportId;
use eframe::egui::WindowLevel;
use eframe::egui::text::LayoutJob;
use komotype_protocol::ipc::Command as DaemonCommand;
use komotype_protocol::ipc::Game;
use komotype_protocol::ipc::GameEventKind;
use komotype_protocol::ipc::IPC_VERSION;
use komotype_protocol::ipc::PIPE_NAME;
use komotype_protocol::ipc::Quest;
use komotype_protocol::ipc::Status;
use komotype_protocol::ipc::Transport;
use parking_lot::Mutex;
use serde::Deserialize;
use serde::Serialize;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// The daemon sends a line at least every 5 s; older data means it is gone.
const STALE_AFTER: Duration = Duration::from_secs(12);
const RECONNECT_INTERVAL: Duration = Duration::from_secs(2);
const AUTOSTART_INTERVAL: Duration = Duration::from_secs(30);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const POPUP_SIZE: [f32; 2] = [420.0, 500.0];
/// How long a fresh event (level-up, quest) is announced in the bar.
const FLASH_MS: i64 = 8_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
/// komotype widget configuration (typing stats from the komotype daemon)
pub struct KomotypeConfig {
    /// Enable the komotype widget
    pub enable: bool,
    /// Show live words per minute
    #[cfg_attr(feature = "schemars", schemars(extend("default" = true)))]
    pub show_wpm: Option<bool>,
    /// Show the active keyboard layer (needs the komotype ZMK module)
    #[cfg_attr(feature = "schemars", schemars(extend("default" = true)))]
    pub show_layer: Option<bool>,
    /// Show level and XP progress
    #[cfg_attr(feature = "schemars", schemars(extend("default" = true)))]
    pub show_level: Option<bool>,
    /// Show the loyalty XP multiplier
    #[cfg_attr(feature = "schemars", schemars(extend("default" = true)))]
    pub show_loyalty: Option<bool>,
    /// Show words typed today
    #[cfg_attr(feature = "schemars", schemars(extend("default" = true)))]
    pub show_words_today: Option<bool>,
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
    Progress,
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
                        Err(e) => s.error = Some(format!("unreadable status (update komotyped?): {e}")),
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

/// Sends a command over a separate, short-lived pipe connection (off the UI thread).
fn send_command(command: DaemonCommand) {
    std::thread::spawn(move || {
        let result = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(PIPE_NAME)
            .and_then(|mut pipe| {
                let mut line = serde_json::to_vec(&command).unwrap_or_default();
                line.push(b'\n');
                pipe.write_all(&line)
            });
        if let Err(e) = result {
            tracing::warn!("komotype: sending command failed: {e}");
        }
    });
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
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

fn ago_text(at_ms: i64) -> String {
    let minutes = (now_ms() - at_ms).max(0) / 60_000;
    match minutes {
        0 => "just now".into(),
        1..=59 => format!("{minutes} min ago"),
        60..=1439 => format!("{} h ago", minutes / 60),
        _ => format!("{} d ago", minutes / 1440),
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

fn level_fraction(g: &Game) -> f32 {
    if g.level_xp_needed == 0 {
        1.0
    } else {
        g.level_xp as f32 / g.level_xp_needed as f32
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

    fn quest_title(&self, title: &str, layer: Option<u8>) -> String {
        match layer {
            Some(l) => title.replace("{layer}", &self.layer_name(l)),
            None => title.to_string(),
        }
    }

    fn event_text(&self, kind: &GameEventKind) -> String {
        use egui_phosphor::regular as icon;
        match kind {
            GameEventKind::LevelUp { level } => format!("{} Level {level}!", icon::ARROW_FAT_UP),
            GameEventKind::TitleUnlocked { title } => format!("{} New title: {title}", icon::CROWN),
            GameEventKind::QuestCompleted { title, layer, xp } => {
                format!("{} {} +{xp} XP", icon::CHECK_CIRCLE, self.quest_title(title, *layer))
            }
            GameEventKind::WeeklyCompleted { title, xp } => {
                format!("{} Weekly: {title} +{xp} XP", icon::TROPHY)
            }
            GameEventKind::StreakExtended { days, xp } => {
                format!("{} {days}-day streak +{xp} XP", icon::FIRE)
            }
            GameEventKind::StreakFrozen { days } => {
                format!("{} Streak of {days} days saved by a freeze", icon::SNOWFLAKE)
            }
            GameEventKind::StreakLost { days } => format!("{} Streak of {days} days lost", icon::HEART_BREAK),
            GameEventKind::LoyaltyChanged { from, to } => {
                format!("{} Loyalty ×{from:.1} → ×{to:.1}", icon::HANDSHAKE)
            }
            GameEventKind::NewBestWpm { wpm } => format!("{} New best: {wpm:.0} WPM", icon::LIGHTNING),
            GameEventKind::SeasonStarted { season } => format!("{} Season {season} started", icon::LEAF),
            GameEventKind::Prestige { prestige } => format!("{} Prestige {prestige}!", icon::STAR),
        }
    }

    /// The newest event, if it is fresh enough to announce in the bar.
    fn flash(&self, status: &Status) -> Option<String> {
        let event = status.game.as_ref()?.events.first()?;
        (now_ms() - event.at_ms < FLASH_MS).then(|| self.event_text(&event.kind))
    }

    fn text_job(config: &RenderConfig) -> impl Fn(&mut LayoutJob, &str, Color32) {
        let font = config.text_font_id.clone();
        move |job: &mut LayoutJob, s: &str, color: Color32| {
            let leading = if job.text.is_empty() { 0.0 } else { 8.0 };
            job.append(
                s,
                leading,
                TextFormat {
                    font_id: font.clone(),
                    color,
                    valign: Align::Center,
                    ..Default::default()
                },
            );
        }
    }

    /// Bar content: two labels around a small XP bar.
    fn bar_contents(&self, ui: &mut Ui, config: &RenderConfig, status: Option<&Status>) {
        let visuals = ui.style().visuals.clone();
        let (text, weak, accent) = (
            visuals.text_color(),
            visuals.weak_text_color(),
            visuals.selection.stroke.color,
        );
        let append = Self::text_job(config);

        let mut left = LayoutJob::simple(
            egui_phosphor::regular::KEYBOARD.to_string(),
            config.icon_font_id.clone(),
            if status.is_some() { accent } else { weak },
            100.0,
        );
        let Some(status) = status else {
            append(&mut left, " –", weak);
            ui.add(Label::new(left).selectable(false));
            return;
        };
        let t = &status.typing;

        if let Some(flash) = self.flash(status) {
            append(&mut left, &flash, accent);
            ui.add(Label::new(left).selectable(false));
            return;
        }

        if self.config.show_wpm.unwrap_or(true) {
            // Highlight when typing near today's best: a nudge to push for a new record.
            let near_best = t.best_wpm_today.is_some_and(|b| t.wpm_live >= b * 0.9);
            let color = match t.wpm_live {
                w if w < 1.0 => weak,
                _ if near_best => accent,
                _ => text,
            };
            append(&mut left, &format!("{:.0} WPM", t.wpm_live), color);
        }
        let firmware = status.firmware.as_ref();
        if let Some(fw) = firmware
            && self.config.show_layer.unwrap_or(true)
        {
            // Base layer in the normal color, any other layer stands out.
            let color = if fw.report.layer == 0 { text } else { accent };
            let name = self.layer_name(fw.report.layer);
            append(&mut left, &format!("{} {name}", egui_phosphor::regular::STACK), color);
        }

        let game = status.game.as_ref();
        let show_level = self.config.show_level.unwrap_or(true) && game.is_some();
        if let Some(g) = game.filter(|_| show_level) {
            let stars = egui_phosphor::regular::STAR.repeat(g.prestige.min(5) as usize);
            append(&mut left, &format!("{stars}Lv {}", g.level), text);
        }
        ui.add(Label::new(left).selectable(false));

        if let Some(g) = game.filter(|_| show_level) {
            let (rect, _) = ui.allocate_exact_size(Vec2::new(34.0, 5.0), Sense::hover());
            let painter = ui.painter();
            painter.rect_filled(rect, CornerRadius::same(2), visuals.extreme_bg_color);
            let filled = Rect::from_min_size(
                rect.min,
                Vec2::new(rect.width() * level_fraction(g), rect.height()),
            );
            painter.rect_filled(filled, CornerRadius::same(2), accent);
        }

        let mut right = LayoutJob::default();
        if let Some(g) = game
            && self.config.show_loyalty.unwrap_or(true)
        {
            let color = if g.loyalty > 1.0 { accent } else { weak };
            append(
                &mut right,
                &format!("{} ×{:.1}", egui_phosphor::regular::FIRE, g.loyalty),
                color,
            );
        }
        if self.config.show_words_today.unwrap_or(true) {
            append(&mut right, &format!("{} words", t.words_today), weak);
        }
        if let Some(fw) = firmware
            && self.config.show_battery.unwrap_or(true)
        {
            let levels: Vec<u8> = std::iter::once(fw.report.battery_central)
                .chain(fw.report.battery_peripherals.iter().copied())
                .flatten()
                .collect();
            if let Some(&min) = levels.iter().min() {
                let joined = levels
                    .iter()
                    .map(|l| format!("{l}"))
                    .collect::<Vec<_>>()
                    .join("·");
                let color = if min <= 20 { visuals.warn_fg_color } else { weak };
                append(&mut right, &format!("{} {joined}%", battery_icon(min)), color);
            }
        }
        if !right.text.is_empty() {
            ui.add(Label::new(right).selectable(false));
        }
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
        Grid::new("komotype-tooltip")
            .num_columns(2)
            .spacing([16.0, 2.0])
            .show(ui, |ui| {
                let mut row = |k: &str, v: String| {
                    ui.label(k);
                    ui.label(v);
                    ui.end_row();
                };
                if let Some(g) = &s.game {
                    row("Level", format!("{} · {}", g.level, g.title));
                    row("XP", format!("{} / {}", g.level_xp, g.level_xp_needed));
                    row("Streak", format!("{} days", g.streak_days));
                    let done = g.quests.iter().filter(|q| q.done).count();
                    row("Quests", format!("{done} / {}", g.quests.len()));
                }
                row("WPM now", format!("{:.0}", t.wpm_live));
                row("Best today", wpm_text(t.best_wpm_today));
                row("Words today", t.words_today.to_string());
                row("Accuracy", pct(t.accuracy_today));
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
        let keyboard = s
            .main_keyboard
            .as_deref()
            .unwrap_or("no main keyboard (run `komotyped pick`)");
        let connection = if s.main_connected { "connected" } else { "not connected" };
        ui.label(RichText::new(format!("{keyboard} · {connection}")).weak());
        ui.add_space(6.0);

        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Today, "Today");
            ui.selectable_value(&mut self.tab, Tab::Progress, "Progress");
            ui.selectable_value(&mut self.tab, Tab::Keyboard, "Keyboard");
            ui.selectable_value(&mut self.tab, Tab::Achievements, "Achievements");
        });
        ui.separator();
        ui.add_space(4.0);

        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.tab {
            Tab::Today => self.today_tab(ui, s),
            Tab::Progress => self.progress_tab(ui, s),
            Tab::Keyboard => self.keyboard_tab(ui, s),
            Tab::Achievements => {
                ui.label(RichText::new("0 / ?? discovered").strong());
                ui.add_space(6.0);
                ui.label("Achievements are not built yet.");
                ui.label(RichText::new("They arrive in the next update.").weak());
            }
        });
    }

    fn quest_row(&self, ui: &mut Ui, q: &Quest) {
        let title = self.quest_title(&q.title, q.layer);
        ui.horizontal(|ui| {
            let icon = if q.done {
                egui_phosphor::regular::CHECK_CIRCLE
            } else {
                egui_phosphor::regular::CIRCLE
            };
            ui.label(icon);
            let text = RichText::new(title);
            ui.label(if q.done { text.weak().strikethrough() } else { text });
        });
        ui.horizontal(|ui| {
            ui.add_space(22.0);
            let fraction = if q.target == 0 { 1.0 } else { q.progress as f32 / q.target as f32 };
            ui.add(
                ProgressBar::new(fraction.min(1.0))
                    .desired_width(250.0)
                    .desired_height(14.0)
                    .text(format!("{} / {}", q.progress, q.target)),
            );
            ui.label(RichText::new(format!("+{} XP", q.reward_xp)).weak());
        });
        ui.add_space(4.0);
    }

    fn today_tab(&self, ui: &mut Ui, s: &Status) {
        if let Some(g) = &s.game {
            ui.label(RichText::new("Daily quests").strong());
            for q in &g.quests {
                self.quest_row(ui, q);
            }
            if let Some(w) = &g.weekly {
                ui.add_space(4.0);
                ui.label(RichText::new("Weekly challenge").strong());
                self.quest_row(ui, w);
            }
            ui.separator();
        }
        let t = &s.typing;
        Grid::new("komotype-today")
            .num_columns(2)
            .spacing([24.0, 4.0])
            .show(ui, |ui| {
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

    fn progress_tab(&self, ui: &mut Ui, s: &Status) {
        let Some(g) = &s.game else {
            ui.label("Choose a main keyboard first: `komotyped pick`.");
            return;
        };
        let stars = egui_phosphor::regular::STAR.repeat(g.prestige as usize);
        ui.label(RichText::new(format!("{stars} Level {} · {}", g.level, g.title)).strong());
        let bar_text = if g.level_xp_needed == 0 {
            "max level".to_string()
        } else {
            format!("{} / {} XP", g.level_xp, g.level_xp_needed)
        };
        ui.add(ProgressBar::new(level_fraction(g)).text(bar_text));
        if let Some(next) = g.next_title_level {
            ui.label(RichText::new(format!("Next title at level {next}: ???")).weak());
        }
        if g.level >= g.max_level {
            ui.add_space(4.0);
            if ui
                .add(Button::new(format!(
                    "{} Prestige: back to level 1 for a star",
                    egui_phosphor::regular::STAR
                )))
                .clicked()
            {
                send_command(DaemonCommand::Prestige);
            }
        }
        ui.add_space(8.0);

        Grid::new("komotype-progress")
            .num_columns(2)
            .spacing([24.0, 6.0])
            .show(ui, |ui| {
                ui.label(format!("{} Streak", egui_phosphor::regular::FIRE));
                ui.label(format!(
                    "{} days · {} {} freeze(s)",
                    g.streak_days,
                    egui_phosphor::regular::SNOWFLAKE,
                    g.streak_freezes
                ));
                ui.end_row();

                ui.label("Today's goal");
                let goal = g.streak_threshold.max(1);
                ui.add(
                    ProgressBar::new((g.streak_today as f32 / goal as f32).min(1.0))
                        .desired_width(200.0)
                        .text(format!("{} / {} keys", g.streak_today.min(goal), goal)),
                );
                ui.end_row();

                ui.label(format!("{} Loyalty", egui_phosphor::regular::HANDSHAKE));
                ui.label(format!("×{:.1} XP", g.loyalty));
                ui.end_row();

                ui.label(format!("{} You vs. rival", egui_phosphor::regular::SWORD));
                ui.label(format!(
                    "{} vs. {} WPM (7 days)",
                    wpm_text(g.wpm_7d),
                    wpm_text(g.rival_wpm_7d)
                ));
                ui.end_row();

                ui.label(format!("{} Season", egui_phosphor::regular::LEAF));
                ui.label(format!("{} · {} XP", g.season, g.season_xp));
                ui.end_row();
            });
        ui.label(
            RichText::new(
                "Loyalty: days with 95%+ of keystrokes on your main keyboard raise it by 0.1 (max ×2.0). \
                 Below 50% it drops back to ×1.0.",
            )
            .weak()
            .small(),
        );

        if !g.events.is_empty() {
            ui.add_space(8.0);
            ui.label(RichText::new("Recent").strong());
            for e in &g.events {
                ui.horizontal(|ui| {
                    ui.label(self.event_text(&e.kind));
                    ui.label(RichText::new(ago_text(e.at_ms)).weak().small());
                });
            }
        }
    }

    fn keyboard_tab(&self, ui: &mut Ui, s: &Status) {
        let Some(fw) = &s.firmware else {
            ui.label("No data from the keyboard firmware.");
            ui.label(
                RichText::new("Layer and battery need the komotype ZMK module on the keyboard.")
                    .weak(),
            );
            return;
        };
        Grid::new("komotype-keyboard")
            .num_columns(2)
            .spacing([24.0, 4.0])
            .show(ui, |ui| {
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
        ctx.show_viewport_immediate(
            ViewportId::from_hash_of("komotype-popup"),
            builder,
            |ctx, _| {
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
            },
        );
        if close {
            self.popup_open = false;
        }
    }

    /// Opens the popup below the widget, or above it if the bar sits at the bottom of the screen.
    fn place_popup(&mut self, ctx: &Context, widget: Rect) {
        let (outer, monitor) =
            ctx.input(|i| (i.viewport().outer_rect, i.viewport().monitor_size));
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
        let flashing = status.as_ref().and_then(|s| self.flash(s)).is_some();
        let render_config = config.clone();

        let response = config.apply_on_widget(false, ui, |ui| {
            SelectableFrame::new(self.popup_open || flashing)
                .show(ui, |ui| {
                    // Breathing room so the highlight frame does not hug the text.
                    Frame::NONE
                        .inner_margin(Margin::symmetric(4, 3))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                self.bar_contents(ui, &render_config, status.as_ref());
                            })
                        })
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
                ctx.send_viewport_cmd_to(
                    ViewportId::from_hash_of("komotype-popup"),
                    ViewportCommand::Focus,
                );
            }
        }
    }
}
