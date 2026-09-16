//! Pure client-side composer matching over daemon-provided command/file lists.

use std::ops::Range;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32Str};
pub use waku_protocol::composer::{CommandScope, FileEntry, SlashCommand};
use waku_protocol::model::{ProviderKind, ProviderModelOption, ReportedCommand};

pub const FILTER_CAP: usize = 64;
pub const FILE_INDEX_CAP: usize = 50_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TriggerKind {
    Command,
    File,
    Skill,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Trigger {
    pub kind: TriggerKind,
    pub query: String,
    pub range: Range<usize>,
}

pub fn detect_trigger(text: &str, cursor: usize) -> Option<Trigger> {
    let cursor = cursor.min(text.len());
    if !text.is_char_boundary(cursor) {
        return None;
    }
    let line_start = text[..cursor].rfind('\n').map_or(0, |index| index + 1);
    let line_prefix = &text[line_start..cursor];
    if let Some(query) = line_prefix.strip_prefix('/') {
        if !query.chars().any(char::is_whitespace) {
            return Some(Trigger {
                kind: TriggerKind::Command,
                query: query.to_owned(),
                range: line_start..cursor,
            });
        }
        return None;
    }
    let token_start = text[..cursor]
        .rfind(char::is_whitespace)
        .map_or(0, |index| {
            index + text[index..].chars().next().unwrap().len_utf8()
        });
    let token = &text[token_start..cursor];
    if let Some(rest) = token.strip_prefix('$') {
        let query = rest.strip_prefix('(').unwrap_or(rest);
        let query = query.strip_suffix(')').unwrap_or(query);
        if !query.chars().any(char::is_whitespace) {
            return Some(Trigger {
                kind: TriggerKind::Skill,
                query: query.to_owned(),
                range: token_start..cursor,
            });
        }
        return None;
    }
    Some(Trigger {
        kind: TriggerKind::File,
        query: token.strip_prefix('@')?.to_owned(),
        range: token_start..cursor,
    })
}

pub fn merge_reported_commands(
    discovered: &[SlashCommand],
    reported: &[ReportedCommand],
) -> Vec<SlashCommand> {
    let mut merged = discovered.to_vec();
    for report in reported {
        if let Some(known) = merged
            .iter_mut()
            .find(|command| command.name == report.name)
        {
            if known.description.is_empty() {
                known.description = report.description.clone();
            }
        } else {
            merged.push(SlashCommand {
                name: report.name.clone(),
                description: report.description.clone(),
                scope: CommandScope::Builtin,
                argument_hint: None,
                template: None,
            });
        }
    }
    merged
        .sort_by(|a, b| (a.scope.display_rank(), &a.name).cmp(&(b.scope.display_rank(), &b.name)));
    merged
}

/// Build the slash-prefixed text shown for an autocomplete command.
pub fn command_composer_text(command: &SlashCommand) -> String {
    format!("/{}", command.name)
}

/// Build the `$(name)` text inserted for an autocomplete skill.
pub fn skill_composer_text(command: &SlashCommand) -> String {
    format!("$({})", command.name)
}

/// Parse a `$(name args)` skill invocation. Returns `(name, args)`.
pub fn parse_skill_invocation(prompt: &str) -> Option<(&str, &str)> {
    let rest = prompt.strip_prefix("$(")?;
    let end = rest.find(')')?;
    let name = &rest[..end];
    if name.is_empty() || name.chars().any(char::is_whitespace) {
        return None;
    }
    let args = rest[end + 1..].trim();
    Some((name, args))
}

/// Whether the composer submitted Waku's global terminal-session picker.
/// The command is reserved by daemon-side discovery, so it is intentionally
/// provider-neutral and never crosses into a provider transport.
pub fn is_resume_submission(prompt: &str) -> bool {
    prompt.trim() == "/resume"
}

/// Whether the submitted text resolves to ChatGPT's native fast-mode command,
/// which Waku bridges to the provider's service-tier control. Checking the
/// resolved entry preserves project/user command precedence when one of them
/// intentionally owns `/fast`.
pub fn is_fast_mode_toggle_submission(
    provider: ProviderKind,
    prompt: &str,
    commands: &[SlashCommand],
) -> bool {
    provider == ProviderKind::ChatGpt
        && prompt.trim() == "/fast"
        && commands.iter().any(|command| {
            command.name == "fast"
                && command.scope == CommandScope::Builtin
                && command.template.is_none()
        })
}

/// Resolve the next concrete service-tier ID for Codex's Fast toggle. Model
/// metadata may expose the Fast tier as `fast` or as `priority`; the display
/// label is the stable product vocabulary, while the ID is provider-owned.
pub fn toggled_fast_service_tier(
    current: Option<&str>,
    service_tiers: &[ProviderModelOption],
) -> Option<String> {
    let fast = service_tiers.iter().find(|tier| {
        matches!(tier.id.as_str(), "fast" | "priority") || tier.label.eq_ignore_ascii_case("fast")
    })?;
    Some(if current == Some(fast.id.as_str()) {
        "default".to_owned()
    } else {
        fast.id.clone()
    })
}

/// A `/goal` composer submission parsed into its intent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalCommand {
    /// Bare `/goal` — show the current goal (and offer to create one).
    Show,
    Edit,
    Pause,
    Resume,
    Clear,
    /// `/goal <objective>` — start or replace the goal with this objective.
    Set(String),
}

/// Parse the submitted text as ChatGPT's native `/goal` command, which Waku
/// bridges to `thread/goal/*`. `None` when it is not one — wrong provider,
/// other text, or a project/user command that deliberately owns `/goal`
/// (resolution precedence stands).
pub fn parse_goal_submission(
    provider: ProviderKind,
    prompt: &str,
    commands: &[SlashCommand],
) -> Option<GoalCommand> {
    if provider != ProviderKind::ChatGpt {
        return None;
    }
    let invocation = prompt.trim().strip_prefix('/')?;
    let (name, arguments) = invocation
        .split_once(char::is_whitespace)
        .map_or((invocation, ""), |(name, arguments)| {
            (name, arguments.trim())
        });
    if name != "goal" {
        return None;
    }
    let goal_is_codex_builtin = commands.iter().any(|command| {
        command.name == "goal"
            && command.scope == CommandScope::Builtin
            && command.template.is_none()
    });
    if !goal_is_codex_builtin {
        return None;
    }
    Some(match arguments {
        "" => GoalCommand::Show,
        "edit" => GoalCommand::Edit,
        "pause" => GoalCommand::Pause,
        "resume" => GoalCommand::Resume,
        "clear" => GoalCommand::Clear,
        objective => GoalCommand::Set(objective.to_owned()),
    })
}

pub fn expand_command_template(template: &str, args: &str) -> String {
    let positional = args.split_whitespace().collect::<Vec<_>>();
    let mut expanded = String::with_capacity(template.len() + args.len());
    let mut consumed_args = false;
    let mut rest = template;
    while let Some(index) = rest.find('$') {
        expanded.push_str(&rest[..index]);
        let after = &rest[index + 1..];
        if let Some(tail) = after.strip_prefix("ARGUMENTS") {
            expanded.push_str(args);
            consumed_args = true;
            rest = tail;
        } else if let Some(tail) = after.strip_prefix('@') {
            expanded.push_str(args);
            consumed_args = true;
            rest = tail;
        } else if let Some(digit) = after
            .chars()
            .next()
            .and_then(|character| character.to_digit(10))
            .filter(|digit| (1..=9).contains(digit))
        {
            if let Some(argument) = positional.get(digit as usize - 1) {
                expanded.push_str(argument);
            }
            consumed_args = true;
            rest = &after[1..];
        } else {
            expanded.push('$');
            rest = after;
        }
    }
    expanded.push_str(rest);
    if !consumed_args && !args.is_empty() {
        expanded.push_str("\n\n");
        expanded.push_str(args);
    }
    expanded
}

/// Resolve composer text into the exact prompt expected by the provider.
///
/// Template commands expand to their body. Skills keep a slash in the
/// composer and transcript, then resolve to each provider's native syntax at
/// the transport boundary.
pub fn resolved_submission(
    provider: ProviderKind,
    prompt: &str,
    commands: &[SlashCommand],
) -> Option<String> {
    if let Some(skill) = resolved_skill_submission(provider, prompt, commands) {
        return Some(skill);
    }
    let invocation = prompt.strip_prefix('/')?;
    let (name, args) = invocation
        .split_once(char::is_whitespace)
        .map_or((invocation, ""), |(name, args)| (name, args.trim()));
    let command = commands.iter().find(|command| command.name == name)?;
    let template = command.template.as_deref()?;
    Some(expand_command_template(template, args))
}

/// Resolve only provider-native skill syntax, without expanding templates.
///
/// Accepts both `/name args` and `$(name) args` forms; the latter is what the
/// `$` picker inserts.
pub fn resolved_skill_submission(
    provider: ProviderKind,
    prompt: &str,
    commands: &[SlashCommand],
) -> Option<String> {
    if provider != ProviderKind::ChatGpt {
        return None;
    }
    let invocation = if let Some((name, args)) = parse_skill_invocation(prompt.trim()) {
        if args.is_empty() {
            name.to_owned()
        } else {
            format!("{name} {args}")
        }
    } else {
        prompt.strip_prefix('/')?.to_owned()
    };
    let name = invocation
        .split_once(char::is_whitespace)
        .map_or(invocation.as_str(), |(name, _)| name);
    if !commands
        .iter()
        .any(|command| command.name == name && command.scope == CommandScope::Skill)
    {
        return None;
    }
    Some(match provider {
        ProviderKind::ChatGpt => format!("${invocation}"),
    })
}

pub fn matcher() -> Matcher {
    Matcher::new(nucleo_matcher::Config::DEFAULT.match_paths())
}

#[derive(Clone, Debug)]
pub struct Scored<T> {
    pub item: T,
    pub positions: Vec<u32>,
}

fn filter_scored(
    haystack: &[&str],
    query: &str,
    matcher: &mut Matcher,
    cap: usize,
) -> Vec<(usize, Vec<u32>)> {
    if query.trim().is_empty() {
        return (0..haystack.len().min(cap))
            .map(|index| (index, Vec::new()))
            .collect();
    }
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut buf = Vec::new();
    let mut scored = Vec::new();
    for (index, text) in haystack.iter().enumerate() {
        if let Some(score) = pattern.score(Utf32Str::new(text, &mut buf), matcher) {
            scored.push((score, index));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.truncate(cap);
    scored
        .into_iter()
        .map(|(_, index)| {
            let mut positions = Vec::new();
            pattern.indices(
                Utf32Str::new(haystack[index], &mut buf),
                matcher,
                &mut positions,
            );
            positions.sort_unstable();
            positions.dedup();
            (index, positions)
        })
        .collect()
}

pub fn filter_commands(
    commands: &[SlashCommand],
    query: &str,
    matcher: &mut Matcher,
) -> Vec<Scored<SlashCommand>> {
    let commands = commands
        .iter()
        .filter(|command| command.scope != CommandScope::Skill)
        .collect::<Vec<_>>();
    let names = commands
        .iter()
        .map(|command| command.name.as_str())
        .collect::<Vec<_>>();
    filter_scored(&names, query, matcher, FILTER_CAP)
        .into_iter()
        .map(|(index, positions)| Scored {
            item: (*commands[index]).clone(),
            positions,
        })
        .collect()
}

/// Fuzzy-filter skills only (commands with `Skill` scope) for the `$` picker.
pub fn filter_skills(
    commands: &[SlashCommand],
    query: &str,
    matcher: &mut Matcher,
) -> Vec<Scored<SlashCommand>> {
    let skills = commands
        .iter()
        .filter(|command| command.scope == CommandScope::Skill)
        .collect::<Vec<_>>();
    let names = skills
        .iter()
        .map(|command| command.name.as_str())
        .collect::<Vec<_>>();
    filter_scored(&names, query, matcher, FILTER_CAP)
        .into_iter()
        .map(|(index, positions)| Scored {
            item: (*skills[index]).clone(),
            positions,
        })
        .collect()
}

pub fn filter_files(
    files: &[FileEntry],
    query: &str,
    matcher: &mut Matcher,
) -> Vec<Scored<FileEntry>> {
    let paths = files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<Vec<_>>();
    filter_scored(&paths, query, matcher, FILTER_CAP)
        .into_iter()
        .map(|(index, positions)| Scored {
            item: files[index].clone(),
            positions,
        })
        .collect()
}

pub fn highlight_byte_ranges(
    text: &str,
    positions: &[u32],
    char_offset: usize,
) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for (char_index, (byte_index, character)) in (char_offset..).zip(text.char_indices()) {
        if positions.binary_search(&(char_index as u32)).is_ok() {
            let byte_end = byte_index + character.len_utf8();
            match ranges.last_mut() {
                Some(last) if last.end == byte_index => last.end = byte_end,
                _ => ranges.push(byte_index..byte_end),
            }
        }
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merged_command_picker_puts_builtins_first_and_skills_last() {
        let discovered = vec![
            command("deploy", CommandScope::Skill),
            command("format", CommandScope::User),
            command("lint", CommandScope::Project),
            command("review", CommandScope::Builtin),
            command("resume", CommandScope::Waku),
        ];
        let reported = vec![ReportedCommand {
            name: "compact".into(),
            description: "Free up context".into(),
        }];

        let merged = merge_reported_commands(&discovered, &reported);
        assert_eq!(
            merged
                .iter()
                .map(|command| (command.scope, command.name.as_str()))
                .collect::<Vec<_>>(),
            [
                (CommandScope::Builtin, "compact"),
                (CommandScope::Waku, "resume"),
                (CommandScope::Builtin, "review"),
                (CommandScope::Project, "lint"),
                (CommandScope::User, "format"),
                (CommandScope::Skill, "deploy"),
            ]
        );
    }

    #[test]
    fn fast_toggle_is_codex_only_and_respects_command_overrides() {
        let builtin = command("fast", CommandScope::Builtin);
        assert!(is_fast_mode_toggle_submission(
            ProviderKind::ChatGpt,
            "/fast ",
            std::slice::from_ref(&builtin),
        ));
        assert!(!is_fast_mode_toggle_submission(
            ProviderKind::ChatGpt,
            "/fast now",
            std::slice::from_ref(&builtin),
        ));
        assert!(!is_fast_mode_toggle_submission(
            ProviderKind::ChatGpt,
            "/fast",
            &[command("fast", CommandScope::Project)],
        ));
    }

    #[test]
    fn resume_is_an_exact_provider_neutral_local_command() {
        assert!(is_resume_submission("/resume"));
        assert!(is_resume_submission("  /resume  "));
        assert!(!is_resume_submission("/resume latest"));
        assert!(!is_resume_submission("please /resume"));
    }

    #[test]
    fn fast_toggle_uses_the_models_concrete_service_tier_id() {
        let tiers = [ProviderModelOption::new("priority", "Fast")];
        assert_eq!(
            toggled_fast_service_tier(Some("default"), &tiers).as_deref(),
            Some("priority")
        );
        assert_eq!(
            toggled_fast_service_tier(Some("priority"), &tiers).as_deref(),
            Some("default")
        );
        assert_eq!(toggled_fast_service_tier(None, &[]), None);
    }

    #[test]
    fn codex_skill_completion_keeps_slash_in_the_composer() {
        let skill = command("mattpocock-skills:to-spec", CommandScope::Skill);
        assert_eq!(command_composer_text(&skill), "/mattpocock-skills:to-spec");
        assert_eq!(
            command_composer_text(&command("fast", CommandScope::Builtin)),
            "/fast"
        );
    }

    #[test]
    fn codex_skill_submission_uses_the_catalog_invocation() {
        let skill = command("mattpocock-skills:to-spec", CommandScope::Skill);
        assert_eq!(
            resolved_submission(
                ProviderKind::ChatGpt,
                "/mattpocock-skills:to-spec carefully",
                std::slice::from_ref(&skill)
            )
            .as_deref(),
            Some("$mattpocock-skills:to-spec carefully")
        );
    }

    #[test]
    fn chatgpt_skill_submission_uses_dollar_invocation() {
        let skill = command("deploy", CommandScope::Skill);
        assert_eq!(
            resolved_submission(
                ProviderKind::ChatGpt,
                "/deploy production",
                std::slice::from_ref(&skill)
            )
            .as_deref(),
            Some("$deploy production")
        );
    }

    fn command(name: &str, scope: CommandScope) -> SlashCommand {
        SlashCommand {
            name: name.into(),
            description: String::new(),
            scope,
            argument_hint: None,
            template: None,
        }
    }

    #[test]
    fn goal_submissions_parse_into_their_intent() {
        let builtin = command("goal", CommandScope::Builtin);
        let commands = std::slice::from_ref(&builtin);
        let parse = |prompt: &str| parse_goal_submission(ProviderKind::ChatGpt, prompt, commands);

        assert_eq!(parse("/goal"), Some(GoalCommand::Show));
        assert_eq!(parse("/goal "), Some(GoalCommand::Show));
        assert_eq!(parse("/goal edit"), Some(GoalCommand::Edit));
        assert_eq!(parse("/goal pause"), Some(GoalCommand::Pause));
        assert_eq!(parse("/goal resume"), Some(GoalCommand::Resume));
        assert_eq!(parse("/goal clear"), Some(GoalCommand::Clear));
        assert_eq!(
            parse("/goal improve benchmark coverage"),
            Some(GoalCommand::Set("improve benchmark coverage".into()))
        );
        assert_eq!(parse("/goals"), None);
        assert_eq!(parse("ship /goal"), None);
    }

    #[test]
    fn dollar_trigger_opens_the_skill_picker_mid_prompt() {
        let trigger = detect_trigger("use $cloud", 11).expect("skill trigger");
        assert_eq!(trigger.kind, TriggerKind::Skill);
        assert_eq!(trigger.query, "cloud");
        assert_eq!(trigger.range, 4..11);

        let trigger = detect_trigger("use $(cloud", 12).expect("parens trigger");
        assert_eq!(trigger.kind, TriggerKind::Skill);
        assert_eq!(trigger.query, "cloud");

        // `$` mid-sentence still completes; whitespace ends the token.
        assert!(detect_trigger("a $ b", 4).is_none());
        // `@` mentions keep working.
        let trigger = detect_trigger("see @src/", 9).expect("file trigger");
        assert_eq!(trigger.kind, TriggerKind::File);
    }

    #[test]
    fn skill_picker_filters_to_skills_and_inserts_dollar_form() {
        let skill = command("deploy", CommandScope::Skill);
        let project = command("deploy", CommandScope::Project);
        let commands = [skill.clone(), project];
        let mut matcher = matcher();
        let rows = filter_skills(&commands, "dep", &mut matcher);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].item.name, "deploy");
        assert_eq!(skill_composer_text(&skill), "$(deploy)");
        assert_eq!(
            parse_skill_invocation("$(deploy production)"),
            Some(("deploy", "production"))
        );
    }

    #[test]
    fn dollar_skill_submission_resolves_like_slash() {
        let skill = command("deploy", CommandScope::Skill);
        let commands = std::slice::from_ref(&skill);
        assert_eq!(
            resolved_submission(ProviderKind::ChatGpt, "$(deploy production)", commands).as_deref(),
            Some("$deploy production")
        );
    }

    #[test]
    fn goal_command_is_chatgpt_only_and_respects_overrides() {
        let builtin = command("goal", CommandScope::Builtin);
        assert_eq!(
            parse_goal_submission(
                ProviderKind::ChatGpt,
                "/goal",
                std::slice::from_ref(&builtin)
            ),
            Some(GoalCommand::Show)
        );
        // A project command deliberately owning /goal wins the collision.
        let mut project = command("goal", CommandScope::Project);
        project.template = Some("do project things".into());
        assert_eq!(
            parse_goal_submission(
                ProviderKind::ChatGpt,
                "/goal",
                std::slice::from_ref(&project)
            ),
            None
        );
    }
}
