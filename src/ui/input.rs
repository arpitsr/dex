use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A small UTF-8-aware multiline editor used by the terminal UI.
pub(crate) struct InputField {
    pub(super) lines: Vec<String>,
    pub(super) row: usize,
    pub(super) col: usize,
}

impl InputField {
    pub(super) fn new() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
        }
    }

    pub(super) fn from_text(text: &str) -> Self {
        let lines: Vec<String> = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(str::to_string).collect()
        };
        let row = lines.len().saturating_sub(1);
        let col = lines[row].len();
        Self { lines, row, col }
    }

    pub(super) fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub(super) fn reset(&mut self) {
        self.lines = vec![String::new()];
        self.row = 0;
        self.col = 0;
    }

    pub(super) fn insert_char(&mut self, c: char) {
        if c == '\n' {
            let line = std::mem::take(&mut self.lines[self.row]);
            let (left, right) = line.split_at(self.col);
            self.lines.insert(self.row + 1, right.to_string());
            self.lines[self.row] = left.to_string();
            self.row += 1;
            self.col = 0;
            return;
        }
        if self.col > self.lines[self.row].len() {
            self.col = self.lines[self.row].len();
        }
        self.lines[self.row].insert(self.col, c);
        self.col += c.len_utf8();
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char(c)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                if !c.is_control() {
                    self.insert_char(c);
                }
            }
            KeyCode::Char(_) => {}
            KeyCode::Enter => self.insert_char('\n'),
            KeyCode::Backspace => {
                if self.col == 0 {
                    if self.row > 0 {
                        let removed = self.lines.remove(self.row);
                        self.row -= 1;
                        self.col = self.lines[self.row].len();
                        self.lines[self.row].push_str(&removed);
                    }
                } else {
                    let line = &mut self.lines[self.row];
                    let mut idx = self.col;
                    while idx > 0 && !line.is_char_boundary(idx - 1) {
                        idx -= 1;
                    }
                    line.remove(idx - 1);
                    self.col = idx - 1;
                }
            }
            KeyCode::Delete => {
                let line = &mut self.lines[self.row];
                if self.col < line.len() {
                    let mut idx = self.col;
                    while idx < line.len() && !line.is_char_boundary(idx + 1) {
                        idx += 1;
                    }
                    line.remove(idx);
                } else if self.row + 1 < self.lines.len() {
                    let removed = self.lines.remove(self.row + 1);
                    self.lines[self.row].push_str(&removed);
                }
            }
            KeyCode::Left => {
                if self.col > 0 {
                    let line = &self.lines[self.row];
                    let mut idx = self.col;
                    while idx > 0 && !line.is_char_boundary(idx - 1) {
                        idx -= 1;
                    }
                    self.col = idx - 1;
                } else if self.row > 0 {
                    self.row -= 1;
                    self.col = self.lines[self.row].len();
                }
            }
            KeyCode::Right => {
                let line = &self.lines[self.row];
                if self.col < line.len() {
                    let mut idx = self.col;
                    while idx < line.len() && !line.is_char_boundary(idx + 1) {
                        idx += 1;
                    }
                    self.col = idx + 1;
                } else if self.row + 1 < self.lines.len() {
                    self.row += 1;
                    self.col = 0;
                }
            }
            KeyCode::Up if self.row > 0 => {
                self.row -= 1;
                self.clamp_col();
            }
            KeyCode::Down if self.row + 1 < self.lines.len() => {
                self.row += 1;
                self.clamp_col();
            }
            KeyCode::Home => self.col = 0,
            KeyCode::End => self.col = self.lines[self.row].len(),
            KeyCode::Tab => self.insert_char('\t'),
            _ => {}
        }
    }

    fn clamp_col(&mut self) {
        let max = self.lines[self.row].len();
        self.col = self.col.min(max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEventKind, KeyEventState};

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    #[test]
    fn alt_and_ctrl_modified_chars_are_dropped() {
        // Correct behavior: Alt/Ctrl-modified chars must never land in the
        // composer. `\x1b]10;rgb:…\x07` is parsed by crossterm as Alt+`]` +
        // plain `10;rgb:…` + Ctrl-G. The Alt/Ctrl wrappers are dropped here;
        // the plain middle is consumed at the source by the timed drain in
        // `remote.rs` before the event loop starts, so it never reaches
        // `handle_key`. This test verifies the input guard only.
        let mut f = InputField::new();
        f.handle_key(key(KeyCode::Char(']'), KeyModifiers::ALT));
        assert_eq!(f.text(), "", "Alt+] must be dropped");
        f.handle_key(key(KeyCode::Char('g'), KeyModifiers::CONTROL));
        assert_eq!(f.text(), "", "Ctrl-G (BEL) must be dropped");
        f.handle_key(key(KeyCode::Char('\\'), KeyModifiers::ALT));
        assert_eq!(f.text(), "", "Alt+\\ (ST) must be dropped");
        // Plain burst would reach input only if drain failed — input itself
        // correctly inserts plain, drain is the source fix.
        for c in "10;rgb:f6f6/dcdc/acac".chars() {
            f.handle_key(key(KeyCode::Char(c), KeyModifiers::empty()));
        }
        assert!(f.text().contains("10;rgb:"), "plain is inserted when it reaches input");
    }

    #[test]
    fn normal_typing_still_works() {
        let mut f = InputField::new();
        for c in "hello".chars() {
            f.handle_key(key(KeyCode::Char(c), KeyModifiers::empty()));
        }
        assert_eq!(f.text(), "hello");
        // Shift+letter (uppercase) must still insert.
        f.handle_key(key(KeyCode::Char('W'), KeyModifiers::SHIFT));
        assert_eq!(f.text(), "helloW");
        // Ctrl+C must not insert.
        f.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(f.text(), "helloW");
    }
}
