#[cfg(unix)]
mod catalog;
#[cfg(unix)]
mod daemon;
#[cfg(unix)]
mod remote_admission;
#[cfg(unix)]
mod remote_authorization;
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
    /// Explicit persistent profile root. When set, the Host loads/creates the
    /// typed profile catalog under this directory and each profile owns a
    /// child directory. When omitted the Host runs a single in-memory profile.
    #[arg(long)]
    profile_root: Option<std::path::PathBuf>,
    /// Explicit typed authorization registry. Required for remote admission.
    /// The path is configuration truth, never discovered from payload or logs.
    #[arg(long)]
    authorization_registry: Option<std::path::PathBuf>,
    #[arg(long)]
    allow_private_network: bool,
}

#[cfg(unix)]
#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    daemon::serve(
        args.socket_dir,
        args.profile_root,
        args.authorization_registry,
        args.allow_private_network,
    )
    .await
}

#[cfg(not(unix))]
fn main() { eprintln!("Local Host requires Unix sockets"); std::process::exit(1); }
