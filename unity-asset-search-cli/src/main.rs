use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "unity-asset-search")]
struct Args {
    #[arg(long, default_value = "http://127.0.0.1:9781")]
    base_url: String,

    #[arg(long)]
    token: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    Search {
        query: String,

        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    Status,
    Reindex {
        #[arg(long)]
        full: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    match args.cmd {
        Cmd::Search { query, limit } => search(&args.base_url, &query, limit).await?,
        Cmd::Status => status(&args.base_url).await?,
        Cmd::Reindex { full } => reindex(&args.base_url, args.token.as_deref(), full).await?,
    }
    Ok(())
}

async fn search(base_url: &str, query: &str, limit: usize) -> Result<()> {
    let url = format!("{base_url}/v1/search");
    let resp = reqwest::Client::new()
        .get(url)
        .query(&[("q", query), ("limit", &limit.to_string())])
        .send()
        .await
        .context("request /v1/search")?
        .error_for_status()
        .context("status /v1/search")?;

    let json: serde_json::Value = resp.json().await?;
    println!("{}", serde_json::to_string_pretty(&json)?);
    Ok(())
}

async fn status(base_url: &str) -> Result<()> {
    let url = format!("{base_url}/v1/status");
    let resp = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .context("request /v1/status")?
        .error_for_status()
        .context("status /v1/status")?;

    let json: serde_json::Value = resp.json().await?;
    println!("{}", serde_json::to_string_pretty(&json)?);
    Ok(())
}

async fn reindex(base_url: &str, token: Option<&str>, full: bool) -> Result<()> {
    let url = format!("{base_url}/v1/reindex");
    let mut req = reqwest::Client::new().post(url);
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    if full {
        req = req.query(&[("full", "true")]);
    }

    let resp = req.send().await.context("request /v1/reindex")?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("reindex failed: {status}: {body}");
    }
    let json: serde_json::Value = resp.json().await?;
    println!("{}", serde_json::to_string_pretty(&json)?);
    Ok(())
}
