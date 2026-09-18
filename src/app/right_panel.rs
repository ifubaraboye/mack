use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::*;

const TAB_SCROLL_FADE_WIDTH: f32 = 24.0;

#[derive(Clone, Debug, Eq, PartialEq)]
enum TranscriptLinkRoute {
    Finder(PathBuf),
    External,
}

fn positive_number(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && value.parse::<usize>().is_ok_and(|value| value > 0)
}

fn line_fragment(fragment: &str) -> bool {
    let Some(location) = fragment.strip_prefix('L') else {
        return false;
    };
    match location.split_once('C') {
        Some((line, column)) => positive_number(line) && positive_number(column),
        None => positive_number(location),
    }
}

/// Removes the `:line`, `:line:column`, or `#LlineCcolumn` suffixes Codex uses
/// in clickable local-file references. The location is not yet consumed by
/// Mack's compact editor, but it must not become part of the filesystem path.
fn strip_file_location(target: &str) -> &str {
    if let Some((path, fragment)) = target.rsplit_once('#')
        && line_fragment(fragment)
    {
        return path;
    }

    let Some((before_last, last)) = target.rsplit_once(':') else {
        return target;
    };
    if !positive_number(last) {
        return target;
    }
    if let Some((path, line)) = before_last.rsplit_once(':')
        && positive_number(line)
    {
        path
    } else {
        before_last
    }
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn percent_decode_file_path(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let (Some(high), Some(low)) = (
                bytes.get(index + 1).copied().and_then(hex_value),
                bytes.get(index + 2).copied().and_then(hex_value),
            )
        {
            decoded.push(high << 4 | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).unwrap_or_else(|_| path.to_owned())
}

fn markdown_file_link_path(target: &str) -> Option<PathBuf> {
    let target = strip_file_location(target.trim());
    if target
        .get(..5)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("file:"))
    {
        return url::Url::parse(target).ok()?.to_file_path().ok();
    }

    let path = PathBuf::from(percent_decode_file_path(target));
    path.is_absolute().then_some(path)
}

fn normalized_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn transcript_link_route(target: &str, workspace: Option<&Path>) -> TranscriptLinkRoute {
    let Some(path) = markdown_file_link_path(target) else {
        return TranscriptLinkRoute::External;
    };
    // Without the Files surface every file link reveals in the file manager;
    // workspace membership no longer changes the destination.
    let _ = workspace;
    TranscriptLinkRoute::Finder(normalized_path(&path))
}

#[derive(Clone, Copy)]
pub(super) struct DiffRowStyle {
    gutter_width: f32,
    row_height: f32,
    text_size: f32,
    /// What to put in the gutter of a row that has no line number. Git always
    /// reports positions, so this only comes up on a diff synthesized from a
    /// provider's before/after text: there the `+`/`-` marker stands in, which
    /// keeps the gutter from going blank and the meaning off color alone.
    marker_fallback: bool,
}

impl DiffRowStyle {
    /// Diff rows at the user's code font size. The gutter holds a
    /// right-aligned line number: ~0.6em per mono digit, five digits, plus
    /// its padding and border.
    pub(super) fn review(text_size: f32) -> Self {
        Self {
            gutter_width: (text_size * 3.0 + 14.0).round(),
            row_height: (text_size * 1.5).round(),
            text_size,
            marker_fallback: false,
        }
    }

    /// Transcript activity diff rows, so an edit reads the same wherever
    /// it is opened.
    pub(super) fn activity(text_size: f32) -> Self {
        Self {
            marker_fallback: true,
            ..Self::review(text_size)
        }
    }
}

/// Selection identity for one diff code row. Selection resolves a drag by
/// looking rows up by key, so every row must have its own.
///
/// Rows with line numbers key on them: they survive Review's gap expansion,
/// where a revealed gap shifts every later row's index. Rows without them — a
/// diff synthesized from a provider's before/after text — key on the row index
/// instead, which is stable there because an activity diff is only ever
/// rebuilt whole. Keying those on their (absent) numbers gave every added row
/// the same key, and a drag resolved against whichever duplicate registered
/// first: selections jumped rows, skipped wrapped lines, and collapsed when
/// the head crossed into context.
fn diff_row_selection_key(
    key_prefix: &str,
    line: &crate::review_diff::Line,
    index: usize,
) -> String {
    let kind = match &line.kind {
        crate::review_diff::LineKind::Context => "context",
        crate::review_diff::LineKind::Addition => "addition",
        crate::review_diff::LineKind::Deletion => "deletion",
        _ => "other",
    };
    match (line.old_line, line.new_line) {
        (None, None) => format!("{key_prefix}-line-{}-{kind}-i{index}", line.file_index),
        (old, new) => format!(
            "{key_prefix}-line-{}-{kind}-{}-{}",
            line.file_index,
            old.unwrap_or(0),
            new.unwrap_or(0),
        ),
    }
}

/// One context, addition, or deletion row, shared by the Review panel and the
/// diff inside an expanded file-change activity so the two never drift.
pub(super) fn render_diff_code_row(
    line: &crate::review_diff::Line,
    index: usize,
    key_prefix: &str,
    selection: &TranscriptSelection,
    style: DiffRowStyle,
    theme: &Theme,
) -> AnyElement {
    let semantic_body_opacity = if theme.is_dark { 0.20 } else { 0.12 };
    let semantic_gutter_opacity = if theme.is_dark { 0.15 } else { 0.09 };
    let (marker, body_background, gutter_background, edge, number_color) = match &line.kind {
        crate::review_diff::LineKind::Addition => (
            "+",
            Some(theme.success.opacity(semantic_body_opacity)),
            Some(theme.success.opacity(semantic_gutter_opacity)),
            Some(theme.success),
            theme.success,
        ),
        crate::review_diff::LineKind::Deletion => (
            "-",
            Some(theme.danger.opacity(semantic_body_opacity)),
            Some(theme.danger.opacity(semantic_gutter_opacity)),
            Some(theme.danger),
            theme.danger,
        ),
        _ => (" ", None, None, None, theme.text_tertiary),
    };
    let shown_line = line.new_line.or(line.old_line);
    let flat = review_diff_flat_text(line, theme);
    let selectable = md::render::selectable_flat_text(
        &flat,
        crate::md::selection::TextKey::new(diff_row_selection_key(key_prefix, line, index), 0),
        selection.clone(),
        theme.code_wash,
        theme.selection,
        false,
    );
    let gutter = div()
        .w(px(style.gutter_width))
        .min_h(px(style.row_height))
        .self_stretch()
        .flex_none()
        .pr(px(9.0))
        .flex()
        .items_start()
        .justify_end()
        .border_r_1()
        .border_color(theme.border)
        .text_color(number_color)
        .when_some(gutter_background, |gutter, background| {
            gutter.bg(background)
        })
        .child(
            shown_line
                .map(|line| line.to_string())
                .or_else(|| style.marker_fallback.then(|| marker.to_owned()))
                .unwrap_or_default(),
        );
    let body = div()
        .min_h(px(style.row_height))
        .self_stretch()
        .min_w_0()
        .flex_1()
        .pl(px(12.0))
        .flex()
        .items_start()
        .when_some(body_background, |body, background| body.bg(background))
        .child(
            div()
                .id(SharedString::from(format!(
                    "{key_prefix}-line-content-{index}"
                )))
                .min_h(px(style.row_height))
                .min_w_0()
                .flex_1()
                .pr(px(10.0))
                .flex()
                .items_start()
                .overflow_hidden()
                .whitespace_normal()
                .child(selectable),
        );
    div()
        .id(SharedString::from(format!("{key_prefix}-row-{index}")))
        .w_full()
        .min_w_0()
        .min_h(px(style.row_height))
        // A wrapped line makes the row taller than one line. Stacked in a
        // scrolling column, a shrinkable row would be squeezed back to one
        // and paint its overflow over the row beneath it.
        .flex_none()
        .flex()
        .items_stretch()
        .font_family(md::render::MONO_FAMILY)
        .text_size(px(style.text_size))
        .line_height(px(style.row_height))
        .when_some(edge, |row, edge| row.border_l_2().border_color(edge))
        .child(gutter)
        .child(body)
        .into_any_element()
}

fn review_diff_flat_text(line: &crate::review_diff::Line, theme: &Theme) -> md::render::FlatText {
    let text = line.content.clone();
    let palette = MarkdownPalette::from_theme(theme);
    let code_font = font(md::render::MONO_FAMILY);
    let mut runs = Vec::with_capacity(line.tokens.len() * 2 + 1);
    let mut offset = 0;
    let mut push = |len: usize, color: Hsla| {
        if len > 0 {
            runs.push(TextRun {
                len,
                font: code_font.clone(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            });
        }
    };
    for token in &line.tokens {
        if token.range.start > offset {
            push(token.range.start - offset, theme.text_secondary);
        }
        push(token.range.len(), palette.token(token.class));
        offset = token.range.end;
    }
    if offset < text.len() {
        push(text.len() - offset, theme.text_secondary);
    }
    md::render::FlatText {
        text: text.into(),
        runs,
        links: Vec::new(),
        code_ranges: Vec::new(),
        math: None,
    }
}

impl RightPanelSurface {
    pub(super) fn new_terminal() -> Self {
        Self::Terminal(Uuid::new_v4())
    }

    fn terminal_id(&self) -> Option<Uuid> {
        match self {
            Self::Terminal(id) => Some(*id),
            _ => None,
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Terminal(_) => tr!("right_panel.terminal"),
            Self::BackgroundWork { key, title } => {
                if title.is_empty() {
                    match key.kind {
                        BackgroundWorkKind::Process => tr!("background.process"),
                        BackgroundWorkKind::Monitor => tr!("background.monitor"),
                        BackgroundWorkKind::Subagent => tr!("background.subagent"),
                    }
                } else {
                    title.clone()
                }
            }
        }
    }

    fn icon_path(&self) -> &'static str {
        match self {
            Self::Terminal(_) => "icons/terminal.svg",
            Self::BackgroundWork { key, .. } => work_kind_icon(key.kind),
        }
    }
}

fn right_panel_tab_label(surface: &RightPanelSurface) -> String {
    single_line_label(&surface.label())
}

fn right_panel_tab_icon(surface: &RightPanelSurface) -> &'static str {
    surface.icon_path()
}

fn reusable_surface_index(
    surfaces: &[RightPanelSurface],
    requested: &RightPanelSurface,
) -> Option<usize> {
    match requested {
        RightPanelSurface::Terminal(_) => None,
        RightPanelSurface::BackgroundWork { key, .. } => surfaces.iter().position(|surface| {
            matches!(surface, RightPanelSurface::BackgroundWork { key: candidate, .. } if candidate == key)
        }),
    }
}

#[derive(Clone, Copy)]
enum TabScrollFadeSide {
    Left,
    Right,
}

fn tab_scroll_fade_visibility(offset_x: Pixels, max_offset: Pixels) -> (bool, bool) {
    let scrolled = -offset_x;
    let threshold = px(0.5);
    (scrolled > threshold, max_offset - scrolled > threshold)
}

fn fade_safe_tab_offset(
    current_offset: Pixels,
    max_offset: Pixels,
    item_left: Pixels,
    item_right: Pixels,
    viewport_left: Pixels,
    viewport_right: Pixels,
) -> Pixels {
    let inset = px(TAB_SCROLL_FADE_WIDTH);
    let mut offset = current_offset;
    let visible_left = item_left + offset;
    let visible_right = item_right + offset;
    if visible_left < viewport_left + inset {
        offset += viewport_left + inset - visible_left;
    } else if visible_right > viewport_right - inset {
        offset -= visible_right - (viewport_right - inset);
    }
    offset.clamp(-max_offset, px(0.0))
}

fn tab_scroll_reveal_guard(
    scroll_handle: ScrollHandle,
    tab_index: usize,
    mack: WeakEntity<Mack>,
) -> impl IntoElement {
    canvas(
        move |_, window, _| {
            if let Some(item) = scroll_handle.bounds_for_item(tab_index) {
                let viewport = scroll_handle.bounds();
                let offset = scroll_handle.offset();
                let safe_offset = fade_safe_tab_offset(
                    offset.x,
                    scroll_handle.max_offset().x,
                    item.left(),
                    item.right(),
                    viewport.left(),
                    viewport.right(),
                );
                if safe_offset != offset.x {
                    scroll_handle.set_offset(point(safe_offset, offset.y));
                }
            }

            window.on_next_frame(move |_, cx| {
                let _ = mack.update(cx, |this, cx| {
                    if this.right_panel_pending_tab_reveal == Some(tab_index) {
                        this.right_panel_pending_tab_reveal = None;
                        cx.notify();
                    }
                });
            });
        },
        |_, _, _, _| {},
    )
    .absolute()
    .size_full()
}

fn tab_scroll_fade(
    scroll_handle: ScrollHandle,
    side: TabScrollFadeSide,
    surface: Hsla,
) -> impl IntoElement {
    canvas(
        move |bounds, _, _| {
            let (show_left, show_right) =
                tab_scroll_fade_visibility(scroll_handle.offset().x, scroll_handle.max_offset().x);
            let visible = match side {
                TabScrollFadeSide::Left => show_left,
                TabScrollFadeSide::Right => show_right,
            };
            visible.then(|| {
                let transparent = surface.opacity(0.0);
                let background = match side {
                    TabScrollFadeSide::Left => linear_gradient(
                        90.0,
                        linear_color_stop(surface, 0.0),
                        linear_color_stop(transparent, 1.0),
                    ),
                    TabScrollFadeSide::Right => linear_gradient(
                        90.0,
                        linear_color_stop(transparent, 0.0),
                        linear_color_stop(surface, 1.0),
                    ),
                };
                fill(bounds, background)
            })
        },
        |_, fade, window, _| {
            if let Some(fade) = fade {
                window.paint_quad(fade);
            }
        },
    )
    .absolute()
    .top_0()
    .bottom_0()
    .when(matches!(side, TabScrollFadeSide::Left), |element| {
        element.left_0()
    })
    .when(matches!(side, TabScrollFadeSide::Right), |element| {
        element.right_0()
    })
    .w(px(TAB_SCROLL_FADE_WIDTH))
}

#[allow(clippy::items_after_test_module)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_file_links_route_by_the_active_workspace() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"));
        let project_file = workspace.join("src/app/right_panel.rs");
        let project_file_with_line = format!("{}:1596", project_file.display());
        let project_file_with_column = format!("{}:1596:8", project_file.display());

        // Without the Files surface every file link reveals in the file
        // manager; workspace membership no longer changes the destination.
        assert_eq!(
            transcript_link_route(&project_file_with_line, Some(workspace)),
            TranscriptLinkRoute::Finder(normalized_path(&project_file))
        );
        assert_eq!(
            transcript_link_route(&project_file_with_column, Some(workspace)),
            TranscriptLinkRoute::Finder(normalized_path(&project_file))
        );

        let encoded_file_url =
            url::Url::from_file_path(workspace.join("My File.rs")).expect("absolute file path");
        assert_eq!(
            transcript_link_route(&format!("{encoded_file_url}#L12C4"), Some(workspace)),
            TranscriptLinkRoute::Finder(normalized_path(&workspace.join("My File.rs")))
        );

        let outside_file = workspace.join("../kero/src/app.rs");
        let outside_file_with_line = format!("{}:20", outside_file.display());
        assert_eq!(
            transcript_link_route(&outside_file_with_line, Some(workspace)),
            TranscriptLinkRoute::Finder(normalized_path(&outside_file))
        );
        assert_eq!(
            transcript_link_route("https://example.com/file.rs:12", Some(workspace)),
            TranscriptLinkRoute::External
        );
    }

    /// Selection resolves rows by key, so a repeated key makes a drag jump
    /// between the duplicates. Numbered rows keep their number-derived keys
    /// (stable across Review's gap expansion); rows a provider never
    /// positioned fall back to the row index.
    #[test]
    fn diff_row_selection_keys_are_unique_even_without_line_numbers() {
        let positionless =
            crate::review_diff::from_file_changes(&[crate::model::ActivityFileChange {
                path: "a.md".into(),
                additions: Some(2),
                deletions: Some(0),
                status: None,
                diff: Some("@@\n+one\n+two\n \n+three\n".into()),
            }]);
        let keys = positionless
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| {
                matches!(
                    line.kind,
                    crate::review_diff::LineKind::Context
                        | crate::review_diff::LineKind::Addition
                        | crate::review_diff::LineKind::Deletion
                )
            })
            .map(|(index, line)| diff_row_selection_key("activity", line, index))
            .collect::<Vec<_>>();
        let unique = keys.iter().collect::<HashSet<_>>();
        assert_eq!(unique.len(), keys.len(), "{keys:?}");

        let numbered = crate::review_diff::Line {
            file_index: 0,
            old_line: Some(4),
            new_line: Some(6),
            kind: crate::review_diff::LineKind::Context,
            content: "kept".into(),
            tokens: Vec::new(),
        };
        assert_eq!(
            diff_row_selection_key("review-diff", &numbered, 9),
            "review-diff-line-0-context-4-6",
        );
    }

    /// A wrapped diff line must grow its row rather than be clipped by it.
    /// Both the panel's own rows and the shared code row have to hold this,
    /// and the shared one is also what the transcript's diff paints with.
    #[test]
    fn diff_text_rows_soft_wrap() {
        let source = include_str!("right_panel.rs");
        let shared = source
            .split_once("\npub(super) fn render_diff_code_row(")
            .expect("shared diff code row")
            .1
            .split_once("\nfn review_diff_flat_text(")
            .expect("shared diff code row end")
            .0;

        assert!(!shared.contains(".whitespace_nowrap()"));
        assert!(shared.contains(".whitespace_normal()"));
        assert!(shared.contains(".min_h(px(style.row_height))"));
        assert!(!shared.contains(".h(px(style.row_height))"));
    }

    /// The editor colours code with the in-house lexer, so what matters is that
    /// the names `file_highlighter_language` produces are ones the lexer knows.
    /// The few it does not are listed here deliberately: they render as plain
    /// monospace rather than silently looking broken.
    #[test]
    fn mapped_languages_resolve_in_the_in_house_lexer() {
        use crate::md::highlight::{Lang, lang_for_tag};

        for (language, expected) in [
            ("rust", Some(Lang::Rust)),
            ("tsx", Some(Lang::Script)),
            ("swift", Some(Lang::Swift)),
            ("json", Some(Lang::Json)),
            ("toml", Some(Lang::Toml)),
            ("yaml", Some(Lang::Yaml)),
            ("make", Some(Lang::Shell)),
            ("cpp", Some(Lang::C)),
            ("markdown", Some(Lang::Markdown)),
            // Not yet lexed; these fall back to unhighlighted monospace.
            ("elixir", None),
            ("text", None),
        ] {
            assert_eq!(lang_for_tag(language), expected, "{language}");
        }
    }

    #[test]
    fn the_editor_lexer_colours_code_it_recognises() {
        use crate::md::highlight::{Carry, Lang, TokenClass, tokenize_line};

        let line = r#"export function Card({ title }: { title: string }) {"#;
        let spans = tokenize_line(Lang::Script, line, Carry::None)
            .0
            .into_iter()
            .map(|token| (&line[token.range], token.class))
            .collect::<Vec<_>>();

        assert!(spans.contains(&("export", TokenClass::Keyword)));
        assert!(spans.contains(&("function", TokenClass::Keyword)));
        assert!(spans.contains(&("Card", TokenClass::Function)));
    }

    #[test]
    fn right_panel_tab_titles_stay_on_one_line() {
        let source = include_str!("right_panel.rs");
        let header = source
            .split_once("\n    fn render_right_panel_header(")
            .expect("right panel header renderer")
            .1
            .split_once("\n    fn render_right_panel_empty_message(")
            .expect("right panel header renderer end")
            .0;

        assert!(header.contains(".truncate()"));
        assert!(!header.contains(".line_clamp(1)"));

        let background = RightPanelSurface::BackgroundWork {
            key: BackgroundWorkKey::new(BackgroundWorkKind::Process, "process-1"),
            title: "node -e '\n  const value = 1'".into(),
        };
        assert_eq!(
            right_panel_tab_label(&background),
            "node -e ' const value = 1'"
        );
    }

    #[test]
    fn only_reuses_single_instance_surface_tabs() {
        let terminal = RightPanelSurface::new_terminal();
        let background = RightPanelSurface::BackgroundWork {
            key: BackgroundWorkKey::new(BackgroundWorkKind::Process, "process-1"),
            title: "Process one".into(),
        };
        let surfaces = vec![terminal, background];

        assert_eq!(
            reusable_surface_index(&surfaces, &RightPanelSurface::new_terminal()),
            None
        );
        assert_eq!(
            reusable_surface_index(
                &surfaces,
                &RightPanelSurface::BackgroundWork {
                    key: BackgroundWorkKey::new(BackgroundWorkKind::Process, "process-1"),
                    title: "Renamed process".into(),
                },
            ),
            Some(1)
        );
    }

    #[test]
    fn right_panel_state_isolated_by_session() {
        let session_with_terminal = Uuid::new_v4();
        let other_session = Uuid::new_v4();
        let terminal_id = Uuid::new_v4();
        let mut states = HashMap::new();
        let mut terminal_state = RightPanelSessionState::empty(true);
        terminal_state.surfaces = vec![RightPanelSurface::Terminal(terminal_id)];
        terminal_state.active_surface = Some(0);
        states.insert(session_with_terminal, terminal_state);

        let other_state = RightPanelSessionState::take_or_closed(&mut states, other_session);
        assert!(!other_state.visible);
        assert!(other_state.surfaces.is_empty());
        assert_eq!(other_state.active_surface, None);

        let restored = RightPanelSessionState::take_or_closed(&mut states, session_with_terminal);
        assert!(restored.visible);
        assert_eq!(
            restored.surfaces,
            vec![RightPanelSurface::Terminal(terminal_id)]
        );
        assert_eq!(restored.active_surface, Some(0));
    }

    #[test]
    fn tab_scroll_fades_only_show_toward_hidden_content() {
        assert_eq!(
            tab_scroll_fade_visibility(px(0.0), px(120.0)),
            (false, true)
        );
        assert_eq!(
            tab_scroll_fade_visibility(px(-40.0), px(120.0)),
            (true, true)
        );
        assert_eq!(
            tab_scroll_fade_visibility(px(-120.0), px(120.0)),
            (true, false)
        );
        assert_eq!(tab_scroll_fade_visibility(px(0.0), px(0.0)), (false, false));
    }

    #[test]
    fn selected_tab_offset_clears_fade_overlays() {
        assert_eq!(
            fade_safe_tab_offset(
                px(-100.0),
                px(300.0),
                px(90.0),
                px(190.0),
                px(0.0),
                px(300.0),
            ),
            px(-66.0)
        );
        assert_eq!(
            fade_safe_tab_offset(
                px(-100.0),
                px(324.0),
                px(300.0),
                px(400.0),
                px(0.0),
                px(300.0),
            ),
            px(-124.0)
        );
        assert_eq!(
            fade_safe_tab_offset(px(0.0), px(0.0), px(0.0), px(100.0), px(0.0), px(300.0),),
            px(0.0)
        );
    }
}

impl Mack {
    pub(super) fn open_transcript_link(&mut self, target: &str, cx: &mut Context<Self>) -> bool {
        match transcript_link_route(target, self.selected_workspace_path()) {
            TranscriptLinkRoute::Finder(path) => {
                if self.daemon.is_remote() {
                    self.show_toast(tr!("errors.remote_host_path"));
                    cx.notify();
                } else {
                    crate::platform::reveal_in_file_manager(&path, cx);
                }
            }
            TranscriptLinkRoute::External => return false,
        }
        true
    }

    /// Open a path a tool reported, from an activity in the transcript.
    ///
    /// Providers name a changed file however they like — absolute, or relative
    /// to the session's workspace — so resolve it before routing it to the
    /// file manager.
    pub(super) fn open_activity_file(&mut self, path: &str, cx: &mut Context<Self>) {
        let path = Path::new(path.trim());
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else if let Some(workspace) = self.selected_workspace_path() {
            workspace.join(path)
        } else {
            return;
        };
        self.open_transcript_link(&resolved.to_string_lossy(), cx);
    }

    pub(super) fn store_selected_right_panel_state(&mut self) {
        let Some(session_id) = self.state.selected_session else {
            return;
        };
        let state = self.take_active_right_panel_state();
        self.right_panel_session_states.insert(session_id, state);
    }

    pub(super) fn restore_right_panel_state(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        let state = RightPanelSessionState::take_or_closed(
            &mut self.right_panel_session_states,
            session_id,
        );
        self.replace_active_right_panel_state(state);
        self.ensure_right_panel_terminals(cx);
        self.state.right_panel_visible = self.right_panel_visible;
        self.ensure_right_panel_terminals(cx);
        if self.right_panel_visible {
            self.request_active_terminal_focus();
        }
    }

    pub(super) fn remove_right_panel_session_state(&mut self, session_id: Uuid) {
        let state = if self.state.selected_session == Some(session_id) {
            let state = self.take_active_right_panel_state();
            self.replace_active_right_panel_state(RightPanelSessionState::empty(false));
            Some(state)
        } else {
            self.right_panel_session_states.remove(&session_id)
        };
        if let Some(state) = state {
            for surface in &state.surfaces {
                if let Some(terminal_id) = surface.terminal_id() {
                    self.right_panel_terminals.remove(&terminal_id);
                }
            }
        }
    }

    fn take_active_right_panel_state(&mut self) -> RightPanelSessionState {
        RightPanelSessionState {
            visible: self.right_panel_visible,
            surfaces: std::mem::take(&mut self.right_panel_surfaces),
            active_surface: self.right_panel_active_surface.take(),
            tabs_scroll_handle: std::mem::replace(
                &mut self.right_panel_tabs_scroll_handle,
                ScrollHandle::new(),
            ),
            pending_tab_reveal: self.right_panel_pending_tab_reveal.take(),
        }
    }

    fn replace_active_right_panel_state(&mut self, state: RightPanelSessionState) {
        self.right_panel_visible = state.visible;
        self.right_panel_surfaces = state.surfaces;
        self.right_panel_active_surface = state.active_surface;
        self.right_panel_tabs_scroll_handle = state.tabs_scroll_handle;
        self.right_panel_pending_tab_reveal = state.pending_tab_reveal;
    }

    fn reveal_right_panel_tab(&mut self, index: usize) {
        self.right_panel_pending_tab_reveal = Some(index);
        self.right_panel_tabs_scroll_handle.scroll_to_item(index);
    }

    fn active_right_panel_surface(&self) -> Option<&RightPanelSurface> {
        self.right_panel_active_surface
            .and_then(|index| self.right_panel_surfaces.get(index))
    }

    pub(super) fn request_active_terminal_focus(&mut self) {
        self.right_panel_pending_terminal_focus = self
            .active_right_panel_surface()
            .and_then(RightPanelSurface::terminal_id);
    }

    pub(super) fn open_right_panel_surface(
        &mut self,
        surface: RightPanelSurface,
        cx: &mut Context<Self>,
    ) {
        let reusable_index = reusable_surface_index(&self.right_panel_surfaces, &surface);
        if let Some(terminal_id) = surface.terminal_id() {
            self.ensure_right_panel_terminal(terminal_id, cx);
        }
        let index = match reusable_index {
            Some(index) => index,
            None => {
                self.right_panel_surfaces.push(surface);
                self.right_panel_surfaces.len() - 1
            }
        };
        self.right_panel_active_surface = Some(index);
        self.reveal_right_panel_tab(index);
        self.request_active_terminal_focus();
        self.set_right_panel_visible(true, cx);
        cx.notify();
    }

    fn close_right_panel_surface(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.right_panel_surfaces.len() {
            return;
        }
        if let Some(terminal_id) = self.right_panel_surfaces[index].terminal_id() {
            self.right_panel_terminals.remove(&terminal_id);
        }
        self.right_panel_surfaces.remove(index);
        self.right_panel_active_surface = if self.right_panel_surfaces.is_empty() {
            None
        } else {
            Some(match self.right_panel_active_surface {
                Some(active) if active > index => active - 1,
                Some(active) if active == index => index.saturating_sub(1),
                Some(active) => active.min(self.right_panel_surfaces.len() - 1),
                None => 0,
            })
        };
        if let Some(active) = self.right_panel_active_surface {
            self.reveal_right_panel_tab(active);
            self.request_active_terminal_focus();
        } else {
            self.right_panel_pending_tab_reveal = None;
            self.right_panel_pending_terminal_focus = None;
            self.set_right_panel_visible(false, cx);
        }
        cx.notify();
    }

    pub(super) fn close_window_or_right_panel_tab_action(
        &mut self,
        _: &CloseWindow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(active) = self.right_panel_active_surface {
            self.close_right_panel_surface(active, cx);
            if self.right_panel_surfaces.is_empty() {
                let focus_handle = self.composer_focus(cx);
                window.focus(&focus_handle, cx);
            }
        } else {
            crate::platform::hide_window(window);
        }
    }

    pub(super) fn render_right_panel_toggle(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = Theme::current(cx);
        div()
            .id("toggle-right-panel")
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
            .child(icon("icons/panel-right.svg", 14.0, theme.text_tertiary))
            .tooltip(|window, cx| Tooltip::new(tr!("right_panel.toggle")).build(window, cx))
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                this.set_right_panel_visible(!this.right_panel_visible, cx);
            }))
    }

    fn render_right_panel_terminal_for(
        &mut self,
        terminal_id: Uuid,
        width: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.right_panel_terminals
            .get(&terminal_id)
            .cloned()
            .inspect(|terminal| {
                terminal.update(cx, |terminal, _| terminal.set_panel_width(width));
            })
            .map(IntoElement::into_any_element)
            .unwrap_or_else(|| {
                self.render_right_panel_empty_message(
                    tr!("right_panel.terminal_unavailable"),
                    tr!("right_panel.terminal_unavailable_description"),
                    cx,
                )
                .into_any_element()
            })
    }

    /// Empty panel: no terminal tabs are open. Offers an explicit way back
    /// instead of spawning terminals unprompted, so closing the last tab
    /// always reads as closed.
    fn render_right_panel_no_terminal(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let weak = cx.entity().downgrade();
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(12.0))
            .pb(px(32.0))
            .child(
                div()
                    .text_size(sp(13.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(tr!("right_panel.terminal")),
            )
            .child(
                div()
                    .text_size(sp(12.5))
                    .text_color(theme.text_tertiary)
                    .child(tr!("right_panel.terminal_description")),
            )
            .child(
                div()
                    .id("right-panel-new-terminal")
                    .tab_index(0)
                    .focus_visible(|style| style.border_color(theme.accent))
                    .h(px(28.0))
                    .px(px(12.0))
                    .rounded(px(7.0))
                    .border_1()
                    .border_color(theme.border_strong)
                    .flex()
                    .items_center()
                    .cursor_default()
                    .text_size(sp(12.5))
                    .text_color(theme.text_secondary)
                    .hover(|element| element.bg(theme.overlay))
                    .child(tr!("right_panel.new_terminal"))
                    .on_click(move |_, _, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.open_right_panel_surface(RightPanelSurface::new_terminal(), cx);
                        });
                    }),
            )
            .into_any_element()
    }

    pub(super) fn render_right_panel(
        &mut self,
        width: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = Theme::current(cx);
        let active_terminal_id = self
            .active_right_panel_surface()
            .and_then(RightPanelSurface::terminal_id);
        if self.right_panel_pending_terminal_focus == active_terminal_id
            && let Some(terminal_id) = active_terminal_id
            && let Some(terminal) = self.right_panel_terminals.get(&terminal_id)
        {
            let focus_handle = terminal.read(cx).focus_handle(cx);
            window.focus(&focus_handle, cx);
            self.right_panel_pending_terminal_focus = None;
        }
        let body = match self.active_right_panel_surface().cloned() {
            None => self.render_right_panel_no_terminal(cx),
            Some(RightPanelSurface::BackgroundWork { key, .. }) => self
                .render_background_work_surface(&key, cx)
                .into_any_element(),
            Some(RightPanelSurface::Terminal(terminal_id)) => self
                .render_right_panel_terminal_for(terminal_id, width, cx)
                .into_any_element(),
        };

        div()
            .id("right-panel")
            .w(px(width))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .min_w_0()
            .border_l_1()
            .border_color(theme.border_strong)
            .bg(theme.surface)
            .relative()
            .child(self.render_right_panel_header(window, cx))
            .child(body)
            .child(self.render_panel_resize_handle(
                "right-panel-resize-handle",
                PanelResizeTarget::RightPanel,
                cx,
            ))
    }

    fn ensure_right_panel_terminal(&mut self, terminal_id: Uuid, cx: &mut Context<Self>) {
        if self.daemon.is_remote() {
            // A desktop PTY would interpret the daemon's cwd on the wrong
            // machine. Keep the surface unavailable until the protocol grows
            // a daemon-owned streaming terminal.
            self.right_panel_terminals.remove(&terminal_id);
            return;
        }
        let Some(working_directory) = self
            .selected_workspace_path()
            .map(std::path::Path::to_path_buf)
        else {
            self.right_panel_terminals.remove(&terminal_id);
            return;
        };
        let matches_project = self
            .right_panel_terminals
            .get(&terminal_id)
            .is_some_and(|terminal| terminal.read(cx).working_directory() == working_directory);
        if !matches_project {
            self.right_panel_terminals.insert(
                terminal_id,
                cx.new(|cx| TerminalView::new(working_directory.clone(), cx)),
            );
        }
    }

    pub(super) fn ensure_right_panel_terminals(&mut self, cx: &mut Context<Self>) {
        let active_terminal_ids = self
            .right_panel_surfaces
            .iter()
            .filter_map(RightPanelSurface::terminal_id)
            .collect::<Vec<_>>();
        let retained_terminal_ids = active_terminal_ids
            .iter()
            .copied()
            .chain(self.right_panel_session_states.values().flat_map(|state| {
                state
                    .surfaces
                    .iter()
                    .filter_map(RightPanelSurface::terminal_id)
            }))
            .collect::<HashSet<_>>();
        self.right_panel_terminals
            .retain(|terminal_id, _| retained_terminal_ids.contains(terminal_id));
        for terminal_id in active_terminal_ids {
            self.ensure_right_panel_terminal(terminal_id, cx);
        }
    }

    fn render_right_panel_header(&self, window: &Window, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = Theme::current(cx);
        let active_surface = self.right_panel_active_surface;
        let mut tabs = div()
            .id("right-panel-tabs")
            .h_full()
            .min_w_0()
            .flex_1()
            .flex()
            .items_center()
            .gap(px(4.0))
            .overflow_x_scroll()
            .track_scroll(&self.right_panel_tabs_scroll_handle);
        for (index, surface) in self.right_panel_surfaces.iter().cloned().enumerate() {
            let active = active_surface == Some(index);
            let label = SharedString::from(right_panel_tab_label(&surface));
            let icon_path = right_panel_tab_icon(&surface);
            let activate_weak = cx.entity().downgrade();
            let close_weak = cx.entity().downgrade();
            tabs = tabs.child(
                div()
                    .id(SharedString::from(format!("right-panel-tab-{index}")))
                    .h(px(28.0))
                    .min_w(px(100.0))
                    .max_w(px(176.0))
                    .px(px(8.0))
                    .rounded(px(6.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .cursor_default()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .when(active, |element| element.bg(theme.overlay_strong))
                    .when(!active, |element| {
                        element.hover(|element| element.bg(theme.overlay))
                    })
                    .child(icon(icon_path, 13.0, theme.text_secondary).into_any_element())
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_size(sp(12.5))
                            .text_color(if active {
                                theme.text
                            } else {
                                theme.text_secondary
                            })
                            .child(label),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("close-right-panel-tab-{index}")))
                            .w(px(16.0))
                            .h(px(16.0))
                            .rounded(px(4.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .hover(|element| element.bg(theme.overlay_strong))
                            .child(icon("icons/x.svg", 10.0, theme.text_tertiary))
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                let _ = close_weak.update(cx, |this, cx| {
                                    this.close_right_panel_surface(index, cx);
                                });
                            }),
                    )
                    .on_click(move |_, _, cx| {
                        let _ = activate_weak.update(cx, |this, cx| {
                            this.right_panel_active_surface = Some(index);
                            this.reveal_right_panel_tab(index);
                            this.request_active_terminal_focus();
                            cx.notify();
                        });
                    }),
            );
        }
        tabs = tabs.child(div().w(px(TAB_SCROLL_FADE_WIDTH)).h(px(1.0)).flex_none());

        let mut header = div()
            .id("right-panel-header")
            .h(px(48.0))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.0))
            .pl(px(10.0))
            .pr(px(14.0))
            .child(
                div()
                    .relative()
                    .h_full()
                    .min_w_0()
                    .flex_1()
                    .overflow_hidden()
                    .child(tabs)
                    .when_some(self.right_panel_pending_tab_reveal, |element, tab_index| {
                        element.child(tab_scroll_reveal_guard(
                            self.right_panel_tabs_scroll_handle.clone(),
                            tab_index,
                            cx.entity().downgrade(),
                        ))
                    })
                    .child(tab_scroll_fade(
                        self.right_panel_tabs_scroll_handle.clone(),
                        TabScrollFadeSide::Left,
                        theme.surface,
                    ))
                    .child(tab_scroll_fade(
                        self.right_panel_tabs_scroll_handle.clone(),
                        TabScrollFadeSide::Right,
                        theme.surface,
                    )),
            );

        if !self.right_panel_surfaces.is_empty() {
            let weak = cx.entity().downgrade();
            header = header.child(
                div()
                    .flex_none()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .child(
                        icon_button("add-right-panel-surface", "icons/plus.svg", theme)
                            .tooltip(|window, cx| {
                                Tooltip::new(tr!("right_panel.terminal_description"))
                                    .build(window, cx)
                            })
                            .on_click(move |_, _, cx| {
                                let _ = weak.update(cx, |this, cx| {
                                    this.open_right_panel_surface(
                                        RightPanelSurface::new_terminal(),
                                        cx,
                                    );
                                });
                            }),
                    ),
            );
        }

        self.window_drag_region(
            header.child(self.render_right_panel_toggle(cx)).children(
                self.render_client_window_controls(
                    super::window_chrome::WindowControlSide::Right,
                    window,
                    cx,
                ),
            ),
            cx,
        )
    }

    fn render_right_panel_empty_message(
        &self,
        title: String,
        description: String,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = Theme::current(cx);
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .pb(px(32.0))
            .child(
                div()
                    .text_size(sp(13.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(title),
            )
            .child(
                div()
                    .mt(px(6.0))
                    .max_w(px(300.0))
                    .text_center()
                    .text_size(sp(12.5))
                    .line_height(sp(17.0))
                    .text_color(theme.text_tertiary)
                    .child(description),
            )
    }
}
