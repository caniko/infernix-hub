//! `infernix-hubd`: API server with the embedded integration worker.

use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use contracts::Role;
use infernix_hubd::bootstrap::{self, Settings};

#[derive(Parser)]
#[command(name = "infernix-hubd", version, about = "infernix-hub API server")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve the API (and the worker unless --no-worker). Configured by env.
    Serve {
        #[arg(long)]
        no_worker: bool,
    },
    /// Run the worker only.
    Worker,
    /// Apply migrations and exit; `--check` only reports (exit 1 if pending).
    Migrate {
        #[arg(long)]
        check: bool,
    },
    /// Issue a bearer token for a principal; prints the token once.
    Token {
        #[arg(long)]
        name: String,
        #[arg(long, value_parser = parse_role)]
        role: Role,
        /// Register this existing token (e.g. a systemd credential) instead
        /// of generating one; idempotent, nothing is printed.
        #[arg(long)]
        token_file: Option<std::path::PathBuf>,
    },
    /// Complete fixture journey: every integration simulated, seeded data,
    /// fixed development tokens, sample news and an inbound interaction.
    Dev {
        #[arg(long, env = "DATABASE_URL")]
        database_url: String,
        #[arg(long, default_value = "127.0.0.1:8470")]
        bind: String,
    },
}

fn parse_role(s: &str) -> Result<Role, String> {
    Role::parse(s).ok_or_else(|| "role must be operator|agent|procedure|ingest".into())
}

/// Fixed tokens for the local fixture journey only.
pub const DEV_TOKENS: &[(&str, Role, &str)] = &[
    ("operator", Role::Operator, "dev-operator-token"),
    ("agent", Role::Agent, "dev-agent-token"),
    ("windmill", Role::Procedure, "dev-procedure-token"),
    ("n8n", Role::Ingest, "dev-ingest-token"),
];

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,sqlx=warn,tower_http=info".into()))
        .init();
    match Cli::parse().cmd {
        Cmd::Migrate { check } => {
            let s = Settings::from_env()?;
            let db = domain::sqlx::PgPool::connect(&s.database_url).await?;
            if check {
                let pending = domain::pending_migrations(&db).await?;
                if !pending.is_empty() {
                    eprintln!("pending migrations: {pending:?}");
                    std::process::exit(1);
                }
                println!("schema current");
            } else {
                domain::MIGRATOR.run(&db).await?;
                println!("migrations applied");
            }
        }
        Cmd::Token { name, role, token_file } => {
            let s = Settings::from_env()?;
            let db = domain::sqlx::PgPool::connect(&s.database_url).await?;
            let pending = domain::pending_migrations(&db).await?;
            anyhow::ensure!(pending.is_empty(), "database has pending migrations {pending:?}");
            match token_file {
                Some(path) => {
                    let token = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?.trim().to_string();
                    anyhow::ensure!(token.len() >= 24, "token in {} is too short (min 24 chars)", path.display());
                    domain::system::issue_token(&db, &name, role, Some(token)).await?;
                }
                None => println!("{}", domain::system::issue_token(&db, &name, role, None).await?),
            }
        }
        Cmd::Serve { no_worker } => {
            let s = Settings::from_env()?;
            let (state, _fx) = bootstrap::build(&s).await?;
            if !no_worker {
                tokio::spawn(worker::run(state.hub.clone(), Duration::from_secs(2)));
            }
            serve(&s.bind, state).await?;
        }
        Cmd::Worker => {
            let s = Settings::from_env()?;
            let (state, _fx) = bootstrap::build(&s).await?;
            worker::run(state.hub, Duration::from_secs(2)).await;
        }
        Cmd::Dev { database_url, bind } => {
            let s = Settings::fixture(&database_url, &bind);
            let (state, fx) = bootstrap::build(&s).await?;
            for (name, role, token) in DEV_TOKENS {
                domain::system::issue_token(&state.hub.db, name, *role, Some(token.to_string())).await?;
            }
            let comfy = fx.comfy.clone().context("fixture comfy")?;
            comfy.spawn_runner(Duration::from_millis(700));
            fx.windmill.as_ref().context("fixture windmill")?.connect(&s.public_url, "dev-procedure-token");
            tokio::spawn(worker::run(state.hub.clone(), Duration::from_millis(700)));
            let url = s.public_url.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                if let Err(e) = demo_events(&url).await {
                    tracing::warn!(error = %e, "demo events failed");
                }
            });
            println!("\nFIXTURE MODE — every integration is simulated in-process.");
            println!("API      {}/v1   (OpenAPI: {}/openapi.json)", s.public_url, s.public_url);
            println!("Desktop  HUB_URL={} HUB_TOKEN=dev-operator-token cargo run -p infernix-hub-desktop", s.public_url);
            println!("CLI      HUB_URL={} HUB_TOKEN=dev-operator-token cargo run -p hubctl -- today\n", s.public_url);
            serve(&bind, state).await?;
        }
    }
    Ok(())
}

/// Sample news (with a retried duplicate) and one inbound mention, posted
/// through the real ingest endpoints as n8n would.
async fn demo_events(url: &str) -> anyhow::Result<()> {
    let ingest = contracts::client::HubClient::new(url, "dev-ingest-token");
    let now = chrono::Utc::now();
    let news = [
        (
            "n8n:rss-studio",
            "item-1001",
            "https://www.example.org/news/local-kiln-reopens?utm_source=rss",
            "Local kiln reopens after two-year restoration",
            "The hillside kiln fires again; volunteers rebuilt the chamber by hand.",
        ),
        (
            "windmill:f/news/scrape",
            "wm-77",
            "https://example.org/news/local-kiln-reopens",
            "Local kiln reopens after two-year restoration",
            "Second feed covering the same story.",
        ),
        (
            "n8n:rss-studio",
            "item-1002",
            "https://example.net/2026/11/glaze-chemistry",
            "A plain-language guide to glaze chemistry",
            "Why iron reds go brown, and how to keep them red.",
        ),
    ];
    for (source, id, link, title, summary) in news {
        let ev = contracts::NewsEventIn {
            source: source.into(),
            external_id: id.into(),
            url: link.into(),
            title: title.into(),
            summary: summary.into(),
            published_at: Some(now),
            evidence: vec![contracts::Evidence { url: link.into(), quote: summary.into() }],
            raw: serde_json::json!({ "fixture": true }),
        };
        // Deliver twice to show retry deduplication.
        for _ in 0..2 {
            let _: contracts::IngestReceipt = ingest.post("/ingest/news", &ev).await?;
        }
    }
    let _: contracts::IngestReceipt = ingest
        .post(
            "/ingest/interactions",
            &contracts::InteractionIn {
                account_id: "fixture-mastodon".into(),
                external_thread_id: "thread-42".into(),
                external_id: "status-9001".into(),
                direction: contracts::Direction::Inbound,
                author: "@potter@fixture.social".into(),
                body: "Is the kiln open to visitors this weekend?".into(),
                occurred_at: now - chrono::Duration::hours(5),
                permalink: Some("https://fixture.social/@potter/9001".into()),
                raw: serde_json::json!({ "fixture": true }),
            },
        )
        .await?;
    Ok(())
}

async fn serve(bind: &str, state: infernix_hubd::AppState) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(bind).await.with_context(|| format!("binding {bind}"))?;
    tracing::info!(%bind, "listening");
    axum::serve(listener, infernix_hubd::router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
