use std::{env, net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Result, anyhow, ensure};
use clap::{Parser, Subcommand};
use fleetix_agent_proxy::{
    Proxy,
    clients::{Client, register_t3code},
    config::{self, Config, DEFAULT_BIND, DEFAULT_URL},
    hub_sync,
};
use tokio::net::TcpListener;

const SOURCE_ID: &str = "fleetix-proxy";
const SOURCE_LABEL: &str = "Fleetix Agent Proxy";

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Serve {
        #[arg(long, default_value = DEFAULT_BIND)]
        bind: SocketAddr,
        #[arg(long, env = "PROXY_STATE_DIR")]
        state_dir: Option<PathBuf>,
        #[arg(long, env = "PROXY_AUTH_DIR")]
        auth_dir: Option<PathBuf>,
        #[arg(long, env = "PROXY_CLIENT_KEY_FILE")]
        client_key_file: Option<PathBuf>,
        #[arg(long, env = "PROXY_MANAGEMENT_KEY_FILE")]
        management_key_file: Option<PathBuf>,
        #[arg(long, env = "HUB_URL")]
        hub_url: Option<String>,
        #[arg(long, env = "HUB_TOKEN_FILE")]
        hub_token_file: Option<PathBuf>,
        #[arg(long)]
        register_t3code: bool,
        #[arg(long)]
        t3code_home: Option<PathBuf>,
        #[arg(long, env = "PROXY_DISCOVER_LOCAL", value_parser = clap::builder::FalseyValueParser::new(), default_value_t = false, action = clap::ArgAction::SetTrue)]
        discover_local: bool,
    },
    RegisterT3code {
        #[arg(long, default_value = DEFAULT_URL)]
        url: String,
        #[arg(long, env = "PROXY_MANAGEMENT_KEY_FILE")]
        management_key_file: Option<PathBuf>,
        #[arg(long)]
        t3code_home: Option<PathBuf>,
        #[arg(long, default_value = SOURCE_ID)]
        source_id: String,
        #[arg(long, default_value = SOURCE_LABEL)]
        label: String,
    },
    ApplyClient {
        #[arg(value_enum)]
        client: Client,
        #[arg(long)]
        undo: bool,
        #[arg(long, default_value = DEFAULT_URL)]
        url: String,
        #[arg(long, env = "PROXY_CLIENT_KEY_FILE")]
        client_key_file: Option<PathBuf>,
    },
}

fn state_dir(path: Option<PathBuf>) -> Result<PathBuf> {
    path.or_else(|| env::var_os("PROXY_STATE_DIR").map(PathBuf::from)).map(Ok).unwrap_or_else(config::state_dir)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).with_writer(std::io::stderr).init();
    match Cli::parse().command {
        Command::Serve {
            bind,
            state_dir: state,
            auth_dir,
            client_key_file,
            management_key_file,
            hub_url,
            hub_token_file,
            register_t3code: register,
            t3code_home,
            discover_local,
        } => {
            let mut config = Config::new(bind, &state_dir(state)?, auth_dir, discover_local, client_key_file, management_key_file)?;
            if let Some(home) = t3code_home {
                config.t3code_home = home;
            }
            let hub_token = match hub_token_file {
                Some(path) => Some(config::secret_file(&path)?),
                None => env::var("HUB_TOKEN").ok().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()),
            };
            ensure!(hub_url.is_some() == hub_token.is_some(), "configure both HUB_URL and HUB_TOKEN (or --hub-token-file)");
            let hub = hub_url
                .zip(hub_token)
                .map(|(url, token)| Ok::<_, anyhow::Error>((config::origin(&url)?.to_string(), token)))
                .transpose()?;
            let listener = TcpListener::bind(config.bind).await?;
            config.bind = listener.local_addr()?;
            config.url = if config.bind.ip().is_unspecified() {
                format!("http://127.0.0.1:{}", config.bind.port())
            } else {
                format!("http://{}", config.bind)
            };
            if register {
                register_t3code(&config.t3code_home, SOURCE_ID, SOURCE_LABEL, &config.url, &config.management_key)?;
            }
            let addr = config.bind;
            let proxy = Arc::new(Proxy::new(config)?);
            proxy.discover_local_commands().await;
            let quota = tokio::spawn(hub_sync::poll_quota(proxy.clone()));
            let sync = hub.map(|(url, token)| tokio::spawn(hub_sync::run(proxy.clone(), url, token)));
            tracing::info!(%addr, "agent proxy listening");
            let result = axum::serve(listener, proxy.clone().app().into_make_service_with_connect_info::<SocketAddr>())
                .with_graceful_shutdown(async move {
                    use tokio::signal::unix::{SignalKind, signal};
                    let term = async {
                        if let Ok(mut signal) = signal(SignalKind::terminate()) {
                            signal.recv().await;
                        } else {
                            std::future::pending::<()>().await;
                        }
                    };
                    tokio::select! {
                        _ = proxy.shutdown_requested() => {},
                        _ = tokio::signal::ctrl_c() => {},
                        _ = term => {},
                    }
                })
                .await;
            quota.abort();
            if let Some(sync) = sync {
                sync.abort();
            }
            result?;
        }
        Command::RegisterT3code { url, management_key_file, t3code_home, source_id, label } => {
            let t3code_home = t3code_home.map(Ok).unwrap_or_else(|| config::home().map(|home| config::t3_home(&home)))?;
            let key = config::key(&state_dir(None)?, "management-key", "PROXY_MANAGEMENT_KEY", management_key_file.as_deref(), false)?;
            register_t3code(&t3code_home, &source_id, &label, &url, &key)?;
        }
        Command::ApplyClient { client, undo, url, client_key_file } => {
            let key = if undo {
                None
            } else {
                match config::key(&state_dir(None)?, "client-key", "PROXY_CLIENT_KEY", client_key_file.as_deref(), false) {
                    Ok(key) => Some(key),
                    Err(_) if matches!(client, Client::Codex) && client_key_file.is_none() => Some(String::new()),
                    Err(_) => return Err(anyhow!("client key unavailable")),
                }
            };
            client.wire(&config::home()?, key.as_deref().map(|key| (url.trim_end_matches('/'), key)))?;
        }
    }
    Ok(())
}
