//! Greif CLI.

use clap::Parser;

#[derive(Parser)]
#[command(name = "greif", version, about = "agentic development toolkit")]
struct Cli;

fn main() {
    Cli::parse();
}
