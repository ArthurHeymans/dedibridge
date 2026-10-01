// Copyright (c) 2026 Arthur Heymans
// SPDX-License-Identifier: MIT OR Apache-2.0

// DediBridge adapter for dutctl's serial module. Copy this file into
// pkg/module/serial alongside the configuration patch in this directory.
package serial

import (
	"bufio"
	"bytes"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"sync"
	"time"
)

const bridgeMaxFrame = 16384
const bridgeReadTimeout = 100 * time.Millisecond
const bridgeCommandTimeout = 5 * time.Second

type bridgeMessage struct {
	Type    string `json:"type"`
	Data    string `json:"data"`
	Message string `json:"message"`
	Version int    `json:"version"`
}

type bridgePort struct {
	conn     net.Conn
	reader   *bufio.Reader
	commands sync.Mutex
	mu       sync.Mutex
	buf      bytes.Buffer
	err      error
	dropping bool
	pending  string
	wake     chan struct{}
	replies  chan bridgeMessage
	done     chan struct{}
	once     sync.Once
}

func bridgeReceive(reader *bufio.Reader) (bridgeMessage, error) {
	line, err := reader.ReadSlice('\n')
	if err != nil {
		return bridgeMessage{}, err
	}
	if len(line) > bridgeMaxFrame {
		return bridgeMessage{}, errors.New("DediBridge frame too large")
	}
	var message bridgeMessage
	if err := json.Unmarshal(line, &message); err != nil {
		return message, fmt.Errorf("DediBridge frame: %w", err)
	}
	if message.Type == "error" {
		return message, fmt.Errorf("DediBridge: %s", message.Message)
	}
	return message, nil
}

func openDediPort(socket string, baud int) (*bridgePort, error) {
	conn, err := net.DialTimeout("unix", socket, bridgeCommandTimeout)
	if err != nil {
		return nil, fmt.Errorf("DediBridge socket %s: %w", socket, err)
	}
	_ = conn.SetDeadline(time.Now().Add(bridgeCommandTimeout))
	reader := bufio.NewReaderSize(conn, bridgeMaxFrame+1)
	err = json.NewEncoder(conn).Encode(struct {
		Op   string `json:"op"`
		Baud int    `json:"baud"`
	}{"serial", baud})
	if err != nil {
		_ = conn.Close()
		return nil, err
	}
	ready, err := bridgeReceive(reader)
	if err == nil && (ready.Type != "ready" || ready.Version != 1) {
		err = errors.New("unsupported DediBridge daemon handshake")
	}
	if err != nil {
		_ = conn.Close()
		return nil, err
	}
	_ = conn.SetDeadline(time.Time{})
	p := &bridgePort{conn: conn, reader: reader, wake: make(chan struct{}, 1), replies: make(chan bridgeMessage, 1), done: make(chan struct{})}
	go p.receive()
	return p, nil
}

func (p *bridgePort) fail(err error) {
	p.once.Do(func() {
		p.mu.Lock()
		p.err = err
		p.buf.Reset()
		p.mu.Unlock()
		close(p.done)
		_ = p.conn.Close()
	})
}
func (p *bridgePort) receive() {
	for {
		message, err := bridgeReceive(p.reader)
		if err != nil {
			p.fail(err)
			return
		}
		if message.Type == "data" {
			data, err := base64.StdEncoding.DecodeString(message.Data)
			if err != nil {
				p.fail(err)
				return
			}
			p.mu.Lock()
			overflow := !p.dropping && p.buf.Len()+len(data) > 65536
			if !p.dropping && !overflow {
				_, _ = p.buf.Write(data)
			}
			p.mu.Unlock()
			if overflow {
				p.fail(errors.New("DediBridge receive buffer overflow"))
				return
			}
			select {
			case p.wake <- struct{}{}:
			default:
			}
			continue
		}
		p.mu.Lock()
		expected := p.pending
		valid := expected != "" && message.Type == expected
		if valid {
			p.pending = ""
			if message.Type == "reset_input" {
				p.dropping = false
			}
		}
		p.mu.Unlock()
		if !valid {
			p.fail(fmt.Errorf("unexpected DediBridge response %q", message.Type))
			return
		}
		select {
		case p.replies <- message:
		case <-p.done:
			return
		}
	}
}

func (p *bridgePort) command(value any, expected string) error {
	p.mu.Lock()
	if p.err != nil {
		err := p.err
		p.mu.Unlock()
		return err
	}
	p.pending = expected
	p.mu.Unlock()
	_ = p.conn.SetWriteDeadline(time.Now().Add(bridgeCommandTimeout))
	if err := json.NewEncoder(p.conn).Encode(value); err != nil {
		p.fail(err)
		return err
	}
	timer := time.NewTimer(bridgeCommandTimeout)
	defer timer.Stop()
	select {
	case <-p.replies:
		p.mu.Lock()
		err := p.err
		p.mu.Unlock()
		return err
	case <-p.done:
		p.mu.Lock()
		err := p.err
		p.mu.Unlock()
		return err
	case <-timer.C:
		err := errors.New("DediBridge command timed out")
		p.fail(err)
		return err
	}
}

func (p *bridgePort) Write(data []byte) (int, error) {
	p.commands.Lock()
	defer p.commands.Unlock()
	written := 0
	for len(data) > 0 {
		n := min(len(data), 4096)
		err := p.command(struct {
			Op   string `json:"op"`
			Data string `json:"data"`
		}{"write", base64.StdEncoding.EncodeToString(data[:n])}, "written")
		if err != nil {
			return written, err
		}
		data = data[n:]
		written += n
	}
	return written, nil
}
func (p *bridgePort) ResetInputBuffer() error {
	p.commands.Lock()
	defer p.commands.Unlock()
	p.mu.Lock()
	p.buf.Reset()
	p.dropping = true
	p.mu.Unlock()
	return p.command(struct {
		Op string `json:"op"`
	}{"reset_input"}, "reset_input")
}
func (p *bridgePort) Read(data []byte) (int, error) {
	if len(data) == 0 {
		return 0, nil
	}
	timer := time.NewTimer(bridgeReadTimeout)
	defer timer.Stop()
	for {
		p.mu.Lock()
		if p.err != nil {
			err := p.err
			p.mu.Unlock()
			return 0, err
		}
		if p.buf.Len() > 0 {
			n, _ := p.buf.Read(data)
			p.mu.Unlock()
			return n, nil
		}
		p.mu.Unlock()
		select {
		case <-p.wake:
		case <-p.done:
		case <-timer.C:
			return 0, nil
		}
	}
}
func (p *bridgePort) Close() error { p.fail(io.ErrClosedPipe); return nil }
