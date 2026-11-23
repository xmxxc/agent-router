//! Cell styles in Herdr 0.9.5's ANSI pane snapshot. Ghostty emits standalone
//! SGR sequences for a styled selection, without screen or cursor extras.

#[derive(Default)]
struct Style {
    background: bool,
    dim: bool,
    reverse: bool,
    hidden: bool,
}

impl Style {
    fn sgr(&mut self, params: &str) -> Option<()> {
        let mut fields = params.split(';');
        let code = fields.next()?;
        if let Some(mode) = fields.next() {
            if !matches!(code, "38" | "48" | "58") {
                return None;
            }
            let components = match mode {
                "2" => 3,
                "5" => 1,
                _ => return None,
            };
            for _ in 0..components {
                fields.next()?.parse::<u8>().ok()?;
            }
            if fields.next().is_some() {
                return None;
            }
            self.background |= code == "48";
            return Some(());
        }

        match code {
            "0" => *self = Self::default(),
            "2" => self.dim = true,
            "7" => self.reverse = true,
            "8" => self.hidden = true,
            "1" | "3" | "4" | "4:2" | "4:3" | "4:4" | "4:5" | "5" | "9" | "53" => {}
            _ => return None,
        }
        Some(())
    }
}

#[derive(Default)]
pub(super) struct Row {
    pub text: String,
    pub prompt_background: bool,
    pub has_nondefault_background: bool,
    pub has_undimmed_body: bool,
}

pub(super) fn rows(input: &str) -> Option<Vec<Row>> {
    let mut style = Style::default();
    input
        .lines()
        .map(|line| {
            let mut row = Row::default();
            for (index, span) in line.split('\x1b').enumerate() {
                let text = if index == 0 {
                    span
                } else {
                    let (params, text) = span.strip_prefix('[')?.split_once('m')?;
                    style.sgr(params)?;
                    text
                };
                if text.chars().any(char::is_control) {
                    return None;
                }
                let prompt = row.text.is_empty() && text.starts_with('›');
                if prompt {
                    row.prompt_background = style.background && !style.reverse && !style.hidden;
                }
                if text.chars().any(|c| c != ' ') {
                    row.has_nondefault_background |=
                        style.background || style.reverse || style.hidden;
                }
                let body = if prompt {
                    text.strip_prefix('›')?
                } else {
                    text
                };
                if !style.dim && body.chars().any(|c| c != ' ') {
                    row.has_undimmed_body = true;
                }
                row.text.push_str(text);
            }
            Some(row)
        })
        .collect()
}
