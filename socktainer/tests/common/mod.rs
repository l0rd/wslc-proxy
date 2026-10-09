//! Helpers shared by the proxy and end-to-end tests.

use std::io::{BufRead, BufReader, Read};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub const SOCKTAINER: &str = env!("CARGO_BIN_EXE_socktainer");

/// Returns a loopback address with a port that was free a moment ago.
pub fn free_addr() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    listener.local_addr().expect("local address").to_string()
}

type Output = Arc<Mutex<Vec<String>>>;

/// A running socktainer process, killed when dropped. Its output is printed
/// if the test using it fails.
pub struct Socktainer {
    child: Child,
    output: Output,
}

impl Socktainer {
    /// Spawns `cmd` and waits until socktainer reports that it is listening.
    pub fn start(mut cmd: Command, startup_timeout: Duration) -> Self {
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn socktainer");

        // Keep draining both streams for the lifetime of the process so that
        // it never blocks or fails on a later write.
        let output = Output::default();
        let (listening_tx, listening_rx) = mpsc::channel();
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        collect(stdout, "stdout", output.clone(), Some(listening_tx));
        collect(stderr, "stderr", output.clone(), None);

        let socktainer = Socktainer { child, output };
        if let Err(err) = listening_rx.recv_timeout(startup_timeout) {
            // Dropping `socktainer` while panicking prints its output.
            panic!("socktainer did not start listening: {err}");
        }
        socktainer
    }
}

/// Records each line of `stream`, signalling `listening` once socktainer
/// announces that it accepts connections.
fn collect(
    stream: impl Read + Send + 'static,
    name: &'static str,
    output: Output,
    listening: Option<Sender<()>>,
) {
    thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            if line.starts_with("Listening on") {
                if let Some(listening) = &listening {
                    let _ = listening.send(());
                }
            }
            output.lock().unwrap().push(format!("[{name}] {line}"));
        }
    });
}

impl Drop for Socktainer {
    fn drop(&mut self) {
        let exited = self.child.try_wait().ok().flatten();
        let _ = self.child.kill();
        let _ = self.child.wait();

        if thread::panicking() {
            if let Some(status) = exited {
                eprintln!("socktainer had already exited: {status}");
            }
            // Give the collector threads a moment to reach end of stream.
            thread::sleep(Duration::from_millis(200));
            eprintln!(
                "--- socktainer output ---\n{}",
                self.output.lock().unwrap().join("\n")
            );
        }
    }
}
