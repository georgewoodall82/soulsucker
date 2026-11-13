//! A small single-line text input with cursor editing and masking.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Default)]
pub struct TextInput {
    chars: Vec<char>,
    cursor: usize,
}

impl TextInput {
    pub fn new(value: impl AsRef<str>) -> Self {
        let chars: Vec<char> = value.as_ref().chars().collect();
        let cursor = chars.len();
        Self { chars, cursor }
    }

    pub fn value(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn chars(&self) -> &[char] {
        &self.chars
    }

    #[allow(dead_code)]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Handle an editing key. Returns true when the text/cursor changed.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if alt {
            return false;
        }
        if ctrl {
            match key.code {
                KeyCode::Char('a') => {
                    self.cursor = 0;
                    return true;
                }
                KeyCode::Char('e') => {
                    self.cursor = self.chars.len();
                    return true;
                }
                KeyCode::Char('u') => {
                    self.chars.drain(..self.cursor);
                    self.cursor = 0;
                    return true;
                }
                KeyCode::Char('k') => {
                    self.chars.truncate(self.cursor);
                    return true;
                }
                KeyCode::Char('w') => {
                    while self.cursor > 0 && self.chars[self.cursor - 1].is_whitespace() {
                        self.chars.remove(self.cursor - 1);
                        self.cursor -= 1;
                    }
                    while self.cursor > 0 && !self.chars[self.cursor - 1].is_whitespace() {
                        self.chars.remove(self.cursor - 1);
                        self.cursor -= 1;
                    }
                    return true;
                }
                _ => return false,
            }
        }

        match key.code {
            KeyCode::Char(ch) => {
                self.chars.insert(self.cursor, ch);
                self.cursor += 1;
                true
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.chars.remove(self.cursor - 1);
                    self.cursor -= 1;
                    true
                } else {
                    false
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.chars.len() {
                    self.chars.remove(self.cursor);
                    true
                } else {
                    false
                }
            }
            KeyCode::Left => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    true
                } else {
                    false
                }
            }
            KeyCode::Right => {
                if self.cursor < self.chars.len() {
                    self.cursor += 1;
                    true
                } else {
                    false
                }
            }
            KeyCode::Home => {
                self.cursor = 0;
                true
            }
            KeyCode::End => {
                self.cursor = self.chars.len();
                true
            }
            _ => false,
        }
    }

    /// Characters before the cursor, masked for secret fields.
    pub fn text_before_cursor(&self, mask: bool) -> String {
        if mask {
            "•".repeat(self.cursor)
        } else {
            self.chars.iter().take(self.cursor).collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn inserts_and_moves_the_cursor() {
        let mut input = TextInput::new("ac");
        input.handle_key(key(KeyCode::Left));
        input.handle_key(key(KeyCode::Char('b')));
        assert_eq!(input.value(), "abc");
        assert_eq!(input.cursor(), 2);
        input.handle_key(key(KeyCode::Backspace));
        assert_eq!(input.value(), "ac");
    }

    #[test]
    fn delete_and_home_end_work() {
        let mut input = TextInput::new("hello");
        input.handle_key(key(KeyCode::Home));
        input.handle_key(key(KeyCode::Delete));
        assert_eq!(input.value(), "ello");
        input.handle_key(key(KeyCode::End));
        input.handle_key(key(KeyCode::Char('!')));
        assert_eq!(input.value(), "ello!");
    }

    #[test]
    fn ctrl_w_removes_a_word() {
        let mut input = TextInput::new("hello world");
        input.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(input.value(), "hello ");
    }

    #[test]
    fn masking_uses_one_bullet_per_character() {
        let mut input = TextInput::new("secret");
        input.handle_key(key(KeyCode::Left));
        assert_eq!(input.text_before_cursor(true), "•••••");
    }
}
