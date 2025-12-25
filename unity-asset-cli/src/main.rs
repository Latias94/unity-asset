//! Unity Asset Parser CLI
//!
//! Command-line interface for parsing and manipulating Unity assets.

use anyhow::Result;
use clap::Parser;

mod cli;
mod commands;
mod fast_path;
mod shared;

fn main() -> Result<()> {
    let args = cli::Cli::parse();
    let ctx = shared::AppContext {
        strict: args.strict,
        show_warnings: args.show_warnings,
        typetree_registries: args.typetree_registry,
    };
    commands::run(args.command, &ctx)
}
