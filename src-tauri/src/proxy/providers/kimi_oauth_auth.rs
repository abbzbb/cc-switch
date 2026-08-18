//! Kimi For Coding OAuth (device-code) account store.
//!
//! Protocol follows the public Kimi Code / OpenCodex device-code flow, stored
//! in cc-switch's app config dir rather than on individual provider cards.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

use super::copilot_auth::GitHubDeviceCodeResponse;

pub const KIMI_CODING_API_BASE_URL: &str = "https://api.kimi.com/coding/v1";
const KIMI_OAUTH_HOST: &str = "https://auth.kimi.com";
const KIMI_CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const KIMI_USER_AGENT: &str = "KimiCLI/0.14.0";
const KIMI_MSH_PLATFORM: &str = "kimi_code_cli";
const KIMI_MSH_VERSION: &str = "0.14.0";
const TOKEN_REFRESH_BUFFER_MS: i64 = 5 * 60_000;
const DEFAULT_TOKEN_LIFETIME_SECS: i64 = 3_600;
const POLLING_SAFETY_MARGIN_SECS: u64 = 3;
const MAX_DEVICE_CODE_LIFETIME_SECS: u64 = 24 * 60 * 60;
const MAX_POLL_INTERVAL_SECS: u64 = 60;
const MAX_OAUTH_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum KimiOAuthError {
    #[error("等待用户授权中")]
    AuthorizationPending,
    #[error("用户拒绝授权")]
    AccessDenied,
    #[error("Device Code 已过期")]
    ExpiredToken,
    #[error("OAuth Token 获取失败: {0}")]
    TokenFetchFailed(String),
    #[error("Refresh Token 失效或已过期，请重新登录 Kimi")]
    RefreshTokenInvalid,
    #[error("账号需要重新登录: {0}")]
    ReauthRequired(String),
    #[error("网络错误: {0}")]
    NetworkError(String),
    #[error("解析错误: {0}")]
    ParseError(String),
    #[error("IO 错误: {0}")]
    IoError(String),
    #[error("账号不存在: {0}")]
    AccountNotFound(String),
}

impl From<reqwest::Error> for KimiOAuthError {
    fn from(err: reqwest::Error) -> Self {
        Self::NetworkError(err.to_string())
    }
}

impl From<std::io::Error> for KimiOAuthError {
    fn from(err: std::io::Error) -> Self {
        Self::IoError(err.to_string())
    }
}

#[derive(Debug, Clone, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    expires_in: u64,
    #[serde(default = "default_poll_interval")]
    interval: u64,
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
struct KimiTokenClaims {
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    sub: Option<String>,
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

#[derive(Debug, Clone)]
struct PendingDeviceCode {
    token_endpoint: String,
    expires_at_ms: i64,
    interval_secs: u64,
    next_poll_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct KimiAccountData {
    account_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    login: Option<String>,
    refresh_token: String,
    authenticated_at: i64,
    #[serde(default)]
    requires_reauth: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KimiOAuthAccount {
    pub id: String,
    pub login: String,
    pub avatar_url: Option<String>,
    pub authenticated_at: i64,
    pub github_domain: String,
    pub requires_reauth: bool,
}

impl From<&KimiAccountData> for KimiOAuthAccount {
    fn from(data: &KimiAccountData) -> Self {
        let short_id: String = data.account_id.chars().take(12).collect();
        Self {
            id: data.account_id.clone(),
            login: data
                .login
                .clone()
                .unwrap_or_else(|| format!("Kimi ({short_id})")),
            avatar_url: None,
            authenticated_at: data.authenticated_at,
            github_domain: "kimi.com".to_string(),
            requires_reauth: data.requires_reauth,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct KimiOAuthStore {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    accounts: HashMap<String, KimiAccountData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default_account_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KimiOAuthStatus {
    pub accounts: Vec<KimiOAuthAccount>,
    pub default_account_id: Option<String>,
    pub authenticated: bool,
    pub username: Option<String>,
}

pub struct KimiOAuthManager {
    accounts: Arc<RwLock<HashMap<String, KimiAccountData>>>,
    default_account_id: Arc<RwLock<Option<String>>>,
    access_tokens: Arc<RwLock<HashMap<String, CachedAccessToken>>>,
    refresh_locks: Arc<RwLock<HashMap<String, Arc<Mutex<()>>>>>,
    pending_device_codes: Arc<RwLock<HashMap<String, PendingDeviceCode>>>,
    mutation_lock: Arc<Mutex<()>>,
    storage_path: PathBuf,
    device_id: String,
}

impl KimiOAuthManager {
    pub fn new(data_dir: PathBuf) -> Self {
        let device_id = load_or_create_device_id(&data_dir);
        let manager = Self {
            accounts: Arc::new(RwLock::new(HashMap::new())),
            default_account_id: Arc::new(RwLock::new(None)),
            access_tokens: Arc::new(RwLock::new(HashMap::new())),
            refresh_locks: Arc::new(RwLock::new(HashMap::new())),
            pending_device_codes: Arc::new(RwLock::new(HashMap::new())),
            mutation_lock: Arc::new(Mutex::new(())),
            storage_path: data_dir.join("kimi_oauth_auth.json"),
            device_id,
        };
        if let Err(error) = manager.load_from_disk_sync() {
            log::warn!("[KimiOAuth] 加载存储失败: {error}");
        }
        manager
    }

    pub async fn start_device_flow(&self) -> Result<GitHubDeviceCodeResponse, KimiOAuthError> {
        let host = kimi_oauth_host();
        let device_endpoint = format!("{host}/api/oauth/device_authorization");
        let token_endpoint = format!("{host}/api/oauth/token");
        let response = crate::proxy::http_client::get()
            .post(&device_endpoint)
            .headers(self.kimi_headers())
            .form(&[("client_id", KIMI_CLIENT_ID)])
            .send()
            .await?;

        let status = response.status();
        let value = read_json_response(response).await?;
        if !status.is_success() {
            return Err(KimiOAuthError::TokenFetchFailed(format_oauth_error(
                status, &value,
            )));
        }
        let device = parse_device_code_response(value)?;
        let interval = device
            .interval
            .clamp(1, MAX_POLL_INTERVAL_SECS)
            .saturating_add(POLLING_SAFETY_MARGIN_SECS);
        let expires_in = device.expires_in.clamp(1, MAX_DEVICE_CODE_LIFETIME_SECS);
        let now_ms = chrono::Utc::now().timestamp_millis();

        {
            let mut pending = self.pending_device_codes.write().await;
            pending.retain(|_, entry| entry.expires_at_ms > now_ms);
            pending.insert(
                device.device_code.clone(),
                PendingDeviceCode {
                    token_endpoint,
                    expires_at_ms: now_ms.saturating_add(
                        i64::try_from(expires_in)
                            .unwrap_or(i64::MAX)
                            .saturating_mul(1_000),
                    ),
                    interval_secs: interval,
                    next_poll_at_ms: now_ms,
                },
            );
        }

        Ok(GitHubDeviceCodeResponse {
            device_code: device.device_code,
            user_code: device.user_code,
            verification_uri: device
                .verification_uri_complete
                .unwrap_or(device.verification_uri),
            expires_in,
            interval,
        })
    }

    pub async fn poll_for_token(
        &self,
        device_code: &str,
    ) -> Result<Option<KimiOAuthAccount>, KimiOAuthError> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let entry = {
            let pending = self.pending_device_codes.read().await;
            pending.get(device_code).cloned()
        }
        .ok_or_else(|| {
            KimiOAuthError::TokenFetchFailed("Device Code 不存在，请重新启动登录".to_string())
        })?;

        if entry.expires_at_ms <= now_ms {
            self.pending_device_codes.write().await.remove(device_code);
            return Err(KimiOAuthError::ExpiredToken);
        }
        if entry.next_poll_at_ms > now_ms {
            return Err(KimiOAuthError::AuthorizationPending);
        }
        self.schedule_next_poll(device_code, entry.interval_secs)
            .await;

        let response = crate::proxy::http_client::get()
            .post(&entry.token_endpoint)
            .headers(self.kimi_headers())
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", KIMI_CLIENT_ID),
                ("device_code", device_code),
            ])
            .send()
            .await?;
        let status = response.status();
        let value = read_json_response(response).await?;

        if let Some(error_code) = oauth_error_code(&value) {
            return match error_code.as_str() {
                "authorization_pending" => Err(KimiOAuthError::AuthorizationPending),
                "slow_down" => {
                    self.increase_poll_interval(device_code).await;
                    Err(KimiOAuthError::AuthorizationPending)
                }
                "access_denied" => {
                    self.pending_device_codes.write().await.remove(device_code);
                    Err(KimiOAuthError::AccessDenied)
                }
                "expired_token" => {
                    self.pending_device_codes.write().await.remove(device_code);
                    Err(KimiOAuthError::ExpiredToken)
                }
                _ => Err(KimiOAuthError::TokenFetchFailed(format_oauth_error(
                    status, &value,
                ))),
            };
        }
        if !status.is_success() {
            return Err(KimiOAuthError::TokenFetchFailed(format_oauth_error(
                status, &value,
            )));
        }

        let tokens = parse_token_response(value)?;
        validate_access_token(&tokens.access_token)?;
        let expires_at_ms = compute_expires_at_ms(tokens.expires_in)?;
        let refresh_token = tokens
            .refresh_token
            .as_deref()
            .filter(|token| !token.trim().is_empty())
            .map(ToString::to_string)
            .ok_or_else(|| {
                KimiOAuthError::TokenFetchFailed("成功响应缺少 refresh_token".to_string())
            })?;
        let (account_id, login) = extract_kimi_identity(&tokens.access_token, Some(&refresh_token))
            .ok_or_else(|| {
                KimiOAuthError::ParseError(
                    "Kimi token 缺少稳定的 user_id/sub claim，未保存账号".to_string(),
                )
            })?;

        let cached_access_token = CachedAccessToken {
            token: tokens.access_token,
            expires_at_ms,
        };
        let account = self
            .add_account_internal(
                account_id,
                login,
                refresh_token,
                Some(device_code),
                Some(cached_access_token),
            )
            .await?;
        Ok(Some(account))
    }

    pub async fn get_valid_token_for_account(
        &self,
        account_id: &str,
    ) -> Result<String, KimiOAuthError> {
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
            .ok_or_else(|| KimiOAuthError::AccountNotFound(account_id.to_string()))?;
        if account.requires_reauth {
            return Err(KimiOAuthError::ReauthRequired(account_id.to_string()));
        }

        let tokens = match self.refresh_with_token(&account.refresh_token).await {
            Ok(tokens) => tokens,
            Err(KimiOAuthError::RefreshTokenInvalid) => {
                self.mark_reauth_required(account_id).await?;
                return Err(KimiOAuthError::ReauthRequired(account_id.to_string()));
            }
            Err(error) => return Err(error),
        };

        self.commit_refreshed_tokens(account_id, &account.refresh_token, tokens)
            .await
    }

    pub async fn get_valid_token(&self) -> Result<String, KimiOAuthError> {
        match self.resolve_default_account_id().await {
            Some(account_id) => self.get_valid_token_for_account(&account_id).await,
            None => Err(KimiOAuthError::AccountNotFound(
                "无可用的 Kimi 账号，请登录或重新登录".to_string(),
            )),
        }
    }

    pub async fn default_account_id(&self) -> Option<String> {
        self.resolve_default_account_id().await
    }

    pub async fn get_status(&self) -> KimiOAuthStatus {
        let accounts = self.accounts.read().await.clone();
        let default_account_id = self.resolve_default_account_id().await;
        let account_list = Self::sorted_accounts(&accounts, default_account_id.as_deref());
        let username = default_account_id
            .as_ref()
            .and_then(|id| accounts.get(id))
            .and_then(|account| account.login.clone());
        KimiOAuthStatus {
            authenticated: default_account_id.is_some(),
            default_account_id,
            accounts: account_list,
            username,
        }
    }

    pub async fn list_accounts(&self) -> Vec<KimiOAuthAccount> {
        let accounts = self.accounts.read().await.clone();
        let default_account_id = self.resolve_default_account_id().await;
        Self::sorted_accounts(&accounts, default_account_id.as_deref())
    }

    pub async fn remove_account(&self, account_id: &str) -> Result<(), KimiOAuthError> {
        let _mutation_guard = self.mutation_lock.lock().await;
        let mut accounts = self.accounts.read().await.clone();
        if accounts.remove(account_id).is_none() {
            return Err(KimiOAuthError::AccountNotFound(account_id.to_string()));
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

    pub async fn set_default_account(&self, account_id: &str) -> Result<(), KimiOAuthError> {
        let _mutation_guard = self.mutation_lock.lock().await;
        let accounts = self.accounts.read().await.clone();
        let account = accounts
            .get(account_id)
            .ok_or_else(|| KimiOAuthError::AccountNotFound(account_id.to_string()))?;
        if account.requires_reauth {
            return Err(KimiOAuthError::ReauthRequired(account_id.to_string()));
        }
        self.persist_and_commit(accounts, Some(account_id.to_string()))
            .await
    }

    pub async fn clear_auth(&self) -> Result<(), KimiOAuthError> {
        let _mutation_guard = self.mutation_lock.lock().await;
        if self.storage_path.exists() {
            fs::remove_file(&self.storage_path)?;
        }
        *self.accounts.write().await = HashMap::new();
        *self.default_account_id.write().await = None;
        self.access_tokens.write().await.clear();
        self.refresh_locks.write().await.clear();
        self.pending_device_codes.write().await.clear();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn add_test_account_with_access_token(
        &self,
        account_id: &str,
        access_token: &str,
        login: Option<&str>,
    ) -> Result<KimiOAuthAccount, KimiOAuthError> {
        self.add_account_internal(
            account_id.to_string(),
            login.map(ToString::to_string),
            "test-refresh-token".to_string(),
            None,
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
    ) -> Result<OAuthTokenResponse, KimiOAuthError> {
        let token_endpoint = format!("{}/api/oauth/token", kimi_oauth_host());
        let response = crate::proxy::http_client::get()
            .post(&token_endpoint)
            .headers(self.kimi_headers())
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", KIMI_CLIENT_ID),
                ("refresh_token", refresh_token),
            ])
            .send()
            .await?;
        let status = response.status();
        let value = read_json_response(response).await?;
        if refresh_response_requires_reauth(status, oauth_error_code(&value).is_some()) {
            return Err(KimiOAuthError::RefreshTokenInvalid);
        }
        if !status.is_success() {
            return Err(KimiOAuthError::TokenFetchFailed(format_oauth_error(
                status, &value,
            )));
        }
        let tokens = parse_token_response(value)?;
        validate_access_token(&tokens.access_token)?;
        Ok(tokens)
    }

    async fn add_account_internal(
        &self,
        account_id: String,
        login: Option<String>,
        refresh_token: String,
        pending_device_code: Option<&str>,
        cached_access_token: Option<CachedAccessToken>,
    ) -> Result<KimiOAuthAccount, KimiOAuthError> {
        let _mutation_guard = self.mutation_lock.lock().await;
        if let Some(device_code) = pending_device_code {
            let login_is_pending = self
                .pending_device_codes
                .read()
                .await
                .contains_key(device_code);
            if !login_is_pending {
                return Err(KimiOAuthError::TokenFetchFailed(
                    "登录已取消，请重新启动登录".to_string(),
                ));
            }
        }
        let mut accounts = self.accounts.read().await.clone();
        let data = KimiAccountData {
            account_id: account_id.clone(),
            login,
            refresh_token,
            authenticated_at: chrono::Utc::now().timestamp(),
            requires_reauth: false,
        };
        let account = KimiOAuthAccount::from(&data);
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
        if let Some(device_code) = pending_device_code {
            self.pending_device_codes.write().await.remove(device_code);
        }
        Ok(account)
    }

    async fn commit_refreshed_tokens(
        &self,
        account_id: &str,
        expected_refresh_token: &str,
        tokens: OAuthTokenResponse,
    ) -> Result<String, KimiOAuthError> {
        let _mutation_guard = self.mutation_lock.lock().await;
        let mut accounts = self.accounts.read().await.clone();
        let account = accounts
            .get_mut(account_id)
            .ok_or_else(|| KimiOAuthError::AccountNotFound(account_id.to_string()))?;
        if account.requires_reauth {
            return Err(KimiOAuthError::ReauthRequired(account_id.to_string()));
        }
        if account.refresh_token != expected_refresh_token {
            return Err(KimiOAuthError::TokenFetchFailed(
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

    async fn mark_reauth_required(&self, account_id: &str) -> Result<(), KimiOAuthError> {
        let _mutation_guard = self.mutation_lock.lock().await;
        let mut accounts = self.accounts.read().await.clone();
        let account = accounts
            .get_mut(account_id)
            .ok_or_else(|| KimiOAuthError::AccountNotFound(account_id.to_string()))?;
        account.requires_reauth = true;
        let default_account_id = Self::fallback_default_account_id(&accounts);
        self.persist_and_commit(accounts, default_account_id)
            .await?;
        self.access_tokens.write().await.remove(account_id);
        Ok(())
    }

    async fn persist_and_commit(
        &self,
        accounts: HashMap<String, KimiAccountData>,
        default_account_id: Option<String>,
    ) -> Result<(), KimiOAuthError> {
        let store = KimiOAuthStore {
            version: 1,
            accounts: accounts.clone(),
            default_account_id: default_account_id.clone(),
        };
        let content = serde_json::to_string_pretty(&store)
            .map_err(|error| KimiOAuthError::ParseError(error.to_string()))?;
        self.write_store_atomic(&content)?;
        *self.accounts.write().await = accounts;
        *self.default_account_id.write().await = default_account_id;
        Ok(())
    }

    async fn cached_token(&self, account_id: &str) -> Option<String> {
        self.access_tokens
            .read()
            .await
            .get(account_id)
            .filter(|token| !token.is_expiring_soon())
            .map(|token| token.token.clone())
    }

    async fn cached_token_for_usable_account(&self, account_id: &str) -> Option<String> {
        let account_is_usable = {
            let accounts = self.accounts.read().await;
            Self::is_usable_account(&accounts, account_id)
        };
        if !account_is_usable {
            return None;
        }
        self.cached_token(account_id).await
    }

    async fn schedule_next_poll(&self, device_code: &str, interval_secs: u64) {
        if let Some(entry) = self.pending_device_codes.write().await.get_mut(device_code) {
            entry.next_poll_at_ms = chrono::Utc::now().timestamp_millis().saturating_add(
                i64::try_from(interval_secs)
                    .unwrap_or(i64::MAX)
                    .saturating_mul(1_000),
            );
        }
    }

    async fn increase_poll_interval(&self, device_code: &str) {
        if let Some(entry) = self.pending_device_codes.write().await.get_mut(device_code) {
            entry.interval_secs = entry
                .interval_secs
                .saturating_add(5)
                .min(MAX_POLL_INTERVAL_SECS + POLLING_SAFETY_MARGIN_SECS);
            entry.next_poll_at_ms = chrono::Utc::now().timestamp_millis().saturating_add(
                i64::try_from(entry.interval_secs)
                    .unwrap_or(i64::MAX)
                    .saturating_mul(1_000),
            );
        }
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

    fn fallback_default_account_id(accounts: &HashMap<String, KimiAccountData>) -> Option<String> {
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

    fn is_usable_account(accounts: &HashMap<String, KimiAccountData>, id: &str) -> bool {
        accounts
            .get(id)
            .is_some_and(|account| !account.requires_reauth)
    }

    fn sorted_accounts(
        accounts: &HashMap<String, KimiAccountData>,
        default_account_id: Option<&str>,
    ) -> Vec<KimiOAuthAccount> {
        let mut result: Vec<_> = accounts.values().map(KimiOAuthAccount::from).collect();
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

    fn load_from_disk_sync(&self) -> Result<(), KimiOAuthError> {
        if !self.storage_path.exists() {
            return Ok(());
        }
        let content = fs::read_to_string(&self.storage_path)?;
        let store: KimiOAuthStore = serde_json::from_str(&content)
            .map_err(|error| KimiOAuthError::ParseError(error.to_string()))?;
        if let Ok(mut accounts) = self.accounts.try_write() {
            *accounts = store.accounts;
        }
        if let Ok(mut default_account_id) = self.default_account_id.try_write() {
            *default_account_id = store.default_account_id;
        }
        Ok(())
    }

    fn write_store_atomic(&self, content: &str) -> Result<(), KimiOAuthError> {
        write_oauth_store_atomic(&self.storage_path, content)
            .map_err(|error| KimiOAuthError::IoError(error.to_string()))
    }

    fn kimi_headers(&self) -> reqwest::header::HeaderMap {
        kimi_request_headers(&self.device_id)
    }
}

pub fn kimi_oauth_upstream_base_url() -> String {
    #[cfg(test)]
    {
        if let Ok(url) = std::env::var("CC_SWITCH_TEST_KIMI_OAUTH_BASE_URL") {
            let url = url.trim().trim_end_matches('/');
            if !url.is_empty() {
                return url.to_string();
            }
        }
    }
    KIMI_CODING_API_BASE_URL.to_string()
}

fn kimi_oauth_host() -> String {
    #[cfg(test)]
    {
        if let Ok(host) = std::env::var("CC_SWITCH_TEST_KIMI_OAUTH_HOST") {
            let host = host.trim().trim_end_matches('/');
            if !host.is_empty() {
                return host.to_string();
            }
        }
    }
    KIMI_OAUTH_HOST.to_string()
}

fn load_or_create_device_id(data_dir: &std::path::Path) -> String {
    let path = data_dir.join("kimi-device-id");
    if let Ok(existing) = fs::read_to_string(&path) {
        let existing = existing.trim();
        if !existing.is_empty() {
            return existing.to_string();
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    let _ = fs::create_dir_all(data_dir);
    let _ = fs::write(&path, &id);
    id
}

fn kimi_request_headers(device_id: &str) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Ok(value) = reqwest::header::HeaderValue::from_str(KIMI_USER_AGENT) {
        headers.insert(reqwest::header::USER_AGENT, value);
    }
    if let Ok(value) = reqwest::header::HeaderValue::from_str(KIMI_MSH_PLATFORM) {
        headers.insert("X-Msh-Platform", value);
    }
    if let Ok(value) = reqwest::header::HeaderValue::from_str(KIMI_MSH_VERSION) {
        headers.insert("X-Msh-Version", value);
    }
    if let Ok(value) = reqwest::header::HeaderValue::from_str("cc-switch") {
        headers.insert("X-Msh-Device-Name", value.clone());
        headers.insert("X-Msh-Device-Model", value);
    }
    if let Ok(value) = reqwest::header::HeaderValue::from_str(std::env::consts::OS) {
        headers.insert("X-Msh-Os-Version", value);
    }
    if let Ok(value) = reqwest::header::HeaderValue::from_str(device_id) {
        headers.insert("X-Msh-Device-Id", value);
    }
    headers
}

fn default_poll_interval() -> u64 {
    5
}

fn compute_expires_at_ms(expires_in: Option<i64>) -> Result<i64, KimiOAuthError> {
    match expires_in {
        Some(secs) if secs < 0 => Err(KimiOAuthError::ParseError(
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

fn validate_access_token(access_token: &str) -> Result<(), KimiOAuthError> {
    if access_token.trim().is_empty() {
        return Err(KimiOAuthError::TokenFetchFailed(
            "成功响应缺少 access_token".to_string(),
        ));
    }
    Ok(())
}

fn parse_device_code_response(
    value: serde_json::Value,
) -> Result<DeviceCodeResponse, KimiOAuthError> {
    serde_json::from_value(value)
        .map_err(|_| KimiOAuthError::ParseError("设备授权响应字段无效".to_string()))
}

fn parse_token_response(value: serde_json::Value) -> Result<OAuthTokenResponse, KimiOAuthError> {
    serde_json::from_value(value)
        .map_err(|_| KimiOAuthError::ParseError("OAuth Token 响应字段无效".to_string()))
}

fn refresh_response_requires_reauth(
    status: reqwest::StatusCode,
    response_body_is_invalid: bool,
) -> bool {
    matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    ) || (status == reqwest::StatusCode::BAD_REQUEST && response_body_is_invalid)
}

fn parse_jwt_claims(token: &str) -> Option<KimiTokenClaims> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&decoded).ok()
}

pub(crate) fn extract_kimi_identity(
    access_token: &str,
    refresh_token: Option<&str>,
) -> Option<(String, Option<String>)> {
    let access_claims = parse_jwt_claims(access_token);
    let refresh_claims = refresh_token.and_then(parse_jwt_claims);
    let account_id = access_claims
        .as_ref()
        .and_then(|claims| claims.user_id.clone().or(claims.sub.clone()))
        .or_else(|| {
            refresh_claims
                .as_ref()
                .and_then(|claims| claims.user_id.clone().or(claims.sub.clone()))
        })
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())?;
    let login = access_claims
        .as_ref()
        .and_then(|claims| claims.email.clone())
        .or_else(|| refresh_claims.and_then(|claims| claims.email))
        .map(|email| email.trim().to_ascii_lowercase())
        .filter(|email| !email.is_empty());
    Some((account_id, login))
}

async fn read_json_response(
    response: reqwest::Response,
) -> Result<serde_json::Value, KimiOAuthError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_OAUTH_RESPONSE_BYTES as u64)
    {
        return Err(KimiOAuthError::ParseError(
            "OAuth 响应超过大小限制".to_string(),
        ));
    }
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_OAUTH_RESPONSE_BYTES {
        return Err(KimiOAuthError::ParseError(
            "OAuth 响应超过大小限制".to_string(),
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| KimiOAuthError::ParseError("OAuth 响应不是有效 JSON".to_string()))
}

fn oauth_error_code(value: &serde_json::Value) -> Option<String> {
    value
        .get("error")
        .and_then(serde_json::Value::as_str)
        .map(sanitize_oauth_error_code)
        .filter(|value| !value.is_empty())
}

fn sanitize_oauth_error_code(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || "_.-".contains(*character))
        .take(64)
        .collect()
}

fn format_oauth_error(status: reqwest::StatusCode, value: &serde_json::Value) -> String {
    let code = oauth_error_code(value).unwrap_or_else(|| "unknown".to_string());
    format!("HTTP {status}: {code}")
}

pub(crate) fn write_oauth_store_atomic(
    storage_path: &std::path::Path,
    content: &str,
) -> Result<(), std::io::Error> {
    let parent = storage_path
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "无效的存储路径"))?;
    fs::create_dir_all(parent)?;
    let file_name = storage_path
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "无效的存储文件名"))?
        .to_string_lossy();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary_path = parent.join(format!("{file_name}.tmp.{nonce}"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let result = (|| -> Result<(), std::io::Error> {
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&temporary_path)?;
            file.write_all(content.as_bytes())?;
            file.flush()?;
            fs::rename(&temporary_path, storage_path)?;
            fs::set_permissions(storage_path, fs::Permissions::from_mode(0o600))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary_path);
        }
        result?;
    }

    #[cfg(windows)]
    {
        let result = (|| -> Result<(), std::io::Error> {
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary_path)?;
            file.write_all(content.as_bytes())?;
            file.flush()?;
            if storage_path.exists() {
                fs::remove_file(storage_path)?;
            }
            fs::rename(&temporary_path, storage_path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary_path);
        }
        result?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt_with_payload(payload: &str) -> String {
        let encoded = URL_SAFE_NO_PAD.encode(payload.as_bytes());
        format!("aaa.{encoded}.sig")
    }

    #[test]
    fn identity_prefers_user_id_and_lowercases_email() {
        let token = jwt_with_payload(r#"{"user_id":"u-1","sub":"ignored","email":"Ada@Kimi.COM"}"#);
        let (id, login) = extract_kimi_identity(&token, None).unwrap();
        assert_eq!(id, "u-1");
        assert_eq!(login.as_deref(), Some("ada@kimi.com"));
    }

    #[test]
    fn identity_falls_back_to_sub_then_refresh_token() {
        let access = jwt_with_payload(r#"{"email":"a@b.com"}"#);
        let refresh = jwt_with_payload(r#"{"sub":"from-refresh"}"#);
        let (id, _) = extract_kimi_identity(&access, Some(&refresh)).unwrap();
        assert_eq!(id, "from-refresh");
    }

    #[test]
    fn expires_in_must_not_be_negative() {
        assert!(compute_expires_at_ms(Some(-1)).is_err());
        assert!(compute_expires_at_ms(Some(0)).is_ok());
        assert!(compute_expires_at_ms(None).is_ok());
    }

    #[tokio::test]
    async fn account_store_round_trips_and_persists_reauth_state() {
        let data_dir = tempfile::tempdir().unwrap();
        let manager = KimiOAuthManager::new(data_dir.path().to_path_buf());
        manager
            .add_account_internal(
                "account-one".to_string(),
                Some("one@example.com".to_string()),
                "refresh-one".to_string(),
                None,
                None,
            )
            .await
            .unwrap();
        manager
            .add_account_internal(
                "account-two".to_string(),
                Some("two@example.com".to_string()),
                "refresh-two".to_string(),
                None,
                None,
            )
            .await
            .unwrap();
        manager.set_default_account("account-one").await.unwrap();

        let reloaded = KimiOAuthManager::new(data_dir.path().to_path_buf());
        let status = reloaded.get_status().await;
        assert_eq!(status.accounts.len(), 2);
        assert_eq!(status.default_account_id.as_deref(), Some("account-one"));

        reloaded.mark_reauth_required("account-one").await.unwrap();
        let after_reauth = KimiOAuthManager::new(data_dir.path().to_path_buf())
            .get_status()
            .await;
        assert_eq!(
            after_reauth.default_account_id.as_deref(),
            Some("account-two")
        );
        assert!(after_reauth
            .accounts
            .iter()
            .find(|account| account.id == "account-one")
            .is_some_and(|account| account.requires_reauth));
    }

    #[tokio::test]
    async fn test_account_injects_cached_access_token() {
        let data_dir = tempfile::tempdir().unwrap();
        let manager = KimiOAuthManager::new(data_dir.path().to_path_buf());
        manager
            .add_test_account_with_access_token("acct-a", "kimi-token-a", Some("a@kimi.com"))
            .await
            .unwrap();
        assert_eq!(
            manager.get_valid_token_for_account("acct-a").await.unwrap(),
            "kimi-token-a"
        );
        assert_eq!(manager.get_valid_token().await.unwrap(), "kimi-token-a");
    }
}
