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

1. Build the relay (Linux binary):
   ```sh
   GOOS=linux GOARCH=arm64 CGO_ENABLED=0 go build -ldflags '-s -w' -o docker-relay ./cmd/relay/
   ```

2. Build socktainer (Windows binary):
   ```sh
   go build -o socktainer.exe ./cmd/socktainer/
   ```

3. Run:
   ```sh
   ./socktainer.exe
   ```

4. Connect:
   ```sh
   curl http://localhost:2375/v1.44/containers/json
   DOCKER_HOST=tcp://localhost:2375 docker ps
   ```
