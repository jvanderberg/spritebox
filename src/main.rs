mod app;
mod auth;
mod git;
mod sprites_api;
mod state;

use std::process;

#[tokio::main]
async fn main() {
    if let Err(err) = app::run().await {
        eprintln!("error: {err}");
        process::exit(1);
    }
}
