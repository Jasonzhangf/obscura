//! Prepaired direct WSS endpoint. Browser authority stays on host.sock.
#[cfg(unix)]
mod channels;
#[cfg(unix)]
mod server;
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Explicitly paired mTLS/WSS access to one existing local Host")]
struct Args {
    #[arg(long)] listen: std::net::SocketAddr,
    #[arg(long)] host_dir: PathBuf,
    /// New private directory for the one encoder's local output socket.
    #[arg(long)] socket_dir: PathBuf,
    /// DER leaf certificate and PKCS#8 DER private key.
    #[arg(long)] server_cert: PathBuf,
    #[arg(long)] server_key: PathBuf,
    /// DER trust anchor for explicitly paired client certificates.
    #[arg(long)] client_ca: PathBuf,
    #[arg(long)] media_bin: PathBuf,
    #[arg(long, default_value = "ffmpeg")] ffmpeg: PathBuf,
}

#[cfg(unix)]
#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> { server::serve(Args::parse()).await }

#[cfg(not(unix))]
fn main() { eprintln!("Endpoint requires a local Unix Host"); std::process::exit(1); }
