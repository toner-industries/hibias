#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ModeMask(u8);

impl ModeMask {
    pub const NOW_PLAYING: ModeMask = ModeMask(1 << 0);
    pub const SEARCH: ModeMask = ModeMask(1 << 1);
    pub const HELP: ModeMask = ModeMask(1 << 2);
    pub const ANY: ModeMask = ModeMask(0b111);

    pub const fn or(self, other: ModeMask) -> ModeMask {
        ModeMask(self.0 | other.0)
    }

    pub fn contains(self, other: ModeMask) -> bool {
        self.0 & other.0 != 0
    }
}

pub struct Hotkey {
    pub key: &'static str,
    pub action: &'static str,
    pub modes: ModeMask,
}

pub const HOTKEYS: &[Hotkey] = &[
    Hotkey { key: "space",  action: "play / pause",    modes: ModeMask::NOW_PLAYING },
    Hotkey { key: "/",      action: "search",          modes: ModeMask::NOW_PLAYING },
    Hotkey { key: "?",      action: "help",            modes: ModeMask::NOW_PLAYING },
    Hotkey { key: "↑ / ↓",  action: "move selection",  modes: ModeMask::SEARCH },
    Hotkey { key: "enter",  action: "play selection",  modes: ModeMask::SEARCH },
    Hotkey { key: "esc",    action: "close / back",    modes: ModeMask::SEARCH.or(ModeMask::HELP) },
    Hotkey { key: "q",      action: "quit",            modes: ModeMask::NOW_PLAYING },
    Hotkey { key: "ctrl-c", action: "quit",            modes: ModeMask::ANY },
];

pub fn for_mode(mask: ModeMask) -> impl Iterator<Item = &'static Hotkey> {
    HOTKEYS.iter().filter(move |h| h.modes.contains(mask))
}
