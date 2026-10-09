# wslc-proxy

Exposes the Docker API from a wslc session VM as a TCP endpoint on the Windows host.

## Architecture

```mermaid
flowchart TB
    subgraph windows["Windows Host"]
        client["Docker Client\n(curl, docker)"]
        subgraph socktainer["socktainer.exe"]
            listener["TCP Listener\nlocalhost:2375"]
            handler["handleConn\n(per connection)"]
        end
        wslc["wslc system session run\n/tmp/docker-relay"]
    end

    subgraph vm["wslc Session VM (Linux)"]
        relay["docker-relay\n(/tmp/docker-relay)"]
        sock["/var/run/docker.sock"]
        engine["Docker Engine\n(dockerd)"]
        keepalive["socktainer-keepalive\n(alpine sleep infinity)\nPrevents VM idle-termination"]
    end

    client -- "TCP :2375" --> listener
    listener --> handler
    handler -- "stdin/stdout" --> wslc
    wslc -. "stdio over\nHvSocket" .-> relay
    relay -- "Unix socket" --> sock
    sock --> engine
    keepalive -. "keeps VM alive" .-> engine
```

### Bootstrap sequence

```mermaid
sequenceDiagram
    participant S as socktainer.exe
    participant W as wslc
    participant VM as Session VM

    S->>W: wslc image pull alpine:latest
    W->>VM: Boot VM (if needed)
    S->>W: wslc run -d --name socktainer-keepalive alpine sleep infinity
    W->>VM: Start keepalive container
    S->>W: wslc system session run /bin/sh -c "cat > /tmp/docker-relay"
    W->>VM: Deploy relay binary (via stdin pipe)
    Note over S: Listen on TCP :2375
```

### Data flow (per connection)

```mermaid
flowchart LR
    A["Docker Client"] -- "TCP" --> B["socktainer"]
    B -- "stdio" --> C["docker-relay"]
    C -- "Unix socket" --> D["docker.sock"]
    D -- "Unix socket" --> C
    C -- "stdio" --> B
    B -- "TCP" --> A
```

## Usage

1. Build the relay (static Linux binary) and copy it to the working directory.
   Use `x86_64-unknown-linux-musl` instead on an x64 host:
   ```sh
   rustup target add aarch64-unknown-linux-musl
   cargo build --release -p docker-relay --target aarch64-unknown-linux-musl
   cp target/aarch64-unknown-linux-musl/release/docker-relay .
   ```

2. Build socktainer (Windows binary):
   ```sh
   cargo build --release
   ```

3. Run (from the directory containing `docker-relay`):
   ```sh
   ./target/release/socktainer.exe
   ```

4. Connect:
   ```sh
   curl http://localhost:2375/v1.44/containers/json
   DOCKER_HOST=tcp://localhost:2375 docker ps
   ```

## Testing

Tests use the built-in harness with [rstest](https://crates.io/crates/rstest) fixtures.

- **Proxy tests** run socktainer against a fake `wslc` (`socktainer/examples/fake_wslc.rs`), so they work on any OS without WSL:
  ```sh
  cargo test
  ```

- **End-to-end tests** run socktainer against the real `wslc` and the Docker engine in the session VM. They need `docker-relay` in the repository root (see step 1 above):
  ```sh
  cargo test --test e2e -- --ignored
  ```

Two environment variables override socktainer's defaults, mainly for the tests: `SOCKTAINER_LISTEN` (listen address, default `localhost:2375`) and `SOCKTAINER_WSLC` (the `wslc` executable to run).
