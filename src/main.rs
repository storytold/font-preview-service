fn main() {
    let code = match std::env::args().nth(1).as_deref() {
        None => font_preview_service::server::main(),
        Some("sync") => font_preview_service::sync::main(),
        Some(other) => {
            eprintln!("unknown command `{other}`; use `sync` or no argument to run the server");
            2
        }
    };
    std::process::exit(code);
}
