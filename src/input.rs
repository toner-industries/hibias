//! Frontend-neutral input type.
//!
//! The terminal frontend (crossterm event stream in `main.rs`) translates
//! its raw key events into `Input` at the edge, so nothing in `app` or
//! `ui` has to know about crossterm.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Input {
    pub key: Key,
    pub mods: Mods,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Esc,
    Backspace,
    Tab,
    Left,
    Right,
    Up,
    Down,
    Other,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

impl Input {
    // Convenience constructors. Not used by the crossterm event path (which
    // builds Input via struct literal), but handy for tests and any
    // alternative frontend.
    #[allow(dead_code)]
    pub const fn new(key: Key) -> Self {
        Self { key, mods: Mods { shift: false, ctrl: false, alt: false } }
    }

    #[allow(dead_code)]
    pub const fn with_mods(key: Key, mods: Mods) -> Self {
        Self { key, mods }
    }

    pub fn is_ctrl_c(&self) -> bool {
        self.mods.ctrl && matches!(self.key, Key::Char('c'))
    }
}

/// Human-readable label for logging.
pub fn label(input: Input) -> String {
    let base = match input.key {
        Key::Char(c) => format!("'{c}'"),
        Key::Esc => "esc".into(),
        Key::Enter => "enter".into(),
        Key::Backspace => "backspace".into(),
        Key::Left => "left".into(),
        Key::Right => "right".into(),
        Key::Up => "up".into(),
        Key::Down => "down".into(),
        Key::Tab => "tab".into(),
        Key::Other => "other".into(),
    };
    let mut parts: Vec<&str> = Vec::new();
    if input.mods.ctrl {
        parts.push("CONTROL");
    }
    if input.mods.shift {
        parts.push("SHIFT");
    }
    if input.mods.alt {
        parts.push("ALT");
    }
    if parts.is_empty() {
        base
    } else {
        format!("{}+{}", parts.join("|"), base)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_basic() {
        assert_eq!(label(Input::new(Key::Char('a'))), "'a'");
        assert_eq!(label(Input::new(Key::Esc)), "esc");
        assert_eq!(label(Input::new(Key::Enter)), "enter");
    }

    #[test]
    fn label_with_ctrl() {
        let s = label(Input::with_mods(
            Key::Char('c'),
            Mods { ctrl: true, ..Default::default() },
        ));
        assert!(s.contains("CONTROL") && s.contains("'c'"), "got: {s}");
    }

    #[test]
    fn is_ctrl_c_detects_ctrl_c() {
        let i = Input::with_mods(Key::Char('c'), Mods { ctrl: true, ..Default::default() });
        assert!(i.is_ctrl_c());
        assert!(!Input::new(Key::Char('c')).is_ctrl_c());
    }
}
