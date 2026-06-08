//! Key dispatch: the `KeyAction` intent enum and the pure, synchronous
//! `dispatch_input` routing (global keys -> overlay -> active tab) plus the
//! search/selection resolvers. No I/O — the run loop performs the async work.

use super::*;
use crate::input::{Input, Key};

#[derive(Debug)]
pub enum KeyAction {
    Stay,
    Quit,
    TogglePlayback,
    EnterSearch,
    SearchInputChanged,
    PlaySelection,
    /// Open the Browse mode loaded with the given album or playlist.
    OpenBrowse(Collection),
    /// Play the currently selected track inside the active Browse view.
    PlayBrowseSelection,
    /// Play the active Browse collection from the start, ignoring the
    /// per-track selection. Used when the tracks endpoint is 403'd —
    /// the user can still kick off playback of the whole album/playlist.
    PlayBrowseCollection,
    /// Seek the current track by ±N milliseconds.
    Seek(i64),
    NextTrack,
    PrevTrack,
    Reconnect,
    LikeCurrent,
    /// Focus the Library tab and lazily load its active sub-tab.
    OpenLibrary,
    /// Play the selected track in the Library "Liked" sub-tab.
    PlayLibrarySelection,
    /// Open the device picker overlay and fetch the device list once.
    OpenDevices,
    /// Transfer playback to the given Connect device id (from the picker).
    TransferToDevice(String),
}

const SEEK_STEP_MS: i64 = 10_000;

/// What hitting Enter on the current selection means.
#[derive(Debug)]
pub enum SelectionAction {
    /// Re-run search with this query (selected a row from "Recent searches").
    PromoteQuery(String),
    Play(PlayAction),
    /// Open an album or playlist to browse its tracks.
    Browse(Collection),
}

#[derive(Debug)]
pub enum PlayAction {
    Track(String),
    Context { uri: String, offset: Option<String> },
}

pub async fn dispatch_input(input: Input, state: &Mutex<AppState>) -> KeyAction {
    let mut s = state.lock().await;
    let shift = input.mods.shift;

    // Tab/Shift-Tab always cycle the top tabs — they're not printable, so even
    // a text field (Search, Command) can't legitimately want them as input.
    // A `tab` press lands you *in* the content of the new tab.
    if matches!(input.key, Key::Tab) {
        s.tab = if shift { s.tab.prev() } else { s.tab.next() };
        s.overlay = None;
        s.focus = Focus::Content;
        return tab_entry_action(s.tab);
    }

    // 1. Global launcher keys. These fire regardless of which tab/overlay is
    //    active, EXCEPT when a text field is capturing characters (Search tab,
    //    Command palette) — there, the printable `/ : ? d l` are literal input.
    if !is_capturing_text(&s) {
        match input.key {
            Key::Char('/') => {
                s.overlay = None;
                s.focus = Focus::Content;
                return KeyAction::EnterSearch;
            }
            Key::Char(':') => {
                s.overlay = Some(Overlay::Command(CommandState::default()));
                return KeyAction::Stay;
            }
            Key::Char('?') => {
                s.overlay = Some(Overlay::Help);
                return KeyAction::Stay;
            }
            Key::Char('d') => {
                return KeyAction::OpenDevices;
            }
            Key::Char('l') => {
                s.tab = Tab::Library;
                s.overlay = None;
                s.focus = Focus::Content;
                return KeyAction::OpenLibrary;
            }
            _ => {}
        }
    }

    // 2. An open overlay consumes everything else.
    if s.overlay.is_some() {
        return dispatch_overlay(&mut s, input);
    }

    // 2.5 The top tab strip is itself a focusable row, reached by arrowing up
    //     past the first content item. While focused, left/right switch tabs.
    if s.focus == Focus::Tabs {
        return dispatch_tabs_focus(&mut s, input);
    }

    // 3. The active tab handles the rest.
    match s.tab {
        Tab::NowPlaying => dispatch_now_playing(&mut s, input, shift),
        Tab::Search => dispatch_search(&mut s, input),
        Tab::Library => dispatch_library(&mut s, input),
    }
}

/// Keys while the top tab strip is focused. Left/right pick a tab (loading its
/// content live underneath), down/enter drop into that content, esc backs into
/// the content without changing tabs.
fn dispatch_tabs_focus(s: &mut AppState, input: Input) -> KeyAction {
    match input.key {
        Key::Left => {
            s.tab = s.tab.prev();
            tab_entry_action(s.tab)
        }
        Key::Right => {
            s.tab = s.tab.next();
            tab_entry_action(s.tab)
        }
        Key::Down | Key::Enter => {
            s.focus = Focus::Content;
            tab_entry_action(s.tab)
        }
        Key::Esc => {
            s.focus = Focus::Content;
            KeyAction::Stay
        }
        _ => KeyAction::Stay,
    }
}

/// What to do right after landing on a tab via `tab`/`shift+tab`: Search
/// re-seeds recents, Library lazy-loads its sub-tab, Now Playing is inert.
fn tab_entry_action(tab: Tab) -> KeyAction {
    match tab {
        // Now Playing's queue refresh is driven by the run loop's
        // visibility transition (see `now_playing_visible`), not an action.
        Tab::NowPlaying => KeyAction::Stay,
        Tab::Search => KeyAction::EnterSearch,
        Tab::Library => KeyAction::OpenLibrary,
    }
}

fn dispatch_now_playing(s: &mut AppState, input: Input, shift: bool) -> KeyAction {
    match input.key {
        Key::Char('q') | Key::Esc => KeyAction::Quit,
        Key::Char(' ') => KeyAction::TogglePlayback,
        Key::Left if shift => KeyAction::Seek(-SEEK_STEP_MS),
        Key::Right if shift => KeyAction::Seek(SEEK_STEP_MS),
        // Now Playing has no list, so Up always rises to the tab strip.
        Key::Up => {
            s.focus = Focus::Tabs;
            KeyAction::Stay
        }
        _ => KeyAction::Stay,
    }
}

fn dispatch_search(s: &mut AppState, input: Input) -> KeyAction {
    let search = &mut s.search;
    match input.key {
        Key::Esc => {
            if let Some(h) = search.debounce.take() {
                h.abort();
            }
            s.tab = Tab::NowPlaying;
            KeyAction::Stay
        }
        Key::Up => {
            if search.selected > 0 {
                search.selected -= 1;
            } else {
                // Above the first result — rise to the tab strip.
                s.focus = Focus::Tabs;
            }
            KeyAction::Stay
        }
        Key::Down => {
            let max = visible_row_count(search).saturating_sub(1);
            if search.selected < max {
                search.selected += 1;
            }
            KeyAction::Stay
        }
        Key::Enter => match resolve_full_selection(search) {
            Some(SelectionAction::PromoteQuery(q)) => {
                search.input = q.clone();
                search.cursor = q.chars().count();
                refilter_in_context(search);
                KeyAction::SearchInputChanged
            }
            Some(SelectionAction::Play(_)) => KeyAction::PlaySelection,
            Some(SelectionAction::Browse(coll)) => KeyAction::OpenBrowse(coll),
            None => KeyAction::Stay,
        },
        Key::Backspace => {
            if search.cursor > 0 {
                let byte = char_idx_to_byte(&search.input, search.cursor - 1);
                search.input.remove(byte);
                search.cursor -= 1;
                refilter_in_context(search);
                KeyAction::SearchInputChanged
            } else {
                KeyAction::Stay
            }
        }
        Key::Left => {
            if search.cursor > 0 {
                search.cursor -= 1;
            }
            KeyAction::Stay
        }
        Key::Right => {
            let max = search.input.chars().count();
            if search.cursor < max {
                search.cursor += 1;
            }
            KeyAction::Stay
        }
        Key::Char(c) => {
            let byte = char_idx_to_byte(&search.input, search.cursor);
            search.input.insert(byte, c);
            search.cursor += 1;
            refilter_in_context(search);
            KeyAction::SearchInputChanged
        }
        _ => KeyAction::Stay,
    }
}

fn dispatch_library(s: &mut AppState, input: Input) -> KeyAction {
    match input.key {
        Key::Esc => {
            s.tab = Tab::NowPlaying;
            KeyAction::Stay
        }
        // Left/Right cycle the sub-tabs (global `tab` is reserved for the top
        // tabs). Landing on a sub-tab lazy-loads it.
        Key::Left | Key::Right => {
            s.library.tab = if matches!(input.key, Key::Right) {
                s.library.tab.next()
            } else {
                s.library.tab.prev()
            };
            s.library.selected = 0;
            KeyAction::OpenLibrary
        }
        Key::Up => {
            if s.library.selected > 0 {
                s.library.selected -= 1;
            } else {
                // Above the first row — rise to the tab strip.
                s.focus = Focus::Tabs;
            }
            KeyAction::Stay
        }
        Key::Down => {
            let max = s.library.row_count().saturating_sub(1);
            if s.library.selected < max {
                s.library.selected += 1;
            }
            KeyAction::Stay
        }
        Key::Enter => library_enter_action(&s.library),
        _ => KeyAction::Stay,
    }
}

/// Resolve what Enter does on the selected Library row: open a collection to
/// browse (playlists/albums), or play a track/artist directly.
fn library_enter_action(lib: &LibraryState) -> KeyAction {
    match lib.tab {
        LibraryTab::Liked => match lib
            .liked
            .items
            .get(lib.selected)
            .and_then(|t| t.uri.clone())
        {
            Some(_) => KeyAction::PlayLibrarySelection,
            None => KeyAction::Stay,
        },
        LibraryTab::Playlists => match lib.playlists.items.get(lib.selected) {
            Some(p) => KeyAction::OpenBrowse(Collection {
                kind: CollectionKind::Playlist,
                uri: p.uri.clone(),
                name: p.name.clone(),
                subtitle: p
                    .owner
                    .as_ref()
                    .and_then(|o| o.display_name.clone())
                    .unwrap_or_default(),
            }),
            None => KeyAction::Stay,
        },
        LibraryTab::Albums => match lib.albums.items.get(lib.selected) {
            Some(a) => match a.uri.clone() {
                Some(uri) => KeyAction::OpenBrowse(Collection {
                    kind: CollectionKind::Album,
                    uri,
                    name: a.name.clone(),
                    subtitle: a
                        .artists
                        .iter()
                        .map(|x| x.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                }),
                None => KeyAction::Stay,
            },
            None => KeyAction::Stay,
        },
        // Playing an artist's context isn't wired through the synth path; for
        // now selecting an artist is inert (artists list is name-only in v1).
        LibraryTab::Artists => KeyAction::Stay,
    }
}

fn dispatch_overlay(s: &mut AppState, input: Input) -> KeyAction {
    match s.overlay.as_mut() {
        Some(Overlay::Help) => {
            if matches!(input.key, Key::Esc | Key::Char('?') | Key::Char('q')) {
                s.overlay = None;
            }
            KeyAction::Stay
        }
        Some(Overlay::Command(_)) => dispatch_command(s, input),
        Some(Overlay::Devices(dev)) => match input.key {
            Key::Esc => {
                s.overlay = None;
                KeyAction::Stay
            }
            Key::Up => {
                if dev.selected > 0 {
                    dev.selected -= 1;
                }
                KeyAction::Stay
            }
            Key::Down => {
                let max = dev.devices.len().saturating_sub(1);
                if dev.selected < max {
                    dev.selected += 1;
                }
                KeyAction::Stay
            }
            Key::Enter => match dev.devices.get(dev.selected).and_then(|d| d.id.clone()) {
                Some(id) => KeyAction::TransferToDevice(id),
                None => KeyAction::Stay,
            },
            _ => KeyAction::Stay,
        },
        Some(Overlay::Browse(browse)) => match input.key {
            Key::Esc => {
                s.overlay = None;
                KeyAction::Stay
            }
            Key::Up => {
                if browse.selected > 0 {
                    browse.selected -= 1;
                }
                KeyAction::Stay
            }
            Key::Down => {
                let max = browse.tracks.len().saturating_sub(1);
                if browse.selected < max {
                    browse.selected += 1;
                }
                KeyAction::Stay
            }
            Key::Enter => {
                if browse.tracks.is_empty() {
                    KeyAction::Stay
                } else {
                    KeyAction::PlayBrowseSelection
                }
            }
            // "Play the whole album/playlist" — works even if the track list
            // failed to load (403 fallback).
            Key::Char('p') => KeyAction::PlayBrowseCollection,
            _ => KeyAction::Stay,
        },
        None => KeyAction::Stay,
    }
}

fn dispatch_command(s: &mut AppState, input: Input) -> KeyAction {
    let Some(Overlay::Command(cmd)) = s.overlay.as_mut() else {
        return KeyAction::Stay;
    };
    match input.key {
        Key::Esc => {
            s.overlay = None;
            KeyAction::Stay
        }
        Key::Up => {
            if cmd.selected > 0 {
                cmd.selected -= 1;
            }
            KeyAction::Stay
        }
        Key::Down => {
            let max = cmd.filtered().len().saturating_sub(1);
            if cmd.selected < max {
                cmd.selected += 1;
            }
            KeyAction::Stay
        }
        Key::Left => {
            if cmd.cursor > 0 {
                cmd.cursor -= 1;
            }
            KeyAction::Stay
        }
        Key::Right => {
            let max = cmd.input.chars().count();
            if cmd.cursor < max {
                cmd.cursor += 1;
            }
            KeyAction::Stay
        }
        Key::Backspace => {
            if cmd.cursor > 0 {
                let byte = char_idx_to_byte(&cmd.input, cmd.cursor - 1);
                cmd.input.remove(byte);
                cmd.cursor -= 1;
                cmd.selected = 0;
            }
            KeyAction::Stay
        }
        Key::Char(c) => {
            let byte = char_idx_to_byte(&cmd.input, cmd.cursor);
            cmd.input.insert(byte, c);
            cmd.cursor += 1;
            cmd.selected = 0;
            KeyAction::Stay
        }
        Key::Enter => {
            let Some(chosen) = cmd.selected_cmd() else {
                return KeyAction::Stay;
            };
            // Most commands close the palette first, then the run loop runs
            // the action. Help re-opens as an overlay instead.
            s.overlay = None;
            match chosen {
                Cmd::PlayPause => KeyAction::TogglePlayback,
                Cmd::Next => KeyAction::NextTrack,
                Cmd::Previous => KeyAction::PrevTrack,
                Cmd::Like => KeyAction::LikeCurrent,
                Cmd::Reconnect => KeyAction::Reconnect,
                Cmd::Search => KeyAction::EnterSearch,
                Cmd::Help => {
                    s.overlay = Some(Overlay::Help);
                    KeyAction::Stay
                }
                Cmd::Quit => KeyAction::Quit,
            }
        }
        _ => KeyAction::Stay,
    }
}

/// The number of selectable rows in the Search tab. Single source of truth for
/// two consumers that MUST agree: the Down-key selection clamp here in the
/// dispatcher, and `ui::render_search_tab` (count display + scroll math). If
/// they disagree, the selection can point at a row the renderer never draws.
pub fn visible_row_count(s: &SearchState) -> usize {
    if s.showing_recents() {
        return s.recent_queries.len() + s.recent_tracks.len();
    }
    let mut n = 0;
    if let Some(c) = &s.in_context {
        n += c.filtered.len();
    }
    n += s.results.tracks.len();
    n += s.results.albums.len();
    n += s.results.artists.len();
    n += s.results.playlists.len();
    n
}

pub fn refilter_in_context(s: &mut SearchState) {
    let Some(ctx) = &mut s.in_context else {
        return;
    };
    let q = s.input.to_lowercase();
    ctx.filtered = if q.is_empty() {
        Vec::new()
    } else {
        ctx.tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| {
                t.name.to_lowercase().contains(&q)
                    || t.artists.iter().any(|a| a.name.to_lowercase().contains(&q))
            })
            .map(|(i, _)| i)
            .collect()
    };
}

pub fn char_idx_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

pub fn resolve_full_selection(s: &SearchState) -> Option<SelectionAction> {
    if s.showing_recents() {
        let mut idx = s.selected;
        if idx < s.recent_queries.len() {
            return Some(SelectionAction::PromoteQuery(s.recent_queries[idx].clone()));
        }
        idx -= s.recent_queries.len();
        if idx < s.recent_tracks.len() {
            let uri = s.recent_tracks[idx].uri.clone()?;
            return Some(SelectionAction::Play(PlayAction::Track(uri)));
        }
        return None;
    }
    // Albums and playlists open Browse instead of playing straight away.
    if let Some(coll) = resolve_collection_to_browse(s) {
        return Some(SelectionAction::Browse(coll));
    }
    resolve_selection(s).map(SelectionAction::Play)
}

/// If the current selection points at an album or playlist row in the
/// search results, return that collection's metadata. Otherwise None —
/// e.g. tracks, artists, and in-context rows fall through to play directly.
pub fn resolve_collection_to_browse(s: &SearchState) -> Option<Collection> {
    let mut idx = s.selected;
    if let Some(ctx) = &s.in_context {
        if idx < ctx.filtered.len() {
            return None;
        }
        idx -= ctx.filtered.len();
    }
    if idx < s.results.tracks.len() {
        return None;
    }
    idx -= s.results.tracks.len();
    if idx < s.results.albums.len() {
        let a = &s.results.albums[idx];
        let uri = a.uri.clone()?;
        let subtitle = a
            .artists
            .iter()
            .map(|x| x.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Some(Collection {
            kind: CollectionKind::Album,
            uri,
            name: a.name.clone(),
            subtitle: if subtitle.is_empty() {
                "album".to_string()
            } else {
                format!("album · {subtitle}")
            },
        });
    }
    idx -= s.results.albums.len();
    if idx < s.results.artists.len() {
        // Artists still play directly (Spotify's "play artist context" picks
        // top tracks). Browsing an artist would need a different endpoint
        // (top tracks vs albums) — out of scope for now.
        return None;
    }
    idx -= s.results.artists.len();
    if idx < s.results.playlists.len() {
        let p = &s.results.playlists[idx];
        let owner = p
            .owner
            .as_ref()
            .and_then(|o| o.display_name.clone())
            .unwrap_or_default();
        return Some(Collection {
            kind: CollectionKind::Playlist,
            uri: p.uri.clone(),
            name: p.name.clone(),
            subtitle: if owner.is_empty() {
                "playlist".to_string()
            } else {
                format!("playlist · {owner}")
            },
        });
    }
    None
}

pub fn resolve_selection(s: &SearchState) -> Option<PlayAction> {
    let mut idx = s.selected;

    if let Some(ctx) = &s.in_context {
        if idx < ctx.filtered.len() {
            let track = &ctx.tracks[ctx.filtered[idx]];
            let track_uri = track.uri.clone()?;
            return Some(PlayAction::Context {
                uri: ctx.playlist_uri.clone(),
                offset: Some(track_uri),
            });
        }
        idx -= ctx.filtered.len();
    }

    if idx < s.results.tracks.len() {
        let t = &s.results.tracks[idx];
        let uri = t.uri.clone()?;
        return Some(PlayAction::Track(uri));
    }
    idx -= s.results.tracks.len();

    if idx < s.results.albums.len() {
        let uri = s.results.albums[idx].uri.clone()?;
        return Some(PlayAction::Context { uri, offset: None });
    }
    idx -= s.results.albums.len();

    if idx < s.results.artists.len() {
        let uri = s.results.artists[idx].uri.clone()?;
        return Some(PlayAction::Context { uri, offset: None });
    }
    idx -= s.results.artists.len();

    if idx < s.results.playlists.len() {
        let uri = s.results.playlists[idx].uri.clone();
        return Some(PlayAction::Context { uri, offset: None });
    }

    None
}

pub fn playlist_id_from_uri(uri: &str) -> Option<String> {
    uri.strip_prefix("spotify:playlist:").map(|s| s.to_string())
}

pub fn album_id_from_uri(uri: &str) -> Option<String> {
    uri.strip_prefix("spotify:album:").map(|s| s.to_string())
}
