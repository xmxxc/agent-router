use super::{failure, Reply, Target, Tmux};
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
        match first {
            ReplyBox::Empty => return Ok(false),
            ReplyBox::Draft(Some(shown)) if matches_single_line_draft(&shown, text) => {}
            _ => return Err(uncertain("chat composer did not confirm the reply")),
        }
        sleep(Duration::from_millis(300));
        let (screen, second) = self.reply_snapshot(t, host)?;
        check_notice(&screen, *earlier_refusals)?;
        match second {
            ReplyBox::Empty => Ok(false),
            ReplyBox::Draft(Some(shown)) if matches_single_line_draft(&shown, text) => {
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
    ) -> Reply<Value> {
        if text.contains('\0') {
            return Err(failure("definitely-not-sent", "tmux input contains NUL"));
        }
        let host_for_reply = host.unwrap_or_default();
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
        // Single-quote for the tmux command parser inside if-shell. No shell is invoked.
        let literal = text.replace('\'', "'\\''");
        send(&format!(
            "send-keys -t {} -l -- '{literal}'",
            t.worker_pane()?
        ))
        .map_err(|mut err| {
            if err["result"] == "uncertain" {
                err["category"] = json!("input_uncertain");
            }
            err
        })?;
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
        for retry in 0..=2 {
            send(&format!("send-keys -t {} Enter", t.worker_pane()?))
                .map_err(|_| uncertain("text was typed but Enter was not confirmed"))?;
            self.observe(t, earlier_refusals, window)
                .map_err(after_text)?;
            if confirm_reply {
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
                    return self.submit(t, text.unwrap_or_default(), Some("opencode"), false);
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
