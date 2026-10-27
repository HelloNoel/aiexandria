use std::path::PathBuf;
use std::time::Duration;

use aiexandria::auth::AuthConfig;
use aiexandria::cache::{RefreshPolicy, RepoCache};
use aiexandria::generate::{generate_index, GenerateOptions};
use aiexandria::index::GitRef;
use aiexandria::registry::RegistryConfig;
use aiexandria::server::AIexandriaServer;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rmcp::{transport::stdio, ServiceExt};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "aiexandria", about = "MCP server for git-hosted documentation")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to the registry config (defines which top-level indexes are exposed).
    /// Required when running the MCP server (no subcommand).
    #[arg(long, env = "AIEXANDRIA_REGISTRY", global = true)]
    registry: Option<PathBuf>,

    /// Path to the central auth config. Defaults to $XDG_CONFIG_HOME/aiexandria/auth.yaml.
    #[arg(long, env = "AIEXANDRIA_AUTH", global = true)]
    auth: Option<PathBuf>,

    /// Override the repo cache root.
    #[arg(long, env = "AIEXANDRIA_CACHE", global = true)]
    cache: Option<PathBuf>,

    /// How often to re-fetch a cached repo. Examples: "5m", "1h", "24h", "never".
    /// Per-registry-entry `refresh:` settings override this. Default: never.
    #[arg(
        long,
        env = "AIEXANDRIA_REFRESH",
        default_value = "never",
        global = true
    )]
    refresh: String,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Generate a `pinakes.yaml` for a repo. Writes to stdout (or
    /// --output) and optionally to nested directories.
    Generate {
        /// Local path to a checked-out repo, OR a git URL to clone.
        #[arg(long)]
        repo: String,

        /// Branch to clone when --repo is a URL.
        #[arg(long)]
        branch: Option<String>,

        /// Where to write the generated root index. Defaults to stdout.
        #[arg(long)]
        output: Option<PathBuf>,

        /// Don't write nested `pinakes.yaml` files into subdirectories.
        #[arg(long)]
        no_nested: bool,

        /// Don't include the default ripgrep search command.
        #[arg(long)]
        no_search: bool,

        /// Skip submodule init when cloning. Ignored when --repo is a path.
        #[arg(long)]
        no_submodules: bool,
    },
}

fn parse_refresh(s: &str) -> anyhow::Result<RefreshPolicy> {
    if s.eq_ignore_ascii_case("never") || s == "0" {
        return Ok(RefreshPolicy::Never);
    }
    let d: Duration = humantime::parse_duration(s)
        .with_context(|| format!("invalid --refresh value '{s}' (try '5m', '1h', 'never')"))?;
    Ok(RefreshPolicy::from_duration(d))
}

fn load_auth(cli: &Cli) -> Result<AuthConfig> {
    let auth_path = cli.auth.clone().or_else(AuthConfig::default_path);
    match auth_path {
        Some(p) if p.exists() => {
            AuthConfig::load_from(&p).with_context(|| format!("loading auth from {}", p.display()))
        }
        _ => Ok(AuthConfig::default()),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let cli = Cli::parse();

    if let Some(cmd) = &cli.command {
        return run_subcommand(&cli, cmd).await;
    }

    let registry_path = cli
        .registry
        .clone()
        .or_else(RegistryConfig::default_path)
        .context(
            "no registry config found — pass --registry, set AIEXANDRIA_REGISTRY, \
             or create ~/.config/aiexandria/registry.yaml",
        )?;
    let registry = RegistryConfig::load_from(&registry_path)
        .with_context(|| format!("loading registry from {}", registry_path.display()))?;

    let auth = load_auth(&cli)?;
    let refresh = parse_refresh(&cli.refresh)?;
    let cache_root = cli.cache.clone().unwrap_or_else(RepoCache::default_root);
    let cache = RepoCache::new(cache_root, auth).with_default_refresh(refresh);

    let server = AIexandriaServer::new(registry, cache);
    let service = server
        .serve(stdio())
        .await
        .context("starting MCP service")?;
    service.waiting().await?;
    Ok(())
}

async fn run_subcommand(cli: &Cli, cmd: &Command) -> Result<()> {
    match cmd {
        Command::Generate {
            repo,
            branch,
            output,
            no_nested,
            no_search,
            no_submodules,
        } => {
            let opts = GenerateOptions {
                recursive: true,
                write_nested: !no_nested,
                include_search_command: !no_search,
            };

            // If repo is a local path, generate against it directly.
            // Otherwise treat it as a URL and clone via the cache.
            let local_path: PathBuf = if PathBuf::from(repo).exists() {
                PathBuf::from(repo)
            } else {
                let auth = load_auth(cli)?;
                let cache_root = cli.cache.clone().unwrap_or_else(RepoCache::default_root);
                let cache = RepoCache::new(cache_root, auth);
                let git_ref = GitRef {
                    url: repo.clone(),
                    branch: branch.clone(),
                    submodules: !no_submodules,
                    ..Default::default()
                };
                cache
                    .ensure(&git_ref)
                    .with_context(|| format!("cloning {repo}"))?
            };

            let yaml = generate_index(&local_path, &opts)
                .with_context(|| format!("generating index for {}", local_path.display()))?;
            match output {
                Some(p) => {
                    std::fs::write(p, &yaml).with_context(|| format!("writing {}", p.display()))?;
                    eprintln!("wrote {}", p.display());
                }
                None => print!("{yaml}"),
            }
            Ok(())
        }
    }
}
