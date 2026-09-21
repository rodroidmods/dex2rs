//! Thin binary wrapper around the `dex2rs` library.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(error) = dex2rs::run_from(&args) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
