//! Direct WSS endpoint. Browser authority stays on the Host-owned
//! `remote-admission.sock` admission stream.
#[cfg(unix)]
mod channels;
#[cfg(unix)]
mod encoder_control;
#[cfg(unix)]
mod server;
#[cfg(unix)]
#[path = "../remote_admission.rs"]
mod remote_admission;
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Direct WSS access to one existing local Host")]
struct Args {
    #[arg(long)] listen: std::net::SocketAddr,
    #[arg(long)] host_dir: PathBuf,
    /// New private directory for the one encoder's local output socket.
    #[arg(long)] socket_dir: PathBuf,
    /// DER leaf certificate and PKCS#8 DER private key. This is the listener's
    /// own TLS identity; no client certificate and no CA is involved.
    #[arg(long)] server_cert: PathBuf,
    #[arg(long)] server_key: PathBuf,
    #[arg(long)] media_bin: PathBuf,
    #[arg(long, default_value = "ffmpeg")] ffmpeg: PathBuf,
    /// Explicitly enable the UDP WebRTC media backend. WSS remains available
    /// independently; no implicit fallback or upgrade is performed.
    #[arg(long, default_value_t = false)] enable_webrtc: bool,
    /// Local address advertised by the explicitly enabled WebRTC backend.
    #[arg(long)] webrtc_bind_ip: Option<std::net::IpAddr>,
    /// Absolute path for the typed endpoint applied receipt. When absent no
    /// receipt is written.
    #[arg(long)] applied_receipt_path: Option<PathBuf>,
    /// Service-owned endpoint generation stamped into the applied receipt.
    #[arg(long, default_value_t = 0)] endpoint_generation: u64,
}

#[cfg(unix)]
#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> { server::serve(Args::parse()).await }

#[cfg(not(unix))]
fn main() { eprintln!("Endpoint requires a local Unix Host"); std::process::exit(1); }
