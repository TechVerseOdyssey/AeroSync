use aerosync_rendezvous::jwt::{decoding_key_from_private_pkcs8_pem, encoding_key_from_rsa_pem};
use aerosync_rendezvous::signaling::SignalingRegistry;
use aerosync_rendezvous::{app_router, connect_database, default_register_ratelimit, AppState};
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use jsonwebtoken::{DecodingKey, EncodingKey};
use quinn::{ClientConfig, Endpoint};
use rsa::pkcs8::{EncodePrivateKey, LineEnding};
use rsa::RsaPrivateKey;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::Deserialize;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tempfile::tempdir;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message as WsMessage;

#[derive(Debug, Deserialize)]
struct RegisterResp {
    jwt: String,
}

#[derive(Debug, Deserialize)]
struct InitiateResp {
    signaling: Signaling,
}

#[derive(Debug, Deserialize)]
struct Signaling {
    websocket_path: String,
}

#[tokio::test]
async fn p2_signaling_punch_and_quic_local_e2e() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("rv.db");
    let pem = rsa_pem();
    let enc: Arc<EncodingKey> = Arc::new(encoding_key_from_rsa_pem(&pem).unwrap());
    let dec: Arc<DecodingKey> = Arc::new(decoding_key_from_private_pkcs8_pem(&pem).unwrap());
    let pool = connect_database(&db).await.unwrap();

    let state = Arc::new(AppState {
        pool,
        jwt_issuer: "test-iss".into(),
        jwt_ttl_secs: 3600,
        encoding_key: enc,
        decoding_key: dec,
        register_ratelimit: default_register_ratelimit(),
        signaling: Arc::new(SignalingRegistry::new()),
    });

    let app = app_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bind = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let base = format!("http://{bind}");
    let http = reqwest::Client::new();
    let alice_jwt = register_peer(&http, &base, "alice", [1u8; 32]).await;
    let bob_jwt = register_peer(&http, &base, "bob", [2u8; 32]).await;

    let init: InitiateResp = http
        .post(format!("{base}/v1/sessions/initiate"))
        .bearer_auth(&alice_jwt)
        .json(&json!({
            "target_name": "bob",
            "target_namespace": ""
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();

    let ws_a_url = format!(
        "ws://{bind}{}?token={}",
        init.signaling.websocket_path, alice_jwt
    );
    let ws_b_url = format!(
        "ws://{bind}{}?token={}",
        init.signaling.websocket_path, bob_jwt
    );
    let (mut ws_a, _) = tokio_tungstenite::connect_async(ws_a_url).await.unwrap();
    let (mut ws_b, _) = tokio_tungstenite::connect_async(ws_b_url).await.unwrap();

    // First frame from each peer should be signaling-ready.
    let ready_a = next_text_frame(&mut ws_a).await;
    let ready_b = next_text_frame(&mut ws_b).await;
    assert!(ready_a.contains("aerosync.signaling.ready"));
    assert!(ready_b.contains("aerosync.signaling.ready"));

    let (cert_der, key_der) = dev_cert_der();
    let ep_a = make_endpoint(cert_der.clone(), key_der.clone_key());
    let ep_b = make_endpoint(cert_der.clone(), key_der.clone_key());
    let addr_a = ep_a.local_addr().unwrap();
    let addr_b = ep_b.local_addr().unwrap();

    ws_a.send(WsMessage::Text(
        json!({
            "type": "candidates",
            "server_reflexive": addr_a.to_string(),
            "local": []
        })
        .to_string(),
    ))
    .await
    .unwrap();
    ws_b.send(WsMessage::Text(
        json!({
            "type": "candidates",
            "server_reflexive": addr_b.to_string(),
            "local": []
        })
        .to_string(),
    ))
    .await
    .unwrap();

    let (remote_for_a, punch_a) = read_remote_and_punch(&mut ws_a).await;
    let (remote_for_b, punch_b) = read_remote_and_punch(&mut ws_b).await;
    assert_eq!(remote_for_a, addr_b.to_string());
    assert_eq!(remote_for_b, addr_a.to_string());
    assert_eq!(punch_a, punch_b, "both peers should receive same punch_at");

    let now_ms = unix_ms();
    if punch_a > now_ms {
        tokio::time::sleep(Duration::from_millis(punch_a - now_ms)).await;
    }

    let a_incoming = ep_a.accept();
    let b_incoming = ep_b.accept();
    let a_connect = ep_a.connect(addr_b, "localhost").unwrap();
    let b_connect = ep_b.connect(addr_a, "localhost").unwrap();

    let (a_accept_conn, b_accept_conn, a_out, b_out) = timeout(Duration::from_secs(5), async {
        let a_accept = a_incoming.await.unwrap().await.unwrap();
        let b_accept = b_incoming.await.unwrap().await.unwrap();
        let a_conn = a_connect.await.unwrap();
        let b_conn = b_connect.await.unwrap();
        (a_accept, b_accept, a_conn, b_conn)
    })
    .await
    .expect("quic connect must complete");

    assert_eq!(a_accept_conn.remote_address(), addr_b);
    assert_eq!(b_accept_conn.remote_address(), addr_a);
    assert_eq!(a_out.remote_address(), addr_b);
    assert_eq!(b_out.remote_address(), addr_a);

    server.abort();
}

fn rsa_pem() -> Vec<u8> {
    let mut rng = rand::thread_rng();
    let key = RsaPrivateKey::new(&mut rng, 2048).unwrap();
    key.to_pkcs8_pem(LineEnding::LF)
        .unwrap()
        .as_bytes()
        .to_vec()
}

async fn register_peer(http: &reqwest::Client, base: &str, name: &str, key: [u8; 32]) -> String {
    let body = json!({
        "name": name,
        "public_key": base64::engine::general_purpose::STANDARD.encode(key),
        "capabilities": 3,
        "observed_addr": null
    });
    let r: RegisterResp = http
        .post(format!("{base}/v1/peers/register"))
        .json(&body)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    r.jwt
}

fn dev_cert_der() -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_der = CertificateDer::from(cert.cert.der().to_vec());
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()));
    (cert_der, key_der)
}

fn make_endpoint(cert_der: CertificateDer<'static>, key_der: PrivateKeyDer<'static>) -> Endpoint {
    let mut tls_server = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der.clone()], key_der)
        .unwrap();
    tls_server.alpn_protocols = vec![b"aerosync/1".to_vec()];
    let server_crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls_server).unwrap();
    let server_cfg = quinn::ServerConfig::with_crypto(Arc::new(server_crypto));
    let mut ep = Endpoint::server(server_cfg, "127.0.0.1:0".parse().unwrap()).unwrap();

    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert_der).unwrap();
    let mut tls_client = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls_client.alpn_protocols = vec![b"aerosync/1".to_vec()];
    let client_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls_client).unwrap();
    let client_cfg = ClientConfig::new(Arc::new(client_crypto));
    ep.set_default_client_config(client_cfg);
    ep
}

async fn next_text_frame(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> String {
    let msg = timeout(Duration::from_secs(3), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    match msg {
        WsMessage::Text(s) => s,
        other => panic!("expected text frame, got {other:?}"),
    }
}

async fn read_remote_and_punch(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> (String, u64) {
    let mut remote: Option<String> = None;
    let mut punch: Option<u64> = None;
    for _ in 0..8 {
        let text = next_text_frame(ws).await;
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        match v.get("type").and_then(|x| x.as_str()).unwrap_or("") {
            "remote.candidates" => {
                remote = v
                    .get("server_reflexive")
                    .and_then(|x| x.as_str())
                    .map(str::to_string);
            }
            "punch_at" => {
                punch = v.get("timestamp_ms").and_then(|x| x.as_u64());
            }
            _ => {}
        }
        if let (Some(r), Some(p)) = (&remote, punch) {
            return (r.clone(), p);
        }
    }
    panic!("did not receive remote.candidates + punch_at");
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
