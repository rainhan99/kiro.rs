//! Startup-only request pipeline editor. No inference, account access or hot apply.
use crate::{
    kiro::token_manager::MultiTokenManager, model::config::Config, pipeline::config::PipelineConfig,
};
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, io::Write, path::Path};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Update {
    pub config: PipelineConfig,
    pub revision: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct View {
    source: &'static str,
    runtime_editable: bool,
    editable: bool,
    effective_config: PipelineConfig,
    saved_config: Option<PipelineConfig>,
    revision: Option<String>,
    restart_required: bool,
}

pub(super) enum SettingsError {
    Invalid(String),
    Conflict,
    Persistence(&'static str),
    PersistenceUncertain,
}

impl IntoResponse for SettingsError {
    fn into_response(self) -> Response {
        let (status, kind, message) = match self {
            Self::Invalid(message) => (StatusCode::BAD_REQUEST, "invalid_request_error", message),
            Self::Conflict => (
                StatusCode::CONFLICT,
                "configuration_conflict",
                "请求管线配置已被其他页面或操作修改；请重新加载后再保存。当前运行配置未改变。"
                    .into(),
            ),
            Self::Persistence(operation) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "persistence_error",
                format!(
                    "无法{operation}配置文件；未应用到运行中的请求管线。请检查配置文件及目录权限。"
                ),
            ),
            Self::PersistenceUncertain => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "persistence_uncertain",
                "配置文件已替换，但目录同步失败；磁盘内容可能已更新。请重新读取确认并检查文件系统，当前运行配置未改变。".into(),
            ),
        };
        (
            status,
            Json(json!({"error":{"type":kind,"message":message}})),
        )
            .into_response()
    }
}

fn config_value(config: &PipelineConfig) -> Value {
    // All fields are finite integers, strings and enums; serialization is infallible.
    serde_json::to_value(config).expect("pipeline config is JSON serializable")
}

fn revision(config: &PipelineConfig) -> String {
    // Only nonsecret pipeline options, never a hash of the entire secret-bearing config.
    hex::encode(Sha256::digest(config_value(config).to_string().as_bytes()))
}

fn view(effective: &PipelineConfig, saved: Option<PipelineConfig>) -> View {
    View {
        source: "startup",
        runtime_editable: false,
        editable: saved.is_some(),
        revision: saved.as_ref().map(revision),
        restart_required: saved
            .as_ref()
            .is_some_and(|c| config_value(c) != config_value(effective)),
        effective_config: effective.clone(),
        saved_config: saved,
    }
}

fn read_document(path: &Path) -> Result<(Value, Config), SettingsError> {
    let bytes = fs::read(path).map_err(|_| SettingsError::Persistence("读取"))?;
    let document: Value =
        serde_json::from_slice(&bytes).map_err(|_| SettingsError::Persistence("解析"))?;
    let config: Config =
        serde_json::from_value(document.clone()).map_err(|_| SettingsError::Persistence("解析"))?;
    config
        .request_pipeline
        .validate()
        .map_err(|_| SettingsError::Persistence("校验已保存的"))?;
    Ok((document, config))
}

pub(super) fn get(manager: &MultiTokenManager) -> Result<View, SettingsError> {
    manager.with_config_file_lock(|path| {
        let saved = path
            .map(|p| read_document(p).map(|(_, c)| c.request_pipeline))
            .transpose()?;
        Ok(view(&manager.config().request_pipeline, saved))
    })
}

pub(super) fn save(manager: &MultiTokenManager, update: Update) -> Result<View, SettingsError> {
    update
        .config
        .validate()
        .map_err(|e| SettingsError::Invalid(e.to_string()))?;
    if !update.config.kiro_only || update.config.allow_simulated_cache {
        return Err(SettingsError::Invalid(
            "此网页配置要求 kiroOnly=true、allowSimulatedCache=false；仅使用 Kiro 和原生缓存证据。"
                .into(),
        ));
    }
    manager.with_config_file_lock(|path| {
        let path = path.ok_or(SettingsError::Persistence("定位"))?;
        let (mut document, mut candidate) = read_document(path)?;
        if revision(&candidate.request_pipeline) != update.revision {
            return Err(SettingsError::Conflict);
        }
        candidate.request_pipeline = update.config.clone();
        // Revalidate against the latest disk settings, including an external token API.
        candidate
            .validate()
            .map_err(|e| SettingsError::Invalid(e.to_string()))?;
        document["requestPipeline"] = config_value(&update.config);
        // Modify only this key, preserving unknown extensions and secret-bearing settings.
        atomic_save(path, &document)?;
        Ok(view(
            &manager.config().request_pipeline,
            Some(update.config),
        ))
    })
}

fn atomic_save(path: &Path, document: &Value) -> Result<(), SettingsError> {
    atomic_save_with_directory_sync(path, document, |directory| {
        #[cfg(unix)]
        {
            fs::File::open(directory)?.sync_all()
        }
        #[cfg(not(unix))]
        {
            // std does not portably flush directory handles on non-Unix hosts.
            // File sync + atomic replacement still apply; no power-loss guarantee.
            let _ = directory;
            Ok(())
        }
    })
}

fn atomic_save_with_directory_sync(
    path: &Path,
    document: &Value,
    sync_directory: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<(), SettingsError> {
    // Follow a configured symlink; replace the target, never the symlink itself.
    let target = fs::canonicalize(path).map_err(|_| SettingsError::Persistence("定位"))?;
    let metadata = fs::metadata(&target).map_err(|_| SettingsError::Persistence("读取权限"))?;
    if !metadata.is_file() {
        return Err(SettingsError::Persistence("写入非普通"));
    }
    let temp = target
        .parent()
        .ok_or(SettingsError::Persistence("定位目录"))?
        .join(format!(".kiro-pipeline-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temp)
        .map_err(|_| SettingsError::Persistence("创建临时"))?;
    let result = (|| {
        let mut bytes = serde_json::to_vec_pretty(document)
            .map_err(|_| SettingsError::Persistence("序列化"))?;
        bytes.push(b'\n');
        file.write_all(&bytes)
            .map_err(|_| SettingsError::Persistence("写入"))?;
        file.set_permissions(metadata.permissions())
            .map_err(|_| SettingsError::Persistence("保留权限"))?;
        file.sync_all()
            .map_err(|_| SettingsError::Persistence("同步"))?;
        drop(file);
        fs::rename(&temp, &target).map_err(|_| SettingsError::Persistence("原子替换"))?;
        // Rename changed the live pathname. A later durability failure is NOT
        // equivalent to a failed write that left the original file untouched.
        sync_directory(target.parent().unwrap())
            .map_err(|_| SettingsError::PersistenceUncertain)?;
        Ok(())
    })();
    if result.is_err() {
        // Only the create_new-owned temporary file. After rename it is already gone.
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn directory_sync_failure_reports_uncertainty_after_real_replacement() {
        let directory =
            std::env::temp_dir().join(format!("kiro-pipeline-sync-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("config.json");
        fs::write(&path, b"{}").unwrap();
        let result = atomic_save_with_directory_sync(&path, &json!({"newValue":true}), |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "synthetic directory-sync failure",
            ))
        });
        // Assert real disk contents and caller-visible status, not mock call counts.
        let disk: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        fs::remove_file(&path).unwrap();
        fs::remove_dir(&directory).unwrap();
        assert_eq!(disk, json!({"newValue":true}));
        let response = result
            .err()
            .expect("must not acknowledge an unsynced replacement")
            .into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["error"]["type"], "persistence_uncertain");
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("可能已更新")
        );
    }
}
