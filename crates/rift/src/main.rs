//! Rift CLI.

use clap::Parser;

#[derive(Parser)]
#[command(name = "rift", version, about = "agentic development toolkit")]
struct Cli;

fn main() {
    Cli::parse();
}
