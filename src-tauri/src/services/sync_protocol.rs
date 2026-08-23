//! Transport-agnostic sync protocol layer.
//!
//! Shared by WebDAV, S3, and future transports. Artifact set: `db.sql` + `skills.zip`.

use std::collections::BTreeMap;
use std::fs;
use std::future::Future;
use std::process::Command;
use std::sync::OnceLock;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::tempdir;

use crate::error::AppError;
use crate::services::skill::{skill_state_read_guard, skill_state_write_guard};

// Re-export archive functions for use by transport layers.
pub(crate) use super::webdav_sync::archive::{
    backup_current_skills, restore_skills_from_backup, restore_skills_zip, zip_skills_ssot,
};

// ─── Protocol constants ──────────────────────────────────────

/// Wire-format identifier stored in remote manifests.
/// Retains historic "webdav" naming for backward compatibility with existing remotes.
pub(crate) const PROTOCOL_FORMAT: &str = "cc-switch-webdav-sync";
pub(crate) const PROTOCOL_VERSION: u32 = 2;
pub(crate) const DB_COMPAT_VERSION: u32 = 6;
pub(crate) const LEGACY_DB_COMPAT_VERSION: u32 = 5;
pub(crate) const REMOTE_DB_SQL: &str = "db.sql";
pub(crate) const REMOTE_SKILLS_ZIP: &str = "skills.zip";
pub(crate) const REMOTE_MANIFEST: &str = "manifest.json";
pub(crate) const MAX_DEVICE_NAME_LEN: usize = 64;
pub(crate) const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_SYNC_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;

// ─── Sync operation lock ────────────────────────────────────

/// Serialize every snapshot upload/download across all transports.
///
/// WebDAV and S3 used to own separate mutexes, which allowed two transports to
/// restore the database and Skills SSOT concurrently. Keep the lock in this
/// transport-agnostic layer so future transports automatically share it too.
pub(crate) fn sync_mutex() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

pub(crate) async fn run_with_sync_lock<T, Fut>(operation: Fut) -> Result<T, AppError>
where
    Fut: Future<Output = Result<T, AppError>>,
{
    let _guard = sync_mutex().lock().await;
    operation.await
}

/// Tables whose changes make the remote configuration snapshot stale.
///
/// Keep this transport-agnostic so WebDAV and S3 cannot silently drift apart.
/// `model_pricing` is intentionally excluded while its local JSON sidecar is
/// the user-owned SSOT.
pub(crate) fn should_trigger_auto_sync_for_table(table: &str) -> bool {
    let normalized = table.trim().to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "providers"
            | "provider_endpoints"
            | "mcp_servers"
            | "prompts"
            | "skills"
            | "skill_repos"
            | "profiles"
            | "settings"
            | "proxy_config"
    )
}

// ─── Error helpers ───────────────────────────────────────────

pub(crate) fn localized(
    key: &'static str,
    zh: impl Into<String>,
    en: impl Into<String>,
) -> AppError {
    AppError::localized(key, zh, en)
}

pub(crate) fn io_context_localized(
    _key: &'static str,
    zh: impl Into<String>,
    en: impl Into<String>,
    source: std::io::Error,
) -> AppError {
    let zh_msg = zh.into();
    let en_msg = en.into();
    AppError::IoContext {
        context: format!("{zh_msg} ({en_msg})"),
        source,
    }
}

// ─── Types ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SyncManifest {
    pub format: String,
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db_compat_version: Option<u32>,
    pub device_name: String,
    pub created_at: String,
    pub artifacts: BTreeMap<String, ArtifactMeta>,
    pub snapshot_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ArtifactMeta {
    pub sha256: String,
    pub size: u64,
}

pub(crate) struct LocalSnapshot {
    pub db_sql: Vec<u8>,
    pub skills_zip: Vec<u8>,
    pub manifest_bytes: Vec<u8>,
    pub manifest_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteLayout {
    Current,
    Legacy,
}

impl RemoteLayout {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Legacy => "legacy",
        }
    }
}

// ─── Snapshot building ───────────────────────────────────────

pub(crate) fn build_local_snapshot(
    db: &crate::database::Database,
) -> Result<LocalSnapshot, AppError> {
    // Keep the DB's skill rows and the filesystem SSOT at one logical point in
    // time. Skill writers take the matching write guard around both mutations.
    let _skill_state_guard = skill_state_read_guard();

    // Export database to SQL string
    let sql_string = db.export_sql_string_for_sync()?;
    let db_sql = sql_string.into_bytes();

    // Pack skills into deterministic ZIP
    let tmp = tempdir().map_err(|e| {
        io_context_localized(
            "sync.snapshot_tmpdir_failed",
            "创建快照临时目录失败",
            "Failed to create temporary directory for snapshot",
            e,
        )
    })?;
    let skills_zip_path = tmp.path().join(REMOTE_SKILLS_ZIP);
    zip_skills_ssot(&skills_zip_path)?;
    let skills_zip = fs::read(&skills_zip_path).map_err(|e| AppError::io(&skills_zip_path, e))?;

    // Build artifact map and compute hashes
    let mut artifacts = BTreeMap::new();
    artifacts.insert(
        REMOTE_DB_SQL.to_string(),
        ArtifactMeta {
            sha256: sha256_hex(&db_sql),
            size: db_sql.len() as u64,
        },
    );
    artifacts.insert(
        REMOTE_SKILLS_ZIP.to_string(),
        ArtifactMeta {
            sha256: sha256_hex(&skills_zip),
            size: skills_zip.len() as u64,
        },
    );

    let snapshot_id = compute_snapshot_id(&artifacts);
    let manifest = SyncManifest {
        format: PROTOCOL_FORMAT.to_string(),
        version: PROTOCOL_VERSION,
        db_compat_version: Some(DB_COMPAT_VERSION),
        device_name: detect_system_device_name().unwrap_or_else(|| "Unknown Device".to_string()),
        created_at: Utc::now().to_rfc3339(),
        artifacts,
        snapshot_id,
    };
    let manifest_bytes =
        serde_json::to_vec_pretty(&manifest).map_err(|e| AppError::JsonSerialize { source: e })?;
    let manifest_hash = sha256_hex(&manifest_bytes);

    Ok(LocalSnapshot {
        db_sql,
        skills_zip,
        manifest_bytes,
        manifest_hash,
    })
}

// ─── Manifest handling ───────────────────────────────────────

/// Compute a deterministic snapshot identity from artifact hashes.
///
/// BTreeMap iteration order is sorted by key, ensuring stability.
pub(crate) fn compute_snapshot_id(artifacts: &BTreeMap<String, ArtifactMeta>) -> String {
    let parts: Vec<String> = artifacts
        .iter()
        .map(|(name, meta)| format!("{}:{}", name, meta.sha256))
        .collect();
    sha256_hex(parts.join("|").as_bytes())
}

pub(crate) fn effective_db_compat_version(
    manifest: &SyncManifest,
    layout: RemoteLayout,
) -> Option<u32> {
    manifest
        .db_compat_version
        .or_else(|| (layout == RemoteLayout::Legacy).then_some(LEGACY_DB_COMPAT_VERSION))
}

pub(crate) fn validate_manifest_compat(
    manifest: &SyncManifest,
    layout: RemoteLayout,
) -> Result<(), AppError> {
    if manifest.format != PROTOCOL_FORMAT {
        return Err(localized(
            "sync.manifest_format_incompatible",
            format!("远端 manifest 格式不兼容: {}", manifest.format),
            format!(
                "Remote manifest format is incompatible: {}",
                manifest.format
            ),
        ));
    }
    if manifest.version != PROTOCOL_VERSION {
        return Err(localized(
            "sync.manifest_version_incompatible",
            format!(
                "远端 manifest 协议版本不兼容: v{} (本地 v{PROTOCOL_VERSION})",
                manifest.version
            ),
            format!(
                "Remote manifest protocol version is incompatible: v{} (local v{PROTOCOL_VERSION})",
                manifest.version
            ),
        ));
    }
    let Some(db_compat_version) = effective_db_compat_version(manifest, layout) else {
        return Err(localized(
            "sync.manifest_db_version_missing",
            "远端 manifest 缺少数据库兼容版本",
            "Remote manifest is missing the database compatibility version.",
        ));
    };
    match layout {
        RemoteLayout::Current if db_compat_version != DB_COMPAT_VERSION => {
            return Err(localized(
                "sync.manifest_db_version_incompatible",
                format!(
                    "远端数据库快照版本不兼容: db-v{db_compat_version} (本地 db-v{DB_COMPAT_VERSION})"
                ),
                format!(
                    "Remote database snapshot version is incompatible: db-v{db_compat_version} (local db-v{DB_COMPAT_VERSION})"
                ),
            ));
        }
        RemoteLayout::Legacy if db_compat_version > DB_COMPAT_VERSION => {
            return Err(localized(
                "sync.manifest_db_version_incompatible",
                format!(
                    "远端数据库快照版本不兼容: db-v{db_compat_version} (本地最高支持 db-v{DB_COMPAT_VERSION})"
                ),
                format!(
                    "Remote database snapshot version is incompatible: db-v{db_compat_version} (local supports up to db-v{DB_COMPAT_VERSION})"
                ),
            ));
        }
        _ => {}
    }
    Ok(())
}

// ─── Artifact verification ───────────────────────────────────

pub(crate) fn validate_artifact_size_limit(artifact_name: &str, size: u64) -> Result<(), AppError> {
    if size > MAX_SYNC_ARTIFACT_BYTES {
        let max_mb = MAX_SYNC_ARTIFACT_BYTES / 1024 / 1024;
        return Err(localized(
            "sync.artifact_too_large",
            format!("artifact {artifact_name} 超过下载上限（{} MB）", max_mb),
            format!(
                "Artifact {artifact_name} exceeds download limit ({} MB)",
                max_mb
            ),
        ));
    }
    Ok(())
}

/// Verify that downloaded artifact bytes match the expected size and SHA-256 hash.
pub(crate) fn verify_artifact(
    bytes: &[u8],
    artifact_name: &str,
    meta: &ArtifactMeta,
) -> Result<(), AppError> {
    // Quick size check before expensive hash
    if bytes.len() as u64 != meta.size {
        return Err(localized(
            "sync.artifact_size_mismatch",
            format!(
                "artifact {artifact_name} 大小不匹配 (expected: {}, got: {})",
                meta.size,
                bytes.len(),
            ),
            format!(
                "Artifact {artifact_name} size mismatch (expected: {}, got: {})",
                meta.size,
                bytes.len(),
            ),
        ));
    }

    let actual_hash = sha256_hex(bytes);
    if actual_hash != meta.sha256 {
        return Err(localized(
            "sync.artifact_hash_mismatch",
            format!(
                "artifact {artifact_name} SHA256 校验失败 (expected: {}..., got: {}...)",
                meta.sha256.get(..8).unwrap_or(&meta.sha256),
                actual_hash.get(..8).unwrap_or(&actual_hash),
            ),
            format!(
                "Artifact {artifact_name} SHA256 verification failed (expected: {}..., got: {}...)",
                meta.sha256.get(..8).unwrap_or(&meta.sha256),
                actual_hash.get(..8).unwrap_or(&actual_hash),
            ),
        ));
    }
    Ok(())
}

// ─── Snapshot application ────────────────────────────────────

pub(crate) fn apply_snapshot(
    db: &crate::database::Database,
    db_sql: &[u8],
    skills_zip: &[u8],
) -> Result<(), AppError> {
    let sql_str = std::str::from_utf8(db_sql).map_err(|e| {
        localized(
            "sync.sql_not_utf8",
            format!("SQL 非 UTF-8: {e}"),
            format!("SQL is not valid UTF-8: {e}"),
        )
    })?;
    // Exclude installs, uninstalls, updates, and local projection while Skills
    // are backed up/replaced and the corresponding database snapshot is applied.
    let _skill_state_guard = skill_state_write_guard();
    let skills_backup = backup_current_skills()?;

    // Replace skills first, then import database; roll back skills on DB failure.
    restore_skills_zip(skills_zip)?;

    if let Err(db_err) = db.import_sql_string_for_sync(sql_str) {
        if let Err(rollback_err) = restore_skills_from_backup(&skills_backup) {
            return Err(localized(
                "sync.db_import_and_rollback_failed",
                format!("导入数据库失败: {db_err}; 同时回滚 Skills 失败: {rollback_err}"),
                format!(
                    "Database import failed: {db_err}; skills rollback also failed: {rollback_err}"
                ),
            ));
        }
        return Err(db_err);
    }

    Ok(())
}

// ─── Utilities ───────────────────────────────────────────────

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

pub(crate) fn detect_system_device_name() -> Option<String> {
    let env_name = ["CC_SWITCH_DEVICE_NAME", "COMPUTERNAME", "HOSTNAME"]
        .iter()
        .filter_map(|key| std::env::var(key).ok())
        .find_map(|value| normalize_device_name(&value));

    if env_name.is_some() {
        return env_name;
    }

    let output = Command::new("hostname").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let hostname = String::from_utf8(output.stdout).ok()?;
    normalize_device_name(&hostname)
}

pub(crate) fn normalize_device_name(raw: &str) -> Option<String> {
    let compact = raw
        .chars()
        .fold(String::with_capacity(raw.len()), |mut acc, ch| {
            if ch.is_whitespace() {
                acc.push(' ');
            } else if !ch.is_control() {
                acc.push(ch);
            }
            acc
        });
    let normalized = compact.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = normalized.trim();
    if trimmed.is_empty() {
        return None;
    }

    let limited = trimmed
        .chars()
        .take(MAX_DEVICE_NAME_LEN)
        .collect::<String>();
    if limited.is_empty() {
        None
    } else {
        Some(limited)
    }
}

// ─── Sync status persistence ─────────────────────────────────

pub(crate) fn persist_sync_success_best_effort<S, F>(
    settings: &mut S,
    manifest_hash: String,
    etag: Option<String>,
    persist_fn: F,
) -> bool
where
    F: FnOnce(&mut S, String, Option<String>) -> Result<(), AppError>,
{
    match persist_fn(settings, manifest_hash, etag) {
        Ok(()) => true,
        Err(err) => {
            log::warn!("[Sync] Persist sync status failed, keep operation success: {err}");
            false
        }
    }
}

// ─── Auto-sync upload guard ──────────────────────────────────

/// Treat blank / whitespace-only cursors as "never synced".
fn nonempty_cursor(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|s| !s.is_empty())
}

/// Strip weak-validator prefix and surrounding quotes so `"abc"` equals `abc`.
pub(crate) fn normalize_etag(raw: &str) -> String {
    let trimmed = raw.trim();
    let without_weak = trimmed
        .strip_prefix("W/")
        .or_else(|| trimmed.strip_prefix("w/"))
        .unwrap_or(trimmed)
        .trim();
    without_weak.trim_matches('"').to_string()
}

fn etags_equal(left: &str, right: &str) -> bool {
    normalize_etag(left) == normalize_etag(right)
}

fn hashes_equal(left: &str, right: &str) -> bool {
    left.trim().eq_ignore_ascii_case(right.trim())
}

fn auto_upload_download_first_error() -> AppError {
    localized(
        "sync.auto_upload.download_first",
        "远端已有同步数据，请先下载后再开启自动同步，以免覆盖远端配置",
        "Remote already has sync data. Download it first before enabling auto-sync to avoid overwriting the remote.",
    )
}

fn auto_upload_conflict_error() -> AppError {
    localized(
        "sync.auto_upload.conflict",
        "远端数据已变更，自动上传已取消。请先下载或使用手动上传。",
        "Remote data has changed; auto-upload aborted. Download first or use manual upload.",
    )
}

pub(crate) fn auto_upload_missing_etag_error() -> AppError {
    localized(
        "sync.auto_upload.missing_etag",
        "远端已有数据但服务器未返回 ETag，自动上传已取消，以免覆盖远端。",
        "Remote already has data but the server omitted ETag; auto-upload aborted to avoid overwriting the remote.",
    )
}

/// Fail-closed PUT decision for a single auto-sync object (artifact or manifest).
///
/// Manual upload is [`ResolvedPut::Unconditional`] and may overwrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResolvedPut<'a> {
    Unconditional,
    IfMatch(&'a str),
    IfNoneMatchAny,
}

/// Resolve If-Match / If-None-Match / abort for one PUT.
///
/// Auto-sync never returns [`ResolvedPut::Unconditional`]. If the remote object
/// exists but HEAD/GET omitted a usable ETag, this is a conflict.
pub(crate) fn resolve_put_precondition<'a>(
    conditional: bool,
    remote_exists: bool,
    fresh_etag: Option<&'a str>,
) -> Result<ResolvedPut<'a>, AppError> {
    if !conditional {
        return Ok(ResolvedPut::Unconditional);
    }
    if !remote_exists {
        return Ok(ResolvedPut::IfNoneMatchAny);
    }
    match nonempty_cursor(fresh_etag) {
        Some(etag) => Ok(ResolvedPut::IfMatch(etag)),
        None => Err(auto_upload_missing_etag_error()),
    }
}

/// Require a usable ETag from the HEAD/GET just performed (never the stored cursor).
pub(crate) fn require_if_match_etag(fresh_etag: Option<&str>) -> Result<&str, AppError> {
    nonempty_cursor(fresh_etag).ok_or_else(auto_upload_missing_etag_error)
}

/// Run snapshot PUTs in order (artifacts then manifest). Stop on the first error
/// so a 412 cannot proceed to remaining objects.
pub(crate) fn put_in_order<T, F>(
    names: impl IntoIterator<Item = T>,
    mut put: F,
) -> Result<(), AppError>
where
    F: FnMut(T) -> Result<(), AppError>,
{
    for name in names {
        put(name)?;
    }
    Ok(())
}

/// Whether a partial-upload rollback should DELETE the object.
///
/// First-upload (`existed_before == false`): DELETE the newly created object so
/// a later artifact/manifest failure cannot leave a half-snapshot.
/// Overwrite (`existed_before == true`): do **not** DELETE — that would destroy
/// the last good remote snapshot. Callers should PUT previous bytes back when
/// they captured them; otherwise skip DELETE.
pub(crate) fn rollback_partial_upload(existed_before: bool) -> bool {
    !existed_before
}

/// Decide whether auto-sync may upload the local snapshot.
///
/// - Empty remote: allow (first seed).
/// - Remote exists and local has never synced (`last_remote_etag` and
///   `last_remote_manifest_hash` both empty): refuse so a second device cannot
///   last-write-wins over an existing remote.
/// - Remote exists and local has a cursor: allow only when etag and/or hash
///   match; any mismatch is a conflict.
pub(crate) fn should_allow_auto_upload(
    local_etag: Option<&str>,
    local_hash: Option<&str>,
    remote_exists: bool,
    remote_etag: Option<&str>,
    remote_hash: Option<&str>,
) -> Result<(), AppError> {
    let local_etag = nonempty_cursor(local_etag);
    let local_hash = nonempty_cursor(local_hash);
    let remote_etag = nonempty_cursor(remote_etag);
    let remote_hash = nonempty_cursor(remote_hash);

    if !remote_exists {
        return Ok(());
    }

    if local_etag.is_none() && local_hash.is_none() {
        return Err(auto_upload_download_first_error());
    }

    let etag_match = match (local_etag, remote_etag) {
        (Some(local), Some(remote)) => Some(etags_equal(local, remote)),
        _ => None,
    };
    let hash_match = match (local_hash, remote_hash) {
        (Some(local), Some(remote)) => Some(hashes_equal(local, remote)),
        _ => None,
    };

    if etag_match == Some(false) || hash_match == Some(false) {
        return Err(auto_upload_conflict_error());
    }
    if etag_match == Some(true) || hash_match == Some(true) {
        return Ok(());
    }

    // Local has a cursor but nothing was comparable (server omitted ETag and
    // we could not hash the remote). Refuse rather than last-write-wins.
    Err(auto_upload_conflict_error())
}

// ─── Tests ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn webdav_and_s3_operations_share_one_sync_mutex() {
        let webdav_lock = crate::services::webdav_sync::sync_mutex();
        let s3_lock = crate::services::s3_sync::sync_mutex();
        assert!(
            std::ptr::eq(webdav_lock, s3_lock),
            "every transport must expose the same global sync lock"
        );

        let guard = webdav_lock.lock().await;
        assert!(s3_lock.try_lock().is_err());
        drop(guard);
        assert!(s3_lock.try_lock().is_ok());
    }

    fn artifact(sha256: &str, size: u64) -> ArtifactMeta {
        ArtifactMeta {
            sha256: sha256.to_string(),
            size,
        }
    }

    #[test]
    fn auto_sync_table_filter_covers_shared_configuration() {
        for table in [
            "providers",
            "provider_endpoints",
            "mcp_servers",
            "prompts",
            "skills",
            "skill_repos",
            "profiles",
            "settings",
            "proxy_config",
        ] {
            assert!(
                should_trigger_auto_sync_for_table(table),
                "{table} should trigger an automatic snapshot upload"
            );
        }

        assert!(should_trigger_auto_sync_for_table("  PROFILES  "));
        for table in [
            "proxy_request_logs",
            "provider_health",
            "session_log_sync",
            "model_pricing",
        ] {
            assert!(
                !should_trigger_auto_sync_for_table(table),
                "{table} should not trigger automatic snapshot upload"
            );
        }
    }

    #[test]
    fn snapshot_id_is_stable() {
        let mut artifacts = BTreeMap::new();
        artifacts.insert("db.sql".to_string(), artifact("abc123", 100));
        artifacts.insert("skills.zip".to_string(), artifact("def456", 200));

        let id1 = compute_snapshot_id(&artifacts);
        let id2 = compute_snapshot_id(&artifacts);
        assert_eq!(id1, id2);
    }

    #[test]
    fn snapshot_id_changes_with_artifacts() {
        let mut a1 = BTreeMap::new();
        a1.insert("db.sql".to_string(), artifact("hash-a", 1));

        let mut a2 = BTreeMap::new();
        a2.insert("db.sql".to_string(), artifact("hash-b", 1));

        assert_ne!(compute_snapshot_id(&a1), compute_snapshot_id(&a2));
    }

    #[test]
    fn sha256_hex_is_correct() {
        let hash = sha256_hex(b"hello");
        assert_eq!(
            hash,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn persist_best_effort_returns_true_on_success() {
        let mut dummy = ();
        let ok = persist_sync_success_best_effort(
            &mut dummy,
            "hash".to_string(),
            Some("etag".to_string()),
            |_settings, _hash, _etag| Ok(()),
        );
        assert!(ok);
    }

    #[test]
    fn persist_best_effort_returns_false_on_error() {
        let mut dummy = ();
        let ok = persist_sync_success_best_effort(
            &mut dummy,
            "hash".to_string(),
            None,
            |_settings, _hash, _etag| Err(AppError::Config("boom".to_string())),
        );
        assert!(!ok);
    }

    fn manifest_with(format: &str, version: u32, db_compat_version: Option<u32>) -> SyncManifest {
        let mut artifacts = BTreeMap::new();
        artifacts.insert("db.sql".to_string(), artifact("abc", 1));
        artifacts.insert("skills.zip".to_string(), artifact("def", 2));
        SyncManifest {
            format: format.to_string(),
            version,
            db_compat_version,
            device_name: "My MacBook".to_string(),
            created_at: "2026-02-12T00:00:00Z".to_string(),
            artifacts,
            snapshot_id: "snap-1".to_string(),
        }
    }

    #[test]
    fn validate_manifest_compat_accepts_supported_manifest() {
        let manifest = manifest_with(PROTOCOL_FORMAT, PROTOCOL_VERSION, Some(DB_COMPAT_VERSION));
        assert!(validate_manifest_compat(&manifest, RemoteLayout::Current).is_ok());
    }

    #[test]
    fn validate_manifest_compat_rejects_wrong_format() {
        let manifest = manifest_with("other-format", PROTOCOL_VERSION, Some(DB_COMPAT_VERSION));
        assert!(validate_manifest_compat(&manifest, RemoteLayout::Current).is_err());
    }

    #[test]
    fn validate_manifest_compat_rejects_wrong_version() {
        let manifest = manifest_with(
            PROTOCOL_FORMAT,
            PROTOCOL_VERSION + 1,
            Some(DB_COMPAT_VERSION),
        );
        assert!(validate_manifest_compat(&manifest, RemoteLayout::Current).is_err());
    }

    #[test]
    fn validate_manifest_compat_accepts_legacy_manifest_without_db_compat() {
        let manifest = manifest_with(PROTOCOL_FORMAT, PROTOCOL_VERSION, None);
        assert!(validate_manifest_compat(&manifest, RemoteLayout::Legacy).is_ok());
    }

    #[test]
    fn validate_manifest_compat_rejects_current_manifest_with_wrong_db_compat() {
        let manifest = manifest_with(
            PROTOCOL_FORMAT,
            PROTOCOL_VERSION,
            Some(LEGACY_DB_COMPAT_VERSION),
        );
        assert!(validate_manifest_compat(&manifest, RemoteLayout::Current).is_err());
    }

    #[test]
    fn validate_manifest_compat_rejects_legacy_manifest_from_newer_db_generation() {
        let manifest = manifest_with(
            PROTOCOL_FORMAT,
            PROTOCOL_VERSION,
            Some(DB_COMPAT_VERSION + 1),
        );
        assert!(validate_manifest_compat(&manifest, RemoteLayout::Legacy).is_err());
    }

    #[test]
    fn effective_db_compat_version_defaults_legacy_layout_to_v5() {
        let manifest = manifest_with(PROTOCOL_FORMAT, PROTOCOL_VERSION, None);
        assert_eq!(
            effective_db_compat_version(&manifest, RemoteLayout::Legacy),
            Some(LEGACY_DB_COMPAT_VERSION)
        );
        assert_eq!(
            effective_db_compat_version(&manifest, RemoteLayout::Current),
            None
        );
    }

    #[test]
    fn normalize_device_name_returns_none_for_blank_input() {
        assert_eq!(normalize_device_name("   \n\t  "), None);
    }

    #[test]
    fn normalize_device_name_collapses_whitespace_and_drops_control_chars() {
        assert_eq!(
            normalize_device_name("  Mac\tBook \n Pro\u{0007} "),
            Some("Mac Book Pro".to_string())
        );
    }

    #[test]
    fn normalize_device_name_truncates_to_max_len() {
        let long = "a".repeat(80);
        assert_eq!(normalize_device_name(&long).map(|s| s.len()), Some(64));
    }

    #[test]
    fn manifest_serialization_uses_device_name_only() {
        let manifest = manifest_with(PROTOCOL_FORMAT, PROTOCOL_VERSION, Some(DB_COMPAT_VERSION));
        let value = serde_json::to_value(&manifest).expect("serialize manifest");
        assert!(
            value.get("deviceName").is_some(),
            "manifest should contain deviceName"
        );
        assert_eq!(
            value.get("dbCompatVersion").and_then(|v| v.as_u64()),
            Some(DB_COMPAT_VERSION as u64)
        );
        assert!(
            value.get("deviceId").is_none(),
            "manifest should not contain deviceId"
        );
    }

    #[test]
    fn validate_artifact_size_limit_rejects_oversized_artifacts() {
        let err = validate_artifact_size_limit("skills.zip", MAX_SYNC_ARTIFACT_BYTES + 1)
            .expect_err("artifact larger than limit should be rejected");
        assert!(
            err.to_string().contains("too large") || err.to_string().contains("超过"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_artifact_size_limit_accepts_limit_boundary() {
        assert!(validate_artifact_size_limit("skills.zip", MAX_SYNC_ARTIFACT_BYTES).is_ok());
    }

    #[test]
    fn verify_artifact_rejects_size_mismatch() {
        let meta = artifact("abc123", 100);
        let bytes = vec![0u8; 50];
        let err = verify_artifact(&bytes, "test.bin", &meta)
            .expect_err("size mismatch should be rejected");
        assert!(
            err.to_string().contains("mismatch") || err.to_string().contains("不匹配"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn verify_artifact_rejects_hash_mismatch() {
        let meta = ArtifactMeta {
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            size: 5,
        };
        let bytes = b"hello";
        let err = verify_artifact(bytes, "test.bin", &meta)
            .expect_err("hash mismatch should be rejected");
        assert!(
            err.to_string().contains("verification failed") || err.to_string().contains("校验失败"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn verify_artifact_accepts_matching_data() {
        let data = b"hello";
        let meta = ArtifactMeta {
            sha256: sha256_hex(data),
            size: data.len() as u64,
        };
        assert!(verify_artifact(data, "test.bin", &meta).is_ok());
    }

    #[test]
    fn auto_upload_allows_empty_remote() {
        assert!(should_allow_auto_upload(None, None, false, None, None).is_ok());
        assert!(should_allow_auto_upload(Some("etag"), Some("hash"), false, None, None).is_ok());
    }

    #[test]
    fn auto_upload_denies_when_remote_exists_and_local_has_no_cursor() {
        let err = should_allow_auto_upload(None, None, true, Some("etag"), Some("hash"))
            .expect_err("fresh device must download first");
        let msg = err.to_string();
        assert!(
            msg.contains("Download") || msg.contains("下载"),
            "unexpected error: {msg}"
        );

        let err = should_allow_auto_upload(Some(""), Some("  "), true, Some("e"), Some("h"))
            .expect_err("blank cursors count as never synced");
        let msg = err.to_string();
        assert!(msg.contains("Download") || msg.contains("下载"));
    }

    #[test]
    fn auto_upload_allows_matching_etag() {
        assert!(should_allow_auto_upload(Some("\"abc\""), None, true, Some("abc"), None).is_ok());
        assert!(should_allow_auto_upload(Some("abc"), None, true, Some("\"abc\""), None).is_ok());
        assert!(should_allow_auto_upload(Some("W/\"abc\""), None, true, Some("abc"), None).is_ok());
    }

    #[test]
    fn auto_upload_allows_matching_hash() {
        assert!(
            should_allow_auto_upload(None, Some("deadbeef"), true, None, Some("DEADBEEF")).is_ok()
        );
    }

    #[test]
    fn auto_upload_denies_etag_mismatch() {
        let err = should_allow_auto_upload(Some("old"), None, true, Some("new"), None)
            .expect_err("etag mismatch is a conflict");
        let msg = err.to_string();
        assert!(
            msg.contains("changed") || msg.contains("变更") || msg.contains("conflict"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn auto_upload_denies_hash_mismatch() {
        let err = should_allow_auto_upload(None, Some("aaa"), true, None, Some("bbb"))
            .expect_err("hash mismatch is a conflict");
        let msg = err.to_string();
        assert!(msg.contains("changed") || msg.contains("变更"));
    }

    #[test]
    fn auto_upload_denies_when_etag_matches_but_hash_differs() {
        assert!(
            should_allow_auto_upload(Some("e"), Some("h1"), true, Some("e"), Some("h2")).is_err()
        );
    }

    #[test]
    fn auto_upload_denies_when_local_cursor_is_not_comparable() {
        assert!(should_allow_auto_upload(Some("e"), None, true, None, None).is_err());
        assert!(should_allow_auto_upload(None, Some("h"), true, None, None).is_err());
    }

    #[test]
    fn normalize_etag_strips_quotes_and_weak_prefix() {
        assert_eq!(normalize_etag("\"abc\""), "abc");
        assert_eq!(normalize_etag("abc"), "abc");
        assert_eq!(normalize_etag("W/\"abc\""), "abc");
        assert_eq!(normalize_etag("  \"abc\"  "), "abc");
    }

    #[test]
    fn resolve_put_precondition_manual_is_unconditional() {
        assert_eq!(
            resolve_put_precondition(false, true, Some("etag")).unwrap(),
            ResolvedPut::Unconditional
        );
        assert_eq!(
            resolve_put_precondition(false, true, None).unwrap(),
            ResolvedPut::Unconditional
        );
    }

    #[test]
    fn resolve_put_precondition_auto_if_match_when_etag_present() {
        assert_eq!(
            resolve_put_precondition(true, true, Some("  artifact-etag  ")).unwrap(),
            ResolvedPut::IfMatch("artifact-etag")
        );
    }

    #[test]
    fn resolve_put_precondition_auto_if_none_match_when_missing() {
        assert_eq!(
            resolve_put_precondition(true, false, None).unwrap(),
            ResolvedPut::IfNoneMatchAny
        );
        // A leftover etag must not be used as If-Match when the object is gone.
        assert_eq!(
            resolve_put_precondition(true, false, Some("stale")).unwrap(),
            ResolvedPut::IfNoneMatchAny
        );
    }

    #[test]
    fn resolve_put_precondition_auto_denies_when_exists_without_etag() {
        let err = resolve_put_precondition(true, true, None)
            .expect_err("existing object without ETag must fail closed");
        let msg = err.to_string();
        assert!(
            msg.contains("ETag") || msg.contains("omitted") || msg.contains("未返回"),
            "unexpected error: {msg}"
        );

        let err =
            resolve_put_precondition(true, true, Some("  ")).expect_err("blank ETag is not usable");
        let msg = err.to_string();
        assert!(msg.contains("ETag") || msg.contains("omitted") || msg.contains("未返回"));
    }

    #[test]
    fn require_if_match_etag_uses_fresh_value_only() {
        assert_eq!(require_if_match_etag(Some("\"abc\"")).unwrap(), "\"abc\"");
        assert!(require_if_match_etag(None).is_err());
        assert!(require_if_match_etag(Some("")).is_err());
    }

    #[test]
    fn put_in_order_stops_on_first_error_and_skips_remaining() {
        let mut attempted = Vec::new();
        let err = put_in_order(["db.sql", "skills.zip", "manifest.json"], |name| {
            attempted.push(name);
            if name == "skills.zip" {
                Err(AppError::localized(
                    "webdav.put.precondition_failed",
                    "远端数据已被其他设备更新（412），未覆盖远端。",
                    "Remote data was updated by another client (412 Precondition Failed); remote was not overwritten.",
                ))
            } else {
                Ok(())
            }
        })
        .expect_err("412 on an artifact must abort");

        assert_eq!(attempted, vec!["db.sql", "skills.zip"]);
        assert!(err.to_string().contains("412"));
    }

    #[test]
    fn rollback_partial_upload_deletes_only_newly_created_objects() {
        assert!(
            rollback_partial_upload(false),
            "first-upload rollback should DELETE"
        );
        assert!(
            !rollback_partial_upload(true),
            "overwrite rollback must not DELETE the previous snapshot"
        );
    }
}
