//! Unit tests for the folder browser: its state, the local source and
//! the screen.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use super::local::{self, LocalSource};
use super::*;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

fn listed(b: &mut Browser, dirs: &[&str]) {
    let generation = b.generation();
    b.apply(Update::Listed {
        generation,
        result: Ok(Listing {
            dirs: names(dirs),
            ..Listing::default()
        }),
    });
}

fn home() -> String {
    config::expand_tilde("~")
}

#[test]
fn navigation_opens_folders_and_goes_back_up_to_where_it_was() {
    let mut b = Browser::new(Place::Local, "/a", None, None);
    assert_eq!(b.generation(), 1);
    listed(&mut b, &["beta", "Alpha", ".hidden", "gamma"]);
    assert_eq!(b.highlighted(), Some("Alpha"), "sorted, hidden left out");
    b.handle_key(key(KeyCode::Down));
    b.handle_key(key(KeyCode::Char('j')));
    assert_eq!(b.highlighted(), Some("gamma"));
    b.handle_key(key(KeyCode::Down));
    assert_eq!(b.highlighted(), Some("gamma"), "stops at the bottom");
    b.handle_key(key(KeyCode::Char('g')));
    assert_eq!(b.highlighted(), Some("Alpha"));
    b.handle_key(key(KeyCode::Char('G')));
    b.handle_key(key(KeyCode::Char('k')));
    assert_eq!(b.highlighted(), Some("beta"));
    // Enter opens it.
    b.handle_key(key(KeyCode::Enter));
    assert_eq!(b.folder(), "/a/beta");
    assert_eq!(b.generation(), 2);
    assert_eq!(b.highlighted(), None, "nothing until it is listed");
    listed(&mut b, &["x"]);
    b.handle_key(key(KeyCode::Right));
    assert_eq!(b.folder(), "/a/beta/x");
    // ←, Backspace and h go up, and the cursor lands where it was.
    b.handle_key(key(KeyCode::Left));
    assert_eq!(b.folder(), "/a/beta");
    listed(&mut b, &["w", "x", "y"]);
    assert_eq!(b.highlighted(), Some("x"));
    b.handle_key(key(KeyCode::Backspace));
    b.handle_key(key(KeyCode::Char('h')));
    assert_eq!(b.folder(), "/");
    b.handle_key(key(KeyCode::Char('h')));
    assert_eq!(b.folder(), "/", "nothing above the top");
    // ~ goes home and v to the drives.
    b.handle_key(key(KeyCode::Char('~')));
    assert_eq!(b.folder(), home());
    b.handle_key(key(KeyCode::Char('v')));
    assert_eq!(b.folder(), "/Volumes");
    {
        // On Linux the drives are under /media/<user>, /run/media/<user>,
        // /media or /mnt, whichever is there first.
        let _linux = crate::platform::testing::describe(crate::platform::Os::Linux);
        b.handle_key(key(KeyCode::Char('v')));
        assert_eq!(b.folder(), crate::platform::drives_folder());
        assert_ne!(b.folder(), "/Volumes");
    }
    assert_eq!(b.handle_key(key(KeyCode::Esc)), Some(Outcome::Back));
}

#[test]
fn hidden_folders_show_when_toggled_and_the_toggle_stays() {
    let mut b = Browser::new(Place::Local, "/a", None, None);
    listed(&mut b, &["b", ".git"]);
    assert_eq!(b.shown.len(), 1);
    b.handle_key(key(KeyCode::Char('.')));
    assert_eq!(b.highlighted(), Some("b"), "the cursor stays on its folder");
    assert_eq!(b.shown.len(), 2);
    b.handle_key(key(KeyCode::Char('k')));
    assert_eq!(b.highlighted(), Some(".git"));
    b.handle_key(key(KeyCode::Char('j')));
    b.handle_key(key(KeyCode::Enter));
    listed(&mut b, &[".cache", "c"]);
    assert_eq!(b.shown.len(), 2, "still on in the next folder");
}

#[test]
fn the_filter_narrows_the_list_like_the_whole_list_view() {
    let mut b = Browser::new(Place::Local, "/a", None, None);
    listed(&mut b, &["Documents", "Downloads", "Music", "Résumés"]);
    b.handle_key(key(KeyCode::Char('/')));
    assert!(b
        .hints(200)
        .starts_with("type to filter · enter keep · esc clear"));
    for c in "OWN".chars() {
        b.handle_key(key(KeyCode::Char(c)));
    }
    assert_eq!(b.highlighted(), Some("Downloads"));
    assert_eq!(b.shown.len(), 1);
    // While typing, keys are text: j is not a move and Esc clears.
    b.handle_key(key(KeyCode::Backspace));
    b.handle_key(key(KeyCode::Backspace));
    b.handle_key(key(KeyCode::Backspace));
    b.handle_key(key(KeyCode::Char('e')));
    b.handle_key(key(KeyCode::Char('\u{301}')));
    assert_eq!(b.highlighted(), Some("Résumés"), "a decomposed é matches");
    b.handle_key(key(KeyCode::Esc));
    assert_eq!(b.shown.len(), 4);
    // Enter keeps it; Esc then clears it before leaving.
    b.handle_key(key(KeyCode::Char('/')));
    b.handle_key(key(KeyCode::Char('c')));
    b.handle_key(key(KeyCode::Enter));
    assert_eq!(b.shown.len(), 2, "Documents and Music");
    assert!(b.hints(200).ends_with("esc clear"));
    assert_eq!(b.handle_key(key(KeyCode::Esc)), None);
    assert_eq!(b.shown.len(), 4);
    assert_eq!(b.handle_key(key(KeyCode::Esc)), Some(Outcome::Back));
    // Nothing matches.
    let mut b = Browser::new(Place::Local, "/a", None, None);
    listed(&mut b, &["x"]);
    b.handle_key(key(KeyCode::Char('/')));
    b.handle_key(key(KeyCode::Char('q')));
    let text: String = b
        .lines(80, 20)
        .iter()
        .map(|l| l.to_string() + "\n")
        .collect();
    assert!(
        text.contains("no folder matches · esc clears the filter"),
        "{text}"
    );
}

#[test]
fn space_chooses_the_highlighted_folder_and_c_this_one() {
    let docs = format!("{}/Documents", home());
    let mut b = Browser::new(Place::Local, &docs, None, None);
    listed(&mut b, &["Taxes", "Projects"]);
    assert_eq!(
        b.handle_key(key(KeyCode::Char(' '))),
        Some(Outcome::Chosen("~/Documents/Projects".into())),
        "tilde form"
    );
    assert_eq!(
        b.handle_key(key(KeyCode::Char('c'))),
        Some(Outcome::Chosen("~/Documents".into()))
    );
    let mut b = Browser::new(Place::Local, "/Volumes", None, None);
    listed(&mut b, &["Backup"]);
    assert_eq!(
        b.handle_key(key(KeyCode::Char(' '))),
        Some(Outcome::Chosen("/Volumes/Backup".into()))
    );
    // OneDrive: relative to the drive root, which is empty.
    let place = Place::Remote {
        label: "work".into(),
    };
    let mut b = Browser::new(place.clone(), "", None, None);
    listed(&mut b, &["Backups"]);
    assert_eq!(
        b.handle_key(key(KeyCode::Char(' '))),
        Some(Outcome::Chosen("Backups".into()))
    );
    assert_eq!(
        b.handle_key(key(KeyCode::Char('c'))),
        Some(Outcome::Chosen(String::new()))
    );
    // ~ and v are for this Mac only.
    b.handle_key(key(KeyCode::Char('v')));
    assert_eq!(b.folder(), "");
}

#[test]
fn a_missing_onedrive_folder_starts_from_the_nearest_one_above() {
    let place = Place::Remote {
        label: "work".into(),
    };
    let mut b = Browser::new(place, "Backups/New/Deeper", None, None);
    let missing = |g| Update::Listed {
        generation: g,
        result: Err(ListFailure {
            missing: true,
            message: "not found".into(),
        }),
    };
    b.apply(missing(1));
    assert_eq!(b.folder(), "Backups/New");
    b.apply(missing(2));
    assert_eq!(b.folder(), "Backups");
    listed(&mut b, &["Documents"]);
    assert_eq!(b.highlighted(), Some("Documents"));
    // After that, a folder that is gone is an error, not a move.
    b.handle_key(key(KeyCode::Enter));
    b.apply(missing(4));
    assert_eq!(b.folder(), "Backups/Documents");
    assert_eq!(b.load, Load::Failed("not found".into()));
}

#[test]
fn sizes_arriving_late_or_out_of_order_are_handled() {
    let mut b = Browser::new(Place::Local, "/a", None, None);
    listed(&mut b, &["x", "y"]);
    let size = |g, name: &str, size| Update::Size {
        generation: g,
        name: name.into(),
        size,
    };
    assert!(b.apply(size(
        1,
        "x",
        Size::Measuring {
            bytes: 10,
            partial: false
        }
    )));
    assert!(b.apply(size(
        1,
        "x",
        Size::Done {
            bytes: 50,
            partial: true
        }
    )));
    // A running total that arrives after the final size is dropped.
    assert!(!b.apply(size(
        1,
        "x",
        Size::Measuring {
            bytes: 30,
            partial: false
        }
    )));
    assert_eq!(
        b.sizes["x"],
        Size::Done {
            bytes: 50,
            partial: true
        }
    );
    // So is anything about a folder no longer shown.
    b.handle_key(key(KeyCode::Enter));
    listed(&mut b, &["z"]);
    assert!(!b.apply(size(
        1,
        "y",
        Size::Done {
            bytes: 1,
            partial: false
        }
    )));
    assert!(!b.apply(Update::Listed {
        generation: 1,
        result: Ok(Listing::default())
    }));
    assert!(b.sizes.is_empty());
    assert_eq!(b.highlighted(), Some("z"));
    // The free space is the drive's, whatever the folder.
    assert!(b.apply(Update::Free(5)));
    assert_eq!(b.free, Some(5));
}

#[test]
fn the_local_start_is_the_nearest_existing_folder() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::create_dir(root.join("a")).unwrap();
    let at = |rel: &str| root.join(rel).display().to_string();
    assert_eq!(local_start(&at("a")), at("a"));
    assert_eq!(local_start(&at("a/not/there")), at("a"));
    assert_eq!(local_start(""), home());
    assert_eq!(local_start("relative/path"), home());
    assert_eq!(local_start("~"), home());
}

/// Waits for updates until `done` says so, failing after five seconds.
fn wait_for(rx: &mpsc::Receiver<Update>, mut done: impl FnMut(&Update) -> bool) -> Vec<Update> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut got = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let update = rx.recv_timeout(left).expect("no update in time");
        let finished = done(&update);
        got.push(update);
        if finished {
            return got;
        }
    }
}

#[test]
fn the_local_source_lists_measures_and_caches() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("a/deep")).unwrap();
    std::fs::write(root.join("a/one"), vec![0u8; 1000]).unwrap();
    std::fs::write(root.join("a/deep/two"), vec![0u8; 500]).unwrap();
    std::fs::create_dir(root.join("empty")).unwrap();
    std::fs::write(root.join("here.txt"), "hello").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.join("a"), root.join("link")).unwrap();
    let (tx, rx) = mpsc::channel();
    let cache = new_cache();
    let mut source = LocalSource::new(tx, cache.clone());
    source.open(&root.display().to_string(), 7);
    let mut done = 0;
    let got = wait_for(&rx, |u| {
        if matches!(
            u,
            Update::Size {
                size: Size::Done { .. },
                ..
            }
        ) {
            done += 1;
        }
        done == 2
    });
    let Update::Listed {
        generation: 7,
        result: Ok(listing),
    } = &got[0]
    else {
        panic!("{got:?}");
    };
    let mut dirs = listing.dirs.clone();
    dirs.sort();
    assert_eq!(dirs, vec!["a", "empty"], "a link to a folder is not a row");
    assert_eq!((listing.files, listing.file_bytes), (1, 5));
    assert!(listing.free.is_some_and(|f| f > 0));
    let final_of = |name: &str| {
        got.iter().find_map(|u| match u {
            Update::Size {
                name: n,
                size: size @ Size::Done { .. },
                ..
            } if n == name => Some(*size),
            _ => None,
        })
    };
    assert_eq!(
        final_of("a"),
        Some(Size::Done {
            bytes: 1500,
            partial: false
        })
    );
    assert_eq!(
        final_of("empty"),
        Some(Size::Done {
            bytes: 0,
            partial: false
        })
    );
    // Opened again, the sizes come from the cache at once.
    assert_eq!(cache.lock().unwrap().len(), 2);
    source.open(&root.display().to_string(), 8);
    let mut done = 0;
    wait_for(&rx, |u| {
        if matches!(
            u,
            Update::Size {
                generation: 8,
                size: Size::Done { .. },
                ..
            }
        ) {
            done += 1;
        }
        done == 2
    });
    // r forgets them.
    source.forget(&root.display().to_string());
    assert!(cache.lock().unwrap().is_empty());
}

#[test]
fn opening_another_folder_cancels_the_work_on_the_one_before() {
    let dir = tempfile::tempdir().unwrap();
    let (tx, _rx) = mpsc::channel();
    let mut source = LocalSource::new(tx, new_cache());
    source.open(&dir.path().display().to_string(), 1);
    let first = source.cancel_flag();
    assert!(!first.load(Ordering::SeqCst));
    source.open("/", 2);
    assert!(
        first.load(Ordering::SeqCst),
        "the first folder's work stops"
    );
    let second = source.cancel_flag();
    assert!(!second.load(Ordering::SeqCst));
    drop(source);
    assert!(
        second.load(Ordering::SeqCst),
        "leaving the browser stops it all"
    );
}

#[test]
fn a_walk_stops_when_cancelled_and_marks_what_it_could_not_read() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("open")).unwrap();
    std::fs::write(root.join("open/f"), vec![0u8; 100]).unwrap();
    let cancelled = AtomicBool::new(true);
    assert_eq!(local::walk(root, &cancelled, &mut |_, _| {}), None);
    let go = AtomicBool::new(false);
    assert_eq!(local::walk(root, &go, &mut |_, _| {}), Some((100, false)));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let locked = root.join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("g"), vec![0u8; 50]).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = local::walk(root, &go, &mut |_, _| {});
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            result,
            Some((100, true)),
            "counts what it could read, marked +"
        );
    }
}

/// A folder with sizes known, one being measured, one partial and one not
/// started, as the snapshots show it.
fn sample() -> Browser {
    let docs = format!("{}/Documents", home());
    let mut b = Browser::new(Place::Local, &docs, None, None);
    b.apply(Update::Listed {
        generation: 1,
        result: Ok(Listing {
            dirs: names(&[
                "Projects",
                "Archive",
                "Photo Library with a very long name that does not fit anywhere at all",
                "Taxes",
                "Empty",
                "Old",
                "Time Machine",
                ".git",
            ]),
            files: 12,
            file_bytes: 48_200_000,
            free: Some(95_600_000_000),
        }),
    });
    let sizes = [
        (
            "Projects",
            Size::Done {
                bytes: 12_400_000_000,
                partial: false,
            },
        ),
        (
            "Archive",
            Size::Done {
                bytes: 3_100_000_000,
                partial: false,
            },
        ),
        (
            "Photo Library with a very long name that does not fit anywhere at all",
            Size::Done {
                bytes: 8_840_000_000,
                partial: true,
            },
        ),
        (
            "Taxes",
            Size::Measuring {
                bytes: 120_000_000,
                partial: false,
            },
        ),
        (
            "Empty",
            Size::Done {
                bytes: 0,
                partial: false,
            },
        ),
        (
            "Time Machine",
            Size::Volume {
                used: 1_200_000_000_000,
            },
        ),
    ];
    for (name, size) in sizes {
        b.apply(Update::Size {
            generation: 1,
            name: name.into(),
            size,
        });
    }
    b.handle_key(key(KeyCode::Down));
    b
}

fn render(b: &Browser, width: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, 20)).unwrap();
    terminal.draw(|f| b.render(f)).unwrap();
    terminal.backend().to_string()
}

fn snapshot(name: &str, value: String) {
    insta::with_settings!({
        snapshot_path => concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots"),
        prepend_module_to_snapshot => false,
    }, {
        insta::assert_snapshot!(name, value);
    });
}

#[test]
fn renders_at_80_and_120_columns() {
    let b = sample();
    snapshot("browse_80", render(&b, 80));
    snapshot("browse_120", render(&b, 120));
}

#[test]
fn renders_listing_errors_and_onedrive() {
    let place = Place::Remote {
        label: "work".into(),
    };
    let mut b = Browser::new(place, "Backups", None, None);
    let text = render(&b, 80);
    assert!(text.contains("listing…"), "{text}");
    assert!(text.contains("work:Backups"), "{text}");
    assert!(!text.contains("free"), "no free space until about answers");
    b.apply(Update::Listed {
        generation: 1,
        result: Err(ListFailure {
            missing: false,
            message: "rclone lsjson failed: token expired".into(),
        }),
    });
    let text = render(&b, 80);
    assert!(
        text.contains("✗ could not list: rclone lsjson failed: token expired"),
        "{text}"
    );
    // A narrow terminal shortens the keys with an ellipsis.
    let b = sample();
    let hints = b.hints(40);
    assert!(ui::width(&hints) <= 40 && hints.ends_with('…'), "{hints}");
}

#[test]
fn ctrl_and_alt_keys_do_nothing_but_ctrl_h_goes_up() {
    let mut b = Browser::new(Place::Local, "/a/b", None, None);
    listed(&mut b, &["c"]);
    for k in [
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::ALT),
        KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
        KeyEvent::new(KeyCode::Char('.'), KeyModifiers::CONTROL),
    ] {
        assert_eq!(b.handle_key(k), None, "{k:?}");
    }
    assert_eq!(b.folder(), "/a/b");
    assert!(!b.hidden);
    b.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL));
    assert_eq!(b.folder(), "/a", "Ctrl-H is Backspace");
    // In the filter it deletes a letter.
    listed(&mut b, &["xy"]);
    b.handle_key(key(KeyCode::Char('/')));
    b.handle_key(key(KeyCode::Char('h')));
    b.handle_key(key(KeyCode::Char('.')));
    assert_eq!(b.filter, "h.", "h and . are text while filtering");
    b.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL));
    assert_eq!(b.filter, "h");
}

#[test]
fn refreshing_drops_sizes_from_before_the_refresh() {
    let mut b = Browser::new(Place::Local, "/a", None, None);
    listed(&mut b, &["x"]);
    b.handle_key(key(KeyCode::Char('r')));
    assert_eq!(b.folder(), "/a", "the same folder");
    assert_eq!(b.generation(), 2, "a new generation");
    listed(&mut b, &["x"]);
    assert_eq!(b.highlighted(), Some("x"));
    // A size measured before the refresh arrives late: dropped.
    assert!(!b.apply(Update::Size {
        generation: 1,
        name: "x".into(),
        size: Size::Done {
            bytes: 9,
            partial: false
        },
    }));
    assert!(b.sizes.is_empty());
}

#[test]
fn a_subfolder_on_another_device_is_a_mount_point() {
    let found = |name: &str, dir: bool, device: u64| local::Found {
        name: name.into(),
        dir,
        file: !dir,
        len: if dir { 0 } else { 10 },
        device,
    };
    let (listing, mounts) = local::sort_out(
        vec![
            found("Backup", true, 7),
            found("Shared", true, 1),
            found("NAS", true, 9),
            found("notes.txt", false, 1),
        ],
        1,
    );
    assert_eq!(listing.dirs, names(&["Backup", "Shared", "NAS"]));
    assert_eq!(mounts, names(&["Backup", "NAS"]));
    assert_eq!((listing.files, listing.file_bytes), (1, 10));
}

#[test]
fn the_keys_fit_80_columns_without_cutting() {
    let b = sample();
    let hints = b.hints(76);
    assert_eq!(
        hints,
        "enter open · h up · space choose · c here · . hidden · / filter · esc back"
    );
}
