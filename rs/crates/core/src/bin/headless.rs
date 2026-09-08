//! `avada-headless` — a runnable, GUI-less daemon for parity testing. Starts the app
//! (central SessionManager + control server + `control.json`) so the REAL MCP server at
//! `C:\avada-mcp` can be pointed at the Rust backend for the acceptance gate.
//! (Auto-discovered bin — no Cargo.toml entry needed.)
//!
//! Usage (gate): point the MCP at an isolated discovery file so it never fights the Electron app:
//!   set AVADA_CONTROL_FILE=C:\tmp\hp-headless\control.json
//!   set AVADA_ALLOW_INPUT=1
//!   cargo run --bin headless
//! Then run the MCP with the same `AVADA_CONTROL_FILE`.

fn main() {
    // See app/src/main.rs: the pre-rename data directory is copied in once, first.
    let migration = avada_core::compat::migrate_user_data();
    avada_core::logging::init("headless", avada_core::logging::DEFAULT_LEVEL);
    migration.log();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("avada-headless: failed to start tokio runtime: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = runtime.block_on(avada_core::app::run(env!("CARGO_PKG_VERSION"))) {
        eprintln!("avada-headless: {e}");
        std::process::exit(1);
    }
}
