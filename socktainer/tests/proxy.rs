//! Proxy tests: run the real socktainer binary against a fake `wslc`
//! (examples/fake_wslc.rs), so they need neither Windows nor WSL.

mod common;

use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use rstest::{fixture, rstest};
use tempfile::TempDir;

use common::{free_addr, Socktainer, SOCKTAINER};

const RELAY_BYTES: &[u8] = b"\x7fELF not really a relay binary\n\x00\x01\x02";
const IO_TIMEOUT: Duration = Duration::from_secs(30);

const PULL: &str = "image pull alpine:latest";
const REMOVE_STALE_KEEPALIVE: &str = "container rm -f socktainer-keepalive";
const START_KEEPALIVE: &str = "run -d --name socktainer-keepalive alpine:latest sleep infinity";
const DEPLOY: &str =
    "system session run /bin/sh -c cat > /tmp/docker-relay && chmod +x /tmp/docker-relay";
const RELAY: &str = "system session run /tmp/docker-relay";
const STOP_KEEPALIVE: &str = "container stop socktainer-keepalive";
const REMOVE_KEEPALIVE: &str = "container rm socktainer-keepalive";

/// cargo builds examples next to the binaries whenever tests are run without
/// a target filter.
fn fake_wslc() -> PathBuf {
    let path = Path::new(SOCKTAINER)
        .parent()
        .expect("socktainer binary has a parent directory")
        .join("examples")
        .join(format!("fake_wslc{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.exists(),
        "{} not found; run `cargo test` without a target filter, or `cargo build --examples` first",
        path.display()
    );
    path
}

/// A working directory holding a relay binary, plus the environment that
/// points socktainer at the fake wslc.
struct Sandbox {
    dir: TempDir,
    addr: String,
}

impl Sandbox {
    fn command(&self) -> Command {
        let mut cmd = Command::new(SOCKTAINER);
        cmd.current_dir(self.dir.path())
            .env("SOCKTAINER_LISTEN", &self.addr)
            .env("SOCKTAINER_WSLC", fake_wslc())
            .env("FAKE_WSLC_LOG", self.log_path())
            .env("FAKE_WSLC_DEPLOY", self.deployed_path());
        cmd
    }

    fn log_path(&self) -> PathBuf {
        self.dir.path().join("wslc.log")
    }

    fn deployed_path(&self) -> PathBuf {
        self.dir.path().join("deployed-relay")
    }

    /// The wslc invocations made so far, one per line.
    fn wslc_calls(&self) -> Vec<String> {
        fs::read_to_string(self.log_path())
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

#[fixture]
fn sandbox() -> Sandbox {
    let dir = tempfile::tempdir().expect("create temp dir");
    fs::write(dir.path().join("docker-relay"), RELAY_BYTES).expect("write relay binary");
    Sandbox {
        dir,
        addr: free_addr(),
    }
}

/// A socktainer that has finished bootstrapping and is accepting connections.
struct Proxy {
    sandbox: Sandbox,
    _socktainer: Socktainer,
}

impl Proxy {
    fn connect(&self) -> TcpStream {
        let conn = TcpStream::connect(&self.sandbox.addr).expect("connect to socktainer");
        conn.set_read_timeout(Some(IO_TIMEOUT))
            .expect("set read timeout");
        conn.set_write_timeout(Some(IO_TIMEOUT))
            .expect("set write timeout");
        conn
    }

    /// Sends `payload`, half-closes, and returns everything sent back.
    fn round_trip(&self, payload: &[u8]) -> Vec<u8> {
        let mut conn = self.connect();
        let mut writer = conn.try_clone().expect("clone connection");
        thread::scope(|scope| {
            scope.spawn(move || {
                writer.write_all(payload).expect("send payload");
                writer.shutdown(Shutdown::Write).expect("half-close");
            });
            let mut received = Vec::new();
            conn.read_to_end(&mut received).expect("read reply");
            received
        })
    }
}

#[fixture]
fn proxy(sandbox: Sandbox) -> Proxy {
    let socktainer = Socktainer::start(sandbox.command(), IO_TIMEOUT);
    Proxy {
        sandbox,
        _socktainer: socktainer,
    }
}

#[rstest]
fn bootstrap_runs_wslc_commands_in_order(proxy: Proxy) {
    assert_eq!(
        proxy.sandbox.wslc_calls(),
        [PULL, REMOVE_STALE_KEEPALIVE, START_KEEPALIVE, DEPLOY]
    );
}

#[rstest]
fn bootstrap_deploys_relay_binary_unmodified(proxy: Proxy) {
    let deployed = fs::read(proxy.sandbox.deployed_path()).expect("read deployed relay");
    assert_eq!(deployed, RELAY_BYTES);
}

#[rstest]
#[case::single_byte(1)]
#[case::one_page(4096)]
#[case::one_megabyte(1024 * 1024)]
fn relays_bytes_in_both_directions(proxy: Proxy, #[case] size: usize) {
    let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    assert_eq!(proxy.round_trip(&payload), payload);
}

#[rstest]
fn relays_data_while_connection_stays_open(proxy: Proxy) {
    let mut conn = proxy.connect();
    for message in [b"ping", b"pong"] {
        conn.write_all(message).expect("send message");
        let mut reply = [0u8; 4];
        conn.read_exact(&mut reply).expect("read reply");
        assert_eq!(&reply, message);
    }
}

#[rstest]
fn closes_connection_when_relay_exits(proxy: Proxy) {
    let mut conn = proxy.connect();
    conn.shutdown(Shutdown::Write).expect("half-close");
    let mut rest = Vec::new();
    conn.read_to_end(&mut rest).expect("read until close");
    assert!(rest.is_empty());
}

#[rstest]
fn spawns_one_relay_per_connection(proxy: Proxy) {
    for _ in 0..3 {
        proxy.round_trip(b"x");
    }
    let relays = proxy
        .sandbox
        .wslc_calls()
        .iter()
        .filter(|call| *call == RELAY)
        .count();
    assert_eq!(relays, 3);
}

#[rstest]
fn serves_connections_concurrently(proxy: Proxy) {
    thread::scope(|scope| {
        let proxy = &proxy;
        let clients: Vec<_> = (0..16)
            .map(|i| {
                scope.spawn(move || {
                    let payload = format!("client {i} ").repeat(1000).into_bytes();
                    assert_eq!(proxy.round_trip(&payload), payload);
                })
            })
            .collect();
        for client in clients {
            client.join().expect("client thread");
        }
    });
}

#[rstest]
#[case::image_pull(PULL, &[PULL])]
#[case::keepalive_start(
    START_KEEPALIVE,
    &[PULL, REMOVE_STALE_KEEPALIVE, START_KEEPALIVE],
)]
#[case::relay_deploy(
    DEPLOY,
    &[PULL, REMOVE_STALE_KEEPALIVE, START_KEEPALIVE, DEPLOY, STOP_KEEPALIVE, REMOVE_KEEPALIVE],
)]
fn exits_with_error_when_wslc_fails(
    sandbox: Sandbox,
    #[case] failing: &str,
    #[case] expected_calls: &[&str],
) {
    let output = sandbox
        .command()
        .env("FAKE_WSLC_FAIL", failing)
        .output()
        .expect("run socktainer");

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(sandbox.wslc_calls(), expected_calls);
}

#[rstest]
fn removes_keepalive_when_port_is_taken(sandbox: Sandbox) {
    let _occupied = TcpListener::bind(&sandbox.addr).expect("occupy the listen address");

    let output = sandbox.command().output().expect("run socktainer");

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Listen failed"));
    assert_eq!(
        sandbox.wslc_calls(),
        [
            PULL,
            REMOVE_STALE_KEEPALIVE,
            START_KEEPALIVE,
            DEPLOY,
            STOP_KEEPALIVE,
            REMOVE_KEEPALIVE
        ]
    );
}

#[rstest]
fn refuses_to_start_without_relay_binary(sandbox: Sandbox) {
    fs::remove_file(sandbox.dir.path().join("docker-relay")).expect("remove relay binary");

    let output = sandbox.command().output().expect("run socktainer");

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("not found"));
    assert!(sandbox.wslc_calls().is_empty());
}
