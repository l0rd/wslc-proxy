// Relay binary — runs inside the wslc VM's root namespace.
//
// Connects to /var/run/docker.sock and bidirectionally copies
// between the Unix socket and stdin/stdout. Each instance handles
// one Docker API connection.
//
// Build: GOOS=linux GOARCH=amd64 CGO_ENABLED=0 go build -ldflags '-s -w' -o docker-relay ./cmd/relay/

package main

import (
	"fmt"
	"io"
	"net"
	"os"
	"sync"
)

func main() {
	conn, err := net.Dial("unix", "/var/run/docker.sock")
	if err != nil {
		fmt.Fprintf(os.Stderr, "relay: connect to docker.sock: %v\n", err)
		os.Exit(1)
	}
	defer conn.Close()

	var wg sync.WaitGroup
	wg.Add(2)

	// stdin → docker.sock
	go func() {
		defer wg.Done()
		io.Copy(conn, os.Stdin)
		if uc, ok := conn.(*net.UnixConn); ok {
			uc.CloseWrite()
		}
	}()

	// docker.sock → stdout
	go func() {
		defer wg.Done()
		io.Copy(os.Stdout, conn)
	}()

	wg.Wait()
}
