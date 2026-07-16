//! Cache maintenance — the `--clear-cache` / `--cleanup-cache` /
//! `--reduce-cache` CLI verbs. All of it runs before the app starts
//! (never concurrently with workers) and only ever touches
//! `cache_root()`.
//!
//! "Stale" is deterministic, not guessed from file ages: every entry's
//! `meta.json` records its source path, and the entry's directory name
//! IS the source's fingerprint under one of the `cache_key` modes. So
//! an entry is dead when its source no longer exists or no longer
//! fingerprints to this entry under either keying (the file was
//! edited/re-encoded — its cache lives elsewhere now).
//! Within live entries, artifacts whose recipe-encoded names don't
//! match the current config would never be served, so they go too.
//! Only `--reduce-cache` uses time, and only to *rank* live entries
//! (oldest last-use first) once the user has asked for a size cap.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::{CacheKey, Meta, Recipe, cache_root, fingerprint_with, mtime_secs};

/// What a maintenance pass removed (or, for `usage`, what exists).
#[derive(Debug, Default, Clone, Copy)]
pub struct Removed {
    pub files: u64,
    pub bytes: u64,
}

impl Removed {
    fn absorb(&mut self, o: Removed) {
        self.files += o.files;
        self.bytes += o.bytes;
    }
}

/// Total size of the cache as it stands.
pub fn usage() -> Removed {
    usage_in(&cache_root())
}

/// `--clear-cache`: remove every object unconditionally.
pub fn clear() -> std::io::Result<Removed> {
    let root = cache_root();
    let total = usage_in(&root);
    if root.exists() {
        std::fs::remove_dir_all(&root)?;
    }
    Ok(total)
}

/// `--cleanup-cache`: drop dead entries (source file gone or changed),
/// artifacts generated under a different recipe than the current config
/// (they'd never be served), and interrupted-write leftovers. Keeps
/// `meta.json` for live entries — it's recipe-independent and cheap.
pub fn cleanup(recipe: &Recipe) -> Removed {
    cleanup_in(&cache_root(), recipe)
}

/// `--reduce-cache`: cleanup first, then delete live entries oldest
/// last-use first until the cache fits `target_bytes`.
pub fn reduce(recipe: &Recipe, target_bytes: u64) -> Removed {
    reduce_in(&cache_root(), recipe, target_bytes)
}

fn usage_in(root: &Path) -> Removed {
    let mut total = Removed::default();
    for entry in entries(root) {
        total.absorb(dir_size(&entry));
    }
    total
}

fn cleanup_in(root: &Path, recipe: &Recipe) -> Removed {
    let mut removed = Removed::default();
    for entry in entries(root) {
        if entry_is_dead(&entry) {
            removed.absorb(remove_entry(&entry));
            continue;
        }
        // Live entry: keep only meta + artifacts the current recipe
        // would actually serve. Everything else (old sizes/qualities,
        // `*.jpg.tmp`, orphaned `animf_*.jpg` frames) is dead weight.
        let keep = [
            String::from("meta.json"),
            recipe.thumb_file(),
            recipe.anim_file(),
        ];
        let Ok(files) = std::fs::read_dir(&entry) else {
            continue;
        };
        for f in files.flatten() {
            let name = f.file_name();
            if !keep.iter().any(|k| k.as_str() == name) {
                removed.absorb(remove_file(&f.path()));
            }
        }
    }
    prune_empty_shards(root);
    removed
}

fn reduce_in(root: &Path, recipe: &Recipe, target_bytes: u64) -> Removed {
    let mut removed = cleanup_in(root, recipe);
    let mut live: Vec<(SystemTime, Removed, PathBuf)> = entries(root)
        .map(|e| (last_used(&e), dir_size(&e), e))
        .collect();
    let mut total: u64 = live.iter().map(|(_, s, _)| s.bytes).sum();
    live.sort_by_key(|(used, _, _)| *used);
    for (_, size, entry) in live {
        if total <= target_bytes {
            break;
        }
        removed.absorb(remove_entry(&entry));
        total = total.saturating_sub(size.bytes);
    }
    prune_empty_shards(root);
    removed
}

/// Iterate entry directories: `<root>/<2-hex shard>/<fingerprint>/`.
fn entries(root: &Path) -> impl Iterator<Item = PathBuf> + use<> {
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|shard| shard.path().is_dir())
        .flat_map(|shard| {
            std::fs::read_dir(shard.path())
                .into_iter()
                .flatten()
                .flatten()
        })
        .map(|e| e.path())
        .filter(|p| p.is_dir())
}

/// Dead = we can no longer tie this entry to a source file that still
/// fingerprints to it. No meta.json means no provenance (interrupted
/// first write) — also dead; regeneration is one queued job away.
/// Either `cache_key` keying counts as alive: a config switch must not
/// turn the other keying's still-valid entries into cleanup fodder
/// (they get adopted or served lazily, never regenerated for nothing).
fn entry_is_dead(entry: &Path) -> bool {
    let Ok(bytes) = std::fs::read(entry.join("meta.json")) else {
        return true;
    };
    let Ok(meta) = serde_json::from_slice::<Meta>(&bytes) else {
        return true;
    };
    let Ok(st) = std::fs::metadata(&meta.src) else {
        return true; // source gone (or unreadable — treat as gone)
    };
    let Some(name) = entry.file_name() else {
        return true;
    };
    ![CacheKey::Path, CacheKey::SizeMtime]
        .iter()
        .any(|&k| name == fingerprint_with(k, &meta.src, st.len(), mtime_secs(&st)).as_str())
}

/// Best-effort last-use of an entry: the newest access-or-modify time
/// across its files. atime is maintained lazily on APFS; mtime (=
/// generation time) is the floor when it isn't.
fn last_used(entry: &Path) -> SystemTime {
    std::fs::read_dir(entry)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|f| f.metadata().ok())
        .flat_map(|m| [m.accessed().ok(), m.modified().ok()])
        .flatten()
        .max()
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn dir_size(dir: &Path) -> Removed {
    let mut total = Removed::default();
    for f in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if let Ok(m) = f.metadata()
            && m.is_file()
        {
            total.files += 1;
            total.bytes += m.len();
        }
    }
    total
}

fn remove_entry(entry: &Path) -> Removed {
    let size = dir_size(entry);
    match std::fs::remove_dir_all(entry) {
        Ok(()) => size,
        Err(_) => Removed::default(),
    }
}

fn remove_file(path: &Path) -> Removed {
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    match std::fs::remove_file(path) {
        Ok(()) => Removed { files: 1, bytes },
        Err(_) => Removed::default(),
    }
}

/// Drop shard directories a pass emptied (remove_dir refuses non-empty).
fn prune_empty_shards(root: &Path) {
    for shard in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let _ = std::fs::remove_dir(shard.path());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECIPE: Recipe = Recipe {
        thumb_w: 640,
        thumb_h: 360,
        quality: 5,
        anim_grid: 3,
    };

    fn meta_for(src: &Path) -> Vec<u8> {
        serde_json::to_vec(&Meta {
            src: src.to_path_buf(),
            duration: Some(1.0),
            width: Some(640),
            height: Some(360),
            codec: Some("h264".into()),
            fps: Some(30.0),
            rotation: None,
            pix_fmt: Some("yuv420p".into()),
        })
        .unwrap()
    }

    /// A cache entry whose fingerprint matches `src` as it exists now.
    fn live_entry(root: &Path, src: &Path) -> PathBuf {
        live_entry_keyed(root, src, CacheKey::Path)
    }

    fn live_entry_keyed(root: &Path, src: &Path, key: CacheKey) -> PathBuf {
        let st = std::fs::metadata(src).unwrap();
        let fp = fingerprint_with(key, src, st.len(), mtime_secs(&st));
        let dir = root.join(&fp[..2]).join(&fp);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("meta.json"), meta_for(src)).unwrap();
        dir
    }

    /// Entries from EITHER keying stay alive as long as their source
    /// still matches — switching `cache_key` in the config must never
    /// turn the old keying's cache into cleanup fodder.
    #[test]
    fn entries_from_either_cache_key_survive_cleanup() {
        let root = std::env::temp_dir().join("sb_maint_keying_test");
        let _ = std::fs::remove_dir_all(&root);
        let src = root.join("clip.mp4");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&src, b"not really a video").unwrap();
        let by_path = live_entry_keyed(&root, &src, CacheKey::Path);
        let by_stat = live_entry_keyed(&root, &src, CacheKey::SizeMtime);
        assert_ne!(by_path, by_stat);
        assert!(!entry_is_dead(&by_path));
        assert!(!entry_is_dead(&by_stat));
        cleanup_in(&root, &RECIPE);
        assert!(by_path.join("meta.json").exists());
        assert!(by_stat.join("meta.json").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cleanup_keeps_live_current_artifacts_only() {
        let root = std::env::temp_dir().join("sb_maint_cleanup_test");
        let _ = std::fs::remove_dir_all(&root);
        let src = root.join("clip.mp4");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&src, b"not really a video").unwrap();

        // Live entry: current artifacts + old-recipe + tmp leftovers.
        let live = live_entry(&root, &src);
        for name in [
            RECIPE.thumb_file(),                   // keep
            RECIPE.anim_file(),                    // keep
            "thumb_fit_320x180_q7.jpg".into(),     // old recipe
            "anim_2x2_640x360_q5.jpg".into(),      // old recipe
            "thumb_fit_640x360_q5.jpg.tmp".into(), // interrupted write
            "animf_4.jpg".into(),                  // orphaned frame
        ] {
            std::fs::write(live.join(name), b"jpg").unwrap();
        }
        // Dead entry: source never existed.
        let dead = root.join("aa").join("aaaaaaaaaaaaaaaa");
        std::fs::create_dir_all(&dead).unwrap();
        std::fs::write(dead.join("meta.json"), meta_for(&root.join("gone.mp4"))).unwrap();
        std::fs::write(dead.join(RECIPE.thumb_file()), b"jpg").unwrap();
        // No-provenance entry: artifacts but no meta.json.
        let orphan = root.join("bb").join("bbbbbbbbbbbbbbbb");
        std::fs::create_dir_all(&orphan).unwrap();
        std::fs::write(orphan.join(RECIPE.thumb_file()), b"jpg").unwrap();

        let removed = cleanup_in(&root, &RECIPE);
        assert_eq!(removed.files, 4 + 2 + 1);
        assert!(live.join("meta.json").exists());
        assert!(live.join(RECIPE.thumb_file()).exists());
        assert!(live.join(RECIPE.anim_file()).exists());
        assert_eq!(std::fs::read_dir(&live).unwrap().count(), 3);
        assert!(!dead.exists() && !dead.parent().unwrap().exists());
        assert!(!orphan.exists());

        // Reduce to zero: even live entries go, oldest first.
        let removed = reduce_in(&root, &RECIPE, 0);
        assert_eq!(removed.files, 3);
        assert!(!live.exists() && !live.parent().unwrap().exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn entry_dies_when_source_changes() {
        let root = std::env::temp_dir().join("sb_maint_changed_test");
        let _ = std::fs::remove_dir_all(&root);
        let src = root.join("clip.mp4");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&src, b"original bytes").unwrap();
        let live = live_entry(&root, &src);
        assert!(!entry_is_dead(&live));
        // Re-encode: different size → different fingerprint elsewhere.
        std::fs::write(&src, b"different length content").unwrap();
        assert!(entry_is_dead(&live));
        let _ = std::fs::remove_dir_all(&root);
    }
}
