package main

import (
	"bytes"
	"encoding/binary"
	"io"
	"path/filepath"
	"testing"
)

func TestFrameRoundTrip(t *testing.T) {
	var encoded bytes.Buffer
	want := []byte{0, 1, 2, 255}
	if err := writeFrame(&encoded, stdoutFrame, want); err != nil {
		t.Fatal(err)
	}
	kind, got, err := readFrame(&encoded)
	if err != nil {
		t.Fatal(err)
	}
	if kind != stdoutFrame || !bytes.Equal(got, want) {
		t.Fatalf("got frame %#x %v, want %#x %v", kind, got, stdoutFrame, want)
	}
}

func TestReadFrameRejectsOversizedPayload(t *testing.T) {
	var encoded [5]byte
	encoded[0] = stdinFrame
	binary.BigEndian.PutUint32(encoded[1:], maxFrameSize+1)
	if _, _, err := readFrame(bytes.NewReader(encoded[:])); err == nil {
		t.Fatal("expected oversized frame to fail")
	}
}

func TestSendOpen(t *testing.T) {
	var encoded bytes.Buffer
	if err := sendOpen(&encoded, "something-cli", []string{"a", "b"}); err != nil {
		t.Fatal(err)
	}
	kind, payload, err := readFrame(&encoded)
	if err != nil {
		t.Fatal(err)
	}
	if kind != openFrame {
		t.Fatalf("got frame kind %#x", kind)
	}
	if !bytes.Equal(payload, []byte{
		0, 0, 0, 13, 's', 'o', 'm', 'e', 't', 'h', 'i', 'n', 'g', '-', 'c', 'l', 'i',
		0, 0, 0, 2,
		0, 0, 0, 1, 'a',
		0, 0, 0, 1, 'b',
	}) {
		t.Fatalf("unexpected OPEN payload: %v", payload)
	}
}

func TestWriteFieldRejectsOversizedValue(t *testing.T) {
	if err := writeField(io.Discard, make([]byte, maxFieldSize+1)); err == nil {
		t.Fatal("expected oversized field to fail")
	}
}

func TestCommandNameUsesBasename(t *testing.T) {
	name := filepath.Base("/usr/local/bin/something-cli")
	if name != "something-cli" {
		t.Fatalf("got %q", name)
	}
}
