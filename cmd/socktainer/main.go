// Socktainer for wslc — exposes the Docker API from a wslc session VM
// as a TCP endpoint on the Windows host.
//
// Architecture:
//
//   Docker client ──TCP:2375──► socktainer ──stdio──► wslc session run ──HvSocket──► relay ──► docker.sock
//
// On startup, socktainer:
//   1. Pulls alpine:latest (also boots the VM if the session doesn't exist yet)
//   2. Starts a keepalive container (prevents VM idle-termination between requests)
//   3. Deploys the relay binary into the VM's root namespace
//   4. Listens on TCP:2375
//
// For each incoming TCP connection, socktainer spawns:
//   wslc system session run /tmp/docker-relay
// which connects to /var/run/docker.sock inside the VM and relays
// bytes bidirectionally over stdin/stdout.
//
// On shutdown (Ctrl-C), the keepalive container is stopped and removed.
//
// Usage:
//   1. Build the relay:
//        GOOS=linux GOARCH=arm64 CGO_ENABLED=0 go build -ldflags '-s -w' -o docker-relay ./cmd/relay/
//   2. Build socktainer:
//        go build -o socktainer.exe ./cmd/socktainer/
//   3. Run:
//        ./socktainer.exe
//   4. In another terminal:
//        curl http://localhost:2375/v1.44/containers/json
//        DOCKER_HOST=tcp://localhost:2375 docker ps

package main

import (
	"bytes"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"os/signal"
	"sync"
	"sync/atomic"
)

const (
	listenAddr        = "localhost:2375"
	relayBinHost      = "docker-relay"
	relayBinGuest     = "/tmp/docker-relay"
	keepaliveImage    = "alpine:latest"
	keepaliveName     = "socktainer-keepalive"
)

var activeConns atomic.Int64

func main() {
	if _, err := os.Stat(relayBinHost); err != nil {
		fmt.Fprintf(os.Stderr, "Relay binary %q not found in working directory.\n", relayBinHost)
		fmt.Fprintf(os.Stderr, "Build it first (see comments at top of this file).\n")
		os.Exit(1)
	}

	fmt.Println("Starting wslc session...")
	if err := bootstrap(); err != nil {
		fmt.Fprintf(os.Stderr, "Bootstrap failed: %v\n", err)
		os.Exit(1)
	}

	// Ensure cleanup runs on exit
	cleanupDone := make(chan struct{})
	defer func() {
		cleanup()
		close(cleanupDone)
	}()

	fmt.Print("Deploying relay binary to VM... ")
	if err := deployRelay(); err != nil {
		fmt.Fprintf(os.Stderr, "failed: %v\n", err)
		os.Exit(1)
	}
	fmt.Println("done.")

	ln, err := net.Listen("tcp", listenAddr)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Listen failed: %v\n", err)
		os.Exit(1)
	}
	defer ln.Close()

	fmt.Printf("\nListening on %s\n", listenAddr)
	fmt.Println("Connect with:")
	fmt.Println("  curl http://localhost:2375/v1.44/containers/json")
	fmt.Println("  DOCKER_HOST=tcp://localhost:2375 docker ps")
	fmt.Println()

	sig := make(chan os.Signal, 1)
	signal.Notify(sig, os.Interrupt)
	go func() {
		<-sig
		fmt.Fprintf(os.Stderr, "\nShutting down (%d active connections)...\n", activeConns.Load())
		ln.Close()
	}()

	for {
		conn, err := ln.Accept()
		if err != nil {
			break
		}
		go handleConn(conn)
	}
}

func bootstrap() error {
	// Pull image — this also creates the default session and boots the VM
	// if they don't exist yet.
	fmt.Println("  Pulling image...")
	if err := run("wslc", "image", "pull", keepaliveImage); err != nil {
		return fmt.Errorf("pull image: %w", err)
	}

	// Remove any leftover keepalive container from a previous run.
	exec.Command("wslc", "container", "rm", "-f", keepaliveName).Run()

	// Start a keepalive container so the VM stays alive between requests.
	// Without this, the VM idle-terminates ~30s after the last container stops,
	// and subsequent requests would pay the full VM boot cost.
	fmt.Println("  Starting keepalive container...")
	if err := run("wslc", "run", "-d", "--name", keepaliveName, keepaliveImage, "sleep", "infinity"); err != nil {
		return fmt.Errorf("start keepalive: %w", err)
	}

	return nil
}

func cleanup() {
	fmt.Println("Stopping keepalive container...")
	exec.Command("wslc", "container", "stop", keepaliveName).Run()
	exec.Command("wslc", "container", "rm", keepaliveName).Run()
	fmt.Println("Done.")
}

func deployRelay() error {
	data, err := os.ReadFile(relayBinHost)
	if err != nil {
		return fmt.Errorf("read %s: %w", relayBinHost, err)
	}

	script := fmt.Sprintf("cat > %s && chmod +x %s", relayBinGuest, relayBinGuest)
	cmd := exec.Command("wslc", "system", "session", "run", "/bin/sh", "-c", script)
	cmd.Stdin = bytes.NewReader(data)
	cmd.Stderr = os.Stderr
	return cmd.Run()
}

func handleConn(conn net.Conn) {
	activeConns.Add(1)
	defer func() {
		activeConns.Add(-1)
		conn.Close()
	}()

	cmd := exec.Command("wslc", "system", "session", "run", relayBinGuest)

	stdinPipe, err := cmd.StdinPipe()
	if err != nil {
		fmt.Fprintf(os.Stderr, "stdin pipe: %v\n", err)
		return
	}

	stdoutPipe, err := cmd.StdoutPipe()
	if err != nil {
		fmt.Fprintf(os.Stderr, "stdout pipe: %v\n", err)
		return
	}

	cmd.Stderr = os.Stderr

	if err := cmd.Start(); err != nil {
		fmt.Fprintf(os.Stderr, "start relay: %v\n", err)
		return
	}

	var wg sync.WaitGroup
	wg.Add(2)

	go func() {
		defer wg.Done()
		io.Copy(stdinPipe, conn)
		stdinPipe.Close()
	}()

	go func() {
		defer wg.Done()
		io.Copy(conn, stdoutPipe)
	}()

	wg.Wait()
	cmd.Wait()
}

func run(name string, args ...string) error {
	cmd := exec.Command(name, args...)
	cmd.Stdout = os.Stderr
	cmd.Stderr = os.Stderr
	return cmd.Run()
}
