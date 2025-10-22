use axum::{
    body::Body,
    extract::State as AxumState,
    http::{Request, Response, StatusCode, Uri},
    Router,
};
use http_body_util::BodyExt;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use hyper_rustls::HttpsConnectorBuilder;
use std::sync::{Arc, OnceLock};
use tokio::task::JoinHandle;
use tower::ServiceBuilder;

use crate::app_config::AppType;
use crate::provider::Provider;
use crate::settings::OperationMode;
use crate::store::AppState;

/// 代理服务器状态
pub struct ProxyServer {
    handle: JoinHandle<()>,
}

/// 全局代理服务器实例
static PROXY_SERVER: OnceLock<tokio::sync::RwLock<Option<ProxyServer>>> = OnceLock::new();

/// 初始化代理服务器全局实例
pub fn init_proxy_server() {
    PROXY_SERVER.get_or_init(|| tokio::sync::RwLock::new(None));

    // 初始化 Rustls 加密提供程序
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// 代理服务器共享状态
#[derive(Clone)]
struct ProxyState {
    app_state: Arc<AppState>,
}

/// Hop-by-hop headers 不应该被转发
const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
    "host", // 不转发 Host 头，让客户端自动设置为目标域名
];


/// 根据 User-Agent 判断应用类型
/// 如果 User-Agent 包含 "claude"（忽略大小写），则为 Claude
/// 否则为 Codex
fn detect_app_type_from_user_agent(user_agent: Option<&str>) -> AppType {
    if let Some(ua) = user_agent {
        if ua.to_lowercase().contains("claude") {
            return AppType::Claude;
        }
    }
    AppType::Codex
}

/// 从供应商配置中提取API Key和Base URL
fn extract_provider_credentials(
    provider: &Provider,
    app_type: &AppType,
) -> Result<(String, String), String> {
    match app_type {
        AppType::Claude => {
            let env = provider
                .settings_config
                .get("env")
                .and_then(|v| v.as_object())
                .ok_or("配置格式错误: 缺少 env")?;

            let api_key = env
                .get("ANTHROPIC_AUTH_TOKEN")
                .and_then(|v| v.as_str())
                .ok_or("缺少 ANTHROPIC_AUTH_TOKEN")?
                .to_string();

            let base_url = env
                .get("ANTHROPIC_BASE_URL")
                .and_then(|v| v.as_str())
                .ok_or("缺少 ANTHROPIC_BASE_URL")?
                .to_string();

            Ok((api_key, base_url))
        }
        AppType::Codex => {
            let auth = provider
                .settings_config
                .get("auth")
                .and_then(|v| v.as_object())
                .ok_or("配置格式错误: 缺少 auth")?;

            let api_key = auth
                .get("OPENAI_API_KEY")
                .and_then(|v| v.as_str())
                .ok_or("缺少 OPENAI_API_KEY")?
                .to_string();

            // 从 config TOML 中提取 base_url
            let config_toml = provider
                .settings_config
                .get("config")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            let base_url = if config_toml.contains("base_url") {
                let re = regex::Regex::new(r#"base_url\s*=\s*["']([^"']+)["']"#).unwrap();
                re.captures(config_toml)
                    .and_then(|caps| caps.get(1))
                    .map(|m| m.as_str().to_string())
                    .ok_or("config.toml 中 base_url 格式错误")?
            } else {
                return Err("config.toml 中缺少 base_url 配置".to_string());
            };

            Ok((api_key, base_url))
        }
    }
}

/// 获取启用代理的供应商列表（按排序顺序）
async fn get_enabled_proxy_providers(
    app_state: &AppState,
    app_type: &AppType,
) -> Result<Vec<Provider>, String> {
    let config = app_state
        .config
        .lock()
        .map_err(|e| format!("获取锁失败: {}", e))?;

    let manager = config
        .get_manager(app_type)
        .ok_or_else(|| format!("应用类型不存在: {:?}", app_type))?;

    let mut providers: Vec<Provider> = manager
        .providers
        .values()
        .filter(|p| p.proxy_enabled.unwrap_or(false))
        .cloned()
        .collect();

    // 按 sort_index 排序
    providers.sort_by(|a, b| {
        match (a.sort_index, b.sort_index) {
            (Some(idx_a), Some(idx_b)) => idx_a.cmp(&idx_b),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => {
                // 按创建时间排序
                match (a.created_at, b.created_at) {
                    (Some(time_a), Some(time_b)) => time_a.cmp(&time_b),
                    _ => std::cmp::Ordering::Equal,
                }
            }
        }
    });

    Ok(providers)
}

/// 过滤 hop-by-hop headers
fn should_forward_header(header_name: &str) -> bool {
    !HOP_BY_HOP_HEADERS.contains(&header_name.to_lowercase().as_str())
}

/// 代理请求处理器
async fn proxy_handler(
    AxumState(state): AxumState<Arc<ProxyState>>,
    uri: Uri,
    req: Request<Body>,
) -> Result<Response<Body>, StatusCode> {
    // 获取请求路径（包含查询参数）
    let request_path = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");

    // 读取请求头和请求体
    let (parts, body) = req.into_parts();

    // 从 User-Agent 判断应用类型
    let user_agent = parts.headers.get("user-agent").and_then(|v| v.to_str().ok());
    let app_type = detect_app_type_from_user_agent(user_agent);

    // 读取请求体
    let body_bytes = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            log::error!("读取请求体失败: {}", e);
            return Err(StatusCode::BAD_REQUEST);
        }
    };

    // 读取设置获取重试次数
    let settings = crate::settings::get_settings();
    let retry_count = settings.proxy_retry_count;

    // 获取对应应用类型的启用代理供应商
    let providers = match get_enabled_proxy_providers(&state.app_state, &app_type).await {
        Ok(p) => p,
        Err(e) => {
            log::error!("获取启用代理的供应商失败: {}", e);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    if providers.is_empty() {
        log::error!("没有启用代理的 {:?} 供应商", app_type);
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }

    // 创建支持 HTTP 和 HTTPS 的客户端（.https_or_http() 会自动根据 URL 协议选择）
    let https_connector = HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()  // 同时支持 http:// 和 https://
        .enable_http1()
        .enable_http2()
        .build();
    let client = Client::builder(TokioExecutor::new()).build(https_connector);

    // 遍历供应商并尝试请求
    for provider in providers.iter() {

        // 提取凭证
        let (api_key, base_url) = match extract_provider_credentials(provider, &app_type) {
            Ok(creds) => creds,
            Err(e) => {
                log::warn!("供应商 {} 凭证提取失败: {}", provider.name, e);
                continue;
            }
        };

        // 构建目标URL
        let target_url = format!("{}{}", base_url.trim_end_matches('/'), request_path);

        // 重试逻辑
        for retry in 0..=retry_count {

            // 构建新请求
            let mut new_req = match Request::builder()
                .method(parts.method.clone())
                .uri(&target_url)
                .body(Body::from(body_bytes.clone()))
            {
                Ok(req) => req,
                Err(e) => {
                    log::error!("构建请求失败: {}", e);
                    break; // 跳出重试，尝试下一个供应商
                }
            };

            // 复制 headers，过滤 hop-by-hop headers，并重写 Authorization
            for (name, value) in parts.headers.iter() {
                if should_forward_header(name.as_str()) && name.as_str().to_lowercase() != "authorization" {
                    new_req.headers_mut().insert(name.clone(), value.clone());
                }
            }

            // 重写 Authorization header
            new_req.headers_mut().insert(
                "authorization",
                format!("Bearer {}", api_key).parse().unwrap(),
            );

            // 发送请求
            match client.request(new_req).await {
                Ok(response) => {
                    let status = response.status();

                    // 只有 200 才算成功，其他状态都重试
                    if status == StatusCode::OK {
                        // 转换响应体
                        let (resp_parts, incoming_body) = response.into_parts();
                        let body_bytes = match incoming_body.collect().await {
                            Ok(collected) => collected.to_bytes(),
                            Err(e) => {
                                log::error!("读取响应体失败: {}", e);
                                return Err(StatusCode::BAD_GATEWAY);
                            }
                        };

                        // 构建响应，过滤 hop-by-hop headers
                        let mut final_response = Response::new(Body::from(body_bytes));
                        *final_response.status_mut() = resp_parts.status;

                        for (name, value) in resp_parts.headers.iter() {
                            if should_forward_header(name.as_str()) {
                                final_response.headers_mut().insert(name.clone(), value.clone());
                            }
                        }

                        return Ok(final_response);
                    } else {
                        log::warn!(
                            "供应商 {} -> {}: {}",
                            provider.name,
                            request_path,
                            status
                        );
                    }
                }
                Err(e) => {
                    log::warn!("供应商 {} -> {}: {}", provider.name, request_path, e);
                }
            }

            // 重试前等待一小段时间
            if retry < retry_count {
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            }
        }
    }

    // log::error!("所有代理供应商都失败");
    Err(StatusCode::INTERNAL_SERVER_ERROR)
}

/// 启动代理服务器
pub async fn start_proxy_server(app_state: &AppState) -> Result<(), String> {
    let settings = crate::settings::get_settings();
    if settings.operation_mode != OperationMode::Proxy {
        return Ok(());
    }

    let mut server_guard = PROXY_SERVER
        .get()
        .ok_or("代理服务器未初始化")?
        .write()
        .await;

    if server_guard.is_some() {
        return Ok(()); // 已经在运行
    }

    let proxy_state = Arc::new(ProxyState {
        app_state: Arc::new(app_state.clone()),
    });

    let app = Router::new()
        .fallback(proxy_handler)
        .with_state(proxy_state)
        .layer(ServiceBuilder::new());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:12857")
        .await
        .map_err(|e| format!("绑定代理端口失败: {}", e))?;

    log::info!("代理服务器启动在 http://127.0.0.1:12857");

    let handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            log::error!("代理服务器运行错误: {}", e);
        }
    });

    *server_guard = Some(ProxyServer { handle });

    Ok(())
}

/// 停止代理服务器
pub async fn stop_proxy_server() -> Result<(), String> {
    let mut server_guard = PROXY_SERVER
        .get()
        .ok_or("代理服务器未初始化")?
        .write()
        .await;

    if let Some(server) = server_guard.take() {
        server.handle.abort();
        log::info!("代理服务器已停止");
    }

    Ok(())
}

/// 在代理模式下写入配置文件
/// 将供应商的ANTHROPIC_BASE_URL或base_url替换为代理地址，
/// 并使用固定token
pub fn write_proxy_mode_config(
    app_type: &AppType,
    _app_state: &AppState,
    common_config: Option<&str>,
) -> Result<(), String> {
    const PROXY_URL: &str = "http://127.0.0.1:12857";
    const PROXY_TOKEN: &str = "ccswitch-proxymode-token";

    match app_type {
        AppType::Claude => {
            let settings_path = crate::config::get_claude_settings_path();

            // 创建代理模式配置
            let mut proxy_config = serde_json::json!({
                "env": {
                    "ANTHROPIC_AUTH_TOKEN": PROXY_TOKEN,
                    "ANTHROPIC_BASE_URL": PROXY_URL,
                }
            });

            // 如果有 Claude 通用配置，合并进去
            if let Some(common) = common_config {
                if let Ok(common_obj) = serde_json::from_str::<serde_json::Value>(common) {
                    if let Some(common_map) = common_obj.as_object() {
                        if let Some(proxy_map) = proxy_config.as_object_mut() {
                            for (key, value) in common_map {
                                if key == "env" {
                                    // 特殊处理 env 字段：合并但忽略特定的键
                                    if let Some(common_env) = value.as_object() {
                                        if let Some(proxy_env) = proxy_map.get_mut("env").and_then(|v| v.as_object_mut()) {
                                            for (env_key, env_value) in common_env {
                                                // 忽略 ANTHROPIC_AUTH_TOKEN 和 ANTHROPIC_BASE_URL
                                                if env_key != "ANTHROPIC_AUTH_TOKEN" && env_key != "ANTHROPIC_BASE_URL" {
                                                    proxy_env.insert(env_key.clone(), env_value.clone());
                                                }
                                            }
                                        }
                                    }
                                } else {
                                    // 其他字段直接合并
                                    proxy_map.insert(key.clone(), value.clone());
                                }
                            }
                        }
                    }
                }
            }

            crate::config::write_json_file(&settings_path, &proxy_config)?;
        }
        AppType::Codex => {
            // 创建auth.json代理配置
            let auth_config = serde_json::json!({
                "OPENAI_API_KEY": PROXY_TOKEN
            });

            let auth_path = crate::codex_config::get_codex_auth_path();
            crate::config::write_json_file(&auth_path, &auth_config)?;

            // 使用传入的通用配置，如果没有则为空
            let common_config_text = common_config.unwrap_or("");

            // 构建代理模式配置：固定模板 + 传入的通用配置
            let proxy_config = format!(
                r#"model_provider = "ccswitch"

[model_providers.ccswitch]
base_url = "{}"
name = "ccswitch"
requires_openai_auth = true
wire_api = "responses"
{}"#,
                PROXY_URL,
                if common_config_text.is_empty() {
                    String::new()
                } else {
                    format!("\n{}", common_config_text)
                }
            );

            let config_path = crate::codex_config::get_codex_config_path();
            std::fs::write(&config_path, proxy_config)
                .map_err(|e| format!("写入 Codex config.toml 失败: {}", e))?;
        }
    }

    Ok(())
}

/// 切换到代理模式时的配置更新
pub fn switch_to_proxy_mode(
    app_state: &AppState,
    claude_common_config: Option<&str>,
    codex_common_config: Option<&str>,
) -> Result<(), String> {
    write_proxy_mode_config(&AppType::Claude, app_state, claude_common_config)?;
    write_proxy_mode_config(&AppType::Codex, app_state, codex_common_config)?;
    Ok(())
}

/// 切换到写入模式时的配置恢复
pub fn switch_to_write_mode(app_state: &AppState) -> Result<(), String> {
    // 恢复Claude配置
    {
        let mut config = app_state
            .config
            .lock()
            .map_err(|e| format!("获取锁失败: {}", e))?;

        if let Some(manager) = config.get_manager_mut(&AppType::Claude) {
            if !manager.current.is_empty() {
                // 有当前供应商，写入其配置
                if let Some(provider) = manager.providers.get(&manager.current) {
                    let settings_path = crate::config::get_claude_settings_path();
                    crate::config::write_json_file(&settings_path, &provider.settings_config)?;
                }
            } else {
                // 没有当前供应商，选择第一个供应商（按 sort_index 排序）
                let mut providers: Vec<_> = manager.providers.iter().collect();
                providers.sort_by(|a, b| {
                    match (a.1.sort_index, b.1.sort_index) {
                        (Some(idx_a), Some(idx_b)) => idx_a.cmp(&idx_b),
                        (Some(_), None) => std::cmp::Ordering::Less,
                        (None, Some(_)) => std::cmp::Ordering::Greater,
                        (None, None) => {
                            match (a.1.created_at, b.1.created_at) {
                                (Some(time_a), Some(time_b)) => time_a.cmp(&time_b),
                                _ => std::cmp::Ordering::Equal,
                            }
                        }
                    }
                });

                if let Some((provider_id, first_provider)) = providers.first() {
                    let settings_path = crate::config::get_claude_settings_path();
                    crate::config::write_json_file(&settings_path, &first_provider.settings_config)?;
                    // 更新 current 字段
                    manager.current = provider_id.to_string();
                } else {
                    log::warn!("没有可用的 Claude 供应商，跳过配置恢复");
                }
            }
        }

        // 恢复Codex配置
        if let Some(manager) = config.get_manager_mut(&AppType::Codex) {
            if !manager.current.is_empty() {
                // 有当前供应商，写入其配置
                if let Some(provider) = manager.providers.get(&manager.current) {
                    let auth = provider
                        .settings_config
                        .get("auth")
                        .ok_or_else(|| "目标供应商缺少 auth 配置".to_string())?;
                    let cfg_text = provider
                        .settings_config
                        .get("config")
                        .and_then(|v| v.as_str());
                    crate::codex_config::write_codex_live_atomic(auth, cfg_text)?;
                }
            } else {
                // 没有当前供应商，选择第一个供应商（按 sort_index 排序）
                let mut providers: Vec<_> = manager.providers.iter().collect();
                providers.sort_by(|a, b| {
                    match (a.1.sort_index, b.1.sort_index) {
                        (Some(idx_a), Some(idx_b)) => idx_a.cmp(&idx_b),
                        (Some(_), None) => std::cmp::Ordering::Less,
                        (None, Some(_)) => std::cmp::Ordering::Greater,
                        (None, None) => {
                            match (a.1.created_at, b.1.created_at) {
                                (Some(time_a), Some(time_b)) => time_a.cmp(&time_b),
                                _ => std::cmp::Ordering::Equal,
                            }
                        }
                    }
                });

                if let Some((provider_id, first_provider)) = providers.first() {
                    let auth = first_provider
                        .settings_config
                        .get("auth")
                        .ok_or_else(|| "目标供应商缺少 auth 配置".to_string())?;
                    let cfg_text = first_provider
                        .settings_config
                        .get("config")
                        .and_then(|v| v.as_str());
                    crate::codex_config::write_codex_live_atomic(auth, cfg_text)?;
                    // 更新 current 字段
                    manager.current = provider_id.to_string();
                } else {
                    log::warn!("没有可用的 Codex 供应商，跳过配置恢复");
                }
            }
        }

        // 保存配置
        drop(config);
        app_state.save()?;
    }

    Ok(())
}
