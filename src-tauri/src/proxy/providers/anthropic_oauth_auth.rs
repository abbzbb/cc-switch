//! Anthropic / Claude Pro-Max OAuth account store.
//!
//! Supports importing the current Claude CLI login
//! (`~/.claude/.credentials.json` / macOS keychain), refreshing via
//! `api.anthropic.com/v1/oauth/token`, and browser PKCE on
//! `http://localhost:54545/callback`.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, RwLock};
use url::Url;

use super::copilot_auth::GitHubDeviceCodeResponse;
use super::kimi_oauth_auth::write_oauth_store_atomic;

const ANTHROPIC_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const ANTHROPIC_TOKEN_URL: &str = "https://api.anthropic.com/v1/oauth/token";
const ANTHROPIC_AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
#[cfg_attr(test, allow(dead_code))]
const ANTHROPIC_REDIRECT_URI: &str = "http://localhost:54545/callback";
const ANTHROPIC_OAUTH_SCOPE: &str = "org:create_api_key user:profile user:inference";
#[cfg_attr(test, allow(dead_code))]
const PKCE_CALLBACK_PORT: u16 = 54545;
const PKCE_LOGIN_LIFETIME_SECS: u64 = 600;
const TOKEN_REFRESH_BUFFER_MS: i64 = 5 * 60_000;
const DEFAULT_TOKEN_LIFETIME_SECS: i64 = 3_600;
const MAX_OAUTH_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum AnthropicOAuthError {
    #[error("Refresh Token 失效或已过期，请重新登录")]
    RefreshTokenInvalid,
    #[error("账号需要重新登录: {0}")]
    ReauthRequired(String),
    #[error("等待浏览器授权中")]
    AuthorizationPending,
    #[error("OAuth 登录已过期，请重试")]
    LoginExpired,
    #[error("OAuth 回调失败: {0}")]
    CallbackFailed(String),
    #[error("无法绑定本机 OAuth 回调端口: {0}")]
    CallbackBindFailed(String),
    #[error("OAuth Token 获取失败: {0}")]
    TokenFetchFailed(String),
    #[error("网络错误: {0}")]
    NetworkError(String),
    #[error("解析错误: {0}")]
    ParseError(String),
    #[error("IO 错误: {0}")]
    IoError(String),
    #[error("账号不存在: {0}")]
    AccountNotFound(String),
    #[error("未找到可导入的 Claude CLI 凭据")]
    CredentialsNotFound,
}

impl From<reqwest::Error> for AnthropicOAuthError {
    fn from(err: reqwest::Error) -> Self {
        Self::NetworkError(err.to_string())
    }
}

impl From<std::io::Error> for AnthropicOAuthError {
    fn from(err: std::io::Error) -> Self {
        Self::IoError(err.to_string())
    }
}

impl From<AnthropicOAuthError> for crate::error::AppError {
    fn from(err: AnthropicOAuthError) -> Self {
        crate::error::AppError::Message(err.to_string())
    }
}

#[derive(Debug, Clone, Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct AnthropicTokenClaims {
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    email: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ClaudeCliOauthEntry {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    #[serde(rename = "refreshToken")]
    refresh_token: Option<String>,
    #[serde(rename = "expiresAt")]
    expires_at: Option<serde_json::Value>,
    #[serde(rename = "accountUuid")]
    account_uuid: Option<String>,
    #[serde(default)]
    email: Option<String>,
}

#[derive(Debug, Clone)]
struct CachedAccessToken {
    token: String,
    expires_at_ms: i64,
}

impl CachedAccessToken {
    fn is_expiring_soon(&self) -> bool {
        self.expires_at_ms - chrono::Utc::now().timestamp_millis() < TOKEN_REFRESH_BUFFER_MS
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnthropicAccountData {
    account_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    login: Option<String>,
    refresh_token: String,
    authenticated_at: i64,
    #[serde(default)]
    requires_reauth: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicOAuthAccount {
    pub id: String,
    pub login: String,
    pub avatar_url: Option<String>,
    pub authenticated_at: i64,
    pub github_domain: String,
    pub requires_reauth: bool,
}

impl From<&AnthropicAccountData> for AnthropicOAuthAccount {
    fn from(data: &AnthropicAccountData) -> Self {
        let short_id: String = data.account_id.chars().take(12).collect();
        Self {
            id: data.account_id.clone(),
            login: data
                .login
                .clone()
                .unwrap_or_else(|| format!("Claude ({short_id})")),
            avatar_url: None,
            authenticated_at: data.authenticated_at,
            github_domain: "anthropic.com".to_string(),
            requires_reauth: data.requires_reauth,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AnthropicOAuthStore {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    accounts: HashMap<String, AnthropicAccountData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default_account_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicOAuthStatus {
    pub accounts: Vec<AnthropicOAuthAccount>,
    pub default_account_id: Option<String>,
    pub authenticated: bool,
    pub username: Option<String>,
}

#[derive(Debug, Clone)]
struct PendingPkce {
    id: String,
    verifier: String,
    expected_state: String,
    redirect_uri: String,
    expires_at_ms: i64,
    authorization_code: Option<String>,
    error: Option<String>,
}

pub struct AnthropicOAuthManager {
    accounts: Arc<RwLock<HashMap<String, AnthropicAccountData>>>,
    default_account_id: Arc<RwLock<Option<String>>>,
    access_tokens: Arc<RwLock<HashMap<String, CachedAccessToken>>>,
    refresh_locks: Arc<RwLock<HashMap<String, Arc<Mutex<()>>>>>,
    mutation_lock: Arc<Mutex<()>>,
    pending_pkce: Arc<Mutex<Option<PendingPkce>>>,
    pkce_shutdown: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
    storage_path: PathBuf,
}

impl AnthropicOAuthManager {
    pub fn new(data_dir: PathBuf) -> Self {
        let manager = Self {
            accounts: Arc::new(RwLock::new(HashMap::new())),
            default_account_id: Arc::new(RwLock::new(None)),
            access_tokens: Arc::new(RwLock::new(HashMap::new())),
            refresh_locks: Arc::new(RwLock::new(HashMap::new())),
            mutation_lock: Arc::new(Mutex::new(())),
            pending_pkce: Arc::new(Mutex::new(None)),
            pkce_shutdown: Arc::new(Mutex::new(None)),
            storage_path: data_dir.join("anthropic_oauth_auth.json"),
        };
        if let Err(error) = manager.load_from_disk_sync() {
            log::warn!("[AnthropicOAuth] 加载存储失败: {error}");
        }
        manager
    }

    pub async fn import_from_claude_cli(
        &self,
    ) -> Result<AnthropicOAuthAccount, AnthropicOAuthError> {
        let json = read_claude_cli_credentials_json()?;
        self.import_from_credentials_json(&json).await
    }

    pub async fn import_from_credentials_json(
        &self,
        json: &str,
    ) -> Result<AnthropicOAuthAccount, AnthropicOAuthError> {
        let bundle = parse_claude_cli_oauth_bundle(json)?;
        let refresh_token = bundle
            .refresh_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| AnthropicOAuthError::ParseError("凭据缺少 refreshToken".to_string()))?
            .to_string();
        let access_token = bundle
            .access_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty());
        let (account_id, login) = extract_anthropic_identity(
            access_token,
            Some(refresh_token.as_str()),
            bundle.account_uuid.as_deref(),
            bundle.email.as_deref(),
        );
        let cached = access_token.map(|token| CachedAccessToken {
            token: token.to_string(),
            expires_at_ms: expires_at_ms_from_cli(bundle.expires_at.as_ref()),
        });
        self.add_account_internal(account_id, login, refresh_token, cached)
            .await
    }

    pub async fn start_pkce_flow(&self) -> Result<GitHubDeviceCodeResponse, AnthropicOAuthError> {
        self.abort_pkce_listener().await;

        let bind_addr = pkce_bind_addr();
        let listener = TcpListener::bind(bind_addr).await.map_err(|error| {
            AnthropicOAuthError::CallbackBindFailed(format!("{bind_addr}: {error}"))
        })?;
        let local_addr = listener
            .local_addr()
            .map_err(|error| AnthropicOAuthError::CallbackBindFailed(error.to_string()))?;
        let redirect_uri = pkce_redirect_uri(local_addr);
        let verifier = generate_pkce_verifier();
        let challenge = pkce_challenge(&verifier);
        let state = uuid::Uuid::new_v4().simple().to_string();
        let pending_id = uuid::Uuid::new_v4().simple().to_string();
        let expires_in = PKCE_LOGIN_LIFETIME_SECS;
        let expires_at_ms = chrono::Utc::now()
            .timestamp_millis()
            .saturating_add((expires_in as i64).saturating_mul(1_000));
        let authorize_url = build_authorize_url(&challenge, &state, &redirect_uri)?;

        {
            let mut pending = self.pending_pkce.lock().await;
            *pending = Some(PendingPkce {
                id: pending_id.clone(),
                verifier,
                expected_state: state,
                redirect_uri,
                expires_at_ms,
                authorization_code: None,
                error: None,
            });
        }

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        *self.pkce_shutdown.lock().await = Some(shutdown_tx);
        let pending = self.pending_pkce.clone();
        tokio::spawn(async move {
            run_pkce_callback_server(listener, pending, shutdown_rx).await;
        });

        Ok(GitHubDeviceCodeResponse {
            device_code: pending_id,
            user_code: "BROWSER".to_string(),
            verification_uri: authorize_url,
            expires_in,
            interval: 2,
        })
    }

    pub async fn poll_pkce(
        &self,
        pending_id: &str,
    ) -> Result<Option<AnthropicOAuthAccount>, AnthropicOAuthError> {
        let snapshot = {
            let pending = self.pending_pkce.lock().await;
            pending.clone()
        };
        let Some(pending) = snapshot else {
            return Err(AnthropicOAuthError::TokenFetchFailed(
                "没有进行中的浏览器登录，请重新开始".to_string(),
            ));
        };
        if pending.id != pending_id {
            return Err(AnthropicOAuthError::TokenFetchFailed(
                "登录会话已更换，请使用最新的授权链接".to_string(),
            ));
        }
        if chrono::Utc::now().timestamp_millis() > pending.expires_at_ms {
            self.clear_pending_pkce().await;
            return Err(AnthropicOAuthError::LoginExpired);
        }
        if let Some(error) = pending.error.as_deref() {
            self.clear_pending_pkce().await;
            return Err(AnthropicOAuthError::CallbackFailed(error.to_string()));
        }
        let Some(code) = pending.authorization_code.clone() else {
            return Err(AnthropicOAuthError::AuthorizationPending);
        };

        let tokens = match self
            .exchange_authorization_code(
                &code,
                &pending.verifier,
                &pending.redirect_uri,
                &pending.expected_state,
            )
            .await
        {
            Ok(tokens) => tokens,
            Err(error) => {
                self.clear_pending_pkce().await;
                return Err(error);
            }
        };
        self.clear_pending_pkce().await;

        let access_token = tokens.access_token.trim();
        let refresh_token = tokens
            .refresh_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                AnthropicOAuthError::ParseError("OAuth 响应缺少 refresh_token".to_string())
            })?
            .to_string();
        let (account_id, login) = extract_anthropic_identity(
            Some(access_token),
            Some(refresh_token.as_str()),
            None,
            None,
        );
        let cached = CachedAccessToken {
            token: access_token.to_string(),
            expires_at_ms: compute_expires_at_ms(tokens.expires_in)?,
        };
        let account = self
            .add_account_internal(account_id, login, refresh_token, Some(cached))
            .await?;
        Ok(Some(account))
    }

    #[cfg(test)]
    pub(crate) async fn pending_pkce_redirect_uri(&self) -> Option<String> {
        self.pending_pkce
            .lock()
            .await
            .as_ref()
            .map(|pending| pending.redirect_uri.clone())
    }

    pub async fn cancel_pkce_flow(&self) {
        self.abort_pkce_listener().await;
    }

    async fn abort_pkce_listener(&self) {
        if let Some(shutdown) = self.pkce_shutdown.lock().await.take() {
            let _ = shutdown.send(());
        }
        *self.pending_pkce.lock().await = None;
    }

    async fn clear_pending_pkce(&self) {
        self.abort_pkce_listener().await;
    }

    async fn exchange_authorization_code(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
        state: &str,
    ) -> Result<OAuthTokenResponse, AnthropicOAuthError> {
        let response = crate::proxy::http_client::get()
            .post(anthropic_token_url())
            .json(&serde_json::json!({
                "grant_type": "authorization_code",
                "client_id": ANTHROPIC_CLIENT_ID,
                "code": code,
                "redirect_uri": redirect_uri,
                "code_verifier": verifier,
                "state": state,
            }))
            .send()
            .await?;
        let status = response.status();
        let value = read_json_response(response).await?;
        if matches!(
            status,
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ) {
            return Err(AnthropicOAuthError::RefreshTokenInvalid);
        }
        if !status.is_success() {
            return Err(AnthropicOAuthError::TokenFetchFailed(format!(
                "HTTP {status}"
            )));
        }
        let tokens: OAuthTokenResponse = serde_json::from_value(value)
            .map_err(|_| AnthropicOAuthError::ParseError("OAuth Token 响应字段无效".to_string()))?;
        if tokens.access_token.trim().is_empty() {
            return Err(AnthropicOAuthError::TokenFetchFailed(
                "成功响应缺少 access_token".to_string(),
            ));
        }
        Ok(tokens)
    }

    pub async fn get_valid_token_for_account(
        &self,
        account_id: &str,
    ) -> Result<String, AnthropicOAuthError> {
        if let Some(token) = self.cached_token_for_usable_account(account_id).await {
            return Ok(token);
        }

        let refresh_lock = self.get_refresh_lock(account_id).await;
        let _refresh_guard = refresh_lock.lock().await;
        if let Some(token) = self.cached_token_for_usable_account(account_id).await {
            return Ok(token);
        }

        let account = self
            .accounts
            .read()
            .await
            .get(account_id)
            .cloned()
            .ok_or_else(|| AnthropicOAuthError::AccountNotFound(account_id.to_string()))?;
        if account.requires_reauth {
            return Err(AnthropicOAuthError::ReauthRequired(account_id.to_string()));
        }

        let tokens = match self.refresh_with_token(&account.refresh_token).await {
            Ok(tokens) => tokens,
            Err(AnthropicOAuthError::RefreshTokenInvalid) => {
                self.mark_reauth_required(account_id).await?;
                return Err(AnthropicOAuthError::ReauthRequired(account_id.to_string()));
            }
            Err(error) => return Err(error),
        };

        self.commit_refreshed_tokens(account_id, &account.refresh_token, tokens)
            .await
    }

    pub async fn get_valid_token(&self) -> Result<String, AnthropicOAuthError> {
        match self.resolve_default_account_id().await {
            Some(account_id) => self.get_valid_token_for_account(&account_id).await,
            None => Err(AnthropicOAuthError::AccountNotFound(
                "无可用的 Claude 账号，请登录或从 Claude CLI 导入".to_string(),
            )),
        }
    }

    pub async fn default_account_id(&self) -> Option<String> {
        self.resolve_default_account_id().await
    }

    pub async fn get_status(&self) -> AnthropicOAuthStatus {
        let accounts = self.accounts.read().await.clone();
        let default_account_id = self.resolve_default_account_id().await;
        let account_list = Self::sorted_accounts(&accounts, default_account_id.as_deref());
        let username = default_account_id
            .as_ref()
            .and_then(|id| accounts.get(id))
            .and_then(|account| account.login.clone());
        AnthropicOAuthStatus {
            authenticated: default_account_id.is_some(),
            default_account_id,
            accounts: account_list,
            username,
        }
    }

    pub async fn list_accounts(&self) -> Vec<AnthropicOAuthAccount> {
        let accounts = self.accounts.read().await.clone();
        let default_account_id = self.resolve_default_account_id().await;
        Self::sorted_accounts(&accounts, default_account_id.as_deref())
    }

    pub async fn remove_account(&self, account_id: &str) -> Result<(), AnthropicOAuthError> {
        let _mutation_guard = self.mutation_lock.lock().await;
        let mut accounts = self.accounts.read().await.clone();
        if accounts.remove(account_id).is_none() {
            return Err(AnthropicOAuthError::AccountNotFound(account_id.to_string()));
        }
        let stored_default = self.default_account_id.read().await.clone();
        let default_account_id = if stored_default.as_deref() == Some(account_id) {
            Self::fallback_default_account_id(&accounts)
        } else {
            stored_default.filter(|id| Self::is_usable_account(&accounts, id))
        };
        self.persist_and_commit(accounts, default_account_id)
            .await?;
        self.access_tokens.write().await.remove(account_id);
        Ok(())
    }

    pub async fn set_default_account(&self, account_id: &str) -> Result<(), AnthropicOAuthError> {
        let _mutation_guard = self.mutation_lock.lock().await;
        let accounts = self.accounts.read().await.clone();
        let account = accounts
            .get(account_id)
            .ok_or_else(|| AnthropicOAuthError::AccountNotFound(account_id.to_string()))?;
        if account.requires_reauth {
            return Err(AnthropicOAuthError::ReauthRequired(account_id.to_string()));
        }
        self.persist_and_commit(accounts, Some(account_id.to_string()))
            .await
    }

    pub async fn clear_auth(&self) -> Result<(), AnthropicOAuthError> {
        self.abort_pkce_listener().await;
        let _mutation_guard = self.mutation_lock.lock().await;
        if self.storage_path.exists() {
            fs::remove_file(&self.storage_path)?;
        }
        *self.accounts.write().await = HashMap::new();
        *self.default_account_id.write().await = None;
        self.access_tokens.write().await.clear();
        self.refresh_locks.write().await.clear();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn add_test_account_with_access_token(
        &self,
        account_id: &str,
        access_token: &str,
        login: Option<&str>,
    ) -> Result<AnthropicOAuthAccount, AnthropicOAuthError> {
        self.add_account_internal(
            account_id.to_string(),
            login.map(ToString::to_string),
            "test-refresh-token".to_string(),
            Some(CachedAccessToken {
                token: access_token.to_string(),
                expires_at_ms: chrono::Utc::now().timestamp_millis() + 3_600_000,
            }),
        )
        .await
    }

    async fn refresh_with_token(
        &self,
        refresh_token: &str,
    ) -> Result<OAuthTokenResponse, AnthropicOAuthError> {
        let response = crate::proxy::http_client::get()
            .post(anthropic_token_url())
            .json(&serde_json::json!({
                "grant_type": "refresh_token",
                "client_id": ANTHROPIC_CLIENT_ID,
                "refresh_token": refresh_token,
            }))
            .send()
            .await?;
        let status = response.status();
        let value = read_json_response(response).await?;
        if matches!(
            status,
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ) {
            return Err(AnthropicOAuthError::RefreshTokenInvalid);
        }
        if !status.is_success() {
            return Err(AnthropicOAuthError::TokenFetchFailed(format!(
                "HTTP {status}"
            )));
        }
        let tokens: OAuthTokenResponse = serde_json::from_value(value)
            .map_err(|_| AnthropicOAuthError::ParseError("OAuth Token 响应字段无效".to_string()))?;
        if tokens.access_token.trim().is_empty() {
            return Err(AnthropicOAuthError::TokenFetchFailed(
                "成功响应缺少 access_token".to_string(),
            ));
        }
        Ok(tokens)
    }

    async fn add_account_internal(
        &self,
        account_id: String,
        login: Option<String>,
        refresh_token: String,
        cached_access_token: Option<CachedAccessToken>,
    ) -> Result<AnthropicOAuthAccount, AnthropicOAuthError> {
        let _mutation_guard = self.mutation_lock.lock().await;
        let mut accounts = self.accounts.read().await.clone();
        let data = AnthropicAccountData {
            account_id: account_id.clone(),
            login,
            refresh_token,
            authenticated_at: chrono::Utc::now().timestamp(),
            requires_reauth: false,
        };
        let account = AnthropicOAuthAccount::from(&data);
        accounts.insert(account_id.clone(), data);
        let current_default = self.default_account_id.read().await.clone();
        let default_account_id = match current_default {
            Some(id) if Self::is_usable_account(&accounts, &id) => Some(id),
            _ => Some(account_id.clone()),
        };
        self.persist_and_commit(accounts, default_account_id)
            .await?;
        if let Some(access_token) = cached_access_token {
            self.access_tokens
                .write()
                .await
                .insert(account_id, access_token);
        }
        Ok(account)
    }

    async fn commit_refreshed_tokens(
        &self,
        account_id: &str,
        expected_refresh_token: &str,
        tokens: OAuthTokenResponse,
    ) -> Result<String, AnthropicOAuthError> {
        let _mutation_guard = self.mutation_lock.lock().await;
        let mut accounts = self.accounts.read().await.clone();
        let account = accounts
            .get_mut(account_id)
            .ok_or_else(|| AnthropicOAuthError::AccountNotFound(account_id.to_string()))?;
        if account.requires_reauth {
            return Err(AnthropicOAuthError::ReauthRequired(account_id.to_string()));
        }
        if account.refresh_token != expected_refresh_token {
            return Err(AnthropicOAuthError::TokenFetchFailed(
                "账号认证状态已变化，请重试请求".to_string(),
            ));
        }
        let expires_at_ms = compute_expires_at_ms(tokens.expires_in)?;
        let refresh_token_changed = tokens
            .refresh_token
            .as_deref()
            .filter(|token| !token.trim().is_empty())
            .is_some_and(|refresh_token| {
                if refresh_token == account.refresh_token {
                    false
                } else {
                    account.refresh_token = refresh_token.to_string();
                    true
                }
            });
        if refresh_token_changed {
            let default_account_id = self.default_account_id.read().await.clone();
            self.persist_and_commit(accounts, default_account_id)
                .await?;
        }
        let access_token = tokens.access_token;
        self.access_tokens.write().await.insert(
            account_id.to_string(),
            CachedAccessToken {
                token: access_token.clone(),
                expires_at_ms,
            },
        );
        Ok(access_token)
    }

    async fn mark_reauth_required(&self, account_id: &str) -> Result<(), AnthropicOAuthError> {
        let _mutation_guard = self.mutation_lock.lock().await;
        let mut accounts = self.accounts.read().await.clone();
        let account = accounts
            .get_mut(account_id)
            .ok_or_else(|| AnthropicOAuthError::AccountNotFound(account_id.to_string()))?;
        account.requires_reauth = true;
        let default_account_id = Self::fallback_default_account_id(&accounts);
        self.persist_and_commit(accounts, default_account_id)
            .await?;
        self.access_tokens.write().await.remove(account_id);
        Ok(())
    }

    async fn persist_and_commit(
        &self,
        accounts: HashMap<String, AnthropicAccountData>,
        default_account_id: Option<String>,
    ) -> Result<(), AnthropicOAuthError> {
        let store = AnthropicOAuthStore {
            version: 1,
            accounts: accounts.clone(),
            default_account_id: default_account_id.clone(),
        };
        let content = serde_json::to_string_pretty(&store)
            .map_err(|error| AnthropicOAuthError::ParseError(error.to_string()))?;
        write_oauth_store_atomic(&self.storage_path, &content)?;
        *self.accounts.write().await = accounts;
        *self.default_account_id.write().await = default_account_id;
        Ok(())
    }

    async fn cached_token_for_usable_account(&self, account_id: &str) -> Option<String> {
        let account_is_usable = {
            let accounts = self.accounts.read().await;
            Self::is_usable_account(&accounts, account_id)
        };
        if !account_is_usable {
            return None;
        }
        self.access_tokens
            .read()
            .await
            .get(account_id)
            .filter(|token| !token.is_expiring_soon())
            .map(|token| token.token.clone())
    }

    async fn get_refresh_lock(&self, account_id: &str) -> Arc<Mutex<()>> {
        let mut locks = self.refresh_locks.write().await;
        locks
            .entry(account_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn resolve_default_account_id(&self) -> Option<String> {
        let accounts = self.accounts.read().await;
        let stored = self.default_account_id.read().await.clone();
        stored
            .filter(|id| Self::is_usable_account(&accounts, id))
            .or_else(|| Self::fallback_default_account_id(&accounts))
    }

    fn fallback_default_account_id(
        accounts: &HashMap<String, AnthropicAccountData>,
    ) -> Option<String> {
        accounts
            .iter()
            .filter(|(_, account)| !account.requires_reauth)
            .max_by(|(id_a, account_a), (id_b, account_b)| {
                account_a
                    .authenticated_at
                    .cmp(&account_b.authenticated_at)
                    .then_with(|| id_b.cmp(id_a))
            })
            .map(|(id, _)| id.clone())
    }

    fn is_usable_account(accounts: &HashMap<String, AnthropicAccountData>, id: &str) -> bool {
        accounts
            .get(id)
            .is_some_and(|account| !account.requires_reauth)
    }

    fn sorted_accounts(
        accounts: &HashMap<String, AnthropicAccountData>,
        default_account_id: Option<&str>,
    ) -> Vec<AnthropicOAuthAccount> {
        let mut result: Vec<_> = accounts.values().map(AnthropicOAuthAccount::from).collect();
        result.sort_by(|a, b| {
            let a_default = default_account_id == Some(a.id.as_str());
            let b_default = default_account_id == Some(b.id.as_str());
            b_default
                .cmp(&a_default)
                .then_with(|| a.requires_reauth.cmp(&b.requires_reauth))
                .then_with(|| b.authenticated_at.cmp(&a.authenticated_at))
                .then_with(|| a.login.cmp(&b.login))
        });
        result
    }

    fn load_from_disk_sync(&self) -> Result<(), AnthropicOAuthError> {
        if !self.storage_path.exists() {
            return Ok(());
        }
        let content = fs::read_to_string(&self.storage_path)?;
        let store: AnthropicOAuthStore = serde_json::from_str(&content)
            .map_err(|error| AnthropicOAuthError::ParseError(error.to_string()))?;
        if let Ok(mut accounts) = self.accounts.try_write() {
            *accounts = store.accounts;
        }
        if let Ok(mut default_account_id) = self.default_account_id.try_write() {
            *default_account_id = store.default_account_id;
        }
        Ok(())
    }
}

fn anthropic_token_url() -> String {
    #[cfg(test)]
    {
        if let Ok(url) = std::env::var("CC_SWITCH_TEST_ANTHROPIC_OAUTH_TOKEN_URL") {
            let url = url.trim();
            if !url.is_empty() {
                return url.to_string();
            }
        }
    }
    ANTHROPIC_TOKEN_URL.to_string()
}

fn pkce_bind_addr() -> SocketAddr {
    #[cfg(test)]
    {
        return SocketAddr::from(([127, 0, 0, 1], 0));
    }
    #[cfg(not(test))]
    SocketAddr::from(([127, 0, 0, 1], PKCE_CALLBACK_PORT))
}

fn pkce_redirect_uri(local_addr: SocketAddr) -> String {
    #[cfg(test)]
    {
        return format!("http://127.0.0.1:{}/callback", local_addr.port());
    }
    #[cfg(not(test))]
    {
        let _ = local_addr;
        ANTHROPIC_REDIRECT_URI.to_string()
    }
}

fn generate_pkce_verifier() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

fn build_authorize_url(
    challenge: &str,
    state: &str,
    redirect_uri: &str,
) -> Result<String, AnthropicOAuthError> {
    let authorize = {
        #[cfg(test)]
        {
            if let Ok(url) = std::env::var("CC_SWITCH_TEST_ANTHROPIC_OAUTH_AUTHORIZE_URL") {
                let url = url.trim();
                if !url.is_empty() {
                    url.to_string()
                } else {
                    ANTHROPIC_AUTHORIZE_URL.to_string()
                }
            } else {
                ANTHROPIC_AUTHORIZE_URL.to_string()
            }
        }
        #[cfg(not(test))]
        ANTHROPIC_AUTHORIZE_URL.to_string()
    };
    let mut url = Url::parse(&authorize)
        .map_err(|error| AnthropicOAuthError::ParseError(error.to_string()))?;
    url.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("client_id", ANTHROPIC_CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", ANTHROPIC_OAUTH_SCOPE)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    Ok(url.to_string())
}

async fn run_pkce_callback_server(
    listener: TcpListener,
    pending: Arc<Mutex<Option<PendingPkce>>>,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let pending = pending.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_pkce_connection(stream, pending).await {
                        log::warn!("[AnthropicOAuth] PKCE callback: {error}");
                    }
                });
            }
        }
    }
}

async fn handle_pkce_connection(
    mut stream: tokio::net::TcpStream,
    pending: Arc<Mutex<Option<PendingPkce>>>,
) -> Result<(), String> {
    let mut buf = vec![0u8; 8192];
    let n = stream.read(&mut buf).await.map_err(|e| e.to_string())?;
    let request = String::from_utf8_lossy(&buf[..n]);
    let request_line = request.lines().next().unwrap_or("");
    let path = request_line.split_whitespace().nth(1).unwrap_or("/");
    let (target, query) = path.split_once('?').unwrap_or((path, ""));
    if !target.starts_with("/callback") {
        write_http_response(&mut stream, 404, "text/plain", "not found").await?;
        return Ok(());
    }
    let params = parse_query(query);
    let mut pending_guard = pending.lock().await;
    let Some(pending) = pending_guard.as_mut() else {
        drop(pending_guard);
        write_http_response(&mut stream, 410, "text/html; charset=utf-8", PKCE_HTML_GONE).await?;
        return Ok(());
    };
    if let Some(error) = params.get("error") {
        let description = params
            .get("error_description")
            .cloned()
            .unwrap_or_else(|| error.clone());
        pending.error = Some(description);
        drop(pending_guard);
        write_http_response(
            &mut stream,
            200,
            "text/html; charset=utf-8",
            PKCE_HTML_DENIED,
        )
        .await?;
        return Ok(());
    }
    let Some(code) = params.get("code").cloned().filter(|c| !c.is_empty()) else {
        drop(pending_guard);
        write_http_response(
            &mut stream,
            400,
            "text/html; charset=utf-8",
            PKCE_HTML_MISSING_CODE,
        )
        .await?;
        return Ok(());
    };
    let returned_state = params.get("state").cloned().unwrap_or_default();
    if returned_state != pending.expected_state {
        pending.error = Some("OAuth state mismatch".to_string());
        drop(pending_guard);
        write_http_response(
            &mut stream,
            400,
            "text/html; charset=utf-8",
            PKCE_HTML_STATE,
        )
        .await?;
        return Ok(());
    }
    pending.authorization_code = Some(code);
    drop(pending_guard);
    write_http_response(&mut stream, 200, "text/html; charset=utf-8", PKCE_HTML_OK).await?;
    Ok(())
}

fn parse_query(query: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        let value = value.into_owned();
        if key.as_ref() == "code" {
            if let Some((code, fragment_state)) = value.split_once('#') {
                if !fragment_state.is_empty() {
                    out.insert("code".to_string(), code.to_string());
                    out.insert("state".to_string(), fragment_state.to_string());
                    continue;
                }
            }
        }
        out.insert(key.into_owned(), value);
    }
    out
}

async fn write_http_response(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
) -> Result<(), String> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        410 => "Gone",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stream.flush().await.map_err(|e| e.to_string())
}

const PKCE_HTML_OK: &str = "<!DOCTYPE html><html><body><p>Claude login complete. You can close this tab.</p></body></html>";
const PKCE_HTML_DENIED: &str =
    "<!DOCTYPE html><html><body><p>Claude login was denied or failed.</p></body></html>";
const PKCE_HTML_MISSING_CODE: &str =
    "<!DOCTYPE html><html><body><p>Missing authorization code.</p></body></html>";
const PKCE_HTML_STATE: &str = "<!DOCTYPE html><html><body><p>Login state mismatch. Please retry from CC Switch.</p></body></html>";
const PKCE_HTML_GONE: &str =
    "<!DOCTYPE html><html><body><p>This login session is no longer active.</p></body></html>";

fn read_claude_cli_credentials_json() -> Result<String, AnthropicOAuthError> {
    #[cfg(target_os = "macos")]
    {
        if let Some(json) = read_claude_credentials_from_keychain() {
            return Ok(json);
        }
    }
    let cred_path = crate::config::get_claude_config_dir().join(".credentials.json");
    if !cred_path.exists() {
        return Err(AnthropicOAuthError::CredentialsNotFound);
    }
    fs::read_to_string(cred_path).map_err(Into::into)
}

#[cfg(target_os = "macos")]
fn read_claude_credentials_from_keychain() -> Option<String> {
    let output = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let json_str = String::from_utf8(output.stdout).ok()?;
    let json_str = json_str.trim();
    if json_str.is_empty() {
        return None;
    }
    Some(json_str.to_string())
}

fn parse_claude_cli_oauth_bundle(
    content: &str,
) -> Result<ClaudeCliOauthEntry, AnthropicOAuthError> {
    let parsed: serde_json::Value = serde_json::from_str(content)
        .map_err(|error| AnthropicOAuthError::ParseError(error.to_string()))?;
    let entry_value = parsed
        .get("claudeAiOauth")
        .or_else(|| parsed.get("claude.ai_oauth"))
        .ok_or_else(|| AnthropicOAuthError::ParseError("No OAuth entry found".to_string()))?;
    serde_json::from_value(entry_value.clone())
        .map_err(|error| AnthropicOAuthError::ParseError(error.to_string()))
}

fn parse_jwt_claims(token: &str) -> Option<AnthropicTokenClaims> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&decoded).ok()
}

pub(crate) fn extract_anthropic_identity(
    access_token: Option<&str>,
    refresh_token: Option<&str>,
    account_uuid: Option<&str>,
    email: Option<&str>,
) -> (String, Option<String>) {
    let claims = access_token
        .and_then(parse_jwt_claims)
        .or_else(|| refresh_token.and_then(parse_jwt_claims));
    let account_id = claims
        .as_ref()
        .and_then(|claims| claims.user_id.clone().or(claims.sub.clone()))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            account_uuid
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string)
        })
        .unwrap_or_else(|| fingerprint_refresh(refresh_token.unwrap_or("unknown")));
    let login = claims
        .and_then(|claims| claims.email)
        .or_else(|| email.map(ToString::to_string))
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty());
    (account_id, login)
}

fn fingerprint_refresh(refresh_token: &str) -> String {
    let digest = Sha256::digest(refresh_token.as_bytes());
    digest
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn compute_expires_at_ms(expires_in: Option<i64>) -> Result<i64, AnthropicOAuthError> {
    match expires_in {
        Some(secs) if secs < 0 => Err(AnthropicOAuthError::ParseError(
            "expires_in 必须是非负整数".to_string(),
        )),
        Some(secs) => Ok(chrono::Utc::now()
            .timestamp_millis()
            .saturating_add(secs.saturating_mul(1_000))),
        None => Ok(chrono::Utc::now()
            .timestamp_millis()
            .saturating_add(DEFAULT_TOKEN_LIFETIME_SECS.saturating_mul(1_000))),
    }
}

fn expires_at_ms_from_cli(expires_at: Option<&serde_json::Value>) -> i64 {
    let now = chrono::Utc::now().timestamp_millis();
    let Some(expires_at) = expires_at else {
        return now.saturating_add(DEFAULT_TOKEN_LIFETIME_SECS.saturating_mul(1_000));
    };
    match expires_at {
        serde_json::Value::Number(n) => {
            let Some(ts) = n.as_i64().or_else(|| n.as_u64().map(|v| v as i64)) else {
                return now.saturating_add(DEFAULT_TOKEN_LIFETIME_SECS.saturating_mul(1_000));
            };
            if ts > 1_000_000_000_000 {
                ts
            } else {
                ts.saturating_mul(1_000)
            }
        }
        _ => now.saturating_add(DEFAULT_TOKEN_LIFETIME_SECS.saturating_mul(1_000)),
    }
}

async fn read_json_response(
    response: reqwest::Response,
) -> Result<serde_json::Value, AnthropicOAuthError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_OAUTH_RESPONSE_BYTES as u64)
    {
        return Err(AnthropicOAuthError::ParseError(
            "OAuth 响应超过大小限制".to_string(),
        ));
    }
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_OAUTH_RESPONSE_BYTES {
        return Err(AnthropicOAuthError::ParseError(
            "OAuth 响应超过大小限制".to_string(),
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| AnthropicOAuthError::ParseError("OAuth 响应不是有效 JSON".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt_with_payload(payload: &str) -> String {
        let encoded = URL_SAFE_NO_PAD.encode(payload.as_bytes());
        format!("aaa.{encoded}.sig")
    }

    #[test]
    fn identity_prefers_jwt_user_id() {
        let token = jwt_with_payload(r#"{"user_id":"user-a","email":"Ada@Claude.AI"}"#);
        let (id, login) = extract_anthropic_identity(Some(&token), None, Some("uuid"), None);
        assert_eq!(id, "user-a");
        assert_eq!(login.as_deref(), Some("ada@claude.ai"));
    }

    #[test]
    fn identity_falls_back_to_account_uuid_then_refresh_fingerprint() {
        let (id, _) = extract_anthropic_identity(None, Some("refresh-1"), Some("acct-uuid"), None);
        assert_eq!(id, "acct-uuid");
        let (finger, _) = extract_anthropic_identity(None, Some("refresh-1"), None, None);
        assert_eq!(finger, fingerprint_refresh("refresh-1"));
        assert_eq!(
            extract_anthropic_identity(None, Some("refresh-1"), None, None).0,
            extract_anthropic_identity(None, Some("refresh-1"), None, None).0
        );
    }

    #[tokio::test]
    async fn import_from_cli_fixture_json_round_trips() {
        let data_dir = tempfile::tempdir().unwrap();
        let manager = AnthropicOAuthManager::new(data_dir.path().to_path_buf());
        let fixture = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "sk-ant-oat01-imported",
                "refreshToken": "refresh-imported",
                "expiresAt": chrono::Utc::now().timestamp_millis() + 3_600_000,
                "accountUuid": "acct-imported",
                "email": "Pro@Claude.AI"
            }
        });
        let account = manager
            .import_from_credentials_json(&fixture.to_string())
            .await
            .unwrap();
        assert_eq!(account.id, "acct-imported");
        assert_eq!(account.login, "pro@claude.ai");
        assert_eq!(
            manager
                .get_valid_token_for_account("acct-imported")
                .await
                .unwrap(),
            "sk-ant-oat01-imported"
        );

        let reloaded = AnthropicOAuthManager::new(data_dir.path().to_path_buf());
        assert_eq!(reloaded.list_accounts().await.len(), 1);
        assert_eq!(
            reloaded.get_status().await.default_account_id.as_deref(),
            Some("acct-imported")
        );
    }

    #[tokio::test]
    async fn import_from_missing_cli_credentials_is_not_found() {
        let home = tempfile::tempdir().unwrap();
        let original = std::env::var("CC_SWITCH_TEST_HOME").ok();
        std::env::set_var("CC_SWITCH_TEST_HOME", home.path());
        let data_dir = tempfile::tempdir().unwrap();
        let manager = AnthropicOAuthManager::new(data_dir.path().to_path_buf());
        let err = manager.import_from_claude_cli().await.unwrap_err();
        assert!(matches!(err, AnthropicOAuthError::CredentialsNotFound));
        match original {
            Some(value) => std::env::set_var("CC_SWITCH_TEST_HOME", value),
            None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
        }
    }

    #[tokio::test]
    async fn pkce_flow_exchanges_code_from_localhost_callback() {
        let captured =
            std::sync::Arc::new(tokio::sync::Mutex::new(Option::<serde_json::Value>::None));
        let token = jwt_with_payload(r#"{"user_id":"user-pkce","email":"Pkce@Claude.AI"}"#);
        let captured_clone = captured.clone();
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(move |body: axum::Json<serde_json::Value>| {
                let captured = captured_clone.clone();
                let token = token.clone();
                async move {
                    *captured.lock().await = Some(body.0);
                    axum::Json(serde_json::json!({
                        "access_token": token,
                        "refresh_token": "refresh-pkce",
                        "expires_in": 3600
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let original = std::env::var("CC_SWITCH_TEST_ANTHROPIC_OAUTH_TOKEN_URL").ok();
        std::env::set_var(
            "CC_SWITCH_TEST_ANTHROPIC_OAUTH_TOKEN_URL",
            format!("http://{addr}/"),
        );

        let data_dir = tempfile::tempdir().unwrap();
        let manager = AnthropicOAuthManager::new(data_dir.path().to_path_buf());
        let started = manager.start_pkce_flow().await.unwrap();
        assert_eq!(started.user_code, "BROWSER");
        assert!(started.verification_uri.contains("code_challenge"));
        assert!(started.verification_uri.contains("client_id"));

        let pending_before = manager.poll_pkce(&started.device_code).await.unwrap_err();
        assert!(matches!(
            pending_before,
            AnthropicOAuthError::AuthorizationPending
        ));

        let redirect = manager.pending_pkce_redirect_uri().await.unwrap();
        let authorize = url::Url::parse(&started.verification_uri).unwrap();
        let state = authorize
            .query_pairs()
            .find(|(key, _)| key == "state")
            .map(|(_, value)| value.into_owned())
            .unwrap();
        let callback = format!("{redirect}?code=auth-code-1&state={state}");
        let page = reqwest::get(&callback).await.unwrap();
        assert!(page.status().is_success());
        let html = page.text().await.unwrap();
        assert!(html.contains("login complete"));

        let account = manager
            .poll_pkce(&started.device_code)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(account.id, "user-pkce");
        assert_eq!(account.login, "pkce@claude.ai");
        assert_eq!(
            manager
                .get_valid_token_for_account("user-pkce")
                .await
                .unwrap(),
            jwt_with_payload(r#"{"user_id":"user-pkce","email":"Pkce@Claude.AI"}"#)
        );

        let body = captured.lock().await.clone().unwrap();
        assert_eq!(body["grant_type"], "authorization_code");
        assert_eq!(body["code"], "auth-code-1");
        assert_eq!(body["redirect_uri"], redirect);
        assert!(body["code_verifier"].as_str().unwrap().len() >= 43);

        match original {
            Some(value) => std::env::set_var("CC_SWITCH_TEST_ANTHROPIC_OAUTH_TOKEN_URL", value),
            None => std::env::remove_var("CC_SWITCH_TEST_ANTHROPIC_OAUTH_TOKEN_URL"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn pkce_rejects_state_mismatch() {
        let data_dir = tempfile::tempdir().unwrap();
        let manager = AnthropicOAuthManager::new(data_dir.path().to_path_buf());
        let started = manager.start_pkce_flow().await.unwrap();
        let redirect = manager.pending_pkce_redirect_uri().await.unwrap();
        let callback = format!("{redirect}?code=auth-code-1&state=wrong");
        let page = reqwest::get(&callback).await.unwrap();
        assert_eq!(page.status(), reqwest::StatusCode::BAD_REQUEST);

        let err = manager.poll_pkce(&started.device_code).await.unwrap_err();
        assert!(matches!(err, AnthropicOAuthError::CallbackFailed(_)));
    }

    #[tokio::test]
    async fn cancel_pkce_flow_clears_pending_callback() {
        let data_dir = tempfile::tempdir().unwrap();
        let manager = AnthropicOAuthManager::new(data_dir.path().to_path_buf());
        let started = manager.start_pkce_flow().await.unwrap();
        assert!(manager.pending_pkce_redirect_uri().await.is_some());

        manager.cancel_pkce_flow().await;
        assert!(manager.pending_pkce_redirect_uri().await.is_none());
        let err = manager.poll_pkce(&started.device_code).await.unwrap_err();
        assert!(matches!(err, AnthropicOAuthError::TokenFetchFailed(_)));
    }
}
