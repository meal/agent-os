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
