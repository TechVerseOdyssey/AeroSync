//! 认证中间件
//!
//! 为 HTTP 和 QUIC 协议提供认证中间件。

use super::AuthManager;
use crate::error::Result;
use std::sync::Arc;

/// 认证中间件
///
/// 用于在 HTTP/QUIC 请求处理中插入认证逻辑。
pub struct AuthMiddleware {
    auth_manager: Arc<AuthManager>,
}

impl AuthMiddleware {
    /// 创建新的认证中间件
    pub fn new(auth_manager: Arc<AuthManager>) -> Self {
        Self { auth_manager }
    }

    /// 从 HTTP 请求头中提取 Token
    ///
    /// 支持以下格式：
    /// - `Authorization: Bearer <token>`
    /// - `X-Auth-Token: <token>`
    pub fn extract_token_from_header(&self, auth_header: Option<&str>) -> Option<String> {
        if let Some(header) = auth_header {
            // Bearer Token 格式
            if let Some(stripped) = header.strip_prefix("Bearer ") {
                return Some(stripped.to_string());
            }
            // 直接 Token
            return Some(header.to_string());
        }
        None
    }

    /// 验证 HTTP 请求
    ///
    /// # 参数
    /// - `auth_header`: Authorization header 的值
    /// - `client_ip`: 客户端 IP 地址
    ///
    /// # 返回
    /// - `Ok(true)`: 认证通过
    /// - `Ok(false)`: 认证失败
    /// - `Err(e)`: 发生错误
    pub fn authenticate_http_request(
        &self,
        auth_header: Option<&str>,
        client_ip: &str,
    ) -> Result<bool> {
        let token = self.extract_token_from_header(auth_header);
        self.auth_manager.authenticate(token.as_deref(), client_ip)
    }

    /// 验证 QUIC 连接
    ///
    /// # 参数
    /// - `connection_data`: QUIC 连接携带的自定义数据
    /// - `client_ip`: 客户端 IP 地址
    ///
    /// # 返回
    /// - `Ok(true)`: 认证通过
    /// - `Ok(false)`: 认证失败
    /// - `Err(e)`: 发生错误
    pub fn authenticate_quic_connection(
        &self,
        connection_data: Option<&[u8]>,
        client_ip: &str,
    ) -> Result<bool> {
        // 从 QUIC 连接数据中提取 Token
        let token = connection_data.and_then(|data| String::from_utf8(data.to_vec()).ok());

        self.auth_manager.authenticate(token.as_deref(), client_ip)
    }

    /// 生成 HTTP 401 Unauthorized 响应
    pub fn unauthorized_response(&self) -> UnauthorizedResponse {
        UnauthorizedResponse {
            status_code: 401,
            message: "Unauthorized: Invalid or missing authentication token".to_string(),
            headers: vec![("WWW-Authenticate".to_string(), "Bearer".to_string())],
        }
    }
}

/// HTTP 401 未授权响应
#[derive(Debug, Clone)]
pub struct UnauthorizedResponse {
    pub status_code: u16,
    pub message: String,
    pub headers: Vec<(String, String)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthConfig, AuthManager};

    fn create_test_middleware() -> (AuthMiddleware, AuthManager) {
        let config = AuthConfig::new()
            .enable_auth()
            .with_secret_key("test-secret-key-12345".to_string());

        let auth_manager = AuthManager::new(config).unwrap();
        let middleware = AuthMiddleware::new(Arc::new(auth_manager.clone()));

        (middleware, auth_manager)
    }

    #[test]
    fn test_extract_bearer_token() {
        let (middleware, _) = create_test_middleware();

        let token = middleware.extract_token_from_header(Some("Bearer abc123"));
        assert_eq!(token, Some("abc123".to_string()));
    }

    #[test]
    fn test_extract_direct_token() {
        let (middleware, _) = create_test_middleware();

        let token = middleware.extract_token_from_header(Some("xyz789"));
        assert_eq!(token, Some("xyz789".to_string()));
    }

    #[test]
    fn test_authenticate_with_valid_token() {
        let (middleware, auth_manager) = create_test_middleware();

        // 生成有效 Token
        let token = auth_manager.generate_token().unwrap();
        let auth_header = format!("Bearer {}", token);

        // 应该认证通过
        let result = middleware.authenticate_http_request(Some(&auth_header), "127.0.0.1");
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[test]
    fn test_authenticate_with_invalid_token() {
        let (middleware, _) = create_test_middleware();

        // 使用无效 Token
        let result =
            middleware.authenticate_http_request(Some("Bearer invalid-token"), "127.0.0.1");

        assert!(result.is_ok());
        assert!(!result.unwrap());
    }

    #[test]
    fn test_authenticate_without_token() {
        let (middleware, _) = create_test_middleware();

        // 没有提供 Token
        let result = middleware.authenticate_http_request(None, "127.0.0.1");

        assert!(result.is_ok());
        assert!(!result.unwrap());
    }

    #[test]
    fn test_unauthorized_response() {
        let (middleware, _) = create_test_middleware();

        let response = middleware.unauthorized_response();
        assert_eq!(response.status_code, 401);
        assert!(response.message.contains("Unauthorized"));
    }
}
