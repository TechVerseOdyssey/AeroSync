//! `aerosync-rendezvous` binary — RFC-004 control-plane server (v0.4 scaffold).

use aerosync_rendezvous::{connect_database, serve};
use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "aerosync-rendezvous",
    about = "RFC-004 WAN rendezvous server (registry + future signaling)"
)]
struct Cli {
    #[arg(long, default_value = "127.0.0.1:8787")]
    bind: String,
    /// SQLite database file path (created if missing).
    #[arg(long, env = "RENDEZVOUS_DATABASE", default_value = "rendezvous.db")]
    database: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let addr: SocketAddr = cli
        .bind
        .parse()
        .map_err(|e| anyhow::anyhow!("--bind: {e}"))?;
    let pool = connect_database(cli.database.as_path()).await?;
    serve(addr, pool).await
}
