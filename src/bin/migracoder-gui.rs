fn main() {
    if let Err(error) = migracoder::gui::run() {
        eprintln!("migracoder-gui: {error:#}");
        std::process::exit(1);
    }
}
