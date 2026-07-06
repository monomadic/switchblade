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
    External {
        program: String,
        #[serde(default)]
        args: Vec<String>,
    },
    Internal {
        action: String,
    },
}

/// A resolved keypress: either external launch data (template-unexpanded)
/// or an internal action for the app to perform.
#[derive(Debug, Clone)]
pub enum Action {
    Spawn { program: String, args: Vec<String> },
    Quit,
    ToggleFullscreen,
    CopyPath,
    ZoomIn,
    ZoomOut,
    ZoomReset,
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
            ("space", "preview"),
            ("c", "copy_path"),
            ("q", "quit"),
            ("f", "fullscreen"),
            ("-", "zoom_out"),
            ("=", "zoom_in"),
            ("+", "zoom_in"),
            ("0", "zoom_reset"),
        ] {
            keys.insert(k.to_string(), v.to_string());
        }
        let mut commands = HashMap::new();
        commands.insert(
            "open".to_string(),
            CommandSpec::External {
                program: "mpv".to_string(),
                args: vec!["{path}".to_string()],
            },
        );
        // Preview is deliberately distinct from open: looping, no playlist
        // behavior — a peek, not a commit (PLAN.md §4 note).
        commands.insert(
            "preview".to_string(),
            CommandSpec::External {
                program: "mpv".to_string(),
                args: vec!["--loop-file=inf".to_string(), "{path}".to_string()],
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
            Some(CommandSpec::External { program, args }) => Some(Action::Spawn {
                program: program.clone(),
                args: args.clone(),
            }),
            Some(CommandSpec::Internal { action }) => internal_action(action),
            // A command name with no [commands] entry is an internal action.
            None => internal_action(name),
        }
    }
}

fn key_name(key: &Key) -> Option<String> {
    Some(match key {
        Key::Enter => "enter".to_string(),
        Key::Space => "space".to_string(),
        Key::Escape => "esc".to_string(),
        Key::Char(c) => c.to_string(),
        // Arrows are reserved movement keys; they never reach the keymap.
        _ => return None,
    })
}

fn internal_action(name: &str) -> Option<Action> {
    Some(match name {
        "quit" => Action::Quit,
        "fullscreen" | "toggle_fullscreen" => Action::ToggleFullscreen,
        "copy_path" => Action::CopyPath,
        "zoom_in" => Action::ZoomIn,
        "zoom_out" => Action::ZoomOut,
        "zoom_reset" => Action::ZoomReset,
        other => {
            log::warn!("unknown command '{other}': no [commands.{other}] entry and not a built-in action");
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
    log::warn!("copy_path is not implemented on this platform ({})", path.display());
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
    if let Some(rest) = program.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return Path::new(&home).join(rest).to_string_lossy().into_owned();
        }
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
        assert_eq!(expand_template("--title={name}", &p), "--title=take one.mp4");
    }

    #[test]
    fn defaults_resolve() {
        let map = KeyMap::default();
        assert!(matches!(map.action_for(&Key::Char('q')), Some(Action::Quit)));
        assert!(matches!(
            map.action_for(&Key::Enter),
            Some(Action::Spawn { ref program, .. }) if program == "mpv"
        ));
        assert!(map.action_for(&Key::Char('x')).is_none());
        assert!(map.action_for(&Key::Left).is_none());
    }

    #[test]
    fn user_entries_override_defaults() {
        let keys = HashMap::from([("o".to_string(), "reveal".to_string())]);
        let commands = HashMap::from([(
            "reveal".to_string(),
            CommandSpec::External {
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
        assert!(matches!(map.action_for(&Key::Char('q')), Some(Action::Quit)));
    }
}
