const HELP: &str = "\
switchblade — a GPU-rendered video clip picker (fzf for videos)

USAGE:
    switchblade [OPTIONS] [PATH ...]
    fd -e mp4 . ~/Clips | switchblade

    PATH arguments (files or directories) are the input when given;
    otherwise paths stream from stdin, newline- or NUL-delimited.
    Directories recurse when `recurse = true` (the default) in
    switchblade.toml. Non-video files are skipped.

KEYS (defaults; remappable via [keys]/[commands] in ./switchblade.toml):
    hjkl / arrows   move selection
    Enter / o       open selected clip (mpv)
    Space           quickview: in-app preview (Esc closes, arrows browse)
    c               copy path
    r               reveal in Finder
    a               toggle animated thumbnails
    p               toggle pause-when-unfocused
    - / = / 0       zoom out / in / reset (also trackpad pinch)
    f               fullscreen
    q               quit

CONFIG:
    ./switchblade.toml — feel constants, keys, and commands; hot-reloads
    while the app runs.

OPTIONS:
    --animation <none|minimal|normal|full>
                    how much moves (overrides the config's `animation`):
                    none = snap everything, no video, no sheets;
                    minimal = UI tweens only; normal = + live video for
                    quickview/selected/hovered; full = + background
                    sheet animation
    --no-anim       legacy alias for --animation normal
    --demo          fake-tile demo grid (no media needed)
    -h, --help      print this help
    -V, --version   print version
";

fn main() -> anyhow::Result<()> {
    let mut opts = sb_app::Options::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--animation" => {
                let level = args.next().and_then(|v| sb_app::AnimLevel::parse(&v));
                match level {
                    Some(l) => opts.animation = Some(l),
                    None => {
                        eprintln!("switchblade: --animation takes none|minimal|normal|full\n");
                        std::process::exit(2);
                    }
                }
            }
            eq if eq.starts_with("--animation=") => {
                match sb_app::AnimLevel::parse(&eq["--animation=".len()..]) {
                    Some(l) => opts.animation = Some(l),
                    None => {
                        eprintln!("switchblade: --animation takes none|minimal|normal|full\n");
                        std::process::exit(2);
                    }
                }
            }
            "--no-anim" => opts.animation = Some(sb_app::AnimLevel::Normal),
            "--demo" => opts.demo = true,
            "--help" | "-h" => {
                print!("{HELP}");
                return Ok(());
            }
            "--version" | "-V" => {
                println!("switchblade {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            flag if flag.starts_with('-') => {
                eprintln!("switchblade: unknown option '{flag}'\n");
                eprint!("{HELP}");
                std::process::exit(2);
            }
            path => opts.inputs.push(path.into()),
        }
    }
    // Nothing to show: no path arguments, nothing piped in.
    if opts.inputs.is_empty() && !opts.demo && std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        print!("{HELP}");
        std::process::exit(2);
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    sb_window::run(sb_app::Switchblade::with_options(opts))
}
