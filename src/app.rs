// SPDX-License-Identifier: GPL-3.0-or-later

use crate::fl;
use cosmic::iced::{
    Alignment, Background, Border, Color, Event, Length, Limits, Padding, Shadow, Subscription,
    futures,
    keyboard::{self, Key, Modifiers, key::Named},
    widget::text::{LineHeight, Wrapping},
    window::Id,
};
use cosmic::prelude::*;
use cosmic::widget;
use futures::SinkExt;
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    time::{Duration, Instant},
};
use tokio::process::Command;

#[derive(Clone, Debug, Default)]
pub struct TailscaleStatus {
    pub backend: String,
    pub tailnet: Option<String>,
    pub auth_url: Option<String>,
    pub hostname: String,
    pub dns_name: Option<String>,
    pub ips: Vec<String>,
    pub peers: Vec<Peer>,
}

#[derive(Clone, Debug)]
pub struct Peer {
    pub id: String,
    pub hostname: String,
    pub dns_name: Option<String>,
    pub ip: String,
    pub online: bool,
    pub os: Option<String>,
    pub exit_node: bool,
}

pub struct AppModel {
    pub core: cosmic::Core,
    pub popup: Option<Id>,
    pub status: Option<TailscaleStatus>,
    pub error: Option<String>,
    /// Target connection state of a `tailscale up`/`down` still running.
    pub pending: Option<bool>,
    /// True while a manual or initial status fetch is in flight.
    pub is_refreshing: bool,
    /// Value last copied to the clipboard, kept briefly to confirm the action.
    pub copied: Option<String>,
    copy_request_id: u64,
    /// Latency checks by peer IP.
    pub pings: HashMap<String, PingState>,
    /// Currently expanded peer ID for actions and detailed metrics.
    pub expanded_peer: Option<String>,
    /// Live filter text for the peer list.
    pub search_query: String,
    /// Multiplier for typography scaling (1.0 in normal applet mode, configurable in preview).
    pub text_scale: f32,
    /// Optional popup container limits override (used by preview to verify narrow/wide widths).
    pub popup_limits: Option<Limits>,
    search_input_id: widget::Id,
    status_request_id: u64,
    motion_preference: Option<bool>,
    expansions: Vec<DisclosureMotion>,
    copy_motions: Vec<CopyMotion>,
    refresh_spin_started: Option<Instant>,
    refresh_confirmed: bool,
    refresh_feedback_id: u64,
    frame_time: Option<Instant>,
}

impl Default for AppModel {
    fn default() -> Self {
        Self {
            core: cosmic::Core::default(),
            popup: None,
            status: None,
            error: None,
            pending: None,
            is_refreshing: false,
            copied: None,
            copy_request_id: 0,
            pings: HashMap::new(),
            expanded_peer: None,
            search_query: String::new(),
            text_scale: 1.0,
            popup_limits: None,
            search_input_id: widget::Id::unique(),
            status_request_id: 0,
            motion_preference: None,
            expansions: Vec::new(),
            copy_motions: Vec::new(),
            refresh_spin_started: None,
            refresh_confirmed: false,
            refresh_feedback_id: 0,
            frame_time: None,
        }
    }
}

#[derive(Clone, Debug)]
pub enum PingState {
    Pending,
    Done { ms: u32, direct: bool },
    Failed,
}

const EXPANSION_DURATION: Duration = Duration::from_millis(240);
const COPY_TRANSITION_DURATION: Duration = Duration::from_millis(140);
const REFRESH_FEEDBACK_DURATION: Duration = Duration::from_millis(420);
const REFRESH_CONFIRMED_HOLD: Duration = Duration::from_millis(550);
const MIN_PEERS_FOR_SEARCH: usize = 3;

/// Standard asymmetric ease-in-out (`cubic-bezier(0.4, 0.0, 0.2, 1.0)`):
/// avoids an abrupt first-frame jump while settling smoothly into place.
fn disclosure_ease(t: f32) -> f32 {
    if t <= 0.0 {
        return 0.0;
    }
    if t >= 1.0 {
        return 1.0;
    }
    const X1: f32 = 0.4;
    const X2: f32 = 0.2;
    const Y1: f32 = 0.0;
    const Y2: f32 = 1.0;

    let cx = 3.0 * X1;
    let bx = 3.0 * (X2 - X1) - cx;
    let ax = 1.0 - cx - bx;

    let mut u = t;
    for _ in 0..6 {
        let x = ((ax * u + bx) * u + cx) * u - t;
        let dx = (3.0 * ax * u + 2.0 * bx) * u + cx;
        if dx.abs() < 1e-6 {
            break;
        }
        u = (u - x / dx).clamp(0.0, 1.0);
    }

    let cy = 3.0 * Y1;
    let by = 3.0 * (Y2 - Y1) - cy;
    let ay = 1.0 - cy - by;
    ((ay * u + by) * u + cy) * u
}

/// Hierarchical opacity for drawer contents (metadata + action buttons).
/// Container opens first so buttons fade in cleanly instead of appearing sliced
/// mid-transition; on collapse they fade out before the clip reaches them.
fn drawer_content_opacity(reveal: f32) -> f32 {
    const FADE_FLOOR: f32 = 0.55;
    ((reveal - FADE_FLOOR) / (1.0 - FADE_FLOOR)).clamp(0.0, 1.0)
}

struct DisclosureMotion {
    peer_id: String,
    from: f32,
    to: f32,
    started_at: Instant,
    duration: Duration,
}

impl DisclosureMotion {
    fn with_start(peer_id: String, from: f32, to: f32, started_at: Instant) -> Self {
        let distance = (to - from).abs().clamp(0.35, 1.0);
        Self {
            peer_id,
            from,
            to,
            started_at,
            duration: EXPANSION_DURATION.mul_f32(distance),
        }
    }

    fn progress_at(&self, now: Instant) -> f32 {
        let progress = now
            .saturating_duration_since(self.started_at)
            .div_duration_f32(self.duration)
            .clamp(0.0, 1.0);
        let eased = disclosure_ease(progress);
        self.from + (self.to - self.from) * eased
    }

    fn is_complete(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started_at) >= self.duration
    }
}

struct CopyMotion {
    value: String,
    from: f32,
    to: f32,
    started_at: Instant,
    duration: Duration,
}

impl CopyMotion {
    fn with_start(value: String, from: f32, to: f32, started_at: Instant) -> Self {
        let distance = (to - from).abs().clamp(0.25, 1.0);
        Self {
            value,
            from,
            to,
            started_at,
            duration: COPY_TRANSITION_DURATION.mul_f32(distance),
        }
    }

    fn progress_at(&self, now: Instant) -> f32 {
        let progress = now
            .saturating_duration_since(self.started_at)
            .div_duration_f32(self.duration)
            .clamp(0.0, 1.0);
        let eased = disclosure_ease(progress);
        self.from + (self.to - self.from) * eased
    }

    fn is_complete(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started_at) >= self.duration
    }
}

/// What the user should understand at a glance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Loading,
    Connecting,
    Disconnecting,
    Connected,
    Disconnected,
    NeedsLogin,
    Failed,
    Other,
}

#[derive(Debug, Clone)]
pub enum Message {
    TogglePopup,
    PopupClosed(Id),
    Surface(cosmic::surface::Action<Message>),
    Refresh,
    PollTick,
    StatusLoaded(u64, Result<TailscaleStatus, String>),
    SetConnected(bool),
    CommandFinished(Result<(), String>),
    Copy(String),
    ClearCopied(String, u64),
    ClearRefreshConfirmed(u64),
    Ping(String),
    PingDone(String, Result<(u32, bool), String>),
    TogglePeerExpand(String),
    SearchChanged(String),
    FocusSearch,
    EscapePressed,
    SignIn,
    OpenAdmin,
    UrlOpened(Result<(), String>),
    AnimationFrame(Instant),
    MotionPreferenceLoaded(Result<bool, String>),
}

const ADMIN_URL: &str = "https://login.tailscale.com/admin/machines";
const ICON_NAME: &str = "io.github.lucasbrt.TailscaleCosmicApplet-symbolic";

impl AppModel {
    #[must_use]
    pub fn new(core: cosmic::Core) -> Self {
        Self {
            core,
            ..Default::default()
        }
    }

    pub fn set_animations_enabled(&mut self, enabled: bool) {
        self.motion_preference = Some(enabled);
        if !enabled {
            self.expansions.clear();
            self.copy_motions.clear();
            self.refresh_spin_started = None;
            self.frame_time = None;
        }
    }

    fn animations_enabled(&self) -> bool {
        self.motion_preference.unwrap_or(true)
    }

    fn scale_px(&self, base: f32) -> f32 {
        (base * self.text_scale.clamp(0.75, 2.5)).round()
    }

    fn title_text<'a>(
        &self,
        text: impl Into<std::borrow::Cow<'a, str>> + 'a,
    ) -> widget::Text<'a, cosmic::Theme, cosmic::Renderer> {
        widget::text(text)
            .size(self.scale_px(18.0))
            .line_height(LineHeight::Relative(1.3))
            .font(cosmic::font::bold())
            .wrapping(Wrapping::WordOrGlyph)
    }

    fn section_heading<'a>(
        &self,
        text: impl Into<std::borrow::Cow<'a, str>> + 'a,
    ) -> widget::Text<'a, cosmic::Theme, cosmic::Renderer> {
        widget::text(text)
            .size(self.scale_px(12.0))
            .line_height(LineHeight::Relative(1.35))
            .font(cosmic::font::semibold())
            .wrapping(Wrapping::WordOrGlyph)
    }

    fn body_text<'a>(
        &self,
        text: impl Into<std::borrow::Cow<'a, str>> + 'a,
    ) -> widget::Text<'a, cosmic::Theme, cosmic::Renderer> {
        widget::text(text)
            .size(self.scale_px(14.0))
            .line_height(LineHeight::Relative(1.35))
            .font(cosmic::font::default())
            .wrapping(Wrapping::WordOrGlyph)
    }

    fn body_strong<'a>(
        &self,
        text: impl Into<std::borrow::Cow<'a, str>> + 'a,
    ) -> widget::Text<'a, cosmic::Theme, cosmic::Renderer> {
        widget::text(text)
            .size(self.scale_px(14.0))
            .line_height(LineHeight::Relative(1.35))
            .font(cosmic::font::semibold())
            .wrapping(Wrapping::WordOrGlyph)
    }

    fn caption_text<'a>(
        &self,
        text: impl Into<std::borrow::Cow<'a, str>> + 'a,
    ) -> widget::Text<'a, cosmic::Theme, cosmic::Renderer> {
        widget::text(text)
            .size(self.scale_px(12.0))
            .line_height(LineHeight::Relative(1.35))
            .font(cosmic::font::default())
            .wrapping(Wrapping::WordOrGlyph)
    }

    fn mono_text<'a>(
        &self,
        text: impl Into<std::borrow::Cow<'a, str>> + 'a,
        size: f32,
    ) -> widget::Text<'a, cosmic::Theme, cosmic::Renderer> {
        widget::text(text)
            .size(self.scale_px(size))
            .line_height(LineHeight::Relative(1.35))
            .font(cosmic::font::mono())
            .wrapping(Wrapping::WordOrGlyph)
    }

    fn next_status_request(&mut self) -> u64 {
        self.status_request_id = self.status_request_id.wrapping_add(1);
        self.status_request_id
    }

    fn fetch_status_task(&mut self, mark_refreshing: bool) -> Task<cosmic::Action<Message>> {
        let req_id = self.next_status_request();
        if mark_refreshing {
            self.is_refreshing = true;
        }
        Task::perform(read_status(), move |res| Message::StatusLoaded(req_id, res))
            .map(cosmic::Action::App)
    }

    fn expansion_progress(&self, peer_id: &str, now: Instant) -> f32 {
        if let Some(motion) = self.expansions.iter().find(|m| m.peer_id == peer_id) {
            return motion.progress_at(now);
        }

        if self.expanded_peer.as_deref() == Some(peer_id) {
            1.0
        } else {
            0.0
        }
    }

    fn current_frame_time(&self) -> Instant {
        self.frame_time.unwrap_or_else(Instant::now)
    }

    fn set_peer_motion(&mut self, peer_id: String, from: f32, to: f32, now: Instant) {
        self.expansions.retain(|motion| motion.peer_id != peer_id);
        if (from - to).abs() > f32::EPSILON {
            self.expansions
                .push(DisclosureMotion::with_start(peer_id, from, to, now));
        }
    }

    fn toggle_peer_expansion(&mut self, peer_id: String) {
        let now = Instant::now();
        let prev_closing = self
            .expanded_peer
            .clone()
            .filter(|prev_id| prev_id != &peer_id)
            .map(|prev_id| {
                let prev_from = self.expansion_progress(&prev_id, now);
                (prev_id, prev_from)
            });
        let from = self.expansion_progress(&peer_id, now);
        let to = if self.expanded_peer.as_deref() == Some(&peer_id) {
            0.0
        } else {
            1.0
        };
        self.expanded_peer = (to > 0.0).then_some(peer_id.clone());

        if !self.animations_enabled() {
            self.expansions.clear();
            self.frame_time = None;
            return;
        }

        if to > 0.0
            && let Some((prev_id, prev_from)) = prev_closing
        {
            self.set_peer_motion(prev_id, prev_from, 0.0, now);
        }

        self.set_peer_motion(peer_id, from, to, now);
        if self.has_active_animations() {
            self.frame_time = Some(now);
        }
    }

    fn copy_progress(&self, value: &str, now: Instant) -> f32 {
        if self.animations_enabled()
            && let Some(motion) = self.copy_motions.iter().find(|m| m.value == value)
        {
            return motion.progress_at(now);
        }
        if self.copied.as_deref() == Some(value) {
            1.0
        } else {
            0.0
        }
    }

    fn set_copy_motion(&mut self, value: String, from: f32, to: f32, now: Instant) {
        self.copy_motions.retain(|m| m.value != value);
        if (from - to).abs() > f32::EPSILON {
            self.copy_motions
                .push(CopyMotion::with_start(value, from, to, now));
        }
    }

    fn refresh_spin_radians(&self, now: Instant) -> f32 {
        if !self.animations_enabled() {
            return 0.0;
        }
        let Some(started_at) = self.refresh_spin_started else {
            return 0.0;
        };
        let progress = now
            .saturating_duration_since(started_at)
            .div_duration_f32(REFRESH_FEEDBACK_DURATION)
            .clamp(0.0, 1.0);
        disclosure_ease(progress) * std::f32::consts::TAU
    }

    fn has_active_animations(&self) -> bool {
        !self.expansions.is_empty()
            || !self.copy_motions.is_empty()
            || self.refresh_spin_started.is_some()
    }

    fn finish_animations_if_complete(&mut self, now: Instant) {
        self.expansions.retain(|motion| !motion.is_complete(now));
        self.copy_motions.retain(|motion| !motion.is_complete(now));
        if let Some(started_at) = self.refresh_spin_started
            && now.saturating_duration_since(started_at) >= REFRESH_FEEDBACK_DURATION
        {
            self.refresh_spin_started = None;
        }
        self.frame_time = self.has_active_animations().then_some(now);
    }

    fn reset_popup_state(&mut self) {
        self.pings.clear();
        self.copied = None;
        self.copy_motions.clear();
        self.expanded_peer = None;
        self.expansions.clear();
        self.refresh_spin_started = None;
        self.refresh_confirmed = false;
        self.frame_time = None;
        self.search_query.clear();
    }

    fn set_motion_preference(&mut self, result: Result<bool, String>) {
        match result {
            Ok(enabled) => self.set_animations_enabled(enabled),
            Err(_) => {
                // Default to enabled when GNOME schema is absent (e.g. pure COSMIC session).
                if self.motion_preference.is_none() {
                    self.motion_preference = Some(true);
                }
            }
        }
    }

    pub fn keyboard_subscription(&self) -> Subscription<Message> {
        cosmic::iced::event::listen_with(|event, _status, _window_id| match event {
            Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => match key {
                Key::Named(Named::Escape) => Some(Message::EscapePressed),
                Key::Named(Named::F5) => Some(Message::Refresh),
                Key::Character(ref ch) if modifiers.contains(Modifiers::CTRL) => {
                    match ch.as_str() {
                        "f" | "F" => Some(Message::FocusSearch),
                        "r" | "R" => Some(Message::Refresh),
                        _ => None,
                    }
                }
                _ => None,
            },
            _ => None,
        })
    }

    fn phase(&self) -> Phase {
        if let Some(target) = self.pending {
            return if target {
                Phase::Connecting
            } else {
                Phase::Disconnecting
            };
        }
        match (&self.status, &self.error) {
            (None, Some(_)) => Phase::Failed,
            (None, None) => Phase::Loading,
            (Some(status), _) => match status.backend.as_str() {
                "Running" => Phase::Connected,
                "Starting" => Phase::Connecting,
                "Stopped" => Phase::Disconnected,
                "NeedsLogin" => Phase::NeedsLogin,
                _ => Phase::Other,
            },
        }
    }

    fn filtered_peers<'a>(&self, peers: &'a [Peer]) -> Vec<&'a Peer> {
        let query = self.search_query.trim().to_lowercase();
        if query.is_empty() {
            return peers.iter().collect();
        }
        peers
            .iter()
            .filter(|peer| {
                peer.hostname.to_lowercase().contains(&query)
                    || peer.ip.to_lowercase().contains(&query)
                    || peer
                        .dns_name
                        .as_deref()
                        .is_some_and(|dns| dns.to_lowercase().contains(&query))
                    || peer
                        .os
                        .as_deref()
                        .is_some_and(|os| os.to_lowercase().contains(&query))
            })
            .collect()
    }

    /// Renders the applet popup view with pinned header, conditional alert banner,
    /// scrollable body, and pinned footer.
    pub fn popup_view(&self) -> Element<'_, Message> {
        let theme = cosmic::theme::active();
        let cosmic_theme = theme.cosmic();
        let phase = self.phase();
        let spacing = cosmic::theme::spacing();
        let muted_color = muted_text_color(cosmic_theme);

        // --- Zone 1: Pinned Header ---
        let header = self.header_view(phase, cosmic_theme, muted_color);

        // Reserved 3px activity bar slot prevents layout jump between idle and busy states.
        let is_busy = phase.is_transitioning() || self.is_refreshing;
        let activity_bar: Element<'_, Message> = if is_busy {
            if self.animations_enabled() {
                widget::progress_bar::indeterminate_linear()
                    .width(Length::Fill)
                    .girth(3)
                    .into()
            } else {
                // Unambiguous static in-progress indicator (never reads as 100% complete).
                widget::row::with_children(vec![
                    widget::Space::new()
                        .width(Length::FillPortion(30))
                        .height(3)
                        .into(),
                    widget::container(widget::Space::new().width(Length::Fill).height(3))
                        .width(Length::FillPortion(40))
                        .class(static_busy_bar_style())
                        .into(),
                    widget::Space::new()
                        .width(Length::FillPortion(30))
                        .height(3)
                        .into(),
                ])
                .width(Length::Fill)
                .height(3)
                .into()
            }
        } else {
            widget::Space::new().width(Length::Fill).height(3).into()
        };

        let mut root_items: Vec<Element<'_, Message>> = vec![
            widget::container(header)
                .padding(Padding {
                    top: f32::from(spacing.space_s),
                    right: f32::from(spacing.space_s),
                    bottom: f32::from(spacing.space_xs),
                    left: f32::from(spacing.space_s),
                })
                .width(Length::Fill)
                .into(),
            activity_bar,
            // Stable tree position before conditional banners/body so diffing preserves state.
            FrameDriver {
                active: self.animations_enabled() && self.has_active_animations(),
            }
            .into(),
        ];

        // --- Zone 2: Pinned Alert / Action Banner (always visible above scroll fold) ---
        if let Some(error) = &self.error {
            root_items.push(self.error_banner(error, self.status.is_some(), cosmic_theme));
        } else if phase == Phase::NeedsLogin {
            root_items.push(self.needs_login_banner(cosmic_theme));
        }

        // --- Zone 3: Scrollable Body ---
        let mut body_items: Vec<Element<'_, Message>> = Vec::new();
        let mut bottom_reserve: f32 = 0.0;
        if let Some(status) = &self.status {
            body_items.extend(self.status_sections(status, phase, cosmic_theme, muted_color));
            // Keep popup height steady while expanding/collapsing peers on short lists so
            // Wayland xdg_popup autosize never clips the pinned footer for a frame.
            // Applied as extra bottom padding (not an extra column child) so column
            // spacing never toggles when a drawer finishes expanding.
            let filtered = self.filtered_peers(&status.peers);
            if !filtered.is_empty() && filtered.len() <= 4 {
                let now = self.current_frame_time();
                let mut max_drawer_h: f32 = 0.0;
                let mut open_drawer_h: f32 = 0.0;
                for peer in &filtered {
                    let has_meta = peer_has_meta(peer);
                    let h = self.drawer_target_height(&spacing, has_meta);
                    max_drawer_h = max_drawer_h.max(h);
                    open_drawer_h += self.expansion_progress(&peer.id, now) * h;
                }
                bottom_reserve = (max_drawer_h - open_drawer_h).max(0.0);
            }
        } else if phase == Phase::Loading {
            body_items.push(self.loading_skeleton(cosmic_theme, muted_color));
        }

        let max_total = self
            .popup_limits
            .map_or(680.0, |limits| limits.max().height);
        let scale = self.text_scale.clamp(0.75, 2.5);
        let header_reserve = 77.0 * scale;
        let footer_reserve = 59.0 * scale;
        let banner_reserve = if self.error.is_some() {
            116.0 * scale
        } else if phase == Phase::NeedsLogin {
            88.0 * scale
        } else {
            0.0
        };
        let max_body_height =
            (max_total - header_reserve - footer_reserve - banner_reserve - 4.0).max(120.0);

        if !body_items.is_empty() {
            root_items.push(widget::divider::horizontal::light().into());
            let body_col = widget::column::with_children(body_items)
                .spacing(spacing.space_s)
                .padding(Padding {
                    top: f32::from(spacing.space_xs),
                    right: f32::from(spacing.space_s) + 4.0,
                    bottom: f32::from(spacing.space_xs) + bottom_reserve,
                    left: f32::from(spacing.space_s),
                })
                .width(Length::Fill);

            root_items.push(
                widget::container(
                    widget::scrollable(body_col)
                        .width(Length::Fill)
                        .height(Length::Shrink),
                )
                .width(Length::Fill)
                .max_height(max_body_height)
                .into(),
            );
        }

        // --- Zone 4: Pinned Footer (always reachable in 1 click) ---
        root_items.push(widget::divider::horizontal::default().into());
        root_items.push(self.footer_view(muted_color));

        let shell = widget::column::with_children(root_items)
            .spacing(0)
            .width(Length::Fill)
            .height(Length::Shrink);

        let limits = self.popup_limits.unwrap_or_else(|| {
            Limits::NONE
                .min_width(392.0)
                .max_width(420.0)
                .min_height(1.0)
                .max_height(680.0)
        });

        self.core
            .applet
            .popup_container(shell)
            .limits(limits)
            .into()
    }

    fn header_view(
        &self,
        phase: Phase,
        theme: &cosmic::cosmic_theme::Theme,
        muted_color: Color,
    ) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let tone = phase.tone(theme);

        // Vector status icon + title (never color alone — WCAG 1.4.1).
        let status_icon = widget::container(
            widget::icon::from_name(phase.icon_name())
                .size(16)
                .symbolic(true),
        )
        .padding(4)
        .class(status_badge_style(phase));

        let state_line = widget::row::with_children(vec![
            status_icon.into(),
            self.title_text(phase.label(self.status.as_ref()))
                .class(tone_text(tone))
                .into(),
        ])
        .spacing(spacing.space_xs)
        .align_y(Alignment::Center);

        let subtitle = if self.error.is_some() && self.status.is_some() {
            fl!("error-stale-note")
        } else {
            match phase {
                Phase::Connected => self
                    .status
                    .as_ref()
                    .and_then(|s| s.tailnet.clone())
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| phase.hint()),
                _ => phase.hint(),
            }
        };

        let mut title_col = widget::column::with_capacity(2)
            .spacing(spacing.space_xxxs)
            .push(state_line);
        if !subtitle.is_empty() {
            title_col = title_col.push(
                self.caption_text(subtitle)
                    .class(cosmic::theme::Text::Color(muted_color)),
            );
        }

        // Refresh button (disabled while busy, 32x32 target, descriptive tooltip).
        // Spins one smooth turn on manual click when motion is enabled, or briefly
        // shows a green check when reduced motion is active so fast refreshes are felt.
        let can_refresh = !self.is_refreshing && self.pending.is_none();
        let refresh_tip = if can_refresh {
            fl!("refresh")
        } else {
            fl!("refreshing")
        };
        let now = self.current_frame_time();
        let refresh_icon: Element<'_, Message> =
            if !self.animations_enabled() && self.refresh_confirmed {
                widget::icon::from_name("object-select-symbolic")
                    .size(16)
                    .symbolic(true)
                    .icon()
                    .class(tinted_svg_class(Color::from(theme.success.base)))
                    .into()
            } else {
                let angle = self.refresh_spin_radians(now);
                let icon = widget::icon::from_name("view-refresh-symbolic")
                    .size(16)
                    .symbolic(true)
                    .icon();
                if angle > 0.001 {
                    icon.rotation(cosmic::iced::Rotation::Floating(cosmic::iced::Radians(
                        angle,
                    )))
                    .into()
                } else {
                    icon.into()
                }
            };
        let refresh: Element<'_, Message> = widget::tooltip(
            widget::button::custom(refresh_icon)
                .class(cosmic::theme::Button::Icon)
                .padding(6)
                .on_press_maybe(can_refresh.then_some(Message::Refresh)),
            self.caption_text(refresh_tip),
            widget::tooltip::Position::Bottom,
        )
        .into();

        // Master power toggle: disabled while Loading or while a command runs.
        let toggle_enabled = self.pending.is_none() && phase != Phase::Loading;
        let target_on = self.pending.unwrap_or(phase == Phase::Connected);
        let toggle_tip = if phase == Phase::Loading {
            fl!("toggle-unavailable-loading")
        } else if self.pending.is_some() {
            fl!("toggle-unavailable-busy")
        } else if target_on {
            fl!("toggle-disconnect")
        } else {
            fl!("toggle-connect")
        };

        let toggle = widget::tooltip(
            widget::toggler(target_on)
                .on_toggle_maybe(
                    toggle_enabled.then_some(Message::SetConnected as fn(bool) -> Message),
                )
                .duration(if self.animations_enabled() {
                    Duration::from_millis(200)
                } else {
                    Duration::from_millis(1)
                }),
            self.caption_text(toggle_tip),
            widget::tooltip::Position::Bottom,
        );

        let actions = widget::row::with_children(vec![refresh, toggle.into()])
            .spacing(spacing.space_xs)
            .align_y(Alignment::Center);

        if self.text_scale >= 1.5 {
            // Stack title above actions at large text scales so headings never break mid-word.
            let actions_row = widget::row::with_children(vec![
                widget::Space::new().width(Length::Fill).into(),
                actions.into(),
            ])
            .align_y(Alignment::Center)
            .width(Length::Fill);

            widget::column::with_children(vec![
                title_col.width(Length::Fill).into(),
                actions_row.into(),
            ])
            .spacing(spacing.space_xs)
            .width(Length::Fill)
            .into()
        } else {
            widget::row::with_children(vec![title_col.width(Length::Fill).into(), actions.into()])
                .spacing(spacing.space_s)
                .align_y(Alignment::Center)
                .width(Length::Fill)
                .into()
        }
    }

    fn error_banner(
        &self,
        error: &str,
        has_stale_status: bool,
        theme: &cosmic::cosmic_theme::Theme,
    ) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let danger = Color::from(theme.destructive.base);

        let mut text_col = widget::column::with_capacity(3)
            .spacing(spacing.space_xxxs)
            .push(
                self.body_strong(fl!("error-title"))
                    .class(cosmic::theme::Text::Color(danger)),
            )
            .push(self.caption_text(error.to_owned()));

        if has_stale_status {
            text_col = text_col.push(
                self.caption_text(fl!("error-stale-note"))
                    .class(cosmic::theme::Text::Color(muted_text_color(theme))),
            );
        }

        let header_row = widget::row::with_children(vec![
            widget::icon::from_name("dialog-error-symbolic")
                .size(16)
                .symbolic(true)
                .into(),
            text_col.width(Length::Fill).into(),
        ])
        .spacing(spacing.space_xs)
        .align_y(Alignment::Start);

        let retry_btn = widget::button::standard(fl!("retry"))
            .leading_icon(widget::icon::from_name("view-refresh-symbolic"))
            .on_press_maybe(
                (!self.is_refreshing && self.pending.is_none()).then_some(Message::Refresh),
            );

        let content = widget::column::with_capacity(2)
            .spacing(spacing.space_xs)
            .push(header_row)
            .push(retry_btn);

        widget::container(
            widget::container(content)
                .padding(spacing.space_s)
                .width(Length::Fill)
                .class(tinted_banner_style(BannerTone::Danger)),
        )
        .padding([spacing.space_xs, spacing.space_s])
        .width(Length::Fill)
        .into()
    }

    fn needs_login_banner(&self, _theme: &cosmic::cosmic_theme::Theme) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();

        let header_row = widget::row::with_children(vec![
            widget::icon::from_name("dialog-warning-symbolic")
                .size(16)
                .symbolic(true)
                .into(),
            self.caption_text(fl!("needs-login-body"))
                .width(Length::Fill)
                .into(),
        ])
        .spacing(spacing.space_xs)
        .align_y(Alignment::Start);

        let sign_in_btn = widget::button::suggested(fl!("sign-in"))
            .leading_icon(widget::icon::from_name("web-browser-symbolic"))
            .on_press_maybe(
                (!self.is_refreshing && self.pending.is_none()).then_some(Message::SignIn),
            );

        let content = widget::column::with_capacity(2)
            .spacing(spacing.space_xs)
            .push(header_row)
            .push(sign_in_btn);

        widget::container(
            widget::container(content)
                .padding(spacing.space_s)
                .width(Length::Fill)
                .class(tinted_banner_style(BannerTone::Warning)),
        )
        .padding([spacing.space_xs, spacing.space_s])
        .width(Length::Fill)
        .into()
    }

    fn loading_skeleton(
        &self,
        _theme: &cosmic::cosmic_theme::Theme,
        _muted_color: Color,
    ) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();

        let skeleton_bar = |width_fill: f32, height: f32| -> Element<'_, Message> {
            widget::container(widget::Space::new().height(height).width(Length::Fill))
                .width(Length::FillPortion((width_fill * 100.0) as u16))
                .class(skeleton_bar_style())
                .into()
        };

        let bar_row = |ratio: f32, height: f32| -> Element<'_, Message> {
            let fill = (ratio * 100.0) as u16;
            let rest = 100u16.saturating_sub(fill);
            let mut row = widget::row::with_capacity(2).push(skeleton_bar(ratio, height));
            if rest > 0 {
                row = row.push(widget::Space::new().width(Length::FillPortion(rest)));
            }
            row.width(Length::Fill).into()
        };

        let device_placeholder = widget::container(
            widget::column::with_children(vec![
                bar_row(0.55, 14.0),
                bar_row(0.75, 12.0),
                bar_row(0.65, 12.0),
            ])
            .spacing(spacing.space_xs)
            .width(Length::Fill),
        )
        .padding(spacing.space_s)
        .width(Length::Fill)
        .class(cosmic::theme::Container::Card);

        let peers_placeholder = widget::container(
            widget::column::with_children(vec![
                bar_row(0.45, 14.0),
                bar_row(0.80, 12.0),
                widget::divider::horizontal::light().into(),
                bar_row(0.50, 14.0),
                bar_row(0.70, 12.0),
            ])
            .spacing(spacing.space_xs)
            .width(Length::Fill),
        )
        .padding(spacing.space_s)
        .width(Length::Fill)
        .class(cosmic::theme::Container::Card);

        widget::column::with_children(vec![device_placeholder.into(), peers_placeholder.into()])
            .spacing(spacing.space_s)
            .width(Length::Fill)
            .into()
    }

    fn footer_view(&self, muted_color: Color) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let row = widget::row::with_children(vec![
            widget::icon::from_name("web-browser-symbolic")
                .size(16)
                .symbolic(true)
                .into(),
            self.body_text(fl!("admin-console"))
                .width(Length::Fill)
                .into(),
            widget::icon::from_name("go-next-symbolic")
                .size(16)
                .symbolic(true)
                .into(),
        ])
        .spacing(spacing.space_xs)
        .align_y(Alignment::Center)
        .width(Length::Fill);

        let _ = muted_color;
        widget::container(
            cosmic::applet::menu_button(row)
                .class(applet_menu_row_style(footer_corner_radii(
                    cosmic::theme::active().cosmic().corner_radii.radius_m,
                )))
                .padding([spacing.space_xs, spacing.space_s])
                .on_press(Message::OpenAdmin),
        )
        .padding(0)
        .width(Length::Fill)
        .into()
    }

    /// Section showing this device's hostname and labeled IPv4 / IPv6 addresses.
    fn device_section(
        &self,
        status: &TailscaleStatus,
        theme: &cosmic::cosmic_theme::Theme,
        muted_color: Color,
    ) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let mut rows: Vec<Element<'_, Message>> = Vec::new();
        let row_count = usize::from(!status.hostname.is_empty()) + status.ips.len();
        let mut row_index = 0;

        if !status.hostname.is_empty() {
            let mut identity = widget::column::with_capacity(2)
                .spacing(2)
                .push(self.body_strong(status.hostname.clone()));
            if let Some(dns) = &status.dns_name
                && dns != &status.hostname
            {
                identity = identity.push(
                    self.caption_text(dns.clone())
                        .class(cosmic::theme::Text::Color(muted_color)),
                );
            }

            let copy_p = self.copy_progress(&status.hostname, self.current_frame_time());
            let trailing = self.device_copy_trailing(copy_p, theme);

            let host_row = widget::row::with_children(vec![
                widget::icon::from_name("computer-symbolic")
                    .size(16)
                    .symbolic(true)
                    .into(),
                identity.width(Length::Fill).into(),
                trailing,
            ])
            .spacing(spacing.space_xs)
            .align_y(Alignment::Center)
            .width(Length::Fill);

            let host_button = widget::button::custom(host_row)
                .class(applet_menu_row_style(row_corner_radii(
                    theme.corner_radii.radius_s,
                    row_index == 0,
                    row_index + 1 == row_count,
                )))
                .padding([spacing.space_xxs, spacing.space_s])
                .width(Length::Fill)
                .on_press(Message::Copy(status.hostname.clone()));
            row_index += 1;

            rows.push(
                widget::tooltip(
                    host_button,
                    self.caption_text(fl!("copy-name")),
                    widget::tooltip::Position::Bottom,
                )
                .into(),
            );
        }

        for ip in &status.ips {
            if row_index > 0 {
                rows.push(widget::divider::horizontal::light().into());
            }
            let kind = if ip.contains(':') {
                fl!("ipv6")
            } else {
                fl!("ipv4")
            };

            let ip_text_col = widget::column::with_capacity(2)
                .spacing(1)
                .push(
                    self.caption_text(kind)
                        .class(cosmic::theme::Text::Color(muted_color)),
                )
                .push(self.mono_text(ip.clone(), 13.0));

            let copy_p = self.copy_progress(ip, self.current_frame_time());
            let trailing = self.device_copy_trailing(copy_p, theme);

            let ip_row =
                widget::row::with_children(vec![ip_text_col.width(Length::Fill).into(), trailing])
                    .spacing(spacing.space_xs)
                    .align_y(Alignment::Center)
                    .width(Length::Fill);

            // Entire IP row is clickable to copy, so target area is generous and effortless.
            let ip_button = widget::button::custom(ip_row)
                .class(applet_menu_row_style(row_corner_radii(
                    theme.corner_radii.radius_s,
                    row_index == 0,
                    row_index + 1 == row_count,
                )))
                .padding([spacing.space_xxs, spacing.space_s])
                .width(Length::Fill)
                .on_press(Message::Copy(ip.clone()));
            row_index += 1;

            rows.push(
                widget::tooltip(
                    ip_button,
                    self.caption_text(fl!("copy-ip")),
                    widget::tooltip::Position::Bottom,
                )
                .into(),
            );
        }

        let card = widget::container(widget::column::with_children(rows).spacing(0))
            .width(Length::Fill)
            .class(cosmic::theme::Container::Card);

        widget::column::with_children(vec![
            self.section_heading(fl!("this-device"))
                .class(cosmic::theme::Text::Color(muted_color))
                .into(),
            card.into(),
        ])
        .spacing(spacing.space_xxs)
        .width(Length::Fill)
        .into()
    }

    fn copied_chip(&self, theme: &cosmic::cosmic_theme::Theme, alpha: f32) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let mut success = Color::from(theme.success.base);
        success.a *= alpha;
        widget::row::with_children(vec![
            widget::icon::from_name("object-select-symbolic")
                .size(14)
                .symbolic(true)
                .icon()
                .class(tinted_svg_class(Color::from(theme.success.base)))
                .opacity(alpha)
                .into(),
            self.caption_text(fl!("copied"))
                .class(cosmic::theme::Text::Color(success))
                .into(),
        ])
        .spacing(spacing.space_xxxs)
        .align_y(Alignment::Center)
        .into()
    }

    fn device_copy_trailing(
        &self,
        copy_progress: f32,
        theme: &cosmic::cosmic_theme::Theme,
    ) -> Element<'_, Message> {
        if copy_progress >= 0.999 {
            return self.copied_chip(theme, 1.0);
        }
        if copy_progress <= 0.001 {
            return widget::icon::from_name("edit-copy-symbolic")
                .size(16)
                .symbolic(true)
                .into();
        }
        let copy_icon = widget::container(
            widget::icon::from_name("edit-copy-symbolic")
                .size(16)
                .symbolic(true)
                .icon()
                .opacity(1.0 - copy_progress),
        )
        .width(Length::Fill)
        .align_x(Alignment::End)
        .align_y(Alignment::Center);
        cosmic::iced::widget::stack![self.copied_chip(theme, copy_progress), copy_icon,].into()
    }

    /// Inline ping action/readout button with fixed scaled width so sibling buttons
    /// never shift horizontally across Ping / Ping… / latency / Failed states.
    fn peer_ping_button(
        &self,
        peer: &Peer,
        is_connected: bool,
        theme: &cosmic::cosmic_theme::Theme,
        spacing: &cosmic::cosmic_theme::Spacing,
        alpha: f32,
    ) -> Element<'_, Message> {
        let btn_height = Length::Fixed(f32::from(spacing.space_l));
        let btn_width = Length::Fixed(self.scale_px(94.0));
        let default_text = with_alpha(Color::from(theme.button.on), alpha);

        let build_btn = |icon_name: &'static str,
                         label: String,
                         label_color: Color,
                         semibold: bool,
                         on_press: Option<Message>|
         -> Element<'_, Message> {
            let mut text_widget =
                widget::text::body(label).class(cosmic::theme::Text::Color(label_color));
            if semibold {
                text_widget = text_widget.font(cosmic::font::semibold());
            }
            let content = widget::container(
                widget::row::with_children(vec![
                    widget::icon::from_name(icon_name)
                        .size(16)
                        .symbolic(true)
                        .icon()
                        .opacity(alpha)
                        .into(),
                    text_widget.into(),
                ])
                .spacing(spacing.space_xxxs)
                .align_y(Alignment::Center),
            )
            .width(btn_width)
            .height(btn_height)
            .align_x(Alignment::Center)
            .align_y(Alignment::Center);
            widget::button::custom(content)
                .padding(0)
                .class(faded_standard_button(alpha))
                .on_press_maybe(on_press)
                .into()
        };

        if !is_connected || !peer.online {
            let tip = if !is_connected {
                fl!("peer-disconnected")
            } else {
                fl!("peer-offline")
            };
            let btn = build_btn(
                "network-transmit-receive-symbolic",
                fl!("ping"),
                with_alpha(Color::from(theme.button.on), alpha * 0.5),
                false,
                None,
            );
            return widget::tooltip(
                btn,
                widget::text::caption(tip),
                widget::tooltip::Position::Bottom,
            )
            .into();
        }

        match self.pings.get(&peer.ip) {
            None => build_btn(
                "network-transmit-receive-symbolic",
                fl!("ping"),
                default_text,
                false,
                Some(Message::Ping(peer.ip.clone())),
            ),
            Some(PingState::Pending) => {
                let btn = build_btn(
                    "network-transmit-receive-symbolic",
                    fl!("ping-short-running"),
                    with_alpha(Color::from(theme.accent.base), alpha),
                    false,
                    None,
                );
                widget::tooltip(
                    btn,
                    widget::text::caption(fl!("ping-running")),
                    widget::tooltip::Position::Bottom,
                )
                .into()
            }
            Some(PingState::Done { ms, direct }) => {
                let path_label = if *direct {
                    fl!("ping-direct")
                } else {
                    fl!("ping-relay")
                };
                let btn = build_btn(
                    "view-refresh-symbolic",
                    format!("{ms} ms"),
                    with_alpha(Color::from(theme.success.base), alpha),
                    true,
                    Some(Message::Ping(peer.ip.clone())),
                );
                widget::tooltip(
                    btn,
                    widget::text::caption(format!("{ms} ms · {path_label} — {}", fl!("reping"))),
                    widget::tooltip::Position::Bottom,
                )
                .into()
            }
            Some(PingState::Failed) => {
                let btn = build_btn(
                    "view-refresh-symbolic",
                    fl!("ping-short-failed"),
                    with_alpha(Color::from(theme.destructive.base), alpha),
                    true,
                    Some(Message::Ping(peer.ip.clone())),
                );
                widget::tooltip(
                    btn,
                    widget::text::caption(format!("{} — {}", fl!("ping-failed"), fl!("retry"))),
                    widget::tooltip::Position::Bottom,
                )
                .into()
            }
        }
    }

    fn drawer_target_height(&self, spacing: &cosmic::cosmic_theme::Spacing, has_meta: bool) -> f32 {
        let scale = self.text_scale.clamp(0.75, 2.5);
        let row_gap = f32::from(spacing.space_xs);
        let mut height = 1.0
            + f32::from(spacing.space_xs)
            + f32::from(spacing.space_s)
            + f32::from(spacing.space_l);
        if has_meta {
            height += (17.0 * scale).round() + row_gap;
        }
        if scale > 1.25 {
            height += f32::from(spacing.space_l) + row_gap;
        }
        height
    }

    fn peer_actions_drawer(
        &self,
        peer: &Peer,
        is_connected: bool,
        theme: &cosmic::cosmic_theme::Theme,
        muted_color: Color,
        reveal: f32,
    ) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let mut drawer_col = widget::column::with_capacity(2)
            .spacing(spacing.space_xs)
            .width(Length::Fill);

        // Optional metadata line (OS / MagicDNS).
        let mut meta_parts: Vec<&str> = Vec::new();
        if let Some(os) = &peer.os
            && !os.is_empty()
        {
            meta_parts.push(os.as_str());
        }
        if let Some(dns) = &peer.dns_name
            && !dns.is_empty()
            && dns != &peer.hostname
        {
            meta_parts.push(dns.as_str());
        }
        let content_alpha = drawer_content_opacity(reveal);
        let now = self.current_frame_time();
        let btn_height = Length::Fixed(f32::from(spacing.space_l));
        let success_color = Color::from(theme.success.base);
        let default_btn_text = with_alpha(Color::from(theme.button.on), content_alpha);

        if !meta_parts.is_empty() {
            drawer_col = drawer_col.push(self.caption_text(meta_parts.join(" · ")).class(
                cosmic::theme::Text::Color(with_alpha(muted_color, content_alpha)),
            ));
        }

        let build_copy_drawer_btn = |value: &str,
                                     idle_label: String,
                                     width_px: f32,
                                     enabled: bool|
         -> Element<'_, Message> {
            let is_copied = self.copied.as_deref() == Some(value);
            let copy_p = self.copy_progress(value, now);
            let label = if is_copied { fl!("copied") } else { idle_label };
            let label_color = if is_copied {
                with_alpha(success_color, content_alpha)
            } else if enabled {
                default_btn_text
            } else {
                with_alpha(Color::from(theme.button.on), content_alpha * 0.5)
            };
            let icon = crossfade_copy_icon(copy_p, 16, success_color, content_alpha);
            let content = widget::container(
                widget::row::with_children(vec![
                    icon,
                    widget::text::body(label)
                        .class(cosmic::theme::Text::Color(label_color))
                        .into(),
                ])
                .spacing(spacing.space_xxxs)
                .align_y(Alignment::Center),
            )
            .width(Length::Fixed(self.scale_px(width_px)))
            .height(btn_height)
            .align_x(Alignment::Center)
            .align_y(Alignment::Center);
            widget::button::custom(content)
                .padding(0)
                .class(faded_standard_button(content_alpha))
                .on_press_maybe(enabled.then(|| Message::Copy(value.to_owned())))
                .into()
        };

        let ip_btn = build_copy_drawer_btn(&peer.ip, fl!("copy-ip"), 94.0, peer.ip != "—");
        let name_btn = build_copy_drawer_btn(&peer.hostname, fl!("copy-name"), 112.0, true);

        let action_buttons: Vec<Element<'_, Message>> = vec![
            self.peer_ping_button(peer, is_connected, theme, &spacing, content_alpha),
            ip_btn,
            name_btn,
        ];

        // Wrapping flex_row keeps all 3 actions on one row at standard sizes while still
        // wrapping cleanly at 200% text scale or very narrow widths.
        let actions_row = widget::flex_row(action_buttons)
            .column_spacing(spacing.space_xxs)
            .row_spacing(spacing.space_xs);

        drawer_col = drawer_col.push(actions_row);

        let drawer_divider: Element<'_, Message> = if reveal < 0.999 {
            cosmic::iced::widget::rule::horizontal(1)
                .class(faded_divider_rule(reveal))
                .into()
        } else {
            widget::divider::horizontal::light().into()
        };

        let inner = widget::column::with_children(vec![
            drawer_divider,
            widget::container(drawer_col)
                .padding(Padding {
                    top: f32::from(spacing.space_xs),
                    right: f32::from(spacing.space_s),
                    bottom: f32::from(spacing.space_s),
                    left: f32::from(spacing.space_s),
                })
                .width(Length::Fill)
                .into(),
        ])
        .spacing(0)
        .width(Length::Fill);

        let target_height = self.drawer_target_height(&spacing, !meta_parts.is_empty());
        let container = widget::container(inner).width(Length::Fill);
        // Only clamp height while animating; settled drawers size naturally (WCAG 1.4.4).
        if reveal < 0.999 {
            container
                .max_height((target_height * reveal).max(0.0))
                .clip(true)
                .into()
        } else {
            container.into()
        }
    }

    /// Single peer row with a smooth inline actions drawer.
    fn peer_item(
        &self,
        peer: &Peer,
        is_connected: bool,
        theme: &cosmic::cosmic_theme::Theme,
        muted_color: Color,
        is_first: bool,
        is_last: bool,
    ) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let is_expanded = self.expanded_peer.as_deref() == Some(&peer.id);
        let reveal = self.expansion_progress(&peer.id, self.current_frame_time());

        // Shape + luminance + color indicator (never color alone; truthful when disconnected).
        let (indicator_glyph, indicator_color) = if !is_connected {
            ("○", muted_color)
        } else if peer.online {
            ("●", Color::from(theme.success.base))
        } else {
            ("○", muted_color)
        };

        let mut title_row_items: Vec<Element<'_, Message>> = vec![if is_connected && peer.online {
            self.body_strong(peer.hostname.clone()).into()
        } else {
            self.body_text(peer.hostname.clone())
                .class(cosmic::theme::Text::Color(muted_color))
                .into()
        }];
        if is_connected && peer.exit_node {
            title_row_items.push(
                widget::container(self.caption_text(fl!("exit-node")))
                    .padding([1, 6])
                    .class(pill_tag_style())
                    .into(),
            );
        }

        let mut subtitle_parts = vec![peer.ip.clone()];
        if is_connected {
            subtitle_parts.push(if peer.online {
                fl!("online")
            } else {
                fl!("offline")
            });
            match self.pings.get(&peer.ip) {
                None => {}
                Some(PingState::Pending) if !is_expanded => {
                    subtitle_parts.push(fl!("ping-running"));
                }
                Some(PingState::Done { ms, direct }) => {
                    let path_label = if *direct {
                        fl!("ping-direct")
                    } else {
                        fl!("ping-relay")
                    };
                    if is_expanded {
                        subtitle_parts.push(path_label);
                    } else {
                        subtitle_parts.push(format!("{ms} ms"));
                    }
                }
                Some(PingState::Failed) if !is_expanded => {
                    subtitle_parts.push(fl!("ping-failed"));
                }
                _ => {}
            }
        }

        let info_col = widget::column::with_capacity(2)
            .spacing(2)
            .push(
                widget::flex_row(title_row_items)
                    .column_spacing(spacing.space_xs)
                    .row_spacing(2),
            )
            .push(
                self.mono_text(subtitle_parts.join(" · "), 12.0)
                    .class(cosmic::theme::Text::Color(muted_color)),
            )
            .width(Length::Fill);

        let chevron = widget::icon::from_name("pan-down-symbolic")
            .size(16)
            .symbolic(true)
            .icon()
            .rotation(cosmic::iced::Rotation::Floating(cosmic::iced::Radians(
                reveal * std::f32::consts::PI,
            )));

        let header_content = widget::row::with_children(vec![
            self.body_text(indicator_glyph)
                .class(cosmic::theme::Text::Color(indicator_color))
                .into(),
            info_col.into(),
            chevron.into(),
        ])
        .spacing(spacing.space_xs)
        .align_y(Alignment::Center)
        .width(Length::Fill);

        let expand_button = widget::button::custom(header_content)
            .class(applet_menu_row_style(row_corner_radii(
                theme.corner_radii.radius_s,
                is_first,
                is_last && !is_expanded && reveal <= 0.001,
            )))
            .width(Length::Fill)
            .padding([spacing.space_xs, spacing.space_s])
            .on_press(Message::TogglePeerExpand(peer.id.clone()));

        let row_line: Element<'_, Message> = expand_button.into();

        if !is_expanded && reveal <= 0.001 {
            return row_line;
        }

        widget::column::with_capacity(2)
            .spacing(0)
            .width(Length::Fill)
            .push(row_line)
            .push(self.peer_actions_drawer(peer, is_connected, theme, muted_color, reveal))
            .into()
    }

    /// Device and peer sections, shown only once a status is available.
    fn status_sections(
        &self,
        status: &TailscaleStatus,
        phase: Phase,
        theme: &cosmic::cosmic_theme::Theme,
        muted_color: Color,
    ) -> Vec<Element<'_, Message>> {
        let spacing = cosmic::theme::spacing();
        let mut out: Vec<Element<'_, Message>> = Vec::new();
        let is_connected = phase == Phase::Connected;

        if !status.ips.is_empty() || !status.hostname.is_empty() {
            out.push(self.device_section(status, theme, muted_color));
        }

        // Don't show a misleading "No other devices yet" list before the user signs in.
        if phase == Phase::NeedsLogin && status.peers.is_empty() {
            return out;
        }

        let total = status.peers.len();
        let online = status.peers.iter().filter(|p| p.online).count();
        let count_text = if is_connected {
            fl!(
                "devices-online-of-total",
                online = online.to_string(),
                total = total.to_string()
            )
        } else {
            fl!("devices-saved", total = total.to_string())
        };

        let peers_header = widget::row::with_children(vec![
            self.section_heading(fl!("devices-title"))
                .class(cosmic::theme::Text::Color(muted_color))
                .width(Length::Fill)
                .into(),
            self.caption_text(count_text)
                .class(cosmic::theme::Text::Color(muted_color))
                .into(),
        ])
        .spacing(spacing.space_xs)
        .align_y(Alignment::Center)
        .width(Length::Fill);

        let mut peers_section = widget::column::with_capacity(4)
            .spacing(spacing.space_xs)
            .width(Length::Fill)
            .push(peers_header);

        if phase == Phase::Disconnected && !status.peers.is_empty() {
            let notice = widget::container(
                widget::row::with_children(vec![
                    widget::icon::from_name("network-disconnected-symbolic")
                        .size(16)
                        .symbolic(true)
                        .into(),
                    self.caption_text(fl!("disconnected-devices-hint"))
                        .class(cosmic::theme::Text::Color(muted_color))
                        .width(Length::Fill)
                        .into(),
                ])
                .spacing(spacing.space_xs)
                .align_y(Alignment::Center),
            )
            .padding(spacing.space_xs)
            .width(Length::Fill)
            .class(cosmic::theme::Container::Secondary);
            peers_section = peers_section.push(notice);
        }

        if total >= MIN_PEERS_FOR_SEARCH || !self.search_query.is_empty() {
            let mut search =
                widget::text_input::search_input(fl!("search-peers"), &self.search_query)
                    .id(self.search_input_id.clone())
                    .on_input(Message::SearchChanged)
                    .width(Length::Fill);
            if !self.search_query.is_empty() {
                search = search.on_clear(Message::SearchChanged(String::new()));
            }
            peers_section = peers_section.push(search);
        }

        if status.peers.is_empty() {
            // Actionable empty state (States & Feedback tactic).
            let empty_card = widget::container(
                widget::column::with_children(vec![
                    widget::icon::from_name("network-workgroup-symbolic")
                        .size(24)
                        .symbolic(true)
                        .into(),
                    self.body_strong(fl!("no-devices-title")).into(),
                    self.caption_text(fl!("no-devices-body"))
                        .class(cosmic::theme::Text::Color(muted_color))
                        .into(),
                    widget::button::standard(fl!("refresh"))
                        .leading_icon(widget::icon::from_name("view-refresh-symbolic"))
                        .on_press(Message::Refresh)
                        .into(),
                ])
                .spacing(spacing.space_xs)
                .align_x(Alignment::Start)
                .width(Length::Fill),
            )
            .padding(spacing.space_s)
            .width(Length::Fill)
            .class(cosmic::theme::Container::Card);

            peers_section = peers_section.push(empty_card);
        } else {
            let filtered = self.filtered_peers(&status.peers);
            if filtered.is_empty() {
                // Distinct no-results state with query restatement and recovery CTA.
                let no_results = widget::container(
                    widget::column::with_children(vec![
                        self.body_strong(fl!(
                            "no-results-title",
                            query = self.search_query.trim().to_owned()
                        ))
                        .into(),
                        widget::button::standard(fl!("clear-search"))
                            .leading_icon(widget::icon::from_name("edit-clear-symbolic"))
                            .on_press(Message::SearchChanged(String::new()))
                            .into(),
                    ])
                    .spacing(spacing.space_xs)
                    .width(Length::Fill),
                )
                .padding(spacing.space_s)
                .width(Length::Fill)
                .class(cosmic::theme::Container::Card);

                peers_section = peers_section.push(no_results);
            } else {
                let mut rows: Vec<Element<'_, Message>> = Vec::with_capacity(filtered.len() * 2);
                for (index, peer) in filtered.iter().enumerate() {
                    if index > 0 {
                        rows.push(widget::divider::horizontal::light().into());
                    }
                    rows.push(self.peer_item(
                        peer,
                        is_connected,
                        theme,
                        muted_color,
                        index == 0,
                        index + 1 == filtered.len(),
                    ));
                }
                let list_card = widget::container(widget::column::with_children(rows).spacing(0))
                    .width(Length::Fill)
                    .class(cosmic::theme::Container::Card);
                peers_section = peers_section.push(list_card);
            }
        }

        out.push(peers_section.into());
        out
    }
}

impl Phase {
    fn label(self, status: Option<&TailscaleStatus>) -> String {
        match self {
            Phase::Loading => fl!("state-loading"),
            Phase::Connecting => fl!("state-connecting"),
            Phase::Disconnecting => fl!("state-disconnecting"),
            Phase::Connected => fl!("state-connected"),
            Phase::Disconnected => fl!("state-disconnected"),
            Phase::NeedsLogin => fl!("state-needs-login"),
            Phase::Failed => fl!("state-failed"),
            Phase::Other => fl!(
                "state-other",
                state = status.map_or_else(String::new, |s| s.backend.clone())
            ),
        }
    }

    fn hint(self) -> String {
        match self {
            Phase::Loading => fl!("hint-loading"),
            Phase::Connecting => fl!("hint-connecting"),
            Phase::Disconnecting => fl!("hint-disconnecting"),
            Phase::Connected => fl!("hint-connected"),
            Phase::Disconnected => fl!("hint-disconnected"),
            Phase::NeedsLogin => fl!("hint-needs-login"),
            Phase::Failed => fl!("hint-failed"),
            Phase::Other => String::new(),
        }
    }

    fn icon_name(self) -> &'static str {
        match self {
            Phase::Connected => "object-select-symbolic",
            Phase::Connecting | Phase::Disconnecting | Phase::Loading => {
                "network-transmit-receive-symbolic"
            }
            Phase::Disconnected => "network-disconnected-symbolic",
            Phase::NeedsLogin => "dialog-warning-symbolic",
            Phase::Failed => "dialog-error-symbolic",
            Phase::Other => "dialog-information-symbolic",
        }
    }

    /// Semantic tone from the active theme; `None` means default text colour.
    fn tone(self, theme: &cosmic::cosmic_theme::Theme) -> Option<Color> {
        match self {
            Phase::Connected => Some(theme.success.base.into()),
            Phase::Connecting | Phase::Disconnecting | Phase::Loading => {
                Some(theme.accent.base.into())
            }
            Phase::NeedsLogin => Some(theme.warning.base.into()),
            Phase::Failed => Some(theme.destructive.base.into()),
            Phase::Disconnected | Phase::Other => None,
        }
    }

    fn is_transitioning(self) -> bool {
        matches!(
            self,
            Phase::Loading | Phase::Connecting | Phase::Disconnecting
        )
    }
}

#[derive(Clone, Copy)]
enum BannerTone {
    Danger,
    Warning,
}

fn tone_text(tone: Option<Color>) -> cosmic::theme::Text {
    tone.map_or(cosmic::theme::Text::Default, cosmic::theme::Text::Color)
}

fn muted_text_color(theme: &cosmic::cosmic_theme::Theme) -> Color {
    let mut color = Color::from(theme.on_bg_color());
    // Keep > 4.5:1 legibility in both light and dark themes while distinguishing secondary text.
    color.a = 0.74;
    color
}

fn status_badge_style<'a>(phase: Phase) -> cosmic::theme::Container<'a> {
    cosmic::theme::Container::custom(move |theme| {
        let cosmic = theme.cosmic();
        let accent: Color = match phase {
            Phase::Connected => cosmic.success.base.into(),
            Phase::Connecting | Phase::Disconnecting | Phase::Loading => cosmic.accent.base.into(),
            Phase::NeedsLogin => cosmic.warning.base.into(),
            Phase::Failed => cosmic.destructive.base.into(),
            Phase::Disconnected | Phase::Other => cosmic.on_bg_color().into(),
        };
        let mut bg = accent;
        bg.a = 0.16;
        widget::container::Style {
            icon_color: Some(accent),
            text_color: Some(accent),
            background: Some(Background::Color(bg)),
            border: Border {
                radius: cosmic.corner_radii.radius_xl.into(),
                width: 0.0,
                color: Color::TRANSPARENT,
            },
            shadow: Shadow::default(),
            snap: true,
        }
    })
}

fn tinted_banner_style<'a>(tone: BannerTone) -> cosmic::theme::Container<'a> {
    cosmic::theme::Container::custom(move |theme| {
        let cosmic = theme.cosmic();
        let base: Color = match tone {
            BannerTone::Danger => cosmic.destructive.base.into(),
            BannerTone::Warning => cosmic.warning.base.into(),
        };
        let mut bg = base;
        bg.a = 0.12;
        let mut border = base;
        border.a = 0.40;
        widget::container::Style {
            icon_color: Some(base),
            text_color: Some(cosmic.on_bg_color().into()),
            background: Some(Background::Color(bg)),
            border: Border {
                radius: cosmic.corner_radii.radius_s.into(),
                width: 1.0,
                color: border,
            },
            shadow: Shadow::default(),
            snap: true,
        }
    })
}

fn skeleton_bar_style<'a>() -> cosmic::theme::Container<'a> {
    cosmic::theme::Container::custom(move |theme| {
        let cosmic = theme.cosmic();
        let mut bg: Color = cosmic.on_bg_color().into();
        bg.a = 0.10;
        widget::container::Style {
            background: Some(Background::Color(bg)),
            border: Border {
                radius: cosmic.corner_radii.radius_xs.into(),
                width: 0.0,
                color: Color::TRANSPARENT,
            },
            ..Default::default()
        }
    })
}

fn pill_tag_style<'a>() -> cosmic::theme::Container<'a> {
    cosmic::theme::Container::custom(move |theme| {
        let cosmic = theme.cosmic();
        let accent: Color = cosmic.accent.base.into();
        let mut bg = accent;
        bg.a = 0.16;
        widget::container::Style {
            text_color: Some(accent),
            background: Some(Background::Color(bg)),
            border: Border {
                radius: cosmic.corner_radii.radius_xs.into(),
                width: 0.0,
                color: Color::TRANSPARENT,
            },
            ..Default::default()
        }
    })
}

impl cosmic::Application for AppModel {
    type Executor = cosmic::executor::Default;
    type Flags = ();
    type Message = Message;
    const APP_ID: &'static str = "io.github.lucasbrt.TailscaleCosmicApplet";

    fn core(&self) -> &cosmic::Core {
        &self.core
    }
    fn core_mut(&mut self) -> &mut cosmic::Core {
        &mut self.core
    }

    fn init(core: cosmic::Core, _flags: ()) -> (Self, Task<cosmic::Action<Message>>) {
        let mut app = Self::new(core);
        let initial_status = app.fetch_status_task(true);
        let motion_preference =
            Task::perform(read_motion_preference(), Message::MotionPreferenceLoaded)
                .map(cosmic::Action::App);
        (app, Task::batch([initial_status, motion_preference]))
    }

    fn on_close_requested(&self, id: Id) -> Option<Message> {
        Some(Message::PopupClosed(id))
    }

    fn view(&self) -> Element<'_, Message> {
        let phase = self.phase();
        let button = self
            .core
            .applet
            .icon_button(ICON_NAME)
            .on_press(Message::TogglePopup);
        // Tooltip gives the panel button the state it cannot show with an icon alone.
        let tip = fl!("tooltip-status", state = phase.label(self.status.as_ref()));
        self.core
            .applet
            .applet_tooltip::<Message>(button, tip, self.popup.is_some(), Message::Surface, None)
            .into()
    }

    fn view_window(&self, _id: Id) -> Element<'_, Message> {
        self.popup_view()
    }

    fn subscription(&self) -> Subscription<Message> {
        let status_poll = Subscription::run(|| {
            cosmic::iced::stream::channel(
                4,
                |mut channel: futures::channel::mpsc::Sender<Message>| async move {
                    let mut interval = tokio::time::interval(Duration::from_secs(5));
                    // Skip the immediate first tick since `init` already fires the initial fetch.
                    interval.tick().await;
                    loop {
                        interval.tick().await;
                        if channel.send(Message::PollTick).await.is_err() {
                            break;
                        }
                    }
                },
            )
        });

        let mut subs = vec![status_poll];
        if self.popup.is_some() {
            subs.push(self.keyboard_subscription());
        }
        Subscription::batch(subs)
    }

    fn update(&mut self, message: Message) -> Task<cosmic::Action<Message>> {
        match message {
            Message::Refresh => {
                if self.is_refreshing || self.pending.is_some() {
                    return Task::none();
                }
                self.error = None;
                self.refresh_feedback_id = self.refresh_feedback_id.wrapping_add(1);
                let fb_id = self.refresh_feedback_id;
                let fetch = self.fetch_status_task(true);
                if self.animations_enabled() {
                    let now = Instant::now();
                    self.refresh_spin_started = Some(now);
                    self.frame_time = Some(now);
                    self.refresh_confirmed = false;
                    return fetch;
                }
                self.refresh_spin_started = None;
                self.refresh_confirmed = true;
                let clear_confirm = Task::perform(
                    async move {
                        tokio::time::sleep(REFRESH_CONFIRMED_HOLD).await;
                        fb_id
                    },
                    Message::ClearRefreshConfirmed,
                )
                .map(cosmic::Action::App);
                return Task::batch([fetch, clear_confirm]);
            }
            Message::ClearRefreshConfirmed(id) => {
                if self.refresh_feedback_id == id {
                    self.refresh_confirmed = false;
                }
            }
            Message::PollTick => {
                if self.pending.is_some() || self.is_refreshing {
                    return Task::none();
                }
                return self.fetch_status_task(false);
            }
            Message::StatusLoaded(req_id, result) => {
                // Reject stale async results so a slow poll never overwrites newer state.
                if req_id != self.status_request_id {
                    return Task::none();
                }
                self.is_refreshing = false;
                match result {
                    Ok(status) => {
                        self.error = None;
                        if status.backend != "Running" {
                            self.pings.clear();
                        }
                        self.status = Some(status);
                    }
                    Err(error) => self.error = Some(error),
                }
            }
            Message::SetConnected(target) => {
                if self.pending.is_some() || self.phase() == Phase::Loading {
                    return Task::none();
                }
                self.pending = Some(target);
                self.error = None;
                // Invalidate any in-flight poll so it cannot race with `up`/`down`.
                self.next_status_request();
                let command = if target { "up" } else { "down" };
                return Task::perform(run_command(command), Message::CommandFinished)
                    .map(cosmic::Action::App);
            }
            Message::CommandFinished(result) => {
                self.pending = None;
                match result {
                    Ok(()) => return self.fetch_status_task(true),
                    Err(error) => self.error = Some(error),
                }
            }
            Message::TogglePopup => return self.toggle_popup(),
            Message::Copy(text) => {
                let now = Instant::now();
                self.copy_request_id = self.copy_request_id.wrapping_add(1);
                let request_id = self.copy_request_id;
                let prev = self.copied.clone().filter(|p| p != &text);
                let prev_from = prev.as_deref().map(|p| self.copy_progress(p, now));
                let next_from = self.copy_progress(&text, now);
                self.copied = Some(text.clone());
                if self.animations_enabled() {
                    if let (Some(prev_val), Some(pf)) = (prev, prev_from) {
                        self.set_copy_motion(prev_val, pf, 0.0, now);
                    }
                    self.set_copy_motion(text.clone(), next_from, 1.0, now);
                    if self.has_active_animations() {
                        self.frame_time = Some(now);
                    }
                } else {
                    self.copy_motions.clear();
                }
                return Task::batch([
                    cosmic::iced::clipboard::write(text.clone()),
                    Task::perform(
                        async move {
                            tokio::time::sleep(Duration::from_millis(1600)).await;
                            text
                        },
                        move |text| Message::ClearCopied(text, request_id),
                    )
                    .map(cosmic::Action::App),
                ]);
            }
            Message::ClearCopied(text, request_id) => {
                if self.copy_request_id == request_id && self.copied.as_ref() == Some(&text) {
                    let now = Instant::now();
                    let from = self.copy_progress(&text, now);
                    self.copied = None;
                    if self.animations_enabled() {
                        self.set_copy_motion(text, from, 0.0, now);
                        if self.has_active_animations() {
                            self.frame_time = Some(now);
                        }
                    } else {
                        self.copy_motions.clear();
                    }
                }
            }
            Message::Ping(ip) => {
                if self.phase() != Phase::Connected
                    || matches!(self.pings.get(&ip), Some(PingState::Pending))
                {
                    return Task::none();
                }
                self.pings.insert(ip.clone(), PingState::Pending);
                return Task::perform(
                    async move {
                        let result = ping(&ip).await;
                        (ip, result)
                    },
                    |(ip, result)| Message::PingDone(ip, result),
                )
                .map(cosmic::Action::App);
            }
            Message::PingDone(ip, result) => {
                if self.phase() != Phase::Connected {
                    return Task::none();
                }
                let state = match result {
                    Ok((ms, direct)) => PingState::Done { ms, direct },
                    Err(_) => PingState::Failed,
                };
                self.pings.insert(ip, state);
            }
            Message::TogglePeerExpand(peer_id) => {
                self.toggle_peer_expansion(peer_id);
            }
            Message::SearchChanged(query) => {
                self.search_query = query;
            }
            Message::FocusSearch => {
                return widget::text_input::focus(self.search_input_id.clone());
            }
            Message::EscapePressed => {
                if !self.search_query.is_empty() {
                    self.search_query.clear();
                } else if let Some(peer_id) = self.expanded_peer.clone() {
                    self.toggle_peer_expansion(peer_id);
                } else if self.popup.is_some() {
                    return self.toggle_popup();
                }
            }
            Message::SignIn => {
                if self.is_refreshing || self.pending.is_some() {
                    return Task::none();
                }
                if let Some(url) = self
                    .status
                    .as_ref()
                    .and_then(|s| s.auth_url.clone())
                    .filter(|u| !u.is_empty())
                {
                    return Task::perform(open_url(url), Message::UrlOpened)
                        .map(cosmic::Action::App);
                }
                return self.update(Message::SetConnected(true));
            }
            Message::AnimationFrame(now) => self.finish_animations_if_complete(now),
            Message::MotionPreferenceLoaded(result) => self.set_motion_preference(result),
            Message::OpenAdmin => {
                return Task::perform(open_url(ADMIN_URL.to_owned()), Message::UrlOpened)
                    .map(cosmic::Action::App);
            }
            Message::UrlOpened(result) => {
                if let Err(err) = result {
                    self.error = Some(err);
                }
            }
            Message::Surface(action) => {
                return cosmic::task::message(cosmic::Action::Surface(action));
            }
            Message::PopupClosed(id) => {
                if self.popup.as_ref() == Some(&id) {
                    self.popup = None;
                    self.reset_popup_state();
                }
            }
        }
        Task::none()
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }
}

impl AppModel {
    fn toggle_popup(&mut self) -> Task<cosmic::Action<Message>> {
        if let Some(popup) = self.popup.take() {
            self.reset_popup_state();
            cosmic::iced::platform_specific::shell::wayland::commands::popup::destroy_popup(popup)
        } else {
            let id = Id::unique();
            self.popup = Some(id);
            let mut settings = self.core.applet.get_popup_settings(
                self.core.main_window_id().unwrap(),
                id,
                None,
                None,
                None,
            );
            settings.positioner.size_limits = Limits::NONE
                .max_width(420.0)
                .min_width(392.0)
                .min_height(180.0)
                .max_height(680.0);
            // Re-read the OS animation preference whenever the popup opens so live
            // reduced-motion toggles take effect immediately without restarting.
            let refresh_motion =
                Task::perform(read_motion_preference(), Message::MotionPreferenceLoaded)
                    .map(cosmic::Action::App);
            Task::batch([
                cosmic::iced::platform_specific::shell::wayland::commands::popup::get_popup(
                    settings,
                ),
                refresh_motion,
            ])
        }
    }
}

fn with_alpha(mut color: Color, alpha: f32) -> Color {
    color.a *= alpha.clamp(0.0, 1.0);
    color
}

fn peer_has_meta(peer: &Peer) -> bool {
    let has_os = peer.os.as_deref().is_some_and(|s| !s.is_empty());
    let has_dns = peer
        .dns_name
        .as_deref()
        .is_some_and(|d| !d.is_empty() && d != peer.hostname);
    has_os || has_dns
}

/// Zero-size leaf widget that drives popup animations synchronously on each
/// Wayland `RedrawRequested(now)` before `interface.draw()`, avoiding both the
/// 1-vsync lag and multi-window double-ticks of `window::frames()`.
struct FrameDriver {
    active: bool,
}

#[derive(Default)]
struct FrameDriverState {
    last_redraw: Option<Instant>,
}

impl cosmic::iced::advanced::Widget<Message, cosmic::Theme, cosmic::Renderer> for FrameDriver {
    fn tag(&self) -> cosmic::iced::advanced::widget::tree::Tag {
        cosmic::iced::advanced::widget::tree::Tag::of::<FrameDriverState>()
    }

    fn state(&self) -> cosmic::iced::advanced::widget::tree::State {
        cosmic::iced::advanced::widget::tree::State::new(FrameDriverState::default())
    }

    fn size(&self) -> cosmic::iced::Size<Length> {
        // Must not be Fixed(0.0): iced Column::push skips children whose size_hint().is_void().
        // layout() still returns Size::ZERO so this leaf has zero visual or layout footprint.
        cosmic::iced::Size::new(Length::Shrink, Length::Shrink)
    }

    fn layout(
        &mut self,
        _tree: &mut cosmic::iced::advanced::widget::Tree,
        _renderer: &cosmic::Renderer,
        _limits: &cosmic::iced::advanced::layout::Limits,
    ) -> cosmic::iced::advanced::layout::Node {
        cosmic::iced::advanced::layout::Node::new(cosmic::iced::Size::ZERO)
    }

    fn draw(
        &self,
        _tree: &cosmic::iced::advanced::widget::Tree,
        _renderer: &mut cosmic::Renderer,
        _theme: &cosmic::Theme,
        _style: &cosmic::iced::advanced::renderer::Style,
        _layout: cosmic::iced::advanced::Layout<'_>,
        _cursor: cosmic::iced::mouse::Cursor,
        _viewport: &cosmic::iced::Rectangle,
    ) {
    }

    fn update(
        &mut self,
        tree: &mut cosmic::iced::advanced::widget::Tree,
        event: &Event,
        _layout: cosmic::iced::advanced::Layout<'_>,
        _cursor: cosmic::iced::mouse::Cursor,
        _renderer: &cosmic::Renderer,
        _clipboard: &mut dyn cosmic::iced::advanced::Clipboard,
        shell: &mut cosmic::iced::advanced::Shell<'_, Message>,
        _viewport: &cosmic::iced::Rectangle,
    ) {
        if !self.active {
            return;
        }
        if let Event::Window(cosmic::iced::window::Event::RedrawRequested(now)) = event {
            let state = tree.state.downcast_mut::<FrameDriverState>();
            if state.last_redraw != Some(*now) {
                state.last_redraw = Some(*now);
                shell.publish(Message::AnimationFrame(*now));
            }
            shell.request_redraw();
        }
    }
}

impl<'a> From<FrameDriver> for Element<'a, Message> {
    fn from(driver: FrameDriver) -> Self {
        Self::new(driver)
    }
}

fn row_corner_radii(radius: [f32; 4], is_first: bool, is_last: bool) -> [f32; 4] {
    [
        if is_first { radius[0] } else { 0.0 },
        if is_first { radius[1] } else { 0.0 },
        if is_last { radius[2] } else { 0.0 },
        if is_last { radius[3] } else { 0.0 },
    ]
}

fn footer_corner_radii(popup_radius: [f32; 4]) -> [f32; 4] {
    [0.0, 0.0, popup_radius[2], popup_radius[3]]
}

fn with_button_radii(mut style: widget::button::Style, radii: [f32; 4]) -> widget::button::Style {
    style.border_radius = radii.into();
    style
}

fn applet_menu_row_style(radii: [f32; 4]) -> cosmic::theme::Button {
    cosmic::theme::Button::Custom {
        active: Box::new(move |focused, theme| {
            with_button_radii(
                widget::button::Catalog::active(
                    theme,
                    focused,
                    false,
                    &cosmic::theme::Button::AppletMenu,
                ),
                radii,
            )
        }),
        disabled: Box::new(move |theme| {
            with_button_radii(
                widget::button::Catalog::disabled(theme, &cosmic::theme::Button::AppletMenu),
                radii,
            )
        }),
        hovered: Box::new(move |focused, theme| {
            with_button_radii(
                widget::button::Catalog::hovered(
                    theme,
                    focused,
                    false,
                    &cosmic::theme::Button::AppletMenu,
                ),
                radii,
            )
        }),
        pressed: Box::new(move |focused, theme| {
            with_button_radii(
                widget::button::Catalog::pressed(
                    theme,
                    focused,
                    false,
                    &cosmic::theme::Button::AppletMenu,
                ),
                radii,
            )
        }),
    }
}

fn tinted_svg_class(color: Color) -> cosmic::theme::Svg {
    cosmic::theme::Svg::custom(move |_| cosmic::iced::widget::svg::Style { color: Some(color) })
}

fn crossfade_copy_icon(
    progress: f32,
    size: u16,
    success_color: Color,
    outer_alpha: f32,
) -> Element<'static, Message> {
    let copy_alpha = ((1.0 - progress) * outer_alpha).clamp(0.0, 1.0);
    let check_alpha = (progress * outer_alpha).clamp(0.0, 1.0);
    if check_alpha <= 0.001 {
        return widget::icon::from_name("edit-copy-symbolic")
            .size(size)
            .symbolic(true)
            .icon()
            .opacity(copy_alpha)
            .into();
    }
    if copy_alpha <= 0.001 {
        return widget::icon::from_name("object-select-symbolic")
            .size(size)
            .symbolic(true)
            .icon()
            .class(tinted_svg_class(success_color))
            .opacity(check_alpha)
            .into();
    }
    cosmic::iced::widget::stack![
        widget::icon::from_name("edit-copy-symbolic")
            .size(size)
            .symbolic(true)
            .icon()
            .opacity(copy_alpha),
        widget::icon::from_name("object-select-symbolic")
            .size(size)
            .symbolic(true)
            .icon()
            .class(tinted_svg_class(success_color))
            .opacity(check_alpha),
    ]
    .into()
}

fn fade_button_style(mut style: widget::button::Style, alpha: f32) -> widget::button::Style {
    let a = alpha.clamp(0.0, 1.0);
    if let Some(Background::Color(ref mut c)) = style.background {
        c.a *= a;
    }
    if let Some(Background::Color(ref mut c)) = style.overlay {
        c.a *= a;
    }
    style.border_color.a *= a;
    style.outline_color.a *= a;
    if let Some(ref mut c) = style.text_color {
        c.a *= a;
    }
    if let Some(ref mut c) = style.icon_color {
        c.a *= a;
    }
    style
}

fn faded_standard_button(alpha: f32) -> cosmic::theme::Button {
    if alpha >= 0.999 {
        return cosmic::theme::Button::Standard;
    }
    cosmic::theme::Button::Custom {
        active: Box::new(move |focused, theme| {
            let base = widget::button::Catalog::active(
                theme,
                focused,
                false,
                &cosmic::theme::Button::Standard,
            );
            fade_button_style(base, alpha)
        }),
        disabled: Box::new(move |theme| {
            let base = widget::button::Catalog::disabled(theme, &cosmic::theme::Button::Standard);
            fade_button_style(base, alpha)
        }),
        hovered: Box::new(move |focused, theme| {
            let base = widget::button::Catalog::hovered(
                theme,
                focused,
                false,
                &cosmic::theme::Button::Standard,
            );
            fade_button_style(base, alpha)
        }),
        pressed: Box::new(move |focused, theme| {
            let base = widget::button::Catalog::pressed(
                theme,
                focused,
                false,
                &cosmic::theme::Button::Standard,
            );
            fade_button_style(base, alpha)
        }),
    }
}

fn faded_divider_rule(alpha: f32) -> cosmic::theme::Rule {
    cosmic::theme::Rule::custom(move |theme| {
        let mut color = Color::from(theme.cosmic().primary_container_divider());
        color.a *= alpha.clamp(0.0, 1.0);
        cosmic::iced::widget::rule::Style {
            color,
            radius: 0.0.into(),
            fill_mode: cosmic::iced::widget::rule::FillMode::Full,
            snap: true,
        }
    })
}

fn static_busy_bar_style<'a>() -> cosmic::theme::Container<'a> {
    cosmic::theme::Container::custom(move |theme| {
        let mut accent: Color = theme.cosmic().accent.base.into();
        accent.a = 0.7;
        widget::container::Style {
            background: Some(Background::Color(accent)),
            border: Border {
                radius: 1.5.into(),
                width: 0.0,
                color: Color::TRANSPARENT,
            },
            ..Default::default()
        }
    })
}

async fn open_url(url: String) -> Result<(), String> {
    let status = Command::new("xdg-open")
        .arg(&url)
        .status()
        .await
        .map_err(|e| format!("{}: {e}", fl!("open-url-failed")))?;
    if status.success() {
        Ok(())
    } else {
        Err(fl!("open-url-failed"))
    }
}

async fn run_command(action: &str) -> Result<(), String> {
    let output = tokio::time::timeout(
        Duration::from_secs(45),
        Command::new("tailscale")
            .arg(action)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| fl!("command-timeout"))?
    .map_err(|e| format!("{}: {e}", fl!("tailscale-unavailable")))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

async fn read_motion_preference() -> Result<bool, String> {
    let output = tokio::time::timeout(
        Duration::from_secs(2),
        Command::new("gsettings")
            .args(["get", "org.gnome.desktop.interface", "enable-animations"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "Timed out reading the system animation preference".to_owned())?
    .map_err(|error| format!("Could not run gsettings: {error}"))?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }

    match String::from_utf8_lossy(&output.stdout).trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        value => Err(format!("Unexpected animation preference value: {value}")),
    }
}

async fn ping(ip: &str) -> Result<(u32, bool), String> {
    let output = tokio::time::timeout(
        Duration::from_secs(8),
        Command::new("tailscale")
            .args(["ping", "--c", "1", "--timeout", "5s", "--", ip])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| fl!("command-timeout"))?
    .map_err(|e| format!("{}: {e}", fl!("tailscale-unavailable")))?;
    parse_ping(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| String::from_utf8_lossy(&output.stderr).trim().to_owned())
}

/// Parses `pong from <host> (<ip>) via <path> in <latency>` into milliseconds and whether it was direct.
fn parse_ping(text: &str) -> Option<(u32, bool)> {
    let line = text.lines().find(|l| l.starts_with("pong"))?;
    let latency = line.rsplit(" in ").next()?.trim();
    let ms = if let Some(ms) = latency.strip_suffix("ms") {
        ms.parse::<f64>().ok()?
    } else {
        latency.strip_suffix('s')?.parse::<f64>().ok()? * 1000.0
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some((ms.round() as u32, !line.contains("DERP")))
}

async fn read_status() -> Result<TailscaleStatus, String> {
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        Command::new("tailscale")
            .args(["status", "--json"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| fl!("status-timeout"))?
    .map_err(|e| format!("{}: {e}", fl!("tailscale-unavailable")))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    parse_status(&output.stdout)
}

fn clean_dns_name(raw: Option<&str>) -> Option<String> {
    raw.map(|s| s.trim_end_matches('.'))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn parse_status(bytes: &[u8]) -> Result<TailscaleStatus, String> {
    let root: Value =
        serde_json::from_slice(bytes).map_err(|e| format!("{}: {e}", fl!("invalid-status")))?;
    let self_node = &root["Self"];
    let ips = self_node["TailscaleIPs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let tailnet = root["CurrentTailnet"]["Name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            root["MagicDNSSuffix"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        });
    let auth_url = root["AuthURL"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let peers: BTreeMap<String, Value> = root["Peer"]
        .as_object()
        .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    let mut peers: Vec<Peer> = peers
        .into_iter()
        .map(|(key, peer)| Peer {
            id: key,
            hostname: peer["HostName"].as_str().unwrap_or("?").to_owned(),
            dns_name: clean_dns_name(peer["DNSName"].as_str()),
            ip: peer["TailscaleIPs"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(Value::as_str)
                .unwrap_or("—")
                .to_owned(),
            online: peer["Online"].as_bool().unwrap_or(false),
            os: peer["OS"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            exit_node: peer["ExitNode"].as_bool().unwrap_or(false),
        })
        .collect();
    peers.sort_by(|a, b| {
        b.online
            .cmp(&a.online)
            .then_with(|| a.hostname.to_lowercase().cmp(&b.hostname.to_lowercase()))
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(TailscaleStatus {
        backend: root["BackendState"]
            .as_str()
            .unwrap_or("Unknown")
            .to_owned(),
        tailnet,
        auth_url,
        hostname: self_node["HostName"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        dns_name: clean_dns_name(self_node["DNSName"].as_str()),
        ips,
        peers,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        AppModel, COPY_TRANSITION_DURATION, DisclosureMotion, EXPANSION_DURATION, Message, Peer,
        REFRESH_FEEDBACK_DURATION, TailscaleStatus, applet_menu_row_style, drawer_content_opacity,
        footer_corner_radii, parse_ping, parse_status, row_corner_radii,
    };
    use cosmic::Application;
    use std::time::Instant;

    #[test]
    fn card_edge_rows_match_the_card_radius() {
        let radius = [8.0, 7.0, 6.0, 5.0];
        assert_eq!(row_corner_radii(radius, true, false), [8.0, 7.0, 0.0, 0.0]);
        assert_eq!(row_corner_radii(radius, false, true), [0.0, 0.0, 6.0, 5.0]);
        assert_eq!(row_corner_radii(radius, true, true), radius);
        assert_eq!(row_corner_radii(radius, false, false), [0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn footer_corners_follow_the_popup_bottom_edge() {
        assert_eq!(
            footer_corner_radii([7.0, 8.0, 13.0, 14.0]),
            [0.0, 0.0, 13.0, 14.0]
        );
    }

    #[test]
    fn applet_row_hover_uses_its_card_edge_radii() {
        let radius = [8.0, 8.0, 0.0, 0.0];
        let class = applet_menu_row_style(radius);
        let style = cosmic::widget::button::Catalog::hovered(
            &cosmic::theme::active(),
            false,
            false,
            &class,
        );
        assert_eq!(style.border_radius, radius.into());
    }

    #[test]
    fn parses_direct_and_relayed_pongs() {
        assert_eq!(
            parse_ping("pong from box (100.64.0.2) via 192.168.1.5:41641 in 4ms\n"),
            Some((4, true))
        );
        assert_eq!(
            parse_ping("pong from box (100.64.0.2) via DERP(nyc) in 23.6ms\n"),
            Some((24, false))
        );
        assert_eq!(parse_ping("no matching peer\n"), None);
    }

    #[test]
    fn parses_tailscale_status_with_metadata_and_unique_peer_ids() {
        let json = br#"{
            "BackendState": "Running",
            "AuthURL": "",
            "CurrentTailnet": { "Name": "acme.ts.net" },
            "Self": {
                "HostName": "workstation",
                "DNSName": "workstation.acme.ts.net.",
                "TailscaleIPs": ["100.64.0.1", "fd7a:115c:a1e0::1"]
            },
            "Peer": {
                "nodekey:bbb": {
                    "HostName": "nas",
                    "DNSName": "nas.acme.ts.net.",
                    "TailscaleIPs": ["100.64.0.9"],
                    "Online": false,
                    "OS": "linux",
                    "ExitNode": false
                },
                "nodekey:aaa": {
                    "HostName": "exit-gw",
                    "DNSName": "exit-gw.acme.ts.net.",
                    "TailscaleIPs": ["100.64.0.2"],
                    "Online": true,
                    "OS": "linux",
                    "ExitNode": true
                }
            }
        }"#;
        let status = parse_status(json).expect("valid status json");
        assert_eq!(status.backend, "Running");
        assert_eq!(status.tailnet.as_deref(), Some("acme.ts.net"));
        assert_eq!(status.dns_name.as_deref(), Some("workstation.acme.ts.net"));
        assert_eq!(status.peers.len(), 2);
        // Online peers sort first.
        assert_eq!(status.peers[0].id, "nodekey:aaa");
        assert!(status.peers[0].exit_node);
        assert_eq!(status.peers[1].id, "nodekey:bbb");
        assert!(!status.peers[1].online);
    }

    #[test]
    fn disclosure_motion_reaches_and_reverses_proportionally() {
        let started_at = Instant::now();
        let opening = DisclosureMotion::with_start("peer-1".to_owned(), 0.0, 1.0, started_at);
        assert_eq!(opening.duration, EXPANSION_DURATION);
        assert_eq!(opening.progress_at(started_at), 0.0);
        assert_eq!(opening.progress_at(started_at + EXPANSION_DURATION), 1.0);

        // Mid-transition reversal scales duration proportionally to remaining distance.
        let closing = DisclosureMotion::with_start("peer-1".to_owned(), 0.4, 0.0, started_at);
        assert!(closing.duration < EXPANSION_DURATION);
        let mid = closing.progress_at(started_at + closing.duration / 2);
        assert!(mid > 0.05 && mid < 0.15, "unexpected mid value: {mid}");
        assert_eq!(closing.progress_at(started_at + closing.duration), 0.0);
        assert!(closing.is_complete(started_at + closing.duration));

        // Hierarchical drawer content opacity stays zero until container clears 55%.
        assert_eq!(drawer_content_opacity(0.0), 0.0);
        assert_eq!(drawer_content_opacity(0.55), 0.0);
        assert!(drawer_content_opacity(0.775) > 0.45 && drawer_content_opacity(0.775) < 0.55);
        assert_eq!(drawer_content_opacity(1.0), 1.0);
    }

    #[test]
    fn repeated_copy_ignores_stale_clear_timer() {
        let mut app = AppModel::default();
        let value = "100.64.0.1".to_owned();
        let _ = app.update(Message::Copy(value.clone()));
        let old_id = app.copy_request_id;
        let _ = app.update(Message::Copy(value.clone()));
        let _ = app.update(Message::ClearCopied(value.clone(), old_id));
        assert_eq!(app.copied.as_deref(), Some(value.as_str()));
        let _ = app.update(Message::ClearCopied(value, app.copy_request_id));
        assert!(app.copied.is_none());
    }

    #[test]
    fn copy_crossfade_and_refresh_feedback_respect_motion_preference() {
        let mut app = AppModel::default();
        app.set_animations_enabled(true);

        let _ = app.update(Message::Copy("100.64.0.1".to_owned()));
        assert_eq!(app.copied.as_deref(), Some("100.64.0.1"));
        assert_eq!(app.copy_motions.len(), 1);
        assert!(app.has_active_animations());
        assert_eq!(
            app.copy_progress("100.64.0.1", Instant::now() + COPY_TRANSITION_DURATION),
            1.0
        );
        // After the 140 ms fade-in settles, ClearCopied animates 1.0 -> 0.0.
        app.copy_motions.clear();

        let _ = app.update(Message::ClearCopied(
            "100.64.0.1".to_owned(),
            app.copy_request_id,
        ));
        assert!(app.copied.is_none());
        assert_eq!(app.copy_motions.len(), 1);
        assert_eq!(
            app.copy_progress("100.64.0.1", Instant::now() + COPY_TRANSITION_DURATION),
            0.0
        );

        // Manual refresh spins once under motion ON.
        let _ = app.update(Message::Refresh);
        assert!(app.refresh_spin_started.is_some());
        assert!(!app.refresh_confirmed);
        let end_angle = app.refresh_spin_radians(Instant::now() + REFRESH_FEEDBACK_DURATION);
        assert!((end_angle - std::f32::consts::TAU).abs() < 0.01);

        // Switching to reduced motion clears in-flight motion cleanly.
        app.set_animations_enabled(false);
        assert!(!app.has_active_animations());
        assert_eq!(app.refresh_spin_radians(Instant::now()), 0.0);

        // Manual refresh under reduced motion uses static confirmation instead of spin.
        app.is_refreshing = false;
        let _ = app.update(Message::Refresh);
        assert!(app.refresh_spin_started.is_none());
        assert!(app.refresh_confirmed);

        // Resetting popup clears all transient feedback and disclosure state.
        app.expanded_peer = Some("peer-1".to_owned());
        app.search_query = "box".to_owned();
        app.reset_popup_state();
        assert!(app.expanded_peer.is_none());
        assert!(app.search_query.is_empty());
        assert!(!app.refresh_confirmed);
        assert!(app.copy_motions.is_empty());
    }

    #[test]
    fn reduced_motion_keeps_disclosure_immediate() {
        let mut app = AppModel::default();
        app.set_animations_enabled(false);

        app.toggle_peer_expansion("peer-1".to_owned());

        assert_eq!(app.expanded_peer.as_deref(), Some("peer-1"));
        assert!(app.expansions.is_empty());
    }

    #[test]
    fn switching_expanded_peer_animates_both_drawers() {
        let mut app = AppModel::default();
        app.set_animations_enabled(true);
        app.expanded_peer = Some("peer-1".to_owned());

        app.toggle_peer_expansion("peer-2".to_owned());

        assert_eq!(app.expanded_peer.as_deref(), Some("peer-2"));
        assert_eq!(app.expansions.len(), 2);
        assert!(
            app.expansions
                .iter()
                .any(|m| m.peer_id == "peer-1" && m.from == 1.0 && m.to == 0.0)
        );
        assert!(
            app.expansions
                .iter()
                .any(|m| m.peer_id == "peer-2" && m.from == 0.0 && m.to == 1.0)
        );
    }

    #[test]
    fn stale_status_response_is_ignored_and_fresh_success_clears_error() {
        let mut app = AppModel {
            error: Some("previous error".to_owned()),
            status_request_id: 5,
            ..Default::default()
        };

        // Stale response (id 4) must be ignored.
        let _ = app.update(Message::StatusLoaded(
            4,
            Ok(TailscaleStatus {
                backend: "Stopped".to_owned(),
                ..Default::default()
            }),
        ));
        assert!(app.status.is_none());
        assert!(app.error.is_some());

        // Matching response (id 5) updates status and clears stale error.
        let _ = app.update(Message::StatusLoaded(
            5,
            Ok(TailscaleStatus {
                backend: "Running".to_owned(),
                ..Default::default()
            }),
        ));
        assert_eq!(
            app.status.as_ref().map(|s| s.backend.as_str()),
            Some("Running")
        );
        assert!(app.error.is_none());
    }

    #[test]
    fn search_filters_by_hostname_ip_and_os_and_escape_clears_it() {
        let mut app = AppModel::default();
        let peers = vec![
            Peer {
                id: "1".to_owned(),
                hostname: "arch-desktop".to_owned(),
                dns_name: Some("arch-desktop.acme.ts.net".to_owned()),
                ip: "100.64.0.2".to_owned(),
                online: true,
                os: Some("linux".to_owned()),
                exit_node: false,
            },
            Peer {
                id: "2".to_owned(),
                hostname: "pixel-phone".to_owned(),
                dns_name: None,
                ip: "100.64.0.9".to_owned(),
                online: false,
                os: Some("android".to_owned()),
                exit_node: false,
            },
        ];
        app.search_query = "android".to_owned();
        assert_eq!(app.filtered_peers(&peers).len(), 1);
        assert_eq!(app.filtered_peers(&peers)[0].id, "2");

        let _ = app.update(Message::EscapePressed);
        assert!(app.search_query.is_empty());
        assert_eq!(app.filtered_peers(&peers).len(), 2);
    }

    #[test]
    fn escape_closes_popup_when_search_and_drawer_are_clear() {
        let mut app = AppModel {
            popup: Some(cosmic::iced::window::Id::unique()),
            ..Default::default()
        };
        let _ = app.update(Message::EscapePressed);
        assert!(app.popup.is_none());
        let _ = app.update(Message::EscapePressed);
        assert!(app.popup.is_none());
    }

    #[test]
    fn frame_driver_drives_popup_redraws_and_settles_to_wait() {
        use cosmic::iced::advanced::{Layout, Shell, clipboard, widget::Tree};
        use cosmic::iced::{Event, Limits, Pixels, Rectangle, Size, mouse, window};
        use std::time::Duration;

        let renderer = cosmic::Renderer::new(cosmic::font::default(), Pixels(14.0));
        let mut cb = clipboard::Null;
        let viewport = Rectangle::with_size(Size::new(420.0, 680.0));
        let limits = Limits::new(Size::ZERO, viewport.size());

        let mut app = AppModel {
            status: Some(TailscaleStatus {
                backend: "Running".to_owned(),
                hostname: "workstation".to_owned(),
                ips: vec!["100.64.0.1".to_owned()],
                peers: vec![Peer {
                    id: "peer-1".to_owned(),
                    hostname: "orangepi".to_owned(),
                    dns_name: Some("orangepi.acme.ts.net".to_owned()),
                    ip: "100.64.0.2".to_owned(),
                    online: true,
                    os: Some("linux".to_owned()),
                    exit_node: false,
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        app.set_animations_enabled(true);
        app.toggle_peer_expansion("peer-1".to_owned());
        assert!(app.has_active_animations());

        let mut el = app.popup_view();
        let mut tree = Tree::new(&el);
        let mut node = el.as_widget_mut().layout(&mut tree, &renderer, &limits);

        // Pass 0 on a mid-animation vsync emits exactly 1 AnimationFrame + NextFrame.
        let t1 = Instant::now() + Duration::from_millis(16);
        let ev1 = Event::Window(window::Event::RedrawRequested(t1));
        let mut msgs = Vec::new();
        let rr0 = {
            let mut shell = Shell::new(&mut msgs);
            el.as_widget_mut().update(
                &mut tree,
                &ev1,
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut cb,
                &mut shell,
                &viewport,
            );
            shell.redraw_request()
        };
        assert_eq!(msgs.len(), 1);
        assert!(matches!(msgs[0], Message::AnimationFrame(ts) if ts == t1));
        assert_eq!(rr0, window::RedrawRequest::NextFrame);

        // Apply AnimationFrame, rebuild + diff tree (as iced_winit does before Pass 1).
        drop(el);
        for msg in msgs.drain(..) {
            let _ = app.update(msg);
        }
        assert!(app.has_active_animations());
        let mut el = app.popup_view();
        tree.diff(&mut el);
        node = el.as_widget_mut().layout(&mut tree, &renderer, &limits);

        // Pass 1 with the same RedrawRequested(t1) must dedupe (0 messages) yet keep NextFrame.
        let rr1 = {
            let mut shell = Shell::new(&mut msgs);
            el.as_widget_mut().update(
                &mut tree,
                &ev1,
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut cb,
                &mut shell,
                &viewport,
            );
            shell.redraw_request()
        };
        assert!(msgs.is_empty());
        assert_eq!(rr1, window::RedrawRequest::NextFrame);

        // Final frame after EXPANSION_DURATION settles animations and returns RedrawRequest::Wait.
        let t_end = t1 + EXPANSION_DURATION + Duration::from_millis(10);
        let ev_end = Event::Window(window::Event::RedrawRequested(t_end));
        {
            let mut shell = Shell::new(&mut msgs);
            el.as_widget_mut().update(
                &mut tree,
                &ev_end,
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut cb,
                &mut shell,
                &viewport,
            );
        }
        assert_eq!(msgs.len(), 1);
        drop(el);
        for msg in msgs.drain(..) {
            let _ = app.update(msg);
        }
        assert!(!app.has_active_animations());

        let mut el = app.popup_view();
        tree.diff(&mut el);
        node = el.as_widget_mut().layout(&mut tree, &renderer, &limits);
        let rr_idle = {
            let mut shell = Shell::new(&mut msgs);
            el.as_widget_mut().update(
                &mut tree,
                &ev_end,
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut cb,
                &mut shell,
                &viewport,
            );
            shell.redraw_request()
        };
        assert!(msgs.is_empty());
        assert_eq!(rr_idle, window::RedrawRequest::Wait);
    }
}
