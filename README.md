# switchblade

**fzf for videos** — a GPU-rendered video clip picker.

Pipe file paths in on stdin, fly around a grid of moving thumbnails, act on the clip you land on.

```sh
switchblade ~/Clips                        # or:
fd -e mp4 -e mov . ~/Clips | switchblade
```

Built for VJs picking clips mid-set, creators sorting AI-generated footage, and anyone with a folder full of clips and no fast way to *see* them. Not an editor, not a media manager — a picker.

> **Status:** early and moving fast. macOS-first (Metal via wgpu). See [PLAN.md](PLAN.md) for the design plan and roadmap.

## What it does

- **Instanced GPU grid** — thousands of clips render as one draw call; near-black field, thick-bordered selection, buttery springs everywhere.
- **Streaming stdin** — tiles appear while `fd` is still finding files. Newline- or NUL-delimited, never blocks on a slow producer.
- **Cached thumbnails** — one frame per clip (ffmpeg) in a content-addressed sidecar cache; a relaunch over the same library is instant. Your media files are never written to.
- **Animated thumbnails** — a sprite sheet of frames sampled across each clip; tiles cycle with per-clip phase offsets and shader crossfade. Toggle with `a`.
- **Live playback in-tile** — the selected and hovered clips play real (silent, looping) video inside their tiles, seek-matched to the thumbnail's frame so nothing jumps.
- **Quickview** — `Space` (or click the selection): the grid dims and the clip plays large and centered at natural resolution (up to 1080p, configurable). Arrows keep browsing without closing it.
- **Pinch zoom** — tile size scales, columns reflow with a crossfade. `-`/`=`/`0` on the keyboard.
- **Keymap** — bind any non-movement key to internal actions or launched programs with `{path}`/`{dir}`/`{name}` templates: open in mpv, reveal in Finder, run your renamer script, push to a VJ tool.
- **Hot-tunable feel** — every motion constant (springs, gaps, scales, fades) lives in `switchblade.toml` and reloads within 250ms while the app runs.
- **iCloud-aware** — placeholder (not-downloaded) files get a cloud badge and are never force-downloaded.
- **Cheap when still** — the render loop idles at ~2% of a core when nothing is animating, and playback/animation pause automatically while the window is unfocused (configurable).

## Requirements

- macOS (first target platform)
- [ffmpeg](https://ffmpeg.org) + ffprobe on `PATH` — thumbnails and live decode (`brew install ffmpeg`)
- [mpv](https://mpv.io) — optional, the default `open` command (`brew install mpv`)

## Build & run

```sh
cargo build --release
fd -e mp4 -e mov . ~/Clips | ./target/release/switchblade

cargo run          # demo mode: fake tiles (when stdin is a TTY)
switchblade --help # options, including --no-anim
```

## Keys (defaults)

| Key | Action |
|---|---|
| `hjkl` / arrows | move selection (row-end wraps to the next row) |
| `Enter` / `o` | open in mpv |
| `Space` | quickview (internal preview; `Esc`/`Space`/click closes, arrows browse) |
| `c` | copy path |
| `r` | reveal in Finder |
| `a` | toggle animated thumbnails |
| `p` | toggle pause-when-unfocused |
| `-` / `=` / `0` | zoom out / in / reset (also trackpad pinch) |
| `f` | fullscreen |
| `q` | quit |

Trackpad pans without changing the selection. Click selects; clicking the selected clip quickviews. Everything except movement is remappable.

## Configuration

`switchblade.toml` in the working directory. Motion and style values hot-reload while the app runs; media quality (`thumb_width/height/quality`, `anim_grid`, atlas size) applies on restart. Keys and commands:

```toml
[keys]
r = "rename_script"

[commands.rename_script]
type = "launch"
program = "~/bin/rename-media"
args = ["{path}"]
```

See the [example config](switchblade.toml) — it documents every field.

## Cache

Thumbnails, sprite sheets, and probed metadata live under
`~/Library/Caches/switchblade.noindex/v1/objects/` keyed by file fingerprint (`.noindex` keeps Spotlight away)
(path + size + mtime). It's plain files — inspect it, or delete it any
time and it regenerates.

## License

MIT
