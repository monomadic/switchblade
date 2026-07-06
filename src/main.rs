fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    sb_window::run(sb_app::Switchblade::new())
}
