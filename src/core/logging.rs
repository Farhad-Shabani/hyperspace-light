use log::LevelFilter;

pub fn setup_logging() {
    env_logger::builder()
        .filter_module("", LevelFilter::Info)
        .format_module_path(false)
        .init();
}
