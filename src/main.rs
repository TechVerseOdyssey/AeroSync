mod config;
use config::AeroSyncConfig;
use zeroize::Zeroizing;

use aerosync_core::{
    auth::{AuthConfig, AuthManager},
    discovery::AeroSyncMdns,
    preflight::preflight_check,
    resume::ResumeStore,
    server::{FileReceiver, ServerConfig, TlsConfig},
    transfer::{TransferConfig, TransferEngine, TransferTask},
    FileManager,
};
use aerosync_protocols::{http::HttpConfig, quic::QuicConfig, ratelimit::parse_limit, AutoAdapter};
use clap::{Parser, Subcommand};
use futures::stream::{self, StreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

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

        /// 跳过发送前磁盘空间预检验
        #[arg(long)]
        no_preflight: bool,

        /// 上传带宽限速，格式: 512KB / 10MB / 1MB/s（0 或不指定 = 不限速）
        #[arg(long)]
        limit: Option<String>,

        /// 服务端 DER 格式证书文件，用于 TLS 证书钉扎（可多次指定）
        /// 示例: --pin-cert server.der --pin-cert ca.der
        #[arg(long, value_name = "CERT_DER")]
        pin_cert: Vec<PathBuf>,

        /// 接受自签名 / 无效 TLS 证书（用于连接 --https 自签名接收端）
        /// [警告] 仅限内网测试环境，生产环境请用 --pin-cert 进行证书钉扎
        #[arg(long)]
        accept_invalid_certs: bool,
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

        /// TLS 证书文件路径（PEM 格式，用于 QUIC）
        #[arg(long)]
        tls_cert: Option<PathBuf>,

        /// TLS 私钥文件路径（PEM 格式，用于 QUIC）
        #[arg(long)]
        tls_key: Option<PathBuf>,

        /// 启用 HTTPS（自动生成自签名证书，或使用 --tls-cert/--tls-key 指定外部证书）
        #[arg(long)]
        https: bool,

        /// HTTPS 监听端口（默认 7790）
        #[arg(long, default_value = "7790")]
        https_port: u16,

        /// 禁用 HTTP 明文服务（配合 --https 实现 HTTPS-only 模式）
        #[arg(long)]
        no_http: bool,
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

    /// 查看传输历史记录
    History {
        /// 最多显示 N 条（默认 20）
        #[arg(long, default_value = "20")]
        limit: usize,

        /// 只显示发送记录
        #[arg(long)]
        sent: bool,

        /// 只显示接收记录
        #[arg(long)]
        received: bool,

        /// 只显示成功记录
        #[arg(long)]
        success_only: bool,
    },

    /// 订阅接收端 WebSocket 事件流（实时感知文件到达）
    ///
    /// 示例:
    ///   aerosync watch                          # 连接 localhost:7788，pretty 格式
    ///   aerosync watch 10.0.0.5:7788            # 连接指定主机
    ///   aerosync watch --format json 2>/dev/null  # agent 模式，只输出 completed JSON
    ///   aerosync watch --filter completed       # 只显示 completed 事件
    Watch {
        /// 接收端地址，格式 host:port 或 ws://host:port/ws
        #[arg(default_value = "localhost:7788")]
        host: String,

        /// 只输出包含指定事件类型的消息（transfer_started / progress / completed / failed）
        #[arg(long)]
        filter: Option<String>,

        /// 输出格式：pretty（默认，人类可读）或 json（machine-readable，适合 agent 脚本解析）
        #[arg(long, default_value = "pretty")]
        format: String,

        /// 断连后自动重连（默认关闭）
        #[arg(long)]
        reconnect: bool,

        /// 最大重连次数，0 表示无限重连（默认 0，需配合 --reconnect 使用）
        #[arg(long, default_value = "0")]
        max_retries: u32,

        /// 初始重连等待时间（秒），每次失败后翻倍，上限 60 秒（默认 2）
        #[arg(long, default_value = "2")]
        retry_delay: u64,
    },

    /// 扫描局域网内的 AeroSync receiver（mDNS 发现）
    ///
    /// 示例:
    ///   aerosync discover                  # 扫描 3 秒，打印所有 receiver
    ///   aerosync discover --timeout 5      # 扫描 5 秒
    ///   aerosync discover --json           # JSON 格式输出（适合脚本）
    Discover {
        /// 扫描等待时间（秒，默认 3）
        #[arg(long, default_value = "3")]
        timeout: u64,

        /// 以 JSON 格式输出（每行一个 receiver，便于脚本解析）
        #[arg(long)]
        json: bool,
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
        /// 保存到磁盘（~/.config/aerosync/tokens.toml）
        #[arg(long)]
        save: bool,
        /// 备注标签
        #[arg(long)]
        label: Option<String>,
    },
    /// 验证 Token
    Verify {
        token: String,
        #[arg(long)]
        secret: String,
    },
    /// 列出所有已保存的 Token
    List,
    /// 撤销一个已保存的 Token（支持前缀匹配）
    Revoke {
        /// Token 字符串或前缀（前 8 字符即可）
        token_prefix: String,
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

    // 加载配置文件（CLI 参数优先级最高）
    let app_config = if let Some(ref cfg_path) = cli.config {
        let expanded = shellexpand::tilde(&cfg_path.to_string_lossy()).to_string();
        AeroSyncConfig::load(std::path::Path::new(&expanded)).unwrap_or_default()
    } else {
        AeroSyncConfig::default()
    };

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
            no_preflight,
            limit,
            pin_cert,
            accept_invalid_certs,
        } => {
            cmd_send(
                source,
                destination,
                recursive,
                protocol,
                token,
                parallel,
                no_verify,
                dry_run,
                no_resume,
                no_preflight,
                limit,
                pin_cert,
                accept_invalid_certs,
                &app_config,
            )
            .await?;
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
            tls_cert,
            tls_key,
            https,
            https_port,
            no_http,
        } => {
            cmd_receive(
                port,
                quic_port,
                save_to,
                bind,
                auth_token,
                one_shot,
                overwrite,
                max_size,
                http_only,
                no_http,
                tls_cert,
                tls_key,
                https,
                https_port,
                &app_config,
                cli.config.clone(),
            )
            .await?;
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

        Commands::History {
            limit,
            sent,
            received,
            success_only,
        } => {
            cmd_history(limit, sent, received, success_only).await?;
        }

        Commands::Watch {
            host,
            filter,
            format,
            reconnect,
            max_retries,
            retry_delay,
        } => {
            cmd_watch(host, filter, format, reconnect, max_retries, retry_delay).await?;
        }

        Commands::Discover { timeout, json } => {
            cmd_discover(timeout, json).await?;
        }
    }

    Ok(())
}

// ──────────────────────────── send ──────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
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
    no_preflight: bool,
    limit: Option<String>,
    pin_cert: Vec<PathBuf>,
    accept_invalid_certs: bool,
    app_config: &AeroSyncConfig,
) -> anyhow::Result<()> {
    // 收集要发送的文件列表
    let files = collect_files(&source, recursive).await?;

    if files.is_empty() {
        eprintln!("No files found at: {}", source.display());
        return Ok(());
    }

    let total_size: u64 = files.iter().map(|f| f.2).sum();
    println!(
        "Sending {} file(s), total {:.2} MB",
        files.len(),
        total_size as f64 / 1_048_576.0
    );

    if dry_run {
        println!("\nDry run — files that would be sent:");
        for (path, rel, size) in &files {
            println!(
                "  {} → {} ({:.2} KB)",
                path.display(),
                rel.display(),
                *size as f64 / 1024.0
            );
        }
        return Ok(());
    }

    // 自动协商协议（host:port 格式时探测对端是否支持 QUIC）
    let dest_url = negotiate_protocol(&destination).await;

    // 预检验：探测接收端磁盘空间
    if !no_preflight {
        // 从 dest_url 提取 HTTP base（quic:// 时转换为 http://）
        let http_base = extract_http_base(&dest_url, &destination);
        match preflight_check(&http_base, total_size).await {
            Ok(info) => {
                tracing::info!(
                    "Preflight OK: free={:.2} GB, version={:?}",
                    info.free_bytes as f64 / 1_073_741_824.0,
                    info.version
                );
            }
            Err(e) => {
                eprintln!("Preflight check failed: {}", e);
                eprintln!("Use --no-preflight to skip this check.");
                return Err(anyhow::anyhow!("Preflight failed: {}", e));
            }
        }
    }

    let config = TransferConfig {
        max_concurrent_transfers: app_config.transfer.max_concurrent,
        chunk_size: (app_config.transfer.chunk_size_mb * 1024 * 1024) as usize,
        retry_attempts: app_config.transfer.retry_attempts,
        timeout_seconds: app_config.transfer.timeout_seconds,
        use_quic: !destination.starts_with("http"),
        auth_token: token
            .clone()
            .or_else(|| app_config.auth.token.clone())
            .map(Zeroizing::new),
        enable_resume: !no_resume,
        ..TransferConfig::default()
    };

    // 构建协议适配器
    let eff_token = token.clone().or_else(|| app_config.auth.token.clone());
    let upload_limit_bps = limit.as_deref().and_then(parse_limit).unwrap_or(0);
    if upload_limit_bps > 0 {
        println!("Upload limit: {:.1} KB/s", upload_limit_bps as f64 / 1024.0);
    }
    let http_config = HttpConfig {
        timeout_seconds: app_config.transfer.timeout_seconds,
        max_retries: app_config.transfer.retry_attempts,
        chunk_size: (app_config.transfer.chunk_size_mb * 1024 * 1024) as usize,
        auth_token: eff_token.clone().map(Zeroizing::new),
        upload_limit_bps,
        accept_invalid_certs,
        pinned_server_certs: pin_cert.clone(),
        concurrent_chunks: 4,
        max_reconnect_attempts: 5,
        reconnect_base_delay_ms: 3_000,
    };
    let quic_config = QuicConfig {
        auth_token: eff_token.clone().map(Zeroizing::new),
        pinned_server_certs: pin_cert,
        ..QuicConfig::default()
    };
    let adapter = Arc::new(AutoAdapter::new(http_config, quic_config));

    let engine = TransferEngine::new(config);
    engine.start(adapter).await?;

    // 并发预计算所有文件的 SHA-256（最多 8 个并发）
    let sha256_map: HashMap<PathBuf, Option<String>> = if !no_verify {
        println!("Computing SHA-256 checksums ({} file(s))...", files.len());
        stream::iter(files.iter().map(|(path, _, _)| path.clone()))
            .map(|path| async move {
                let hash = match FileManager::compute_sha256(&path).await {
                    Ok(h) => Some(h),
                    Err(e) => {
                        tracing::warn!("Could not compute SHA-256 for {}: {}", path.display(), e);
                        None
                    }
                };
                (path, hash)
            })
            .buffer_unordered(8)
            .collect()
            .await
    } else {
        HashMap::new()
    };

    // ── MultiProgress 流水线进度显示 ──────────────────────────────────────────
    let mp = MultiProgress::new();

    // 汇总进度条（顶部）
    let summary_style = ProgressStyle::with_template(
        "Overall [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta}) @ {binary_bytes_per_sec}",
    )
    .unwrap()
    .progress_chars("=>-");
    let summary_pb = mp.add(ProgressBar::new(total_size));
    summary_pb.set_style(summary_style);

    // 每文件一条进度条
    let file_style = ProgressStyle::with_template(
        "  {spinner} {msg:<30} [{bar:30.green/white}] {bytes}/{total_bytes}",
    )
    .unwrap()
    .progress_chars("=>-");

    // task_id → ProgressBar 映射
    let mut file_bars: HashMap<Uuid, ProgressBar> = HashMap::new();

    for (path, relative_path, size) in &files {
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        // 从预计算结果中读取 SHA-256
        let sha256 = sha256_map.get(path).and_then(|h| h.clone());

        // 保留相对路径结构：dest_url/subdir/file.bin
        let rel_str = relative_path.to_string_lossy();
        let task_dest = format!("{}/{}", dest_url.trim_end_matches('/'), rel_str);

        let mut task = TransferTask::new_upload(path.clone(), task_dest, *size);
        task.sha256 = sha256;
        let task_id = task.id;

        engine.add_transfer(task).await?;

        // 为该文件创建进度条
        let pb = mp.add(ProgressBar::new(*size));
        pb.set_style(file_style.clone());
        pb.set_message(file_name);
        file_bars.insert(task_id, pb);
    }

    // 轮询 ProgressMonitor，驱动所有进度条更新
    let monitor = engine.get_progress_monitor().await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    let mut last_total_bytes: u64 = 0;

    loop {
        let done = {
            let m = monitor.read().await;
            let stats = m.get_stats();

            // 更新每文件进度条
            for tp in m.get_active_transfers() {
                if let Some(pb) = file_bars.get(&tp.task_id) {
                    pb.set_position(tp.bytes_transferred.min(tp.total_bytes));
                    if matches!(
                        tp.status,
                        aerosync_core::progress::TransferStatus::Completed
                    ) {
                        pb.finish_with_message(format!("{} ✓", pb.message()));
                    } else if matches!(
                        tp.status,
                        aerosync_core::progress::TransferStatus::Failed(_)
                    ) {
                        pb.abandon_with_message(format!("{} ✗", pb.message()));
                    }
                }
            }

            // 更新汇总条
            let delta = stats.transferred_bytes.saturating_sub(last_total_bytes);
            if delta > 0 {
                summary_pb.inc(delta);
                last_total_bytes = stats.transferred_bytes;
            }

            stats.completed_files + stats.failed_files >= stats.total_files
        };

        if done {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            eprintln!("Timeout waiting for transfers");
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    summary_pb.finish_with_message("Done");

    let m = monitor.read().await;
    let stats = m.get_stats();
    let speed_mb = stats.overall_speed / 1_048_576.0;
    println!(
        "\nCompleted: {}/{} files, Failed: {}, Avg speed: {:.2} MB/s",
        stats.completed_files, stats.total_files, stats.failed_files, speed_mb
    );

    // Print friendly error advice for failed transfers
    if stats.failed_files > 0 {
        let transfers = m.get_active_transfers();
        for t in &transfers {
            if let aerosync_core::progress::TransferStatus::Failed(ref err_msg) = t.status {
                let fake_err = aerosync_core::AeroSyncError::Network(err_msg.clone());
                if let Some(adv) = aerosync_core::error_advice::advice_for(&fake_err) {
                    eprintln!("\n  {}", adv.summary);
                    eprintln!("  Suggestions:");
                    for (i, s) in adv.suggestions.iter().enumerate() {
                        eprintln!("    {}. {}", i + 1, s);
                    }
                    break; // Only print advice once
                }
            }
        }
    }

    Ok(())
}

/// 收集要发送的文件列表，返回 (absolute_path, relative_path, size) 三元组
async fn collect_files(
    source: &PathBuf,
    recursive: bool,
) -> anyhow::Result<Vec<(PathBuf, PathBuf, u64)>> {
    let meta = tokio::fs::metadata(source).await?;
    if meta.is_file() {
        // 单文件：relative_path 就是文件名本身
        let rel = PathBuf::from(source.file_name().unwrap_or(source.as_os_str()));
        return Ok(vec![(source.clone(), rel, meta.len())]);
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
    out: &'a mut Vec<(PathBuf, PathBuf, u64)>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
    Box::pin(async move {
        let mut entries = tokio::fs::read_dir(dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let meta = entry.metadata().await?;
            if meta.is_file() {
                // 计算相对于 base 的相对路径
                let rel = path.strip_prefix(base).unwrap_or(&path).to_path_buf();
                out.push((path, rel, meta.len()));
            } else if meta.is_dir() {
                collect_files_recursive(base, &path, out).await?;
            }
        }
        Ok(())
    })
}

/// 从目标 URL 提取 HTTP/HTTPS base URL（用于 preflight probe）
/// quic://host:7789/... → http://host:7788
/// http://host:7788/... → http://host:7788
/// https://host:7790/... → https://host:7790
/// host:port → http://host:port
fn extract_http_base(dest_url: &str, original_dest: &str) -> String {
    if dest_url.starts_with("https://") {
        // 保留 https:// 前缀，去掉路径
        let trimmed = dest_url.trim_start_matches("https://");
        let host_port = trimmed.split('/').next().unwrap_or(trimmed);
        return format!("https://{}", host_port);
    }
    if dest_url.starts_with("http://") {
        // 去掉路径，只保留 scheme + host + port
        let trimmed = dest_url.trim_start_matches("http://");
        let host_port = trimmed.split('/').next().unwrap_or(trimmed);
        return format!("http://{}", host_port);
    }
    if dest_url.starts_with("quic://") {
        // quic://host:7789 → http://host:7788
        let trimmed = dest_url.trim_start_matches("quic://");
        let host_port = trimmed.split('/').next().unwrap_or(trimmed);
        if let Some(colon_pos) = host_port.rfind(':') {
            let host = &host_port[..colon_pos];
            let quic_port: u16 = host_port[colon_pos + 1..].parse().unwrap_or(7789);
            let http_port = quic_port.saturating_sub(1);
            return format!("http://{}:{}", host, http_port);
        }
        return format!("http://{}", host_port);
    }
    // 原始 host:port 格式
    format!("http://{}", original_dest)
}

/// 自动协商协议：探测对端是否为 AeroSync，若是则升级 QUIC，否则降级 HTTP
async fn negotiate_protocol(dest: &str) -> String {
    // 已有协议前缀，直接返回
    if dest.starts_with("http://")
        || dest.starts_with("https://")
        || dest.starts_with("quic://")
        || dest.starts_with("s3://")
        || dest.starts_with("ftp://")
    {
        return dest.to_string();
    }

    // host:port 格式：尝试 HTTP health 探测
    let health_url = format!("http://{}/health", dest);
    let probe = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build();

    if let Ok(client) = probe {
        if let Ok(resp) = client.get(&health_url).send().await {
            let is_aerosync = resp
                .headers()
                .get("x-aerosync")
                .and_then(|v| v.to_str().ok())
                .map(|v| v == "true")
                .unwrap_or(false);

            if is_aerosync {
                // 解析端口，QUIC 端口 = HTTP 端口 + 1（默认 7789）
                let quic_dest = if let Some(colon_pos) = dest.rfind(':') {
                    let host = &dest[..colon_pos];
                    let http_port: u16 = dest[colon_pos + 1..].parse().unwrap_or(7788);
                    let quic_port = http_port + 1;
                    format!("quic://{}:{}/upload", host, quic_port)
                } else {
                    format!("quic://{}:7789/upload", dest)
                };
                tracing::info!("AeroSync peer detected, upgrading to QUIC: {}", quic_dest);
                return quic_dest;
            }
        }
    }

    // 无法探测或非 AeroSync → 降级 HTTP
    format!("http://{}/upload", dest)
}

// ──────────────────────────── receive ───────────────────────────────────────

#[allow(clippy::too_many_arguments)]
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
    no_http: bool,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
    https: bool,
    https_port: u16,
    app_config: &AeroSyncConfig,
    config_path: Option<PathBuf>,
) -> anyhow::Result<()> {
    // 构建认证配置
    let auth_cfg = auth_token.map(|token| {
        // 直接用 token 作为 secret key，sender 用同一个 secret 生成 JWT 即可验证通过
        AuthConfig {
            enable_auth: true,
            secret_key: token,
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
        enable_http: !no_http,
        enable_quic: !http_only && !no_http,
        auth: auth_cfg,
        audit_log: None,
        tls: match (tls_cert, tls_key) {
            (Some(cert), Some(key)) => Some(TlsConfig {
                cert_path: cert,
                key_path: key,
            }),
            _ => None,
        },
        enable_metrics: app_config.metrics.enabled,
        enable_ws: app_config.ws.enabled,
        ws_event_buffer: app_config.ws.event_buffer,
        routing: app_config.routing.clone(),
        enable_https: https,
        https_port,
    };

    println!("AeroSync receiver starting...");
    if !no_http {
        println!("  HTTP:  {}:{}", bind, port);
    }
    if !http_only && !no_http {
        println!("  QUIC:  {}:{}", bind, quic_port);
    }
    if https {
        println!("  HTTPS: {}:{}", bind, https_port);
    }
    println!("  Save:  {}", save_to.display());
    if overwrite {
        println!("  Mode:  overwrite enabled");
    }
    println!("\nReady. Waiting for files... (Ctrl+C to stop)\n");

    let mut receiver = FileReceiver::new(config);
    receiver.start().await?;

    // 激活 SIGHUP 热重载（仅 Unix；配置文件路径有效时）
    if let Some(ref cfg_path) = config_path {
        let expanded = shellexpand::tilde(&cfg_path.to_string_lossy()).to_string();
        receiver.watch_config_reload(std::path::PathBuf::from(expanded));
    }

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
        TokenAction::Generate {
            secret,
            hours,
            save,
            label,
        } => {
            let secret_key = secret
                .unwrap_or_else(|| format!("{}-{}", uuid::Uuid::new_v4(), uuid::Uuid::new_v4()));

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

            if save {
                let store_path = aerosync_core::auth::TokenStore::default_path();
                let store = aerosync_core::auth::TokenStore::new(&store_path);
                let expires_at = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
                    + hours * 3600;
                store.save(&token, label.as_deref(), expires_at)?;
                println!("Saved to: {}", store_path.display());
            }

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

        TokenAction::List => {
            let store_path = aerosync_core::auth::TokenStore::default_path();
            let store = aerosync_core::auth::TokenStore::new(&store_path);
            let tokens = store.list_all()?;
            if tokens.is_empty() {
                println!("No saved tokens. Use: aerosync token generate --save");
            } else {
                println!("{} token(s):\n", tokens.len());
                for t in &tokens {
                    let status = if t.revoked {
                        "revoked"
                    } else if t.is_expired() {
                        "expired"
                    } else {
                        "valid"
                    };
                    println!(
                        "  [{}] {}... ({}{})",
                        status,
                        &t.token[..t.token.len().min(32)],
                        t.label.as_deref().unwrap_or(""),
                        if t.label.is_some() { " " } else { "" }
                    );
                }
            }
        }

        TokenAction::Revoke { token_prefix } => {
            let store_path = aerosync_core::auth::TokenStore::default_path();
            let store = aerosync_core::auth::TokenStore::new(&store_path);

            // 先按前缀查找
            if let Some(found) = store.find_by_prefix(&token_prefix)? {
                store.revoke(&found.token)?;
                println!("Revoked: {}...", &found.token[..found.token.len().min(32)]);
            } else {
                // 直接按完整 token 撤销
                if store.revoke(&token_prefix)? {
                    println!("Revoked token.");
                } else {
                    eprintln!(
                        "Token not found: {}",
                        &token_prefix[..token_prefix.len().min(16)]
                    );
                }
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

// ──────────────────────────── history ───────────────────────────────────────

async fn cmd_history(
    limit: usize,
    sent: bool,
    received: bool,
    success_only: bool,
) -> anyhow::Result<()> {
    use aerosync_core::{HistoryQuery, HistoryStore};

    let store_path = HistoryStore::default_path();
    if !store_path.exists() {
        println!("No transfer history yet.");
        return Ok(());
    }

    let store = HistoryStore::new(&store_path).await?;
    let direction = if sent {
        Some("send".to_string())
    } else if received {
        Some("receive".to_string())
    } else {
        None
    };

    let q = HistoryQuery {
        direction,
        success_only,
        limit,
        ..Default::default()
    };

    let entries = store.query(&q).await?;

    if entries.is_empty() {
        println!("No matching history records.");
        return Ok(());
    }

    println!("{} record(s):\n", entries.len());
    for e in &entries {
        let status_marker = if e.success { "✓" } else { "✗" };
        let speed_kb = if e.avg_speed_bps > 0 {
            e.avg_speed_bps as f64 / 1024.0
        } else {
            0.0
        };
        println!(
            "  {} [{:>7}] {:>6.1} KB/s  {:<30}  {} → {}",
            status_marker,
            e.protocol,
            speed_kb,
            &e.filename[..e.filename.len().min(30)],
            e.direction,
            e.remote_ip.as_deref().unwrap_or("?")
        );
        if let Some(ref err) = e.error {
            println!("      error: {}", err);
        }
    }

    Ok(())
}

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
                    let pct = if total > 0 {
                        done * 100 / total as usize
                    } else {
                        0
                    };
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

// ──────────────────────────── watch ─────────────────────────────────────────

/// 将用户输入的 host 字符串归一化为 ws:// URL。
/// 提取为独立函数以便单元测试。
fn build_ws_url(host: &str) -> String {
    if host.starts_with("ws://") || host.starts_with("wss://") {
        host.to_string()
    } else {
        format!("ws://{}/ws", host.trim_end_matches('/'))
    }
}

/// 单次连接结果：区分"正常退出"和"需要重连的错误"。
enum WatchResult {
    /// 服务端主动关闭连接（Close frame），不应重连
    ServerClosed,
    /// 网络断开或协议错误，可以重连
    Disconnected(String),
}

/// 建立一次 WebSocket 连接并持续消费消息，直到断连。
async fn watch_once(url: &str, filter: &Option<String>, format: &str) -> WatchResult {
    use futures::StreamExt;
    use tokio_tungstenite::{connect_async, tungstenite::Message};

    let (ws_stream, _) = match connect_async(url).await {
        Ok(s) => s,
        Err(e) => return WatchResult::Disconnected(e.to_string()),
    };

    let (_write, mut read) = ws_stream.split();

    while let Some(msg) = read.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                // 可选事件类型过滤
                if let Some(ref f) = filter {
                    if !text.contains(&format!("\"{}\"", f)) {
                        continue;
                    }
                }

                if format == "json" {
                    println!("{}", text);
                } else if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    print_event_pretty(&v);
                } else {
                    println!("{}", text);
                }
            }
            Ok(Message::Close(_)) => {
                return WatchResult::ServerClosed;
            }
            Err(e) => {
                return WatchResult::Disconnected(e.to_string());
            }
            _ => {}
        }
    }
    // read stream exhausted without Close frame — treat as disconnect
    WatchResult::Disconnected("stream ended".to_string())
}

async fn cmd_watch(
    host: String,
    filter: Option<String>,
    format: String,
    reconnect: bool,
    max_retries: u32,
    retry_delay: u64,
) -> anyhow::Result<()> {
    let url = build_ws_url(&host);
    let unlimited = max_retries == 0;
    let mut attempt: u32 = 0;
    // 指数退避上限 60 秒
    const MAX_DELAY_SECS: u64 = 60;

    eprintln!("Connecting to {}...", url);

    loop {
        match watch_once(&url, &filter, &format).await {
            WatchResult::ServerClosed => {
                eprintln!("Server closed connection.");
                // 服务端主动关闭，无论是否开启重连都退出
                return Ok(());
            }
            WatchResult::Disconnected(reason) => {
                if attempt == 0 && reason.contains("stream ended") {
                    // 首次且流正常结束（已接收过消息），安静退出
                    return Ok(());
                }

                if !reconnect {
                    if attempt == 0 {
                        // 连接从未成功
                        return Err(anyhow::anyhow!("Failed to connect to {}: {}", url, reason));
                    }
                    eprintln!("WebSocket error: {}", reason);
                    return Ok(());
                }

                attempt += 1;
                if !unlimited && attempt > max_retries {
                    eprintln!("Max retries ({}) reached. Giving up.", max_retries);
                    return Err(anyhow::anyhow!(
                        "Failed to connect after {} attempt(s)",
                        attempt
                    ));
                }

                // 指数退避：delay = retry_delay * 2^(attempt-1)，上限 MAX_DELAY_SECS
                let delay =
                    (retry_delay.saturating_mul(1u64 << (attempt - 1).min(5))).min(MAX_DELAY_SECS);

                eprintln!(
                    "Disconnected ({}). Reconnecting in {}s... (attempt {}/{})",
                    reason,
                    delay,
                    attempt,
                    if unlimited {
                        "∞".to_string()
                    } else {
                        max_retries.to_string()
                    }
                );
                tokio::time::sleep(Duration::from_secs(delay)).await;
                eprintln!("Reconnecting to {}...", url);
            }
        }

        // 重连成功后重置 attempt
        attempt = 0;
    }
}

/// pretty 模式下格式化输出一条 WsEvent。
///
/// 输出规则（便于 agent 脚本解析）：
/// - completed → println!（stdout）：agent 只需捕获 stdout
/// - 其余事件  → eprintln!（stderr）：人类可读的状态信息，不干扰管道
fn print_event_pretty(v: &serde_json::Value) {
    let event = v["event"].as_str().unwrap_or("unknown");
    match event {
        "transfer_started" => eprintln!(
            "[--> started ] {} ({} bytes) from {}",
            v["filename"].as_str().unwrap_or("?"),
            v["size"].as_u64().unwrap_or(0),
            v["sender_ip"].as_str().unwrap_or("?"),
        ),
        "progress" => {
            // 进度事件默认静默（避免刷屏）；只在 verbose 模式下打印
        }
        "completed" => {
            let sha = v["sha256"].as_str().unwrap_or("");
            let sha_short = &sha[..sha.len().min(8)];
            println!(
                "[completed  ] {} ({} bytes) sha256={}",
                v["filename"].as_str().unwrap_or("?"),
                v["size"].as_u64().unwrap_or(0),
                sha_short,
            );
        }
        "failed" => eprintln!(
            "[   failed  ] {} reason={}",
            v["filename"].as_str().unwrap_or("?"),
            v["reason"].as_str().unwrap_or("?"),
        ),
        _ => println!("[{}] {}", event, v),
    }
}

// ──────────────────────────── discover ──────────────────────────────────────

async fn cmd_discover(timeout_secs: u64, json: bool) -> anyhow::Result<()> {
    let timeout = Duration::from_secs(timeout_secs);

    if !json {
        eprintln!("Scanning for AeroSync receivers on local network ({timeout_secs}s)…");
    }

    let mut peers = AeroSyncMdns::discover(timeout).await;

    // 排序：按 host 字母顺序，输出稳定
    peers.sort_by(|a, b| a.host.cmp(&b.host).then(a.port.cmp(&b.port)));

    if json {
        for peer in &peers {
            let obj = serde_json::json!({
                "name":         peer.name,
                "host":         peer.host,
                "port":         peer.port,
                "addr":         peer.addr(),
                "version":      peer.version,
                "ws_enabled":   peer.ws_enabled,
                "auth_required": peer.auth_required,
            });
            println!("{}", obj);
        }
    } else if peers.is_empty() {
        println!("No AeroSync receivers found.");
        println!("Tip: make sure receiver is running with: aerosync receive");
    } else {
        println!("\nFound {} receiver(s):\n", peers.len());
        println!(
            "{:<20} {:<22} {:<10} {:<6} {:<6}",
            "NAME", "ADDRESS", "VERSION", "WS", "AUTH"
        );
        println!("{}", "-".repeat(68));
        for peer in &peers {
            println!(
                "{:<20} {:<22} {:<10} {:<6} {:<6}",
                peer.name,
                peer.addr(),
                peer.version.as_deref().unwrap_or("-"),
                if peer.ws_enabled { "yes" } else { "no" },
                if peer.auth_required { "yes" } else { "no" },
            );
        }
        println!();
        println!("Use: aerosync send <file> <ADDRESS> [--token <TOKEN>]");
    }

    Ok(())
}

#[cfg(test)]
mod watch_tests {
    use super::*;

    #[test]
    fn test_build_ws_url_bare_host_port() {
        assert_eq!(build_ws_url("localhost:7788"), "ws://localhost:7788/ws");
    }

    #[test]
    fn test_build_ws_url_already_ws_scheme() {
        assert_eq!(build_ws_url("ws://myhost:9000/ws"), "ws://myhost:9000/ws");
    }

    #[test]
    fn test_build_ws_url_wss_scheme() {
        assert_eq!(build_ws_url("wss://myhost:443/ws"), "wss://myhost:443/ws");
    }

    #[test]
    fn test_build_ws_url_trailing_slash_stripped() {
        assert_eq!(
            build_ws_url("192.168.1.10:7788/"),
            "ws://192.168.1.10:7788/ws"
        );
    }

    /// 验证指数退避延迟计算：delay = retry_delay * 2^(attempt-1)，上限 60s
    #[test]
    fn test_retry_backoff_delay() {
        let base: u64 = 2;
        const MAX: u64 = 60;

        let delays: Vec<u64> = (1u32..=8)
            .map(|attempt| (base.saturating_mul(1u64 << (attempt - 1).min(5))).min(MAX))
            .collect();

        // attempt 1 → 2s, 2 → 4s, 3 → 8s, 4 → 16s, 5 → 32s, 6+ → 60s (capped)
        assert_eq!(delays[0], 2);
        assert_eq!(delays[1], 4);
        assert_eq!(delays[2], 8);
        assert_eq!(delays[3], 16);
        assert_eq!(delays[4], 32);
        assert_eq!(delays[5], 60); // capped at MAX_DELAY_SECS
        assert_eq!(delays[6], 60);
        assert_eq!(delays[7], 60);
    }

    /// 验证 max_retries=0 时表示无限重连
    #[test]
    fn test_unlimited_retries_flag() {
        let max_retries: u32 = 0;
        assert!(max_retries == 0, "0 should mean unlimited");
    }
}
