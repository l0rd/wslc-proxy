//! Helpers shared by the proxy and end-to-end tests.

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

pub const SOCKTAINER: &str = env!("CARGO_BIN_EXE_socktainer");

/// Returns a loopback address with a port that was free a moment ago.
pub fn free_addr() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    listener.local_addr().expect("local address").to_string()
}

/// A running socktainer process, killed when dropped.
pub struct Socktainer {
    child: Child,
}

impl Socktainer {
    /// Spawns `cmd` and waits until socktainer reports that it is listening.
    pub fn start(mut cmd: Command, startup_timeout: Duration) -> Self {
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn socktainer");

        // Keep draining stdout for the lifetime of the process so that it
        // never blocks or fails on a later write.
        let stdout = child.stdout.take().expect("stdout is piped");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });

        let mut socktainer = Socktainer { child };
        let mut output = Vec::new();
        loop {
            match rx.recv_timeout(startup_timeout) {
                Ok(line) if line.starts_with("Listening on") => return socktainer,
                Ok(line) => output.push(line),
                Err(err) => {
                    socktainer.stop();
                    panic!("socktainer did not start listening ({err}); output: {output:#?}");
                }
            }
        }
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Socktainer {
    fn drop(&mut self) {
        self.stop();
    }
}
