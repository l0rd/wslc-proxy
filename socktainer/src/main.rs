//! Socktainer for wslc — exposes the Docker API from a wslc session VM
//! as a TCP endpoint on the Windows host.
//!
//! Architecture:
//!
//!   Docker client ──TCP:2375──► socktainer ──stdio──► wslc session run ──HvSocket──► relay ──► docker.sock
//!
//! On startup, socktainer:
//!   1. Pulls alpine:latest (also boots the VM if the session doesn't exist yet)
//!   2. Starts a keepalive container (prevents VM idle-termination between requests)
//!   3. Deploys the relay binary into the VM's root namespace
//!   4. Listens on TCP:2375
//!
//! For each incoming TCP connection, socktainer spawns:
//!   wslc system session run /tmp/docker-relay
//! which connects to /var/run/docker.sock inside the VM and relays
//! bytes bidirectionally over stdin/stdout.
//!
//! On shutdown (Ctrl-C), the keepalive container is stopped and removed.
//!
//! Environment overrides (mainly for tests):
//!   SOCKTAINER_LISTEN  address to listen on (default: localhost:2375)
//!   SOCKTAINER_WSLC    wslc executable to run (default: wslc)
//!
//! See README.md for build and usage instructions.

use std::env;
use std::fs;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{self, Child, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::thread;

const DEFAULT_LISTEN_ADDR: &str = "localhost:2375";
const DEFAULT_WSLC: &str = "wslc";
const RELAY_BIN_HOST: &str = "docker-relay";
const RELAY_BIN_GUEST: &str = "/tmp/docker-relay";
const KEEPALIVE_IMAGE: &str = "alpine:latest";
const KEEPALIVE_NAME: &str = "socktainer-keepalive";

static ACTIVE_CONNS: AtomicI64 = AtomicI64::new(0);

fn main() {
    if !Path::new(RELAY_BIN_HOST).exists() {
        eprintln!("Relay binary {RELAY_BIN_HOST:?} not found in working directory.");
        eprintln!("Build it first (see README.md).");
        process::exit(1);
    }

    println!("Starting wslc session...");
    if let Err(err) = bootstrap() {
        eprintln!("Bootstrap failed: {err}");
        process::exit(1);
    }

    // From here on the keepalive container is running, so every exit path
    // has to go through cleanup().
    let result = serve();
    cleanup();
    if let Err(err) = result {
        eprintln!("{err}");
        process::exit(1);
    }
}

fn serve() -> Result<(), String> {
    print!("Deploying relay binary to VM... ");
    let _ = io::stdout().flush();
    deploy_relay().map_err(|err| format!("failed: {err}"))?;
    println!("done.");

    let listen_addr =
        env::var("SOCKTAINER_LISTEN").unwrap_or_else(|_| DEFAULT_LISTEN_ADDR.to_string());
    let listener =
        TcpListener::bind(&listen_addr).map_err(|err| format!("Listen failed: {err}"))?;

    println!("\nListening on {listen_addr}");
    println!("Connect with:");
    println!("  curl http://{listen_addr}/v1.44/containers/json");
    println!("  DOCKER_HOST=tcp://{listen_addr} docker ps");
    println!();

    // accept() can't be interrupted from another thread, so the Ctrl-C
    // handler cleans up and exits the process itself.
    ctrlc::set_handler(|| {
        eprintln!(
            "\nShutting down ({} active connections)...",
            ACTIVE_CONNS.load(Ordering::Relaxed)
        );
        cleanup();
        process::exit(0);
    })
    .map_err(|err| format!("Install Ctrl-C handler: {err}"))?;

    for conn in listener.incoming() {
        match conn {
            Ok(conn) => {
                // Start the relay here rather than in the handler thread, so
                // that no wslc process is ever created while another
                // connection is half set up. Windows children inherit every
                // inheritable handle, and a relay holding a stray copy of
                // some other connection's socket or pipe keeps it open after
                // we close ours.
                if let Err(err) = keep_from_children(&conn) {
                    eprintln!("mark connection non-inheritable: {err}");
                    continue;
                }
                match spawn_relay() {
                    Ok(relay) => {
                        thread::spawn(move || handle_conn(conn, relay));
                    }
                    Err(err) => eprintln!("start relay: {err}"),
                }
            }
            Err(err) => return Err(format!("Accept failed: {err}")),
        }
    }
    Ok(())
}

fn bootstrap() -> Result<(), String> {
    // Pull image — this also creates the default session and boots the VM
    // if they don't exist yet.
    println!("  Pulling image...");
    run(&["image", "pull", KEEPALIVE_IMAGE]).map_err(|err| format!("pull image: {err}"))?;

    // Remove any leftover keepalive container from a previous run.
    run_quiet(&["container", "rm", "-f", KEEPALIVE_NAME]);

    // Start a keepalive container so the VM stays alive between requests.
    // Without this, the VM idle-terminates ~30s after the last container stops,
    // and subsequent requests would pay the full VM boot cost.
    println!("  Starting keepalive container...");
    run(&[
        "run",
        "-d",
        "--name",
        KEEPALIVE_NAME,
        KEEPALIVE_IMAGE,
        "sleep",
        "infinity",
    ])
    .map_err(|err| format!("start keepalive: {err}"))?;

    Ok(())
}

fn cleanup() {
    println!("Stopping keepalive container...");
    run_quiet(&["container", "stop", KEEPALIVE_NAME]);
    run_quiet(&["container", "rm", KEEPALIVE_NAME]);
    println!("Done.");
}

fn deploy_relay() -> Result<(), String> {
    let data = fs::read(RELAY_BIN_HOST).map_err(|err| format!("read {RELAY_BIN_HOST}: {err}"))?;

    let script = format!("cat > {RELAY_BIN_GUEST} && chmod +x {RELAY_BIN_GUEST}");
    let mut child = wslc()
        .args(["system", "session", "run", "/bin/sh", "-c", &script])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|err| err.to_string())?;

    // Dropping stdin after the write closes the pipe, which ends `cat`.
    let written = child.stdin.take().expect("stdin is piped").write_all(&data);
    let status = child.wait().map_err(|err| err.to_string())?;
    written.map_err(|err| err.to_string())?;
    if !status.success() {
        return Err(status.to_string());
    }
    Ok(())
}

/// Decrements the active connection count when a handler returns.
struct ConnGuard;

impl ConnGuard {
    fn new() -> Self {
        ACTIVE_CONNS.fetch_add(1, Ordering::Relaxed);
        ConnGuard
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        ACTIVE_CONNS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Accepted sockets are inheritable on Windows; make sure wslc never gets one.
#[cfg(windows)]
fn keep_from_children(conn: &TcpStream) -> io::Result<()> {
    socket2::SockRef::from(conn).set_no_inherit(true)
}

/// Elsewhere the standard library already opens sockets close-on-exec.
#[cfg(not(windows))]
fn keep_from_children(_conn: &TcpStream) -> io::Result<()> {
    Ok(())
}

fn spawn_relay() -> io::Result<Child> {
    wslc()
        .args(["system", "session", "run", RELAY_BIN_GUEST])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
}

fn handle_conn(conn: TcpStream, mut child: Child) {
    let _guard = ConnGuard::new();

    let mut conn_reader = match conn.try_clone() {
        Ok(clone) => clone,
        Err(err) => {
            eprintln!("clone connection: {err}");
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
    };
    let mut conn_writer = conn;

    let mut stdin = child.stdin.take().expect("stdin is piped");
    let mut stdout = child.stdout.take().expect("stdout is piped");

    // client → relay; dropping stdin at the end signals EOF to the relay.
    let upstream = thread::spawn(move || {
        pump(&mut conn_reader, &mut stdin);
    });

    // relay → client
    let downstream = thread::spawn(move || {
        pump(&mut stdout, &mut conn_writer);
    });

    let _ = upstream.join();
    let _ = downstream.join();
    let _ = child.wait();
}

fn wslc() -> Command {
    Command::new(env::var_os("SOCKTAINER_WSLC").unwrap_or_else(|| DEFAULT_WSLC.into()))
}

/// Runs a wslc command with its output sent to our stderr, failing on a non-zero exit.
fn run(args: &[&str]) -> Result<(), String> {
    let status = wslc()
        .args(args)
        .stdout(Stdio::from(io::stderr()))
        .stderr(Stdio::inherit())
        .status()
        .map_err(|err| err.to_string())?;
    if !status.success() {
        return Err(status.to_string());
    }
    Ok(())
}

/// Runs a wslc command, discarding its output and ignoring any failure.
fn run_quiet(args: &[&str]) {
    let _ = wslc()
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Copies `reader` to `writer` until EOF or an error.
///
/// Deliberately not io::copy: on Linux that splices socket data straight into
/// a pipe, and a blocked splice keeps the pipe locked, so the process at the
/// other end can't read what has already arrived until more data shows up.
fn pump(mut reader: impl Read, mut writer: impl Write) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                if writer.write_all(&buf[..n]).is_err() {
                    return;
                }
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}
