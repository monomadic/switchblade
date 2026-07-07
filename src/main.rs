const HELP: &str = "\
switchblade — a GPU-rendered video clip picker (fzf for videos)

USAGE:
    fd -e mp4 -e mov . ~/Clips | switchblade
    switchblade                # demo mode (stdin is a TTY)

    Paths are read from stdin, newline- or NUL-delimited, streaming.

KEYS (defaults; remappable via [keys]/[commands] in ./switchblade.toml):
    hjkl / arrows   move selection
    Enter / o       open selected clip (mpv)
    Space           preview selected clip (looping windowed mpv)
    c               copy path
    - / = / 0       zoom out / in / reset (also trackpad pinch)
    f               fullscreen
    q               quit

CONFIG:
    ./switchblade.toml — feel constants, keys, and commands; hot-reloads
    while the app runs.

OPTIONS:
    --no-anim       start with background tile animation off (toggle: 'a')
    -h, --help      print this help
    -V, --version   print version
";

fn main() -> anyhow::Result<()> {
    let mut opts = sb_app::Options::default();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--no-anim" => opts.anim = false,
            "--help" | "-h" => {
                print!("{HELP}");
                return Ok(());
            }
            "--version" | "-V" => {
                println!("switchblade {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            other => {
                eprintln!("switchblade: unknown argument '{other}'\n");
                eprint!("{HELP}");
                std::process::exit(2);
            }
        }
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    sb_window::run(sb_app::Switchblade::with_options(opts))
}
