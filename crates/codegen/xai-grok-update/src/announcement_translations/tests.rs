use super::*;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn document(version: u64, text: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "schema_version": 1,
        "version": version,
        "locale": "zh-CN",
        "entries": [{"field": "title", "source": "From the team", "translation": text}]
    }))
    .unwrap()
}

fn manifest(version: u64, bytes: &[u8]) -> Manifest {
    Manifest {
        schema_version: 1,
        version,
        sha256: digest(bytes),
    }
}

fn catalog(version: u64, text: &str) -> TranslationCatalog {
    let bytes = document(version, text);
    TranslationCatalog::parse(manifest(version, &bytes), &bytes).unwrap()
}

fn client() -> reqwest::Client {
    xai_grok_extra_ca::build_reqwest_client(|builder| {
        builder
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(2))
    })
    .unwrap()
}

async fn serve_manifest(server: &MockServer, manifest: &Manifest) {
    Mock::given(method("GET"))
        .and(path("/manifest.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(manifest))
        .mount(server)
        .await;
}

#[test]
fn bundled_catalog_has_known_notices_and_keeps_exact_match_boundaries() {
    let catalog = TranslationCatalog::bundled();
    assert_eq!(catalog.version(), 1);
    assert_eq!(
        catalog.lookup(TranslationField::Title, "From the team"),
        Some("团队寄语")
    );
    assert_eq!(
        catalog.lookup(
            TranslationField::Message,
            "Hope you are having a wonderful day!"
        ),
        Some("祝你今天过得愉快！")
    );
    assert_eq!(
        catalog.lookup(TranslationField::Title, "From the team!"),
        None
    );
    assert_eq!(
        catalog.lookup(TranslationField::Title, " From the team"),
        None
    );
    assert_eq!(
        catalog.lookup(TranslationField::Message, "From the team"),
        None
    );
    assert_eq!(
        catalog.lookup(TranslationField::Title, "Future official notice"),
        None
    );
}

#[tokio::test]
async fn production_client_rejects_plain_http_before_sending_any_request() {
    let server = MockServer::start().await;
    let client = build_client().unwrap();
    assert!(
        read_response(&client, &server.uri(), MAX_MANIFEST_BYTES)
            .await
            .is_err()
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn offline_worker_loads_verified_cache_and_rejects_a_corrupt_cache() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.json");
    write_cache(&path, &catalog(2, "离线新版")).await.unwrap();
    let (sender, mut receiver) = watch::channel(TranslationCatalog::bundled());
    run_worker(sender, Some(path.clone()), false).await;
    receiver.changed().await.unwrap();
    assert_eq!(receiver.borrow_and_update().version(), 2);
    assert_eq!(
        receiver
            .borrow()
            .lookup(TranslationField::Title, "From the team"),
        Some("离线新版")
    );
    assert!(
        receiver.changed().await.is_err(),
        "offline worker must exit after loading cache"
    );

    tokio::fs::write(&path, b"broken cache").await.unwrap();
    let (sender, receiver) = watch::channel(TranslationCatalog::bundled());
    run_worker(sender, Some(path), false).await;
    assert_eq!(receiver.borrow().version(), 1);
}

#[tokio::test]
async fn dropping_translation_owner_cancels_an_inflight_request() {
    let server = MockServer::start().await;
    Mock::given(path("/manifest.json"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
        .mount(&server)
        .await;
    let (sender, receiver) = watch::channel(TranslationCatalog::bundled());
    let task = tokio::spawn(refresh_loop(
        sender,
        TranslationCatalog::bundled(),
        None,
        RefreshSource {
            client: client(),
            base_url: server.uri(),
            interval: REFRESH_INTERVAL,
            timeout: REFRESH_TIMEOUT,
        },
    ));
    let abort = task.abort_handle();
    let owner = TranslationUpdates { receiver, task };
    tokio::time::timeout(Duration::from_secs(1), async {
        while server.received_requests().await.unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(owner.current().version(), 1);
    drop(owner);
    tokio::time::timeout(Duration::from_secs(1), async {
        while !abort.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[test]
fn catalog_rejects_untrusted_shape_digest_and_duplicate_mappings() {
    let bytes = document(2, "新版团队寄语");
    assert!(TranslationCatalog::parse(manifest(3, &bytes), &bytes).is_err());
    let mut bad_hash = manifest(2, &bytes);
    bad_hash.sha256 = "0".repeat(64);
    assert!(TranslationCatalog::parse(bad_hash, &bytes).is_err());
    for field in ["id", "severity", "url", "expires_at"] {
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["entries"][0]["field"] = json!(field);
        let encoded = serde_json::to_vec(&value).unwrap();
        assert!(TranslationCatalog::parse(manifest(2, &encoded), &encoded).is_err());
    }
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let duplicate = value["entries"][0].clone();
    value["entries"].as_array_mut().unwrap().push(duplicate);
    let encoded = serde_json::to_vec(&value).unwrap();
    assert!(TranslationCatalog::parse(manifest(2, &encoded), &encoded).is_err());
    value["entries"].as_array_mut().unwrap().pop();
    value["entries"][0]["translation"] = json!("\u{001b}[2J");
    let encoded = serde_json::to_vec(&value).unwrap();
    assert!(TranslationCatalog::parse(manifest(2, &encoded), &encoded).is_err());
    value["entries"][0]["translation"] = json!("翻译");
    value["schema_version"] = json!(2);
    let encoded = serde_json::to_vec(&value).unwrap();
    assert!(TranslationCatalog::parse(manifest(2, &encoded), &encoded).is_err());
}

#[test]
fn catalog_rejects_oversize_and_allows_an_explicit_empty_new_version() {
    let bytes = vec![b' '; MAX_CATALOG_BYTES + 1];
    assert!(TranslationCatalog::parse(manifest(2, &bytes), &bytes).is_err());
    let bytes = serde_json::to_vec(
        &json!({"schema_version": 1, "version": 2, "locale": "zh-CN", "entries": []}),
    )
    .unwrap();
    let empty = TranslationCatalog::parse(manifest(2, &bytes), &bytes).unwrap();
    assert_eq!(empty.lookup(TranslationField::Title, "From the team"), None);
}

#[tokio::test]
async fn same_or_older_version_never_downloads_a_catalog() {
    let server = MockServer::start().await;
    let current = TranslationCatalog::bundled();
    serve_manifest(&server, &current.manifest).await;
    assert!(
        refresh_catalog(&client(), &server.uri(), &current)
            .await
            .unwrap()
            .is_none()
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), "/manifest.json");

    server.reset().await;
    serve_manifest(&server, &manifest(1, BUNDLED_CATALOG.as_bytes())).await;
    assert!(
        refresh_catalog(&client(), &server.uri(), &catalog(2, "新版"))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn changed_version_downloads_validates_and_replaces_the_entire_catalog() {
    let server = MockServer::start().await;
    let bytes = document(2, "新版团队寄语");
    serve_manifest(&server, &manifest(2, &bytes)).await;
    Mock::given(path("/catalogs/2.json"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
        .expect(1)
        .mount(&server)
        .await;
    let newer = refresh_catalog(&client(), &server.uri(), &TranslationCatalog::bundled())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(newer.version(), 2);
    assert_eq!(
        newer.lookup(TranslationField::Title, "From the team"),
        Some("新版团队寄语")
    );
    // An entry removed by the maintainer stays removed; a newer catalog is not
    // merged with stale bundled or cached translations.
    assert_eq!(
        newer.lookup(TranslationField::Title, "Degraded performance"),
        None
    );
}

#[tokio::test]
async fn forbidden_redirect_and_oversize_manifest_do_not_fetch_catalogs() {
    for response in [
        ResponseTemplate::new(403),
        ResponseTemplate::new(429),
        ResponseTemplate::new(302).insert_header("Location", "https://example.invalid/other.json"),
        ResponseTemplate::new(200).set_body_bytes(vec![b' '; MAX_MANIFEST_BYTES + 1]),
    ] {
        let server = MockServer::start().await;
        Mock::given(path("/manifest.json"))
            .respond_with(response)
            .mount(&server)
            .await;
        assert!(
            refresh_catalog(&client(), &server.uri(), &TranslationCatalog::bundled())
                .await
                .is_err()
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn reused_version_or_bad_download_is_rejected_without_mutating_cache() {
    let dir = tempfile::tempdir().unwrap();
    let cache_path = dir.path().join("catalog.json");
    let current = TranslationCatalog::bundled();
    write_cache(&cache_path, &current).await.unwrap();
    let original = tokio::fs::read(&cache_path).await.unwrap();
    let server = MockServer::start().await;
    serve_manifest(&server, &manifest(1, &document(1, "非法覆盖"))).await;
    assert!(
        refresh_catalog(&client(), &server.uri(), &current)
            .await
            .is_err()
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);

    server.reset().await;
    serve_manifest(&server, &manifest(2, &document(2, "新版"))).await;
    Mock::given(path("/catalogs/2.json"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(document(2, "错误的下载内容")))
        .mount(&server)
        .await;
    assert!(
        refresh_catalog(&client(), &server.uri(), &current)
            .await
            .is_err()
    );
    assert_eq!(tokio::fs::read(&cache_path).await.unwrap(), original);
    assert_eq!(read_cache(&cache_path).await.unwrap().version(), 1);
}

#[tokio::test]
async fn cache_is_atomic_digest_checked_and_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let cache_path = dir.path().join("nested/catalog.json");
    write_cache(&cache_path, &catalog(2, "第二版"))
        .await
        .unwrap();
    write_cache(&cache_path, &catalog(3, "第三版"))
        .await
        .unwrap();
    let cached = read_cache(&cache_path).await.unwrap();
    assert_eq!(cached.version(), 3);
    assert_eq!(
        cached.lookup(TranslationField::Title, "From the team"),
        Some("第三版")
    );
    let files = std::fs::read_dir(cache_path.parent().unwrap())
        .unwrap()
        .count();
    assert_eq!(files, 1);
    let original = tokio::fs::read(&cache_path).await.unwrap();
    let mut corrupt: CachedCatalog = serde_json::from_slice(&original).unwrap();
    corrupt.catalog_json = corrupt.catalog_json.replace("第三版", "篡改版");
    tokio::fs::write(&cache_path, serde_json::to_vec(&corrupt).unwrap())
        .await
        .unwrap();
    assert!(read_cache(&cache_path).await.is_err());
    tokio::fs::write(&cache_path, vec![b' '; MAX_CACHE_BYTES + 1])
        .await
        .unwrap();
    assert!(read_cache(&cache_path).await.is_err());
}

#[tokio::test]
async fn failed_cache_replacement_cleans_its_temporary_file_and_preserves_existing_data() {
    let dir = tempfile::tempdir().unwrap();
    let cache_path = dir.path().join("catalog.json");
    // A directory at the target forces rename to fail after the temporary
    // file has been fully written, exercising the writer's cleanup path.
    tokio::fs::create_dir(&cache_path).await.unwrap();
    let existing = cache_path.join("existing.txt");
    tokio::fs::write(&existing, b"keep me").await.unwrap();
    assert!(
        write_cache(&cache_path, &catalog(2, "第二版"))
            .await
            .is_err()
    );
    assert_eq!(tokio::fs::read(existing).await.unwrap(), b"keep me");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn stalled_refresh_leaves_snapshot_available_and_does_not_retry_immediately() {
    let server = MockServer::start().await;
    Mock::given(path("/manifest.json"))
        .respond_with(ResponseTemplate::new(403).set_delay(Duration::from_millis(300)))
        .mount(&server)
        .await;
    let current = TranslationCatalog::bundled();
    let (sender, receiver) = watch::channel(Arc::clone(&current));
    let worker = tokio::spawn(refresh_loop(
        sender,
        current,
        None,
        RefreshSource {
            client: client(),
            base_url: server.uri(),
            interval: Duration::from_secs(60),
            timeout: Duration::from_millis(50),
        },
    ));
    // A UI/input task remains schedulable while the network request is stalled.
    tokio::time::timeout(Duration::from_millis(100), tokio::task::yield_now())
        .await
        .unwrap();
    assert_eq!(
        receiver
            .borrow()
            .lookup(TranslationField::Title, "From the team"),
        Some("团队寄语")
    );
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert!(!receiver.has_changed().unwrap());
    drop(receiver);
    tokio::time::timeout(Duration::from_millis(100), worker)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn background_refresh_notifies_without_waiting_for_cache_write() {
    let server = MockServer::start().await;
    let bytes = document(2, "实时中文");
    serve_manifest(&server, &manifest(2, &bytes)).await;
    Mock::given(path("/catalogs/2.json"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let invalid_parent = dir.path().join("not-a-directory");
    tokio::fs::write(&invalid_parent, "existing data")
        .await
        .unwrap();
    let (sender, mut receiver) = watch::channel(TranslationCatalog::bundled());
    let worker = tokio::spawn(refresh_loop(
        sender,
        TranslationCatalog::bundled(),
        Some(invalid_parent.join("catalog.json")),
        RefreshSource {
            client: client(),
            base_url: server.uri(),
            interval: Duration::from_secs(60),
            timeout: Duration::from_secs(1),
        },
    ));
    tokio::time::timeout(Duration::from_secs(1), receiver.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        receiver
            .borrow_and_update()
            .lookup(TranslationField::Title, "From the team"),
        Some("实时中文")
    );
    drop(receiver);
    tokio::time::timeout(Duration::from_secs(1), worker)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        tokio::fs::read_to_string(invalid_parent).await.unwrap(),
        "existing data"
    );
}
