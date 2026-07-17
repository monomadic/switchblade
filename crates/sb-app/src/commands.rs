//! Key → command dispatch (PLAN.md §11): one keymap resolves keys to
//! internal actions or external programs run on the selected clip.
//!
//! Movement keys (hjkl/arrows) are reserved and never reach the keymap.
//! User `[keys]`/`[commands]` entries overlay the built-in defaults.

use std::collections::HashMap;
use std::path::Path;
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
    ToggleAnim,
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
    /// Re-ingest from the selected clip's parent directory (siblings view).
    OpenParent,
    /// Select a uniformly random other clip in the library.
    JumpRandom,
    /// Shuffle the whole grid in place (Fisher–Yates over the clip order).
    ShuffleLibrary,
    /// Auto-advance: once the selected clip has played `skip_timer_s`,
    /// selection moves to the next clip (wraps at the end).
    ToggleSkipTimer,
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
            ("enter", "open"),
            ("o", "open"),
            ("space", "quickview"),
            ("tab", "fullview"),
            ("c", "copy_path"),
            ("q", "quit"),
            ("f", "fullscreen"),
            ("-", "zoom_out"),
            ("=", "zoom_in"),
            ("+", "zoom_in"),
            ("0", "zoom_reset"),
            ("a", "toggle_anim"),
            ("p", "toggle_focus_pause"),
            ("r", "reveal"),
            ("[", "skip_back"),
            ("]", "skip_forward"),
            ("g", "chapter_mode"),
            ("D", "open_parent"),
            ("x", "jump_random"),
            ("s", "shuffle_library"),
            ("t", "toggle_skip_timer"),
        ] {
            keys.insert(k.to_string(), v.to_string());
        }
        let mut commands = HashMap::new();
        commands.insert(
            "open".to_string(),
            CommandSpec::Launch {
                program: "mpv".to_string(),
                args: vec!["{path}".to_string()],
            },
        );
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
        let name = self.keys.get(&key_name(key)?)?;
        match self.commands.get(name) {
            Some(CommandSpec::Launch { program, args }) => Some(Action::Spawn {
                program: program.clone(),
                args: args.clone(),
            }),
            Some(CommandSpec::Internal { action, amount }) => internal_action(action, *amount),
            // A command name with no [commands] entry is an internal action.
            None => internal_action(name, None),
        }
    }
}

fn key_name(key: &Key) -> Option<String> {
    Some(match key {
        Key::Enter => "enter".to_string(),
        Key::Space => "space".to_string(),
        Key::Escape => "esc".to_string(),
        Key::Tab => "tab".to_string(),
        Key::Char(c) => c.to_string(),
        // Arrows are reserved movement keys; they never reach the keymap.
        _ => return None,
    })
}

fn internal_action(name: &str, amount: Option<f32>) -> Option<Action> {
    Some(match name {
        "quit" => Action::Quit,
        "fullscreen" | "toggle_fullscreen" => Action::ToggleFullscreen { fast: false },
        "fast_fullscreen" | "toggle_fast_fullscreen" => Action::ToggleFullscreen { fast: true },
        "copy_path" => Action::CopyPath,
        "zoom_in" => Action::ZoomIn,
        "zoom_out" => Action::ZoomOut,
        "zoom_reset" => Action::ZoomReset,
        "toggle_anim" => Action::ToggleAnim,
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
        "open_parent" | "browse_parent" => Action::OpenParent,
        "jump_random" | "jump_to_random" => Action::JumpRandom,
        "shuffle_library" | "shuffle" => Action::ShuffleLibrary,
        "toggle_skip_timer" | "skip_timer" => Action::ToggleSkipTimer,
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
pub fn spawn_external(program: &str, args: &[String], path: &Path) {
    let program = expand_tilde(program);
    let args: Vec<String> = args.iter().map(|a| expand_template(a, path)).collect();
    log::info!("run: {program} {}", args.join(" "));
    match Command::new(&program)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            // Reap off-thread so finished children don't linger as zombies.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => log::warn!("failed to run {program}: {e}"),
    }
}

pub fn copy_path(path: &Path) {
    #[cfg(target_os = "macos")]
    {
        use std::io::Write;
        match Command::new("pbcopy")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                let bytes = path.as_os_str().as_encoded_bytes().to_vec();
                if let Some(mut stdin) = child.stdin.take() {
                    std::thread::spawn(move || {
                        let _ = stdin.write_all(&bytes);
                        drop(stdin);
                        let _ = child.wait();
                    });
                }
                log::info!("copied path: {}", path.display());
            }
            Err(e) => log::warn!("pbcopy failed: {e}"),
        }
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
        assert!(map.action_for(&Key::Left).is_none());
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
    fn random_shuffle_and_skip_timer_defaults_resolve() {
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
            Some(Action::ToggleSkipTimer)
        ));
        // The alias spellings resolve too.
        assert!(matches!(
            internal_action("jump_to_random", None),
            Some(Action::JumpRandom)
        ));
        assert!(matches!(
            internal_action("shuffle", None),
            Some(Action::ShuffleLibrary)
        ));
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
            internal_action("chapters", None),
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
