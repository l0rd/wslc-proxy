//! End-to-end tests: run socktainer against the real `wslc` and talk to the
//! Docker engine inside the session VM.
//!
//! They need Windows with WSL (including `wslc`) and a built `docker-relay`,
//! so they are ignored by default. Run them with:
//!
//!   cargo test --test e2e -- --ignored
//!
//! The relay binary is looked up in $SOCKTAINER_E2E_DIR, or the workspace
//! root if that is unset.

mod common;

use std::env;
use std::io::Read;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use rstest::{fixture, rstest};
use serde_json::{json, Value};

use common::{free_addr, Socktainer, SOCKTAINER};

/// Covers a cold start: booting the VM and pulling the keepalive image.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(300);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Every socktainer instance uses the same session VM and keepalive
/// container, so only one test may run at a time.
static SESSION: Mutex<()> = Mutex::new(());

struct Docker {
    base_url: String,
    agent: ureq::Agent,
    _socktainer: Socktainer,
    _session: MutexGuard<'static, ()>,
}

impl Docker {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    fn get(&self, path: &str) -> ureq::Response {
        self.agent
            .get(&self.url(path))
            .call()
            .unwrap_or_else(|err| panic!("GET {path}: {err}"))
    }

    fn get_json(&self, path: &str) -> Value {
        self.get(path)
            .into_json()
            .unwrap_or_else(|err| panic!("GET {path}: {err}"))
    }

    fn post(&self, path: &str) -> ureq::Response {
        self.agent
            .post(&self.url(path))
            .call()
            .unwrap_or_else(|err| panic!("POST {path}: {err}"))
    }

    fn post_json(&self, path: &str, body: Value) -> Value {
        self.agent
            .post(&self.url(path))
            .send_json(body)
            .unwrap_or_else(|err| panic!("POST {path}: {err}"))
            .into_json()
            .unwrap_or_else(|err| panic!("POST {path}: {err}"))
    }

    fn delete(&self, path: &str) {
        self.agent
            .delete(&self.url(path))
            .call()
            .unwrap_or_else(|err| panic!("DELETE {path}: {err}"));
    }
}

#[fixture]
fn docker() -> Docker {
    let session = SESSION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let dir = env::var_os("SOCKTAINER_E2E_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".."));
    assert!(
        dir.join("docker-relay").exists(),
        "docker-relay not found in {}; build it first (see README.md)",
        dir.display()
    );

    let addr = free_addr();
    let mut cmd = Command::new(SOCKTAINER);
    cmd.current_dir(dir).env("SOCKTAINER_LISTEN", &addr);
    let socktainer = Socktainer::start(cmd, STARTUP_TIMEOUT);

    Docker {
        base_url: format!("http://{addr}"),
        agent: ureq::AgentBuilder::new().timeout(REQUEST_TIMEOUT).build(),
        _socktainer: socktainer,
        _session: session,
    }
}

#[rstest]
#[ignore = "needs wslc"]
fn answers_ping(docker: Docker) {
    let body = docker.get("/_ping").into_string().expect("read body");
    assert_eq!(body, "OK");
}

#[rstest]
#[ignore = "needs wslc"]
fn reports_engine_version(docker: Docker) {
    let version = docker.get_json("/version");
    assert!(
        version["ApiVersion"].is_string(),
        "unexpected response: {version}"
    );
}

#[rstest]
#[ignore = "needs wslc"]
fn lists_keepalive_container(docker: Docker) {
    let containers = docker.get_json("/containers/json");
    let names: Vec<&str> = containers
        .as_array()
        .expect("container list")
        .iter()
        .flat_map(|container| container["Names"].as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .collect();
    assert!(
        names.contains(&"/socktainer-keepalive"),
        "running containers: {names:?}"
    );
}

#[rstest]
#[ignore = "needs wslc"]
fn runs_container_and_streams_its_logs(docker: Docker) {
    const GREETING: &str = "hello from socktainer";

    let created = docker.post_json(
        "/containers/create",
        json!({ "Image": "alpine:latest", "Cmd": ["echo", GREETING] }),
    );
    let id = created["Id"]
        .as_str()
        .unwrap_or_else(|| panic!("no container id in {created}"));

    docker.post(&format!("/containers/{id}/start"));
    let exit: Value = docker
        .post(&format!("/containers/{id}/wait"))
        .into_json()
        .expect("wait response");

    // The log stream is multiplexed (8-byte frame headers), so look for the
    // text rather than comparing the whole body.
    let mut logs = Vec::new();
    docker
        .get(&format!("/containers/{id}/logs?stdout=1"))
        .into_reader()
        .read_to_end(&mut logs)
        .expect("read logs");

    docker.delete(&format!("/containers/{id}?force=1"));

    assert_eq!(exit["StatusCode"], 0, "unexpected wait response: {exit}");
    let logs = String::from_utf8_lossy(&logs);
    assert!(logs.contains(GREETING), "unexpected logs: {logs:?}");
}

#[rstest]
#[ignore = "needs wslc"]
fn serves_requests_in_parallel(docker: Docker) {
    thread::scope(|scope| {
        let docker = &docker;
        let requests: Vec<_> = (0..8)
            .map(|_| scope.spawn(move || docker.get("/_ping").into_string().expect("read body")))
            .collect();
        for request in requests {
            assert_eq!(request.join().expect("request thread"), "OK");
        }
    });
}
