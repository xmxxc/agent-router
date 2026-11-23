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

fn composer_end(tail: &[&str]) -> (usize, usize) {
    let end = 1 + tail[1..].iter().take_while(|s| s.starts_with("  ")).count();
    let adjacent = if end == tail.len() {
        tail[1..]
            .iter()
            .rev()
            .take(2)
            .take_while(|s| s.contains(" · "))
            .count()
    } else {
        0
    };
    (end, adjacent)
}

fn footer_lines<'a>(lines: &[&'a str]) -> Option<Vec<&'a str>> {
    let footer: Vec<_> = lines.iter().copied().filter(|s| !s.is_empty()).collect();
    ((1..=2).contains(&footer.len())
        && footer
            .iter()
            .all(|s| s.starts_with("  ") && s.contains(" · ")))
    .then_some(footer)
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

fn matching_styles(plain: &Screen, tail: &[&str], styled: &Screen) -> Option<Vec<ansi::Row>> {
    // Herdr 0.9.5 always returns revision 0 for pane.read. The later read
    // supplies styles for a matching suffix on the bound pane, not a frame ID.
    let mut rows = ansi::rows(&styled.text)?;
    while rows.last().is_some_and(|row| row.text.trim().is_empty()) {
        rows.pop();
    }
    let visible = rows
        .iter()
        .map(|row| row.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if read_only(&visible) || refusals(&visible) > refusals(&plain.text) {
        return None;
    }
    let rows = rows.split_off(rows.len().checked_sub(tail.len())?);
    rows.iter()
        .zip(tail)
        .all(|(row, &line)| row.text.trim_end_matches(' ') == line)
        .then_some(rows)
}

enum ReplyBox {
    Empty,
    Draft(Option<String>),
    Unproven(&'static str),
}

fn observe_reply_box(
    plain: &Screen,
    read_ansi: impl FnOnce() -> Result<Screen>,
) -> Result<ReplyBox> {
    let lines = screen_lines(&plain.text);
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
    let Some(rows) = matching_styles(plain, tail, &read_ansi()?) else {
        return Ok(unknown);
    };
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
    if bare
        || (shows_placeholder(body) && rows[..end].iter().all(|row| !row.has_nonplaceholder_body))
    {
        Ok(ReplyBox::Empty)
    } else {
        Ok(ReplyBox::Draft(draft))
    }
}

pub(super) fn require_empty_reply_box(handle: &Handle, screen: &Screen) -> Result<()> {
    match observe_reply_box(screen, || handle.read_ansi())? {
        ReplyBox::Empty => Ok(()),
        ReplyBox::Draft(_) => Err(Failure::unsent("Codex composer already contains a draft")),
        ReplyBox::Unproven(diagnostic) => Err(Failure::unsent(diagnostic)),
    }
}

pub(super) fn confirm_codex(
    handle: &mut Handle,
    text: &str,
    before: &Screen,
    confirm_reply: bool,
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
        if confirm_reply {
            return match observe_reply_box(&screen, || handle.read_ansi())? {
                ReplyBox::Empty => Ok(None),
                ReplyBox::Draft(Some(draft)) if compact(&draft) == expected => {
                    Ok(Some((draft, true)))
                }
                _ => Err(Failure::unknown("Codex composer did not confirm the reply")),
            };
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
        if confirm_reply && first.is_none() && second.is_some() {
            return Err(Failure::unknown(
                "Codex showed the reply draft after an empty composer",
            ));
        }
        let (Some((first_draft, first_certain)), Some((second_draft, second_certain))) =
            (first, second)
        else {
            return Ok(());
        };
        if first_draft != second_draft {
            if confirm_reply {
                return Err(Failure::unknown(
                    "Codex composer changed before Enter could be retried",
                ));
            }
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
    fn view(text: &str) -> Screen {
        Screen {
            text: text.into(),
            revision: 0,
            truncated: false,
        }
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
            observe_reply_box(&flat, || Ok(view(empty))).unwrap(),
            ReplyBox::Empty
        ));
        let draft = "\x1b[48;2;0;12;32m\x1b[1m›\r\n  model · dir\x1b[0m\r\n  keys · help";
        for ansi in [
            draft,
            flat.text.as_str(),
            "› different\n  model · dir\n  keys · help",
        ] {
            assert!(matches!(
                observe_reply_box(&flat, || Ok(view(ansi))).unwrap(),
                ReplyBox::Unproven(_)
            ));
        }

        let placeholder = view("› Ask Codex to do anything\n  model · dir\n  keys · help");
        let ordinary =
            "\x1b[48;5;24m\x1b[1m› Ask Codex to do anything\x1b[0m\n  model · dir\n  keys · help";
        assert!(matches!(
            observe_reply_box(&placeholder, || Ok(view(ordinary))).unwrap(),
            ReplyBox::Draft(Some(draft)) if draft == "Ask Codex to do anything"
        ));
        let dimmed =
            "\x1b[48;5;24m\x1b[1m› \x1b[2mAsk Codex to do anything\x1b[0m\n  model · dir\n  keys · help";
        assert!(matches!(
            observe_reply_box(&placeholder, || Ok(view(dimmed))).unwrap(),
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
                matches!(
                    observe_reply_box(&flat, || Ok(ansi)).unwrap(),
                    ReplyBox::Empty
                ),
                empty
            );
        }
        assert!(matches!(
            observe_reply_box(
                &view("›\n\n  model · dir\n! echo x\n\n  model · dir"),
                || { panic!("a non-chat frame cannot supply a reply box") }
            )
            .unwrap(),
            ReplyBox::Unproven(_)
        ));
        assert!(matches!(
            observe_reply_box(&view("› \u{a0}\n\n  model · dir"), || {
                Ok(view("\x1b[1m›\x1b[0m \u{a0}\n\n  model · dir"))
            })
            .unwrap(),
            ReplyBox::Draft(_)
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
