fn main() {
    let settings = marzban_node::config::Settings::from_env();
    if let Err(error) = marzban_node::server::run(settings) {
        eprintln!("marzban-node failed: {error}");
        std::process::exit(1);
    }
}
