#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use rusqlite::{params, Connection};
use serde_json::{json, Value};

struct App {
    root: PathBuf,
    child: Option<Child>,
    port: u16,
    http: reqwest::Client,
}

impl App {
    async fn new() -> Self {
        let root = std::env::temp_dir().join(format!("orx-compact-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(
            root.join("peer.mjs"),
            include_str!("fixtures/codex-compaction.mjs"),
        )
        .unwrap();
        let bin = root.join("bin/codex");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\nexec node '{}' \"$@\"\n",
                root.join("peer.mjs").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut app = Self {
            root,
            child: None,
            port,
            http: reqwest::Client::new(),
        };
        app.start().await;
        app
    }

    async fn start(&mut self) {
        self.child = Some(
            Command::new(env!("CARGO_BIN_EXE_orx"))
                .args(["--no-telemetry", "up", "--no-browser", "--port"])
                .arg(self.port.to_string())
                .env("HOME", &self.root)
                .env("XDG_CONFIG_HOME", self.root.join("config"))
                .env("ORX_DATA_DIR", self.root.join("data"))
                .env("ORX_CACHE_DIR", self.root.join("cache"))
                .env("CODEX_HOME", self.root.join("codex"))
                .env("ORX_COMPACT_TEST_DIR", &self.root)
                .env("ORX_NO_UPDATE_CHECK", "1")
                .env(
                    "PATH",
                    format!(
                        "{}:{}",
                        self.root.join("bin").display(),
                        std::env::var("PATH").unwrap()
                    ),
                )
                .env_remove("ORX_CODEX_EXEC")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if self.http.get(self.url("/api/health")).send().await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            unsafe {
                libc::kill(child.id() as i32, libc::SIGINT);
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
    fn db(&self) -> Connection {
        Connection::open(self.root.join("data/orx.db")).unwrap()
    }
    fn mode(&self, mode: &str) {
        std::fs::write(self.root.join("mode"), mode).unwrap();
    }
    fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(self.root.join("requests.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn seed(&self, id: &str, history: bool, rollout: bool) {
        let db = self.db();
        db.execute("INSERT INTO chat_sessions (id, project_id, harness, native_session_id, created_at, updated_at, active_leaf_id) VALUES (?1, 'fixture', 'codex', ?1, 1, 1, ?2)", params![id, history.then(|| format!("{id}-message"))]).unwrap();
        if history {
            db.execute("INSERT INTO chat_messages (id, session_id, role, parts_json, created_at, completed_at) VALUES (?1, ?2, 'user', ?3, 1, 2)", params![format!("{id}-message"), id, json!([{"id":"text", "type":"text", "text":"Saved conversation"}]).to_string()]).unwrap();
        }
        if rollout {
            let sessions = self.root.join("data/agents/codex/sessions");
            std::fs::create_dir_all(&sessions).unwrap();
            std::fs::write(sessions.join(format!("{id}.jsonl")), "{}\n").unwrap();
        }
    }

    async fn compact(&self, id: &str) -> Value {
        let response = self
            .http
            .post(self.url(&format!("/api/chat/sessions/{id}/compact")))
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "{}",
            response.text().await.unwrap()
        );
        response.json::<Value>().await.unwrap()
    }

    async fn settle(&self, marker: &Value, status: &str) {
        let id = marker["message"]["id"].as_str().unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let parts: String = self
                    .db()
                    .query_row(
                        "SELECT parts_json FROM chat_messages WHERE id = ?1",
                        [id],
                        |r| r.get(0),
                    )
                    .unwrap();
                let parts: Value = serde_json::from_str(&parts).unwrap();
                let actual = parts[0]["state"]["status"].as_str().unwrap();
                if actual != "running" {
                    assert_eq!(actual, status, "{parts}");
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn wait_request(&self, method: &str, after: usize) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !self
                .requests()
                .iter()
                .skip(after)
                .any(|r| r["method"] == method)
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Exercise the real HTTP/host/harness path without credentials or inference.
#[tokio::test]
async fn inactive_codex_compaction_restores_the_thread_and_stays_cancellable() {
    let mut app = App::new().await;
    app.seed("saved", true, true);
    let marker = app.compact("saved").await;
    app.settle(&marker, "completed").await;
    let first = app.requests();
    let resume = first
        .iter()
        .find(|r| r["method"] == "thread/resume")
        .unwrap();
    assert_eq!(resume["params"]["threadId"], "saved");
    assert_eq!(
        resume["params"]["path"],
        app.root
            .join("data/agents/codex/sessions/saved.jsonl")
            .to_string_lossy()
            .as_ref()
    );
    app.settle(&app.compact("saved").await, "completed").await;
    assert_eq!(
        app.requests()
            .iter()
            .filter(|r| r["method"] == "thread/resume")
            .count(),
        1,
        "warm compaction must not resume twice"
    );

    app.stop();
    app.start().await;
    app.settle(&app.compact("saved").await, "completed").await;
    assert_eq!(
        app.requests()
            .iter()
            .filter(|r| r["method"] == "thread/resume")
            .count(),
        2
    );
    let db = app.db();
    let native: String = db
        .query_row(
            "SELECT native_session_id FROM chat_sessions WHERE id = 'saved'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(native, "saved");
    let users: i64 = db
        .query_row(
            "SELECT count(*) FROM chat_messages WHERE session_id = 'saved' AND role = 'user'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(users, 1, "compaction must not insert synthetic user input");
    assert!(app
        .requests()
        .iter()
        .all(|r| !matches!(r["method"].as_str(), Some("turn/start" | "thread/start"))));

    app.seed("missing", true, false);
    app.settle(&app.compact("missing").await, "error").await;
    app.seed("rejected", true, true);
    app.mode("reject-resume");
    app.settle(&app.compact("rejected").await, "error").await;
    assert!(!app
        .requests()
        .iter()
        .any(|r| r["method"] == "thread/compact/start" && r["params"]["threadId"] == "rejected"));

    app.seed("wrong-thread", true, true);
    app.mode("wrong-thread");
    app.settle(&app.compact("wrong-thread").await, "error")
        .await;
    assert!(!app.requests().iter().any(
        |r| r["method"] == "thread/compact/start" && r["params"]["threadId"] == "wrong-thread"
    ));

    // Older chats live under the user Codex home, not orx's isolated home.
    app.seed("legacy", true, true);
    let legacy = app.root.join("codex/sessions/legacy.jsonl");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::fs::rename(
        app.root.join("data/agents/codex/sessions/legacy.jsonl"),
        &legacy,
    )
    .unwrap();
    app.mode("");
    app.settle(&app.compact("legacy").await, "completed").await;
    assert!(app.requests().iter().any(|r| r["method"] == "thread/resume"
        && r["params"]["path"] == legacy.to_string_lossy().as_ref()));

    app.seed("empty", false, false);
    let response = app
        .http
        .post(app.url("/api/chat/sessions/empty/compact"))
        .send()
        .await
        .unwrap();
    assert!(!response.status().is_success());
    assert!(response
        .text()
        .await
        .unwrap()
        .contains("nothing to compact"));

    for (id, mode, method) in [
        ("cancel-startup", "stall-initialize", "initialize"),
        ("cancel-turn", "stall-compact", "thread/compact/start"),
    ] {
        app.seed(id, true, true);
        app.mode(mode);
        let before = app.requests().len();
        let marker = app.compact(id).await;
        app.wait_request(method, before).await;
        let busy = app
            .http
            .post(app.url(&format!("/api/chat/sessions/{id}/compact")))
            .send()
            .await
            .unwrap();
        assert_eq!(busy.status(), reqwest::StatusCode::CONFLICT);
        let pid = app
            .requests()
            .iter()
            .skip(before)
            .find(|r| r["method"] == method)
            .unwrap()["pid"]
            .as_i64()
            .unwrap() as i32;
        let response = app
            .http
            .post(app.url(&format!("/api/chat/sessions/{id}/interrupt")))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        app.settle(&marker, "error").await;
        tokio::time::timeout(Duration::from_secs(10), async {
            while unsafe { libc::kill(pid, 0) } == 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }
}
