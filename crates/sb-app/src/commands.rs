//! Key → command dispatch (DESIGN.md §11): one keymap resolves keys to
//! internal actions or external programs run on the selected clip.
//!
//! Movement is fully remappable: hjkl and the arrows bind to the
//! context-sensitive `move_left`/`move_down`/`move_up`/`move_right`
//! actions by default (grid/quickview move the selection; in fullview —
//! chapter bar up or not — left/right step chapters instead). The plain
//! linear `next`/`prev` actions remain for bindings that always want a
//! selection move (auto-skip fires `next`). User `[keys]`/`[commands]`
//! entries overlay the built-in defaults.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;

use sb_window::Key;

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommandSpec {
    /// Launch a program on the selected clip ("external" accepted as a
    /// legacy alias for configs written before the rename).
    #[serde(alias = "external")]
    Launch {
        program: String,
        #[serde(default)]
        args: Vec<String>,
    },
    Internal {
        action: String,
        /// Action parameter — today only the skip actions read it (the
        /// fraction of the clip to jump, overriding `skip_fraction`).
        #[serde(default)]
        amount: Option<f32>,
    },
}

/// A resolved keypress: either external launch data (template-unexpanded)
/// or an internal action for the app to perform.
#[derive(Debug, Clone)]
pub enum Action {
    Spawn {
        program: String,
        args: Vec<String>,
    },
    Quit,
    /// `fast` = borderless desktop-sized window (mpv's no-native-fs)
    /// instead of macOS native fullscreen; either exits whichever mode
    /// is active. Bind "fullscreen" and/or "fast_fullscreen".
    ToggleFullscreen {
        fast: bool,
    },
    CopyPath,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    ToggleFocusPause,
    Quickview,
    /// Toggle fullview: the selected clip fills the whole window,
    /// letterboxed on black. Esc or the same key exits.
    Fullview,
    /// Toggle the fullview chapter bar (entering fullview if needed):
    /// filmstrip-style chips — real chapters or synthesized checkpoints
    /// — slide up from the bottom; clicking one jumps the video there.
    ChapterMode,
    /// Jump the playing clip by a fraction of its duration; `None` falls
    /// back to the tuning `skip_fraction`.
    Skip {
        forward: bool,
        amount: Option<f32>,
    },
    /// Advance the selection to the next clip (linear order; row-end
    /// wraps to the next row). Auto-skip fires this when its countdown
    /// expires.
    SelectNext,
    /// Selection back one clip (linear order).
    SelectPrev,
    /// Context-sensitive movement — the default hjkl/arrow bindings
    /// (`move_left`/`move_down`/`move_up`/`move_right`). In the grid and
    /// quickview it moves the selection; in fullview (chapter bar up or
    /// not) horizontal moves step the playing clip between chapters.
    Move {
        dx: i32,
        dy: i32,
    },
    /// Re-ingest from the selected clip's parent directory (siblings view).
    OpenParent,
    /// Swap the library to an explicit directory. Bound with an inline
    /// path argument in `[keys]`, e.g. `"1" = "open '~/Movies'"` —
    /// honours the ingest `recurse` flag like a CLI path arg.
    OpenLibrary {
        dir: PathBuf,
    },
    /// Select a uniformly random other clip in the library.
    JumpRandom,
    /// Shuffle the whole grid in place (Fisher–Yates over the clip order).
    ShuffleLibrary,
    /// Auto-skip: while quickview or fullview is up, once the selected
    /// clip has played `auto_skip_s`, selection moves to the next clip
    /// (wraps at the end). Back in the grid the countdown suspends.
    ToggleAutoSkip,
}

#[derive(Debug, Clone)]
pub struct KeyMap {
    keys: HashMap<String, String>,
    commands: HashMap<String, CommandSpec>,
}

impl Default for KeyMap {
    fn default() -> Self {
        let mut keys = HashMap::new();
        for (k, v) in [
            ("enter", "exec"),
            ("o", "exec"),
            ("space", "quickview"),
            ("tab", "fullview"),
            ("c", "copy_path"),
            ("q", "quit"),
            ("f", "fullscreen"),
            ("-", "zoom_out"),
            ("=", "zoom_in"),
            ("+", "zoom_in"),
            ("0", "zoom_reset"),
            ("p", "toggle_focus_pause"),
            ("r", "reveal"),
            ("[", "skip_back"),
            ("]", "skip_forward"),
            ("g", "chapter_mode"),
            ("D", "open_parent"),
            ("x", "jump_random"),
            ("s", "shuffle_library"),
            ("t", "toggle_auto_skip"),
            ("h", "move_left"),
            ("j", "move_down"),
            ("k", "move_up"),
            ("l", "move_right"),
            ("left", "move_left"),
            ("down", "move_down"),
            ("up", "move_up"),
            ("right", "move_right"),
        ] {
            keys.insert(k.to_string(), v.to_string());
        }
        let mut commands = HashMap::new();
        // The default "play in mpv" launcher. Bound to enter/o; `exec` is the
        // canonical name, `launch` an alias (both are just command entries a
        // user config can override). `open` is NOT this — it's the library
        // swap (see internal_action) so `"1" = "open '~/Movies'"` reads well.
        for name in ["exec", "launch"] {
            commands.insert(
                name.to_string(),
                CommandSpec::Launch {
                    program: "mpv".to_string(),
                    args: vec!["{path}".to_string()],
                },
            );
        }
        // Preview is deliberately distinct from open: a macOS-Quick-Look-ish
        // peek — windowed, floating on top, looping, sized sensibly.
        commands.insert(
            "preview".to_string(),
            CommandSpec::Launch {
                program: "mpv".to_string(),
                args: [
                    "--no-fs",
                    "--loop-file=inf",
                    "--ontop",
                    "--no-terminal",
                    "--force-window=immediate",
                    "--autofit-larger=70%x70%",
                    "{path}",
                ]
                .map(String::from)
                .to_vec(),
            },
        );
        // Reveal in Finder — like "open", just another external command;
        // remap or replace it freely (macOS default).
        commands.insert(
            "reveal".to_string(),
            CommandSpec::Launch {
                program: "open".to_string(),
                args: vec!["-R".to_string(), "{path}".to_string()],
            },
        );
        Self { keys, commands }
    }
}

impl KeyMap {
    /// Built-in defaults overlaid with the user's `[keys]`/`[commands]`.
    pub fn merged(
        user_keys: HashMap<String, String>,
        user_commands: HashMap<String, CommandSpec>,
    ) -> Self {
        let mut map = Self::default();
        map.keys.extend(user_keys);
        map.commands.extend(user_commands);
        map
    }

    pub fn action_for(&self, key: &Key) -> Option<Action> {
        // A binding value is a command name optionally followed by an inline
        // argument: `"1" = "open_library '~/Movies'"`. Only internal actions
        // that take a path read the argument today.
        let (name, arg) = split_binding(self.keys.get(&key_name(key)?)?);
        match self.commands.get(name) {
            Some(CommandSpec::Launch { program, args }) => Some(Action::Spawn {
                program: program.clone(),
                args: args.clone(),
            }),
            Some(CommandSpec::Internal { action, amount }) => internal_action(action, *amount, arg),
            // A command name with no [commands] entry is an internal action.
            None => internal_action(name, None, arg),
        }
    }
}

/// Split a binding value into its command name and an optional inline
/// argument. The first whitespace-delimited token is the name; the trimmed
/// remainder is the argument, with a single wrapping pair of `'`/`"` quotes
/// stripped so paths with spaces can be quoted.
fn split_binding(value: &str) -> (&str, Option<&str>) {
    match value.trim().split_once(char::is_whitespace) {
        Some((name, rest)) => (name, Some(unquote(rest.trim()))),
        None => (value.trim(), None),
    }
}

fn unquote(s: &str) -> &str {
    let b = s.as_bytes();
    if b.len() >= 2 && (b[0] == b'\'' || b[0] == b'"') && b[b.len() - 1] == b[0] {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

fn key_name(key: &Key) -> Option<String> {
    Some(match key {
        Key::Enter => "enter".to_string(),
        Key::Space => "space".to_string(),
        Key::Escape => "esc".to_string(),
        Key::Tab => "tab".to_string(),
        Key::Char(c) => c.to_string(),
        // Arrows resolve like any key (bound to the move_* actions by
        // default), so movement is remappable end to end.
        Key::Left => "left".to_string(),
        Key::Right => "right".to_string(),
        Key::Up => "up".to_string(),
        Key::Down => "down".to_string(),
    })
}

fn internal_action(name: &str, amount: Option<f32>, arg: Option<&str>) -> Option<Action> {
    Some(match name {
        "quit" => Action::Quit,
        "fullscreen" | "toggle_fullscreen" => Action::ToggleFullscreen { fast: false },
        "fast_fullscreen" | "toggle_fast_fullscreen" => Action::ToggleFullscreen { fast: true },
        "copy_path" => Action::CopyPath,
        "zoom_in" => Action::ZoomIn,
        "zoom_out" => Action::ZoomOut,
        "zoom_reset" => Action::ZoomReset,
        "toggle_focus_pause" => Action::ToggleFocusPause,
        "quickview" => Action::Quickview,
        "fullview" | "toggle_fullview" => Action::Fullview,
        "chapter_mode" | "chapters" => Action::ChapterMode,
        "skip_forward" => Action::Skip {
            forward: true,
            amount,
        },
        "skip_back" | "skip_backward" => Action::Skip {
            forward: false,
            amount,
        },
        "next" | "select_next" => Action::SelectNext,
        "prev" | "previous" | "select_prev" => Action::SelectPrev,
        "move_left" => Action::Move { dx: -1, dy: 0 },
        "move_right" => Action::Move { dx: 1, dy: 0 },
        "move_up" => Action::Move { dx: 0, dy: -1 },
        "move_down" => Action::Move { dx: 0, dy: 1 },
        "open_parent" | "browse_parent" => Action::OpenParent,
        // Swap the library to an inline path: `"1" = "open '~/Movies'"`. The
        // mpv launcher is `exec`/`launch` now, so `open` reads naturally here.
        "open" | "open_library" | "open_dir" | "library" => match arg {
            Some(a) => Action::OpenLibrary {
                dir: PathBuf::from(expand_tilde(a)),
            },
            None => {
                log::warn!(
                    "'{name}' needs a path argument, e.g. \"1\" = \"open_library '~/Movies'\""
                );
                return None;
            }
        },
        "jump_random" | "jump_to_random" => Action::JumpRandom,
        "shuffle_library" | "shuffle" => Action::ShuffleLibrary,
        // The old "skip timer" spellings stay as config aliases.
        "toggle_auto_skip" | "auto_skip" | "toggle_skip_timer" | "skip_timer" => {
            Action::ToggleAutoSkip
        }
        other => {
            log::warn!(
                "unknown command '{other}': no [commands.{other}] entry and not a built-in action"
            );
            return None;
        }
    })
}

/// Launch an external command against the selected clip, detached from the
/// UI. `{path}`, `{dir}` and `{name}` expand in every arg.
///
/// The spawn itself runs on a throwaway thread (P1.6): process creation
/// costs single-digit milliseconds on macOS, and this is called from the
/// render thread mid-playback — `o` opening mpv must not eat a video
/// frame. The same thread reaps the child (no zombies); spawn failures
/// log from there instead of returning.
pub fn spawn_external(program: &str, args: &[String], path: &Path) {
    let program = expand_tilde(program);
    let args: Vec<String> = args.iter().map(|a| expand_template(a, path)).collect();
    log::info!("run: {program} {}", args.join(" "));
    std::thread::spawn(move || {
        match Command::new(&program)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                let _ = child.wait();
            }
            Err(e) => log::warn!("failed to run {program}: {e}"),
        }
    });
}

pub fn copy_path(path: &Path) {
    // Spawn + pipe-feed + reap all on a throwaway thread (P1.6): the
    // caller is the render thread, and process creation is a hitch.
    #[cfg(target_os = "macos")]
    {
        use std::io::Write;
        let bytes = path.as_os_str().as_encoded_bytes().to_vec();
        let shown = path.display().to_string();
        std::thread::spawn(move || {
            match Command::new("pbcopy")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(mut child) => {
                    if let Some(mut stdin) = child.stdin.take() {
                        let _ = stdin.write_all(&bytes);
                        drop(stdin);
                    }
                    let _ = child.wait();
                    log::info!("copied path: {shown}");
                }
                Err(e) => log::warn!("pbcopy failed: {e}"),
            }
        });
    }
    #[cfg(not(target_os = "macos"))]
    log::warn!(
        "copy_path is not implemented on this platform ({})",
        path.display()
    );
}

fn expand_template(arg: &str, path: &Path) -> String {
    let dir = path
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    arg.replace("{path}", &path.to_string_lossy())
        .replace("{dir}", &dir)
        .replace("{name}", &name)
}

fn expand_tilde(program: &str) -> String {
    if let Some(rest) = program.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return Path::new(&home).join(rest).to_string_lossy().into_owned();
    }
    program.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn templates_expand() {
        let p = PathBuf::from("/clips/sub/take one.mp4");
        assert_eq!(expand_template("{path}", &p), "/clips/sub/take one.mp4");
        assert_eq!(expand_template("{dir}", &p), "/clips/sub");
        assert_eq!(
            expand_template("--title={name}", &p),
            "--title=take one.mp4"
        );
    }

    #[test]
    fn defaults_resolve() {
        let map = KeyMap::default();
        assert!(matches!(
            map.action_for(&Key::Char('q')),
            Some(Action::Quit)
        ));
        assert!(matches!(
            map.action_for(&Key::Enter),
            Some(Action::Spawn { ref program, .. }) if program == "mpv"
        ));
        assert!(map.action_for(&Key::Char('y')).is_none());
    }

    #[test]
    fn movement_defaults_resolve_and_stay_remappable() {
        let map = KeyMap::default();
        // hjkl and the arrows bind to the context-sensitive move_* actions.
        for (key, dx, dy) in [
            (Key::Char('h'), -1, 0),
            (Key::Char('l'), 1, 0),
            (Key::Char('k'), 0, -1),
            (Key::Char('j'), 0, 1),
            (Key::Left, -1, 0),
            (Key::Right, 1, 0),
            (Key::Up, 0, -1),
            (Key::Down, 0, 1),
        ] {
            assert!(
                matches!(
                    map.action_for(&key),
                    Some(Action::Move { dx: ax, dy: ay }) if ax == dx && ay == dy
                ),
                "wrong action for {key:?}"
            );
        }
        // move_* and next/prev are ordinary commands: a user config can
        // rebind any of them.
        let keys = HashMap::from([
            ("n".to_string(), "next".to_string()),
            ("j".to_string(), "prev".to_string()),
        ]);
        let map = KeyMap::merged(keys, HashMap::new());
        assert!(matches!(
            map.action_for(&Key::Char('n')),
            Some(Action::SelectNext)
        ));
        assert!(matches!(
            map.action_for(&Key::Char('j')),
            Some(Action::SelectPrev)
        ));
    }

    #[test]
    fn skip_and_parent_defaults_resolve() {
        let map = KeyMap::default();
        assert!(matches!(
            map.action_for(&Key::Char('[')),
            Some(Action::Skip {
                forward: false,
                amount: None
            })
        ));
        assert!(matches!(
            map.action_for(&Key::Char(']')),
            Some(Action::Skip {
                forward: true,
                amount: None
            })
        ));
        assert!(matches!(
            map.action_for(&Key::Char('D')),
            Some(Action::OpenParent)
        ));
    }

    #[test]
    fn random_shuffle_and_auto_skip_defaults_resolve() {
        let map = KeyMap::default();
        assert!(matches!(
            map.action_for(&Key::Char('x')),
            Some(Action::JumpRandom)
        ));
        assert!(matches!(
            map.action_for(&Key::Char('s')),
            Some(Action::ShuffleLibrary)
        ));
        assert!(matches!(
            map.action_for(&Key::Char('t')),
            Some(Action::ToggleAutoSkip)
        ));
        // The alias spellings resolve too.
        assert!(matches!(
            internal_action("jump_to_random", None, None),
            Some(Action::JumpRandom)
        ));
        assert!(matches!(
            internal_action("shuffle", None, None),
            Some(Action::ShuffleLibrary)
        ));
        // Configs written before the auto-skip rename keep working.
        for legacy in ["toggle_skip_timer", "skip_timer", "auto_skip"] {
            assert!(matches!(
                internal_action(legacy, None, None),
                Some(Action::ToggleAutoSkip)
            ));
        }
    }

    #[test]
    fn fullview_binds_to_tab() {
        let map = KeyMap::default();
        assert!(matches!(map.action_for(&Key::Tab), Some(Action::Fullview)));
    }

    #[test]
    fn chapter_mode_binds_to_g() {
        let map = KeyMap::default();
        assert!(matches!(
            map.action_for(&Key::Char('g')),
            Some(Action::ChapterMode)
        ));
        // Alias spelling resolves too.
        assert!(matches!(
            internal_action("chapters", None, None),
            Some(Action::ChapterMode)
        ));
    }

    #[test]
    fn fullscreen_flavors_resolve() {
        let map = KeyMap::default();
        assert!(matches!(
            map.action_for(&Key::Char('f')),
            Some(Action::ToggleFullscreen { fast: false })
        ));
        let keys = HashMap::from([("F".to_string(), "fast_fullscreen".to_string())]);
        let map = KeyMap::merged(keys, HashMap::new());
        assert!(matches!(
            map.action_for(&Key::Char('F')),
            Some(Action::ToggleFullscreen { fast: true })
        ));
    }

    #[test]
    fn internal_amount_reaches_the_action() {
        let spec: CommandSpec =
            toml::from_str("type = \"internal\"\naction = \"skip_forward\"\namount = 0.25")
                .unwrap();
        let keys = HashMap::from([("s".to_string(), "big_skip".to_string())]);
        let commands = HashMap::from([("big_skip".to_string(), spec)]);
        let map = KeyMap::merged(keys, commands);
        match map.action_for(&Key::Char('s')) {
            Some(Action::Skip {
                forward: true,
                amount: Some(a),
            }) => assert!((a - 0.25).abs() < 1e-6),
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn open_library_binding_parses_an_inline_path() {
        // A key value can carry an inline argument: name + quoted path.
        let keys = HashMap::from([
            ("1".to_string(), "open '~/Movies/Prawns/'".to_string()),
            ("2".to_string(), "library /Volumes/Prawns/Movies".to_string()),
        ]);
        let map = KeyMap::merged(keys, HashMap::new());
        let home = std::env::var_os("HOME").map(PathBuf::from);
        // `open` is the library swap now (the mpv launcher is `exec`).
        match map.action_for(&Key::Char('1')) {
            Some(Action::OpenLibrary { dir }) => {
                if let Some(home) = home {
                    assert_eq!(dir, home.join("Movies/Prawns/"));
                }
            }
            other => panic!("unexpected action: {other:?}"),
        }
        // The `library` alias resolves, and an unquoted absolute path works.
        assert!(matches!(
            map.action_for(&Key::Char('2')),
            Some(Action::OpenLibrary { ref dir }) if dir == Path::new("/Volumes/Prawns/Movies")
        ));
        // Without a path argument the binding is ignored (warns), not a panic.
        assert!(internal_action("open", None, None).is_none());
    }

    #[test]
    fn several_keys_can_share_one_action() {
        // Arrows ship bound to move_* alongside hjkl by default, and a user
        // can point extra keys at the same action.
        let map = KeyMap::default();
        for key in [Key::Up, Key::Char('k')] {
            assert!(matches!(
                map.action_for(&key),
                Some(Action::Move { dx: 0, dy: -1 })
            ));
        }
        let keys = HashMap::from([
            ("w".to_string(), "move_up".to_string()),
            ("up".to_string(), "move_up".to_string()),
        ]);
        let map = KeyMap::merged(keys, HashMap::new());
        for key in [Key::Char('w'), Key::Up, Key::Char('k')] {
            assert!(
                matches!(map.action_for(&key), Some(Action::Move { dx: 0, dy: -1 })),
                "wrong action for {key:?}"
            );
        }
    }

    #[test]
    fn launch_parses_and_external_stays_an_alias() {
        for ty in ["launch", "external"] {
            let spec: CommandSpec =
                toml::from_str(&format!("type = \"{ty}\"\nprogram = \"mpv\"")).unwrap();
            assert!(matches!(spec, CommandSpec::Launch { .. }), "type = {ty}");
        }
    }

    #[test]
    fn user_entries_override_defaults() {
        let keys = HashMap::from([("o".to_string(), "reveal".to_string())]);
        let commands = HashMap::from([(
            "reveal".to_string(),
            CommandSpec::Launch {
                program: "open".to_string(),
                args: vec!["-R".to_string(), "{path}".to_string()],
            },
        )]);
        let map = KeyMap::merged(keys, commands);
        assert!(matches!(
            map.action_for(&Key::Char('o')),
            Some(Action::Spawn { ref program, .. }) if program == "open"
        ));
        // Untouched defaults survive the merge.
        assert!(matches!(
            map.action_for(&Key::Char('q')),
            Some(Action::Quit)
        ));
    }
}
