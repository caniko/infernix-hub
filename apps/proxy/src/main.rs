use std::{env, net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Result, anyhow, ensure};
use clap::{Parser, Subcommand};
use fleetix_agent_proxy::{
    Lane, Proxy,
    clients::{Client, register_t3code},
    config::{self, Config, DEFAULT_BIND, DEFAULT_URL},
    hub_sync, verify,
    worker::{GoogleWorkerOptions, WorkerOptions},
};
use futures_util::FutureExt;
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
        /// Skip this provider during local credential discovery (does not affect --auth-dir).
        #[arg(long = "exclude-local-provider", value_name = "PROVIDER", env = "PROXY_EXCLUDE_LOCAL_PROVIDERS", value_delimiter = ',', value_parser = parse_local_provider)]
        exclude_local_providers: Vec<Lane>,
        #[command(flatten)]
        worker: Box<WorkerOptions>,
        #[command(flatten)]
        google_worker: Box<GoogleWorkerOptions>,
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
    /// Run the offline algorithmic T3 Code + Antigravity integration verification suite.
    ///
    /// Set T3CODE_VERIFY_ROOT to a T3 Code checkout with installed dependencies
    /// to also run the native TypeScript client and recovery checks via Node.
    VerifyT3code {
        /// Emit the full verification report as JSON.
        #[arg(long)]
        json: bool,
    },
}

fn parse_local_provider(name: &str) -> Result<Lane, String> {
    Lane::parse(name.trim()).ok_or_else(|| format!("unknown provider {name:?}; use claude, codex, antigravity, or opencode-go"))
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
            exclude_local_providers,
            worker,
            google_worker,
        } => {
            let mut config = Config::new(bind, &state_dir(state)?, auth_dir, discover_local, client_key_file, management_key_file)?;
            let worker = worker.validate(&config)?;
            let google_worker = google_worker.validate(&config, worker.as_ref())?;
            config.exclude_local_lanes = exclude_local_providers.into_iter().collect();
            if let Some(home) = t3code_home {
                config.t3code_home = home;
            }
            let hub_token = match hub_token_file {
                Some(path) => Some(config::secret_file(&path)?),
                None => env::var("HUB_TOKEN").ok().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()),
            };
            ensure!(hub_url.is_some() == hub_token.is_some(), "configure both HUB_URL and HUB_TOKEN (or --hub-token-file)");
            if let Some(token) = hub_token.as_deref() {
                if let Some(worker) = worker.as_ref() {
                    worker.validate_hub_token(token)?;
                }
                if let Some(google_worker) = google_worker.as_ref() {
                    google_worker.validate_hub_token(token)?;
                }
            }
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
            let proxy = Arc::new(
                Proxy::new(config)?.with_hub_sync_configured(hub.is_some()).with_worker_configs(worker.as_ref(), google_worker.as_ref()),
            );
            proxy.discover_local_commands().await;
            let worker = if let Some(worker) = worker {
                worker.validate_pool(&proxy).await?;
                Some(worker.bind().await?)
            } else {
                None
            };
            let google_worker = if let Some(google_worker) = google_worker {
                google_worker.validate_pool(&proxy).await?;
                Some(google_worker.bind().await?)
            } else {
                None
            };
            let quota = tokio::spawn(hub_sync::poll_quota(proxy.clone()));
            let sync = hub.map(|(url, token)| tokio::spawn(hub_sync::run(proxy.clone(), url, token)));
            tracing::info!(%addr, "agent proxy listening");
            let shutdown = {
                let proxy = proxy.clone();
                async move {
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
                }
                .shared()
            };
            let shared = axum::serve(listener, proxy.clone().app().into_make_service_with_connect_info::<SocketAddr>())
                .with_graceful_shutdown(shutdown.clone());
            let result = match (worker, google_worker) {
                (Some(worker), Some(google_worker)) => tokio::try_join!(
                    std::future::IntoFuture::into_future(shared),
                    worker.serve(proxy.clone(), shutdown.clone()),
                    google_worker.serve(proxy, shutdown),
                )
                .map(|_| ()),
                (Some(worker), None) => {
                    tokio::try_join!(std::future::IntoFuture::into_future(shared), worker.serve(proxy, shutdown)).map(|_| ())
                }
                (None, Some(google_worker)) => {
                    tokio::try_join!(std::future::IntoFuture::into_future(shared), google_worker.serve(proxy, shutdown)).map(|_| ())
                }
                (None, None) => shared.await,
            };
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
        Command::VerifyT3code { json } => {
            let report = verify::run_all().await;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", report.format_human());
            }
            ensure!(report.passed, "T3 Code verification failed ({}/{} checks passed)", report.passed_count, report.total_count);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_parses_worker_flags_and_rejects_conflicting_transports() {
        let cli = Cli::try_parse_from([
            "proxy",
            "serve",
            "--worker-unix-socket",
            "/tmp/worker.sock",
            "--worker-allowed-models",
            "gpt-a,gpt-b",
            "--worker-allowed-models",
            "gpt-c",
        ])
        .unwrap();
        let Command::Serve { worker, .. } = cli.command else { panic!("expected serve command") };
        assert_eq!(worker.worker_allowed_models, ["gpt-a", "gpt-b", "gpt-c"]);
        assert_eq!(worker.worker_unix_socket, Some(PathBuf::from("/tmp/worker.sock")));
        assert!(
            Cli::try_parse_from(["proxy", "serve", "--worker-unix-socket", "/tmp/worker.sock", "--worker-listen", "127.0.0.1:9000",])
                .is_err()
        );
        assert!(Cli::try_parse_from(["proxy", "serve", "--worker-listen", "127.0.0.1:9000", "--worker-api-key", "fixture"]).is_err());

        let google_cli = Cli::try_parse_from([
            "proxy",
            "serve",
            "--google-worker-unix-socket",
            "/tmp/worker-google.sock",
            "--google-worker-allowed-models",
            "gemini-4-neon-high,gemini-3.8-flash",
        ])
        .unwrap();
        let Command::Serve { google_worker, .. } = google_cli.command else { panic!("expected serve command") };
        assert_eq!(google_worker.google_worker_allowed_models, ["gemini-4-neon-high", "gemini-3.8-flash"]);
        assert_eq!(google_worker.google_worker_unix_socket, Some(PathBuf::from("/tmp/worker-google.sock")));
        assert!(
            Cli::try_parse_from([
                "proxy",
                "serve",
                "--google-worker-unix-socket",
                "/tmp/worker-google.sock",
                "--google-worker-listen",
                "127.0.0.1:9001",
            ])
            .is_err()
        );
    }

    #[test]
    fn serve_excludes_multiple_local_provider_aliases_and_rejects_unknowns() {
        let cli =
            Cli::try_parse_from(["proxy", "serve", "--exclude-local-provider", "anthropic,google", "--exclude-local-provider", "openai"])
                .unwrap();
        let Command::Serve { exclude_local_providers, .. } = cli.command else { panic!("expected serve command") };
        assert_eq!(exclude_local_providers, [Lane::Claude, Lane::Google, Lane::Codex]);
        let error = Cli::try_parse_from(["proxy", "serve", "--exclude-local-provider", "unknown"]).err().unwrap();
        assert!(error.to_string().contains("unknown provider"));
    }
}
