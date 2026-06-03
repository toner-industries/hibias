#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ModeMask(u8);

impl ModeMask {
    // Tabs
    pub const NOW_PLAYING: ModeMask = ModeMask(1 << 0);
    pub const SEARCH: ModeMask = ModeMask(1 << 1);
    pub const LIBRARY: ModeMask = ModeMask(1 << 2);
    // Overlays
    pub const HELP: ModeMask = ModeMask(1 << 3);
    pub const COMMAND: ModeMask = ModeMask(1 << 4);
    pub const BROWSE: ModeMask = ModeMask(1 << 5);
    pub const DEVICES: ModeMask = ModeMask(1 << 6);
    pub const ANY: ModeMask = ModeMask(0b1111111);

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

// Footer entries, filtered per mode. Labels are tuned so every mode's footer
// fits inside the 94-col inner width of the fixed 96-col canvas. Wide unicode
// characters (left/right/up/down arrows) each occupy two terminal columns
// despite reading as one glyph, so leave breathing room when adding entries.
// These mirror the per-screen footers in design/mockups.html.
pub const HOTKEYS: &[Hotkey] = &[
    // Now Playing
    Hotkey { key: "space",  action: "play/pause", modes: ModeMask::NOW_PLAYING },
    Hotkey { key: "l",      action: "library",    modes: ModeMask::NOW_PLAYING },
    // Search
    Hotkey { key: "enter",  action: "play/open",  modes: ModeMask::SEARCH },
    Hotkey { key: "tab",    action: "switch tab", modes: ModeMask::SEARCH.or(ModeMask::LIBRARY) },
    // Library
    Hotkey { key: "←/→",    action: "section",    modes: ModeMask::LIBRARY },
    Hotkey { key: "enter",  action: "open",       modes: ModeMask::LIBRARY },
    // Browse
    Hotkey { key: "enter",  action: "play",       modes: ModeMask::BROWSE },
    Hotkey { key: "p",      action: "play all",   modes: ModeMask::BROWSE },
    // Devices
    Hotkey { key: "enter",  action: "transfer",   modes: ModeMask::DEVICES },
    // Command
    Hotkey { key: "enter",  action: "run",        modes: ModeMask::COMMAND },
    // Movement, shared by the list-based surfaces
    Hotkey { key: "↑/↓",    action: "move",       modes: ModeMask::SEARCH.or(ModeMask::COMMAND).or(ModeMask::BROWSE).or(ModeMask::LIBRARY).or(ModeMask::DEVICES) },
    // Search / commands / help reachable from the tabs
    Hotkey { key: "/",      action: "search",     modes: ModeMask::NOW_PLAYING.or(ModeMask::LIBRARY).or(ModeMask::BROWSE) },
    Hotkey { key: ":",      action: "commands",   modes: ModeMask::NOW_PLAYING },
    Hotkey { key: "?",      action: "help",       modes: ModeMask::NOW_PLAYING.or(ModeMask::SEARCH) },
    Hotkey { key: "q",      action: "quit",       modes: ModeMask::NOW_PLAYING },
    Hotkey { key: "esc",    action: "back",       modes: ModeMask::SEARCH.or(ModeMask::HELP).or(ModeMask::COMMAND).or(ModeMask::BROWSE).or(ModeMask::LIBRARY).or(ModeMask::DEVICES) },
    Hotkey { key: "ctrl-c", action: "quit",       modes: ModeMask::ANY },
];

pub fn for_mode(mask: ModeMask) -> impl Iterator<Item = &'static Hotkey> {
    HOTKEYS.iter().filter(move |h| h.modes.contains(mask))
}

/// Curated hotkey reference for the help overlay — richer than any single
/// mode's footer, matching the help screen in design/mockups.html.
pub const HELP_ROWS: &[(&str, &str)] = &[
    ("space", "play / pause"),
    ("shift ←/→", "seek ±10s"),
    ("/", "search"),
    (":", "commands"),
    ("l", "library"),
    ("d", "devices"),
    ("?", "help"),
    ("tab", "switch tab"),
    ("↑/↓", "move"),
    ("enter", "play / open"),
    ("esc", "back"),
    ("ctrl-c", "quit"),
];
