use super::{CodexInput, Failure, Handle, Result, Screen};
use serde_json::json;
use std::thread::sleep;
use std::time::{Duration, Instant};

fn notices(screen: &str) -> impl Iterator<Item = String> + '_ {
    let lines: Vec<_> = screen.lines().map(str::trim).collect();
    (0..lines.len()).filter_map(move |i| {
        if lines[i].is_empty() {
            return None;
        }
        let joined = lines[i..(i + 4).min(lines.len())].join(" ");
        Some(
            joined
                .trim_start_matches(['■', '•'])
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
        )
    })
}

pub(super) fn read_only(text: &str) -> bool {
    notices(text).any(|s| s.starts_with("This conversation is unavailable. Its cached transcript and draft remain here; input is paused.") ||
        (s.starts_with("This chat uses ") && s.contains(", but Codex now uses ") && s.contains(", so this chat is read-only.")))
}

fn refusals(text: &str) -> usize {
    notices(text)
        .filter(|s| {
            s.starts_with("This conversation is read-only or unavailable; no operation was sent.")
        })
        .count()
}

fn compact(text: &str) -> String {
    text.split_whitespace().collect()
}

struct Composer<'a> {
    // A visible scroll marker identifies an occupied but incomplete composer.
    draft: Option<String>,
    // None when only one of two plausible footer boundaries yields this draft.
    footer: Option<Vec<&'a str>>,
}

fn shows_placeholder(lines: &[&str]) -> bool {
    let Some((prompt, continuation)) = lines.split_first() else {
        return false;
    };
    let Some(first) = prompt
        .strip_prefix(['›', '»'])
        .and_then(|body| body.strip_prefix(' '))
    else {
        return false;
    };
    // Remove only the composer prefix and the indentation of displayed
    // continuation lines. Join a word wrap with one space.
    let text = std::iter::once(first)
        .chain(
            continuation
                .iter()
                .map(|line| line.strip_prefix("  ").unwrap_or(line)),
        )
        .collect::<Vec<_>>()
        .join(" ");
    text == "Ask Codex to do anything"
}

fn visible_draft(lines: &[&str]) -> Option<String> {
    if lines.iter().any(|s| s.ends_with(['↑', '↓'])) {
        return None;
    }
    let body = lines.join(" ");
    Some(body.strip_prefix(['›', '»'])?.trim().to_owned())
}

fn complete_draft(lines: &[&str]) -> Option<String> {
    let draft = visible_draft(lines)?;
    Some(if shows_placeholder(lines) {
        String::new()
    } else {
        draft
    })
}

fn screen_lines(text: &str) -> Vec<&str> {
    // Herdr can truncate old scrollback even when the entire bottom composer is visible.
    text.trim_end()
        .lines()
        .map(|line| line.trim_end_matches(' '))
        .collect()
}

fn prompt_row(lines: &[&str]) -> Option<usize> {
    lines.iter().rposition(|s| {
        s.strip_prefix(['›', '»'])
            .is_some_and(|body| body.is_empty() || body.starts_with(' '))
    })
}

fn codex_footer(line: &str) -> bool {
    let Some(label) = line.strip_prefix("  ") else {
        return false;
    };
    let label = label.trim_start_matches(' ');
    if label.contains(" · ") {
        return true;
    }
    let Some(context) = label.strip_suffix("% context left") else {
        return false;
    };
    let percent = match context.split_once("  ") {
        Some(("? for shortcuts", right)) => right.trim_start_matches(' '),
        Some(_) => return false,
        None => context,
    };
    percent.bytes().all(|b| b.is_ascii_digit())
        && percent.parse::<u8>().is_ok_and(|value| value <= 100)
}

fn composer_end(tail: &[&str]) -> (usize, usize) {
    let end = 1 + tail[1..].iter().take_while(|s| s.starts_with("  ")).count();
    let adjacent = if end == tail.len() {
        tail[1..]
            .iter()
            .rev()
            .take(2)
            .take_while(|s| codex_footer(s))
            .count()
    } else {
        0
    };
    (end, adjacent)
}

fn footer_lines<'a>(lines: &[&'a str]) -> Option<Vec<&'a str>> {
    let footer: Vec<_> = lines.iter().copied().filter(|s| !s.is_empty()).collect();
    ((1..=2).contains(&footer.len()) && footer.iter().all(|s| codex_footer(s))).then_some(footer)
}

fn codex_box<'a>(
    screen: &'a Screen,
    prior_footer: &[&str],
    expected: Option<&str>,
) -> Option<Composer<'a>> {
    let lines = screen_lines(&screen.text);
    let start = prompt_row(&lines)?;
    let (end, adjacent) = composer_end(&lines[start..]);
    let mut end = start + end;
    if adjacent > 0 {
        // Wrapped draft lines can also contain the footer separator. If both
        // footer lengths fit, use the footer seen independently before typing.
        end -= match adjacent {
            2 if !prior_footer.is_empty() && lines.ends_with(prior_footer) => prior_footer.len(),
            2 => {
                let longer = complete_draft(&lines[start..end - 1]);
                let shorter = complete_draft(&lines[start..end - 2]);
                let scrolled = longer.is_none() || shorter.is_none();
                let draft = match expected {
                    Some(expected) => [longer, shorter]
                        .into_iter()
                        .flatten()
                        .find(|draft| !draft.is_empty() && compact(draft) == expected),
                    // Before typing, protect any visible text that may belong
                    // to the composer. A scroll marker still means occupied.
                    None if scrolled => None,
                    None => longer,
                };
                return (scrolled || draft.is_some()).then_some(Composer {
                    draft,
                    footer: None,
                });
            }
            count => count,
        };
    }
    Some(Composer {
        draft: complete_draft(&lines[start..end]),
        footer: Some(footer_lines(&lines[end..])?),
    })
}

fn visible_text(rows: &[ansi::Row]) -> String {
    rows.iter()
        .map(|row| row.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn matching_styles(
    plain: &str,
    tail: &[&str],
    rows: Option<Vec<ansi::Row>>,
) -> Option<(Vec<ansi::Row>, usize)> {
    let mut rows = rows?;
    while rows.last().is_some_and(|row| row.text.trim().is_empty()) {
        rows.pop();
    }
    let visible = visible_text(&rows);
    if read_only(&visible) || refusals(&visible) > refusals(plain) {
        return None;
    }
    let first = rows.len().checked_sub(tail.len())?;
    rows[first..]
        .iter()
        .zip(tail)
        .all(|(row, &line)| row.text.trim_end_matches(' ') == line)
        .then_some((rows, first))
}

pub enum ReplyBox {
    Empty,
    Draft(Option<String>),
    Unproven(&'static str),
}

fn attached_image(line: &str) -> bool {
    let Some(number) = line
        .strip_prefix("  [Image #")
        .and_then(|label| label.strip_suffix(']'))
    else {
        return false;
    };
    !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
}

fn observe_codex(
    plain: &str,
    read_styles: impl FnOnce() -> Result<Option<Vec<ansi::Row>>>,
) -> Result<ReplyBox> {
    let lines = screen_lines(plain);
    let unknown = ReplyBox::Unproven("Codex writable chat composer could not be verified");
    let Some(start) = prompt_row(&lines) else {
        return Ok(unknown);
    };
    let tail = &lines[start..];
    let (end, adjacent) = composer_end(tail);
    let end = end - adjacent;
    let Some(footer) = footer_lines(&tail[end..]) else {
        return Ok(unknown);
    };
    // Associate the preceding rows as well as the prompt. Codex puts remote
    // image labels above the textarea, separated by an optional blank row.
    let offset = start.saturating_sub(2);
    let Some((styled, first)) = matching_styles(plain, &lines[offset..], read_styles()?) else {
        return Ok(unknown);
    };
    let prompt = first + start - offset;
    let rows = &styled[prompt..];
    let body = &tail[..end];
    let draft = visible_draft(body);
    let bare = matches!(body, ["›"] | ["»"]);
    // Codex paints its editable rectangle separately from the footer. If
    // a footer-shaped row could be the draft, require that distinction.
    if ((adjacent == 2 || bare && (adjacent == 1 || footer.len() == 2))
        && !rows[0].prompt_background)
        || rows[end..].iter().any(|row| row.has_nondefault_background)
    {
        return Ok(ReplyBox::Unproven(
            "Codex composer already contains a draft or has an unverified footer boundary",
        ));
    }
    // Both enabled chat glyphs are bold. The disabled composer uses a dim ›.
    if !rows[0].active_prompt {
        return Ok(unknown);
    }
    for row in styled[..prompt].iter().rev().take(2) {
        let line = row.text.trim_end_matches(' ');
        if line.is_empty() {
            continue;
        }
        if attached_image(line) && (!rows[0].prompt_background || row.has_nondefault_background) {
            return Ok(ReplyBox::Draft(None));
        }
        break;
    }
    if bare
        || (shows_placeholder(body) && rows[..end].iter().all(|row| !row.has_nonplaceholder_body))
    {
        Ok(ReplyBox::Empty)
    } else {
        // Reflow and hard newlines cannot be distinguished in pane.read.
        let exact = match body {
            [line] if draft.is_some() => line
                .strip_prefix(['›', '»'])
                .and_then(|body| body.strip_prefix(' '))
                .map(str::to_owned),
            _ => None,
        };
        Ok(ReplyBox::Draft(exact))
    }
}

fn opencode_row<'a>(line: &'a str, indent: &str) -> Option<&'a str> {
    line.strip_prefix(indent)?
        .strip_prefix('┃')
        .map(|body| body.trim_matches(' '))
}

fn observe_opencode(
    plain: &str,
    read_styles: impl FnOnce() -> Result<Option<Vec<ansi::Row>>>,
) -> Result<ReplyBox> {
    let unknown = ReplyBox::Unproven("OpenCode writable chat composer could not be verified");
    let lines = screen_lines(plain);
    let [above @ .., bar, footer] = lines.as_slice() else {
        return Ok(unknown);
    };
    let bar_text = bar.trim_start_matches(' ');
    let indent = &bar[..bar.len() - bar_text.len()];
    let Some(rule) = bar_text.strip_prefix('╹') else {
        return Ok(unknown);
    };
    if rule.is_empty() || !rule.chars().all(|c| c == '▀') || !footer.ends_with("ctrl+p commands")
    {
        return Ok(unknown);
    }
    let Some((metadata, body)) = above.split_last() else {
        return Ok(unknown);
    };
    if !opencode_row(metadata, indent)
        .is_some_and(|line| line.contains(" · ") && !line.starts_with("Shell"))
    {
        return Ok(unknown);
    }
    let start = body
        .iter()
        .rposition(|line| opencode_row(line, indent).is_none())
        .map_or(0, |index| index + 1);
    let body = &body[start..];
    let Some((styles, first)) = matching_styles(plain, &lines[start..], read_styles()?) else {
        return Ok(unknown);
    };
    if !styles[first + body.len() + 1].visible_foreground {
        return Ok(unknown);
    }
    // In the default session prompt, an empty textarea is one row between
    // the top padding and the padding before agent/model metadata.
    match body.len() {
        3 if body
            .iter()
            .all(|line| opencode_row(line, indent).is_some_and(str::is_empty)) =>
        {
            Ok(ReplyBox::Empty)
        }
        3.. => Ok(ReplyBox::Draft(None)),
        _ => Ok(unknown),
    }
}

fn observe_claude(
    plain: &str,
    read_styles: impl FnOnce() -> Result<Option<Vec<ansi::Row>>>,
) -> Result<ReplyBox> {
    let unknown = ReplyBox::Unproven("Claude writable chat composer could not be verified");
    let lines = screen_lines(plain);
    let [.., top, prompt, bottom, footer] = lines.as_slice() else {
        return Ok(unknown);
    };
    let footer = footer.trim();
    let known_footer = matches!(
        footer,
        "? for shortcuts" | "⏵⏵ auto mode" | "⏵⏵ auto mode (shift+tab to cycle)"
    );
    // Claude's classic, full-width input row has horizontal rails. Other
    // layouts, modal selectors and shell input have no such certified shape.
    if !known_footer
        || top != bottom
        || top.len() < 36
        || !top.chars().all(|c| c == '─')
        || !(*prompt == "❯" || prompt.starts_with("❯ "))
    {
        return Ok(unknown);
    }
    let Some((styles, first)) = matching_styles(plain, &lines[lines.len() - 4..], read_styles()?)
    else {
        return Ok(unknown);
    };
    if !styles[first + 1].visible_claude_prompt {
        return Ok(unknown);
    }
    if *prompt == "❯" {
        Ok(ReplyBox::Empty)
    } else {
        Ok(ReplyBox::Draft(None))
    }
}

fn observe_reply_box(
    host: &str,
    plain: &str,
    read_styles: impl FnOnce() -> Result<Option<Vec<ansi::Row>>>,
) -> Result<ReplyBox> {
    match host {
        "codex" => observe_codex(plain, read_styles),
        "opencode" => observe_opencode(plain, read_styles),
        "claude" => observe_claude(plain, read_styles),
        _ => Ok(ReplyBox::Unproven(
            "writable chat composer could not be verified for this host",
        )),
    }
}

/// Classify a single styled terminal capture and return its visible text.
pub fn classify_reply_capture(host: &str, capture: &str) -> Option<(String, ReplyBox)> {
    let rows = ansi::rows(capture)?;
    let plain = visible_text(&rows);
    let observation = observe_reply_box(host, &plain, || Ok(Some(rows))).ok()?;
    Some((plain, observation))
}

fn observe_herdr_reply_box(handle: &Handle, screen: &Screen, host: &str) -> Result<ReplyBox> {
    // Herdr 0.9.5 always returns revision 0. The later read supplies styles
    // for a matching suffix on the bound pane, not a frame ID.
    observe_reply_box(host, &screen.text, || {
        Ok(ansi::rows(&handle.read_ansi()?.text))
    })
}

pub(super) fn require_empty_reply_box(handle: &Handle, screen: &Screen, host: &str) -> Result<()> {
    match observe_herdr_reply_box(handle, screen, host)? {
        ReplyBox::Empty => Ok(()),
        ReplyBox::Draft(_) => Err(Failure::unsent("chat composer already contains a draft")),
        ReplyBox::Unproven(diagnostic) => Err(Failure::unsent(diagnostic)),
    }
}

pub(super) fn confirm_reply(
    handle: &mut Handle,
    text: &str,
    before: &Screen,
    host: &str,
    save: &mut impl FnMut(&Handle) -> std::io::Result<()>,
) -> Result<()> {
    let sample = |handle: &Handle| {
        sleep(Duration::from_millis(300));
        let screen = handle.read()?;
        if read_only(&screen.text) || refusals(&screen.text) > refusals(&before.text) {
            return Err(Failure::unknown("worker refused or paused input"));
        }
        match observe_herdr_reply_box(handle, &screen, host)? {
            ReplyBox::Empty => Ok(false),
            ReplyBox::Draft(Some(draft)) if super::matches_single_line_draft(&draft, text) => {
                Ok(true)
            }
            _ => Err(Failure::unknown("chat composer did not confirm the reply")),
        }
    };
    for retry in 0..=2 {
        let first = sample(handle)?;
        let second = sample(handle)?;
        if !second {
            return Ok(());
        }
        if !first {
            return Err(Failure::unknown(
                "Codex showed the reply draft after an empty composer",
            ));
        }
        if retry == 2 {
            return Err(Failure::unknown("Codex still shows the submitted draft"));
        }
        handle.send(
            "pane.send_keys",
            json!({"pane_id":handle.pane,"keys":["Enter"]}),
            true,
            save,
        )?;
    }
    unreachable!()
}

fn codex_history(screen: &Screen) -> Vec<&str> {
    let mut lines = screen_lines(&screen.text);
    lines.truncate(prompt_row(&lines).unwrap_or(0));
    lines
}

fn matching_user_entries(history: &[&str], text: &str) -> usize {
    (0..history.len())
        .filter(|&index| matches_user_entry(&history[index..], text))
        .count()
}

fn matches_user_entry(rows: &[&str], text: &str) -> bool {
    let [first, tail @ ..] = rows else {
        return false;
    };
    let Some(body) = first
        .strip_prefix("› ")
        .filter(|body| body.starts_with("[router] "))
    else {
        return false;
    };
    let Some(mut rest) = text.strip_prefix(body) else {
        return false;
    };
    for row in tail {
        if row.is_empty() {
            return rest.is_empty();
        }
        if rest.is_empty() {
            return false;
        }
        // A screen wrap may indent the continuation or omit its ending space.
        let next = row.strip_prefix("  ").unwrap_or(row);
        let Some(remaining) = rest
            .strip_prefix(next)
            .or_else(|| rest.strip_prefix(' ')?.strip_prefix(next))
        else {
            return false;
        };
        rest = remaining;
    }
    false
}

pub(super) fn check_codex_input(action: CodexInput, before: &Screen, text: &str) -> Result<()> {
    match action {
        CodexInput::Continue if matching_user_entries(&codex_history(before), text) != 0 => {
            Err(Failure::unsent("new task already appeared before input"))
        }
        _ => Ok(()),
    }
}

fn appended_history<'a, 'row>(
    base: &[&str],
    history: &'a [&'row str],
    truncated: bool,
    text: &str,
) -> Option<&'a [&'row str]> {
    if base.is_empty() && truncated {
        return None;
    }
    if history.starts_with(base) {
        return Some(&history[base.len()..]);
    }
    if !truncated {
        return None;
    }
    let old_copies = matching_user_entries(base, text);
    let mut overlaps = (2..base.len()).filter(|&length| {
        let retained = &base[base.len() - length..];
        history.starts_with(retained)
            && retained.windows(2).any(|pair| pair[0] != pair[1])
            && matching_user_entries(retained, text) == old_copies
            && base
                .windows(length)
                .filter(|part| *part == retained)
                .count()
                == 1
            && history
                .windows(length)
                .filter(|part| *part == retained)
                .count()
                == 1
    });
    let overlap = overlaps.next()?;
    overlaps.next().is_none().then_some(&history[overlap..])
}

fn visible_history_start(recent: &Screen, visible: &Screen) -> Option<usize> {
    if visible.truncated {
        return None;
    }
    let recent = screen_lines(&recent.text);
    let viewport = screen_lines(&visible.text);
    let first = viewport.iter().position(|line| !line.is_empty())?;
    let viewport = &viewport[first..];
    if !recent.ends_with(viewport) {
        return None;
    }
    let start = recent.len() - viewport.len();
    (prompt_row(&recent)? >= start).then_some(start)
}

pub(super) fn nudge_visible_copies(
    text: &str,
    before: &Screen,
    archive: &Screen,
    before_visible: &Screen,
) -> Option<usize> {
    // An unseen part of history might contain the same reminder. Account for
    // prior copies with the wider read and locate the append with the recent read.
    if archive.truncated {
        return None;
    }
    let start = visible_history_start(archive, before_visible)?;
    visible_history_start(before, before_visible)?;
    let history = codex_history(archive);
    let copies = matching_user_entries(&history[start..], text);
    (history.ends_with(&codex_history(before)) && matching_user_entries(&history, text) == copies)
        .then_some(copies)
}

fn confirms_nudge(
    text: &str,
    before: &Screen,
    after: &Screen,
    after_visible: &Screen,
    visible_copies: Option<usize>,
) -> bool {
    let Some(copies) = visible_copies else {
        return false;
    };
    if before.truncated && !after.truncated {
        return false;
    }
    let Some(after_start) = visible_history_start(after, after_visible) else {
        return false;
    };
    let base = codex_history(before);
    let history = codex_history(after);
    let Some(added) = appended_history(&base, &history, after.truncated, text) else {
        return false;
    };
    let Some(first) = added.iter().position(|row| !row.is_empty()) else {
        return false;
    };
    let entry_start = history.len() - added.len() + first;
    entry_start >= after_start
        && matching_user_entries(&history[after_start..entry_start], text) == copies
        && matches_user_entry(&added[first..], text)
        && matching_user_entries(added, text) == 1
}

pub(super) fn confirm_codex_input(
    handle: &Handle,
    action: CodexInput,
    text: &str,
    before: &Screen,
    visible_copies: Option<usize>,
) -> Result<()> {
    for _ in 0..2 {
        sleep(Duration::from_millis(300));
        let screen = handle.read()?;
        let entry = match action {
            // The full brief contains a fresh callback reference.
            CodexInput::Continue => matching_user_entries(&codex_history(&screen), text) == 1,
            CodexInput::Nudge => confirms_nudge(
                text,
                before,
                &screen,
                &handle.read_visible()?,
                visible_copies,
            ),
        };
        if read_only(&screen.text)
            || refusals(&screen.text) > refusals(&before.text)
            || !matches!(
                observe_herdr_reply_box(handle, &screen, "codex")?,
                ReplyBox::Empty
            )
            || !entry
        {
            let diagnostic = match action {
                CodexInput::Continue => {
                    "Herdr did not show a new task entry in the Codex transcript"
                }
                CodexInput::Nudge => {
                    "Herdr did not show a new reminder entry in the Codex transcript"
                }
            };
            return Err(Failure::unknown(diagnostic));
        }
    }
    Ok(())
}

pub(super) fn confirm_codex(
    handle: &mut Handle,
    text: &str,
    before: &Screen,
    save: &mut impl FnMut(&Handle) -> std::io::Result<()>,
) -> Result<()> {
    let expected = compact(text);
    let footer = codex_box(before, &[], Some(&expected))
        .filter(|c| c.draft.is_some())
        .and_then(|c| c.footer)
        .unwrap_or_default();
    let sample = |handle: &Handle| -> Result<Option<(String, bool)>> {
        sleep(Duration::from_millis(300));
        let screen = handle.read()?;
        if read_only(&screen.text) || refusals(&screen.text) > refusals(&before.text) {
            return Err(Failure::unknown("worker refused or paused input"));
        }
        let composer = codex_box(&screen, &footer, Some(&expected));
        let composer = composer.and_then(|c| Some((c.draft?, c.footer.is_some())));
        match composer {
            Some((draft, certain)) if !draft.is_empty() && compact(&draft) == expected => {
                Ok(Some((draft, certain)))
            }
            _ => Ok(None),
        }
    };
    for retry in 0..=2 {
        let first = sample(handle)?;
        let second = sample(handle)?;
        let (Some((first_draft, first_certain)), Some((second_draft, second_certain))) =
            (first, second)
        else {
            return Ok(());
        };
        if first_draft != second_draft {
            return Ok(());
        }
        if !first_certain || !second_certain {
            return Err(Failure::unknown(
                "Codex draft and footer boundary is ambiguous",
            ));
        }
        if retry == 2 {
            return Err(Failure::unknown("Codex still shows the submitted draft"));
        }
        handle.send(
            "pane.send_keys",
            json!({"pane_id":handle.pane,"keys":["Enter"]}),
            true,
            save,
        )?;
    }
    unreachable!()
}

fn opencode_box(text: &str) -> Option<String> {
    let lines: Vec<_> = text.lines().map(str::trim).collect();
    let end = lines.iter().rposition(|s| {
        s.strip_prefix('╹')
            .is_some_and(|bar| !bar.is_empty() && bar.chars().all(|c| c == '▀'))
    })?;
    let start = lines[..end]
        .iter()
        .rposition(|s| !s.starts_with('┃'))
        .map_or(0, |i| i + 1);
    let mut rows: Vec<_> = lines[start..end]
        .iter()
        .filter_map(|s| s.strip_prefix('┃'))
        .map(str::trim)
        .collect();
    while rows.last() == Some(&"") {
        rows.pop();
    }
    rows.pop()?.contains(" · ").then(|| rows.join(" "))
}

impl Handle {
    /// OpenCode's argv prompt may still be in the composer. Confirm this
    /// attempt's stable visible suffix before sending one Enter.
    pub fn activate_opencode_prompt(
        &mut self,
        text: &str,
        mut save: impl FnMut(&Self) -> std::io::Result<()>,
    ) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut previous = None;
        while Instant::now() < deadline {
            let screen = self.read()?;
            if read_only(&screen.text) {
                return Err(Failure::refused("worker input is paused"));
            }
            let observed = opencode_box(&screen.text);
            if let Some(draft) = &observed {
                if observed == previous {
                    let shown = compact(draft);
                    if shown.contains(&self.attempt) && compact(text).ends_with(&shown) {
                        self.send(
                            "pane.send_keys",
                            json!({"pane_id":self.pane,"keys":["Enter"]}),
                            true,
                            &mut save,
                        )?;
                    }
                    return Ok(());
                }
            }
            previous = observed;
            sleep(Duration::from_millis(300));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observe_codex(
        plain: &Screen,
        read_ansi: impl FnOnce() -> Result<Screen>,
    ) -> Result<ReplyBox> {
        super::observe_codex(&plain.text, || Ok(ansi::rows(&read_ansi()?.text)))
    }

    fn view(text: &str) -> Screen {
        Screen {
            text: text.into(),
            revision: 0,
            truncated: false,
        }
    }

    #[test]
    fn codex_nudge_confirmation_distinguishes_visible_input_from_an_archived_copy() {
        let text = "[router] Message from the controller: check the report";
        let composer = "› Ask Codex to do anything\n\n  model · dir";
        let before_view = view(&format!("› {text}\n\n{composer}"));
        let after_view = view(&format!("› {text}\n\n› {text}\n\n{composer}"));
        let before = view(&format!("Earlier output\n{}", before_view.text));
        let after = view(&format!("Earlier output\n{}", after_view.text));
        let copies = nudge_visible_copies(text, &before, &before, &before_view);
        assert_eq!(copies, Some(1));
        assert!(confirms_nudge(text, &before, &after, &after_view, copies));
        let archived = nudge_visible_copies(text, &before, &before, &view(composer));
        assert_eq!(archived, None);
        assert!(!confirms_nudge(
            text,
            &before,
            &after,
            &after_view,
            archived
        ));
        assert!(!confirms_nudge(
            text,
            &before,
            &after,
            &view("different snapshot"),
            copies,
        ));
        let incomplete = Screen {
            truncated: true,
            ..view(&before.text)
        };
        assert_eq!(
            nudge_visible_copies(text, &before, &incomplete, &before_view),
            None
        );

        let recent = Screen {
            truncated: true,
            ..view(&format!(
                "Dropped row\nStable row A\nStable row B\n{composer}"
            ))
        };
        let current_view = view(&format!("› {text}\n\n{composer}"));
        let current = Screen {
            truncated: true,
            ..view(&format!(
                "Stable row A\nStable row B\n\n{}",
                current_view.text
            ))
        };
        let archive = view(&format!("Earlier output\n{}", recent.text));
        let copies = nudge_visible_copies(text, &recent, &archive, &view(composer));
        assert_eq!(copies, Some(0));
        assert!(confirms_nudge(
            text,
            &recent,
            &current,
            &current_view,
            copies
        ));
        let old_copy = view(&format!("› {text}\n\n{}", archive.text));
        let archived = nudge_visible_copies(text, &recent, &old_copy, &view(composer));
        assert_eq!(archived, None);
        assert!(!confirms_nudge(
            text,
            &recent,
            &current,
            &current_view,
            archived
        ));
    }

    #[test]
    fn footer_ambiguity_depends_on_whether_the_draft_matches() {
        let screen = Screen {
            text: "› draft\n  tail · value\n  model · dir".into(),
            revision: 2,
            truncated: true,
        };
        for footer in [&[][..], &["  previous · footer"][..]] {
            for text in ["draft", "draft tail · value"] {
                let composer = codex_box(&screen, footer, Some(&compact(text))).unwrap();
                assert!(composer.footer.is_none());
                assert_eq!(compact(composer.draft.as_deref().unwrap()), compact(text));
            }
            assert!(codex_box(&screen, footer, Some("unrelated")).is_none());
        }
    }

    #[test]
    fn only_matching_styled_cells_prove_a_compact_footer() {
        let flat = view("›\n  model · dir\n  keys · help");
        let empty =
            "\x1b[48;2;0;12;32m\x1b[1m›\x1b[0m\r\n  \x1b[38;2;0;0;18mmodel · dir\x1b[0m\r\n  keys · help";
        assert!(matches!(
            observe_codex(&flat, || Ok(view(empty))).unwrap(),
            ReplyBox::Empty
        ));
        let draft = "\x1b[48;2;0;12;32m\x1b[1m›\r\n  model · dir\x1b[0m\r\n  keys · help";
        for ansi in [
            draft,
            flat.text.as_str(),
            "› different\n  model · dir\n  keys · help",
        ] {
            assert!(matches!(
                observe_codex(&flat, || Ok(view(ansi))).unwrap(),
                ReplyBox::Unproven(_)
            ));
        }

        let placeholder = view("› Ask Codex to do anything\n  model · dir\n  keys · help");
        let ordinary =
            "\x1b[48;5;24m\x1b[1m› Ask Codex to do anything\x1b[0m\n  model · dir\n  keys · help";
        assert!(matches!(
            observe_codex(&placeholder, || Ok(view(ordinary))).unwrap(),
            ReplyBox::Draft(Some(draft)) if draft == "Ask Codex to do anything"
        ));
        let dimmed =
            "\x1b[48;5;24m\x1b[1m› \x1b[2mAsk Codex to do anything\x1b[0m\n  model · dir\n  keys · help";
        assert!(matches!(
            observe_codex(&placeholder, || Ok(view(dimmed))).unwrap(),
            ReplyBox::Empty
        ));
    }

    #[test]
    fn matching_placeholder_needs_dim_text_and_an_enabled_chat_prompt() {
        let flat = view("» Ask Codex to do\n  anything\n\n  model · dir");
        for (styled, empty) in [
            (
                "\x1b[1m»\x1b[0m \x1b[2mAsk Codex to do\n  anything\x1b[0m",
                true,
            ),
            ("\x1b[1m»\x1b[0m Ask Codex to do\n  anything", false),
            (
                "\x1b[1m»\x1b[0m \x1b[2mAsk Codex to do\x1b[0m\n  anything",
                false,
            ),
            ("\x1b[2m» Ask Codex to do\n  anything\x1b[0m", false),
        ] {
            let ansi = view(&format!("{styled}\n\n  model · dir"));
            assert_eq!(
                matches!(observe_codex(&flat, || Ok(ansi)).unwrap(), ReplyBox::Empty),
                empty
            );
        }
        assert!(matches!(
            observe_codex(
                &view("›\n\n  model · dir\n! echo x\n\n  model · dir"),
                || { panic!("a non-chat frame cannot supply a reply box") }
            )
            .unwrap(),
            ReplyBox::Unproven(_)
        ));
        assert!(matches!(
            observe_codex(&view("› \u{a0}\n\n  model · dir"), || {
                Ok(view("\x1b[1m›\x1b[0m \u{a0}\n\n  model · dir"))
            })
            .unwrap(),
            ReplyBox::Draft(_)
        ));
    }

    #[test]
    fn real_codex_footer_and_attachment_area_are_independent_signals() {
        let footer = "  ? for shortcuts                              100% context left";
        let text = format!("  [Image #1]\n\n› Ask Codex to do anything\n\n{footer}");
        let editor = "\x1b[48;5;24m\x1b[1m›\x1b[0m \x1b[2mAsk Codex to do anything\x1b[0m";
        for (marker, occupied) in [
            ("  [Image #1]", false),
            ("\x1b[48;5;24m  [Image #1]\x1b[0m", true),
        ] {
            let styled = view(&format!("{marker}\n\n{editor}\n\n{footer}"));
            let actual = observe_codex(&view(&text), || Ok(styled)).unwrap();
            assert_eq!(matches!(&actual, ReplyBox::Draft(None)), occupied);
            assert_eq!(matches!(actual, ReplyBox::Empty), !occupied);
        }
        let empty = view(&format!("› Ask Codex to do anything\n\n{footer}"));
        let newly_attached = view(&format!(
            "\x1b[48;5;24m  [Image #1]\x1b[0m\n\n{editor}\n\n{footer}"
        ));
        assert!(matches!(
            observe_codex(&empty, || Ok(newly_attached)).unwrap(),
            ReplyBox::Draft(None)
        ));
    }

    #[test]
    fn reply_text_proof_preserves_whitespace_and_requires_a_single_line() {
        assert!(super::super::matches_single_line_draft("a b", "a b"));
        assert!(!super::super::matches_single_line_draft("ab", "a b"));
        assert!(!super::super::matches_single_line_draft("a  b", "a b"));
        assert!(!super::super::matches_single_line_draft("a ", "a"));
        assert!(!super::super::matches_single_line_draft("a\nb", "a\nb"));
        let folded = "› reply: a\n  b\n\n  model · dir";
        let styled = "\x1b[1m›\x1b[0m reply: a\n  b\n\n  model · dir";
        assert!(matches!(
            observe_codex(&view(folded), || Ok(view(styled))).unwrap(),
            ReplyBox::Draft(None)
        ));
    }

    #[test]
    fn only_a_complete_current_composer_matches() {
        let screen = |s: &str| Screen {
            text: s.into(),
            revision: 1,
            truncated: false,
        };
        assert_eq!(
            codex_box(
                &screen("› hello\n  世界\n\n  model · dir"),
                &[],
                Some("hello世界")
            )
            .and_then(|c| c.draft)
            .as_deref(),
            Some("hello   世界")
        );
        for s in [
            "› hello\n  … ↑\n\n  model · dir",
            "› hello\n  … ↓\n\n  model · dir",
            "› hello\ntranscript\n  model · dir",
            "› Ask Codex to do anything\n\n  model · dir",
        ] {
            assert!(codex_box(&screen(s), &[], Some("hello"))
                .and_then(|c| c.draft)
                .is_none_or(|d| d.is_empty()));
        }
        assert_eq!(
            opencode_box("┃ hello attempt-01\n┃ agent · model\n╹▀▀▀").as_deref(),
            Some("hello attempt-01")
        );
        assert!(read_only("■ This conversation is unavailable. Its cached transcript and\ndraft remain here; input is paused."));
    }
}
mod ansi;
