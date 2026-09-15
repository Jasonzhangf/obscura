#[cfg(unix)]
mod daemon;
#[cfg(unix)]
mod worker;
#[cfg(unix)]
mod media;
use clap::Parser;

#[derive(Parser)]
#[command(about = "Local persistent browser Session (one session per process)")]
struct Args {
    /// New private directory for host.sock. Existing directories are rejected.
    #[arg(long)]
    socket_dir: std::path::PathBuf,
    /// Directory containing the persistent browser profile (cookies.json).
    #[arg(long)]
    storage_dir: Option<std::path::PathBuf>,
    #[arg(long)]
    allow_private_network: bool,
}

#[cfg(unix)]
#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    daemon::serve(args.socket_dir, args.storage_dir, args.allow_private_network).await
}

#[cfg(not(unix))]
fn main() { eprintln!("Local Host requires Unix sockets"); std::process::exit(1); }
