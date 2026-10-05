#[path = "common/ui.rs"]
#[allow(dead_code)]
mod common;
use common::UiFixture;
use serde_json::json;

#[tokio::test]
async fn local_session_protects_task_data_and_checks_host_origin_csrf() {
    let fixture = UiFixture::new();
    let task = fixture.seed_ready();
    let server = fixture.start();
    let anonymous = reqwest::Client::builder().no_proxy().build().unwrap();
    let bootstrap = anonymous.get(&server.url).send().await.unwrap();
    assert_eq!(bootstrap.status(), 200);
    assert!(!bootstrap.text().await.unwrap().contains("fix the parser"));
    assert_eq!(
        anonymous
            .get(format!("{}/tasks", server.url))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let session = server.session().await;
    let before = fixture.events(&task);
    let response = session
        .client
        .get(format!("{}/tasks", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert!(
        response.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("script-src 'self'")
    );
    let html = response.text().await.unwrap();
    assert!(html.contains("fix the parser"));
    assert!(!html.contains("provider-key-sentinel"));
    assert_eq!(
        session
            .client
            .get(format!("{}/tasks", server.url))
            .header("Host", "evil.invalid")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        session
            .client
            .post(format!("{}/tasks", server.url))
            .header("Origin", "http://evil.invalid")
            .header("X-CSRF-Token", &session.csrf)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        session
            .client
            .post(format!("{}/tasks", server.url))
            .header("Origin", &server.url)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        session
            .client
            .post(format!("{}/tasks", server.url))
            .header("Origin", &server.url)
            .header("X-CSRF-Token", "wrong")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(fixture.events(&task), before);
    assert!(
        !server
            .stderr
            .lock()
            .unwrap()
            .contains("provider-key-sentinel")
    );
}

#[tokio::test]
async fn local_session_rejects_bad_bootstrap_and_stale_cookie() {
    let fixture = UiFixture::new();
    fixture.seed_ready();
    let mut server = fixture.start();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let bad = client
        .post(format!("{}/session", server.url))
        .header("Origin", &server.url)
        .header("Content-Type", "application/json")
        .body(json!({"token":"wrong"}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 403);
    let session = server.session().await;
    let old_cookie = session
        .client
        .get(format!("{}/tasks", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(old_cookie.status(), 200);
    server.stop();
    let restarted = fixture.start();
    // Cookies are host scoped rather than port scoped, so this specifically exercises
    // the restarted server's session registry, not the client's cookie routing.
    assert_eq!(
        session
            .client
            .get(format!("{}/tasks", restarted.url))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let fresh = restarted.session().await;
    assert_eq!(
        fresh
            .client
            .get(format!("{}/tasks", restarted.url))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
}

#[tokio::test]
async fn local_session_bounds_bootstrap_body_and_allowlists_assets() {
    let fixture = UiFixture::new();
    fixture.seed_ready();
    let server = fixture.start();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let huge = json!({"token":"x".repeat(262_145)}).to_string();
    let response = client
        .post(format!("{}/session", server.url))
        .header("Origin", &server.url)
        .header("Content-Type", "application/json")
        .body(huge)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 413);
    let session = server.session().await;
    for path in [
        "/assets/unknown",
        "/assets/%2e%2e%2fagentos.db",
        "/tasks/not-a-uuid",
        "/tasks/00000000-0000-0000-0000-000000000000",
    ] {
        let response = session
            .client
            .get(format!("{}{path}", server.url))
            .send()
            .await
            .unwrap();
        assert!(
            response.status() == 400 || response.status() == 404,
            "{path}: {}",
            response.status()
        );
        assert!(
            response.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
    }
}

#[tokio::test]
async fn review_routes_are_read_only_escaped_and_distinguish_unfinished_results() {
    let fixture = UiFixture::new();
    let mut contract: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&fixture.contract).unwrap()).unwrap();
    contract["goal"] = json!("<script>goal-sentinel</script>");
    std::fs::write(&fixture.contract, contract.to_string()).unwrap();
    let id = fixture.seed_ready();
    let server = fixture.start();
    let session = server.session().await;
    let before = fixture.events(&id);
    let status = fixture.status(&id);
    for _ in 0..2 {
        for tail in ["", "/status", "/events?after=0"] {
            let response = session
                .client
                .get(format!("{}/tasks/{}{tail}", server.url, id))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let text = response.text().await.unwrap();
            assert!(!text.contains("<script>goal-sentinel"));
            if tail.is_empty() {
                assert!(text.contains("goal-sentinel"));
                assert!(text.contains("READY"));
            }
        }
        assert_eq!(
            session
                .client
                .get(format!("{}/tasks/{id}/result", server.url))
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
    }
    assert_eq!(fixture.events(&id), before);
    assert_eq!(fixture.status(&id), status);
    assert_eq!(
        session
            .client
            .get(format!("{}/tasks/not-a-uuid/status", server.url))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        session
            .client
            .get(format!("{}/tasks/{id}/events?after=bad", server.url))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
}

#[tokio::test]
async fn terminal_review_checks_integrity_bounds_and_current_export_authority() {
    let fixture = UiFixture::new();
    let patch = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/parser-repo.fix.patch");
    let result = fixture.cli(&[
        "submit",
        fixture.contract.to_str().unwrap(),
        "--fake-agent-patch",
        patch.to_str().unwrap(),
        "--yes",
    ]);
    let id = result["task_id"].as_str().unwrap();
    assert_eq!(result["state"], "SUCCEEDED");
    let server = fixture.start();
    let session = server.session().await;
    let before = fixture.events(id);
    let response = session
        .client
        .get(format!("{}/tasks/{id}/result", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(text.contains("Verified final workspace"));
    assert!(text.contains("accepted_for_final_workspace"));
    assert_eq!(fixture.events(id), before);
    fixture.cli(&["revoke", id, "--capability", "artifact.export"]);
    let revoked = fixture.events(id);
    assert_eq!(
        session
            .client
            .get(format!("{}/tasks/{id}/result", server.url))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(fixture.events(id), revoked);
    let conn = rusqlite::Connection::open(fixture.home.join("agentos.db")).unwrap();
    conn.execute("UPDATE tasks SET contract_json=?1", ["x".repeat(300_000)])
        .unwrap();
    assert_eq!(
        session
            .client
            .get(format!("{}/tasks/{id}", server.url))
            .send()
            .await
            .unwrap()
            .status(),
        413
    );
}

#[tokio::test]
async fn export_download_is_scoped_and_matches_cli_bytes_after_gc() {
    let fixture = UiFixture::new();
    let patch = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/parser-repo.fix.patch");
    let ready = fixture.cli(&[
        "submit",
        fixture.contract.to_str().unwrap(),
        "--fake-agent-patch",
        patch.to_str().unwrap(),
        "--yes",
    ]);
    let id = ready["task_id"].as_str().unwrap();
    let server = fixture.start();
    let a = server.session().await;
    let b = server.session().await;
    let response = a.post(&server, &format!("/tasks/{id}/export"), &[]).await;
    assert_eq!(response.status(), 200);
    let html = response.text().await.unwrap();
    let path = html
        .split("href=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    assert!(path.starts_with("/downloads/"));
    let before = fixture.events(id);
    let response = a
        .client
        .get(format!("{}{path}", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "application/x-tar");
    let archive = response.bytes().await.unwrap();
    assert!(archive.windows(10).any(|w| w == b"patch.diff"));
    assert_eq!(fixture.events(id), before);
    let bundle = fixture.root.path().join("cli-bundle");
    fixture.cli(&["export", id, bundle.to_str().unwrap()]);
    let mut tar = tar::Archive::new(&archive[..]);
    for entry in tar.entries().unwrap() {
        use std::io::Read;
        let mut entry = entry.unwrap();
        assert!(entry.header().entry_type().is_file());
        let rel = entry.path().unwrap().into_owned();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        let original = std::fs::read(bundle.join(&rel)).unwrap();
        if rel == std::path::Path::new("manifest.json") {
            let mut actual: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            let mut expected: serde_json::Value = serde_json::from_slice(&original).unwrap();
            actual.as_object_mut().unwrap().remove("generated_events");
            expected.as_object_mut().unwrap().remove("generated_events");
            assert_eq!(actual, expected);
        } else {
            assert_eq!(bytes, original, "{}", rel.display());
        }
    }

    let before = fixture.events(id);
    assert_eq!(
        a.client
            .get(format!("{}/downloads/invalid", server.url))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        b.client
            .get(format!("{}{path}", server.url))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(fixture.events(id), before);
    fixture.cli(&["gc"]);
    let again = a
        .client
        .get(format!("{}{path}", server.url))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(archive, again);
    fixture.cli(&["revoke", id, "--capability", "artifact.export"]);
    let revoked = fixture.events(id);
    assert_eq!(
        a.client
            .get(format!("{}{path}", server.url))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(fixture.events(id), revoked);
}
