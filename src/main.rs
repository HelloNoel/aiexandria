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
#[command(
    name = "aiexandria",
    version,
    long_version = cli_version(),
    about = "MCP server for git-hosted documentation"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to the registry config (defines which top-level indexes are exposed).
    /// Uses the public starter collection when no registry is configured.
    #[arg(long, env = "AIEXANDRIA_REGISTRY", global = true)]
    registry: Option<PathBuf>,

    /// Path to the central auth config. Defaults to $XDG_CONFIG_HOME/aiexandria/auth.yaml.
    #[arg(long, env = "AIEXANDRIA_AUTH", global = true)]
    auth: Option<PathBuf>,

    /// Override the repo cache root.
    #[arg(long, env = "AIEXANDRIA_CACHE", global = true)]
    cache: Option<PathBuf>,
    /// Git executable (Git 2.43.7 or newer).
    #[arg(long, env = "AIEXANDRIA_GIT", global = true)]
    git: Option<PathBuf>,
    /// Maximum duration of an acquisition job, independently of MCP wait limits.
    #[arg(
        long,
        env = "AIEXANDRIA_REPO_TIMEOUT",
        default_value = "120s",
        global = true
    )]
    repo_timeout: String,

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
    /// Refresh index metadata and cached sources, or prewarm a selected route.
    Update {
        #[arg(long)]
        index: Option<String>,
        /// Entry-ID path components; repeat for nested indexes.
        #[arg(long)]
        path: Vec<String>,
        #[arg(long)]
        command_id: Option<String>,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        wait: bool,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value = "5m")]
        timeout: String,
    },
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

fn cli_version() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION
        .get_or_init(|| {
            format!(
                "{}\nPinakes v1 draft {}",
                env!("CARGO_PKG_VERSION"),
                aiexandria::index::SUPPORTED_DRAFT_REVISION
            )
        })
        .as_str()
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

fn load_cache(cli: &Cli) -> Result<RepoCache> {
    let mut git = aiexandria::git_cli::GitCli::default();
    if let Some(path) = &cli.git {
        git.executable = path.clone();
    }
    git.timeout = humantime::parse_duration(&cli.repo_timeout).context("invalid --repo-timeout")?;
    if git.timeout.is_zero() {
        anyhow::bail!("--repo-timeout must be positive");
    }
    git.check()?;
    Ok(RepoCache::new(
        cli.cache.clone().unwrap_or_else(RepoCache::default_root),
        load_auth(cli)?,
    )
    .with_default_refresh(parse_refresh(&cli.refresh)?)
    .with_git(git))
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("__git-credential") {
        aiexandria::git_cli::credential_helper(&std::env::args().nth(2).unwrap_or_default())?;
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let cli = Cli::parse();

    if let Some(cmd) = &cli.command {
        return run_subcommand(&cli, cmd).await;
    }

    let registry = RegistryConfig::select(
        cli.registry.as_deref(),
        RegistryConfig::default_path().as_deref(),
    )
    .context("loading documentation registry")?;

    let cache = load_cache(&cli)?;

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
        Command::Update {
            index,
            path,
            command_id,
            force,
            wait,
            dry_run,
            json,
            timeout,
        } => {
            let registry = usage(
                RegistryConfig::select(
                    cli.registry.as_deref(),
                    RegistryConfig::default_path().as_deref(),
                )
                .map_err(Into::into),
            );
            let cache = usage(load_cache(cli));
            let options = aiexandria::update::UpdateOptions {
                index: index.clone(),
                path: path.clone(),
                command_id: command_id.clone(),
                force: *force,
                wait: *wait,
                dry_run: *dry_run,
                timeout: usage(humantime::parse_duration(timeout).map_err(Into::into)),
            };
            usage(options.validate(&registry).map_err(Into::into));
            let report = tokio::task::spawn_blocking(move || {
                aiexandria::update::run(&cache, &registry, &options)
            })
            .await??;
            if *json {
                println!("{}", serde_json::to_string(&report)?);
            } else {
                println!(
                    "{}: {} checked, {} unchanged, {} skipped",
                    report.status, report.updated, report.unchanged, report.skipped
                );
                for error in &report.errors {
                    eprintln!("{error}");
                }
            }
            if matches!(report.status.as_str(), "failed" | "timed_out") {
                std::process::exit(1);
            }
            Ok(())
        }
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
            let (local_path, _remote_view) = if PathBuf::from(repo).exists() {
                (PathBuf::from(repo), None)
            } else {
                let cache = load_cache(cli)?;
                let cache = cache.session(
                    aiexandria::cache::Freshness::CachedOk,
                    std::time::Instant::now() + cache.git.timeout,
                );
                let git_ref = GitRef {
                    url: repo.clone(),
                    branch: branch.clone(),
                    submodules: !no_submodules,
                    ..Default::default()
                };
                // Generation may write nested indexes; preserve the published snapshot.
                let result = cache
                    .ensure(&git_ref)
                    .and_then(|dir| cache.execution_view(&dir));
                cache.git.cancel();
                let view = result.with_context(|| format!("cloning {repo}"))?;
                (view.path().to_path_buf(), Some(view))
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

fn usage<T>(value: anyhow::Result<T>) -> T {
    value.unwrap_or_else(|error| {
        eprintln!("{error:#}");
        std::process::exit(2)
    })
}
