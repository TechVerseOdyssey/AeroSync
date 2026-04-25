//! `aerosync-rendezvous` binary — RFC-004 control-plane server (registry + JWT).

use aerosync_rendezvous::signaling::SignalingRegistry;
use aerosync_rendezvous::{connect_database, default_register_ratelimit, jwt, serve, AppState};
use clap::Parser;
use jsonwebtoken::{DecodingKey, EncodingKey};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

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
    /// PKCS#8 PEM path for RSA private key used to sign JWTs (RS256).
    #[arg(long, env = "RENDEZVOUS_JWT_RSA_PRIVATE_KEY_PATH")]
    jwt_rsa_private_key: PathBuf,
    #[arg(long, default_value = "aerosync-rendezvous")]
    jwt_issuer: String,
    #[arg(long, default_value_t = 86_400_u64)]
    jwt_ttl_secs: u64,
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

    let pem = std::fs::read(&cli.jwt_rsa_private_key)?;
    let encoding_key: Arc<EncodingKey> = Arc::new(jwt::encoding_key_from_rsa_pem(&pem)?);
    let decoding_key: Arc<DecodingKey> = Arc::new(jwt::decoding_key_from_private_pkcs8_pem(&pem)?);

    let pool = connect_database(cli.database.as_path()).await?;
    let state = Arc::new(AppState {
        pool,
        jwt_issuer: cli.jwt_issuer,
        jwt_ttl_secs: cli.jwt_ttl_secs,
        encoding_key,
        decoding_key,
        register_ratelimit: default_register_ratelimit(),
        signaling: Arc::new(SignalingRegistry::new()),
    });

    serve(addr, state).await
}
