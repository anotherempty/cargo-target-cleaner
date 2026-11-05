// Clippy has no configuration flags for these test-only exceptions.
#![cfg_attr(test, allow(clippy::todo, clippy::unimplemented))]

mod app;
mod cleanup;
mod diagnostics;
mod error;
mod shell;

fn main() -> std::process::ExitCode {
    app::App::run()
}
