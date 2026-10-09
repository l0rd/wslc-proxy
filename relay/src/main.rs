//! Relay binary — runs inside the wslc VM's root namespace.
//!
//! Connects to /var/run/docker.sock and bidirectionally copies
//! between the Unix socket and stdin/stdout. Each instance handles
//! one Docker API connection.
//!
//! Build: cargo build --release -p docker-relay --target aarch64-unknown-linux-musl

use std::fs::File;
use std::io::{self, ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::process;
use std::thread;

const DOCKER_SOCK: &str = "/var/run/docker.sock";

fn main() {
    let conn = match UnixStream::connect(DOCKER_SOCK) {
        Ok(conn) => conn,
        Err(err) => {
            eprintln!("relay: connect to docker.sock: {err}");
            process::exit(1);
        }
    };

    if let Err(err) = relay(conn) {
        eprintln!("relay: {err}");
        process::exit(1);
    }
}

fn relay(conn: UnixStream) -> io::Result<()> {
    // Use the raw descriptors rather than io::stdin()/io::stdout(): the std
    // handles are buffered (stdout is line-buffered), which would stall
    // streamed Docker responses that don't end in a newline.
    let mut stdin = File::from(io::stdin().as_fd().try_clone_to_owned()?);
    let mut stdout = File::from(io::stdout().as_fd().try_clone_to_owned()?);

    let mut sock_writer = conn.try_clone()?;
    let mut sock_reader = conn;

    // stdin → docker.sock
    let upstream = thread::spawn(move || {
        if let Err(err) = pump(&mut stdin, &mut sock_writer) {
            eprintln!("relay: stdin → docker.sock: {err}");
        }
        let _ = sock_writer.shutdown(Shutdown::Write);
    });

    // docker.sock → stdout
    let downstream = thread::spawn(move || {
        if let Err(err) = pump(&mut sock_reader, &mut stdout) {
            eprintln!("relay: docker.sock → stdout: {err}");
        }
    });

    let _ = upstream.join();
    let _ = downstream.join();
    Ok(())
}

/// Copies `reader` to `writer` until EOF or an error.
///
/// Deliberately not io::copy: on Linux that splices socket data straight into
/// a pipe, and a blocked splice keeps the pipe locked, so the process at the
/// other end can't read what has already arrived until more data shows up.
fn pump(mut reader: impl Read, mut writer: impl Write) -> io::Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => writer.write_all(&buf[..n])?,
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
}
