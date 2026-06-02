#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ModeMask(u8);

impl ModeMask {
    pub const NOW_PLAYING: ModeMask = ModeMask(1 << 0);
    pub const SEARCH: ModeMask = ModeMask(1 << 1);
    pub const HELP: ModeMask = ModeMask(1 << 2);
    pub const COMMAND: ModeMask = ModeMask(1 << 3);
    pub const BROWSE: ModeMask = ModeMask(1 << 4);
    pub const ANY: ModeMask = ModeMask(0b11111);

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

// Labels are tuned so every mode's footer fits inside the 94-col inner
// width of the fixed 96-col canvas. Wide unicode characters (←, →, ±, ↑, ↓)
// each occupy two terminal columns despite reading as one glyph, so leave
// breathing room when adding entries.
pub const HOTKEYS: &[Hotkey] = &[
    Hotkey { key: "space",      action: "play/pause",      modes: ModeMask::NOW_PLAYING },
    Hotkey { key: "shift ←/→",  action: "seek",            modes: ModeMask::NOW_PLAYING },
    Hotkey { key: "/",          action: "search",          modes: ModeMask::NOW_PLAYING },
    Hotkey { key: ":",          action: "commands",        modes: ModeMask::NOW_PLAYING },
    Hotkey { key: "?",          action: "help",            modes: ModeMask::NOW_PLAYING },
    Hotkey { key: "↑/↓",        action: "move",            modes: ModeMask::SEARCH.or(ModeMask::COMMAND).or(ModeMask::BROWSE) },
    Hotkey { key: "enter",      action: "play",            modes: ModeMask::SEARCH },
    Hotkey { key: "enter",      action: "play",            modes: ModeMask::BROWSE },
    Hotkey { key: "p",          action: "play all",        modes: ModeMask::BROWSE },
    Hotkey { key: "enter",      action: "run",             modes: ModeMask::COMMAND },
    Hotkey { key: "esc",        action: "back",            modes: ModeMask::SEARCH.or(ModeMask::HELP).or(ModeMask::COMMAND).or(ModeMask::BROWSE) },
    Hotkey { key: "q",          action: "quit",            modes: ModeMask::NOW_PLAYING },
    Hotkey { key: "ctrl-c",     action: "quit",            modes: ModeMask::ANY },
];

pub fn for_mode(mask: ModeMask) -> impl Iterator<Item = &'static Hotkey> {
    HOTKEYS.iter().filter(move |h| h.modes.contains(mask))
}
