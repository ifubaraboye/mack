//! The settings Usage page: lifetime token totals for turns executed inside
//! Mack, summed over every stored session by the daemon. Frames read only
//! the snapshot stored on the entity.

use super::*;

/// A snapshot older than this refetches when the page is next opened.
const USAGE_RESCAN_AFTER: Duration = Duration::from_secs(120);

impl Mack {
    /// Switch the settings view to `page`, warming the Usage totals when
    /// that is where the user is heading.
    pub(super) fn open_settings_page(&mut self, page: SettingsPage, cx: &mut Context<Self>) {
        // Secrets are revealed only for the current visit to the page. This
        // also masks the token again when the Daemon row is reselected.
        self.daemon_token_revealed = false;
        self.settings_page = Some(page);
        // Each page starts at its own top; a scroll position carried over
        // from the previous page would land mid-content.
        self.settings_scroll.set_offset(gpui::Point::default());
        if page == SettingsPage::Usage {
            self.ensure_mack_usage(false, cx);
        }
        if page == SettingsPage::Skills {
            self.ensure_skills_catalog(false, cx);
        }
        if page == SettingsPage::Memory {
            self.ensure_memory_list(false, cx);
        }
        cx.notify();
    }

    /// Fetch Mack-native lifetime totals unless a fresh-enough snapshot (or
    /// an in-flight fetch) already covers them. `force` is the refresh
    /// button. Results from superseded fetches are discarded by generation.
    pub(super) fn ensure_mack_usage(&mut self, force: bool, cx: &mut Context<Self>) {
        let satisfied = self.mack_usage_totals.is_some()
            && self
                .mack_usage_fetched_at
                .is_some_and(|fetched| fetched.elapsed() < USAGE_RESCAN_AFTER);
        if self.mack_usage_pending {
            return;
        }
        if !force && satisfied {
            return;
        }
        self.mack_usage_pending = true;
        self.mack_usage_generation += 1;
        let generation = self.mack_usage_generation;
        let daemon = self.daemon.client();
        cx.spawn(async move |this, cx| {
            let totals = cx
                .background_executor()
                .spawn(async move {
                    match daemon.request(
                        Uuid::nil(),
                        Uuid::nil(),
                        waku_client::Command::LoadMackUsageTotals,
                    )? {
                        waku_client::ResponsePayload::MackUsageTotals { totals } => Ok(totals),
                        _ => anyhow::bail!("the daemon returned an invalid usage response"),
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.mack_usage_generation != generation {
                    return;
                }
                this.mack_usage_pending = false;
                match totals {
                    Ok(totals) => {
                        this.mack_usage_fetched_at = Some(Instant::now());
                        this.mack_usage_totals = Some(totals);
                    }
                    Err(error) => this.show_toast(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn render_usage_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        div()
            .flex()
            .flex_col()
            .pb(px(16.0))
            .child(self.render_usage_header(&theme, cx))
            .child(self.render_mack_usage(&theme))
            .into_any_element()
    }

    /// The refresh control, right-aligned. The page reports a single
    /// lifetime total, so there are no view, window, or metric selectors.
    fn render_usage_header(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let pending = self.mack_usage_pending;
        let refresh_glyph: AnyElement = if pending {
            motion::spin(icon("icons/loader-circle.svg", 12.0, theme.text_tertiary))
        } else {
            icon("icons/rotate-cw.svg", 12.0, theme.text_tertiary).into_any_element()
        };
        let refresh = div()
            .id("usage-refresh")
            .tab_index(0)
            .focus_visible(|style| style.border_color(theme.accent))
            .h(px(28.0))
            .px(px(8.0))
            .rounded(px(7.0))
            .border_1()
            .border_color(theme.border_strong)
            .flex()
            .items_center()
            .cursor_default()
            .hover(|element| element.bg(theme.overlay))
            .tooltip(Tooltip::text(if pending {
                tr!("usage.mack_loading")
            } else {
                tr!("usage.rescan")
            }))
            .child(refresh_glyph)
            .on_click(cx.listener(|this, _, _, cx| {
                this.ensure_mack_usage(true, cx);
            }));

        div()
            .mt(px(6.0))
            .flex()
            .items_center()
            .justify_end()
            .child(refresh)
    }

    /// Lifetime token totals for turns executed inside Mack. The fetch runs
    /// once per page visit; turns that settle while the page is open bump
    /// the snapshot inline (see the `TurnUsage` handler), so the section
    /// stays current without refetching. A missing snapshot means the fetch
    /// is still in flight and degrades to a named loading row.
    fn render_mack_usage(&self, theme: &Theme) -> Div {
        let (headline, caption, tiles) = match &self.mack_usage_totals {
            Some(mack) => {
                let observed_input = mack.totals.uncached_input + mack.totals.cached_input;
                let cached_share = if observed_input == 0 {
                    0.0
                } else {
                    mack.totals.cached_input as f64 / observed_input as f64
                };
                let tiles = vec![
                    (
                        tr!("usage.cached_input"),
                        format_tokens_compact(mack.totals.cached_input as f64),
                        tr!(
                            "usage.observed_input_share",
                            share = format_percent(cached_share)
                        ),
                    ),
                    (
                        tr!("usage.uncached_input"),
                        format_tokens_compact(mack.totals.uncached_input as f64),
                        tr!(
                            "usage.cache_writes",
                            count = format_tokens_compact(mack.totals.cache_creation as f64)
                        ),
                    ),
                    (
                        tr!("usage.output"),
                        format_tokens_compact(mack.totals.output as f64),
                        tr!(
                            "usage.includes_reasoning",
                            count = format_tokens_compact(mack.totals.reasoning as f64)
                        ),
                    ),
                ];
                (
                    format_tokens_compact(mack.total_tokens() as f64),
                    tr!(
                        "usage.mack_turns_chats",
                        turns = format_count(mack.turns),
                        chats = format_count(mack.sessions)
                    ),
                    tiles,
                )
            }
            None => ("—".to_owned(), tr!("usage.mack_loading"), Vec::new()),
        };
        let mut section = div().mt(px(18.0)).flex().flex_col().gap(px(10.0)).child(
            div()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .child(
                    div()
                        .text_size(sp(12.5))
                        .text_color(theme.text_tertiary)
                        .child(tr!("usage.mack_tokens_upper")),
                )
                .child(
                    div()
                        .text_size(sp(30.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(SharedString::from(headline)),
                )
                .child(
                    div()
                        .text_size(sp(12.5))
                        .text_color(theme.text_tertiary)
                        .child(SharedString::from(caption)),
                ),
        );
        if !tiles.is_empty() {
            section = section.child(usage_tile_strip(tiles, theme));
        }
        section
    }
}

/// One row of label/value/detail tiles in the Usage page's strip idiom.
fn usage_tile_strip(
    tiles: impl IntoIterator<Item = (String, String, String)>,
    theme: &Theme,
) -> Div {
    let mut strip = div()
        .border_t_1()
        .border_b_1()
        .border_color(theme.border)
        .flex();
    for (index, (label, value, detail)) in tiles.into_iter().enumerate() {
        strip = strip.child(
            div()
                .flex_1()
                .min_w_0()
                .px(px(14.0))
                .py(px(11.0))
                .when(index > 0, |element| {
                    element.border_l_1().border_color(theme.border)
                })
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .text_size(sp(12.5))
                        .text_color(theme.text_tertiary)
                        .truncate()
                        .child(label),
                )
                .child(
                    div()
                        .text_size(sp(15.0))
                        .text_color(theme.text)
                        .truncate()
                        .child(SharedString::from(value)),
                )
                .child(
                    div()
                        .text_size(sp(12.5))
                        .text_color(theme.text_tertiary)
                        .truncate()
                        .child(SharedString::from(detail)),
                ),
        );
    }
    strip
}

/* ------------------------------------------------------------------------- */
/* Formatting                                                                */
/* ------------------------------------------------------------------------- */

fn group_thousands(number: &str) -> String {
    let (integer, fraction) = match number.split_once('.') {
        Some((integer, fraction)) => (integer, Some(fraction)),
        None => (number, None),
    };
    let grouped = integer
        .as_bytes()
        .rchunks(3)
        .rev()
        .map(|chunk| std::str::from_utf8(chunk).unwrap_or_default())
        .collect::<Vec<_>>()
        .join(",");
    match fraction {
        Some(fraction) => format!("{grouped}.{fraction}"),
        None => grouped,
    }
}

fn format_count(value: u64) -> String {
    group_thousands(&value.to_string())
}

/// Compacts a token count to three significant figures with a unit suffix, so
/// columns of numbers line up at a glance (`19.9B`, `76.7M`, `804K`).
fn format_tokens_compact(value: f64) -> String {
    let abs = value.abs();
    let (scaled, suffix) = if abs >= 1e12 {
        (value / 1e12, "T")
    } else if abs >= 1e9 {
        (value / 1e9, "B")
    } else if abs >= 1e6 {
        (value / 1e6, "M")
    } else if abs >= 1e3 {
        (value / 1e3, "K")
    } else {
        return format_count(value.round().max(0.0) as u64);
    };
    let digits = if scaled.abs() >= 100.0 {
        0
    } else if scaled.abs() >= 10.0 {
        1
    } else {
        2
    };
    let mut text = format!("{scaled:.digits$}");
    // Trim an all-zero fraction ("1.00" → "1") but keep "1.50".
    if let Some(dot) = text.find('.')
        && text[dot + 1..].bytes().all(|byte| byte == b'0')
    {
        text.truncate(dot);
    }
    format!("{text}{suffix}")
}

fn format_percent(share: f64) -> String {
    format!("{:.1}%", share * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_counts_compact_to_three_significant_figures() {
        assert_eq!(format_tokens_compact(804_000.0), "804K");
        assert_eq!(format_tokens_compact(76_700_000.0), "76.7M");
        assert_eq!(format_tokens_compact(19_900_000_000.0), "19.9B");
        assert_eq!(format_tokens_compact(950.0), "950");
        assert_eq!(format_tokens_compact(1_000.0), "1K");
        assert_eq!(format_tokens_compact(1_500.0), "1.50K");
    }
}
