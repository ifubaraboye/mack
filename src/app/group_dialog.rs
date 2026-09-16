//! Small modal for naming a chat group, at creation or rename time.
//!
//! The dialog owns a single-line name field plus one primary row. Enter
//! applies, Escape or the scrim dismisses. Naming used to live in the
//! sidebar header and the palette search field; both proved fragile (fresh
//! mounts stealing focus, query state doubling as draft state), so the
//! name now has a surface of its own. Callers only pick the mode: creating
//! assigns the new group to the given session, renaming edits in place.

use gpui::{KeyBinding, actions};

use super::*;

actions!(waku_group_dialog, [ConfirmGroupDialog, DismissGroupDialog]);

const DIALOG_CONTEXT: &str = "GroupDialog";
const DIALOG_INPUT_CONTEXT: &str = "GroupDialog > TextInput";

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("enter", ConfirmGroupDialog, Some(DIALOG_INPUT_CONTEXT)),
        KeyBinding::new("enter", ConfirmGroupDialog, Some(DIALOG_CONTEXT)),
        KeyBinding::new("escape", DismissGroupDialog, Some(DIALOG_CONTEXT)),
        KeyBinding::new(
            "escape",
            DismissGroupDialog,
            Some(DIALOG_INPUT_CONTEXT),
        ),
    ]);
}

pub(super) enum GroupDialogMode {
    CreateGroup { session_id: Uuid },
    RenameGroup { group_id: Uuid },
}

pub(super) struct GroupDialogState {
    mode: GroupDialogMode,
    name: Entity<TextInput>,
    save_focus: FocusHandle,
}

impl Waku {
    /// Open the naming modal. The name field is prefilled for renames and
    /// empty for creates; focus lands two frames out, once the deferred
    /// overlay has joined the dispatch tree.
    pub(super) fn open_group_dialog(
        &mut self,
        mode: GroupDialogMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prefill = match &mode {
            GroupDialogMode::CreateGroup { .. } => None,
            GroupDialogMode::RenameGroup { group_id } => self
                .chat_group(*group_id)
                .map(|group| group.name.clone()),
        };
        if matches!(mode, GroupDialogMode::RenameGroup { .. }) && prefill.is_none() {
            return;
        }
        let name = cx.new(|cx| {
            TextInput::new(window, cx).placeholder(tr!("group_dialog.name_placeholder"))
        });
        if let Some(prefill) = prefill {
            name.update(cx, |input, cx| {
                input.set_content(prefill, cx);
                input.select_all_text(cx);
            });
        }
        let focus = name.read(cx).focus();
        self.group_dialog = Some(GroupDialogState {
            mode,
            name,
            save_focus: cx.focus_handle(),
        });
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |window, cx| window.focus(&focus, cx));
        });
        cx.notify();
    }

    fn confirm_group_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.group_dialog.take() else {
            return;
        };
        let name = dialog.name.read(cx).content().trim().to_owned();
        if name.is_empty() {
            // Keep the dialog open: an unnamed group helps nobody.
            self.group_dialog = Some(dialog);
            return;
        }
        match dialog.mode {
            GroupDialogMode::CreateGroup { session_id } => {
                if let Some(group_id) = self.create_chat_group(session_id, cx) {
                    self.rename_chat_group(group_id, name, cx);
                }
            }
            GroupDialogMode::RenameGroup { group_id } => {
                self.rename_chat_group(group_id, name, cx);
            }
        }
        self.refresh_command_palette_visible_results(cx);
        self.close_group_dialog(window, cx);
    }

    fn close_group_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.group_dialog.take().is_none() {
            return;
        }
        let focus = self.composer_focus(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    pub(super) fn render_group_dialog(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let dialog = self.group_dialog.as_ref()?;
        let theme = Theme::current(cx);
        let (title, save_label) = match dialog.mode {
            GroupDialogMode::CreateGroup { .. } => {
                (tr!("sidebar.new_group"), tr!("sidebar.new_group"))
            }
            GroupDialogMode::RenameGroup { .. } => {
                (tr!("sidebar.rename_group"), tr!("common.rename"))
            }
        };
        let can_save = !dialog.name.read(cx).content().trim().is_empty();
        let weak = cx.entity().downgrade();
        let save_weak = weak.clone();
        let card = div()
            .id("group-dialog-card")
            .key_context(DIALOG_CONTEXT)
            .on_action(cx.listener(|waku, _: &ConfirmGroupDialog, window, cx| {
                waku.confirm_group_dialog(window, cx);
            }))
            .on_action(cx.listener(|waku, _: &DismissGroupDialog, window, cx| {
                waku.close_group_dialog(window, cx);
            }))
            .tab_group()
            .tab_stop(false)
            .w_full()
            .max_w(px(360.0))
            .overflow_hidden()
            .rounded(px(14.0))
            .bg(theme.composer)
            .shadow_xl()
            .flex()
            .flex_col()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .h(px(44.0))
                    .px(px(16.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .text_size(sp(14.0))
                    .text_color(theme.text)
                    .child(icon("icons/folder.svg", 15.0, theme.text))
                    .child(div().child(title)),
            )
            .child(
                div()
                    .mx(px(16.0))
                    .h(px(34.0))
                    .px(px(10.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.accent)
                    .bg(theme.inset)
                    .flex()
                    .items_center()
                    .text_size(sp(13.5))
                    .text_color(theme.text)
                    .child(dialog.name.clone()),
            )
            .child(div().mx(px(8.0)).mt(px(10.0)).h(px(1.0)).bg(theme.border))
            .child(
                div()
                    .p(px(8.0))
                    .child(
                        div()
                            .id("group-dialog-save")
                            .track_focus(&dialog.save_focus)
                            .when(can_save, |row| row.tab_index(0))
                            .h(px(38.0))
                            .w_full()
                            .px(px(10.0))
                            .rounded(px(9.0))
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .cursor_default()
                            .text_size(sp(14.0))
                            .text_color(if can_save {
                                theme.text
                            } else {
                                theme.text_ghost
                            })
                            .focus_visible(|style| style.border_1().border_color(theme.accent))
                            .when(can_save, |row| {
                                let click_weak = save_weak.clone();
                                let key_weak = save_weak.clone();
                                row.hover(|style| style.bg(theme.overlay_strong))
                                    .on_click(move |_, window, cx| {
                                        let _ = click_weak.update(cx, |waku, cx| {
                                            waku.confirm_group_dialog(window, cx)
                                        });
                                    })
                                    .on_key_down(move |event: &KeyDownEvent, window, cx| {
                                        if !event.keystroke.modifiers.modified()
                                            && matches!(
                                                event.keystroke.key.as_str(),
                                                "enter" | "space"
                                            )
                                        {
                                            let _ = key_weak.update(cx, |waku, cx| {
                                                waku.confirm_group_dialog(window, cx)
                                            });
                                            cx.stop_propagation();
                                        }
                                    })
                            })
                            .child(icon(
                                "icons/check.svg",
                                15.0,
                                if can_save {
                                    theme.text
                                } else {
                                    theme.text_ghost
                                },
                            ))
                            .child(div().min_w_0().flex_1().truncate().child(save_label)),
                    ),
            );
        let scrim = if theme.is_dark {
            gpui::hsla(0.0, 0.0, 0.0, 0.34)
        } else {
            gpui::hsla(0.0, 0.0, 0.0, 0.16)
        };
        let layer = div()
            .id("group-dialog-layer")
            .absolute()
            .inset_0()
            .occlude()
            .bg(scrim)
            .p(px(24.0))
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|waku, _, window, cx| waku.close_group_dialog(window, cx)),
            )
            .child(card);
        Some(gpui::deferred(layer).with_priority(4).into_any_element())
    }
}
