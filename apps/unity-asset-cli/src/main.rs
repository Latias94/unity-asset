//! Unity Asset Parser CLI
//!
//! Command-line interface for parsing and manipulating Unity assets.

use anyhow::Result;
use clap::Parser;

mod cli;
mod commands;
mod fast_path;
mod pattern;
mod shared;

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("error"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

fn main() -> Result<()> {
    init_tracing();
    let args = cli::Cli::parse();
    let ctx = shared::AppContext {
        strict: args.strict,
        show_warnings: args.show_warnings,
        typetree_registries: args.typetree_registry,
    };
    commands::run(args.command, &ctx)
}
