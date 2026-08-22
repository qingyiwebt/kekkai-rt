package main

import (
	"bytes"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"path/filepath"
)

const (
	socketPath = "/run/agentcell-tools.socket"

	openFrame    byte = 1
	stdinFrame   byte = 2
	stdinEOF     byte = 3
	stdoutFrame  byte = 0x10
	stderrFrame  byte = 0x11
	exitFrame    byte = 0x12
	errorFrame   byte = 0x13
	maxFrameSize      = 1024 * 1024
	maxFieldSize      = 1024 * 1024
	maxArguments      = 1024
	localFailure      = 125
)

func main() {
	os.Exit(run())
}

func run() int {
	command := filepath.Base(os.Args[0])
	if command == "" || command == "." || command == string(filepath.Separator) {
		return fail("unable to determine command name")
	}

	conn, err := net.Dial("unix", socketPath)
	if err != nil {
		return fail("connect to AgentCell: %v", err)
	}
	defer conn.Close()

	if err := sendOpen(conn, command, os.Args[1:]); err != nil {
		return fail("send request: %v", err)
	}

	go forwardInput(conn)

	code, err := receiveOutput(conn)
	if err != nil {
		_ = conn.Close()
		return fail("receive response: %v", err)
	}
	_ = conn.Close()
	return code
}

func fail(format string, args ...any) int {
	_, _ = fmt.Fprintf(os.Stderr, "agentcell-tool-proxy: "+format+"\n", args...)
	return localFailure
}

func sendOpen(w io.Writer, command string, args []string) error {
	if len(args) > maxArguments {
		return fmt.Errorf("argument count exceeds %d", maxArguments)
	}
	var payload bytes.Buffer
	if err := writeField(&payload, []byte(command)); err != nil {
		return err
	}
	if err := binary.Write(&payload, binary.BigEndian, uint32(len(args))); err != nil {
		return err
	}
	for _, arg := range args {
		if err := writeField(&payload, []byte(arg)); err != nil {
			return err
		}
	}
	return writeFrame(w, openFrame, payload.Bytes())
}

func forwardInput(conn net.Conn) {
	buffer := make([]byte, 32*1024)
	for {
		length, readErr := os.Stdin.Read(buffer)
		if length > 0 {
			if err := writeFrame(conn, stdinFrame, buffer[:length]); err != nil {
				_ = conn.Close()
				return
			}
		}
		if errors.Is(readErr, io.EOF) {
			_ = writeFrame(conn, stdinEOF, nil)
			return
		}
		if readErr != nil {
			_ = conn.Close()
			return
		}
	}
}

func receiveOutput(r io.Reader) (int, error) {
	for {
		kind, payload, err := readFrame(r)
		if err != nil {
			return 0, err
		}
		switch kind {
		case stdoutFrame:
			if err := writeAll(os.Stdout, payload); err != nil {
				return 0, err
			}
		case stderrFrame:
			if err := writeAll(os.Stderr, payload); err != nil {
				return 0, err
			}
		case errorFrame:
			_, _ = fmt.Fprintf(os.Stderr, "agentcell-tool-proxy: %s\n", payload)
		case exitFrame:
			if len(payload) != 4 {
				return 0, fmt.Errorf("invalid EXIT payload length %d", len(payload))
			}
			return int(int32(binary.BigEndian.Uint32(payload))), nil
		default:
			return 0, fmt.Errorf("unexpected response frame 0x%02x", kind)
		}
	}
}

func writeFrame(w io.Writer, kind byte, payload []byte) error {
	if len(payload) > maxFrameSize {
		return fmt.Errorf("frame length exceeds %d", maxFrameSize)
	}
	header := [5]byte{kind}
	binary.BigEndian.PutUint32(header[1:], uint32(len(payload)))
	if err := writeAll(w, header[:]); err != nil {
		return err
	}
	return writeAll(w, payload)
}

func readFrame(r io.Reader) (byte, []byte, error) {
	var header [5]byte
	if _, err := io.ReadFull(r, header[:]); err != nil {
		return 0, nil, err
	}
	length := binary.BigEndian.Uint32(header[1:])
	if length > maxFrameSize {
		return 0, nil, fmt.Errorf("frame length exceeds %d", maxFrameSize)
	}
	payload := make([]byte, length)
	if _, err := io.ReadFull(r, payload); err != nil {
		return 0, nil, err
	}
	return header[0], payload, nil
}

func writeField(w io.Writer, value []byte) error {
	if len(value) > maxFieldSize {
		return fmt.Errorf("field length exceeds %d", maxFieldSize)
	}
	var length [4]byte
	binary.BigEndian.PutUint32(length[:], uint32(len(value)))
	if err := writeAll(w, length[:]); err != nil {
		return err
	}
	return writeAll(w, value)
}

func writeAll(w io.Writer, bytes []byte) error {
	for len(bytes) > 0 {
		written, err := w.Write(bytes)
		if err != nil {
			return err
		}
		if written == 0 {
			return io.ErrShortWrite
		}
		bytes = bytes[written:]
	}
	return nil
}
