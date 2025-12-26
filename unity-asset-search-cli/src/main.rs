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
    Suggest {
        prefix: String,

        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    Status,
    Reindex {
        #[arg(long)]
        full: bool,

        #[arg(long, value_name = "PATH")]
        path: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    match args.cmd {
        Cmd::Search { query, limit } => search(&args.base_url, &query, limit).await?,
        Cmd::Suggest { prefix, limit } => suggest(&args.base_url, &prefix, limit).await?,
        Cmd::Status => status(&args.base_url).await?,
        Cmd::Reindex { full, path } => {
            reindex(&args.base_url, args.token.as_deref(), full, &path).await?
        }
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

async fn suggest(base_url: &str, prefix: &str, limit: usize) -> Result<()> {
    let url = format!("{base_url}/v1/suggest");
    let resp = reqwest::Client::new()
        .get(url)
        .query(&[("prefix", prefix), ("limit", &limit.to_string())])
        .send()
        .await
        .context("request /v1/suggest")?
        .error_for_status()
        .context("status /v1/suggest")?;

    let json: serde_json::Value = resp.json().await?;
    println!("{}", serde_json::to_string_pretty(&json)?);
    Ok(())
}

async fn reindex(base_url: &str, token: Option<&str>, full: bool, paths: &[String]) -> Result<()> {
    let mut url = reqwest::Url::parse(&format!("{base_url}/v1/reindex"))?;
    {
        let mut qp = url.query_pairs_mut();
        if full {
            qp.append_pair("full", "true");
        }
        for p in paths {
            qp.append_pair("path", p);
        }
    }

    let mut req = reqwest::Client::new().post(url);
    if let Some(token) = token {
        req = req.bearer_auth(token);
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
