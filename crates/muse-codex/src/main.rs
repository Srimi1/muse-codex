fn main() {
    match muse_codex::run() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("muse-codex: {error:#}");
            std::process::exit(1);
        }
    }
}
