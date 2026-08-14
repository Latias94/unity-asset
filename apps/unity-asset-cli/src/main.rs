//! Unity Asset Parser CLI
//!
//! Command-line interface for parsing and manipulating Unity assets.

use clap::Parser;
use serde::Serialize;
use serde_json::Value;
use std::io::Write as _;
use std::process::ExitCode;

mod build_identity;
mod cli;
mod cli_error;
mod commands;
mod fast_path;
mod json_io;
mod shared;
mod workspace_contract;
mod workspace_loader;

const CLI_ERROR_CONTRACT: &str = "unity_asset.cli_error";
const CLI_ERROR_VERSION: u8 = 2;

#[derive(Serialize)]
struct CliErrorReport {
    contract: &'static str,
    version: u8,
    status: &'static str,
    code: &'static str,
    details: Option<Value>,
    message: String,
    causes: Vec<String>,
    warnings: Vec<String>,
}

fn main() -> ExitCode {
    let args = match cli::Cli::try_parse() {
        Ok(args) => args,
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            let code = error.exit_code();
            let _ = error.print();
            return exit_code(code);
        }
        Err(error) => {
            let code = error.exit_code();
            write_error(
                "CLI_ARGUMENT_ERROR",
                None,
                Vec::new(),
                anyhow::Error::msg(error.to_string()),
            );
            return exit_code(code);
        }
    };
    let ctx = shared::AppContext::new(args.strict, args.show_warnings, args.typetree_registry);
    match commands::run(args.command, &ctx) {
        Ok(()) => {
            ctx.flush_warnings();
            ExitCode::SUCCESS
        }
        Err(error) => {
            let (code, details) = cli_error::report_parts(&error);
            write_error(code, details, ctx.take_warnings(), error);
            ExitCode::FAILURE
        }
    }
}

fn write_error(
    code: &'static str,
    details: Option<Value>,
    warnings: Vec<String>,
    error: anyhow::Error,
) {
    let report = CliErrorReport {
        contract: CLI_ERROR_CONTRACT,
        version: CLI_ERROR_VERSION,
        status: "error",
        code,
        details,
        message: error.to_string(),
        causes: error.chain().skip(1).map(ToString::to_string).collect(),
        warnings,
    };
    let stderr = std::io::stderr();
    let mut output = stderr.lock();
    if serde_json::to_writer(&mut output, &report).is_err() {
        let _ = output.write_all(
            b"{\"contract\":\"unity_asset.cli_error\",\"version\":2,\"status\":\"error\",\
              \"code\":\"CLI_ERROR_ENCODING_FAILED\",\"details\":null,\
              \"message\":\"failed to encode CLI error\",\"causes\":[],\"warnings\":[]}",
        );
    }
    let _ = output.write_all(b"\n");
    let _ = output.flush();
}

fn exit_code(code: i32) -> ExitCode {
    u8::try_from(code)
        .map(ExitCode::from)
        .unwrap_or(ExitCode::FAILURE)
}
