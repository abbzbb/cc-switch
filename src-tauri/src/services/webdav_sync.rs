//! WebDAV v2 sync protocol layer with DB compatibility subdirectories.
//!
//! Implements manifest-based synchronization on top of the HTTP transport
//! primitives in [`super::webdav`]. Artifact set: `db.sql` + `skills.zip`.

use std::collections::BTreeMap;

use chrono::Utc;
use serde_json::Value;

use crate::error::AppError;
use crate::services::webdav::{
    auth_from_credentials, build_remote_url, ensure_remote_directories, get_bytes, head_etag,
    head_object_state, path_segments, put_bytes, test_connection, HeadState, PutPrecondition,
    WebDavAuth,
};
use crate::settings::{update_webdav_sync_status, WebDavSyncSettings, WebDavSyncStatus};

pub(crate) use super::sync_protocol::run_with_sync_lock;
use super::sync_protocol::{
    apply_snapshot, build_local_snapshot, effective_db_compat_version, localized,
    persist_sync_success_best_effort, require_if_match_etag, resolve_put_precondition, sha256_hex,
    should_allow_auto_upload, validate_artifact_size_limit, validate_manifest_compat,
    verify_artifact, ArtifactMeta, RemoteLayout, ResolvedPut, SyncManifest, DB_COMPAT_VERSION,
    MAX_MANIFEST_BYTES, MAX_SYNC_ARTIFACT_BYTES, PROTOCOL_VERSION, REMOTE_DB_SQL, REMOTE_MANIFEST,
    REMOTE_SKILLS_ZIP,
};

#[cfg(test)]
pub(crate) fn sync_mutex() -> &'static tokio::sync::Mutex<()> {
    super::sync_protocol::sync_mutex()
}

pub(crate) mod archive;

struct RemoteSnapshot {
    layout: RemoteLayout,
    manifest: SyncManifest,
    manifest_bytes: Vec<u8>,
    manifest_etag: Option<String>,
}
// ─── Public API ──────────────────────────────────────────────

/// Check WebDAV connectivity and ensure remote directory structure.
pub async fn check_connection(settings: &WebDavSyncSettings) -> Result<(), AppError> {
    settings.validate()?;
    let auth = auth_for(settings);
    test_connection(&settings.base_url, &auth).await?;
    let dir_segs = remote_dir_segments(settings, RemoteLayout::Current);
    ensure_remote_directories(&settings.base_url, &dir_segs, &auth).await?;
    Ok(())
}

/// Upload local snapshot (db + skills) to remote.
///
/// Manual/explicit UI upload may overwrite the remote.
pub async fn upload(
    db: &crate::database::Database,
    settings: &mut WebDavSyncSettings,
) -> Result<Value, AppError> {
    upload_snapshot(db, settings, false).await
}

/// Auto-sync upload: fetch the remote first and refuse to last-write-wins.
pub async fn upload_auto(
    db: &crate::database::Database,
    settings: &mut WebDavSyncSettings,
) -> Result<Value, AppError> {
    upload_snapshot(db, settings, true).await
}

async fn upload_snapshot(
    db: &crate::database::Database,
    settings: &mut WebDavSyncSettings,
    conditional: bool,
) -> Result<Value, AppError> {
    settings.validate()?;
    let auth = auth_for(settings);
    let dir_segs = remote_dir_segments(settings, RemoteLayout::Current);
    ensure_remote_directories(&settings.base_url, &dir_segs, &auth).await?;

    let manifest_url = remote_file_url(settings, RemoteLayout::Current, REMOTE_MANIFEST)?;
    let mut write_target_exists = false;
    let mut if_match_etag: Option<String> = None;

    if conditional {
        // Uploads always write Current. Consult Legacy only for the
        // download-first / conflict decision so a second device cannot seed
        // Current and hide an existing Legacy snapshot. If-Match is sent only
        // when Current itself exists (Legacy etags are a different URL).
        let current = fetch_remote_snapshot(settings, &auth, RemoteLayout::Current).await?;
        write_target_exists = current
            .as_ref()
            .is_some_and(|snapshot| !snapshot.manifest_bytes.is_empty());
        let legacy = if write_target_exists {
            None
        } else {
            fetch_remote_snapshot(settings, &auth, RemoteLayout::Legacy).await?
        };
        let remote = if write_target_exists {
            current.as_ref()
        } else {
            legacy.as_ref()
        };
        let remote_exists = remote.is_some_and(|snapshot| !snapshot.manifest_bytes.is_empty());
        let remote_etag = remote.and_then(|snapshot| snapshot.manifest_etag.as_deref());
        let remote_hash = remote.and_then(|snapshot| {
            (!snapshot.manifest_bytes.is_empty()).then(|| sha256_hex(&snapshot.manifest_bytes))
        });

        should_allow_auto_upload(
            settings.status.last_remote_etag.as_deref(),
            settings.status.last_remote_manifest_hash.as_deref(),
            remote_exists,
            remote_etag,
            remote_hash.as_deref(),
        )?;

        // Prefer the ETag from this GET, not the stored cursor. Hash-only
        // should_allow_auto_upload can succeed without last_remote_etag.
        if_match_etag = current
            .as_ref()
            .filter(|snapshot| !snapshot.manifest_bytes.is_empty())
            .and_then(|snapshot| snapshot.manifest_etag.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
    }

    // Fail closed before any artifact PUT: existing Current without ETag
    // must not last-write-wins.
    let manifest_precondition =
        put_precondition(conditional, write_target_exists, if_match_etag.as_deref())?;

    let snapshot = build_local_snapshot(db)?;

    // Upload order: artifacts first, manifest last. Any 412 aborts remaining
    // objects. Auto-sync HEADs each artifact and uses If-Match / If-None-Match;
    // manual upload stays overwrite.
    let db_url = remote_file_url(settings, RemoteLayout::Current, REMOTE_DB_SQL)?;
    put_bytes_for_snapshot(
        &db_url,
        &auth,
        snapshot.db_sql,
        "application/sql",
        conditional,
    )
    .await?;

    let skills_url = remote_file_url(settings, RemoteLayout::Current, REMOTE_SKILLS_ZIP)?;
    put_bytes_for_snapshot(
        &skills_url,
        &auth,
        snapshot.skills_zip,
        "application/zip",
        conditional,
    )
    .await?;

    put_bytes(
        &manifest_url,
        &auth,
        snapshot.manifest_bytes,
        "application/json",
        manifest_precondition,
    )
    .await?;

    // Fetch etag (best-effort, don't fail the upload)
    let etag = match head_etag(&manifest_url, &auth).await {
        Ok(e) => e,
        Err(e) => {
            log::debug!("[WebDAV] Failed to fetch ETag after upload: {e}");
            None
        }
    };

    let persisted = persist_sync_success_best_effort(
        settings,
        snapshot.manifest_hash,
        etag,
        persist_sync_success,
    );
    if persisted {
        Ok(serde_json::json!({ "status": "uploaded" }))
    } else {
        Ok(serde_json::json!({
            "status": "uploaded",
            "warning": "remote upload succeeded but local sync cursor was not saved"
        }))
    }
}

/// Download remote snapshot and apply to local database + skills.
pub async fn download(
    db: &crate::database::Database,
    settings: &mut WebDavSyncSettings,
) -> Result<Value, AppError> {
    settings.validate()?;
    let auth = auth_for(settings);
    let snapshot = find_remote_snapshot(settings, &auth)
        .await?
        .ok_or_else(|| {
            localized(
                "webdav.sync.remote_empty",
                "远端没有可下载的同步数据",
                "No downloadable sync data found on the remote.",
            )
        })?;

    validate_manifest_compat(&snapshot.manifest, snapshot.layout)?;

    // Download and verify artifacts
    let db_sql = download_and_verify(
        settings,
        &auth,
        snapshot.layout,
        REMOTE_DB_SQL,
        &snapshot.manifest.artifacts,
    )
    .await?;
    let skills_zip = download_and_verify(
        settings,
        &auth,
        snapshot.layout,
        REMOTE_SKILLS_ZIP,
        &snapshot.manifest.artifacts,
    )
    .await?;

    // Apply snapshot
    apply_snapshot(db, &db_sql, &skills_zip)?;

    let manifest_hash = sha256_hex(&snapshot.manifest_bytes);
    let _persisted = persist_sync_success_best_effort(
        settings,
        manifest_hash,
        snapshot.manifest_etag,
        persist_sync_success,
    );
    Ok(serde_json::json!({
        "status": "downloaded",
        "sourceLayout": snapshot.layout.as_str(),
        "sourcePath": remote_dir_display(settings, snapshot.layout),
    }))
}

/// Fetch remote manifest info without downloading artifacts.
pub async fn fetch_remote_info(settings: &WebDavSyncSettings) -> Result<Option<Value>, AppError> {
    settings.validate()?;
    let auth = auth_for(settings);
    let Some(snapshot) = find_remote_snapshot(settings, &auth).await? else {
        return Ok(None);
    };
    let compatible = validate_manifest_compat(&snapshot.manifest, snapshot.layout).is_ok();
    let db_compat_version = effective_db_compat_version(&snapshot.manifest, snapshot.layout);

    let payload = serde_json::json!({
        "deviceName": snapshot.manifest.device_name,
        "createdAt": snapshot.manifest.created_at,
        "snapshotId": snapshot.manifest.snapshot_id,
        "version": snapshot.manifest.version,
        "protocolVersion": snapshot.manifest.version,
        "dbCompatVersion": db_compat_version,
        "compatible": compatible,
        "artifacts": snapshot.manifest.artifacts.keys().collect::<Vec<_>>(),
        "layout": snapshot.layout.as_str(),
        "remotePath": remote_dir_display(settings, snapshot.layout),
    });

    Ok(Some(payload))
}

// ─── Sync status persistence ─────────────────────────────────

fn persist_sync_success(
    settings: &mut WebDavSyncSettings,
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
    update_webdav_sync_status(status)
}

async fn find_remote_snapshot(
    settings: &WebDavSyncSettings,
    auth: &WebDavAuth,
) -> Result<Option<RemoteSnapshot>, AppError> {
    if let Some(snapshot) = fetch_remote_snapshot(settings, auth, RemoteLayout::Current).await? {
        return Ok(Some(snapshot));
    }
    fetch_remote_snapshot(settings, auth, RemoteLayout::Legacy).await
}

async fn fetch_remote_snapshot(
    settings: &WebDavSyncSettings,
    auth: &WebDavAuth,
    layout: RemoteLayout,
) -> Result<Option<RemoteSnapshot>, AppError> {
    let manifest_url = remote_file_url(settings, layout, REMOTE_MANIFEST)?;
    let Some((manifest_bytes, manifest_etag)) =
        get_bytes(&manifest_url, auth, MAX_MANIFEST_BYTES).await?
    else {
        return Ok(None);
    };

    let manifest: SyncManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|e| AppError::Json {
            path: REMOTE_MANIFEST.to_string(),
            source: e,
        })?;

    Ok(Some(RemoteSnapshot {
        layout,
        manifest,
        manifest_bytes,
        manifest_etag,
    }))
}
// ─── Download & verify ───────────────────────────────────────

async fn download_and_verify(
    settings: &WebDavSyncSettings,
    auth: &WebDavAuth,
    layout: RemoteLayout,
    artifact_name: &str,
    artifacts: &BTreeMap<String, ArtifactMeta>,
) -> Result<Vec<u8>, AppError> {
    let meta = artifacts.get(artifact_name).ok_or_else(|| {
        localized(
            "webdav.sync.manifest_missing_artifact",
            format!("manifest 中缺少 artifact: {artifact_name}"),
            format!("Manifest missing artifact: {artifact_name}"),
        )
    })?;
    validate_artifact_size_limit(artifact_name, meta.size)?;

    let url = remote_file_url(settings, layout, artifact_name)?;
    let (bytes, _) = get_bytes(&url, auth, MAX_SYNC_ARTIFACT_BYTES as usize)
        .await?
        .ok_or_else(|| {
            localized(
                "webdav.sync.remote_missing_artifact",
                format!("远端缺少 artifact 文件: {artifact_name}"),
                format!("Remote artifact file missing: {artifact_name}"),
            )
        })?;

    verify_artifact(&bytes, artifact_name, meta)?;
    Ok(bytes)
}

// ─── Remote path helpers ─────────────────────────────────────

fn remote_dir_segments(settings: &WebDavSyncSettings, layout: RemoteLayout) -> Vec<String> {
    let mut segs = Vec::new();
    segs.extend(path_segments(&settings.remote_root).map(str::to_string));
    segs.push(format!("v{PROTOCOL_VERSION}"));
    if layout == RemoteLayout::Current {
        segs.push(format!("db-v{DB_COMPAT_VERSION}"));
    }
    segs.extend(path_segments(&settings.profile).map(str::to_string));
    segs
}

fn remote_file_url(
    settings: &WebDavSyncSettings,
    layout: RemoteLayout,
    file_name: &str,
) -> Result<String, AppError> {
    let mut segs = remote_dir_segments(settings, layout);
    segs.extend(path_segments(file_name).map(str::to_string));
    build_remote_url(&settings.base_url, &segs)
}

fn remote_dir_display(settings: &WebDavSyncSettings, layout: RemoteLayout) -> String {
    let segs = remote_dir_segments(settings, layout);
    format!("/{}", segs.join("/"))
}

fn auth_for(settings: &WebDavSyncSettings) -> WebDavAuth {
    auth_from_credentials(&settings.username, &settings.password)
}

fn put_precondition<'a>(
    conditional: bool,
    remote_exists: bool,
    if_match_etag: Option<&'a str>,
) -> Result<PutPrecondition<'a>, AppError> {
    Ok(
        match resolve_put_precondition(conditional, remote_exists, if_match_etag)? {
            ResolvedPut::Unconditional => PutPrecondition::None,
            ResolvedPut::IfMatch(etag) => PutPrecondition::IfMatch(etag),
            ResolvedPut::IfNoneMatchAny => PutPrecondition::IfNoneMatchAny,
        },
    )
}

/// PUT one snapshot object. Auto-sync HEADs first and fails closed when the
/// object exists without an ETag. Manual upload overwrites unconditionally.
/// A 412 aborts the caller so later objects (including the manifest) are not PUT.
async fn put_bytes_for_snapshot(
    url: &str,
    auth: &WebDavAuth,
    bytes: Vec<u8>,
    content_type: &str,
    conditional: bool,
) -> Result<(), AppError> {
    if !conditional {
        return put_bytes(url, auth, bytes, content_type, PutPrecondition::None).await;
    }

    match head_object_state(url, auth).await? {
        HeadState::Missing => {
            put_bytes(
                url,
                auth,
                bytes,
                content_type,
                PutPrecondition::IfNoneMatchAny,
            )
            .await
        }
        HeadState::Exists { etag } => {
            let etag = require_if_match_etag(etag.as_deref())?.to_string();
            put_bytes(
                url,
                auth,
                bytes,
                content_type,
                PutPrecondition::IfMatch(&etag),
            )
            .await
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_dir_segments_uses_current_layout() {
        let settings = WebDavSyncSettings {
            remote_root: "cc-switch-sync".to_string(),
            profile: "default".to_string(),
            ..WebDavSyncSettings::default()
        };
        let segs = remote_dir_segments(&settings, RemoteLayout::Current);
        assert_eq!(segs, vec!["cc-switch-sync", "v2", "db-v6", "default"]);
    }

    #[test]
    fn remote_dir_segments_uses_legacy_layout() {
        let settings = WebDavSyncSettings {
            remote_root: "cc-switch-sync".to_string(),
            profile: "default".to_string(),
            ..WebDavSyncSettings::default()
        };
        let segs = remote_dir_segments(&settings, RemoteLayout::Legacy);
        assert_eq!(segs, vec!["cc-switch-sync", "v2", "default"]);
    }

    #[test]
    fn manual_upload_is_unconditional() {
        assert_eq!(
            put_precondition(false, true, Some("etag")).unwrap(),
            PutPrecondition::None
        );
        assert_eq!(
            put_precondition(false, true, None).unwrap(),
            PutPrecondition::None
        );
    }

    #[test]
    fn auto_upload_sends_if_match_when_remote_exists() {
        assert_eq!(
            put_precondition(true, true, Some("etag")).unwrap(),
            PutPrecondition::IfMatch("etag")
        );
    }

    #[test]
    fn auto_upload_sends_if_none_match_when_creating() {
        assert_eq!(
            put_precondition(true, false, None).unwrap(),
            PutPrecondition::IfNoneMatchAny
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
            PutPrecondition::IfMatch("artifact-etag")
        );
    }

    #[test]
    fn artifact_put_uses_if_none_match_when_missing() {
        assert_eq!(
            put_precondition(true, false, None).unwrap(),
            PutPrecondition::IfNoneMatchAny
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
                    "webdav.put.precondition_failed",
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
            PutPrecondition::IfMatch("fresh-from-get")
        );
        assert!(put_precondition(true, true, None).is_err());
    }
}
