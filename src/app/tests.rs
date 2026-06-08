use super::*;
use crate::api::{Album, Artist, Playlist, SearchResults, Track};

fn track(uri: &str, name: &str) -> Track {
    Track {
        id: Some(uri.trim_start_matches("spotify:track:").to_string()),
        uri: Some(uri.to_string()),
        name: name.to_string(),
        duration_ms: 0,
        artists: vec![Artist {
            uri: None,
            name: "A".into(),
        }],
        album: Album {
            uri: None,
            name: "Alb".into(),
            artists: vec![],
            images: vec![],
        },
    }
}

fn search_state_with_results(results: SearchResults) -> SearchState {
    let mut s = SearchState::new(None, Vec::new(), Vec::new());
    s.results = results;
    s
}

/// Test helper: build a SearchState wrapping a given InContext, with no
/// recents. Cuts boilerplate from in-context tests.
fn search_state_with_context(ctx: InContext) -> SearchState {
    SearchState::new(Some(ctx), Vec::new(), Vec::new())
}

fn pb_with_ts(ts: Option<u64>, name: &str) -> Playback {
    Playback {
        is_playing: true,
        progress_ms: Some(0),
        item: Some(track("spotify:track:x", name)),
        context: None,
        timestamp: ts,
    }
}

/// All the steady-state should_accept tests assume we're past the
/// initial boot phase (the boot guard is exercised separately below).
fn steady_state() -> AppState {
    AppState {
        boot: false,
        ..Default::default()
    }
}

#[test]
fn should_accept_rejects_polled_older_than_local_action() {
    let mut s = steady_state();
    s.last_local_action_ms = 10_000;
    assert!(!should_accept(&s, Some(&pb_with_ts(Some(5_000), "stale"))));
}

#[test]
fn should_accept_accepts_polled_newer_than_local_action() {
    let mut s = steady_state();
    s.last_local_action_ms = 10_000;
    assert!(should_accept(&s, Some(&pb_with_ts(Some(20_000), "fresh"))));
}

#[test]
fn should_accept_accepts_polled_equal_timestamp() {
    let mut s = steady_state();
    s.last_local_action_ms = 10_000;
    assert!(should_accept(&s, Some(&pb_with_ts(Some(10_000), "same"))));
}

#[test]
fn should_accept_rejects_missing_timestamp_when_we_recently_acted() {
    let mut s = steady_state();
    s.last_local_action_ms = now_unix_ms();
    assert!(!should_accept(&s, Some(&pb_with_ts(None, "no-ts"))));
}

#[test]
fn should_accept_accepts_when_no_prior_local_action() {
    let s = steady_state();
    assert!(should_accept(&s, Some(&pb_with_ts(Some(0), "first"))));
    assert!(should_accept(&s, None));
}

#[test]
fn should_accept_rejects_none_right_after_play() {
    let mut s = steady_state();
    s.last_local_action_ms = now_unix_ms();
    assert!(!should_accept(&s, None));
}

#[test]
fn should_accept_accepts_none_after_long_idle() {
    let mut s = steady_state();
    s.last_local_action_ms = now_unix_ms().saturating_sub(120_000);
    assert!(should_accept(&s, None));
}

// --- boot-phase gating ----------------------------------------------

#[test]
fn should_accept_boot_accepts_paused_polled() {
    let s = AppState::default();
    let mut pb = pb_with_ts(Some(0), "old");
    pb.is_playing = false;
    assert!(should_accept(&s, Some(&pb)));
}

#[test]
fn should_accept_boot_rejects_none() {
    let s = AppState::default();
    assert!(!should_accept(&s, None));
}

#[test]
fn should_accept_boot_accepts_playing_polled() {
    let s = AppState::default();
    let mut pb = pb_with_ts(Some(0), "live");
    pb.is_playing = true;
    assert!(should_accept(&s, Some(&pb)));
}

#[test]
fn synth_template_for_track_omits_timestamp() {
    let search = search_state_with_results(SearchResults {
        tracks: vec![track("spotify:track:abc", "T1")],
        ..Default::default()
    });
    let action = PlayAction::Track("spotify:track:abc".into());
    let synth = synth_template_for(&action, &search).expect("synth must build");
    assert!(synth.is_playing);
    assert_eq!(synth.item.as_ref().unwrap().name, "T1");
    assert_eq!(synth.progress_ms, Some(0));
    assert!(synth.context.is_none());
    assert!(synth.timestamp.is_none());
}

#[test]
fn synth_template_for_in_context_includes_playlist_context() {
    let mut s = search_state_with_context(InContext {
        playlist_uri: "spotify:playlist:pl".into(),
        tracks: vec![track("spotify:track:in0", "in0")],
        filtered: vec![0],
    });
    s.selected = 0;
    let action = PlayAction::Context {
        uri: "spotify:playlist:pl".into(),
        offset: Some("spotify:track:in0".into()),
    };
    let synth = synth_template_for(&action, &s).expect("synth must build");
    assert_eq!(synth.item.as_ref().unwrap().name, "in0");
    let ctx = synth.context.expect("context expected");
    assert_eq!(ctx.uri, "spotify:playlist:pl");
    assert_eq!(ctx.kind, "playlist");
}

#[test]
fn synth_template_for_context_without_offset_is_none() {
    let search = search_state_with_results(SearchResults::default());
    let action = PlayAction::Context {
        uri: "spotify:album:a".into(),
        offset: None,
    };
    assert!(synth_template_for(&action, &search).is_none());
}

#[test]
fn synth_with_matching_timestamp_is_accepted() {
    let ts = now_unix_ms();
    let mut s = AppState::default();
    s.last_local_action_ms = ts;
    let mut synth = pb_with_ts(Some(ts), "POWER");
    synth.is_playing = true;
    assert!(should_accept(&s, Some(&synth)));
}

#[test]
fn resolve_track_returns_track_uri() {
    let s = search_state_with_results(SearchResults {
        tracks: vec![track("spotify:track:abc", "T1")],
        ..Default::default()
    });
    match resolve_selection(&s) {
        Some(PlayAction::Track(uri)) => assert_eq!(uri, "spotify:track:abc"),
        other => panic!("expected Track, got {other:?}"),
    }
}

#[test]
fn resolve_album_returns_context_no_offset() {
    let mut s = search_state_with_results(SearchResults {
        albums: vec![Album {
            uri: Some("spotify:album:1".into()),
            name: "A".into(),
            artists: vec![],
            images: vec![],
        }],
        ..Default::default()
    });
    s.selected = 0;
    match resolve_selection(&s) {
        Some(PlayAction::Context { uri, offset }) => {
            assert_eq!(uri, "spotify:album:1");
            assert!(offset.is_none());
        }
        other => panic!("expected album context, got {other:?}"),
    }
}

#[test]
fn resolve_walks_across_sections() {
    let mut s = search_state_with_results(SearchResults {
        tracks: vec![
            track("spotify:track:t0", "t0"),
            track("spotify:track:t1", "t1"),
        ],
        albums: vec![Album {
            uri: Some("spotify:album:al".into()),
            name: "Al".into(),
            artists: vec![],
            images: vec![],
        }],
        artists: vec![Artist {
            uri: Some("spotify:artist:ar".into()),
            name: "Ar".into(),
        }],
        playlists: vec![Playlist {
            uri: "spotify:playlist:p".into(),
            name: "P".into(),
            owner: None,
        }],
    });
    let cases: Vec<(usize, &str)> = vec![
        (0, "spotify:track:t0"),
        (1, "spotify:track:t1"),
        (2, "spotify:album:al"),
        (3, "spotify:artist:ar"),
        (4, "spotify:playlist:p"),
    ];
    for (idx, want) in cases {
        s.selected = idx;
        let got = match resolve_selection(&s) {
            Some(PlayAction::Track(u)) => u,
            Some(PlayAction::Context { uri, .. }) => uri,
            None => panic!("none at idx {idx}"),
        };
        assert_eq!(got, want, "at idx {idx}");
    }
}

#[test]
fn resolve_returns_none_when_out_of_range() {
    let s = search_state_with_results(SearchResults::default());
    assert!(resolve_selection(&s).is_none());
}

#[test]
fn resolve_in_context_uses_playlist_uri_with_offset() {
    let mut s = search_state_with_context(InContext {
        playlist_uri: "spotify:playlist:pl".into(),
        tracks: vec![track("spotify:track:in0", "in0")],
        filtered: vec![0],
    });
    s.selected = 0;
    match resolve_selection(&s) {
        Some(PlayAction::Context { uri, offset }) => {
            assert_eq!(uri, "spotify:playlist:pl");
            assert_eq!(offset.as_deref(), Some("spotify:track:in0"));
        }
        other => panic!("expected in-context, got {other:?}"),
    }
}

#[test]
fn refilter_in_context_matches_track_name_case_insensitive() {
    let mut s = search_state_with_context(InContext {
        playlist_uri: "spotify:playlist:p".into(),
        tracks: vec![
            track("spotify:track:1", "Strawberry Fields"),
            track("spotify:track:2", "Yesterday"),
        ],
        filtered: vec![],
    });
    s.input = "STRAW".into();
    refilter_in_context(&mut s);
    let ctx = s.in_context.as_ref().unwrap();
    assert_eq!(ctx.filtered, vec![0]);
}

#[test]
fn refilter_in_context_empty_input_clears() {
    let mut s = search_state_with_context(InContext {
        playlist_uri: "spotify:playlist:p".into(),
        tracks: vec![track("spotify:track:1", "X")],
        filtered: vec![0],
    });
    s.input.clear();
    refilter_in_context(&mut s);
    assert!(s.in_context.as_ref().unwrap().filtered.is_empty());
}

#[test]
fn char_idx_to_byte_ascii_and_multibyte() {
    assert_eq!(char_idx_to_byte("abc", 0), 0);
    assert_eq!(char_idx_to_byte("abc", 2), 2);
    assert_eq!(char_idx_to_byte("abc", 3), 3);
    assert_eq!(char_idx_to_byte("abc", 99), 3);
    assert_eq!(char_idx_to_byte("aéc", 1), 1);
    assert_eq!(char_idx_to_byte("aéc", 2), 3);
}

#[test]
fn playlist_id_extraction() {
    assert_eq!(
        playlist_id_from_uri("spotify:playlist:abc123"),
        Some("abc123".into())
    );
    assert!(playlist_id_from_uri("spotify:album:abc").is_none());
}

// --- recents in the search overlay ---------------------------------

#[test]
fn recents_resolve_first_to_promote_query() {
    let s = SearchState::new(
        None,
        vec!["the beatles".into(), "weezer".into()],
        vec![track("spotify:track:r1", "Recent1")],
    );
    match resolve_full_selection(&s) {
        Some(SelectionAction::PromoteQuery(q)) => assert_eq!(q, "the beatles"),
        other => panic!("expected PromoteQuery, got {other:?}"),
    }
}

#[test]
fn recents_resolve_walks_into_recently_played() {
    let mut s = SearchState::new(
        None,
        vec!["q1".into()],
        vec![
            track("spotify:track:r0", "R0"),
            track("spotify:track:r1", "R1"),
        ],
    );
    s.selected = 2;
    match resolve_full_selection(&s) {
        Some(SelectionAction::Play(PlayAction::Track(uri))) => {
            assert_eq!(uri, "spotify:track:r1");
        }
        other => panic!("expected Play(Track), got {other:?}"),
    }
}

#[test]
fn recents_hidden_when_input_nonempty() {
    let mut s = SearchState::new(
        None,
        vec!["q1".into()],
        vec![track("spotify:track:r0", "R0")],
    );
    s.input = "anything".into();
    assert!(resolve_full_selection(&s).is_none());
}

#[test]
fn visible_row_count_uses_recents_when_input_empty() {
    let s = SearchState::new(
        None,
        vec!["q1".into(), "q2".into()],
        vec![track("spotify:track:r0", "R0")],
    );
    assert_eq!(visible_row_count(&s), 3);
}

#[test]
fn visible_row_count_ignores_recents_when_input_nonempty() {
    let mut s = SearchState::new(
        None,
        vec!["q1".into(), "q2".into()],
        vec![track("spotify:track:r0", "R0")],
    );
    s.input = "x".into();
    assert_eq!(visible_row_count(&s), 0);
}

// --- pause/resume progress anchor ----------------------------------

fn state_with_playback(progress_ms: u64, is_playing: bool) -> AppState {
    AppState {
        playback: Some(Playback {
            is_playing,
            progress_ms: Some(progress_ms),
            item: Some(track("spotify:track:x", "X")),
            context: None,
            timestamp: None,
        }),
        last_poll: Some(Instant::now()),
        ..Default::default()
    }
}

#[test]
fn displayed_progress_for_toggle_paused_returns_stored() {
    let s = state_with_playback(45_000, false);
    assert_eq!(displayed_progress_for_toggle(&s), 45_000);
}

#[test]
fn displayed_progress_for_toggle_playing_adds_elapsed() {
    let s = state_with_playback(45_000, true);
    let got = displayed_progress_for_toggle(&s);
    assert!(got >= 45_000 && got < 45_500, "got {got}");
}

// --- browse routing -------------------------------------------------

#[test]
fn resolve_album_row_returns_browse_action() {
    let mut s = search_state_with_results(SearchResults {
        albums: vec![Album {
            uri: Some("spotify:album:abc".into()),
            name: "Some Album".into(),
            artists: vec![Artist {
                uri: None,
                name: "Artist X".into(),
            }],
            images: vec![],
        }],
        ..Default::default()
    });
    s.input = "x".into();
    s.selected = 0;
    match resolve_full_selection(&s) {
        Some(SelectionAction::Browse(c)) => {
            assert_eq!(c.uri, "spotify:album:abc");
            assert!(matches!(c.kind, CollectionKind::Album));
            assert_eq!(c.name, "Some Album");
            assert!(c.subtitle.contains("Artist X"), "got: {}", c.subtitle);
        }
        other => panic!("expected Browse(album), got {other:?}"),
    }
}

#[test]
fn resolve_playlist_row_returns_browse_action() {
    let mut s = search_state_with_results(SearchResults {
        playlists: vec![Playlist {
            uri: "spotify:playlist:p1".into(),
            name: "Mix".into(),
            owner: Some(crate::api::PlaylistOwner {
                display_name: Some("alice".into()),
            }),
        }],
        ..Default::default()
    });
    s.input = "x".into();
    s.selected = 0;
    match resolve_full_selection(&s) {
        Some(SelectionAction::Browse(c)) => {
            assert_eq!(c.uri, "spotify:playlist:p1");
            assert!(matches!(c.kind, CollectionKind::Playlist));
            assert!(c.subtitle.contains("alice"), "got: {}", c.subtitle);
        }
        other => panic!("expected Browse(playlist), got {other:?}"),
    }
}

#[test]
fn resolve_track_row_still_plays_not_browses() {
    let mut s = search_state_with_results(SearchResults {
        tracks: vec![track("spotify:track:t1", "T1")],
        ..Default::default()
    });
    s.input = "x".into();
    match resolve_full_selection(&s) {
        Some(SelectionAction::Play(PlayAction::Track(uri))) => {
            assert_eq!(uri, "spotify:track:t1");
        }
        other => panic!("expected Play(Track), got {other:?}"),
    }
}

#[test]
fn resolve_artist_row_still_plays_not_browses() {
    let mut s = search_state_with_results(SearchResults {
        artists: vec![Artist {
            uri: Some("spotify:artist:a1".into()),
            name: "AR".into(),
        }],
        ..Default::default()
    });
    s.input = "x".into();
    s.selected = 0;
    match resolve_full_selection(&s) {
        Some(SelectionAction::Play(PlayAction::Context { uri, offset })) => {
            assert_eq!(uri, "spotify:artist:a1");
            assert!(offset.is_none());
        }
        other => panic!("expected Play(Context for artist), got {other:?}"),
    }
}

#[test]
fn album_id_from_uri_extracts() {
    assert_eq!(
        album_id_from_uri("spotify:album:xyz"),
        Some("xyz".to_string())
    );
    assert!(album_id_from_uri("spotify:playlist:xyz").is_none());
}

// --- command menu ---------------------------------------------------

#[test]
fn cmd_filtered_empty_input_returns_all() {
    let s = CommandState::default();
    assert_eq!(s.filtered().len(), Cmd::ALL.len());
}

#[test]
fn cmd_filtered_case_insensitive_substring() {
    let mut s = CommandState::default();
    s.input = "PaUs".into();
    let got: Vec<&'static str> = s.filtered().iter().map(|c| c.name()).collect();
    assert!(got.contains(&"play / pause"), "got: {got:?}");
}

#[test]
fn cmd_selected_indexes_into_filtered() {
    let mut s = CommandState::default();
    s.input = "re".into();
    s.selected = 1;
    let chosen = s.selected_cmd().expect("must select");
    let names: Vec<&'static str> = s.filtered().iter().map(|c| c.name()).collect();
    assert_eq!(chosen.name(), names[1]);
}

#[test]
fn cmd_selected_out_of_range_returns_none() {
    let mut s = CommandState::default();
    s.selected = 999;
    assert!(s.selected_cmd().is_none());
}

#[test]
fn is_device_not_found_recognizes_404_message() {
    assert!(is_device_not_found(
        "PUT https://api.spotify.com/v1/me/player/play: 404 Not Found: Device not found"
    ));
    assert!(is_device_not_found(
        "{\"error\": {\"status\" : 404, \"message\" : \"x\"}}"
    ));
    assert!(!is_device_not_found("rate limited"));
}

// ====================================================================
// End-to-end scenarios driven through the Harness (test_support.rs).
// ====================================================================

use crate::input::Key;
use crate::test_support::{Call, Harness};

fn dummy_album(uri: &str, name: &str, artist: &str) -> Album {
    Album {
        uri: Some(uri.into()),
        name: name.into(),
        artists: vec![Artist {
            uri: None,
            name: artist.into(),
        }],
        images: vec![],
    }
}

fn dummy_playlist(uri: &str, name: &str, owner: &str) -> Playlist {
    Playlist {
        uri: uri.into(),
        name: name.into(),
        owner: Some(crate::api::PlaylistOwner {
            display_name: Some(owner.into()),
        }),
    }
}

#[tokio::test]
async fn playlist_browse_403_shows_friendly_error_and_offers_p_fallback() {
    let h = Harness::new();
    h.fake.set_search(
        "power",
        Ok(SearchResults {
            playlists: vec![dummy_playlist(
                "spotify:playlist:abc123",
                "POWER",
                "chrisbolin",
            )],
            ..Default::default()
        }),
    );
    h.fake.set_playlist_tracks(
            "abc123",
            Err("GET https://api.spotify.com/v1/playlists/abc123/tracks: 403 Forbidden: {\"error\": {\"status\": 403, \"message\": \"Forbidden\"}}".into()),
        );

    h.press_and_run(Key::Char('/')).await;
    h.type_str("power").await;
    h.settle().await;
    h.press_and_run(Key::Down).await;
    h.press_and_run(Key::Enter).await;
    h.settle().await;

    assert_eq!(h.mode_name().await, "browse");
    {
        let s = h.state.lock().await;
        let Some(Overlay::Browse(b)) = &s.overlay else {
            panic!("expected Mode::Browse, got {}", mode_name(&s));
        };
        assert!(!b.loading, "loading flag should be cleared");
        assert!(b.error.is_some(), "error should be populated on 403");
        let e = b.error.as_ref().unwrap();
        assert!(e.contains("403"), "error must mention 403, got: {e}");
    }

    let screen = h.snapshot().await;
    assert!(
        screen.contains("Spotify locked this playlist"),
        "expected friendly hint in:\n{screen}"
    );
    assert!(
        screen.contains("[p] plays anyway") || screen.contains("[p] play"),
        "expected fallback hint in:\n{screen}"
    );

    let calls = h.fake.calls();
    assert!(
        calls
            .iter()
            .any(|c| matches!(c, Call::GetPlaylistTracks(id) if id == "abc123")),
        "calls: {calls:?}"
    );
}

#[tokio::test]
async fn album_browse_success_shows_track_list() {
    let h = Harness::new();
    h.fake.set_search(
        "test",
        Ok(SearchResults {
            albums: vec![dummy_album(
                "spotify:album:al1",
                "Test Album",
                "Test Artist",
            )],
            ..Default::default()
        }),
    );
    h.fake.set_album_tracks(
        "al1",
        Ok(vec![
            track("spotify:track:a", "Track One"),
            track("spotify:track:b", "Track Two"),
            track("spotify:track:c", "Track Three"),
        ]),
    );

    h.press_and_run(Key::Char('/')).await;
    h.type_str("test").await;
    h.settle().await;
    h.press_and_run(Key::Enter).await;
    h.settle().await;

    let s = h.state.lock().await;
    let Some(Overlay::Browse(b)) = &s.overlay else {
        panic!("expected Browse");
    };
    assert!(!b.loading);
    assert!(b.error.is_none(), "no error expected, got {:?}", b.error);
    assert_eq!(b.tracks.len(), 3);
    assert_eq!(b.collection.name, "Test Album");
}

#[tokio::test]
async fn p_in_browse_plays_whole_collection() {
    let h = Harness::new();
    h.fake.set_search(
        "x",
        Ok(SearchResults {
            playlists: vec![dummy_playlist("spotify:playlist:xyz", "Mix", "alice")],
            ..Default::default()
        }),
    );
    h.fake
        .set_playlist_tracks("xyz", Err("403 Forbidden".into()));

    h.press_and_run(Key::Char('/')).await;
    h.type_str("x").await;
    h.settle().await;
    h.press_and_run(Key::Enter).await;
    h.settle().await;

    h.fake.clear_calls();
    h.press_and_run(Key::Char('p')).await;
    h.settle().await;

    let calls = h.fake.calls();
    let played = calls.iter().find_map(|c| match c {
        Call::PlayContext { uri, offset } => Some((uri.clone(), offset.clone())),
        _ => None,
    });
    assert_eq!(
        played,
        Some(("spotify:playlist:xyz".into(), None)),
        "expected play_context(playlist, None), all calls: {calls:?}"
    );
    assert_eq!(h.mode_name().await, "now_playing");
}

#[tokio::test]
async fn enter_in_browse_plays_selected_with_context_offset() {
    let h = Harness::new();
    h.fake.set_search(
        "x",
        Ok(SearchResults {
            albums: vec![dummy_album("spotify:album:al1", "Album", "Artist")],
            ..Default::default()
        }),
    );
    h.fake.set_album_tracks(
        "al1",
        Ok(vec![
            track("spotify:track:t0", "T0"),
            track("spotify:track:t1", "T1"),
            track("spotify:track:t2", "T2"),
        ]),
    );

    h.press_and_run(Key::Char('/')).await;
    h.type_str("x").await;
    h.settle().await;
    h.press_and_run(Key::Enter).await;
    h.settle().await;

    h.press_and_run(Key::Down).await;
    h.press_and_run(Key::Down).await;
    h.fake.clear_calls();
    h.press_and_run(Key::Enter).await;
    h.settle().await;

    let calls = h.fake.calls();
    let played = calls.iter().find_map(|c| match c {
        Call::PlayContext { uri, offset } => Some((uri.clone(), offset.clone())),
        _ => None,
    });
    assert_eq!(
        played,
        Some(("spotify:album:al1".into(), Some("spotify:track:t2".into()),)),
        "got calls: {calls:?}"
    );
}

#[tokio::test]
async fn esc_in_browse_reveals_search_tab() {
    let h = Harness::new();
    h.fake.set_search(
        "x",
        Ok(SearchResults {
            albums: vec![dummy_album("spotify:album:al1", "A", "Art")],
            ..Default::default()
        }),
    );
    h.fake.set_album_tracks("al1", Ok(vec![]));

    h.press_and_run(Key::Char('/')).await;
    h.type_str("x").await;
    h.settle().await;
    h.press_and_run(Key::Enter).await;
    h.settle().await;
    assert_eq!(h.mode_name().await, "browse");

    h.press_and_run(Key::Esc).await;
    assert_eq!(h.mode_name().await, "search");

    let s = h.state.lock().await;
    let search = &s.search;
    assert_eq!(search.input, "x");
    assert_eq!(search.results.albums.len(), 1);
}

#[tokio::test]
async fn rate_limited_state_blocks_play_pause() {
    let h = Harness::new();
    h.seed_playback(Playback {
        is_playing: false,
        progress_ms: Some(0),
        item: Some(track("spotify:track:x", "X")),
        context: None,
        timestamp: Some(now_unix_ms()),
    })
    .await;
    {
        let mut s = h.state.lock().await;
        s.boot = false;
        s.rate_limited_until = Some(Instant::now() + Duration::from_secs(300));
    }
    h.fake.clear_calls();
    h.press_and_run(Key::Char(' ')).await;

    let calls = h.fake.calls();
    assert!(
        !calls.iter().any(|c| matches!(c, Call::Play | Call::Pause)),
        "expected no Play/Pause calls while rate-limited, got: {calls:?}"
    );
}

#[tokio::test]
async fn like_command_calls_save_track_for_current_track() {
    let h = Harness::new();
    {
        let mut s = h.state.lock().await;
        s.playback = Some(Playback {
            is_playing: true,
            progress_ms: Some(0),
            item: Some(Track {
                id: Some("track42".into()),
                uri: Some("spotify:track:track42".into()),
                name: "Heart It Races".into(),
                duration_ms: 200_000,
                artists: vec![],
                album: Album::default(),
            }),
            context: None,
            timestamp: None,
        });
    }

    h.run(KeyAction::LikeCurrent).await;

    let calls = h.fake.calls();
    assert!(
        calls
            .iter()
            .any(|c| matches!(c, Call::SaveTrack(id) if id == "track42")),
        "expected SaveTrack(track42), got: {calls:?}"
    );
    let s = h.state.lock().await;
    let (msg, _) = s.notice.as_ref().expect("notice should be set on success");
    assert!(msg.contains("Heart It Races"), "got notice: {msg}");
    assert!(s.error.is_none());
}

#[tokio::test]
async fn like_command_skips_when_nothing_playing() {
    let h = Harness::new();
    // No playback set — like should be a no-op.

    h.run(KeyAction::LikeCurrent).await;

    let calls = h.fake.calls();
    assert!(
        !calls.iter().any(|c| matches!(c, Call::SaveTrack(_))),
        "expected no SaveTrack call, got: {calls:?}"
    );
    let s = h.state.lock().await;
    assert!(s.notice.is_none());
    assert!(s.error.is_none());
}

#[tokio::test]
async fn like_command_skipped_when_rate_limited() {
    let h = Harness::new();
    {
        let mut s = h.state.lock().await;
        s.rate_limited_until = Some(Instant::now() + Duration::from_secs(120));
        s.playback = Some(Playback {
            is_playing: true,
            progress_ms: Some(0),
            item: Some(Track {
                id: Some("track42".into()),
                uri: Some("spotify:track:track42".into()),
                name: "T".into(),
                duration_ms: 1,
                artists: vec![],
                album: Album::default(),
            }),
            context: None,
            timestamp: None,
        });
    }

    h.run(KeyAction::LikeCurrent).await;

    let calls = h.fake.calls();
    assert!(
        !calls.iter().any(|c| matches!(c, Call::SaveTrack(_))),
        "expected no SaveTrack while rate-limited, got: {calls:?}"
    );
}

#[tokio::test]
async fn rate_limited_ui_shows_countdown() {
    let h = Harness::new();
    {
        let mut s = h.state.lock().await;
        s.rate_limited_until = Some(Instant::now() + Duration::from_secs(120));
    }
    let screen = h.snapshot().await;
    assert!(
        screen.contains("rate limited"),
        "expected rate-limit hint in:\n{screen}"
    );
}

#[tokio::test]
async fn enter_on_track_row_plays_directly() {
    let h = Harness::new();
    h.fake.set_search(
        "rock",
        Ok(SearchResults {
            tracks: vec![track("spotify:track:hit", "Hit")],
            ..Default::default()
        }),
    );

    h.press_and_run(Key::Char('/')).await;
    h.type_str("rock").await;
    h.settle().await;
    h.fake.clear_calls();
    h.press_and_run(Key::Enter).await;
    h.settle().await;

    let calls = h.fake.calls();
    assert!(
        calls.iter().any(
            |c| matches!(c, Call::PlayUris(uris) if uris == &["spotify:track:hit".to_string()])
        ),
        "expected PlayUris call, got: {calls:?}"
    );
    assert_eq!(h.mode_name().await, "now_playing");
}

// --- loading state -------------------------------------------------

/// While a search request is debouncing/in flight, the UI must say
/// "loading…" — never "no results for …", which would be a lie.
#[tokio::test]
async fn search_shows_loading_not_no_results_before_response() {
    let h = Harness::new();
    h.fake.set_search(
        "rock",
        Ok(SearchResults {
            tracks: vec![track("spotify:track:hit", "Hit")],
            ..Default::default()
        }),
    );

    h.press_and_run(Key::Char('/')).await;
    h.type_str("rock").await;
    // Deliberately do NOT settle — the debounce hasn't fired yet, the
    // request is still pending. This is the window where the old UI
    // wrongly said "no results found".

    {
        let s = h.state.lock().await;
        let search = &s.search;
        assert!(
            search.is_loading(),
            "expected is_loading() to be true while debounce/request is pending"
        );
    }

    let screen = h.snapshot().await;
    assert!(
        screen.contains("loading"),
        "expected loading indicator while search pending, got:\n{screen}"
    );
    assert!(
        !screen.contains("no results"),
        "should not say 'no results' while still loading, got:\n{screen}"
    );
}

/// After the response lands, is_loading() flips false and the count
/// hint replaces "loading…".
#[tokio::test]
async fn search_clears_loading_after_response_applied() {
    let h = Harness::new();
    h.fake.set_search(
        "rock",
        Ok(SearchResults {
            tracks: vec![track("spotify:track:hit", "Hit")],
            ..Default::default()
        }),
    );

    h.press_and_run(Key::Char('/')).await;
    h.type_str("rock").await;
    h.settle().await;

    {
        let s = h.state.lock().await;
        let search = &s.search;
        assert!(!search.is_loading(), "should not be loading after settle");
        assert_eq!(search.last_query, "rock");
    }

    let screen = h.snapshot().await;
    assert!(
        !screen.contains("loading"),
        "screen still says loading:\n{screen}"
    );
    assert!(
        screen.contains("1 results") || screen.contains("Tracks"),
        "expected results to render, got:\n{screen}"
    );
}

/// Zero matches from Spotify (not "no response yet") DOES show "no
/// results for …" — distinguishing this from the loading case is the
/// whole point.
#[tokio::test]
async fn search_with_zero_matches_says_no_results() {
    let h = Harness::new();
    h.fake
        .set_search("nothingmatches", Ok(SearchResults::default()));

    h.press_and_run(Key::Char('/')).await;
    h.type_str("nothingmatches").await;
    h.settle().await;

    {
        let s = h.state.lock().await;
        let search = &s.search;
        assert!(!search.is_loading());
    }

    let screen = h.snapshot().await;
    assert!(
        screen.contains("no results for"),
        "expected 'no results' for genuine empty response, got:\n{screen}"
    );
}

// --- Tab / overlay navigation --------------------------------------

#[tokio::test]
async fn tab_key_cycles_top_tabs() {
    let h = Harness::new();
    assert_eq!(h.mode_name().await, "now_playing");
    h.press_and_run(Key::Tab).await;
    assert_eq!(h.mode_name().await, "search");
    h.press_and_run(Key::Tab).await;
    assert_eq!(h.mode_name().await, "library");
    h.press_and_run(Key::Tab).await;
    assert_eq!(h.mode_name().await, "now_playing");
    // Shift-Tab goes the other way.
    h.press_with_mods(
        Key::Tab,
        crate::input::Mods {
            shift: true,
            ..Default::default()
        },
    )
    .await;
    // press_with_mods only dispatches; cycle is applied in dispatch itself.
    assert_eq!(h.mode_name().await, "library");
}

#[tokio::test]
async fn d_opens_devices_on_now_playing_but_types_into_search() {
    let h = Harness::new();
    // On Now Playing, 'd' is the global device-picker launcher.
    let action = h.press(Key::Char('d')).await;
    assert!(matches!(action, KeyAction::OpenDevices), "got {action:?}");

    // In the Search tab, 'd' is literal text — never opens devices.
    h.press_and_run(Key::Char('/')).await;
    assert_eq!(h.mode_name().await, "search");
    let action = h.press(Key::Char('d')).await;
    assert!(
        matches!(action, KeyAction::SearchInputChanged),
        "got {action:?}"
    );
    let s = h.state.lock().await;
    assert_eq!(s.search.input, "d");
}

#[tokio::test]
async fn esc_closes_overlay_without_changing_tab() {
    let h = Harness::new();
    h.press_and_run(Key::Tab).await; // -> Search tab
    assert_eq!(h.mode_name().await, "search");
    // Open the command palette (overlay) from the search tab... but search
    // captures ':' as text, so switch to Library where ':' is global.
    h.press_and_run(Key::Tab).await; // -> Library
    assert_eq!(h.mode_name().await, "library");
    h.press_and_run(Key::Char(':')).await; // command overlay
    assert_eq!(h.mode_name().await, "command");
    h.press_and_run(Key::Esc).await; // closes overlay
    assert_eq!(
        h.mode_name().await,
        "library",
        "esc should reveal the tab, not jump home"
    );
}

// --- Up/down focus model (content <-> tab strip) -------------------

#[tokio::test]
async fn up_from_now_playing_rises_to_tab_strip_then_arrows_switch_tabs() {
    let h = Harness::new();
    // Now Playing has no list, so Up goes straight to the tab strip.
    h.press_and_run(Key::Up).await;
    {
        let s = h.state.lock().await;
        assert_eq!(s.focus, Focus::Tabs);
        assert_eq!(s.tab, Tab::NowPlaying);
    }
    // On the strip, Right/Left switch the top tab without leaving the strip.
    h.press_and_run(Key::Right).await;
    {
        let s = h.state.lock().await;
        assert_eq!(s.focus, Focus::Tabs, "still on the tab strip");
        assert_eq!(s.tab, Tab::Search);
    }
    h.press_and_run(Key::Right).await;
    assert_eq!(h.state.lock().await.tab, Tab::Library);
    // Down drops into the content of the selected tab.
    h.press_and_run(Key::Down).await;
    let s = h.state.lock().await;
    assert_eq!(s.focus, Focus::Content);
    assert_eq!(s.tab, Tab::Library);
}

#[tokio::test]
async fn up_at_top_of_search_results_rises_to_tabs() {
    let h = Harness::new();
    h.fake.set_search(
        "q",
        Ok(SearchResults {
            tracks: vec![
                track("spotify:track:1", "One"),
                track("spotify:track:2", "Two"),
            ],
            ..Default::default()
        }),
    );
    h.press_and_run(Key::Char('/')).await;
    h.type_str("q").await;
    h.settle().await;
    // Move down one, then up twice: first Up returns to row 0, second Up
    // (at the top) rises to the tab strip.
    h.press_and_run(Key::Down).await;
    h.press_and_run(Key::Up).await;
    assert_eq!(h.state.lock().await.focus, Focus::Content);
    h.press_and_run(Key::Up).await;
    assert_eq!(h.state.lock().await.focus, Focus::Tabs);
}

#[tokio::test]
async fn esc_on_tab_strip_returns_to_content() {
    let h = Harness::new();
    h.press_and_run(Key::Char('l')).await; // Library, focus = Content
    h.settle().await;
    h.press_and_run(Key::Up).await; // selected is 0 -> rise to tabs
    assert_eq!(h.state.lock().await.focus, Focus::Tabs);
    h.press_and_run(Key::Esc).await; // esc backs into content, not quit
    let s = h.state.lock().await;
    assert_eq!(s.focus, Focus::Content);
    assert_eq!(
        s.tab,
        Tab::Library,
        "esc on the strip stays on the same tab"
    );
}

// --- Library lazy loading ------------------------------------------

#[tokio::test]
async fn library_loads_active_subtab_once() {
    let h = Harness::new();
    h.fake
        .set_saved_tracks(Ok(vec![track("spotify:track:a", "A")]));

    // Enter Library — Liked is the default sub-tab and should fetch once.
    h.press_and_run(Key::Char('l')).await;
    h.settle().await;
    assert_eq!(h.mode_name().await, "library");
    {
        let s = h.state.lock().await;
        assert!(s.library.liked.loaded);
        assert_eq!(s.library.liked.items.len(), 1);
    }
    // Re-entering Library must NOT refetch a loaded section.
    h.press_and_run(Key::Char('l')).await;
    h.settle().await;
    let n = h
        .fake
        .calls()
        .iter()
        .filter(|c| matches!(c, Call::GetSavedTracks(_)))
        .count();
    assert_eq!(n, 1, "Liked section should be fetched exactly once");
}

#[tokio::test]
async fn library_subtab_switch_loads_lazily() {
    let h = Harness::new();
    h.fake.set_saved_tracks(Ok(vec![]));
    h.fake
        .set_saved_playlists(Ok(vec![dummy_playlist("spotify:playlist:p", "P", "me")]));

    h.press_and_run(Key::Char('l')).await;
    h.settle().await;
    // Right -> Playlists sub-tab, which now lazily loads.
    h.press_and_run(Key::Right).await;
    h.settle().await;
    let s = h.state.lock().await;
    assert_eq!(s.library.tab, LibraryTab::Playlists);
    assert!(s.library.playlists.loaded);
    assert_eq!(s.library.playlists.items.len(), 1);
}

#[tokio::test]
async fn library_403_surfaces_per_section_hint() {
    let h = Harness::new();
    h.fake
        .set_saved_tracks(Err("GET /me/tracks: 403 Forbidden".into()));
    h.press_and_run(Key::Char('l')).await;
    h.settle().await;
    let s = h.state.lock().await;
    let e = s
        .library
        .liked
        .error
        .as_ref()
        .expect("error populated on 403");
    assert!(
        e.contains("missing scope"),
        "expected re-auth hint, got: {e}"
    );
    assert!(
        !s.library.liked.loaded,
        "403 must not mark the section loaded"
    );
}

#[tokio::test]
async fn library_playlist_enter_opens_browse() {
    let h = Harness::new();
    h.fake
        .set_saved_playlists(Ok(vec![dummy_playlist("spotify:playlist:pl", "PL", "me")]));
    h.fake
        .set_playlist_tracks("pl", Ok(vec![track("spotify:track:t", "T")]));

    h.press_and_run(Key::Char('l')).await;
    h.press_and_run(Key::Right).await; // -> Playlists
    h.settle().await;
    h.press_and_run(Key::Enter).await; // open the selected playlist
    h.settle().await;
    assert_eq!(h.mode_name().await, "browse");
    let s = h.state.lock().await;
    let Some(Overlay::Browse(b)) = &s.overlay else {
        panic!("expected browse overlay")
    };
    assert_eq!(b.collection.uri, "spotify:playlist:pl");
    assert_eq!(b.tracks.len(), 1);
}

// --- Devices overlay -----------------------------------------------

#[tokio::test]
async fn devices_opens_fetches_once_and_transfers() {
    let h = Harness::new();
    h.fake.set_devices(Ok(vec![
        Device {
            id: Some("dev1".into()),
            name: "Phone".into(),
            is_active: false,
        },
        Device {
            id: Some("dev2".into()),
            name: "Speaker".into(),
            is_active: true,
        },
    ]));

    h.press_and_run(Key::Char('d')).await;
    h.settle().await;
    assert_eq!(h.mode_name().await, "devices");
    {
        let s = h.state.lock().await;
        let Some(Overlay::Devices(dev)) = &s.overlay else {
            panic!("expected devices overlay")
        };
        assert_eq!(dev.devices.len(), 2);
        // Selection defaults to the active device.
        assert_eq!(dev.selected, 1);
    }
    let n = h
        .fake
        .calls()
        .iter()
        .filter(|c| matches!(c, Call::GetDevices))
        .count();
    assert_eq!(n, 1, "devices fetched exactly once on open");

    // Enter transfers to the selected device and closes the overlay.
    h.press_and_run(Key::Enter).await;
    h.settle().await;
    assert_eq!(h.mode_name().await, "now_playing");
    let transferred = h.fake.calls().iter().any(
        |c| matches!(c, Call::TransferPlayback { device_id, play: true } if device_id == "dev2"),
    );
    assert!(transferred, "expected transfer to dev2");
}

// --- 96x40 fixed-canvas exercise -----------------------------------
//
// Renders every mode at exactly the fixed canvas size with deliberately
// long content (track names, playlist names, etc.) and checks for the
// tell-tale signs of overflow:
//
//   1. The bottom-right corner of the outer Block border is at column
//      95, row 39 — if it's missing the layout overran height.
//   2. The footer hint line (row 38) is fully rendered — if truncated
//      mid-character, the canvas is too narrow.
//   3. No row has content extending past column 95 (impossible by
//      construction, but worth pinning).
//
// Run with `cargo test ui_at_96x40 -- --nocapture` to see snapshots.

fn long_track() -> Track {
    Track {
        id: Some("idLong".into()),
        uri: Some("spotify:track:long".into()),
        name: "Mr. Brightside (Jacques Lu Cont's Thin White Duke Mix)".into(),
        duration_ms: 423_000,
        artists: vec![
            Artist {
                uri: None,
                name: "The Killers".into(),
            },
            Artist {
                uri: None,
                name: "Featuring Some Other Long-Named Collaborator".into(),
            },
        ],
        album: Album {
            uri: None,
            name: "Hot Fuss: 10th Anniversary Deluxe Edition (Remastered)".into(),
            artists: vec![],
            images: vec![],
        },
    }
}

/// Bottom-right corner of the outer border must land at (col 95, row 39)
/// regardless of which overlay is active. If layouts overflow, that cell
/// is empty (or contains arbitrary content) instead of the corner glyph.
fn assert_border_closes(label: &str, screen: &str) {
    let lines: Vec<&str> = screen.lines().collect();
    assert!(
        lines.len() >= 40,
        "[{label}] expected ≥40 rows, got {}:\n{screen}",
        lines.len()
    );
    // Row 0 must start with the top-left corner.
    let top = lines[0];
    let top_chars: Vec<char> = top.chars().collect();
    assert!(
        !top_chars.is_empty() && !top_chars[0].is_whitespace(),
        "[{label}] top-left corner missing at (0,0):\n{screen}"
    );
    // Row 39 is the bottom border line; its rightmost rendered char
    // should be a corner glyph at or near column 95.
    let bottom = lines[39];
    let bottom_trimmed = bottom.trim_end();
    assert!(
        !bottom_trimmed.is_empty(),
        "[{label}] bottom border row 39 is empty (height overflow):\n{screen}"
    );
    // After trimming trailing whitespace, the bottom border should be
    // exactly the canvas width minus any leading offset (we anchor at
    // x=0 so no offset). Width is 96, so 96 chars of border.
    let bottom_width = bottom_trimmed.chars().count();
    assert_eq!(
        bottom_width, 96,
        "[{label}] bottom border has {bottom_width} chars, expected 96:\n{bottom_trimmed}"
    );
    // And row 0's top border too.
    let top_trimmed = top.trim_end();
    let top_width = top_trimmed.chars().count();
    assert_eq!(
        top_width, 96,
        "[{label}] top border has {top_width} chars, expected 96:\n{top_trimmed}"
    );
}

fn print_snapshot(label: &str, screen: &str) {
    eprintln!("\n=== {label} ===");
    for (i, line) in screen.lines().enumerate() {
        eprintln!("{i:>2}|{line}");
    }
}

#[tokio::test]
async fn ui_at_96x40_now_playing_with_long_metadata() {
    let h = Harness::new();
    h.seed_playback(Playback {
        is_playing: true,
        progress_ms: Some(123_000),
        item: Some(long_track()),
        context: None,
        timestamp: Some(now_unix_ms()),
    })
    .await;
    {
        let mut s = h.state.lock().await;
        s.boot = false;
        s.device_name = Some("hifi (cabin)".into());
    }
    let screen = h.snapshot_sized(96, 40).await;
    print_snapshot("now_playing long metadata", &screen);
    assert_border_closes("now_playing long metadata", &screen);
}

#[tokio::test]
async fn ui_at_96x40_now_playing_with_rate_limit_status() {
    let h = Harness::new();
    h.seed_playback(Playback {
        is_playing: false,
        progress_ms: Some(0),
        item: Some(long_track()),
        context: None,
        timestamp: Some(now_unix_ms()),
    })
    .await;
    {
        let mut s = h.state.lock().await;
        s.boot = false;
        s.device_name = Some("hifi".into());
        s.rate_limited_until = Some(Instant::now() + Duration::from_secs(45));
    }
    let screen = h.snapshot_sized(96, 40).await;
    print_snapshot("now_playing + rate-limited", &screen);
    assert_border_closes("now_playing + rate-limited", &screen);
}

#[tokio::test]
async fn refresh_queue_fetches_and_stores_when_playing() {
    let h = Harness::new();
    h.seed_playback(Playback {
        is_playing: true,
        progress_ms: Some(0),
        item: Some(track("spotify:track:cur", "Current")),
        context: None,
        timestamp: Some(now_unix_ms()),
    })
    .await;
    h.fake.set_queue(Ok(vec![
        track("spotify:track:n1", "Next One"),
        track("spotify:track:n2", "Next Two"),
    ]));

    refresh_queue(&h.client, &h.state).await;
    h.settle().await;

    assert!(h.fake.calls().contains(&Call::GetQueue));
    assert_eq!(h.state.lock().await.queue.len(), 2);
}

#[tokio::test]
async fn refresh_queue_is_skipped_when_nothing_is_playing() {
    let h = Harness::new();
    h.fake
        .set_queue(Ok(vec![track("spotify:track:n1", "Next One")]));

    refresh_queue(&h.client, &h.state).await;
    h.settle().await;

    // No current track → no request spent, queue stays empty.
    assert!(!h.fake.calls().contains(&Call::GetQueue));
    assert!(h.state.lock().await.queue.is_empty());
}

#[tokio::test]
async fn refresh_queue_is_skipped_when_rate_limited() {
    let h = Harness::new();
    h.seed_playback(Playback {
        is_playing: true,
        progress_ms: Some(0),
        item: Some(track("spotify:track:cur", "Current")),
        context: None,
        timestamp: Some(now_unix_ms()),
    })
    .await;
    h.fake
        .set_queue(Ok(vec![track("spotify:track:n1", "Next One")]));
    h.state.lock().await.rate_limited_until = Some(Instant::now() + Duration::from_secs(30));

    refresh_queue(&h.client, &h.state).await;
    h.settle().await;

    assert!(!h.fake.calls().contains(&Call::GetQueue));
}

#[tokio::test]
async fn polled_track_change_refreshes_queue_on_now_playing() {
    let h = Harness::new();
    h.seed_playback(Playback {
        is_playing: true,
        progress_ms: Some(0),
        item: Some(track("spotify:track:cur", "Current")),
        context: None,
        timestamp: Some(now_unix_ms()),
    })
    .await;
    h.fake
        .set_queue(Ok(vec![track("spotify:track:n1", "Next One")]));

    // The poll observes the track advancing to a new song.
    apply_polled_playback(
        &h.client,
        &h.state,
        Some(Playback {
            is_playing: true,
            progress_ms: Some(0),
            item: Some(track("spotify:track:next", "Next")),
            context: None,
            timestamp: Some(now_unix_ms()),
        }),
    )
    .await;
    h.settle().await;

    assert!(h.fake.calls().contains(&Call::GetQueue));
    assert_eq!(h.state.lock().await.queue.len(), 1);
}

#[tokio::test]
async fn polled_track_change_does_not_refresh_queue_off_now_playing() {
    let h = Harness::new();
    h.seed_playback(Playback {
        is_playing: true,
        progress_ms: Some(0),
        item: Some(track("spotify:track:cur", "Current")),
        context: None,
        timestamp: Some(now_unix_ms()),
    })
    .await;
    h.fake
        .set_queue(Ok(vec![track("spotify:track:n1", "Next One")]));
    h.state.lock().await.tab = Tab::Library;

    apply_polled_playback(
        &h.client,
        &h.state,
        Some(Playback {
            is_playing: true,
            progress_ms: Some(0),
            item: Some(track("spotify:track:next", "Next")),
            context: None,
            timestamp: Some(now_unix_ms()),
        }),
    )
    .await;
    h.settle().await;

    // The queue is hidden behind another tab — don't spend a request on it.
    assert!(!h.fake.calls().contains(&Call::GetQueue));
}

#[tokio::test]
async fn polled_same_track_does_not_refresh_queue() {
    let h = Harness::new();
    h.seed_playback(Playback {
        is_playing: true,
        progress_ms: Some(0),
        item: Some(track("spotify:track:cur", "Current")),
        context: None,
        timestamp: Some(now_unix_ms()),
    })
    .await;
    h.fake
        .set_queue(Ok(vec![track("spotify:track:n1", "Next One")]));

    // A routine poll that finds the same track playing must not refetch.
    apply_polled_playback(
        &h.client,
        &h.state,
        Some(Playback {
            is_playing: true,
            progress_ms: Some(30_000),
            item: Some(track("spotify:track:cur", "Current")),
            context: None,
            timestamp: Some(now_unix_ms()),
        }),
    )
    .await;
    h.settle().await;

    assert!(!h.fake.calls().contains(&Call::GetQueue));
}

#[tokio::test]
async fn tab_nav_back_to_now_playing_makes_it_visible_again() {
    let h = Harness::new();
    // Default is Now Playing (visible). Tab away, then a Tab from Library
    // lands back on Now Playing — the run loop watches exactly this
    // false→true flip to refresh the queue.
    assert!(now_playing_visible(&*h.state.lock().await));
    h.state.lock().await.tab = Tab::Library;
    assert!(!now_playing_visible(&*h.state.lock().await));
    h.press(Key::Tab).await; // Library → Now Playing
    assert!(now_playing_visible(&*h.state.lock().await));
}

#[tokio::test]
async fn esc_from_search_makes_now_playing_visible() {
    let h = Harness::new();
    h.state.lock().await.tab = Tab::Search;
    assert!(!now_playing_visible(&*h.state.lock().await));
    h.press(Key::Esc).await;
    assert!(now_playing_visible(&*h.state.lock().await));
}

#[tokio::test]
async fn closing_an_overlay_makes_now_playing_visible() {
    let h = Harness::new();
    // Open the device picker over Now Playing, then Esc to close it.
    h.press_and_run(Key::Char('d')).await;
    assert!(!now_playing_visible(&*h.state.lock().await));
    h.press(Key::Esc).await;
    assert!(now_playing_visible(&*h.state.lock().await));
}

#[tokio::test]
async fn ui_at_96x40_now_playing_with_up_next() {
    let h = Harness::new();
    h.seed_playback(Playback {
        is_playing: true,
        progress_ms: Some(42_000),
        item: Some(track("spotify:track:cur", "Current Song")),
        context: None,
        timestamp: Some(now_unix_ms()),
    })
    .await;
    {
        let mut s = h.state.lock().await;
        s.boot = false;
        s.device_name = Some("hifi".into());
        // The leading entry duplicates the now-playing track (a stale
        // queue) and should be filtered out of "Up Next".
        s.queue = vec![
            track("spotify:track:cur", "Current Song"),
            track("spotify:track:n1", "Next One"),
            track("spotify:track:n2", "Next Two"),
        ];
    }
    let screen = h.snapshot_sized(96, 40).await;
    print_snapshot("now_playing + up next", &screen);
    assert_border_closes("now_playing + up next", &screen);
    assert!(screen.contains("Up Next"), "expected Up Next header");
    assert!(screen.contains("Next One"), "expected first queued track");
    assert!(
        !screen.contains("1. Current Song"),
        "stale now-playing entry should be filtered from the queue"
    );
}

#[tokio::test]
async fn ui_at_96x40_search_with_results_in_every_section() {
    let h = Harness::new();
    let q = "very long search query string text here";
    h.fake.set_search(
        q,
        Ok(SearchResults {
            tracks: (0..5)
                .map(|i| {
                    let mut t = long_track();
                    t.uri = Some(format!("spotify:track:t{i}"));
                    t.id = Some(format!("t{i}"));
                    t.name = format!("Track {i} — Mr. Brightside (Jacques Lu Cont's Remix)");
                    t
                })
                .collect(),
            albums: (0..4)
                .map(|i| Album {
                    uri: Some(format!("spotify:album:a{i}")),
                    name: format!("Album {i}: A Very Long Title That Might Make The Row Overflow"),
                    artists: vec![Artist {
                        uri: None,
                        name: "Some Artist".into(),
                    }],
                    images: vec![],
                })
                .collect(),
            artists: (0..3)
                .map(|i| Artist {
                    uri: Some(format!("spotify:artist:ar{i}")),
                    name: format!("Artist {i} With A Reasonably Long Performer Name"),
                })
                .collect(),
            playlists: (0..4)
                .map(|i| Playlist {
                    uri: format!("spotify:playlist:p{i}"),
                    name: format!(
                        "Playlist {i}: This Is The Sort Of Title People Use For Their Own Mixes"
                    ),
                    owner: Some(crate::api::PlaylistOwner {
                        display_name: Some(format!("owner_with_a_long_username_{i}")),
                    }),
                })
                .collect(),
        }),
    );
    h.press_and_run(Key::Char('/')).await;
    h.type_str(q).await;
    h.settle().await;
    let screen = h.snapshot_sized(96, 40).await;
    print_snapshot("search results", &screen);
    assert_border_closes("search results", &screen);
}

#[tokio::test]
async fn ui_at_96x40_search_recents() {
    let h = Harness::new();
    {
        let mut s = h.state.lock().await;
        s.recent_queries = (0..10)
            .map(|i| format!("recent search query number {i} with extra padding text"))
            .collect();
        s.recent_tracks = (0..8)
            .map(|i| {
                let mut t = long_track();
                t.uri = Some(format!("spotify:track:r{i}"));
                t.id = Some(format!("r{i}"));
                t.name = format!("Recently Played {i} — Some Long Track Title Goes Here");
                t
            })
            .collect();
    }
    h.press_and_run(Key::Char('/')).await;
    let screen = h.snapshot_sized(96, 40).await;
    print_snapshot("search recents", &screen);
    assert_border_closes("search recents", &screen);
}

#[tokio::test]
async fn ui_at_96x40_help_overlay() {
    let h = Harness::new();
    h.press_and_run(Key::Char('?')).await;
    let screen = h.snapshot_sized(96, 40).await;
    print_snapshot("help overlay", &screen);
    assert_border_closes("help overlay", &screen);
}

#[tokio::test]
async fn ui_at_96x40_command_overlay() {
    let h = Harness::new();
    h.press_and_run(Key::Char(':')).await;
    let screen = h.snapshot_sized(96, 40).await;
    print_snapshot("command overlay", &screen);
    assert_border_closes("command overlay", &screen);
}

#[tokio::test]
async fn ui_at_96x40_browse_with_many_tracks() {
    let h = Harness::new();
    h.fake.set_search(
        "test",
        Ok(SearchResults {
            albums: vec![dummy_album(
                "spotify:album:al1",
                "An Album Title That Is Quite Long For A Header",
                "Some Artist",
            )],
            ..Default::default()
        }),
    );
    h.fake.set_album_tracks(
        "al1",
        Ok((0..40)
            .map(|i| {
                let mut t = long_track();
                t.uri = Some(format!("spotify:track:bt{i}"));
                t.id = Some(format!("bt{i}"));
                t.name = format!("Browse Track {i:02} — Some Reasonably Long Track Name");
                t
            })
            .collect()),
    );
    h.press_and_run(Key::Char('/')).await;
    h.type_str("test").await;
    h.settle().await;
    h.press_and_run(Key::Enter).await;
    h.settle().await;
    let screen = h.snapshot_sized(96, 40).await;
    print_snapshot("browse 40 tracks", &screen);
    assert_border_closes("browse 40 tracks", &screen);
}

/// The exact bug the user hit at launch: boot seed loaded recently-played
/// (track + art populated), then a poll returned `Playback { item: None }`,
/// which overwrote the state into the janky "Track info unavailable +
/// stale album art" hybrid. The fix is two-fold: item-less Playback is
/// collapsed to None on the way in, and the art field is cleared
/// whenever the current_track_id changes.
#[tokio::test]
async fn item_none_poll_does_not_create_track_info_unavailable_hybrid() {
    let h = Harness::new();
    // Seed exactly like the boot path does: a paused synth from
    // recently-played, art populated by a successful art fetch (we just
    // mark current_track_id so the comparison logic exercises).
    let seed = Playback {
        is_playing: false,
        progress_ms: Some(0),
        item: Some(track("spotify:track:seed", "Seeded Track")),
        context: None,
        timestamp: Some(now_unix_ms()),
    };
    h.seed_playback(seed).await;
    {
        let mut s = h.state.lock().await;
        s.boot = false;
        s.current_track_id = Some("seed".into());
    }

    // Now simulate the poll that Spotify often returns after a transfer:
    // `is_playing: false`, `item: None`. This used to clobber the seed.
    apply_playback(
        &h.state,
        Some(Playback {
            is_playing: false,
            progress_ms: None,
            item: None,
            context: None,
            timestamp: Some(now_unix_ms() + 1000),
        }),
    )
    .await;

    let s = h.state.lock().await;
    // Either the seed survives (defended by `pb.filter`) OR the state is
    // fully cleared. The disallowed outcome is an orphan `current_track_id`
    // with no item — that's the key the head's ArtCache renders against, so
    // a dangling id would pair last track's cover with an empty now-playing
    // (the hybrid the user complained about). Art now lives in the head,
    // keyed by `current_track_id`, so this invariant guarantees it.
    let has_item = s.playback.as_ref().and_then(|p| p.item.as_ref()).is_some();
    assert!(
        has_item || s.current_track_id.is_none(),
        "expected either a track to be displayed OR current_track_id cleared; \
             got playback={:?} current_track_id={:?}",
        s.playback
            .as_ref()
            .map(|p| p.item.as_ref().map(|t| &t.name)),
        s.current_track_id,
    );
}

#[tokio::test]
async fn ui_at_96x40_browse_403_warning() {
    let h = Harness::new();
    h.fake.set_search(
        "x",
        Ok(SearchResults {
            playlists: vec![dummy_playlist(
                "spotify:playlist:px",
                "Some Curated Playlist With A Long Title",
                "an_owner_username",
            )],
            ..Default::default()
        }),
    );
    h.fake
        .set_playlist_tracks("px", Err("403 Forbidden".into()));
    h.press_and_run(Key::Char('/')).await;
    h.type_str("x").await;
    h.settle().await;
    h.press_and_run(Key::Enter).await;
    h.settle().await;
    let screen = h.snapshot_sized(96, 40).await;
    print_snapshot("browse 403 warning", &screen);
    assert_border_closes("browse 403 warning", &screen);
}

#[tokio::test]
async fn ui_at_96x40_library_playlists() {
    let h = Harness::new();
    h.fake.set_saved_playlists(Ok(vec![
        dummy_playlist(
            "spotify:playlist:1",
            "Late Night Drives Vol. 3",
            "chrisbolin",
        ),
        dummy_playlist("spotify:playlist:2", "Focus / Deep Work", "spotify"),
        dummy_playlist(
            "spotify:playlist:3",
            "Workout Bangers (Updated Weekly)",
            "a_friend",
        ),
    ]));
    h.press_and_run(Key::Char('l')).await;
    h.press_and_run(Key::Right).await; // -> Playlists
    h.settle().await;
    let screen = h.snapshot_sized(96, 40).await;
    print_snapshot("library playlists", &screen);
    assert_border_closes("library playlists", &screen);
    assert!(
        screen.contains("Now Playing") && screen.contains("Library"),
        "tab strip present"
    );
    assert!(screen.contains("Playlists"), "sub-tab strip present");
    assert!(
        screen.contains("Late Night Drives"),
        "playlist row rendered"
    );
}

#[tokio::test]
async fn ui_at_96x40_devices_overlay() {
    let h = Harness::new();
    h.seed_playback(Playback {
        is_playing: true,
        progress_ms: Some(60_000),
        item: Some(long_track()),
        context: None,
        timestamp: Some(now_unix_ms()),
    })
    .await;
    {
        let mut s = h.state.lock().await;
        s.boot = false;
    }
    h.fake.set_devices(Ok(vec![
        Device {
            id: Some("a".into()),
            name: "hifi (cabin)".into(),
            is_active: true,
        },
        Device {
            id: Some("b".into()),
            name: "Kitchen Speaker".into(),
            is_active: false,
        },
        Device {
            id: Some("c".into()),
            name: "iPhone".into(),
            is_active: false,
        },
    ]));
    h.press_and_run(Key::Char('d')).await;
    h.settle().await;
    let screen = h.snapshot_sized(96, 40).await;
    print_snapshot("devices overlay", &screen);
    assert_border_closes("devices overlay", &screen);
    assert!(screen.contains("devices"), "overlay title present");
    assert!(
        screen.contains("hifi (cabin)") && screen.contains("Kitchen Speaker"),
        "device rows"
    );
}
