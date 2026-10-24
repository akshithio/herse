//! `herse [path...]` opens notebooks and/or a folder.

// GPUI rebuilds element trees every frame; a fast allocator is a cheap win
// there and in notebook parsing (~15% on many-cell notebooks).
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    // An agent CLI starting Herse's tools (see `herse::agent::mcp`).
    let mut args = std::env::args_os().skip(1);
    if args.next().is_some_and(|a| a == "--mcp-bridge") {
        let socket = args.next().map(std::path::PathBuf::from).unwrap_or_default();
        if let Err(err) = herse::agent::mcp::bridge(&socket) {
            eprintln!("herse --mcp-bridge: {err}");
            std::process::exit(1);
        }
        return;
    }
    let paths = std::env::args_os().skip(1).map(std::path::PathBuf::from).collect();
    herse::run(paths);
}
