//! Stand-in for `wslc`, used by the proxy tests (tests/proxy.rs) so that
//! socktainer can be exercised without WSL.
//!
//! Every invocation appends its arguments to $FAKE_WSLC_LOG. Then:
//!   - if the arguments contain $FAKE_WSLC_FAIL, it exits with status 1;
//!   - `system session run /bin/sh -c ...` saves stdin to $FAKE_WSLC_DEPLOY
//!     (the relay deployment);
//!   - `system session run <relay>` echoes stdin back to stdout, standing in
//!     for the relay and the Docker engine behind it;
//!   - anything else succeeds silently.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::process;

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let line = args.join(" ");

    if let Some(log) = env::var_os("FAKE_WSLC_LOG") {
        let mut log = OpenOptions::new().create(true).append(true).open(log)?;
        log.write_all(format!("{line}\n").as_bytes())?;
    }

    if let Ok(fail) = env::var("FAKE_WSLC_FAIL") {
        if line.contains(&fail) {
            eprintln!("fake wslc: failing `{line}`");
            process::exit(1);
        }
    }

    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["system", "session", "run", "/bin/sh", "-c", _script] => {
            let mut data = Vec::new();
            io::stdin().read_to_end(&mut data)?;
            if let Some(path) = env::var_os("FAKE_WSLC_DEPLOY") {
                fs::write(path, data)?;
            }
        }
        ["system", "session", "run", _relay] => echo()?,
        _ => {}
    }
    Ok(())
}

/// Echoes stdin to stdout chunk by chunk, flushing each one so that nothing
/// is held back while the connection stays open.
fn echo() -> io::Result<()> {
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    let mut buf = [0u8; 8192];
    loop {
        let n = stdin.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        stdout.write_all(&buf[..n])?;
        stdout.flush()?;
    }
}
