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

#[tokio::test]
async fn web_controls_admit_one_owned_run_and_preserve_query_responsiveness() {
    let fixture = UiFixture::slow();
    let id = fixture.seed_ready_patch();
    let digest = fixture.reviewed_digest(&id);
    let server = fixture.start();
    let session = server.session().await;
    assert_eq!(
        session
            .post(&server, &format!("/tasks/{id}/resume"), &[])
            .await
            .status(),
        409
    );
    assert_eq!(
        session
            .post(
                &server,
                &format!("/tasks/{id}/start"),
                &[("contract_digest", &digest)]
            )
            .await
            .status(),
        202
    );
    fixture.wait_entered().await;
    assert_eq!(
        session
            .post(
                &server,
                &format!("/tasks/{id}/start"),
                &[("contract_digest", &digest)]
            )
            .await
            .status(),
        409
    );
    assert_eq!(
        fixture
            .events(&id)
            .iter()
            .filter(|e| e["type"] == "CapabilitiesIssued")
            .count(),
        1
    );
    assert_eq!(
        session
            .client
            .get(format!("{}/tasks/{id}/status", server.url))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        session
            .post(&server, &format!("/tasks/{id}/pause"), &[])
            .await
            .status(),
        409
    ); // VERIFYING follows reducer rules.
    fixture.release();
    fixture.wait_state(&id, "SUCCEEDED").await;
    assert_eq!(
        session
            .post(&server, &format!("/tasks/{id}/resume"), &[])
            .await
            .status(),
        200
    );
    assert_eq!(
        session
            .post(&server, &format!("/tasks/{id}/cancel"), &[])
            .await
            .status(),
        200
    );
}

#[tokio::test]
async fn web_controls_refuse_forged_review_missing_agent_and_external_driver() {
    let fixture = UiFixture::new();
    let id = fixture.seed_ready();
    let digest = fixture.reviewed_digest(&id);
    let server = fixture.start();
    let session = server.session().await;
    let before = fixture.events(&id);
    let wrong = "0".repeat(64);
    assert_eq!(
        session
            .post(
                &server,
                &format!("/tasks/{id}/start"),
                &[("contract_digest", &wrong)]
            )
            .await
            .status(),
        409
    );
    assert_eq!(
        session
            .post(
                &server,
                &format!("/tasks/{id}/start"),
                &[("contract_digest", &digest)]
            )
            .await
            .status(),
        400
    );
    assert_eq!(fixture.events(&id), before);
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(fixture.home.join("driver.lock"))
        .unwrap();
    lock.lock().unwrap();
    let runnable = fixture.seed_ready_patch();
    let reviewed = fixture.reviewed_digest(&runnable);
    assert_eq!(
        session
            .post(
                &server,
                &format!("/tasks/{runnable}/start"),
                &[("contract_digest", &reviewed)]
            )
            .await
            .status(),
        409
    );
    drop(lock);
    assert_eq!(fixture.status(&runnable)["state"], "READY");
    assert!(
        !fixture
            .events(&runnable)
            .iter()
            .any(|e| e["type"] == "CapabilitiesIssued")
    );
}

#[tokio::test]
async fn web_controls_cancel_pending_external_driver_and_active_verification() {
    let fixture = UiFixture::slow();
    let id = fixture.seed_ready_patch();
    let server = fixture.start();
    let session = server.session().await;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(fixture.home.join("driver.lock"))
        .unwrap();
    lock.lock().unwrap();
    assert_eq!(
        session
            .post(&server, &format!("/tasks/{id}/cancel"), &[])
            .await
            .status(),
        202
    );
    assert_eq!(fixture.status(&id)["cancel_requested"], true);
    assert_eq!(fixture.status(&id)["state"], "READY");
    drop(lock);
    session
        .post(&server, &format!("/tasks/{id}/cancel"), &[])
        .await;
    fixture.wait_state(&id, "CANCELLED").await;
    let active = fixture.seed_ready_patch();
    let digest = fixture.reviewed_digest(&active);
    assert_eq!(
        session
            .post(
                &server,
                &format!("/tasks/{active}/start"),
                &[("contract_digest", &digest)]
            )
            .await
            .status(),
        202
    );
    fixture.wait_entered().await;
    assert_eq!(
        session
            .post(&server, &format!("/tasks/{active}/cancel"), &[])
            .await
            .status(),
        202
    );
    fixture.wait_state(&active, "CANCELLED").await;
}

#[tokio::test]
async fn web_controls_normal_shutdown_drains_owned_driver() {
    let fixture = UiFixture::slow();
    let id = fixture.seed_ready_patch();
    let digest = fixture.reviewed_digest(&id);
    let mut server = fixture.start();
    let session = server.session().await;
    assert_eq!(
        session
            .post(
                &server,
                &format!("/tasks/{id}/start"),
                &[("contract_digest", &digest)]
            )
            .await
            .status(),
        202
    );
    fixture.wait_entered().await;
    assert!(
        std::process::Command::new("kill")
            .args(["-TERM", &server.child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(server.child.try_wait().unwrap().is_none());
    fixture.release();
    fixture.wait_state(&id, "SUCCEEDED").await;
    let start = std::time::Instant::now();
    while server.child.try_wait().unwrap().is_none() {
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn contract_form_coalesces_duplicates_and_binds_approval_to_recorded_inputs() {
    let fixture = UiFixture::new();
    let server = fixture.start();
    let session = server.session().await;
    let nonce = session.nonce(&server).await;
    let contract = std::fs::read_to_string(&fixture.contract).unwrap();
    let model = fixture.model();
    let (first, repeated) = tokio::join!(
        session.create(&server, &nonce, &contract, &model),
        session.create(&server, &nonce, &contract, &model)
    );
    assert_eq!(first.task_id, repeated.task_id);
    let replay = session.create(&server, &nonce, &contract, &model).await;
    assert_eq!(first.task_id, replay.task_id);
    let before = fixture.events(&first.task_id);
    assert!(!before.iter().any(|e| e["type"] == "CapabilitiesIssued"));
    assert_eq!(
        session
            .post(
                &server,
                "/tasks",
                &[
                    ("nonce", &nonce),
                    ("contract_json", "{}"),
                    ("model", &model),
                    ("worker", "host")
                ]
            )
            .await
            .status(),
        409
    );
    fixture.change_staged_profile(&first.task_id);
    assert_eq!(
        session
            .post(
                &server,
                &format!("/tasks/{}/start", first.task_id),
                &[("contract_digest", &first.contract_digest)]
            )
            .await
            .status(),
        409
    );
    assert_eq!(fixture.status(&first.task_id)["state"], "READY");
    assert_eq!(fixture.events(&first.task_id), before);
    let nonce = session.nonce(&server).await;
    let second = session.create(&server, &nonce, &contract, &model).await;
    std::fs::write(
        fixture.repo.join("src/parser.py"),
        "source changed after recording",
    )
    .unwrap();
    assert_eq!(
        session
            .post(
                &server,
                &format!("/tasks/{}/start", second.task_id),
                &[("contract_digest", &second.contract_digest)]
            )
            .await
            .status(),
        202
    );
    fixture.wait_state(&second.task_id, "SUCCEEDED").await;
}

#[tokio::test]
async fn contract_form_rejects_invalid_inputs_and_stale_or_cross_session_nonce() {
    let fixture = UiFixture::new();
    let mut server = fixture.start();
    let session = server.session().await;
    let contract = std::fs::read_to_string(&fixture.contract).unwrap();
    let model = fixture.model();
    for (body, agent, worker) in [
        ("bad".to_string(), model.clone(), "host"),
        ("{}".to_string(), model.clone(), "host"),
        (contract.clone(), "bad-model".into(), "host"),
        (contract.clone(), "".into(), "host"),
        (contract.clone(), model.clone(), "firecracker"),
    ] {
        let nonce = session.nonce(&server).await;
        assert_eq!(
            session
                .post(
                    &server,
                    "/tasks",
                    &[
                        ("nonce", &nonce),
                        ("contract_json", &body),
                        ("model", &agent),
                        ("worker", worker)
                    ]
                )
                .await
                .status(),
            400
        );
    }
    for field in ["source", "profile", "transcript"] {
        let mut body: serde_json::Value = serde_json::from_str(&contract).unwrap();
        let mut agent = model.clone();
        if field == "source" {
            body["repository"]["source"] = json!("/missing-repository");
        }
        if field == "profile" {
            body["verification_profile"] = json!("missing-profile");
        }
        if field == "transcript" {
            agent = "fake:/missing-transcript".into();
        }
        let nonce = session.nonce(&server).await;
        assert_eq!(
            session
                .post(
                    &server,
                    "/tasks",
                    &[
                        ("nonce", &nonce),
                        ("contract_json", &body.to_string()),
                        ("model", &agent),
                        ("worker", "host")
                    ]
                )
                .await
                .status(),
            400
        );
    }
    let nonce = session.nonce(&server).await;
    let other = server.session().await;
    assert_eq!(
        other
            .post(
                &server,
                "/tasks",
                &[
                    ("nonce", &nonce),
                    ("contract_json", &contract),
                    ("model", &model),
                    ("worker", "host")
                ]
            )
            .await
            .status(),
        410
    );
    server.stop();
    let restarted = fixture.start();
    let fresh = restarted.session().await;
    assert_eq!(
        fresh
            .post(
                &restarted,
                "/tasks",
                &[
                    ("nonce", &nonce),
                    ("contract_json", &contract),
                    ("model", &model),
                    ("worker", "host")
                ]
            )
            .await
            .status(),
        410
    );
}

#[tokio::test]
async fn contract_form_missing_key_start_preserves_ready_and_grants() {
    let fixture = UiFixture::new();
    let server = fixture.start();
    let session = server.session().await;
    let nonce = session.nonce(&server).await;
    let contract = std::fs::read_to_string(&fixture.contract).unwrap();
    let created = session
        .create(&server, &nonce, &contract, "anthropic:test-model")
        .await;
    std::fs::remove_file(fixture.root.path().join("provider-key")).unwrap();
    let before = fixture.events(&created.task_id);
    let response = session
        .post(
            &server,
            &format!("/tasks/{}/start", created.task_id),
            &[("contract_digest", &created.contract_digest)],
        )
        .await;
    assert_eq!(response.status(), 400);
    assert_eq!(fixture.status(&created.task_id)["state"], "READY");
    assert_eq!(fixture.events(&created.task_id), before);
    assert!(!before.iter().any(|e| e["type"] == "CapabilitiesIssued"));
}
