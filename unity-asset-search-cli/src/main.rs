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
    Bench {
        #[arg(long)]
        query: Vec<String>,

        #[arg(long, default_value = "scripts/bench_queries.txt")]
        query_file: String,

        #[arg(long, default_value_t = 1)]
        warmup: usize,

        #[arg(long, default_value_t = 1)]
        repeat: usize,

        #[arg(long, default_value_t = 20)]
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
        Cmd::Bench {
            query,
            query_file,
            warmup,
            repeat,
            limit,
        } => bench(&args.base_url, &query, &query_file, warmup, repeat, limit).await?,
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

async fn bench(
    base_url: &str,
    inline_queries: &[String],
    query_file: &str,
    warmup: usize,
    repeat: usize,
    limit: usize,
) -> Result<()> {
    let mut queries = Vec::new();
    queries.extend(inline_queries.iter().cloned());
    queries.extend(load_queries_from_file(query_file).unwrap_or_default());
    queries.retain(|q| !q.trim().is_empty());

    if queries.is_empty() {
        anyhow::bail!("no queries provided (use --query or --query-file)");
    }

    let client = reqwest::Client::new();
    for q in &queries {
        for _ in 0..warmup {
            let _ = search_once(&client, base_url, q, limit).await?;
        }
    }

    let mut tooks = Vec::new();
    for q in &queries {
        for _ in 0..repeat {
            let took_ms = search_once(&client, base_url, q, limit).await?;
            tooks.push(took_ms);
        }
    }
    tooks.sort();

    let p50 = percentile(&tooks, 0.50);
    let p95 = percentile(&tooks, 0.95);
    let max = tooks.last().copied().unwrap_or(0);

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "queries": queries.len(),
            "runs": tooks.len(),
            "p50_ms": p50,
            "p95_ms": p95,
            "max_ms": max,
        }))?
    );

    Ok(())
}

async fn search_once(
    client: &reqwest::Client,
    base_url: &str,
    query: &str,
    limit: usize,
) -> Result<u128> {
    let url = format!("{base_url}/v1/search");
    let resp = client
        .get(url)
        .query(&[("q", query), ("limit", &limit.to_string())])
        .send()
        .await
        .with_context(|| format!("request /v1/search (q={query})"))?
        .error_for_status()
        .with_context(|| format!("status /v1/search (q={query})"))?;

    let json: serde_json::Value = resp.json().await?;
    let took_ms = json
        .get("took_ms")
        .and_then(|v| v.as_u64())
        .map(|v| v as u128)
        .unwrap_or(0);
    Ok(took_ms)
}

fn load_queries_from_file(path: &str) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read queries: {path}"))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect())
}

fn percentile(sorted: &[u128], p: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let p = p.clamp(0.0, 1.0);
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
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
