//! Real loopback HTTP and temporary files, never the inference provider or live config.
use super::*;
use crate::{kiro::token_manager::MultiTokenManager, model::config::Config};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};

const ADMIN_KEY: &str = "pipeline-fixture-admin";

struct Fixture {
    directory: PathBuf,
    manager: Arc<MultiTokenManager>,
    client: reqwest::Client,
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new(with_path: bool) -> Self {
        let directory =
            std::env::temp_dir().join(format!("kiro-web-pipeline-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("config.json");
        std::fs::write(
            &path,
            br#"{"port":8999,"apiKey":"fixture-private-api-key","futureOption":{"preserve":true}}"#,
        )
        .unwrap();
        let config = if with_path {
            Config::load(&path).unwrap()
        } else {
            Config::default()
        };
        let manager = Arc::new(MultiTokenManager::new(config, vec![], None, None, false).unwrap());
        let state = AdminState::new(
            ADMIN_KEY,
            AdminService::new(manager.clone(), vec![]),
            Arc::new(ClientKeyManager::new()),
            Arc::new(UsageAggregator::new()),
            Arc::new(TraceStore::open_in_memory().unwrap()),
            Arc::new(GroupManager::new()),
        );
        let api = create_admin_router(state);
        let router = axum::Router::new()
            .nest("/api/admin", api.clone())
            .nest("/admin", crate::admin_ui::create_admin_ui_router())
            .merge(api);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/request-pipeline", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            directory,
            manager,
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap(),
            url,
            task,
        }
    }

    fn path(&self) -> PathBuf {
        self.directory.join("config.json")
    }

    async fn get(&self) -> Value {
        self.client
            .get(&self.url)
            .header("x-api-key", ADMIN_KEY)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn put(&self, config: &Value, revision: &Value) -> reqwest::Response {
        self.client
            .put(&self.url)
            .header("x-api-key", ADMIN_KEY)
            .json(&json!({"config":config,"revision":revision}))
            .send()
            .await
            .unwrap()
    }

    fn disk(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.path()).unwrap()).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[tokio::test]
async fn web_pipeline_get_reports_saved_and_effective_without_exposing_secrets() {
    let fixture = Fixture::new(true).await;
    let view = fixture.get().await;
    assert_eq!(view["editable"], true);
    assert_eq!(view["runtimeEditable"], false);
    assert_eq!(view["restartRequired"], false);
    assert_eq!(view["savedConfig"]["artifacts"]["enabled"], false);
    assert_eq!(view["savedConfig"], view["effectiveConfig"]);
    assert!(view["revision"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(!view.to_string().contains("fixture-private-api-key"));
    assert!(!view.to_string().contains("futureOption"));
}

#[tokio::test]
async fn web_pipeline_save_persists_only_pipeline_and_does_not_fake_hot_apply() {
    let fixture = Fixture::new(true).await;
    let initial = fixture.get().await;
    let mut next = initial["effectiveConfig"].clone();
    next["artifacts"]["enabled"] = json!(true);
    next["images"]["strategy"] = json!("lossless-tiles");
    next["limits"]["bodyBytes"] = json!(8_000_000);
    let response = fixture.put(&next, &initial["revision"]).await;
    assert_eq!(response.status(), 200);
    let saved: Value = response.json().await.unwrap();
    assert_eq!(saved["savedConfig"], next);
    assert_eq!(saved["effectiveConfig"], initial["effectiveConfig"]);
    assert_eq!(saved["restartRequired"], true);
    assert_ne!(saved["revision"], initial["revision"]);
    assert!(!fixture.manager.config().request_pipeline.artifacts.enabled);
    let disk = fixture.disk();
    assert_eq!(disk["port"], 8999);
    assert_eq!(disk["apiKey"], "fixture-private-api-key");
    assert_eq!(disk["futureOption"], json!({"preserve":true}));
    assert_eq!(disk["requestPipeline"], next);
    assert_eq!(std::fs::read_dir(&fixture.directory).unwrap().count(), 1);
    let after_reload = Config::load(fixture.path()).unwrap();
    assert!(after_reload.request_pipeline.artifacts.enabled);
    assert_eq!(fixture.get().await, saved);
}

#[tokio::test]
async fn web_pipeline_stale_revision_is_a_conflict_not_last_writer_wins() {
    let fixture = Fixture::new(true).await;
    let initial = fixture.get().await;
    let mut first = initial["effectiveConfig"].clone();
    first["artifacts"]["enabled"] = json!(true);
    assert_eq!(
        fixture.put(&first, &initial["revision"]).await.status(),
        200
    );
    let response = fixture
        .put(&initial["effectiveConfig"], &initial["revision"])
        .await;
    assert_eq!(response.status(), 409);
    let error: Value = response.json().await.unwrap();
    assert_eq!(error["error"]["type"], "configuration_conflict");
    assert_eq!(fixture.disk()["requestPipeline"], first);
}

#[tokio::test]
async fn web_pipeline_rejects_invalid_or_unsafe_changes_without_writing() {
    let fixture = Fixture::new(true).await;
    let initial = fixture.get().await;
    let original = std::fs::read(fixture.path()).unwrap();
    for (pointer, value) in [
        ("/artifacts/readBytes", json!(0)),
        ("/limits/bodyBytes", json!(-1)),
        ("/limits/bodyBytes", json!(0)),
        ("/limits/bodyBytes", json!(2.5)),
        ("/artifacts/thresholdBytes", json!(100_000_000)),
        ("/kiroOnly", json!(false)),
        ("/allowSimulatedCache", json!(true)),
    ] {
        let mut invalid = initial["effectiveConfig"].clone();
        *invalid.pointer_mut(pointer).unwrap() = value;
        let response = fixture.put(&invalid, &initial["revision"]).await;
        assert_eq!(response.status(), 400, "invalid field {pointer}");
        let error: Value = response.json().await.unwrap();
        assert_eq!(error["error"]["type"], "invalid_request_error");
        assert_eq!(std::fs::read(fixture.path()).unwrap(), original);
    }
    let response = fixture.client.put(&fixture.url).header("x-api-key", ADMIN_KEY)
        .json(&json!({"config":initial["effectiveConfig"],"revision":initial["revision"],"surprise":true})).send().await.unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(std::fs::read(fixture.path()).unwrap(), original);
}

#[tokio::test]
async fn web_pipeline_save_revalidates_fresh_disk_external_token_api() {
    let fixture = Fixture::new(true).await;
    let initial = fixture.get().await;
    let mut disk = fixture.disk();
    disk["countTokensApiUrl"] = json!("https://example.invalid/not-called");
    std::fs::write(fixture.path(), disk.to_string()).unwrap();
    let response = fixture
        .put(&initial["effectiveConfig"], &initial["revision"])
        .await;
    assert_eq!(response.status(), 400);
    assert_eq!(fixture.disk(), disk);
}

#[tokio::test]
async fn web_pipeline_unknown_or_missing_config_never_claims_save_success() {
    let fixture = Fixture::new(false).await;
    let view = fixture.get().await;
    assert_eq!(view["editable"], false);
    assert!(view["savedConfig"].is_null());
    assert!(view["revision"].is_null());
    assert_eq!(
        fixture
            .put(&view["effectiveConfig"], &json!("unknown"))
            .await
            .status(),
        500
    );

    let fixture = Fixture::new(true).await;
    let view = fixture.get().await;
    std::fs::rename(fixture.path(), fixture.directory.join("original.json")).unwrap();
    let response = fixture
        .put(&view["effectiveConfig"], &view["revision"])
        .await;
    assert_eq!(response.status(), 500);
    let error: Value = response.json().await.unwrap();
    assert_eq!(error["error"]["type"], "persistence_error");
    assert!(!fixture.path().exists());
}

#[tokio::test]
async fn web_pipeline_requires_existing_admin_auth_for_reads_and_writes() {
    let fixture = Fixture::new(true).await;
    let initial = fixture.get().await;
    let original = std::fs::read(fixture.path()).unwrap();
    assert_eq!(
        fixture
            .client
            .get(&fixture.url)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let response = fixture
        .client
        .put(&fixture.url)
        .json(&json!({"config":initial["effectiveConfig"],"revision":initial["revision"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert_eq!(std::fs::read(fixture.path()).unwrap(), original);
}

#[tokio::test]
async fn web_pipeline_concurrent_saves_allow_only_one_writer_per_revision() {
    let fixture = Fixture::new(true).await;
    let initial = fixture.get().await;
    let mut first = initial["effectiveConfig"].clone();
    first["artifacts"]["enabled"] = json!(true);
    let mut second = initial["effectiveConfig"].clone();
    second["images"]["strategy"] = json!("lossless-tiles");
    let (a, b) = tokio::join!(
        fixture.put(&first, &initial["revision"]),
        fixture.put(&second, &initial["revision"])
    );
    let mut statuses = [a.status().as_u16(), b.status().as_u16()];
    statuses.sort();
    assert_eq!(statuses, [200, 409]);
    assert_eq!(
        fixture.disk()["requestPipeline"],
        if a.status() == 200 { first } else { second }
    );
}

#[tokio::test]
async fn web_pipeline_cannot_replace_a_directory_or_report_io_failure_as_success() {
    let fixture = Fixture::new(true).await;
    let initial = fixture.get().await;
    let original = fixture.disk();
    let backup = fixture.directory.join("original.json");
    std::fs::rename(fixture.path(), &backup).unwrap();
    std::fs::create_dir(fixture.path()).unwrap();
    let response = fixture
        .put(&initial["effectiveConfig"], &initial["revision"])
        .await;
    assert_eq!(response.status(), 500);
    assert!(fixture.path().is_dir());
    let preserved: Value = serde_json::from_slice(&std::fs::read(&backup).unwrap()).unwrap();
    assert_eq!(preserved, original);
}

#[cfg(unix)]
#[tokio::test]
async fn web_pipeline_atomic_save_preserves_symlink_and_private_permissions() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let fixture = Fixture::new(true).await;
    let target = fixture.directory.join("target.json");
    std::fs::rename(fixture.path(), &target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
    symlink(&target, fixture.path()).unwrap();
    let initial = fixture.get().await;
    let mut next = initial["effectiveConfig"].clone();
    next["artifacts"]["enabled"] = json!(true);
    assert_eq!(fixture.put(&next, &initial["revision"]).await.status(), 200);
    assert!(
        std::fs::symlink_metadata(fixture.path())
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(fixture.disk()["requestPipeline"], next);
    assert_eq!(std::fs::read_dir(&fixture.directory).unwrap().count(), 2);
}

/// Run manually after building admin-ui. This fixture has no accounts, provider,
/// network warmers or real configuration; it only binds an ephemeral loopback port.
#[tokio::test]
#[ignore = "manual browser QA of isolated admin fixture, no Kiro traffic"]
async fn web_pipeline_browser_fixture() {
    let fixture = Fixture::new(true).await;
    println!(
        "Browser QA: {}/admin/#/settings?s=pipeline",
        fixture.url.trim_end_matches("/request-pipeline")
    );
    println!("Fixture admin key (not a live secret): {ADMIN_KEY}");
    println!("Temporary config: {}", fixture.path().display());
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = tokio::time::sleep(std::time::Duration::from_secs(900)) => {},
    }
}
