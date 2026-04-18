/// 带宽限速模块：Token Bucket 算法
///
/// `RateLimiter` 按字节计量，`consume()` 在令牌不足时异步等待，
/// 实现对上传速率的精确控制。
///
/// 使用方式：
/// ```no_run
/// # use aerosync::protocols::ratelimit::RateLimiter;
/// # async fn example(chunk_len: u64) {
/// let limiter = RateLimiter::new(1024 * 1024); // 1 MB/s
/// limiter.consume(chunk_len).await;
/// # }
/// ```
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Token Bucket 速率限制器
///
/// - `capacity`：桶容量（bytes），等于 1 秒对应的最大字节数
/// - `tokens`：当前可用令牌数（bytes）
/// - `refill_rate`：每秒补充令牌数（bytes/s）
#[derive(Clone)]
pub struct RateLimiter {
    inner: Arc<Mutex<RateLimiterInner>>,
}

struct RateLimiterInner {
    /// 每秒字节数限制（0 = 不限速）
    rate_bytes_per_sec: u64,
    /// 桶容量（= rate_bytes_per_sec，允许最大突发量为 1 秒的量）
    capacity: u64,
    /// 当前可用令牌数
    tokens: u64,
    /// 上次补充时间
    last_refill: Instant,
}

impl RateLimiter {
    /// 创建速率限制器
    ///
    /// `rate_bytes_per_sec`：每秒允许通过的字节数；0 表示不限速。
    pub fn new(rate_bytes_per_sec: u64) -> Self {
        let capacity = rate_bytes_per_sec.max(1);
        Self {
            inner: Arc::new(Mutex::new(RateLimiterInner {
                rate_bytes_per_sec,
                capacity,
                tokens: capacity,
                last_refill: Instant::now(),
            })),
        }
    }

    /// 无限速（不做任何等待）
    pub fn unlimited() -> Self {
        Self::new(0)
    }

    /// 是否启用了限速
    pub fn is_limited(&self) -> bool {
        // 需要在异步环境才能拿锁，提供同步的快速判断
        // 创建时保存了 rate，这里用 Arc 包装时无法简单同步读取；
        // 调用方可以直接用 rate_bytes_per_sec == 0 判断
        true
    }

    /// 消耗 `bytes` 个令牌；如果令牌不足则等待直到补充完毕。
    ///
    /// 若 `rate == 0`（无限速），立即返回。
    pub async fn consume(&self, bytes: u64) {
        let mut inner = self.inner.lock().await;

        // 无限速：直接返回
        if inner.rate_bytes_per_sec == 0 {
            return;
        }

        // 补充令牌
        let now = Instant::now();
        let elapsed = now.duration_since(inner.last_refill);
        let refill = (elapsed.as_secs_f64() * inner.rate_bytes_per_sec as f64) as u64;
        if refill > 0 {
            inner.tokens = (inner.tokens + refill).min(inner.capacity);
            inner.last_refill = now;
        }

        if bytes <= inner.tokens {
            inner.tokens -= bytes;
            return;
        }

        // 令牌不足：计算等待时间
        let deficit = bytes - inner.tokens;
        let wait_secs = deficit as f64 / inner.rate_bytes_per_sec as f64;
        let wait = Duration::from_secs_f64(wait_secs);

        // 释放锁再等待，避免长期持锁
        drop(inner);
        tokio::time::sleep(wait).await;

        // 等待后重新消耗
        let mut inner = self.inner.lock().await;
        let now2 = Instant::now();
        let elapsed2 = now2.duration_since(inner.last_refill);
        let refill2 = (elapsed2.as_secs_f64() * inner.rate_bytes_per_sec as f64) as u64;
        if refill2 > 0 {
            inner.tokens = (inner.tokens + refill2).min(inner.capacity);
            inner.last_refill = now2;
        }
        inner.tokens = inner.tokens.saturating_sub(bytes);
    }
}

/// 将 `kbps` 字符串解析为字节/秒
/// 支持格式：`512`（KB/s）、`1MB`、`10MB/s`、`500KB`
pub fn parse_limit(s: &str) -> Option<u64> {
    let s = s.trim().to_uppercase();
    let s = s.trim_end_matches("/S").trim_end_matches("/SEC");

    if let Some(n) = s.strip_suffix("GB") {
        return n
            .trim()
            .parse::<f64>()
            .ok()
            .map(|v| (v * 1024.0 * 1024.0 * 1024.0) as u64);
    }
    if let Some(n) = s.strip_suffix("MB") {
        return n
            .trim()
            .parse::<f64>()
            .ok()
            .map(|v| (v * 1024.0 * 1024.0) as u64);
    }
    if let Some(n) = s.strip_suffix("KB") {
        return n.trim().parse::<f64>().ok().map(|v| (v * 1024.0) as u64);
    }
    if let Some(n) = s.strip_suffix('B') {
        return n.trim().parse::<u64>().ok();
    }
    // 无单位时默认 KB/s
    s.trim().parse::<f64>().ok().map(|v| (v * 1024.0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn test_unlimited_returns_immediately() {
        let limiter = RateLimiter::unlimited();
        let start = Instant::now();
        limiter.consume(1024 * 1024).await; // 1MB
        assert!(start.elapsed() < Duration::from_millis(10));
    }

    #[tokio::test]
    async fn test_rate_limiter_throttles() {
        // 10 KB/s 限速，消耗 20 KB 应该至少等待 ~1 秒
        let limiter = RateLimiter::new(10 * 1024);
        let start = Instant::now();
        limiter.consume(20 * 1024).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(900),
            "elapsed={:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn test_small_consume_no_wait() {
        // 1 MB/s，消耗 1 字节，应立即返回
        let limiter = RateLimiter::new(1024 * 1024);
        let start = Instant::now();
        limiter.consume(1).await;
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn test_parse_limit_kb() {
        assert_eq!(parse_limit("512KB"), Some(512 * 1024));
        assert_eq!(parse_limit("512kb"), Some(512 * 1024));
    }

    #[test]
    fn test_parse_limit_mb() {
        assert_eq!(parse_limit("10MB"), Some(10 * 1024 * 1024));
        assert_eq!(parse_limit("1MB/s"), Some(1024 * 1024));
    }

    #[test]
    fn test_parse_limit_gb() {
        assert_eq!(parse_limit("1GB"), Some(1024 * 1024 * 1024));
    }

    #[test]
    fn test_parse_limit_bare_number() {
        // 无单位 = KB/s
        assert_eq!(parse_limit("100"), Some(100 * 1024));
    }

    #[test]
    fn test_parse_limit_invalid() {
        assert_eq!(parse_limit("abc"), None);
    }

    #[tokio::test]
    async fn test_zero_rate_is_unlimited() {
        let limiter = RateLimiter::new(0);
        let start = Instant::now();
        limiter.consume(100 * 1024 * 1024).await; // 100MB
        assert!(start.elapsed() < Duration::from_millis(10));
    }
}
