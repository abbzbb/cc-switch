//! S3 v2 sync protocol layer.
//!
//! Implements manifest-based synchronization on top of the S3 transport
//! primitives in [`super::s3`]. Artifact set: `db.sql` + `skills.zip`.

use std::collections::BTreeMap;

use chrono::Utc;
use serde_json::Value;

use crate::error::AppError;
use crate::services::s3::{self, S3Credentials};
use crate::settings::{update_s3_sync_status, S3SyncSettings, WebDavSyncStatus};

pub(crate) use super::sync_protocol::run_with_sync_lock;
use super::sync_protocol::{
    apply_snapshot, build_local_snapshot, localized, persist_sync_success_best_effort,
    require_if_match_etag, resolve_put_precondition, sha256_hex, should_allow_auto_upload,
    validate_artifact_size_limit, validate_manifest_compat, verify_artifact, ArtifactMeta,
    RemoteLayout, ResolvedPut, SyncManifest, DB_COMPAT_VERSION, MAX_MANIFEST_BYTES,
    MAX_SYNC_ARTIFACT_BYTES, PROTOCOL_VERSION, REMOTE_DB_SQL, REMOTE_MANIFEST, REMOTE_SKILLS_ZIP,
};

#[cfg(test)]
pub(crate) fn sync_mutex() -> &'static tokio::sync::Mutex<()> {
    super::sync_protocol::sync_mutex()
}

// ─── Public API ──────────────────────────────────────────────

/// Check S3 connectivity by issuing a HEAD request against the bucket.
pub async fn check_connection(settings: &S3SyncSettings) -> Result<(), AppError> {
    settings.validate()?;
    let creds = creds_for(settings);
    s3::test_connection(&creds).await
}

/// Upload local snapshot (db + skills) to remote S3.
///
/// Manual/explicit UI upload may overwrite the remote.
pub async fn upload(
    db: &crate::database::Database,
    settings: &mut S3SyncSettings,
) -> Result<Value, AppError> {
    upload_snapshot(db, settings, false).await
}

/// Auto-sync upload: fetch the remote first and refuse to last-write-wins.
pub async fn upload_auto(
    db: &crate::database::Database,
    settings: &mut S3SyncSettings,
) -> Result<Value, AppError> {
    upload_snapshot(db, settings, true).await
}

async fn upload_snapshot(
    db: &crate::database::Database,
    settings: &mut S3SyncSettings,
    conditional: bool,
) -> Result<Value, AppError> {
    settings.validate()?;
    let creds = creds_for(settings);

    let manifest_key = s3_key(settings, REMOTE_MANIFEST);
    let mut remote_exists = false;
    let mut if_match_etag: Option<String> = None;

    if conditional {
        let remote = s3::get_object(&creds, &manifest_key, MAX_MANIFEST_BYTES).await?;
        remote_exists = remote.is_some();
        let remote_etag = remote.as_ref().and_then(|(_, etag)| etag.as_deref());
        let remote_hash = remote
            .as_ref()
            .and_then(|(bytes, _etag)| (!bytes.is_empty()).then(|| sha256_hex(bytes)));

        should_allow_auto_upload(
            settings.status.last_remote_etag.as_deref(),
            settings.status.last_remote_manifest_hash.as_deref(),
            remote_exists,
            remote_etag,
            remote_hash.as_deref(),
        )?;

        // Prefer the ETag from this GET, not the stored cursor. Hash-only
        // should_allow_auto_upload can succeed without last_remote_etag.
        if_match_etag = remote
            .as_ref()
            .and_then(|(_bytes, etag)| etag.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
    }

    // Fail closed before any artifact PUT: existing object without ETag
    // must not last-write-wins.
    let manifest_precondition =
        put_precondition(conditional, remote_exists, if_match_etag.as_deref())?;

    let snapshot = build_local_snapshot(db)?;

    // Upload order: artifacts first, manifest last. Any 412 aborts remaining
    // objects. Auto-sync HEADs each artifact and uses If-Match / If-None-Match;
    // manual upload stays overwrite.
    let db_key = s3_key(settings, REMOTE_DB_SQL);
    put_object_for_snapshot(
        &creds,
        &db_key,
        snapshot.db_sql,
        "application/sql",
        conditional,
    )
    .await?;

    let skills_key = s3_key(settings, REMOTE_SKILLS_ZIP);
    put_object_for_snapshot(
        &creds,
        &skills_key,
        snapshot.skills_zip,
        "application/zip",
        conditional,
    )
    .await?;

    s3::put_object_conditional(
        &creds,
        &manifest_key,
        snapshot.manifest_bytes,
        "application/json",
        manifest_precondition,
    )
    .await?;

    // Fetch etag (best-effort, don't fail the upload)
    let etag = match s3::head_object(&creds, &manifest_key).await {
        Ok(e) => e,
        Err(e) => {
            log::debug!("[S3] Failed to fetch ETag after upload: {e}");
            None
        }
    };

    let _persisted = persist_sync_success_best_effort(
        settings,
        snapshot.manifest_hash,
        etag,
        persist_sync_success,
    );
    Ok(serde_json::json!({ "status": "uploaded" }))
}

/// Download remote snapshot and apply to local database + skills.
pub async fn download(
    db: &crate::database::Database,
    settings: &mut S3SyncSettings,
) -> Result<Value, AppError> {
    settings.validate()?;
    let creds = creds_for(settings);

    let manifest_key = s3_key(settings, REMOTE_MANIFEST);
    let (manifest_bytes, etag) = s3::get_object(&creds, &manifest_key, MAX_MANIFEST_BYTES)
        .await?
        .ok_or_else(|| {
            localized(
                "s3.sync.remote_empty",
                "远端没有可下载的同步数据",
                "No downloadable sync data found on the remote.",
            )
        })?;

    let manifest: SyncManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|e| AppError::Json {
            path: REMOTE_MANIFEST.to_string(),
            source: e,
        })?;

    validate_manifest_compat(&manifest, RemoteLayout::Current)?;

    // Download and verify artifacts
    let db_sql = download_and_verify(settings, &creds, REMOTE_DB_SQL, &manifest.artifacts).await?;
    let skills_zip =
        download_and_verify(settings, &creds, REMOTE_SKILLS_ZIP, &manifest.artifacts).await?;

    // Apply snapshot
    apply_snapshot(db, &db_sql, &skills_zip)?;

    let manifest_hash = sha256_hex(&manifest_bytes);
    let _persisted =
        persist_sync_success_best_effort(settings, manifest_hash, etag, persist_sync_success);
    Ok(serde_json::json!({ "status": "downloaded" }))
}

/// Fetch remote manifest info without downloading artifacts.
pub async fn fetch_remote_info(settings: &S3SyncSettings) -> Result<Option<Value>, AppError> {
    settings.validate()?;
    let creds = creds_for(settings);
    let manifest_key = s3_key(settings, REMOTE_MANIFEST);

    let Some((bytes, _)) = s3::get_object(&creds, &manifest_key, MAX_MANIFEST_BYTES).await? else {
        return Ok(None);
    };

    let manifest: SyncManifest = serde_json::from_slice(&bytes).map_err(|e| AppError::Json {
        path: REMOTE_MANIFEST.to_string(),
        source: e,
    })?;

    let compatible = validate_manifest_compat(&manifest, RemoteLayout::Current).is_ok();

    let payload = serde_json::json!({
        "deviceName": manifest.device_name,
        "createdAt": manifest.created_at,
        "snapshotId": manifest.snapshot_id,
        "version": manifest.version,
        "protocolVersion": manifest.version,
        "dbCompatVersion": manifest.db_compat_version,
        "compatible": compatible,
        "artifacts": manifest.artifacts.keys().collect::<Vec<_>>(),
        "layout": RemoteLayout::Current.as_str(),
        "remotePath": s3_dir_display(settings),
    });

    Ok(Some(payload))
}

// ─── Sync status persistence ─────────────────────────────────

fn persist_sync_success(
    settings: &mut S3SyncSettings,
    manifest_hash: String,
    etag: Option<String>,
) -> Result<(), AppError> {
    let status = WebDavSyncStatus {
        last_sync_at: Some(Utc::now().timestamp()),
        last_error: None,
        last_error_source: None,
        last_local_manifest_hash: Some(manifest_hash.clone()),
        last_remote_manifest_hash: Some(manifest_hash),
        last_remote_etag: etag,
    };
    settings.status = status.clone();
    update_s3_sync_status(status)
}

// ─── Download & verify ───────────────────────────────────────

async fn download_and_verify(
    settings: &S3SyncSettings,
    creds: &S3Credentials,
    artifact_name: &str,
    artifacts: &BTreeMap<String, ArtifactMeta>,
) -> Result<Vec<u8>, AppError> {
    let meta = artifacts.get(artifact_name).ok_or_else(|| {
        localized(
            "s3.sync.manifest_missing_artifact",
            format!("manifest 中缺少 artifact: {artifact_name}"),
            format!("Manifest missing artifact: {artifact_name}"),
        )
    })?;
    validate_artifact_size_limit(artifact_name, meta.size)?;

    let key = s3_key(settings, artifact_name);
    let (bytes, _) = s3::get_object(creds, &key, MAX_SYNC_ARTIFACT_BYTES as usize)
        .await?
        .ok_or_else(|| {
            localized(
                "s3.sync.remote_missing_artifact",
                format!("远端缺少 artifact 文件: {artifact_name}"),
                format!("Remote artifact file missing: {artifact_name}"),
            )
        })?;

    verify_artifact(&bytes, artifact_name, meta)?;
    Ok(bytes)
}

// ─── S3 key helpers ──────────────────────────────────────────

/// Build the S3 object key for a given artifact.
///
/// Format: `{remote_root}/v{PROTOCOL_VERSION}/db-v{DB_COMPAT_VERSION}/{profile}/{artifact}`
/// Example: `cc-switch-sync/v2/db-v6/default/manifest.json`
fn s3_key(settings: &S3SyncSettings, artifact: &str) -> String {
    format!(
        "{}/v{}/db-v{}/{}/{}",
        settings.remote_root, PROTOCOL_VERSION, DB_COMPAT_VERSION, settings.profile, artifact
    )
}

fn s3_dir_display(settings: &S3SyncSettings) -> String {
    format!(
        "{}/v{}/db-v{}/{}",
        settings.remote_root, PROTOCOL_VERSION, DB_COMPAT_VERSION, settings.profile
    )
}

fn creds_for(settings: &S3SyncSettings) -> S3Credentials {
    S3Credentials {
        access_key_id: settings.access_key_id.clone(),
        secret_access_key: settings.secret_access_key.clone(),
        region: settings.region.clone(),
        bucket: settings.bucket.clone(),
        endpoint: settings.endpoint.clone(),
    }
}

fn put_precondition<'a>(
    conditional: bool,
    remote_exists: bool,
    if_match_etag: Option<&'a str>,
) -> Result<s3::PutPrecondition<'a>, AppError> {
    Ok(
        match resolve_put_precondition(conditional, remote_exists, if_match_etag)? {
            ResolvedPut::Unconditional => s3::PutPrecondition::None,
            ResolvedPut::IfMatch(etag) => s3::PutPrecondition::IfMatch(etag),
            ResolvedPut::IfNoneMatchAny => s3::PutPrecondition::IfNoneMatchAny,
        },
    )
}

/// PUT one snapshot object. Auto-sync HEADs first and fails closed when the
/// object exists without an ETag. Manual upload overwrites unconditionally.
/// A 412 aborts the caller so later objects (including the manifest) are not PUT.
async fn put_object_for_snapshot(
    creds: &s3::S3Credentials,
    key: &str,
    bytes: Vec<u8>,
    content_type: &str,
    conditional: bool,
) -> Result<(), AppError> {
    if !conditional {
        return s3::put_object(creds, key, bytes, content_type).await;
    }

    match s3::head_object_state(creds, key).await? {
        s3::HeadState::Missing => {
            s3::put_object_conditional(
                creds,
                key,
                bytes,
                content_type,
                s3::PutPrecondition::IfNoneMatchAny,
            )
            .await
        }
        s3::HeadState::Exists { etag } => {
            let etag = require_if_match_etag(etag.as_deref())?.to_string();
            s3::put_object_conditional(
                creds,
                key,
                bytes,
                content_type,
                s3::PutPrecondition::IfMatch(&etag),
            )
            .await
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_settings() -> S3SyncSettings {
        S3SyncSettings {
            remote_root: "cc-switch-sync".to_string(),
            profile: "default".to_string(),
            ..S3SyncSettings::default()
        }
    }

    #[test]
    fn s3_key_uses_v2_and_correct_format() {
        let settings = test_settings();
        let key = s3_key(&settings, "manifest.json");
        assert_eq!(key, "cc-switch-sync/v2/db-v6/default/manifest.json");
    }

    #[test]
    fn s3_key_with_custom_profile() {
        let settings = S3SyncSettings {
            remote_root: "my-root".to_string(),
            profile: "work".to_string(),
            ..S3SyncSettings::default()
        };
        assert_eq!(s3_key(&settings, "db.sql"), "my-root/v2/db-v6/work/db.sql");
    }

    #[test]
    fn s3_key_matches_expected_pattern() {
        let settings = test_settings();
        let key = s3_key(&settings, "skills.zip");
        // Should follow {remote_root}/v{version}/db-v{db}/{profile}/{artifact}
        let parts: Vec<&str> = key.splitn(5, '/').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[0], "cc-switch-sync");
        assert_eq!(parts[1], "v2");
        assert_eq!(parts[2], "db-v6");
        assert_eq!(parts[3], "default");
        assert_eq!(parts[4], "skills.zip");
    }

    #[test]
    fn sync_mutex_is_singleton() {
        let m1 = sync_mutex();
        let m2 = sync_mutex();
        assert!(
            std::ptr::eq(m1, m2),
            "sync_mutex must return the same instance"
        );
    }

    #[test]
    fn creds_for_maps_all_fields() {
        let settings = S3SyncSettings {
            access_key_id: "AKID".to_string(),
            secret_access_key: "SECRET".to_string(),
            region: "us-west-2".to_string(),
            bucket: "my-bucket".to_string(),
            endpoint: "minio.local:9000".to_string(),
            ..S3SyncSettings::default()
        };
        let creds = creds_for(&settings);
        assert_eq!(creds.access_key_id, "AKID");
        assert_eq!(creds.secret_access_key, "SECRET");
        assert_eq!(creds.region, "us-west-2");
        assert_eq!(creds.bucket, "my-bucket");
        assert_eq!(creds.endpoint, "minio.local:9000");
    }

    #[test]
    fn manual_upload_is_unconditional() {
        assert_eq!(
            put_precondition(false, true, Some("etag")).unwrap(),
            s3::PutPrecondition::None
        );
        assert_eq!(
            put_precondition(false, true, None).unwrap(),
            s3::PutPrecondition::None
        );
    }

    #[test]
    fn auto_upload_sends_if_match_when_remote_exists() {
        assert_eq!(
            put_precondition(true, true, Some("etag")).unwrap(),
            s3::PutPrecondition::IfMatch("etag")
        );
    }

    #[test]
    fn auto_upload_sends_if_none_match_when_creating() {
        assert_eq!(
            put_precondition(true, false, None).unwrap(),
            s3::PutPrecondition::IfNoneMatchAny
        );
    }

    #[test]
    fn auto_upload_denies_when_remote_exists_without_etag() {
        let err = put_precondition(true, true, None)
            .expect_err("existing remote without ETag must fail closed");
        let text = err.to_string();
        assert!(
            text.contains("ETag") || text.contains("omitted") || text.contains("未返回"),
            "unexpected error: {text}"
        );
    }

    #[test]
    fn artifact_put_uses_if_match_when_etag_present() {
        assert_eq!(
            put_precondition(true, true, Some("artifact-etag")).unwrap(),
            s3::PutPrecondition::IfMatch("artifact-etag")
        );
    }

    #[test]
    fn artifact_put_uses_if_none_match_when_missing() {
        assert_eq!(
            put_precondition(true, false, None).unwrap(),
            s3::PutPrecondition::IfNoneMatchAny
        );
    }

    #[test]
    fn artifact_put_denies_when_remote_exists_without_etag() {
        assert!(put_precondition(true, true, Some("  ")).is_err());
    }

    #[test]
    fn artifact_412_does_not_proceed_to_manifest() {
        use super::super::sync_protocol::put_in_order;

        let mut attempted = Vec::new();
        let err = put_in_order(["db.sql", "skills.zip", "manifest.json"], |name| {
            attempted.push(name);
            if name == "skills.zip" {
                Err(AppError::localized(
                    "s3.put.precondition_failed",
                    "远端数据已被其他设备更新（412），未覆盖远端。",
                    "Remote data was updated by another client (412 Precondition Failed); remote was not overwritten.",
                ))
            } else {
                Ok(())
            }
        })
        .expect_err("412 on an artifact must abort remaining PUTs");

        assert_eq!(attempted, vec!["db.sql", "skills.zip"]);
        assert!(err.to_string().contains("412"));
    }

    #[test]
    fn auto_upload_if_match_uses_fresh_get_etag_not_stored_cursor() {
        // Production takes the ETag from the GET/HEAD just performed. A stored
        // cursor alone is not a usable If-Match validator.
        assert_eq!(
            put_precondition(true, true, Some("fresh-from-get")).unwrap(),
            s3::PutPrecondition::IfMatch("fresh-from-get")
        );
        assert!(put_precondition(true, true, None).is_err());
    }
}
