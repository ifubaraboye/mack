use chrono::{DateTime, Datelike, Days, Local, NaiveDate, Utc};
use gpui::{KeyBinding, actions};

use super::*;

actions!(waku_sidebar, [CancelSessionRename, CancelGroupRename]);

const SESSION_RENAME_PARENT_CONTEXT: &str = "SessionRename";
const SESSION_RENAME_FIELD_CONTEXT: &str = "SessionRename > TextInput";
const GROUP_RENAME_PARENT_CONTEXT: &str = "GroupRename";
const GROUP_RENAME_FIELD_CONTEXT: &str = "GroupRename > TextInput";

/// Keep Escape inside the focused inline editor so it cancels the rename,
/// rather than falling through to the window-wide Stop action.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new(
            "escape",
            CancelSessionRename,
            Some(SESSION_RENAME_FIELD_CONTEXT),
        ),
        KeyBinding::new(
            "escape",
            CancelGroupRename,
            Some(GROUP_RENAME_FIELD_CONTEXT),
        ),
    ]);
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum SessionDateGroup {
    Today,
    Yesterday,
    ThisWeek,
    ThisMonth,
    ThisYear,
    More,
}

impl SessionDateGroup {
    const ALL: [Self; 6] = [
        Self::Today,
        Self::Yesterday,
        Self::ThisWeek,
        Self::ThisMonth,
        Self::ThisYear,
        Self::More,
    ];

    fn index(self) -> usize {
        match self {
            Self::Today => 0,
            Self::Yesterday => 1,
            Self::ThisWeek => 2,
            Self::ThisMonth => 3,
            Self::ThisYear => 4,
            Self::More => 5,
        }
    }

    fn label(self) -> String {
        match self {
            Self::Today => tr!("sidebar.today"),
            Self::Yesterday => tr!("sidebar.yesterday"),
            Self::ThisWeek => tr!("sidebar.this_week"),
            Self::ThisMonth => tr!("sidebar.this_month"),
            Self::ThisYear => tr!("sidebar.this_year"),
            Self::More => tr!("sidebar.more"),
        }
    }
}

/// Stable identity for a collapsible sidebar section.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum SidebarGroup {
    Updated(SessionDateGroup),
    /// A user-defined chat group. Renders like the old project sections
    /// did — folder icon plus the group name — ahead of the date sections,
    /// in registry order.
    ChatGroup(Uuid),
}

impl SidebarGroup {
    fn element_key(self) -> SharedString {
        match self {
            Self::Updated(group) => format!("updated-{}", group.index()).into(),
            Self::ChatGroup(group_id) => format!("group-{group_id}").into(),
        }
    }

    fn mix_fingerprint(self, fingerprint: u64) -> u64 {
        match self {
            Self::Updated(group) => mix(fingerprint, group.index() as u64 + 1),
            Self::ChatGroup(group_id) => mix_uuid(mix(fingerprint, 0x300), group_id),
        }
    }
}

fn sidebar_ordering_label(ordering: SidebarOrdering) -> String {
    match ordering {
        SidebarOrdering::Newest => tr!("sidebar.ordering_newest"),
        SidebarOrdering::Oldest => tr!("sidebar.ordering_oldest"),
    }
}

fn session_date_group(timestamp: u64, today: NaiveDate) -> SessionDateGroup {
    let session_date = i64::try_from(timestamp)
        .ok()
        .and_then(|timestamp| DateTime::<Utc>::from_timestamp(timestamp, 0))
        .map(|timestamp| timestamp.with_timezone(&Local).date_naive())
        .unwrap_or(today);
    session_date_group_for_dates(session_date, today)
}

fn session_date_group_for_dates(session_date: NaiveDate, today: NaiveDate) -> SessionDateGroup {
    if session_date >= today {
        return SessionDateGroup::Today;
    }

    if today.pred_opt() == Some(session_date) {
        return SessionDateGroup::Yesterday;
    }

    let week_start = today
        .checked_sub_days(Days::new(today.weekday().num_days_from_monday().into()))
        .unwrap_or(today);
    if session_date >= week_start {
        return SessionDateGroup::ThisWeek;
    }

    if session_date.year() == today.year() && session_date.month() == today.month() {
        return SessionDateGroup::ThisMonth;
    }

    if session_date.year() == today.year() {
        return SessionDateGroup::ThisYear;
    }

    SessionDateGroup::More
}

fn session_group_header(theme: &Theme) -> Div {
    div()
        .h(px(SIDEBAR_GROUP_HEADER_HEIGHT))
        .px(px(8.0))
        .flex()
        .items_center()
        .text_size(sp(13.0))
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme.text_secondary)
}

fn append_sidebar_group_rows(
    rows: &mut Vec<SidebarRow>,
    group: SidebarGroup,
    sessions: &[Uuid],
    collapsed: bool,
) {
    if sessions.is_empty() {
        return;
    }

    rows.push(SidebarRow::Header(group));
    if !collapsed {
        rows.extend(sessions.iter().copied().map(SidebarRow::Session));
    }
    rows.push(SidebarRow::GroupSpacer);
}

fn updater_button_available_content(
    foreground: Hsla,
    label: SharedString,
    label_reveal: f32,
) -> Div {
    div()
        .relative()
        .size_full()
        .child(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .opacity(1.0 - label_reveal)
                .child(icon("icons/download.svg", 12.0, foreground)),
        )
        .child(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .whitespace_nowrap()
                .opacity(label_reveal)
                .child(label),
        )
}

/// Height of a session card plus the separation reserved beneath it in the
/// virtualized sidebar list. Keep the gap inside the list row so measured and
/// estimated heights stay identical for off-screen sessions.
const SIDEBAR_SESSION_CARD_HEIGHT: f32 = 51.0;
const SIDEBAR_SESSION_ROW_GAP: f32 = 1.0;
const SIDEBAR_SESSION_ROW_HEIGHT: f32 = SIDEBAR_SESSION_CARD_HEIGHT + SIDEBAR_SESSION_ROW_GAP;
const SIDEBAR_ACTION_ROW_HEIGHT: f32 = 32.0;
const SIDEBAR_SEARCH_BOTTOM_GAP: f32 = 10.0;
const SIDEBAR_GROUP_HEADER_HEIGHT: f32 = 28.0;
const SIDEBAR_GROUP_HEADER_BOTTOM_GAP: f32 = 2.0;
const SIDEBAR_GROUP_SPACER_HEIGHT: f32 = 10.0;
const SIDEBAR_GROUP_GUIDE_X: f32 = 15.0;
const SIDEBAR_GROUP_CHILD_PADDING: f32 = 28.0;

/// The session row's trailing time: how long the live turn has been working,
/// or how long ago the agent last replied. A session that has never replied
/// shows nothing.
pub(super) fn session_time_label(session: &AgentSession, now: u64) -> Option<String> {
    if session.status == SessionStatus::Background {
        return Some(tr!("sidebar.status_background"));
    }
    if session.is_busy()
        && let Some(turn) = session
            .turns
            .last()
            .filter(|turn| turn.status == TurnStatus::Running)
    {
        return Some(tr!(
            "sidebar.working",
            elapsed = format_working_elapsed(now.saturating_sub(turn.started_at))
        ));
    }
    session
        .last_reply_at
        .map(|last_reply_at| format_time_ago(now.saturating_sub(last_reply_at)))
}

/// Recency for sidebar ordering and date groups. A submitted turn promotes the
/// task immediately, while metadata edits such as a rename do not; a task with
/// no turns stays anchored to when it was created.
fn sidebar_session_timestamp(session: &AgentSession) -> u64 {
    session.last_reply_at.unwrap_or(session.created_at)
}

fn sort_sidebar_sessions(sessions: &mut Vec<&AgentSession>, ordering: SidebarOrdering) {
    match ordering {
        SidebarOrdering::Newest => {
            sessions.sort_by_key(|session| std::cmp::Reverse(sidebar_session_timestamp(session)))
        }
        SidebarOrdering::Oldest => {
            sessions.sort_by_key(|session| sidebar_session_timestamp(session))
        }
    }
}

/// Split the sorted sessions into user-defined chat-group sections plus the
/// remainder. Groups render ahead of the date sections in registry
/// order; a session whose group id is missing from the registry renders
/// ungrouped, so deleting a group entry can never strand a chat.
fn chat_group_sections<'a>(
    sessions: &[&'a AgentSession],
    groups: &[ChatGroup],
) -> (Vec<(SidebarGroup, Vec<Uuid>)>, Vec<&'a AgentSession>) {
    if groups.is_empty() {
        return (Vec::new(), sessions.to_vec());
    }
    let known: HashSet<Uuid> = groups.iter().map(|group| group.id).collect();
    let mut sections: Vec<(SidebarGroup, Vec<Uuid>)> = Vec::new();
    let mut indexes = HashMap::new();
    let mut rest = Vec::with_capacity(sessions.len());
    for session in sessions {
        let Some(group_id) = session.group_id.filter(|id| known.contains(id)) else {
            rest.push(*session);
            continue;
        };
        let index = *indexes.entry(group_id).or_insert_with(|| {
            let index = sections.len();
            sections.push((SidebarGroup::ChatGroup(group_id), Vec::new()));
            index
        });
        sections[index].1.push(session.id);
    }
    // Registry order, not first-seen order: renaming or regrouping never
    // shuffles the sections.
    sections.sort_by_key(|(group, _)| {
        let SidebarGroup::ChatGroup(id) = group else {
            return usize::MAX;
        };
        groups
            .iter()
            .position(|known| known.id == *id)
            .unwrap_or(usize::MAX)
    });
    (sections, rest)
}

fn persisted_sidebar_branch_label(_workspace: &SessionWorkspace) -> Option<&str> {
    None
}

/// Compact "how long ago" for the sidebar: "just now", then one coarse unit —
/// "5m", "3h", "420d". Days are the largest unit so a glance still reads as a
/// count rather than a date.
pub(super) fn format_time_ago(seconds: u64) -> String {
    match seconds {
        0..=59 => tr!("sidebar.just_now"),
        60..=3_599 => tr!("sidebar.minutes_ago", count = seconds / 60),
        3_600..=86_399 => tr!("sidebar.hours_ago", count = seconds / 3_600),
        _ => tr!("sidebar.days_ago", count = seconds / 86_400),
    }
}

/// One row of the virtualized sidebar session history.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SidebarRow {
    /// Opens the window-wide command palette and scrolls with history.
    Search,
    /// Opens the palette straight into the Projects view.
    Projects,
    /// Group header; the first row also carries the sidebar actions.
    Header(SidebarGroup),
    /// A started session.
    Session(Uuid),
    /// Spacing between date groups.
    GroupSpacer,
}

fn sidebar_session_row_index(rows: &[SidebarRow], session_id: Uuid) -> Option<usize> {
    rows.iter()
        .position(|row| *row == SidebarRow::Session(session_id))
}

fn sidebar_row_height(row: SidebarRow) -> Pixels {
    px(match row {
        SidebarRow::Search => SIDEBAR_ACTION_ROW_HEIGHT + SIDEBAR_SEARCH_BOTTOM_GAP,
        SidebarRow::Projects => SIDEBAR_ACTION_ROW_HEIGHT,
        SidebarRow::Header(_) => SIDEBAR_GROUP_HEADER_HEIGHT + SIDEBAR_GROUP_HEADER_BOTTOM_GAP,
        SidebarRow::Session(_) => SIDEBAR_SESSION_ROW_HEIGHT,
        SidebarRow::GroupSpacer => SIDEBAR_GROUP_SPACER_HEIGHT,
    })
}

fn sidebar_bottom_aligned_offset(
    rows: &[SidebarRow],
    target: usize,
    viewport_height: Pixels,
) -> ListOffset {
    let mut item_ix = target;
    let mut height = sidebar_row_height(rows[target]);
    while item_ix > 0 && height < viewport_height {
        item_ix -= 1;
        height += sidebar_row_height(rows[item_ix]);
    }
    ListOffset {
        item_ix,
        offset_in_item: (height - viewport_height).max(Pixels::ZERO),
    }
}

fn reveal_sidebar_list_row(list: &ListState, rows: &[SidebarRow], index: usize) {
    let viewport = list.viewport_bounds();
    if viewport.size.height <= Pixels::ZERO {
        return;
    }
    if let Some(item) = list.bounds_for_item(index) {
        if item.top() >= viewport.top() && item.bottom() <= viewport.bottom() {
            return;
        }
        list.scroll_to_reveal_item(index);
    } else if index <= list.logical_scroll_top().item_ix {
        list.scroll_to(ListOffset {
            item_ix: index,
            offset_in_item: Pixels::ZERO,
        });
    } else {
        // Off-screen rows have not necessarily been measured yet. Their
        // sidebar heights are fixed, so align a lower target to the viewport
        // bottom just like scrollIntoView({ block: "nearest" }).
        list.scroll_to(sidebar_bottom_aligned_offset(
            rows,
            index,
            viewport.size.height,
        ));
    }
}

impl Waku {
    pub(super) fn window_drag_region(
        &self,
        region: Stateful<Div>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        // Windows drags from the hit test, not from a mouse-move handler:
        // `DefWindowProc` moves the window once the region reports itself as
        // caption, and performs the user's configured double-click action.
        #[cfg(target_os = "windows")]
        let region = region.window_control_area(gpui::WindowControlArea::Drag);

        region
            .on_click(|event, window, _| {
                if event.click_count() == 2 {
                    crate::platform::titlebar_double_click(window);
                }
            })
            .on_mouse_down_out(cx.listener(|this, _, _, _| {
                this.header_drag_armed = false;
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.header_drag_armed = true;
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.header_drag_armed = false;
                }),
            )
            .on_mouse_move(cx.listener(|this, _, window, _| {
                if this.header_drag_armed {
                    this.header_drag_armed = false;
                    crate::platform::start_window_move(window);
                }
            }))
    }
    // ── Sidebar ────────────────────────────────────────────────────────────

    fn render_fps_counter(&self, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let fps = self.fps_value;
        let dot = if fps == 0 {
            theme.text_ghost
        } else if fps >= 55 {
            theme.success
        } else if fps >= 30 {
            theme.warning
        } else {
            theme.danger
        };
        div()
            .flex_none()
            .h(px(26.0))
            .px(px(6.0))
            .flex()
            .items_center()
            .gap(px(5.0))
            .text_size(sp(12.5))
            .line_height(sp(0.0))
            .child(div().w(px(6.0)).h(px(6.0)).rounded_full().bg(dot))
            .child(
                div()
                    .text_color(theme.text_tertiary)
                    .font_family(crate::md::render::MONO_FAMILY)
                    .child(SharedString::from(format!("{fps} FPS"))),
            )
    }

    fn render_sidebar_toggle(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = Theme::current(cx);
        div()
            .id("toggle-sidebar")
            .w(px(26.0))
            .h(px(26.0))
            .flex_none()
            .rounded(px(6.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .hover(|element| element.bg(theme.overlay))
            .active(|element| element.bg(theme.overlay_strong))
            .child(icon("icons/panel-left.svg", 14.0, theme.text_tertiary))
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                this.set_sidebar_visible(!this.sidebar_visible, cx);
            }))
    }

    pub(super) fn render_history_button(
        &self,
        id: &'static str,
        icon_path: &'static str,
        enabled: bool,
        navigate_back: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = Theme::current(cx);
        div()
            .id(id)
            .w(px(26.0))
            .h(px(26.0))
            .flex_none()
            .rounded(px(6.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .when(!enabled, |element| element.opacity(0.35))
            .when(enabled, |element| {
                element
                    .hover(|element| element.bg(theme.overlay))
                    .active(|element| element.bg(theme.overlay_strong))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        if navigate_back {
                            this.navigate_back_action(&NavigateBack, window, cx);
                        } else {
                            this.navigate_forward_action(&NavigateForward, window, cx);
                        }
                    }))
            })
            .child(icon(icon_path, 14.0, theme.text_tertiary))
    }

    fn render_sidebar_titlebar(&self, window: &Window, cx: &mut Context<Self>) -> Stateful<Div> {
        div()
            .id("sidebar-titlebar")
            .h(px(48.0))
            .flex_none()
            .flex()
            .items_center()
            .children(self.render_client_window_controls(
                super::window_chrome::WindowControlSide::Left,
                window,
                cx,
            ))
            .child(
                self.window_drag_region(
                    div()
                        .id("sidebar-traffic-light-drag-region")
                        .w(px(TRAFFIC_LIGHT_CLEARANCE))
                        .h_full()
                        .flex_none(),
                    cx,
                ),
            )
            .child(self.render_sidebar_toggle(cx))
            .child(
                div()
                    .ml(px(6.0))
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .child(self.render_history_button(
                        "navigate-back",
                        "icons/arrow-left.svg",
                        !self.session_navigation.back.is_empty(),
                        true,
                        cx,
                    ))
                    .child(self.render_history_button(
                        "navigate-forward",
                        "icons/arrow-right.svg",
                        !self.session_navigation.forward.is_empty(),
                        false,
                        cx,
                    )),
            )
            .child(self.window_drag_region(
                div().id("sidebar-titlebar-drag-region").h_full().flex_1(),
                cx,
            ))
    }

    fn render_sidebar_header_actions(&self, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let menu = self.menu_handle("sidebar-options", cx);
        let menu_open = menu.is_open();
        let weak = cx.entity().downgrade();
        let ordering = self.state.sidebar_ordering;
        let options = dropdown_menu(
            div()
                .id("sidebar-options")
                .w(px(20.0))
                .h(px(20.0))
                .rounded(px(6.0))
                .flex()
                .items_center()
                .justify_center()
                .cursor_default()
                .focus_visible(|style| style.border_1().border_color(theme.accent))
                .when(menu_open, |element| element.bg(theme.overlay_strong))
                .hover(|element| element.bg(theme.overlay))
                .active(|element| element.bg(theme.overlay_strong))
                .tooltip(Tooltip::text(tr!("sidebar.options")))
                .child(icon("icons/list-filter.svg", 14.0, theme.text_secondary)),
            "sidebar-options-menu",
            &menu,
            MenuAlign::BelowLeft,
            move |_| {
                let ordering_weak = weak.clone();
                vec![
                    MenuItem::submenu_with_value(
                        tr!("sidebar.ordering"),
                        sidebar_ordering_label(ordering),
                        move |_| {
                            let newest_weak = ordering_weak.clone();
                            let oldest_weak = ordering_weak.clone();
                            vec![
                                MenuItem::new(tr!("sidebar.ordering_newest"), move |_, cx| {
                                    let _ = newest_weak.update(cx, |this, cx| {
                                        this.set_sidebar_ordering(SidebarOrdering::Newest, cx);
                                    });
                                })
                                .selected(ordering == SidebarOrdering::Newest),
                                MenuItem::new(tr!("sidebar.ordering_oldest"), move |_, cx| {
                                    let _ = oldest_weak.update(cx, |this, cx| {
                                        this.set_sidebar_ordering(SidebarOrdering::Oldest, cx);
                                    });
                                })
                                .selected(ordering == SidebarOrdering::Oldest),
                            ]
                        },
                    ),
                ]
            },
        );
        div().flex().items_center().gap(px(2.0)).child(options)
    }

    fn render_sidebar_action_row(
        &self,
        id: &'static str,
        icon_path: &'static str,
        label: String,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = Theme::current(cx);
        div()
            .id(id)
            .tab_index(0)
            .w_full()
            .h(px(SIDEBAR_ACTION_ROW_HEIGHT))
            .flex_none()
            .px(px(4.0))
            .rounded(px(7.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .cursor_default()
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .hover(|element| element.bg(theme.sidebar_item_background))
            .active(|element| element.bg(theme.overlay_strong))
            .child(
                div()
                    .size(px(20.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icon(icon_path, 14.0, theme.text_secondary)),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(sp(13.0))
                    .text_color(theme.text_secondary)
                    .child(label),
            )
    }

    fn render_sidebar_new_session(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        self.render_sidebar_action_row(
            "sidebar-new-session",
            "icons/compose.svg",
            tr!("menu.new_task"),
            cx,
        )
        .on_click(cx.listener(|this, _, window, cx| {
            this.new_session_action(&NewSession, window, cx);
        }))
        .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                this.new_session_action(&NewSession, window, cx);
                cx.stop_propagation();
            }
        }))
    }

    fn render_sidebar_search(&self, cx: &mut Context<Self>) -> Div {
        let search = self
            .render_sidebar_action_row(
                "sidebar-search",
                "icons/search.svg",
                tr!("sidebar.search"),
                cx,
            )
            .on_click(cx.listener(|this, _, window, cx| {
                this.toggle_command_palette_action(&ToggleCommandPalette, window, cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.toggle_command_palette_action(&ToggleCommandPalette, window, cx);
                    cx.stop_propagation();
                }
            }));
        div()
            .w_full()
            .h(px(SIDEBAR_ACTION_ROW_HEIGHT + SIDEBAR_SEARCH_BOTTOM_GAP))
            .flex_none()
            .child(search)
    }

    fn render_sidebar_projects(&self, cx: &mut Context<Self>) -> Div {
        let projects = self
            .render_sidebar_action_row(
                "sidebar-projects",
                "icons/folder.svg",
                tr!("sidebar.projects"),
                cx,
            )
            .on_click(cx.listener(|this, _, window, cx| {
                this.open_projects_palette(window, cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.open_projects_palette(window, cx);
                    cx.stop_propagation();
                }
            }));
        div()
            .w_full()
            .h(px(SIDEBAR_ACTION_ROW_HEIGHT))
            .flex_none()
            .child(projects)
    }

    fn start_available_update(&mut self, cx: &mut Context<Self>) {
        if self.updater_status != crate::updater::UpdateStatus::Available {
            return;
        }
        let started = cx
            .try_global::<crate::updater::UpdaterState>()
            .and_then(|state| state.0.as_ref())
            .is_some_and(|updater| updater.install_available_update());
        if started {
            self.updater_status = crate::updater::UpdateStatus::Updating;
            self.reset_updater_button_animation();
            cx.notify();
        }
    }

    fn render_updater_button(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let status = self.updater_status;
        if status == crate::updater::UpdateStatus::Idle {
            return None;
        }

        let theme = Theme::current(cx);
        let foreground = rgb(0xFFFFFF).into();
        let available = status == crate::updater::UpdateStatus::Available;
        let button = div()
            .id("sidebar-update")
            .track_focus(&self.updater_button_focus)
            .when(available, |button| button.tab_index(0))
            .w(px(UPDATER_BUTTON_COLLAPSED_WIDTH))
            .h(px(20.0))
            .flex_none()
            .overflow_hidden()
            .rounded_full()
            .relative()
            .cursor_default()
            .bg(theme.gauge)
            .text_color(foreground)
            .text_size(sp(12.5))
            .font_weight(FontWeight::MEDIUM)
            .when(available, |button| {
                button
                    .hover(|style| style.opacity(0.92))
                    .focus_visible(|style| style.border_1().border_color(rgb(0xFFFFFF)))
                    .active(|style| style.opacity(0.8))
                    .on_hover(cx.listener(|this, hovering: &bool, _, cx| {
                        this.set_updater_button_hovered(*hovering, cx);
                    }))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.start_available_update(cx);
                    }))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.start_available_update(cx);
                            cx.stop_propagation();
                        }
                    }))
            });

        if !available {
            let indicator = motion::spin_slow(icon("icons/loader-circle.svg", 14.0, foreground));
            return Some(
                button
                    .tooltip(Tooltip::text(tr!("updater.updating")))
                    .child(
                        div()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(indicator),
                    )
                    .into_any_element(),
            );
        }

        let label: SharedString = tr_cow!("updater.update").into();
        let animation_generation = self.updater_button_animation_generation;
        if animation_generation == 0 {
            return Some(
                button
                    .child(updater_button_available_content(foreground, label, 0.0))
                    .into_any_element(),
            );
        }

        let from_width = self.updater_button_animation_from_width;
        let from_reveal = self.updater_button_animation_from_reveal;
        let target_width = if self.updater_button_expanded() {
            UPDATER_BUTTON_EXPANDED_WIDTH
        } else {
            UPDATER_BUTTON_COLLAPSED_WIDTH
        };
        let target_reveal = if self.updater_button_expanded() {
            1.0
        } else {
            0.0
        };
        let current_width = self.updater_button_width.clone();
        let current_reveal = self.updater_button_label_reveal.clone();

        Some(
            button
                .with_animation(
                    SharedString::from(format!("sidebar-updater-expand-{animation_generation}")),
                    Animation::new(Duration::from_millis(150)).with_easing(ease_out_quint()),
                    move |button, delta| {
                        let width = from_width + (target_width - from_width) * delta;
                        let reveal = from_reveal + (target_reveal - from_reveal) * delta;
                        current_width.set(width);
                        current_reveal.set(reveal);
                        button.w(px(width)).child(updater_button_available_content(
                            foreground,
                            label.clone(),
                            reveal,
                        ))
                    },
                )
                .into_any_element(),
        )
    }

    fn render_sidebar_footer(&self, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        div()
            .flex_none()
            .h(px(40.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .child(
                div()
                    .id("open-settings")
                    .tab_index(0)
                    .focus_visible(|style| style.border_1().border_color(theme.accent))
                    .w(px(26.0))
                    .h(px(26.0))
                    .flex_none()
                    .rounded(px(6.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_default()
                    .hover(|element| element.bg(theme.overlay))
                    .active(|element| element.bg(theme.overlay_strong))
                    .tooltip(Tooltip::text(tr_cow!("common.settings")))
                    .child(icon("icons/settings.svg", 14.0, theme.text_tertiary))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_settings_action(&OpenSettings, window, cx);
                    })),
            )
            .child(div().flex_1())
            .when_some(self.render_updater_button(cx), |footer, button| {
                footer.child(button)
            })
    }

    /// Branch labels are hidden in chat mode; keep the entry point so callers
    /// do not need to change.
    fn ensure_sidebar_branch_labels(&self, _cx: &mut Context<Self>) {}

    pub(super) fn cache_sidebar_branch_label(&self, _path: &Path, _branch: Option<&str>) {}

    pub(super) fn render_sidebar(
        &self,
        width: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = Theme::current(cx);
        self.ensure_sidebar_branch_labels(cx);
        let is_resizing = self
            .panel_resize_drag
            .is_some_and(|drag| drag.target == PanelResizeTarget::Sidebar);

        let rows = self.sidebar_rows_cached(Local::now().date_naive(), unix_time());
        self.sync_sidebar_rows(&rows);
        // Restored selection exists before ListState knows the viewport size.
        // Retry after the first layout so nearest-edge alignment has a height.
        if self.sidebar_list_state.viewport_bounds().size.height <= Pixels::ZERO
            && let Some(session_id) = self
                .pending_session_activation
                .map(|pending| pending.session_id)
                .or(self.state.selected_session)
        {
            let entity = cx.entity().downgrade();
            window.on_next_frame(move |_, cx| {
                let _ = entity.update(cx, |this, cx| {
                    let selected_session = this
                        .pending_session_activation
                        .map(|pending| pending.session_id)
                        .or(this.state.selected_session);
                    if selected_session == Some(session_id) {
                        this.reveal_sidebar_session(session_id);
                        cx.notify();
                    }
                });
            });
        }
        let history_scrolled =
            self.sidebar_list_state.scroll_px_offset_for_scrollbar().y < px(-0.5);
        let entity = cx.entity().downgrade();

        div()
            .w(px(width))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(if is_resizing {
                theme.sidebar_drag_background
            } else {
                theme.sidebar
            })
            .child(self.render_sidebar_titlebar(window, cx))
            .child(
                div()
                    .flex_none()
                    .px(px(10.0))
                    .child(self.render_sidebar_new_session(cx)),
            )
            .child(
                div()
                    .id("sidebar-scroll")
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(
                        div().px(px(10.0)).size_full().child(
                            list(
                                self.sidebar_list_state.clone(),
                                move |index, _window, cx| {
                                    entity
                                        .upgrade()
                                        .map(|entity| {
                                            entity.update(cx, |this, cx| {
                                                this.sidebar_row(index, &rows, cx)
                                            })
                                        })
                                        .unwrap_or_else(|| div().into_any_element())
                                },
                            )
                            .size_full(),
                        ),
                    )
                    .child(scrollbar::vertical(
                        &self.sidebar_list_state,
                        &self.sidebar_scrollbar,
                    ))
                    .when(history_scrolled, |scroll| {
                        scroll.child(
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .w_full()
                                .h(px(1.0))
                                .bg(theme.border),
                        )
                    }),
            )
            .child(self.render_sidebar_footer(cx))
    }

    /// Keep a newly selected task visible without disturbing the sidebar when
    /// its row is already fully inside the viewport.
    pub(super) fn reveal_sidebar_session(&self, session_id: Uuid) {
        let rows = self.sidebar_rows_cached(Local::now().date_naive(), unix_time());
        self.sync_sidebar_rows(&rows);
        if let Some(index) = sidebar_session_row_index(&rows, session_id) {
            reveal_sidebar_list_row(&self.sidebar_list_state, &rows, index);
        }
    }

    /// The sidebar row snapshot, rebuilt only when its inputs move.
    ///
    /// The sidebar re-renders at pulse cadence whenever one of its session
    /// rows shows a working spinner, and rebuilding the snapshot sorts every
    /// started session and runs calendar math per session — far too much per
    /// tick for values that move at most once per stream commit. The
    /// fingerprint is an allocation-free scan of exactly what
    /// [`Self::sidebar_rows`] reads: started sessions with their project,
    /// group, and recency, the ordering preference, the group registry, the
    /// collapsed-group set, and today's date.
    fn sidebar_rows_cached(&self, today: NaiveDate, now: u64) -> Rc<Vec<SidebarRow>> {
        let mut         fingerprint = mix(0x51de_ba5e_5eed_c0de, today.num_days_from_ce() as u64);
        fingerprint = mix(
            fingerprint,
            match self.state.sidebar_ordering {
                SidebarOrdering::Newest => 1,
                SidebarOrdering::Oldest => 2,
            },
        );
        for session in &self.state.sessions {
            if !session.has_started() {
                continue;
            }
            fingerprint = mix_uuid(fingerprint, session.id);
            fingerprint = mix_uuid(fingerprint, session.project_id);
            fingerprint = mix(fingerprint, sidebar_session_timestamp(session));
            if let Some(group_id) = session.group_id {
                fingerprint = mix_uuid(fingerprint, group_id);
            }
        }
        // A set has no stable iteration order; combine order-independently.
        let collapsed = self
            .sidebar_collapsed_groups
            .iter()
            .fold(0u64, |combined, group| {
                combined.wrapping_add(group.mix_fingerprint(0))
            });
        fingerprint = mix(
            mix(fingerprint, self.sidebar_collapsed_groups.len() as u64),
            collapsed,
        );
        // Registry order is render order; names paint the headers.
        fingerprint = mix(fingerprint, self.state.chat_groups.len() as u64);
        for group in &self.state.chat_groups {
            fingerprint = mix_uuid(fingerprint, group.id);
            fingerprint = mix(
                fingerprint,
                group.name.bytes().fold(0u64, |hash, byte| {
                    hash.wrapping_mul(31).wrapping_add(byte as u64)
                }),
            );
        }
        if self.sidebar_rows_fingerprint.get() != Some(fingerprint) {
            *self.sidebar_rows_snapshot.borrow_mut() = Rc::new(self.sidebar_rows(today, now));
            self.sidebar_rows_fingerprint.set(Some(fingerprint));
        }
        self.sidebar_rows_snapshot.borrow().clone()
    }

    /// Snapshot the session history as a flat list of lightweight rows:
    /// chat-group sections first, then one date section per recency period.
    fn sidebar_rows(&self, today: NaiveDate, now: u64) -> Vec<SidebarRow> {
        let mut sorted_sessions = self
            .state
            .sessions
            .iter()
            .filter(|session| session.has_started())
            .collect::<Vec<_>>();
        sort_sidebar_sessions(&mut sorted_sessions, self.state.sidebar_ordering);

        let mut rows = vec![SidebarRow::Search, SidebarRow::Projects];
        // User-defined groups render ahead of the date sections; the
        // date grouping below only ever sees the remainder.
        let (chat_sections, rest) = chat_group_sections(&sorted_sessions, &self.state.chat_groups);
        for (group, sessions) in &chat_sections {
            append_sidebar_group_rows(
                &mut rows,
                *group,
                sessions,
                self.sidebar_collapsed_groups.contains(group),
            );
        }
        // The sidebar is always date-grouped: chat groups first, then one
        // section per recency period with the project under each chat.
        let mut grouped_sessions: [Vec<Uuid>; 6] = std::array::from_fn(|_| Vec::new());
        for session in rest {
            grouped_sessions
                [session_date_group(sidebar_session_timestamp(session), today).index()]
            .push(session.id);
        }
        let mut groups = SessionDateGroup::ALL;
        if self.state.sidebar_ordering == SidebarOrdering::Oldest {
            groups.reverse();
        }
        for date_group in groups {
            let group = SidebarGroup::Updated(date_group);
            append_sidebar_group_rows(
                &mut rows,
                group,
                &grouped_sessions[date_group.index()],
                self.sidebar_collapsed_groups.contains(&group),
            );
        }
        if rows.len() == 2 {
            // Keep the header actions visible while there is no history.
            rows.push(SidebarRow::Header(SidebarGroup::Updated(
                SessionDateGroup::Today,
            )));
        }
        rows
    }

    /// Keep the virtualized list in sync with the current row snapshot.
    /// Rows are cheap values, so only the minimal changed suffix is spliced,
    /// preserving scroll position and measured heights across unrelated churn
    /// (e.g. the active session's `updated_at` bumping on every stream tick).
    fn sync_sidebar_rows(&self, rows: &[SidebarRow]) {
        let mut cached = self.sidebar_row_cache.borrow_mut();
        if cached.as_slice() == rows {
            return;
        }
        let prefix = cached
            .iter()
            .zip(rows.iter())
            .take_while(|(a, b)| a == b)
            .count();
        let old_count = cached.len();
        *cached = rows.to_vec();
        if old_count == 0 {
            self.sidebar_list_state
                .reset_with_uniform_height(rows.len(), px(SIDEBAR_SESSION_ROW_HEIGHT));
        } else {
            self.sidebar_list_state
                .splice(prefix..old_count, rows.len() - prefix);
            // Newly inserted rows have no measured height yet; give them the
            // uniform hint so the scrollbar keeps a correct total height.
            self.sidebar_list_state
                .clone()
                .with_uniform_item_height(px(SIDEBAR_SESSION_ROW_HEIGHT));
        }
    }

    fn sidebar_row(&self, index: usize, rows: &[SidebarRow], cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = rows.get(index) else {
            return div().into_any_element();
        };
        match *row {
            SidebarRow::Search => self.render_sidebar_search(cx).into_any_element(),
            SidebarRow::Projects => self.render_sidebar_projects(cx).into_any_element(),
            SidebarRow::Header(group) => {
                let has_expanded_children = rows.get(index + 1).is_some_and(|row| {
                    matches!(row, SidebarRow::Session(_))
                });
                // Search and Projects lead; the first header carries actions.
                self.render_sidebar_group_header(group, index == 2, has_expanded_children, cx)
                    .into_any_element()
            }
            SidebarRow::Session(session_id) => self
                .render_sidebar_session_item(session_id, cx)
                .into_any_element(),
            SidebarRow::GroupSpacer => div()
                .w_full()
                .h(px(SIDEBAR_GROUP_SPACER_HEIGHT))
                .into_any_element(),
        }
    }

    fn render_sidebar_group_header(
        &self,
        group: SidebarGroup,
        first: bool,
        _has_expanded_children: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::current(cx);
        let collapsed = self.sidebar_collapsed_groups.contains(&group);
        let group_key = group.element_key();
        let group_name = SharedString::from(format!("sidebar-group-header-{group_key}"));
        let header_focus = self
            .sidebar_group_header_focuses
            .borrow_mut()
            .entry(group)
            .or_insert_with(|| cx.focus_handle())
            .clone();
        let show_folder_icon = matches!(group, SidebarGroup::ChatGroup(_));
        let folder_icon = if collapsed {
            "icons/folder.svg"
        } else {
            "icons/folder-open.svg"
        };
        let label = match group {
            SidebarGroup::Updated(group) => group.label(),
            SidebarGroup::ChatGroup(group_id) => self
                .state
                .chat_groups
                .iter()
                .find(|group| group.id == group_id)
                .map(|group| group.name.clone())
                .unwrap_or_else(|| tr!("sidebar.untitled_group")),
        };
        // A group being renamed swaps its label for the shared inline
        // editor, mirroring session rows. Header toggle stays off while the
        // field owns focus so activating it cannot collapse the section.
        let renaming = matches!(
            group,
            SidebarGroup::ChatGroup(group_id) if self.group_rename == Some(group_id)
        );
        let label_or_field = if renaming {
            div()
                .id(SharedString::from(format!(
                    "group-rename-field-{group_key}"
                )))
                .key_context(GROUP_RENAME_PARENT_CONTEXT)
                .on_action(cx.listener(|this, _: &CancelGroupRename, window, cx| {
                    this.cancel_group_rename(window, cx);
                }))
                .h(px(18.0))
                .flex_1()
                .min_w_0()
                .px(px(4.0))
                .rounded(px(4.0))
                .border_1()
                .border_color(theme.accent)
                .bg(theme.inset)
                .flex()
                .items_center()
                .text_size(sp(13.0))
                .text_color(theme.text)
                .child(self.group_rename_input.clone())
                .into_any_element()
        } else {
            div().min_w_0().truncate().child(label).into_any_element()
        };
        let updated_chevron = matches!(group, SidebarGroup::Updated(_)).then(|| {
            icon("icons/chevron-down.svg", 14.0, theme.text_secondary)
                .when(collapsed, |icon| {
                    icon.with_transformation(gpui::Transformation::rotate(gpui::percentage(0.75)))
                })
                .invisible()
                .group_hover(group_name.clone(), |icon| icon.visible())
        });
        let compose = show_folder_icon.then(|| {
            let compose_focus = self
                .sidebar_group_compose_focuses
                .borrow_mut()
                .entry(group)
                .or_insert_with(|| cx.focus_handle())
                .clone();
            div()
                .w(px(20.0))
                .h(px(22.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_end()
                .child(
                    div()
                        .id(SharedString::from(format!(
                            "sidebar-group-compose-{group_key}"
                        )))
                        .track_focus(&compose_focus)
                        .tab_index(0)
                        .tab_stop(true)
                        .w_0()
                        .h(px(22.0))
                        .overflow_hidden()
                        .rounded(px(4.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_default()
                        .opacity(0.0)
                        .group_hover(group_name.clone(), |style| style.w(px(20.0)).opacity(1.0))
                        .focus_visible(|style| {
                            style
                                .w(px(20.0))
                                .opacity(1.0)
                                .border_1()
                                .border_color(theme.accent)
                        })
                        .hover(|style| style.bg(theme.overlay))
                        .active(|style| style.bg(theme.overlay_strong))
                        .tooltip(Tooltip::text(tr!("menu.new_task")))
                        .child(icon("icons/compose.svg", 14.0, theme.text_secondary))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.open_new_task_for_sidebar_group(group, window, cx);
                        }))
                        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                this.open_new_task_for_sidebar_group(group, window, cx);
                                cx.stop_propagation();
                            }
                        })),
                )
        });

        // The header menu handle doubles as the keyboard path: Shift+F10
        // opens the group menu for ChatGroup headers, mirroring session
        // rows. Created up front so the key handler below can borrow it.
        let header_menu = matches!(group, SidebarGroup::ChatGroup(_))
            .then(|| self.menu_handle(format!("sidebar-group-menu-{group_key}"), cx));
        let keyboard_menu = header_menu.clone();

        let header = session_group_header(&theme)
            .id(SharedString::from(format!(
                "sidebar-group-toggle-{group_key}"
            )))
            .track_focus(&header_focus)
            .tab_index(0)
            .tab_group()
            .tab_stop(true)
            .group(group_name)
            .relative()
            .w_full()
            .rounded(px(6.0))
            .cursor_default()
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .hover(|style| style.bg(theme.sidebar_item_background))
            .active(|style| style.bg(theme.overlay_strong))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h(px(22.0))
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .when(show_folder_icon, |element| {
                        element.child(icon(folder_icon, 14.0, theme.text_secondary))
                    })
                    .child(
                        div()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap(px(2.0))
                            // The editor must fill the header like session
                            // rows do: in a content-sized parent the field
                            // collapses and typed text scrolls out of view.
                            .when(renaming, |element| element.flex_1())
                            .child(label_or_field)
                            .when_some(updated_chevron, |element, chevron| element.child(chevron)),
                    )
                    .child(div().flex_1()),
            )
            .when_some(compose, |element, compose| element.child(compose))
            .when(first, |element| {
                element.child(self.render_sidebar_header_actions(cx))
            })
            .when(!renaming, |element| {
                element
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_sidebar_group(group, cx);
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        let key = event.keystroke.key.as_str();
                        if matches!(key, "enter" | "space") {
                            this.toggle_sidebar_group(group, cx);
                            cx.stop_propagation();
                        } else if key == "left" && !collapsed {
                            this.set_sidebar_group_collapsed(group, true, cx);
                            cx.stop_propagation();
                        } else if key == "right" && collapsed {
                            this.set_sidebar_group_collapsed(group, false, cx);
                            cx.stop_propagation();
                        } else if key == "f10" && event.keystroke.modifiers.shift {
                            if let Some(menu) = keyboard_menu.as_ref() {
                                menu.open_context_menu(window, cx);
                                cx.stop_propagation();
                            }
                        }
                    }))
            });

        let footer = div()
            .w_full()
            .pb(px(SIDEBAR_GROUP_HEADER_BOTTOM_GAP))
            .child(header);
        let SidebarGroup::ChatGroup(group_id) = group else {
            return footer.into_any_element();
        };
        // Group headers carry a context menu for rename and delete. While
        // the inline editor is open the menu stays off and losing focus
        // commits, exactly like session rows.
        if renaming {
            return footer
                .on_mouse_down_out(cx.listener(move |this, _, _, cx| {
                    if this.group_rename == Some(group_id) {
                        this.commit_group_rename(cx);
                    }
                }))
                .into_any_element();
        }
        let waku = cx.entity().downgrade();
        let Some(menu) = header_menu else {
            return footer.into_any_element();
        };
        context_menu(
            footer,
            SharedString::from(format!("sidebar-group-{group_key}")),
            &menu,
            move |_| {
                let rename_waku = waku.clone();
                let delete_waku = waku.clone();
                vec![
                    MenuItem::new(tr!("common.rename"), move |window, cx| {
                        let _ = rename_waku.update(cx, |waku, cx| {
                            waku.begin_group_rename(group_id, window, cx);
                        });
                    }),
                    MenuItem::Separator,
                    MenuItem::new(tr!("sidebar.delete_group"), move |_, cx| {
                        let _ = delete_waku.update(cx, |waku, cx| {
                            waku.delete_chat_group(group_id, cx);
                        });
                    }),
                ]
            },
        )
    }

    fn open_new_task_for_sidebar_group(
        &mut self,
        group: SidebarGroup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings_page = None;
        match group {
            SidebarGroup::ChatGroup(group_id) => self.create_session_in_group(group_id, cx),
            SidebarGroup::Updated(_) => return,
        }
        let focus = self.composer_focus(cx);
        window.focus(&focus, cx);
    }

    fn toggle_sidebar_group(&mut self, group: SidebarGroup, cx: &mut Context<Self>) {
        let collapsed = !self.sidebar_collapsed_groups.contains(&group);
        self.set_sidebar_group_collapsed(group, collapsed, cx);
    }

    pub(super) fn collapse_all_sidebar_groups(&mut self, cx: &mut Context<Self>) {
        let groups = self
            .sidebar_rows_cached(Local::now().date_naive(), unix_time())
            .iter()
            .filter_map(|row| match row {
                SidebarRow::Header(group) => Some(*group),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut changed = false;
        for group in groups {
            changed |= self.sidebar_collapsed_groups.insert(group);
        }
        if changed {
            self.sidebar_rows_fingerprint.set(None);
            cx.notify();
        }
    }

    fn set_sidebar_group_collapsed(
        &mut self,
        group: SidebarGroup,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) {
        let collapse_changed = if collapsed {
            self.sidebar_collapsed_groups.insert(group)
        } else {
            self.sidebar_collapsed_groups.remove(&group)
        };
        if collapse_changed {
            self.sidebar_rows_fingerprint.set(None);
            cx.notify();
        }
    }

    // ── Chat groups ──────────────────────────────────────────────────────────

    fn chat_group(&self, group_id: Uuid) -> Option<&ChatGroup> {
        self.state
            .chat_groups
            .iter()
            .find(|group| group.id == group_id)
    }

    /// Create a group holding `session_id` and open the inline rename so the
    /// name is chosen up front. Sessions only surface their menu once they
    /// have started, so the member always has a row to reveal.
    pub(super) fn create_chat_group(
        &mut self,
        session_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Uuid> {
        if !self
            .state
            .sessions
            .iter()
            .any(|session| session.id == session_id)
        {
            return None;
        }
        let group = ChatGroup::new(tr!("sidebar.new_group_name"));
        let group_id = group.id;
        self.state.chat_groups.push(group);
        self.move_session_to_group(session_id, group_id, cx);
        self.begin_group_rename(group_id, window, cx);
        Some(group_id)
    }

    pub(super) fn move_session_to_group(
        &mut self,
        session_id: Uuid,
        group_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        if !self
            .state
            .chat_groups
            .iter()
            .any(|group| group.id == group_id)
        {
            return;
        }
        let already_there = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .is_some_and(|session| session.group_id == Some(group_id));
        if already_there {
            return;
        }
        if let Some(session) = self.state.session_mut(session_id) {
            session.group_id = Some(group_id);
        }
        // The moved chat must be visible: reveal its new section.
        self.sidebar_collapsed_groups
            .remove(&SidebarGroup::ChatGroup(group_id));
        self.prune_empty_chat_groups();
        self.save();
        cx.notify();
    }

    pub(super) fn remove_session_from_group(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        let grouped = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .is_some_and(|session| session.group_id.is_some());
        if !grouped {
            return;
        }
        if let Some(session) = self.state.session_mut(session_id) {
            session.group_id = None;
        }
        self.prune_empty_chat_groups();
        self.save();
        cx.notify();
    }

    pub(super) fn rename_chat_group(
        &mut self,
        group_id: Uuid,
        name: String,
        cx: &mut Context<Self>,
    ) {
        let name = name.trim().to_owned();
        if name.is_empty() {
            return;
        }
        if let Some(group) = self
            .state
            .chat_groups
            .iter_mut()
            .find(|group| group.id == group_id)
        {
            if group.name != name {
                group.name = name;
                self.save();
            }
        }
        cx.notify();
    }

    pub(super) fn delete_chat_group(&mut self, group_id: Uuid, cx: &mut Context<Self>) {
        self.state.chat_groups.retain(|group| group.id != group_id);
        let members: Vec<Uuid> = self
            .state
            .sessions
            .iter()
            .filter(|session| session.group_id == Some(group_id))
            .map(|session| session.id)
            .collect();
        for session_id in members {
            if let Some(session) = self.state.session_mut(session_id) {
                session.group_id = None;
            }
        }
        self.sidebar_collapsed_groups
            .remove(&SidebarGroup::ChatGroup(group_id));
        if self.group_rename == Some(group_id) {
            self.group_rename = None;
        }
        self.save();
        cx.notify();
    }

    /// Drop registry entries with no member sessions, so an emptied group
    /// leaves no invisible zombie behind. Sessions keep rendering — an
    /// unknown group id reads as ungrouped — but the registry stays tight.
    fn prune_empty_chat_groups(&mut self) {
        let used: HashSet<Uuid> = self
            .state
            .sessions
            .iter()
            .filter_map(|session| session.group_id)
            .collect();
        let before = self.state.chat_groups.len();
        self.state
            .chat_groups
            .retain(|group| used.contains(&group.id));
        if self.state.chat_groups.len() != before {
            let known: HashSet<Uuid> = self
                .state
                .chat_groups
                .iter()
                .map(|group| group.id)
                .collect();
            self.sidebar_collapsed_groups.retain(|group| match group {
                SidebarGroup::ChatGroup(id) => known.contains(id),
                _ => true,
            });
            self.save();
        }
    }

    fn set_sidebar_ordering(&mut self, ordering: SidebarOrdering, cx: &mut Context<Self>) {
        if self.state.sidebar_ordering == ordering {
            return;
        }
        self.state.sidebar_ordering = ordering;
        self.sidebar_rows_fingerprint.set(None);
        self.sidebar_list_state.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: Pixels::ZERO,
        });
        self.save();
        cx.notify();
    }

    fn begin_session_rename(
        &mut self,
        session_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(title) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .map(localized_session_title)
        else {
            return;
        };

        self.session_rename = Some(session_id);
        self.session_rename_input.update(cx, |input, cx| {
            input.set_content(title, cx);
            input.select_all_text(cx);
        });
        let focus = self.session_rename_input.read(cx).focus();
        window.on_next_frame(move |window, cx| window.focus(&focus, cx));
        cx.notify();
    }

    pub(super) fn commit_session_rename(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_rename.take() else {
            return;
        };
        let title = self
            .session_rename_input
            .read(cx)
            .content()
            .trim()
            .to_owned();
        let should_update = !title.is_empty()
            && self
                .state
                .sessions
                .iter()
                .find(|session| session.id == session_id)
                .is_some_and(|session| session.title != title);
        if should_update
            && self
                .state
                .session_mut(session_id)
                .is_some_and(|session| session.set_title(&title))
        {
            self.save();
        }
        cx.notify();
    }

    fn cancel_session_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session_rename.take().is_none() {
            return;
        }
        let focus = self.composer_focus(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    fn begin_group_rename(&mut self, group_id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.chat_group(group_id).map(|group| group.name.clone()) else {
            return;
        };
        self.group_rename = Some(group_id);
        self.group_rename_input.update(cx, |input, cx| {
            input.set_content(name, cx);
            input.select_all_text(cx);
        });
        let focus = self.group_rename_input.read(cx).focus();
        window.on_next_frame(move |window, cx| window.focus(&focus, cx));
        cx.notify();
    }

    pub(super) fn commit_group_rename(&mut self, cx: &mut Context<Self>) {
        let Some(group_id) = self.group_rename.take() else {
            return;
        };
        let name = self.group_rename_input.read(cx).content().trim().to_owned();
        if !name.is_empty() {
            self.rename_chat_group(group_id, name, cx);
        }
        cx.notify();
    }

    fn cancel_group_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.group_rename.take().is_none() {
            return;
        }
        let focus = self.composer_focus(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    fn render_sidebar_session_item(&self, session_id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let Some(session) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
        else {
            return div().into_any_element();
        };
        let selected = sidebar_session_selected(
            self.state.selected_session,
            self.pending_session_activation
                .map(|pending| pending.session_id),
            session_id,
        );
        let working = matches!(
            session.status,
            SessionStatus::Connecting | SessionStatus::Working
        );
        let project = self
            .state
            .projects
            .iter()
            .find(|project| project.id == session.project_id);
        // Chats filed in a group sit under its header, so they take the
        // indented child treatment with the guide rail.
        let indented = session
            .group_id
            .is_some_and(|group_id| self.chat_group(group_id).is_some());
        let left_padding = if indented {
            SIDEBAR_GROUP_CHILD_PADDING
        } else {
            8.0
        };
        let detail_label = if let Some(group) = session
            .group_id
            .and_then(|group_id| self.chat_group(group_id))
        {
            // A grouped chat names its group, not its folder: the group is
            // what the section above says, and the project underneath would
            // contradict it (e.g. projectless chats reading "No project").
            Some(SharedString::from(group.name.clone()))
        } else {
            Some(SharedString::from(
                project
                    .map(Project::display_name)
                    .unwrap_or_else(|| tr!("sidebar.unknown_project")),
            ))
        };
        let has_detail_label = detail_label.is_some();
        let detail_icon = "icons/folder.svg";
        let rename_input =
            (self.session_rename == Some(session_id)).then(|| self.session_rename_input.clone());
        let renaming = rename_input.is_some();
        let title = if let Some(rename_input) = rename_input {
            div()
                .id(SharedString::from(format!(
                    "session-rename-field-{session_id}"
                )))
                .key_context(SESSION_RENAME_PARENT_CONTEXT)
                .on_action(cx.listener(|this, _: &CancelSessionRename, window, cx| {
                    this.cancel_session_rename(window, cx);
                }))
                .h(px(18.0))
                .flex_1()
                .min_w_0()
                .px(px(4.0))
                .rounded(px(4.0))
                .border_1()
                .border_color(theme.accent)
                .bg(theme.inset)
                .flex()
                .items_center()
                .text_size(sp(13.5))
                .text_color(theme.text)
                .child(rename_input)
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_w_0()
                .whitespace_normal()
                .line_clamp(1)
                .text_overflow(gpui::TextOverflow::Truncate("...".into()))
                .text_size(sp(13.5))
                .text_color(theme.text)
                .child(SharedString::from(localized_session_title(session)))
                .into_any_element()
        };
        let waku = cx.entity().downgrade();
        let menu = self.menu_handle(format!("session-{session_id}"), cx);
        let row_focus = menu.trigger_focus_handle().clone();
        let keyboard_menu = menu.clone();
        let row = div()
            .id(SharedString::from(format!("session-{}", session.id)))
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .pl(px(left_padding))
            .pr(px(8.0))
            .py(px(7.0))
            .rounded(px(7.0))
            .cursor_default()
            .when(selected, |element| {
                element.bg(theme.sidebar_item_background)
            })
            .hover(|element| element.bg(theme.sidebar_item_background))
            .active(|element| element.bg(theme.sidebar_item_background))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .overflow_hidden()
                    .line_height(sp(18.0))
                    .child(title)
                    .when(working, |element| {
                        element.child(motion::spin_slow(icon(
                            "icons/loader-circle.svg",
                            12.0,
                            status_color(&theme, session.status),
                        )))
                    })
                    .when(session.status == SessionStatus::Background, |element| {
                        element.child(icon(
                            "icons/hourglass.svg",
                            12.0,
                            status_color(&theme, session.status),
                        ))
                    })
                    .when(session.status == SessionStatus::Waiting, |element| {
                        element.child(icon(
                            "icons/alert.svg",
                            12.0,
                            status_color(&theme, session.status),
                        ))
                    })
                    .when(session.status == SessionStatus::Failed, |element| {
                        element.child(icon(
                            "icons/x.svg",
                            12.0,
                            status_color(&theme, session.status),
                        ))
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .text_size(sp(13.0))
                    .line_height(sp(15.0))
                    .when_some(detail_label, |element, label| {
                        element
                            .child(icon(detail_icon, 12.5, theme.text_tertiary))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(theme.text_tertiary)
                                    .child(label),
                            )
                    })
                    .when(!has_detail_label, |element| element.child(div().flex_1()))
                    .when_some(
                        session_time_label(session, unix_time()),
                        |element, label| {
                            element.child(
                                div()
                                    .flex_none()
                                    .text_size(sp(12.5))
                                    .text_color(if session.is_busy() {
                                        theme.text_tertiary
                                    } else {
                                        theme.text_ghost
                                    })
                                    .child(SharedString::from(label)),
                            )
                        },
                    ),
            )
            .when(!renaming, |element| {
                element
                    .track_focus(&row_focus)
                    .tab_index(0)
                    .focus_visible(|style| style.border_1().border_color(theme.accent))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        let key = event.keystroke.key.as_str();
                        if matches!(key, "enter" | "space") {
                            this.select_session(session_id, cx);
                            cx.stop_propagation();
                        } else if key == "f10" && event.keystroke.modifiers.shift {
                            keyboard_menu.open_context_menu(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_session(session_id, cx);
                    }))
            });
        let row = if renaming {
            div()
                .w_full()
                .child(row)
                .on_mouse_down_out(cx.listener(move |this, _, _, cx| {
                    if this.session_rename == Some(session_id) {
                        this.commit_session_rename(cx);
                    }
                }))
                .into_any_element()
        } else {
            context_menu(
                div().w_full().child(row),
                SharedString::from(format!("session-menu-{session_id}")),
                &menu,
                move |cx| {
                    let rename_waku = waku.clone();
                    let remove_waku = waku.clone();
                    let move_waku = waku.clone();
                    let move_value_waku = waku.clone();
                    vec![
                        MenuItem::new(tr!("common.rename"), move |window, cx| {
                            let _ = rename_waku.update(cx, |waku, cx| {
                                waku.begin_session_rename(session_id, window, cx);
                            });
                        }),
                        MenuItem::submenu_with_value(
                            tr!("sidebar.move_to_group"),
                            move_value_waku
                                .upgrade()
                                .map(|entity| {
                                    entity.update(cx, |waku, _| {
                                        waku.state
                                            .sessions
                                            .iter()
                                            .find(|session| session.id == session_id)
                                            .and_then(|session| session.group_id)
                                            .and_then(|group_id| {
                                                waku.chat_group(group_id)
                                                    .map(|group| group.name.clone())
                                            })
                                            .unwrap_or_default()
                                    })
                                })
                                .unwrap_or_default(),
                            move |cx| {
                                let snapshot = move_waku.upgrade().map(|entity| {
                                    entity.update(cx, |waku, _| {
                                        (
                                            waku.state
                                                .chat_groups
                                                .iter()
                                                .map(|group| (group.id, group.name.clone()))
                                                .collect::<Vec<_>>(),
                                            waku.state
                                                .sessions
                                                .iter()
                                                .find(|session| session.id == session_id)
                                                .and_then(|session| session.group_id),
                                        )
                                    })
                                });
                                let Some((groups, current)) = snapshot else {
                                    return Vec::new();
                                };
                                let new_group_waku = move_waku.clone();
                                let mut items = vec![MenuItem::new(
                                    tr!("sidebar.new_group"),
                                    move |window, cx| {
                                        let _ = new_group_waku.update(cx, |waku, cx| {
                                            waku.create_chat_group(session_id, window, cx);
                                        });
                                    },
                                )];
                                if !groups.is_empty() {
                                    items.push(MenuItem::Separator);
                                }
                                for (group_id, name) in groups {
                                    let target = move_waku.clone();
                                    items.push(
                                        MenuItem::new(name, move |_, cx| {
                                            let _ = target.update(cx, |waku, cx| {
                                                waku.move_session_to_group(
                                                    session_id, group_id, cx,
                                                );
                                            });
                                        })
                                        .selected(current == Some(group_id)),
                                    );
                                }
                                if current.is_some() {
                                    let ungroup = move_waku.clone();
                                    items.push(MenuItem::Separator);
                                    items.push(MenuItem::new(
                                        tr!("sidebar.remove_from_group"),
                                        move |_, cx| {
                                            let _ = ungroup.update(cx, |waku, cx| {
                                                waku.remove_session_from_group(session_id, cx);
                                            });
                                        },
                                    ));
                                }
                                items
                            },
                        ),
                        MenuItem::Separator,
                        MenuItem::new(tr!("common.remove"), move |_, cx| {
                            let _ = remove_waku
                                .update(cx, |waku, cx| waku.remove_session(session_id, cx));
                        }),
                    ]
                },
            )
        };

        div()
            .relative()
            .w_full()
            .pb(px(SIDEBAR_SESSION_ROW_GAP))
            .child(row)
            .into_any_element()
    }

    // ── Header ─────────────────────────────────────────────────────────────

    pub(super) fn render_header(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = Theme::current(cx);
        let session = self.selected_session();
        let title = session
            .map(localized_session_title)
            .unwrap_or_else(|| tr!("session.new_task"));
        let agent_preset_label = session
            .filter(|session| session.provider == ProviderKind::ChatGpt && session.has_started())
            .and_then(|session| self.agent_preset_label_for_session(session));
        let left_window_controls = (!self.sidebar_visible)
            .then(|| {
                self.render_client_window_controls(
                    super::window_chrome::WindowControlSide::Left,
                    window,
                    cx,
                )
            })
            .flatten();
        let right_window_controls = (!self.right_panel_visible)
            .then(|| {
                self.render_client_window_controls(
                    super::window_chrome::WindowControlSide::Right,
                    window,
                    cx,
                )
            })
            .flatten();
        div()
            .id("window-header")
            .h(px(48.0))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.0))
            .children(left_window_controls)
            // The header starts where the sidebar ends, so until the sidebar
            // is wide enough to host the traffic lights itself the header has
            // to clear them. Steady state with the sidebar open adds nothing;
            // a sidebar sliding in shrinks the inset as it takes the lights
            // over, which is what keeps the title from passing under them.
            .pl(if self.sidebar_visible {
                px(14.0 + (TRAFFIC_LIGHT_CLEARANCE - self.sidebar_rendered_width).max(0.0))
            } else {
                px(0.0)
            })
            .pr(px(14.0))
            .when(!self.sidebar_visible, |element| {
                element
                    .child(
                        self.window_drag_region(
                            div()
                                .id("header-traffic-light-drag-region")
                                .w(px(TRAFFIC_LIGHT_CLEARANCE - 8.0))
                                .h_full()
                                .flex_none(),
                            cx,
                        ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(self.render_sidebar_toggle(cx))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(2.0))
                                    .child(self.render_history_button(
                                        "navigate-back",
                                        "icons/arrow-left.svg",
                                        !self.session_navigation.back.is_empty(),
                                        true,
                                        cx,
                                    ))
                                    .child(self.render_history_button(
                                        "navigate-forward",
                                        "icons/arrow-right.svg",
                                        !self.session_navigation.forward.is_empty(),
                                        false,
                                        cx,
                                    )),
                            ),
                    )
            })
            .child(
                self.window_drag_region(
                    div()
                        .id("header-title-drag-region")
                        .h_full()
                        .min_w_0()
                        .flex_shrink(1.0)
                        .flex()
                        .items_center()
                        .gap(px(7.0))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(sp(13.0))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(SharedString::from(title)),
                        )
                        .children(agent_preset_label.map(|label| {
                            div()
                                .h(px(22.0))
                                .max_w(px(180.0))
                                .px(px(6.0))
                                .rounded(px(6.0))
                                .flex_none()
                                .flex()
                                .items_center()
                                .gap(px(4.0))
                                .bg(theme.overlay)
                                .text_size(sp(12.5))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.text_secondary)
                                .child(icon("icons/bot.svg", 10.5, theme.text_tertiary))
                                .child(div().min_w_0().truncate().child(SharedString::from(label)))
                        })),
                    cx,
                ),
            )
            .child(
                self.window_drag_region(
                    div().id("header-center-drag-region").h_full().flex_1(),
                    cx,
                ),
            )
            .child(self.render_background_work_summary(cx))
            .when(!self.right_panel_visible, |element| {
                element
                    .when(self.fps_counter_visible, |element| {
                        element.child(self.render_fps_counter(cx))
                    })
                    .child(self.render_right_panel_toggle(cx))
            })
            .children(right_window_controls)
    }

    // ── Empty states ───────────────────────────────────────────────────────

    pub(super) fn render_empty_state(&self, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        if self.selected_project().is_none() {
            // No folder needed: one click starts a fresh chat.
            return div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .px_8()
                .pb(px(46.0))
                .child(icon("icons/sparkle.svg", 24.0, theme.accent))
                .child(
                    div()
                        .mt(px(16.0))
                        .text_size(sp(20.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(tr_cow!("onboarding.what_should_we_build")),
                )
                .child(
                    div()
                        .mt(px(20.0))
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(8.0))
                        .tab_index(0)
                        .tab_group()
                        .tab_stop(false)
                        .child(
                            div()
                                .id("onboarding-new-chat")
                                .track_focus(&self.onboarding_projectless_focus)
                                .tab_index(0)
                                .focus_visible(|style| style.border_1().border_color(theme.accent))
                                .h(px(32.0))
                                .px(px(14.0))
                                .rounded_full()
                                .flex()
                                .items_center()
                                .cursor_default()
                                .bg(theme.inverse)
                                .text_color(theme.on_inverse)
                                .text_size(sp(12.5))
                                .font_weight(FontWeight::SEMIBOLD)
                                .hover(|element| element.opacity(0.9))
                                .active(|element| element.opacity(0.8))
                                .child(tr_cow!("menu.new_task"))
                                .on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.create_projectless_session(cx)
                                    }),
                                )
                                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                        this.create_projectless_session(cx);
                                        cx.stop_propagation();
                                    }
                                })),
                        ),
                );
        }
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .px_8()
            .pb(px(52.0))
            .child(icon("icons/sparkle.svg", 20.0, theme.accent))
            .child(
                div()
                    .mt(px(14.0))
                    .flex()
                    .items_baseline()
                    .text_size(sp(20.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(tr_cow!("onboarding.what_should_we_build")),
            )
    }
}

fn localized_session_title(session: &AgentSession) -> String {
    let title = session.display_title();
    if title == AgentSession::DEFAULT_TITLE {
        tr!("session.new_task")
    } else {
        title.to_owned()
    }
}

fn sidebar_session_selected(
    selected_session: Option<Uuid>,
    pending_session: Option<Uuid>,
    session_id: Uuid,
) -> bool {
    pending_session.map_or(selected_session == Some(session_id), |pending| {
        pending == session_id
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_sessions_by_calendar_period() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 12).unwrap();
        let cases = [
            ((2026, 8, 12), SessionDateGroup::Today),
            ((2026, 8, 11), SessionDateGroup::Yesterday),
            ((2026, 8, 10), SessionDateGroup::ThisWeek),
            ((2026, 8, 1), SessionDateGroup::ThisMonth),
            ((2026, 1, 1), SessionDateGroup::ThisYear),
            ((2025, 12, 31), SessionDateGroup::More),
        ];

        for ((year, month, day), expected) in cases {
            let session_date = NaiveDate::from_ymd_opt(year, month, day).unwrap();
            assert_eq!(session_date_group_for_dates(session_date, today), expected);
        }
    }

    #[test]
    fn future_sessions_stay_in_today() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 12).unwrap();
        let tomorrow = NaiveDate::from_ymd_opt(2026, 8, 13).unwrap();
        assert_eq!(
            session_date_group_for_dates(tomorrow, today),
            SessionDateGroup::Today
        );
    }

    #[test]
    fn collapsed_sidebar_group_keeps_only_its_header_and_spacer() {
        let sessions = [Uuid::from_u128(1), Uuid::from_u128(2)];
        let group = SidebarGroup::Updated(SessionDateGroup::Today);
        let mut expanded = Vec::new();
        append_sidebar_group_rows(&mut expanded, group, &sessions, false);
        assert_eq!(
            expanded,
            vec![
                SidebarRow::Header(group),
                SidebarRow::Session(sessions[0]),
                SidebarRow::Session(sessions[1]),
                SidebarRow::GroupSpacer,
            ]
        );

        let mut collapsed = Vec::new();
        append_sidebar_group_rows(&mut collapsed, group, &sessions, true);
        assert_eq!(
            collapsed,
            vec![SidebarRow::Header(group), SidebarRow::GroupSpacer,]
        );
    }

    #[test]
    fn chat_group_sections_follow_registry_order_and_ignore_unknown_groups() {
        let first = AgentSession::new(Uuid::from_u128(10), ProviderKind::ChatGpt);
        let mut second = AgentSession::new(Uuid::from_u128(11), ProviderKind::ChatGpt);
        let mut third = AgentSession::new(Uuid::from_u128(12), ProviderKind::ChatGpt);
        let mut fourth = AgentSession::new(Uuid::from_u128(13), ProviderKind::ChatGpt);
        let beta = ChatGroup {
            id: Uuid::from_u128(2),
            name: "Beta".into(),
            created_at: 2,
        };
        let alpha = ChatGroup {
            id: Uuid::from_u128(1),
            name: "Alpha".into(),
            created_at: 1,
        };
        second.group_id = Some(beta.id);
        third.group_id = Some(alpha.id);
        // A deleted group's chats fall back to the ungrouped remainder.
        fourth.group_id = Some(Uuid::from_u128(99));
        let sessions = [&first, &second, &third, &fourth];
        let (sections, rest) = chat_group_sections(&sessions, &[beta.clone(), alpha.clone()]);
        // Registry order wins even though beta's session arrived first.
        assert_eq!(
            sections.iter().map(|(group, _)| *group).collect::<Vec<_>>(),
            vec![
                SidebarGroup::ChatGroup(beta.id),
                SidebarGroup::ChatGroup(alpha.id),
            ]
        );
        assert_eq!(sections[0].1, vec![second.id]);
        assert_eq!(sections[1].1, vec![third.id]);
        assert_eq!(
            rest.iter().map(|session| session.id).collect::<Vec<_>>(),
            vec![first.id, fourth.id]
        );
    }

    #[test]
    fn sidebar_recency_uses_last_reply_with_creation_fallback() {
        let project_id = Uuid::new_v4();
        let mut renamed_old_session = AgentSession::new(project_id, ProviderKind::ChatGpt);
        renamed_old_session.created_at = 10;
        renamed_old_session.last_reply_at = Some(20);
        renamed_old_session.updated_at = 1_000;

        let mut newer_unanswered_session = AgentSession::new(project_id, ProviderKind::ChatGpt);
        newer_unanswered_session.created_at = 30;
        newer_unanswered_session.last_reply_at = None;
        newer_unanswered_session.updated_at = 30;

        assert_eq!(sidebar_session_timestamp(&renamed_old_session), 20);
        assert_eq!(sidebar_session_timestamp(&newer_unanswered_session), 30);

        let mut sessions = vec![&renamed_old_session, &newer_unanswered_session];
        sort_sidebar_sessions(&mut sessions, SidebarOrdering::Newest);
        assert_eq!(sessions[0].id, newer_unanswered_session.id);

        sort_sidebar_sessions(&mut sessions, SidebarOrdering::Oldest);
        assert_eq!(sessions[0].id, renamed_old_session.id);
    }

    #[test]
    fn persisted_worktree_branches_supply_sidebar_labels() {
        // Chat mode hides branch labels everywhere.
        let local = SessionWorkspace::Local;
        let planned = SessionWorkspace::NewWorktree {
            base_branch: Some("develop".to_owned()),
        };
        let worktree = SessionWorkspace::Worktree {
            path: PathBuf::from("/tmp/worktree"),
            branch: "feature/sidebar".to_owned(),
        };

        assert_eq!(persisted_sidebar_branch_label(&local), None);
        assert_eq!(persisted_sidebar_branch_label(&planned), None);
        assert_eq!(persisted_sidebar_branch_label(&worktree), None);
    }

    #[test]
    fn pending_session_replaces_sidebar_selection_immediately() {
        let current = Uuid::from_u128(1);
        let pending = Uuid::from_u128(2);

        assert!(!sidebar_session_selected(
            Some(current),
            Some(pending),
            current
        ));
        assert!(sidebar_session_selected(
            Some(current),
            Some(pending),
            pending
        ));
        assert!(sidebar_session_selected(Some(current), None, current));
    }

    #[test]
    fn selected_session_uses_nearest_bottom_edge_for_an_unmeasured_lower_row() {
        let target = Uuid::from_u128(31);
        let group = SidebarGroup::Updated(SessionDateGroup::Today);
        let mut rows = vec![SidebarRow::Search, SidebarRow::Header(group)];
        rows.extend((1..=40).map(|id| SidebarRow::Session(Uuid::from_u128(id))));
        rows.push(SidebarRow::GroupSpacer);

        let index = sidebar_session_row_index(&rows, target).unwrap();
        let offset = sidebar_bottom_aligned_offset(&rows, index, px(400.0));

        assert_eq!(index, 32);
        assert_eq!(offset.item_ix, 25);
        assert_eq!(offset.offset_in_item, px(16.0));
        let visible_height = rows[offset.item_ix..=index]
            .iter()
            .copied()
            .map(sidebar_row_height)
            .fold(Pixels::ZERO, |height, row| height + row)
            - offset.offset_in_item;
        assert_eq!(visible_height, px(400.0));
        assert_eq!(sidebar_session_row_index(&rows, Uuid::from_u128(41)), None);
    }
}
