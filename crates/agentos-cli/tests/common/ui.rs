//! Real-process fixtures. No test-only lifecycle changes or provider credentials.
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
use tempfile::TempDir;

pub struct UiFixture {
    pub root: TempDir,
    pub home: PathBuf,
    pub repo: PathBuf,
    pub contract: PathBuf,
    pub profiles: PathBuf,
}
pub struct UiServer {
    pub url: String,
    pub launch_url: String,
    pub child: Child,
    pub stderr: Arc<Mutex<String>>,
}
pub struct UiSession {
    pub client: reqwest::Client,
    pub csrf: String,
}
impl UiFixture {
    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures")
            .canonicalize()
            .unwrap();
        let repo = root.path().join("repo");
        agentos_engine::workspace::copy_tree(&fixture.join("parser-repo"), &repo).unwrap();
        let contract = root.path().join("task.json");
        fs::write(&contract,json!({
            "goal":"fix the parser","repository":{"source":repo,"revision":"recorded-at-submission"},
            "profile":"python-stdlib-v1","verification_profile":"parser-checks-v1","editable_paths":["src/**"],
            "capabilities":["snapshot.read","workspace.apply_patch","verification.run","artifact.export","model.request"],
            "limits":{"model_requests":12,"max_output_tokens_per_request":4096,"tool_actions":50,"deadline_seconds":600,"worker_vcpus":1,"worker_memory_mib":256}
        }).to_string()).unwrap();
        let home = root.path().join("home");
        Self {
            root,
            home,
            repo,
            contract,
            profiles: fixture.join("profiles"),
        }
    }
    pub fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_agentos"));
        c.args([
            "--home",
            self.home.to_str().unwrap(),
            "--profiles",
            self.profiles.to_str().unwrap(),
        ]);
        for name in [
            "ANTHROPIC_API_KEY",
            "AGENTOS_API_KEY_FILE",
            "AGENTOS_ANTHROPIC_BASE_URL",
            "AGENTOS_WORKER",
            "AGENTOS_FIRECRACKER",
            "AGENTOS_JAILER",
            "AGENTOS_ALLOW_UNJAILED",
            "AGENTOS_TEST_WORKERS",
            "AGENTOS_TEST_FAKE_GUEST",
            "AGENTOS_TEST_JAIL_PROBE",
        ] {
            c.env_remove(name);
        }
        c
    }
    pub fn cli(&self, args: &[&str]) -> Value {
        let out = self.command().args(args).output().unwrap();
        assert!(
            out.status.success(),
            "CLI failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
    pub fn seed_ready(&self) -> String {
        self.cli(&["submit", self.contract.to_str().unwrap()])["task_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }
    pub fn status(&self, id: &str) -> Value {
        self.cli(&["status", id])
    }
    pub fn events(&self, id: &str) -> Vec<Value> {
        let out = self.command().args(["events", id]).output().unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect()
    }
    pub fn start(&self) -> UiServer {
        let secret = self.root.path().join("provider-key");
        fs::write(&secret, "provider-key-sentinel\n").unwrap();
        let mut child = self
            .command()
            .args([
                "--api-key-file",
                secret.to_str().unwrap(),
                "ui",
                "--port",
                "0",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = Arc::new(Mutex::new(String::new()));
        let errors = Arc::clone(&stderr);
        let err = child.stderr.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let mut guard = errors.lock().unwrap();
                if guard.len() < 64 * 1024 {
                    guard.push_str(&line);
                    guard.push('\n');
                }
            }
        });
        let out = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(out).read_line(&mut line);
            let _ = tx.send((result, line));
        });
        let (_, line) = rx
            .recv_timeout(Duration::from_secs(15))
            .unwrap_or_else(|_| {
                let _ = child.kill();
                panic!(
                    "UI did not announce a loopback listener: {}",
                    stderr.lock().unwrap()
                );
            });
        let startup: Value = serde_json::from_str(&line).unwrap_or_else(|_| {
            let _ = child.kill();
            panic!("UI unavailable: {}", stderr.lock().unwrap());
        });
        UiServer {
            url: startup["listening"].as_str().unwrap().into(),
            launch_url: startup["launch_url"].as_str().unwrap().into(),
            child,
            stderr,
        }
    }
}
impl UiServer {
    pub async fn session(&self) -> UiSession {
        let client = reqwest::Client::builder()
            .no_proxy()
            .cookie_store(true)
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let token = self.launch_url.split('#').nth(1).unwrap();
        let response = client
            .post(format!("{}/session", self.url))
            .header("Origin", &self.url)
            .header("Content-Type", "application/json")
            .body(json!({"token":token}).to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let value: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
        UiSession {
            client,
            csrf: value["csrf"].as_str().unwrap().into(),
        }
    }
    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Drop for UiServer {
    fn drop(&mut self) {
        self.stop();
    }
}
impl UiSession {
    pub async fn post(
        &self,
        server: &UiServer,
        path: &str,
        fields: &[(&str, &str)],
    ) -> reqwest::Response {
        self.client
            .post(format!("{}{path}", server.url))
            .header("Origin", &server.url)
            .header("X-CSRF-Token", &self.csrf)
            .form(fields)
            .send()
            .await
            .unwrap()
    }
}
impl UiFixture {
    pub fn seed_ready_patch(&self) -> String {
        let patch =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/parser-repo.fix.patch");
        self.cli(&[
            "submit",
            self.contract.to_str().unwrap(),
            "--fake-agent-patch",
            patch.to_str().unwrap(),
        ])["task_id"]
            .as_str()
            .unwrap()
            .into()
    }
    pub fn reviewed_digest(&self, id: &str) -> String {
        self.events(id)
            .into_iter()
            .find(|e| e["type"] == "TaskCreated")
            .unwrap()["payload"]["contract_digest"]
            .as_str()
            .unwrap()
            .into()
    }
    pub fn slow() -> Self {
        let mut fixture = Self::new();
        let profiles = fixture.root.path().join("profiles");
        agentos_engine::workspace::copy_tree(&fixture.profiles, &profiles).unwrap();
        fixture.profiles = profiles;
        let path = fixture.profiles.join("parser-checks-v1/check_parser.py");
        let original = fs::read_to_string(&path).unwrap();
        let entered = serde_json::to_string(&fixture.root.path().join("entered")).unwrap();
        let release = serde_json::to_string(&fixture.root.path().join("release")).unwrap();
        fs::write(path,format!("import pathlib,time\npathlib.Path({entered}).write_text('entered')\nwhile not pathlib.Path({release}).exists(): time.sleep(0.02)\n{original}")).unwrap();
        fixture
    }
    pub fn release(&self) {
        fs::write(self.root.path().join("release"), b"release").unwrap();
    }
    pub async fn wait_entered(&self) {
        let start = std::time::Instant::now();
        while !self.root.path().join("entered").exists() {
            assert!(
                start.elapsed() < Duration::from_secs(15),
                "verification never entered"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    pub async fn wait_state(&self, id: &str, state: &str) {
        let start = std::time::Instant::now();
        loop {
            if self.status(id)["state"] == state {
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "state did not reach {state}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
