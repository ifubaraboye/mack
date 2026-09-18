//! Settings → Memory page: cross-chat memory toggle, list, delete, clear.
//!
//! The page never touches SQLite. Reads and mutations go over RPC to the
//! daemon, which scopes everything to the signed-in ChatGPT account. List
//! loads follow the usage-page background pattern (generation-guarded spawn);
//! destructive actions reuse the skills-page arm/disarm two-click pattern.

use super::*;
use waku_protocol::persistence::StoredMemory;

impl Mack {
    /// Start a background memory load unless one is in flight or a fresh
    /// list is already shown. `force` reloads unconditionally (retry button,
    /// returning to the page).
    pub(super) fn ensure_memory_list(&mut self, force: bool, cx: &mut Context<Self>) {
        if matches!(self.memory_list, MemoryListState::Loading { .. }) {
            return;
        }
        if !force && matches!(self.memory_list, MemoryListState::Loaded { .. }) {
            return;
        }
        self.memory_list_generation = self.memory_list_generation.wrapping_add(1);
        let generation = self.memory_list_generation;
        self.memory_list = MemoryListState::Loading { generation };
        let daemon = self.daemon.client();
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    match daemon.request(
                        Uuid::nil(),
                        Uuid::nil(),
                        waku_client::Command::ListMemories,
                    )? {
                        waku_client::ResponsePayload::Memories { memories } => Ok(memories),
                        _ => anyhow::bail!("the daemon returned an invalid memories response"),
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.memory_list_generation != generation {
                    return;
                }
                match loaded {
                    Ok(memories) => {
                        this.memory_list = MemoryListState::Loaded { memories };
                    }
                    Err(error) => {
                        this.memory_list = MemoryListState::Failed;
                        this.show_toast(tr!("memory.load_error", error = error.to_string()));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Flip the memory toggle: persist locally, push to the daemon, then push
    /// live to every running worker so no restart is needed. Existing stored
    /// memories are never touched by the toggle itself.
    pub(super) fn set_memory_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.state.memory_enabled == enabled {
            return;
        }
        self.state.memory_enabled = enabled;
        self.save();
        if let Err(error) = self.daemon.update_settings(self.state.daemon_settings()) {
            self.state.memory_enabled = !enabled;
            self.save();
            self.show_toast(tr!("memory.toggle_error", error = error.to_string()));
            cx.notify();
            return;
        }
        let running: Vec<Uuid> = self.runtimes.keys().copied().collect();
        for session_id in running {
            self.apply_session_options(session_id, cx);
        }
        cx.notify();
    }

    /// Arm-or-execute memory deletion, mirroring the skills-page two-click
    /// confirm: first click arms the row's button, second click deletes.
    pub(super) fn step_memory_delete(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if self.memory_delete_arming != Some(id) {
            self.memory_delete_arming = Some(id);
            self.memory_clear_arming = false;
            cx.notify();
            return;
        }
        self.memory_delete_arming = None;
        let daemon = self.daemon.client();
        cx.spawn(async move |this, cx| {
            let deleted = cx
                .background_executor()
                .spawn(async move {
                    match daemon.request(
                        Uuid::nil(),
                        Uuid::nil(),
                        waku_client::Command::DeleteMemory { id },
                    )? {
                        waku_client::ResponsePayload::Ack => Ok(()),
                        _ => anyhow::bail!("the daemon returned an invalid delete response"),
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match deleted {
                    Ok(()) => {
                        // Invalidate in-flight loads; drop the row locally so
                        // the list updates without a refetch.
                        this.memory_list_generation = this.memory_list_generation.wrapping_add(1);
                        if let MemoryListState::Loaded { memories } = &mut this.memory_list {
                            memories.retain(|memory| memory.id != id);
                        }
                    }
                    Err(error) => {
                        this.show_toast(tr!("memory.delete_error", error = error.to_string()));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Arm-or-execute clear-all, same two-click pattern as single delete.
    pub(super) fn step_memory_clear(&mut self, cx: &mut Context<Self>) {
        if !self.memory_clear_arming {
            self.memory_clear_arming = true;
            self.memory_delete_arming = None;
            cx.notify();
            return;
        }
        self.memory_clear_arming = false;
        let daemon = self.daemon.client();
        cx.spawn(async move |this, cx| {
            let cleared = cx
                .background_executor()
                .spawn(async move {
                    match daemon.request(
                        Uuid::nil(),
                        Uuid::nil(),
                        waku_client::Command::ClearMemories,
                    )? {
                        waku_client::ResponsePayload::Ack => Ok(()),
                        _ => anyhow::bail!("the daemon returned an invalid clear response"),
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match cleared {
                    Ok(()) => {
                        this.memory_list_generation = this.memory_list_generation.wrapping_add(1);
                        if let MemoryListState::Loaded { memories } = &mut this.memory_list {
                            memories.clear();
                        }
                    }
                    Err(error) => {
                        this.show_toast(tr!("memory.clear_error", error = error.to_string()));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn render_memory_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let enabled = self.state.memory_enabled;

        let toggle = toggle_switch(
            "memory-enabled-toggle",
            enabled,
            false,
            theme,
            cx,
            move |this, _, cx| this.set_memory_enabled(!enabled, cx),
        );

        div()
            .mt(px(15.0))
            .w_full()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(
                div()
                    .px(px(20.0))
                    .py(px(14.0))
                    .rounded(px(13.0))
                    .bg(theme.raised)
                    .child(
                        div()
                            .text_size(sp(13.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(tr!("memory.title")),
                    )
                    .child(
                        div()
                            .mt(px(5.0))
                            .text_size(sp(12.5))
                            .line_height(sp(18.0))
                            .text_color(theme.text_secondary)
                            .child(tr!("memory.description")),
                    ),
            )
            .child(
                div()
                    .px(px(20.0))
                    .py(px(14.0))
                    .rounded(px(13.0))
                    .bg(theme.raised)
                    .flex()
                    .items_center()
                    .gap(px(20.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(sp(13.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(tr!("memory.toggle_title")),
                            )
                            .child(
                                div()
                                    .mt(px(5.0))
                                    .text_size(sp(12.5))
                                    .line_height(sp(18.0))
                                    .text_color(theme.text_secondary)
                                    .child(tr!("memory.toggle_description")),
                            ),
                    )
                    .child(toggle),
            )
            .when(!enabled, |element| {
                element.child(
                    div()
                        .px(px(20.0))
                        .py(px(14.0))
                        .rounded(px(13.0))
                        .bg(theme.raised)
                        .child(
                            div()
                                .text_size(sp(13.5))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(tr!("memory.off_title")),
                        )
                        .child(
                            div()
                                .mt(px(5.0))
                                .text_size(sp(12.5))
                                .line_height(sp(18.0))
                                .text_color(theme.text_secondary)
                                .child(tr!("memory.off_description")),
                        ),
                )
            })
            .child(self.render_memory_list(theme, cx))
            .into_any_element()
    }

    fn render_memory_list(&self, theme: Theme, cx: &mut Context<Self>) -> AnyElement {
        match &self.memory_list {
            MemoryListState::NotLoaded | MemoryListState::Loading { .. } => div()
                .px(px(20.0))
                .py(px(14.0))
                .rounded(px(13.0))
                .bg(theme.raised)
                .child(
                    div()
                        .text_size(sp(12.5))
                        .text_color(theme.text_tertiary)
                        .child(tr!("memory.loading")),
                )
                .into_any_element(),
            MemoryListState::Failed => div()
                .px(px(20.0))
                .py(px(14.0))
                .rounded(px(13.0))
                .bg(theme.raised)
                .flex()
                .flex_col()
                .gap(px(10.0))
                .child(
                    div()
                        .text_size(sp(12.5))
                        .text_color(theme.text_secondary)
                        .child(tr!("memory.load_error")),
                )
                .child(memory_retry_button(theme, cx))
                .into_any_element(),
            MemoryListState::Loaded { memories } if memories.is_empty() => div()
                .px(px(20.0))
                .py(px(14.0))
                .rounded(px(13.0))
                .bg(theme.raised)
                .child(
                    div()
                        .text_size(sp(13.5))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(tr!("memory.empty_title")),
                )
                .child(
                    div()
                        .mt(px(5.0))
                        .text_size(sp(12.5))
                        .line_height(sp(18.0))
                        .text_color(theme.text_secondary)
                        .child(tr!("memory.empty_description")),
                )
                .into_any_element(),
            MemoryListState::Loaded { memories } => div()
                .flex()
                .flex_col()
                .gap(px(12.0))
                .child(render_memory_rows(self, memories, theme, cx))
                .child(self.render_memory_clear(theme, cx))
                .into_any_element(),
        }
    }

    fn render_memory_clear(&self, theme: Theme, cx: &mut Context<Self>) -> AnyElement {
        let armed = self.memory_clear_arming;
        div()
            .px(px(20.0))
            .py(px(14.0))
            .rounded(px(13.0))
            .bg(theme.raised)
            .flex()
            .items_center()
            .gap(px(20.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(sp(13.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(if armed { theme.danger } else { theme.text })
                            .child(tr!("memory.clear_title")),
                    )
                    .child(
                        div()
                            .mt(px(5.0))
                            .text_size(sp(12.5))
                            .line_height(sp(18.0))
                            .text_color(theme.text_secondary)
                            .child(tr!("memory.clear_description")),
                    ),
            )
            .child(
                div()
                    .id("memory-clear-all")
                    .tab_index(0)
                    .h(px(29.0))
                    .px(px(11.0))
                    .flex_none()
                    .rounded(px(7.0))
                    .border_1()
                    .border_color(if armed {
                        theme.danger
                    } else {
                        theme.border_strong
                    })
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_default()
                    .text_size(sp(12.5))
                    .text_color(if armed {
                        theme.danger
                    } else {
                        theme.text_secondary
                    })
                    .focus_visible(|style| style.border_color(theme.accent))
                    .hover(|element| element.bg(theme.overlay).text_color(theme.danger))
                    .child(if armed {
                        tr!("memory.confirm_clear")
                    } else {
                        tr!("memory.clear_title")
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.step_memory_clear(cx);
                    }))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        if !event.keystroke.modifiers.modified()
                            && matches!(event.keystroke.key.as_str(), "enter" | "space")
                        {
                            this.step_memory_clear(cx);
                            cx.stop_propagation();
                        }
                    }))
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        if this.memory_clear_arming {
                            this.memory_clear_arming = false;
                            cx.notify();
                        }
                    })),
            )
            .into_any_element()
    }
}

/// One memory row: content plus relative update time, with an arm-to-confirm
/// delete button following the skills-page pattern.
fn render_memory_rows(
    mack: &Mack,
    memories: &[StoredMemory],
    theme: Theme,
    cx: &mut Context<Mack>,
) -> AnyElement {
    let now = unix_time();
    let mut rows = div()
        .px(px(20.0))
        .py(px(8.0))
        .rounded(px(13.0))
        .bg(theme.raised)
        .flex()
        .flex_col();
    for (index, memory) in memories.iter().enumerate() {
        let id = memory.id;
        let armed = mack.memory_delete_arming == Some(id);
        let age = now.saturating_sub(memory.updated_at);
        let updated = if age < 60 {
            tr!("memory.updated_just_now")
        } else {
            super::sidebar::format_time_ago(age)
        };
        rows = rows.child(
            div()
                .py(px(9.0))
                .flex()
                .items_center()
                .gap(px(10.0))
                .when(index + 1 < memories.len(), |element| {
                    element.border_b_1().border_color(theme.border)
                })
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .text_size(sp(12.5))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(SharedString::from(memory.content.clone())),
                        )
                        .child(
                            div()
                                .mt(px(2.0))
                                .text_size(sp(12.0))
                                .text_color(theme.text_tertiary)
                                .child(SharedString::from(updated)),
                        ),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("memory-delete-{id}")))
                        .tab_index(0)
                        .h(px(25.0))
                        .px(px(9.0))
                        .flex_none()
                        .rounded(px(6.0))
                        .border_1()
                        .border_color(if armed {
                            theme.danger
                        } else {
                            theme.border_strong
                        })
                        .flex()
                        .items_center()
                        .gap(px(5.0))
                        .cursor_default()
                        .text_size(sp(12.5))
                        .text_color(if armed {
                            theme.danger
                        } else {
                            theme.text_secondary
                        })
                        .focus_visible(|style| style.border_color(theme.accent))
                        .hover(|element| element.bg(theme.overlay).text_color(theme.danger))
                        .child(icon(
                            "icons/trash.svg",
                            11.0,
                            if armed {
                                theme.danger
                            } else {
                                theme.text_tertiary
                            },
                        ))
                        .child(if armed {
                            tr!("memory.confirm_delete")
                        } else {
                            tr!("memory.delete")
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.step_memory_delete(id, cx);
                        }))
                        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                            if !event.keystroke.modifiers.modified()
                                && matches!(event.keystroke.key.as_str(), "enter" | "space")
                            {
                                this.step_memory_delete(id, cx);
                                cx.stop_propagation();
                            }
                        }))
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            if this.memory_delete_arming.take().is_some() {
                                cx.notify();
                            }
                        })),
                ),
        );
    }
    rows.into_any_element()
}

fn memory_retry_button(theme: Theme, cx: &mut Context<Mack>) -> AnyElement {
    div()
        .id("memory-retry-load")
        .tab_index(0)
        .h(px(29.0))
        .px(px(11.0))
        .flex_none()
        .rounded(px(7.0))
        .border_1()
        .border_color(theme.border_strong)
        .flex()
        .items_center()
        .justify_center()
        .cursor_default()
        .text_size(sp(12.5))
        .text_color(theme.text_secondary)
        .focus_visible(|style| style.border_color(theme.accent))
        .hover(|element| element.bg(theme.overlay))
        .child(tr!("memory.retry"))
        .on_click(cx.listener(|this, _, _, cx| {
            this.ensure_memory_list(true, cx);
        }))
        .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
            if !event.keystroke.modifiers.modified()
                && matches!(event.keystroke.key.as_str(), "enter" | "space")
            {
                this.ensure_memory_list(true, cx);
                cx.stop_propagation();
            }
        }))
        .into_any_element()
}
