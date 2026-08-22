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
    path_segments, put_bytes, test_connection, PutPrecondition, WebDavAuth,
};
use crate::settings::{update_webdav_sync_status, WebDavSyncSettings, WebDavSyncStatus};

pub(crate) use super::sync_protocol::run_with_sync_lock;
use super::sync_protocol::{
    apply_snapshot, build_local_snapshot, effective_db_compat_version, localized,
    persist_sync_success_best_effort, sha256_hex, should_allow_auto_upload,
    validate_artifact_size_limit, validate_manifest_compat, verify_artifact, ArtifactMeta,
    RemoteLayout, SyncManifest, DB_COMPAT_VERSION, MAX_MANIFEST_BYTES, MAX_SYNC_ARTIFACT_BYTES,
    PROTOCOL_VERSION, REMOTE_DB_SQL, REMOTE_MANIFEST, REMOTE_SKILLS_ZIP,
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
        let remote = if write_target_exists {
            current
        } else {
            fetch_remote_snapshot(settings, &auth, RemoteLayout::Legacy).await?
        };
        let remote_exists = remote
            .as_ref()
            .is_some_and(|snapshot| !snapshot.manifest_bytes.is_empty());
        let remote_etag = remote
            .as_ref()
            .and_then(|snapshot| snapshot.manifest_etag.as_deref());
        let remote_hash = remote.as_ref().and_then(|snapshot| {
            (!snapshot.manifest_bytes.is_empty()).then(|| sha256_hex(&snapshot.manifest_bytes))
        });

        should_allow_auto_upload(
            settings.status.last_remote_etag.as_deref(),
            settings.status.last_remote_manifest_hash.as_deref(),
            remote_exists,
            remote_etag,
            remote_hash.as_deref(),
        )?;

        if_match_etag = settings
            .status
            .last_remote_etag
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
    }

    let snapshot = build_local_snapshot(db)?;

    // Upload order: artifacts first, manifest last (best-effort consistency)
    let db_url = remote_file_url(settings, RemoteLayout::Current, REMOTE_DB_SQL)?;
    put_bytes(
        &db_url,
        &auth,
        snapshot.db_sql,
        "application/sql",
        PutPrecondition::None,
    )
    .await?;

    let skills_url = remote_file_url(settings, RemoteLayout::Current, REMOTE_SKILLS_ZIP)?;
    put_bytes(
        &skills_url,
        &auth,
        snapshot.skills_zip,
        "application/zip",
        PutPrecondition::None,
    )
    .await?;

    let manifest_precondition =
        manifest_put_precondition(conditional, write_target_exists, if_match_etag.as_deref());

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

fn manifest_put_precondition<'a>(
    conditional: bool,
    remote_exists: bool,
    if_match_etag: Option<&'a str>,
) -> PutPrecondition<'a> {
    if !conditional {
        PutPrecondition::None
    } else if remote_exists {
        match if_match_etag {
            Some(etag) => PutPrecondition::IfMatch(etag),
            None => PutPrecondition::None,
        }
    } else {
        PutPrecondition::IfNoneMatchAny
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
            manifest_put_precondition(false, true, Some("etag")),
            PutPrecondition::None
        );
    }

    #[test]
    fn auto_upload_sends_if_match_when_remote_exists() {
        assert_eq!(
            manifest_put_precondition(true, true, Some("etag")),
            PutPrecondition::IfMatch("etag")
        );
    }

    #[test]
    fn auto_upload_sends_if_none_match_when_creating() {
        assert_eq!(
            manifest_put_precondition(true, false, None),
            PutPrecondition::IfNoneMatchAny
        );
    }
}
