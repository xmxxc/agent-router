use super::{Failure, Handle, Result, Screen};
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

fn codex_box(screen: &Screen) -> Option<String> {
    if screen.truncated {
        return None;
    }
    let lines: Vec<_> = screen.text.trim_end().lines().map(str::trim_end).collect();
    let start = lines
        .iter()
        .rposition(|s| *s == "›" || s.starts_with("› "))?;
    let mut end = start + 1;
    while end < lines.len() && lines[end].starts_with("  ") {
        end += 1;
    }
    if end == lines.len() {
        while end > start + 1 && lines[end - 1].contains(" · ") {
            end -= 1;
        }
    }
    let footer: Vec<_> = lines[end..].iter().filter(|s| !s.is_empty()).collect();
    if !(1..=2).contains(&footer.len())
        || !footer
            .iter()
            .all(|s| s.starts_with("  ") && s.contains(" · "))
        || lines[start..end].iter().any(|s| s.ends_with(['↑', '↓']))
    {
        return None;
    }
    let text = lines[start..end].join(" ");
    let draft = text.strip_prefix('›')?.trim();
    if draft.is_empty() || compact(draft) == "AskCodextodoanything" {
        return None;
    }
    Some(draft.into())
}

pub(super) fn confirm_codex(handle: &Handle, text: &str, before: &Screen) -> Result<()> {
    let sample = || {
        sleep(Duration::from_millis(300));
        let screen = handle.read()?;
        if read_only(&screen.text) || refusals(&screen.text) > refusals(&before.text) {
            return Err(Failure::unknown("worker refused or paused input"));
        }
        Ok(codex_box(&screen))
    };
    for retry in 0..=2 {
        let first = sample()?;
        let second = sample()?;
        if first != second
            || second
                .as_deref()
                .is_none_or(|draft| compact(draft) != compact(text))
        {
            return Ok(());
        }
        if retry == 2 {
            return Err(Failure::unknown("Codex still shows the submitted draft"));
        }
        handle.send(
            "pane.send_keys",
            json!({"pane_id":handle.pane,"keys":["Enter"]}),
            true,
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
    pub fn activate_opencode_prompt(&self, text: &str) -> Result<()> {
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
    #[test]
    fn only_a_complete_current_composer_matches() {
        let screen = |s: &str| Screen {
            text: s.into(),
            revision: 1,
            truncated: false,
        };
        assert_eq!(
            codex_box(&screen("› hello\n  世界\n\n  model · dir")).as_deref(),
            Some("hello   世界")
        );
        for s in [
            "› hello\n  … ↑\n\n  model · dir",
            "› hello\n  … ↓\n\n  model · dir",
            "› hello\ntranscript\n  model · dir",
            "› Ask Codex to do anything\n\n  model · dir",
        ] {
            assert_eq!(codex_box(&screen(s)), None);
        }
        assert_eq!(
            opencode_box("┃ hello attempt-01\n┃ agent · model\n╹▀▀▀").as_deref(),
            Some("hello attempt-01")
        );
        assert!(read_only("■ This conversation is unavailable. Its cached transcript and\ndraft remain here; input is paused."));
    }
}
