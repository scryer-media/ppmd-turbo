// Package fixtures reads the bench corpus that `ppmd-corpus bench` writes
// (bench/fixtures/<profile>/manifest.json), generates it by running that
// tool, and finds the optional RAR performance corpus.
//
// The corpus is deterministic: every payload comes from ppmd-corpus's seeded
// generator, every .7z from the 7zz the operator names, so two hosts given
// the same 7zz release hold byte-identical inputs.
package fixtures

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
)

// Schema is the manifest schema ppmd-corpus writes.
const Schema = "ppmd-turbo-corpus/bench/1"

// ManifestName is the manifest's file name inside the corpus directory.
const ManifestName = "manifest.json"

// Archive is one .7z holding one PPMd-compressed payload. Order and Mem are
// what the container's coder properties say: 7zz shrinks a requested memory
// size that is far larger than the input, so RequestedMem may differ.
type Archive struct {
	Name         string `json:"name"`
	File         string `json:"file"`
	Payload      string `json:"payload"`
	Kind         string `json:"kind"`
	Order        int    `json:"order"`
	Mem          int64  `json:"mem"`
	RequestedMem int64  `json:"requested_mem"`
	Offset       int64  `json:"offset"`
	PackedLen    int64  `json:"packed_len"`
	UnpackedLen  int64  `json:"unpacked_len"`
	ContainerLen int64  `json:"container_len"`
	PayloadCRC32 string `json:"payload_crc32"`
}

// Raw is a raw 7z-coder stream written by ppmd-rust, for parameters 7zz
// cannot be made to write (it shrinks the memory size to the input).
type Raw struct {
	Name         string `json:"name"`
	File         string `json:"file"`
	Payload      string `json:"payload"`
	Kind         string `json:"kind"`
	Order        int    `json:"order"`
	Mem          int64  `json:"mem"`
	PackedLen    int64  `json:"packed_len"`
	UnpackedLen  int64  `json:"unpacked_len"`
	PayloadCRC32 string `json:"payload_crc32"`
}

// Source is an uncompressed payload the encode rows compress.
type Source struct {
	Name  string `json:"name"`
	File  string `json:"file"`
	Kind  string `json:"kind"`
	Size  int64  `json:"size"`
	CRC32 string `json:"crc32"`
}

// SevenZip is the 7zz that wrote the archives.
type SevenZip struct {
	Banner  string `json:"banner"`
	Version string `json:"version"`
	Path    string `json:"path"`
}

// Manifest is manifest.json.
type Manifest struct {
	Schema    string    `json:"schema"`
	Profile   string    `json:"profile"`
	Generator string    `json:"generator"`
	SevenZip  SevenZip  `json:"sevenzip"`
	Archives  []Archive `json:"archives"`
	Raw       []Raw     `json:"raw"`
	Sources   []Source  `json:"sources"`
	// RAR is the RAR performance corpus found next to the run, filled by
	// the harness (not by ppmd-corpus).
	RAR []RARFile `json:"rar,omitempty"`
}

// Load reads dir/manifest.json and checks every file it names exists.
func Load(dir string) (*Manifest, error) {
	data, err := os.ReadFile(filepath.Join(dir, ManifestName))
	if err != nil {
		return nil, err
	}
	var manifest Manifest
	if err := json.Unmarshal(data, &manifest); err != nil {
		return nil, fmt.Errorf("%s: %w", ManifestName, err)
	}
	if manifest.Schema != Schema {
		return nil, fmt.Errorf("%s: schema %q, want %q", ManifestName, manifest.Schema, Schema)
	}
	var files []string
	for _, a := range manifest.Archives {
		files = append(files, a.File)
	}
	for _, r := range manifest.Raw {
		files = append(files, r.File)
	}
	for _, s := range manifest.Sources {
		files = append(files, s.File)
	}
	for _, file := range files {
		if _, err := os.Stat(filepath.Join(dir, file)); err != nil {
			return nil, fmt.Errorf("%s names %s: %w", ManifestName, file, err)
		}
	}
	return &manifest, nil
}

// Options drive Generate.
type Options struct {
	// Tool is the ppmd-corpus binary.
	Tool     string
	Dir      string
	Profile  string
	SevenZip string
	Only     []string
	Log      io.Writer
}

// Generate runs `ppmd-corpus bench` and loads what it wrote.
func Generate(ctx context.Context, o Options) (*Manifest, error) {
	args := []string{"bench", "--profile", o.Profile, "--dir", o.Dir}
	if o.SevenZip != "" {
		args = append(args, "--sevenzip", o.SevenZip)
	}
	if len(o.Only) > 0 {
		args = append(args, "--only", strings.Join(o.Only, ","))
	}
	cmd := exec.CommandContext(ctx, o.Tool, args...)
	cmd.Stdout, cmd.Stderr = o.Log, o.Log
	if err := cmd.Run(); err != nil {
		return nil, fmt.Errorf("%s %s: %w", o.Tool, strings.Join(args, " "), err)
	}
	return Load(o.Dir)
}

// RARFile is one archive of the RAR performance corpus. RARLAB's rar is the
// only RAR writer, so the corpus is not generated here: it is rarpar's
// RARLAB-written PPMd set (see docs/benchmarking.md).
type RARFile struct {
	Name string `json:"name"`
	// Path is the archive on this host.
	Path string `json:"path"`
	// Unpacked is the member's size, which the reference tools do not
	// report: ppmd-bench checks it against the header.
	Unpacked int64  `json:"unpacked"`
	Bytes    int64  `json:"bytes"`
	Note     string `json:"note"`
	// Large marks archives only the full and fleet profiles decode.
	Large bool `json:"large"`
}

// RARCorpus lists the RAR archives the harness knows, in plan order.
var RARCorpus = []RARFile{
	{Name: "rar4_ppm_solid_restart.rar", Unpacked: 1_600_000,
		Note: "RAR 4 PPMd, order 16 over a 1 MiB model: the sub-allocator restarts several times"},
	{Name: "rar4_ppm_order16_32m.rar", Unpacked: 32 << 20, Large: true,
		Note: "RAR 4 PPMd, order 16, 16 MiB model, 32 MiB of base64 text (rar -m5 -mc16:16t+)"},
}

// ErrNoRARCorpus is returned when no RAR corpus directory was given.
var ErrNoRARCorpus = errors.New("no RAR corpus directory")

// FindRAR returns the archives of RARCorpus present in dir, with their
// sizes. A missing directory is ErrNoRARCorpus; missing files are skipped.
func FindRAR(dir string, large bool) ([]RARFile, error) {
	if dir == "" {
		return nil, ErrNoRARCorpus
	}
	if info, err := os.Stat(dir); err != nil || !info.IsDir() {
		return nil, fmt.Errorf("RAR corpus %s: not a directory", dir)
	}
	var found []RARFile
	for _, file := range RARCorpus {
		if file.Large && !large {
			continue
		}
		path := filepath.Join(dir, file.Name)
		info, err := os.Stat(path)
		if err != nil {
			continue
		}
		file.Path, file.Bytes = path, info.Size()
		found = append(found, file)
	}
	return found, nil
}
