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
    --no-anim       start with background tile animation off (toggle: 'a')
    --demo          fake-tile demo grid (no media needed)
    -h, --help      print this help
    -V, --version   print version
";

fn main() -> anyhow::Result<()> {
    let mut opts = sb_app::Options::default();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--no-anim" => opts.anim = false,
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
