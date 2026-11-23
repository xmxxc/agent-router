use super::{failure, InputBoundary, Reply, Target, Tmux, APPENDED_USER_ENTRY};
use serde_json::{json, Value};
use std::env;
use std::thread::sleep;
use std::time::{Duration, Instant};
use wfc_herdr::{classify_reply_capture, matches_single_line_draft, ReplyBox};

const UNAVAILABLE: &str = "This conversation is unavailable. Its cached transcript and draft remain here; input is paused.";
const REFUSAL: &str = "This conversation is read-only or unavailable; no operation was sent.";
const NO_TMUX_MODE: &str = "#{==:#{pane_in_mode},0}";

#[derive(Default)]
struct Notices {
    state: Option<String>,
    refusals: usize,
}

fn notices(screen: &str) -> Notices {
    let mut found = Notices::default();
    let lines: Vec<_> = screen.lines().map(str::trim).collect();
    for i in 0..lines.len() {
        if lines[i].is_empty() {
            continue;
        }
        let line = lines[i..(i + 4).min(lines.len())].join(" ");
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        let text = line.strip_prefix(['■', '•']).unwrap_or(&line).trim_start();
        if text.starts_with(REFUSAL) {
            found.refusals += 1;
        }
        if text.starts_with(UNAVAILABLE) {
            found.state = Some(UNAVAILABLE.into());
        }
        if let Some((banner, _)) = text
            .strip_prefix("This chat uses ")
            .and_then(|s| s.split_once(", so this chat is read-only."))
        {
            if banner.contains(", but Codex now uses ") {
                found.state = Some(format!(
                    "This chat uses {banner}, so this chat is read-only."
                ));
            }
        }
    }
    found
}

// Only the last prompt immediately above the model footer is a composer.
// Submitted transcript lines use the same leading glyph.
#[derive(PartialEq)]
struct Draft {
    text: String,
    scrolled: bool,
    single_line: Option<String>,
}

fn codex_draft(screen: &str) -> Option<Draft> {
    let lines: Vec<_> = screen
        .trim_end()
        .lines()
        .map(|line| line.trim_end_matches(' '))
        .collect();
    let start = lines
        .iter()
        .rposition(|s| *s == "›" || s.starts_with("› "))?;
    let mut end = start + 1;
    while end < lines.len() && lines[end].starts_with("  ") {
        end += 1;
    }
    if end == lines.len() {
        for _ in 0..2 {
            if end > start + 1 && lines[end - 1].contains(" · ") {
                end -= 1;
            } else {
                break;
            }
        }
    }
    let footer: Vec<_> = lines[end..].iter().filter(|s| !s.is_empty()).collect();
    if !(1..=2).contains(&footer.len())
        || !footer
            .iter()
            .all(|s| s.starts_with("  ") && s.contains(" · "))
    {
        return None;
    }
    let mut text = String::new();
    let mut scrolled = false;
    for (n, line) in lines[start..end].iter().enumerate() {
        let line = if n == 0 {
            line.strip_prefix('›')?
        } else {
            line
        };
        scrolled |= line.ends_with(['↑', '↓']);
        let line = line.strip_suffix(['↑', '↓']).unwrap_or(line);
        if n > 0 {
            text.push(' ');
        }
        text.push_str(line.trim());
    }
    let single_line = match &lines[start..end] {
        [line] if !scrolled => line.strip_prefix("› ").map(str::to_owned),
        _ => None,
    };
    Some(Draft {
        text,
        scrolled,
        single_line,
    })
}

fn strict_composer(screen: &str) -> Option<(usize, &str)> {
    let lines: Vec<_> = screen.trim_end().lines().map(str::trim_end).collect();
    let prompt = lines
        .iter()
        .rposition(|s| *s == "›" || s.starts_with("› "))?;
    let [line, "", footer @ ..] = &lines[prompt..] else {
        return None;
    };
    if !(1..=2).contains(&footer.len())
        || !footer
            .iter()
            .all(|s| s.starts_with("  ") && s.contains(" · "))
    {
        return None;
    }
    // A plain redraw cannot distinguish an adjacent draft attachment from history.
    if lines[..prompt].iter().rev().take(2).any(|line| {
        line.trim_start()
            .strip_prefix("[Image #")
            .and_then(|label| label.strip_suffix(']'))
            .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
    }) {
        return None;
    }
    if *line == "›" {
        return Some((prompt, ""));
    }
    let shown = line.strip_prefix("› ")?;
    (!shown.ends_with(['↑', '↓'])).then_some((prompt, shown))
}

fn strict_screen(screen: &str) -> Option<(Vec<&str>, &str)> {
    let (start, draft) = strict_composer(screen)?;
    let history = screen
        .lines()
        .take(start)
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect();
    Some((history, draft))
}

fn refusal_entry(line: &str) -> bool {
    let line = line.trim_start();
    line.strip_prefix(['■', '•']).unwrap_or(line).trim_start() == REFUSAL
}

#[derive(Clone, Copy)]
struct HistoryExtent {
    size: usize,
    limit: usize,
}

struct InputScreen<'a> {
    scrollback: Vec<&'a str>,
    visible: Vec<&'a str>,
    entry_breaks: Vec<bool>,
    draft: &'a str,
    extent: HistoryExtent,
}

struct InputDelta<'a, 'row> {
    rows: &'a [&'row str],
    retained_history: &'a [&'row str],
    separated_entry: bool,
}

struct InputCapture {
    full: String,
    viewport: String,
    extent: HistoryExtent,
}

impl InputCapture {
    fn parse(&self) -> Option<InputScreen<'_>> {
        let (mut full, full_draft) = strict_screen(&self.full)?;
        let (visible, draft) = strict_screen(&self.viewport)?;
        let cut = full.len().checked_sub(visible.len())?;
        let active = &full[cut..];
        // The first visible row may begin partway through a native soft wrap.
        if full_draft != draft
            || !active
                .iter()
                .zip(&visible)
                .enumerate()
                .all(|(i, (all, view))| all == view || (i == 0 && all.ends_with(*view)))
        {
            return None;
        }
        let visible = full.split_off(cut);
        let (prompt, _) = strict_composer(&self.viewport)?;
        let mut entry_breaks = Vec::with_capacity(visible.len());
        for line in self.viewport.lines().take(prompt) {
            if line.trim_end().is_empty() {
                if let Some(last) = entry_breaks.last_mut() {
                    *last = true;
                }
            } else {
                entry_breaks.push(false);
            }
        }
        Some(InputScreen {
            scrollback: full,
            visible,
            entry_breaks,
            draft,
            extent: self.extent,
        })
    }
}

impl InputScreen<'_> {
    fn history_overlap(&self, after: &InputScreen<'_>) -> Option<usize> {
        if after.extent.limit != self.extent.limit {
            return None;
        }
        if after.extent.size >= self.extent.size && after.scrollback.starts_with(&self.scrollback) {
            return Some(self.scrollback.len());
        }
        // tmux reclaims the oldest history in batches near its configured cap.
        // A reset, missing anchor or multiple matching boundaries is unproven.
        let limit = after.extent.limit;
        if limit == 0 || after.extent.size < limit.saturating_sub((limit / 10).max(1)) {
            return None;
        }
        let mut matches = (2..self.scrollback.len()).filter(|&count| {
            let retained = &self.scrollback[self.scrollback.len() - count..];
            retained.windows(2).any(|rows| rows[0] != rows[1])
                && after.scrollback.starts_with(retained)
        });
        let overlap = matches.next()?;
        matches.next().is_none().then_some(overlap)
    }

    fn added<'a, 'row>(&self, after: &'a InputScreen<'row>) -> Option<InputDelta<'a, 'row>> {
        let overlap = self.history_overlap(after)?;
        let (retained_history, scrolled) = after.scrollback.split_at(overlap);
        let added = if self.visible.is_empty() {
            0
        } else {
            let mut matches = (0..self.visible.len()).filter_map(|drop| {
                // The omitted head must have just scrolled off, immediately
                // before the same retained transcript suffix.
                let retained = &self.visible[drop..];
                (scrolled.ends_with(&self.visible[..drop]) && after.visible.starts_with(retained))
                    .then_some(retained.len())
            });
            let added = matches.next()?;
            if matches.next().is_some() {
                return None;
            }
            added
        };
        Some(InputDelta {
            rows: &after.visible[added..],
            retained_history,
            separated_entry: after.entry_breaks.get(added).copied().unwrap_or(false),
        })
    }

    fn attributable(&self, delta: &InputDelta<'_, '_>, matches: impl Fn(&str) -> bool) -> bool {
        let hidden = self.scrollback.iter().any(|line| matches(line))
            && !self.visible.iter().any(|line| matches(line));
        // Require both versions when the only earlier lookalike was hidden:
        // the old row still anchored in history and an independently delimited
        // entry at the live append boundary. A raster with no boundary is ambiguous.
        !hidden
            || (delta.separated_entry && delta.retained_history.iter().any(|line| matches(line)))
    }

    fn refused(&self, after: &InputScreen<'_>) -> Option<Value> {
        let delta = self.added(after)?;
        let [notice] = delta.rows else {
            return None;
        };
        if after.draft.is_empty()
            && refusal_entry(notice)
            && self.attributable(&delta, refusal_entry)
        {
            let mut result = refused(REFUSAL);
            result["inputEvidence"] = json!("new-host-refusal");
            return Some(result);
        }
        None
    }
}

fn user_entry(line: &str, text: &str) -> bool {
    line.strip_prefix("› ") == Some(text)
}

fn appended_result(
    before: &InputScreen<'_>,
    after: &InputScreen<'_>,
    text: &str,
) -> Reply<Option<Value>> {
    if !after.draft.is_empty() {
        return Ok(None);
    }
    if let Some(refusal) = before.refused(after) {
        return Err(refusal);
    }
    let delta = before
        .added(after)
        .ok_or_else(|| uncertain("Codex transcript continuity was lost after input"))?;
    let added = delta.rows;
    let Some(first) = added.first() else {
        return Ok(None);
    };
    if user_entry(first, text)
        && before.attributable(&delta, |line| user_entry(line, text))
        && added.iter().filter(|line| user_entry(line, text)).count() == 1
        && !added.iter().any(|line| refusal_entry(line))
    {
        return Ok(Some(
            json!({"result":"accepted","inputEvidence":APPENDED_USER_ENTRY}),
        ));
    }
    Err(uncertain(
        "Codex showed no entry attributable to this input",
    ))
}

fn matches_input(draft: &Draft, text: &str) -> bool {
    let shown: String = draft.text.split_whitespace().collect();
    if shown.is_empty() || shown == "AskCodextodoanything" {
        return false;
    }
    let typed: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if shown.chars().count() < 8.min(typed.chars().count()) {
        return false;
    }
    if draft.scrolled {
        typed.contains(&shown)
    } else {
        typed.starts_with(&shown)
    }
}

fn setting(key: &str, default: f64) -> Duration {
    env::var(key)
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .and_then(|s| Duration::try_from_secs_f64(s.max(0.)).ok())
        .unwrap_or_else(|| Duration::from_secs_f64(default))
}

fn refused(message: &str) -> Value {
    json!({"result":"refused","category":"permission_required","diagnostic":message,"notice":message})
}

fn check_notice(screen: &str, before: usize) -> Reply<()> {
    let n = notices(screen);
    if let Some(message) = n
        .state
        .or_else(|| (n.refusals > before).then(|| REFUSAL.to_owned()))
    {
        return Err(refused(&message));
    }
    Ok(())
}

fn uncertain(message: &str) -> Value {
    json!({"result":"uncertain","category":"input_uncertain","diagnostic":message})
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum ResumeInput {
    SendBrief,
    CheckReadOnly,
}

impl ResumeInput {
    pub(super) fn for_host(host: Option<&str>, probe_read_only: bool) -> Option<Self> {
        match host {
            Some("opencode") => Some(Self::SendBrief),
            Some("codex") if probe_read_only => Some(Self::CheckReadOnly),
            _ => None,
        }
    }
}

impl Tmux<'_> {
    fn reply_snapshot(&self, t: &Target, host: &str) -> Reply<(String, ReplyBox)> {
        let capture = self.worker_command_if(
            t,
            &format!("capture-pane -p -e -t {}", t.worker_pane()?),
            NO_TMUX_MODE,
        )?;
        classify_reply_capture(host, &capture)
            .ok_or_else(|| failure("definitely-not-sent", "could not read tmux composer styles"))
    }

    fn reply_needs_enter(
        &self,
        t: &Target,
        host: &str,
        text: &str,
        earlier_refusals: &mut usize,
    ) -> Reply<bool> {
        let (screen, first) = self.reply_snapshot(t, host)?;
        check_notice(&screen, *earlier_refusals)?;
        let first_was_draft = match first {
            ReplyBox::Empty => false,
            ReplyBox::Draft(Some(shown)) if matches_single_line_draft(&shown, text) => true,
            _ => return Err(uncertain("chat composer did not confirm the reply")),
        };
        sleep(Duration::from_millis(300));
        let (screen, second) = self.reply_snapshot(t, host)?;
        check_notice(&screen, *earlier_refusals)?;
        match second {
            ReplyBox::Empty => Ok(false),
            ReplyBox::Draft(Some(shown)) if matches_single_line_draft(&shown, text) => {
                if !first_was_draft {
                    return Err(uncertain("reply draft reappeared after an empty composer"));
                }
                *earlier_refusals = notices(&screen).refusals;
                Ok(true)
            }
            _ => Err(uncertain(
                "chat composer changed before Enter could be retried",
            )),
        }
    }

    fn observe(&self, t: &Target, before: usize, window: Duration) -> Reply<()> {
        let start = Instant::now();
        loop {
            sleep(
                window
                    .saturating_sub(start.elapsed())
                    .min(Duration::from_millis(250)),
            );
            check_notice(&self.worker_screen(t)?, before)?;
            if start.elapsed() >= window {
                return Ok(());
            }
        }
    }

    pub(super) fn submit(
        &self,
        t: &Target,
        text: &str,
        host: Option<&str>,
        confirm_reply: bool,
        check_input: &mut dyn FnMut(InputBoundary) -> Reply<()>,
    ) -> Reply<Value> {
        if text.contains('\0') {
            return Err(failure("definitely-not-sent", "tmux input contains NUL"));
        }
        let host_for_reply = host.unwrap_or_default();
        if confirm_reply {
            check_input(InputBoundary::PrepareText)?;
        }
        let before = if confirm_reply {
            self.reply_snapshot(t, host_for_reply)
                .map(|(screen, composer)| (screen, Some(composer)))
        } else {
            self.worker_screen(t).map(|screen| (screen, None))
        };
        let (screen, composer) = before.map_err(|e| {
            if e["result"] == "refused" || e["result"] == "definitely-not-sent" {
                e
            } else {
                failure(
                    "definitely-not-sent",
                    "could not observe the session before input",
                )
            }
        })?;
        let before = notices(&screen);
        if let Some(message) = before.state {
            return Err(refused(&message));
        }
        let codex = host == Some("codex");
        match composer {
            Some(ReplyBox::Draft(_)) => {
                return Err(failure(
                    "definitely-not-sent",
                    "chat composer already contains a draft",
                ));
            }
            Some(ReplyBox::Unproven(reason)) => return Err(failure("definitely-not-sent", reason)),
            _ => {}
        }
        let send = |action: &str| {
            if confirm_reply {
                self.worker_command_if(t, action, NO_TMUX_MODE)
            } else {
                self.worker_command(t, action)
            }
        };
        check_input(InputBoundary::Text)?;
        self.type_literal(t, text, confirm_reply)?;
        sleep(Duration::from_millis(500));
        let window = setting("WFC_TYPED_CHECK_SECONDS", 1.5);
        let mut earlier_refusals = before.refusals;
        let after_text = |err: Value| {
            if err["category"] == "permission_required" {
                if confirm_reply {
                    let mut err = err;
                    err["result"] = json!("uncertain");
                    err
                } else {
                    err
                }
            } else if err["category"] == "input_uncertain" {
                err
            } else {
                uncertain("could not observe the session after input")
            }
        };
        if confirm_reply {
            check_input(InputBoundary::PrepareEnter)?;
            let (screen, draft) = self.reply_snapshot(t, host_for_reply).map_err(after_text)?;
            check_notice(&screen, earlier_refusals).map_err(after_text)?;
            if !matches!(draft, ReplyBox::Draft(Some(shown)) if matches_single_line_draft(&shown, text))
            {
                return Err(uncertain(
                    "chat composer did not confirm the complete reply before Enter",
                ));
            }
        }
        for retry in 0..=2 {
            if confirm_reply {
                check_input(InputBoundary::Enter)?;
            }
            send(&format!("send-keys -t {} Enter", t.worker_pane()?))
                .map_err(|_| uncertain("text was typed but Enter was not confirmed"))?;
            self.observe(t, earlier_refusals, window)
                .map_err(after_text)?;
            if confirm_reply {
                check_input(InputBoundary::PrepareEnter)?;
                if !self
                    .reply_needs_enter(t, host_for_reply, text, &mut earlier_refusals)
                    .map_err(after_text)?
                {
                    break;
                }
                if retry == 2 {
                    return Err(uncertain("typed text remained in the host input box"));
                }
                continue;
            }
            if !codex {
                break;
            }
            let first = self.worker_screen(t).map_err(after_text)?;
            check_notice(&first, earlier_refusals).map_err(after_text)?;
            let draft = codex_draft(&first);
            if !draft.as_ref().is_some_and(|d| matches_input(d, text)) {
                break;
            }
            sleep(Duration::from_millis(300));
            let second = self.worker_screen(t).map_err(after_text)?;
            check_notice(&second, earlier_refusals).map_err(after_text)?;
            let next = codex_draft(&second);
            if draft != next {
                break;
            }
            earlier_refusals = notices(&second).refusals;
            if retry == 2 {
                return Err(uncertain("typed text remained in the host input box"));
            }
        }
        Ok(json!({"result":"accepted"}))
    }

    fn type_literal(&self, t: &Target, text: &str, guard_mode: bool) -> Reply<()> {
        // Single-quote for the tmux command parser inside if-shell. No shell is invoked.
        let literal = text.replace('\'', "'\\''");
        let pane = t.worker_pane()?;
        let action = format!("send-keys -t {pane} -l -- '{literal}'");
        (if guard_mode {
            self.worker_command_if(t, &action, NO_TMUX_MODE)
        } else {
            self.worker_command(t, &action)
        })
        .map_err(|mut err| {
            if err["result"] == "uncertain" {
                err["category"] = json!("input_uncertain");
            }
            err
        })?;
        Ok(())
    }

    fn input_capture(&self, t: &Target) -> Reply<InputCapture> {
        let pane = t.worker_pane()?;
        let marker = format!("wfc-input-viewport-{}", uuid::Uuid::new_v4().simple());
        // Capture both regions in one guarded tmux command queue.
        let capture = self.worker_command_if(t, &format!(
            "capture-pane -p -J -t {pane} -S - ; display-message -p -t {pane} '{marker}:#{{history_size}}:#{{history_limit}}' ; capture-pane -p -J -t {pane}"
        ), NO_TMUX_MODE)?;
        let marker = format!("\n{marker}:");
        let (full, tail) = capture
            .split_once(&marker)
            .filter(|(_, tail)| !tail.contains(&marker))
            .ok_or_else(|| uncertain("could not identify the live Codex viewport"))?;
        let extent_error = || uncertain("could not verify tmux history limits");
        let (extent, viewport) = tail.split_once('\n').ok_or_else(extent_error)?;
        let (size, limit) = extent.split_once(':').ok_or_else(extent_error)?;
        let extent = HistoryExtent {
            size: size.parse().map_err(|_| extent_error())?,
            limit: limit.parse().map_err(|_| extent_error())?,
        };
        if extent.size > extent.limit {
            return Err(extent_error());
        }
        Ok(InputCapture {
            full: full.to_owned(),
            viewport: viewport.to_owned(),
            extent,
        })
    }

    pub(super) fn checked_submit(
        &self,
        t: &Target,
        text: &str,
        host: Option<&str>,
        check_input: &mut dyn FnMut(InputBoundary) -> Reply<()>,
    ) -> Reply<Value> {
        if text.contains('\0') {
            return Err(failure("definitely-not-sent", "tmux input contains NUL"));
        }
        let unavailable = || {
            failure(
                "definitely-not-sent",
                "could not verify an empty Codex composer",
            )
        };
        let empty = || -> Reply<InputCapture> {
            let capture = self.input_capture(t).map_err(|_| unavailable())?;
            if let Some(message) = notices(&capture.viewport).state {
                return Err(refused(&message));
            }
            Ok(capture)
        };
        let opening = empty()?;
        let starting = opening
            .parse()
            .filter(|screen| screen.draft.is_empty())
            .ok_or_else(unavailable)?;
        if host != Some("codex") {
            return Err(unavailable());
        }
        check_input(InputBoundary::PrepareText)?;
        let fresh = empty()?;
        let before = fresh
            .parse()
            .filter(|screen| screen.draft.is_empty())
            .ok_or_else(unavailable)?;
        if !starting
            .added(&before)
            .is_some_and(|delta| delta.rows.is_empty())
        {
            return Err(unavailable());
        }
        check_input(InputBoundary::Text)?;
        self.type_literal(t, text, true)?;
        sleep(Duration::from_millis(500));
        let window = setting("WFC_TYPED_CHECK_SECONDS", 1.5);
        self.confirm_input(t, text, &before, window, check_input)
    }

    fn confirm_input(
        &self,
        t: &Target,
        text: &str,
        before: &InputScreen<'_>,
        window: Duration,
        check_input: &mut dyn FnMut(InputBoundary) -> Reply<()>,
    ) -> Reply<Value> {
        let after_text = |_| uncertain("input was attempted but submission could not be confirmed");
        for final_check in [false, true] {
            if final_check {
                sleep(Duration::from_millis(300));
                check_input(InputBoundary::PrepareEnter)?;
            }
            let capture = self.input_capture(t).map_err(after_text)?;
            let draft_mismatch =
                || uncertain("Codex draft does not match the complete input; Enter was not sent");
            let screen = capture.parse().ok_or_else(draft_mismatch)?;
            if let Some(refusal) = before.refused(&screen) {
                return Err(refusal);
            }
            if notices(&capture.viewport).state.is_some()
                || !wfc_herdr::matches_single_line_draft(screen.draft, text)
            {
                return Err(draft_mismatch());
            }
            if !before
                .added(&screen)
                .is_some_and(|delta| delta.rows.is_empty())
            {
                return Err(uncertain(
                    "Codex visible transcript changed; Enter was not sent",
                ));
            }
        }
        check_input(InputBoundary::Enter)?;
        self.worker_command_if(
            t,
            &format!("send-keys -t {} Enter", t.worker_pane()?),
            NO_TMUX_MODE,
        )
        .map_err(after_text)?;
        let start = Instant::now();
        loop {
            sleep(
                window
                    .saturating_sub(start.elapsed())
                    .min(Duration::from_millis(250)),
            );
            let capture = self.input_capture(t).map_err(after_text)?;
            if notices(&capture.viewport).state.is_some() {
                return Err(uncertain(
                    "Codex changed state before submission was confirmed",
                ));
            }
            if let Some(after) = capture.parse() {
                if let Some(result) = appended_result(before, &after, text)? {
                    return Ok(result);
                }
            }
            if start.elapsed() >= window {
                return Err(uncertain(
                    "Codex did not show the submitted input in the transcript",
                ));
            }
        }
    }

    pub(super) fn resume_input(
        &self,
        t: &Target,
        action: ResumeInput,
        text: Option<&str>,
    ) -> Reply<Value> {
        if action == ResumeInput::SendBrief {
            let start = Instant::now();
            let mut last = String::new();
            while start.elapsed() < Duration::from_secs(30) {
                let screen = self.worker_screen(t).map_err(|_| {
                    failure(
                        "definitely-not-sent",
                        "resumed worker started but its prompt could not be observed",
                    )
                })?;
                if !screen.trim().is_empty() && screen == last {
                    return self.submit(
                        t,
                        text.unwrap_or_default(),
                        Some("opencode"),
                        false,
                        &mut |_| Ok(()),
                    );
                }
                last = screen;
                sleep(Duration::from_secs(1));
            }
            return Err(failure(
                "definitely-not-sent",
                "resumed worker screen did not become ready",
            ));
        }
        self.observe(t, 0, setting("WFC_RESUME_CHECK_SECONDS", 5.))
            .map_err(|e| {
                if e["category"] == "permission_required" {
                    e
                } else {
                    uncertain("could not observe the resumed brief")
                }
            })?;
        Ok(json!({"result":"accepted"}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_codex_composer_distinguishes_stability_from_matching_wrapped_text() {
        let draft =
            |text| codex_draft(&format!("› old transcript\n\n› {text}\n\n  model · cwd")).unwrap();
        let first = draft("hello world");
        let changed = draft("helloworld");
        assert!(matches_input(&first, "helloworld"));
        assert!(matches_input(&changed, "helloworld"));
        assert!(first != changed);
        assert!(matches_input(
            &draft("visible slice ↑"),
            "start visible slice end"
        ));
        assert!(!matches_input(
            &draft("Ask Codex to do anything"),
            "Ask Codex to do anything"
        ));
        assert!(codex_draft("› transcript alone\nother output").is_none());
    }

    #[test]
    fn read_only_state_and_stale_refusal_are_different_signals() {
        assert!(check_notice(&format!("■ {REFUSAL}"), 1).is_ok());
        assert!(check_notice(&format!("■ {REFUSAL}\n• {REFUSAL}"), 1).is_err());
        assert!(check_notice(
            "│ This chat uses A, but Codex now uses B, so this chat is read-only.",
            0
        )
        .is_ok());
        assert!(check_notice(
            "This chat uses A, but Codex now uses B,\nso this chat is read-only.",
            0
        )
        .is_err());
    }
}
