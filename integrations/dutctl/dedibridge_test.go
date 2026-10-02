// Copyright (c) 2026 Arthur Heymans
// SPDX-License-Identifier: MIT OR Apache-2.0

package serial

import (
	"bufio"
	"encoding/base64"
	"encoding/json"
	"io"
	"net"
	"path/filepath"
	"testing"
	"time"
)

type bridgeRequest struct {
	Op   string `json:"op"`
	Baud int    `json:"baud"`
	Data string `json:"data"`
}

func testBridge(t *testing.T, run func(net.Conn, *json.Decoder, *json.Encoder)) string {
	t.Helper()
	return testBridgeWithVersion(t, 1, run)
}

func testBridgeWithVersion(t *testing.T, version int, run func(net.Conn, *json.Decoder, *json.Encoder)) string {
	t.Helper()
	socket := filepath.Join(t.TempDir(), "d.sock")
	listener, err := net.Listen("unix", socket)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = listener.Close() })
	go func() {
		conn, err := listener.Accept()
		if err != nil {
			return
		}
		defer conn.Close()
		decoder := json.NewDecoder(bufio.NewReader(conn))
		encoder := json.NewEncoder(conn)
		var open bridgeRequest
		if decoder.Decode(&open) != nil {
			return
		}
		if open.Op != "serial" || open.Baud != 115200 {
			return
		}
		_ = encoder.Encode(bridgeMessage{Type: "ready", Version: version})
		run(conn, decoder, encoder)
	}()
	return socket
}
func uart(encoder *json.Encoder, data []byte) {
	_ = encoder.Encode(bridgeMessage{Type: "data", Data: base64.StdEncoding.EncodeToString(data)})
}

func TestDediBridgeSerialAndFlushBarrier(t *testing.T) {
	socket := testBridge(t, func(conn net.Conn, decoder *json.Decoder, encoder *json.Encoder) {
		uart(encoder, []byte("stale"))
		for {
			var request bridgeRequest
			if decoder.Decode(&request) != nil {
				return
			}
			switch request.Op {
			case "reset_input":
				uart(encoder, []byte("also stale"))
				_ = encoder.Encode(bridgeMessage{Type: "reset_input"})
				uart(encoder, []byte("fresh"))
			case "write":
				data, _ := base64.StdEncoding.DecodeString(request.Data)
				uart(encoder, data)
				_ = encoder.Encode(bridgeMessage{Type: "written"})
			}
		}
	})
	p, err := openDediPort(socket, 115200)
	if err != nil {
		t.Fatal(err)
	}
	defer p.Close()
	if err := p.ResetInputBuffer(); err != nil {
		t.Fatal(err)
	}
	bytes := make([]byte, 5)
	if _, err := io.ReadFull(p, bytes); err != nil || string(bytes) != "fresh" {
		t.Fatalf("flush: %q %v", bytes, err)
	}
	data := []byte{0, 255, 3, '\n'}
	if n, err := p.Write(data); n != len(data) || err != nil {
		t.Fatalf("write %d %v", n, err)
	}
	got := make([]byte, len(data))
	if _, err := io.ReadFull(p, got); err != nil {
		t.Fatal(err)
	}
	for i := range got {
		if got[i] != data[i] {
			t.Fatalf("binary data: %v", got)
		}
	}
	start := time.Now()
	n, err := p.Read(bytes)
	if n != 0 || err != nil || time.Since(start) > time.Second {
		t.Fatalf("read timeout %d %v", n, err)
	}
}

func TestDediBridgeDisconnectMidWriteDoesNotReplay(t *testing.T) {
	chunks := make(chan []int, 1)
	socket := testBridge(t, func(_ net.Conn, decoder *json.Decoder, encoder *json.Encoder) {
		var lengths []int
		for i := 0; i < 2; i++ {
			var request bridgeRequest
			if decoder.Decode(&request) != nil || request.Op != "write" {
				chunks <- lengths
				return
			}
			data, _ := base64.StdEncoding.DecodeString(request.Data)
			lengths = append(lengths, len(data))
			if i == 0 {
				_ = encoder.Encode(bridgeMessage{Type: "written"})
			}
		}
		// The second chunk may have executed, but its reply was lost.
		chunks <- lengths
	})
	p, err := openDediPort(socket, 115200)
	if err != nil {
		t.Fatal(err)
	}
	defer p.Close()
	written, err := p.Write(make([]byte, 5000))
	if err == nil || written != 4096 {
		t.Fatalf("ambiguous partial write: %d %v", written, err)
	}
	if n, err := p.Write([]byte("no replay")); err == nil || n != 0 {
		t.Fatalf("failed session reused: %d %v", n, err)
	}
	select {
	case lengths := <-chunks:
		if len(lengths) != 2 || lengths[0] != 4096 || lengths[1] != 904 {
			t.Fatalf("unexpected chunks: %v", lengths)
		}
	case <-time.After(time.Second):
		t.Fatal("server did not receive the original chunks")
	}
}

func TestDediBridgeCloseUnblocksPendingCommand(t *testing.T) {
	waiting := make(chan struct{})
	socket := testBridge(t, func(conn net.Conn, decoder *json.Decoder, encoder *json.Encoder) {
		var request bridgeRequest
		if decoder.Decode(&request) == nil {
			close(waiting)
		}
		_, _ = io.Copy(io.Discard, conn)
	})
	p, err := openDediPort(socket, 115200)
	if err != nil {
		t.Fatal(err)
	}
	result := make(chan error, 1)
	go func() { _, err := p.Write([]byte("hello")); result <- err }()
	select {
	case <-waiting:
	case <-time.After(time.Second):
		t.Fatal("write did not arrive")
	}
	_ = p.Close()
	select {
	case err := <-result:
		if err == nil {
			t.Fatal("closed write succeeded")
		}
	case <-time.After(time.Second):
		t.Fatal("close left write blocked")
	}
}

func TestDediBridgeRejectsIncompatibleReadyVersion(t *testing.T) {
	socket := testBridgeWithVersion(t, 2, func(net.Conn, *json.Decoder, *json.Encoder) {})
	p, err := openDediPort(socket, 115200)
	if err == nil {
		_ = p.Close()
		t.Fatal("incompatible ready version accepted")
	}
}

func TestDediBridgeOverflowAndOversizedFramesAreErrors(t *testing.T) {
	socket := testBridge(t, func(conn net.Conn, decoder *json.Decoder, encoder *json.Encoder) {
		_ = encoder.Encode(bridgeMessage{Type: "error", Message: "UART overflow"})
	})
	p, err := openDediPort(socket, 115200)
	if err != nil {
		t.Fatal(err)
	}
	defer p.Close()
	if _, err := p.Read(make([]byte, 1)); err == nil {
		t.Fatal("overflow was hidden")
	}
	reader := bufio.NewReaderSize(io.LimitReader(&zeroReader{}, bridgeMaxFrame+10), bridgeMaxFrame+1)
	if _, err := bridgeReceive(reader); err == nil {
		t.Fatal("oversized frame accepted")
	}
}

type zeroReader struct{}

func (*zeroReader) Read(p []byte) (int, error) {
	for i := range p {
		p[i] = 'x'
	}
	return len(p), nil
}
