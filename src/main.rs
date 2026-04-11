use aerosync_core::{
    auth::{AuthConfig, AuthManager},
    resume::ResumeStore,
    server::{FileReceiver, ServerConfig},
    transfer::{TransferConfig, TransferEngine, TransferTask},
    FileManager,
};
use aerosync_protocols::{
    http::HttpConfig,
    quic::QuicConfig,
    AutoAdapter,
};
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid as _Uuid;

#[derive(Parser)]
#[command(
    name = "aerosync",
    about = "High-performance cross-network file transfer for agents",
    version = "0.2.0"
)]
struct Cli {
    /// 详细日志输出
    #[arg(short, long, global = true)]
    verbose: bool,

    /// 配置文件路径
    #[arg(long, global = true, default_value = "~/.aerosync/config.toml")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 发送文件或目录到远端
    Send {
        /// 源文件或目录路径
        source: PathBuf,

        /// 目标地址，格式: host:port 或 protocol://host:port/path
        /// 示例: 192.168.1.10:7789, http://host:7788/upload, s3://bucket/path
        destination: String,

        /// 递归发送目录
        #[arg(short, long)]
        recursive: bool,

        /// 强制使用指定协议: quic | http
        #[arg(long)]
        protocol: Option<String>,

        /// 认证 Token
        #[arg(long)]
        token: Option<String>,

        /// 并发流数量（默认 4）
        #[arg(long, default_value = "4")]
        parallel: usize,

        /// 跳过 SHA-256 完整性校验
        #[arg(long)]
        no_verify: bool,

        /// 只显示传输计划，不实际传输
        #[arg(long)]
        dry_run: bool,

        /// 禁用断点续传（强制重新传输）
        #[arg(long)]
        no_resume: bool,
    },

    /// 启动接收端，监听文件传输
    Receive {
        /// HTTP 监听端口
        #[arg(long, default_value = "7788")]
        port: u16,

        /// QUIC 监听端口
        #[arg(long, default_value = "7789")]
        quic_port: u16,

        /// 文件保存目录
        #[arg(long, default_value = "./received")]
        save_to: PathBuf,

        /// 绑定地址
        #[arg(long, default_value = "0.0.0.0")]
        bind: String,

        /// 要求发送方携带此 Token（留空不启用认证）
        #[arg(long)]
        auth_token: Option<String>,

        /// 接收一个文件后自动退出
        #[arg(long)]
        one_shot: bool,

        /// 允许覆盖同名文件
        #[arg(long)]
        overwrite: bool,

        /// 最大文件大小（字节，默认 100GB）
        #[arg(long, default_value = "107374182400")]
        max_size: u64,

        /// 仅启用 HTTP（禁用 QUIC）
        #[arg(long)]
        http_only: bool,
    },

    /// Token 管理
    Token {
        #[command(subcommand)]
        action: TokenAction,
    },

    /// 查看传输状态
    Status {
        /// 远端接收器地址，格式 host:port
        #[arg(default_value = "localhost:7788")]
        host: String,
    },

    /// 列出并管理未完成的断点续传任务
    Resume {
        #[command(subcommand)]
        action: ResumeAction,
    },
}

#[derive(Subcommand)]
enum TokenAction {
    /// 生成新 Token
    Generate {
        /// 密钥（留空自动生成）
        #[arg(long)]
        secret: Option<String>,
        /// 有效时长（小时，默认 24）
        #[arg(long, default_value = "24")]
        hours: u64,
    },
    /// 验证 Token
    Verify {
        token: String,
        #[arg(long)]
        secret: String,
    },
}

#[derive(Subcommand)]
enum ResumeAction {
    /// 列出所有未完成的传输任务
    List {
        /// 状态文件目录（默认当前目录）
        #[arg(long, default_value = ".")]
        state_dir: PathBuf,
    },
    /// 清除指定 task_id 的续传状态
    Clear {
        task_id: String,
        #[arg(long, default_value = ".")]
        state_dir: PathBuf,
    },
    /// 清除所有未完成的续传状态
    ClearAll {
        #[arg(long, default_value = ".")]
        state_dir: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // 初始化日志
    let level = if cli.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level)),
        )
        .init();

    match cli.command {
        Commands::Send {
            source,
            destination,
            recursive,
            protocol,
            token,
            parallel,
            no_verify,
            dry_run,
            no_resume,
        } => {
            cmd_send(source, destination, recursive, protocol, token, parallel, no_verify, dry_run, no_resume).await?;
        }

        Commands::Receive {
            port,
            quic_port,
            save_to,
            bind,
            auth_token,
            one_shot,
            overwrite,
            max_size,
            http_only,
        } => {
            cmd_receive(port, quic_port, save_to, bind, auth_token, one_shot, overwrite, max_size, http_only).await?;
        }

        Commands::Token { action } => {
            cmd_token(action).await?;
        }

        Commands::Status { host } => {
            cmd_status(host).await?;
        }

        Commands::Resume { action } => {
            cmd_resume(action).await?;
        }
    }

    Ok(())
}

// ──────────────────────────── send ──────────────────────────────────────────

async fn cmd_send(
    source: PathBuf,
    destination: String,
    recursive: bool,
    _protocol: Option<String>,
    token: Option<String>,
    _parallel: usize,
    no_verify: bool,
    dry_run: bool,
    no_resume: bool,
) -> anyhow::Result<()> {
    // 收集要发送的文件列表
    let files = collect_files(&source, recursive).await?;

    if files.is_empty() {
        eprintln!("No files found at: {}", source.display());
        return Ok(());
    }

    let total_size: u64 = files.iter().map(|f| f.1).sum();
    println!(
        "Sending {} file(s), total {:.2} MB",
        files.len(),
        total_size as f64 / 1_048_576.0
    );

    if dry_run {
        println!("\nDry run — files that would be sent:");
        for (path, size) in &files {
            println!("  {} ({:.2} KB)", path.display(), *size as f64 / 1024.0);
        }
        return Ok(());
    }

    // 自动补全 destination URL
    let dest_url = normalize_destination(&destination, &source, recursive);

    let config = TransferConfig {
        max_concurrent_transfers: 4,
        chunk_size: 4 * 1024 * 1024,
        retry_attempts: 3,
        timeout_seconds: 60,
        use_quic: !destination.starts_with("http"),
        auth_token: token.clone(),
        enable_resume: !no_resume,
        ..TransferConfig::default()
    };

    // 构建协议适配器
    let http_config = HttpConfig {
        timeout_seconds: 60,
        max_retries: 3,
        chunk_size: 4 * 1024 * 1024,
        auth_token: token.clone(),
    };
    let quic_config = QuicConfig {
        auth_token: token.clone(),
        ..QuicConfig::default()
    };
    let adapter = Arc::new(AutoAdapter::new(http_config, quic_config));

    let engine = TransferEngine::new(config);
    engine.start(adapter).await?;

    let pb = ProgressBar::new(total_size);
    pb.set_style(
        ProgressStyle::with_template(
            "[{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta}) {msg}",
        )
        .unwrap()
        .progress_chars("=>-"),
    );

    for (path, size) in &files {
        let msg = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        pb.set_message(msg.clone());

        // 计算 SHA-256（除非跳过校验）
        let sha256 = if !no_verify {
            match FileManager::compute_sha256(&path).await {
                Ok(h) => Some(h),
                Err(e) => {
                    tracing::warn!("Could not compute SHA-256 for {}: {}", path.display(), e);
                    None
                }
            }
        } else {
            None
        };

        let task_dest = if dest_url.ends_with('/') {
            format!("{}{}", dest_url, msg)
        } else {
            dest_url.clone()
        };

        let mut task = TransferTask::new_upload(path.clone(), task_dest, *size);
        task.sha256 = sha256;

        engine.add_transfer(task).await?;
        pb.inc(*size);
    }

    // 等待所有任务完成（简单 poll）
    tokio::time::sleep(Duration::from_millis(500)).await;
    let monitor = engine.get_progress_monitor().await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    loop {
        {
            let m = monitor.read().await;
            let stats = m.get_stats();
            if stats.completed_files + stats.failed_files >= stats.total_files {
                break;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            eprintln!("Timeout waiting for transfers");
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    pb.finish_with_message("Done");

    let m = monitor.read().await;
    let stats = m.get_stats();
    println!(
        "\nCompleted: {}/{} files, Failed: {}",
        stats.completed_files, stats.total_files, stats.failed_files
    );

    Ok(())
}

/// 收集要发送的文件列表，返回 (path, size) 对
async fn collect_files(source: &PathBuf, recursive: bool) -> anyhow::Result<Vec<(PathBuf, u64)>> {
    let meta = tokio::fs::metadata(source).await?;
    if meta.is_file() {
        return Ok(vec![(source.clone(), meta.len())]);
    }
    if !meta.is_dir() {
        return Err(anyhow::anyhow!("Source is not a file or directory"));
    }
    if !recursive {
        return Err(anyhow::anyhow!(
            "'{}' is a directory. Use --recursive to send directories.",
            source.display()
        ));
    }

    let mut result = Vec::new();
    collect_files_recursive(source, source, &mut result).await?;
    Ok(result)
}

fn collect_files_recursive<'a>(
    base: &'a PathBuf,
    dir: &'a PathBuf,
    out: &'a mut Vec<(PathBuf, u64)>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
    Box::pin(async move {
        let mut entries = tokio::fs::read_dir(dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let meta = entry.metadata().await?;
            if meta.is_file() {
                out.push((path, meta.len()));
            } else if meta.is_dir() {
                collect_files_recursive(base, &path, out).await?;
            }
        }
        Ok(())
    })
}

/// 将用户输入的 destination 规范化为完整 URL
fn normalize_destination(dest: &str, _source: &PathBuf, _recursive: bool) -> String {
    if dest.starts_with("http://") || dest.starts_with("https://") || dest.starts_with("quic://") {
        return dest.to_string();
    }
    // host:port 格式 → 先尝试 QUIC，再 HTTP
    // 此处默认用 HTTP（自动协商在 Phase 2 实现）
    format!("http://{}/upload", dest)
}

// ──────────────────────────── receive ───────────────────────────────────────

async fn cmd_receive(
    port: u16,
    quic_port: u16,
    save_to: PathBuf,
    bind: String,
    auth_token: Option<String>,
    one_shot: bool,
    overwrite: bool,
    max_size: u64,
    http_only: bool,
) -> anyhow::Result<()> {
    // 构建认证配置
    let auth_cfg = auth_token.map(|token| {
        // 以 token 作为 secret key 验证（接收方预期 token 与 secret 相同）
        let secret = format!("aerosync-recv-{}", token);
        // 创建 TokenManager 并注册这个 token
        AuthConfig {
            enable_auth: true,
            secret_key: secret,
            token_lifetime_hours: 24,
            allowed_ips: vec![],
        }
    });

    let config = ServerConfig {
        http_port: port,
        quic_port,
        bind_address: bind.clone(),
        receive_directory: save_to.clone(),
        max_file_size: max_size,
        allow_overwrite: overwrite,
        enable_http: true,
        enable_quic: !http_only,
        auth: auth_cfg,
    };

    println!("AeroSync receiver starting...");
    println!("  HTTP:  {}:{}", bind, port);
    if !http_only {
        println!("  QUIC:  {}:{}", bind, quic_port);
    }
    println!("  Save:  {}", save_to.display());
    if overwrite {
        println!("  Mode:  overwrite enabled");
    }
    println!("\nReady. Waiting for files... (Ctrl+C to stop)\n");

    let mut receiver = FileReceiver::new(config);
    receiver.start().await?;

    if one_shot {
        // 等待第一个文件到达后退出
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let files = receiver.get_received_files().await;
            if !files.is_empty() {
                let f = &files[0];
                println!(
                    "Received: {} ({:.2} KB) -> {}",
                    f.original_name,
                    f.size as f64 / 1024.0,
                    f.saved_path.display()
                );
                receiver.stop().await?;
                break;
            }
        }
    } else {
        // 持续运行，打印接收到的文件
        let mut last_count = 0;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let files = receiver.get_received_files().await;
            if files.len() > last_count {
                for f in &files[last_count..] {
                    println!(
                        "  [+] {} ({:.2} KB) sha256={}",
                        f.original_name,
                        f.size as f64 / 1024.0,
                        f.sha256.as_deref().map(|h| &h[..8]).unwrap_or("none")
                    );
                }
                last_count = files.len();
            }
        }
    }

    Ok(())
}

// ──────────────────────────── token ─────────────────────────────────────────

async fn cmd_token(action: TokenAction) -> anyhow::Result<()> {
    match action {
        TokenAction::Generate { secret, hours } => {
            let secret_key = secret.unwrap_or_else(|| {
                format!("{}-{}", uuid::Uuid::new_v4(), uuid::Uuid::new_v4())
            });

            let config = AuthConfig {
                enable_auth: true,
                secret_key: secret_key.clone(),
                token_lifetime_hours: hours,
                allowed_ips: vec![],
            };

            let manager = AuthManager::new(config)
                .map_err(|e| anyhow::anyhow!("Failed to create auth manager: {}", e))?;

            let token = manager
                .generate_token()
                .map_err(|e| anyhow::anyhow!("Failed to generate token: {}", e))?;

            println!("Token:   {}", token);
            println!("Secret:  {}", secret_key);
            println!("Expires: {} hours", hours);
            println!("\nUsage:");
            println!("  aerosync send ./file host:7788 --token {}", &token[..32]);
            println!("  aerosync receive --auth-token <same-token>");
        }

        TokenAction::Verify { token, secret } => {
            let config = AuthConfig {
                enable_auth: true,
                secret_key: secret,
                token_lifetime_hours: 24,
                allowed_ips: vec![],
            };
            let manager = AuthManager::new(config)
                .map_err(|e| anyhow::anyhow!("Failed to create auth manager: {}", e))?;

            match manager.authenticate(Some(&token), "127.0.0.1") {
                Ok(true) => println!("Token is VALID"),
                Ok(false) => println!("Token is INVALID or EXPIRED"),
                Err(e) => println!("Verification error: {}", e),
            }
        }
    }
    Ok(())
}

// ──────────────────────────── status ────────────────────────────────────────

async fn cmd_status(host: String) -> anyhow::Result<()> {
    let url = if host.starts_with("http") {
        format!("{}/health", host)
    } else {
        format!("http://{}/health", host)
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;

    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            let body: serde_json::Value = resp.json().await?;
            println!("Status: {}", body["status"].as_str().unwrap_or("unknown"));
            println!(
                "Received files: {}",
                body["received_files"].as_u64().unwrap_or(0)
            );
        }
        Ok(resp) => {
            eprintln!("Server returned: {}", resp.status());
        }
        Err(e) => {
            eprintln!("Cannot reach {}: {}", host, e);
        }
    }

    Ok(())
}

// ──────────────────────────── resume ────────────────────────────────────────

async fn cmd_resume(action: ResumeAction) -> anyhow::Result<()> {
    match action {
        ResumeAction::List { state_dir } => {
            let store = ResumeStore::new(&state_dir);
            let pending = store.list_pending().await?;
            if pending.is_empty() {
                println!("No pending resume tasks.");
            } else {
                println!("{} pending transfer(s):\n", pending.len());
                for s in &pending {
                    let done = s.completed_chunks.len();
                    let total = s.total_chunks;
                    let pct = if total > 0 { done * 100 / total as usize } else { 0 };
                    println!(
                        "  [{}] {} → {}",
                        s.task_id,
                        s.source_path.display(),
                        s.destination
                    );
                    println!(
                        "      Progress: {}/{} chunks ({}%), {:.2} MB / {:.2} MB",
                        done,
                        total,
                        pct,
                        s.bytes_transferred() as f64 / 1_048_576.0,
                        s.total_size as f64 / 1_048_576.0
                    );
                }
                println!("\nResume with: aerosync send <source> <destination>");
            }
        }

        ResumeAction::Clear { task_id, state_dir } => {
            let uuid = task_id
                .parse::<uuid::Uuid>()
                .map_err(|_| anyhow::anyhow!("Invalid task ID: {}", task_id))?;
            let store = ResumeStore::new(&state_dir);
            store.delete(uuid).await?;
            println!("Cleared resume state for task {}", task_id);
        }

        ResumeAction::ClearAll { state_dir } => {
            let store = ResumeStore::new(&state_dir);
            let pending = store.list_pending().await?;
            let count = pending.len();
            for s in pending {
                store.delete(s.task_id).await?;
            }
            println!("Cleared {} resume state(s).", count);
        }
    }
    Ok(())
}
