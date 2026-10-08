// Package toolchain records what a run measured with: the ppmd-bench driver
// and which ppmd-turbo operations it provides, the 7zz and unrar references
// with their provenance, and the Rust toolchain and commit when the run is
// next to a checkout.
package toolchain

import (
	"bufio"
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
)

// Binary is one executable the harness ran, identified by its digest.
type Binary struct {
	Path         string `json:"path"`
	ResolvedPath string `json:"resolved_path"`
	SHA256       string `json:"sha256"`
}

// Driver is the ppmd-bench binary and what its `info` reports.
type Driver struct {
	Binary
	// Info is the `info` object: tool version and, per implementation, which
	// operations exist.
	Info map[string]any `json:"info"`
}

// Supports reports whether implementation impl provides op ("decode-7z",
// "decode-rar", "encode-7z").
func (d Driver) Supports(impl, op string) bool {
	impls, _ := d.Info["impls"].(map[string]any)
	ops, _ := impls[impl].(map[string]any)
	supported, _ := ops[op].(bool)
	return supported
}

// Reference is a third-party tool the contenders are measured against.
type Reference struct {
	Binary
	Banner string `json:"banner"`
	// Version is the release in the banner ("26.01", "7.11").
	Version string `json:"version"`
	// Provenance says where the binary came from: the operator's
	// PPMD_BENCH_<TOOL>_PROVENANCE, else what the path suggests.
	Provenance string `json:"provenance"`
	// DecodesRAR reports a 7zz built with the RAR codecs. Official 7-Zip
	// releases carry them; some distribution builds (Homebrew's among them)
	// leave them out.
	DecodesRAR bool `json:"decodes_rar,omitempty"`
}

// Rust is the build environment, when the run is next to a checkout.
type Rust struct {
	Rustc       string `json:"rustc"`
	RustcHost   string `json:"rustc_host,omitempty"`
	Cargo       string `json:"cargo"`
	Commit      string `json:"commit"`
	Dirty       bool   `json:"dirty"`
	CargoLock   string `json:"cargo_lock_sha256,omitempty"`
	LockedTurbo string `json:"locked_ppmd_turbo,omitempty"`
	LockedRust  string `json:"locked_ppmd_rust,omitempty"`
}

// Toolchain is the whole record.
type Toolchain struct {
	Driver   Driver     `json:"driver"`
	SevenZip Reference  `json:"sevenzip"`
	Unrar    *Reference `json:"unrar,omitempty"`
	Rust     Rust       `json:"rust"`
}

// ErrNoSevenZip is returned when no 7-Zip is found.
var ErrNoSevenZip = errors.New("no 7-Zip found: pass --sevenzip or set PPMD_BENCH_SEVENZIP (the official 7zz from https://www.7-zip.org/download.html)")

// Find resolves a tool: the explicit path, then the environment variable,
// then each name on PATH. It returns "" with no error when nothing is found
// and the tool is optional.
func Find(explicit, envName string, names ...string) (string, error) {
	for _, path := range []string{explicit, os.Getenv(envName)} {
		if path != "" {
			if _, err := os.Stat(path); err != nil {
				return "", fmt.Errorf("%s: %w", path, err)
			}
			return path, nil
		}
	}
	for _, name := range names {
		if path, err := exec.LookPath(name); err == nil {
			return path, nil
		}
	}
	return "", nil
}

// ParseBanner returns the first line of a tool's startup text that contains
// marker, and the first field after it that starts with a digit.
func ParseBanner(text, marker string) (banner, version string) {
	scanner := bufio.NewScanner(strings.NewReader(text))
	for scanner.Scan() {
		line := strings.TrimSpace(scanner.Text())
		if !strings.Contains(strings.ToUpper(line), strings.ToUpper(marker)) {
			continue
		}
		for _, field := range strings.Fields(line) {
			if strings.EqualFold(field, marker) {
				continue
			}
			if field[0] >= '0' && field[0] <= '9' {
				return line, field
			}
		}
		return line, ""
	}
	return "", ""
}

// ProbeSevenZip identifies 7zz. p7zip, an unofficial fork frozen at 16.02, is
// refused unless allowP7zip.
func ProbeSevenZip(ctx context.Context, path string, allowP7zip bool) (Reference, error) {
	binary, err := Identify(path)
	if err != nil {
		return Reference{}, err
	}
	output, _ := exec.CommandContext(ctx, path).CombinedOutput()
	banner, version := ParseBanner(string(output), "7-Zip")
	if banner == "" {
		return Reference{}, fmt.Errorf("%s: no 7-Zip banner in its output", path)
	}
	if strings.Contains(strings.ToLower(banner), "p7zip") && !allowP7zip {
		return Reference{}, fmt.Errorf("%s is p7zip (%s), not the official 7-Zip; pass --allow-p7zip to use it anyway", path, banner)
	}
	codecs, _ := exec.CommandContext(ctx, path, "i").Output()
	return Reference{Binary: binary, Banner: banner, Version: version, Provenance: provenance("PPMD_BENCH_SEVENZIP_PROVENANCE", binary.ResolvedPath),
		DecodesRAR: HasRARCodec(string(codecs))}, nil
}

// HasRARCodec reports whether `7zz i` lists the RAR 2.9-4.x codec (method
// id 40303, "Rar3").
func HasRARCodec(info string) bool {
	_, codecs, found := strings.Cut(info, "Codecs:")
	if !found {
		return false
	}
	codecs, _, _ = strings.Cut(codecs, "Hashers:")
	for _, line := range strings.Split(codecs, "\n") {
		fields := strings.Fields(line)
		if len(fields) >= 2 && fields[len(fields)-1] == "Rar3" {
			return true
		}
	}
	return false
}

// ProbeUnrar identifies RARLAB's unrar.
func ProbeUnrar(ctx context.Context, path string) (Reference, error) {
	binary, err := Identify(path)
	if err != nil {
		return Reference{}, err
	}
	output, _ := exec.CommandContext(ctx, path).CombinedOutput()
	banner, version := ParseBanner(string(output), "UNRAR")
	if banner == "" {
		return Reference{}, fmt.Errorf("%s: no UNRAR banner in its output", path)
	}
	return Reference{Binary: binary, Banner: banner, Version: version, Provenance: provenance("PPMD_BENCH_UNRAR_PROVENANCE", binary.ResolvedPath)}, nil
}

func provenance(envName, resolved string) string {
	if value := os.Getenv(envName); value != "" {
		return value
	}
	slashed := filepath.ToSlash(resolved)
	switch {
	case strings.Contains(slashed, "/Cellar/") || strings.HasPrefix(slashed, "/opt/homebrew/"):
		return "Homebrew build"
	case strings.HasPrefix(slashed, "/usr/bin/") || strings.HasPrefix(slashed, "/usr/lib/"):
		return "distribution package"
	}
	return "unknown: set " + envName
}

// Identify digests a binary and resolves its symlinks.
func Identify(path string) (Binary, error) {
	resolved, err := filepath.EvalSymlinks(path)
	if err != nil {
		return Binary{}, err
	}
	file, err := os.Open(resolved)
	if err != nil {
		return Binary{}, err
	}
	defer file.Close()
	digest := sha256.New()
	if _, err := io.Copy(digest, file); err != nil {
		return Binary{}, err
	}
	return Binary{Path: path, ResolvedPath: resolved, SHA256: hex.EncodeToString(digest.Sum(nil))}, nil
}

// ProbeDriver runs `<path> info`.
func ProbeDriver(ctx context.Context, path string) (Driver, error) {
	binary, err := Identify(path)
	if err != nil {
		return Driver{}, err
	}
	output, err := exec.CommandContext(ctx, path, "info").Output()
	if err != nil {
		return Driver{}, fmt.Errorf("%s info: %w (is it ppmd-bench from this branch?)", path, err)
	}
	info, err := LastJSON(output)
	if err != nil {
		return Driver{}, fmt.Errorf("%s info: %w", path, err)
	}
	if info["tool"] != "ppmd-bench" {
		return Driver{}, fmt.Errorf("%s info: not ppmd-bench", path)
	}
	return Driver{Binary: binary, Info: info}, nil
}

// LastJSON parses the last non-empty line of output as a JSON object.
func LastJSON(output []byte) (map[string]any, error) {
	lines := bytes.Split(bytes.TrimSpace(output), []byte("\n"))
	if len(lines) == 0 || len(lines[len(lines)-1]) == 0 {
		return nil, errors.New("no output")
	}
	var object map[string]any
	if err := json.Unmarshal(bytes.TrimSpace(lines[len(lines)-1]), &object); err != nil {
		return nil, fmt.Errorf("last line is not JSON: %w", err)
	}
	return object, nil
}

// LockedVersion returns the version of the first [[package]] named name in a
// Cargo.lock.
func LockedVersion(lock, name string) string {
	var current string
	for _, line := range strings.Split(lock, "\n") {
		line = strings.TrimSpace(line)
		switch {
		case line == "[[package]]":
			current = ""
		case strings.HasPrefix(line, "name = "):
			current = strings.Trim(strings.TrimPrefix(line, "name = "), `"`)
		case strings.HasPrefix(line, "version = ") && current == name:
			return strings.Trim(strings.TrimPrefix(line, "version = "), `"`)
		}
	}
	return ""
}

// ProbeRust records the toolchain and commit of the checkout at repo, or
// "not-collected" for what it cannot read (a fleet host given only binaries).
func ProbeRust(ctx context.Context, repo string) Rust {
	rust := Rust{Rustc: "not-collected", Cargo: "not-collected", Commit: "not-collected"}
	run := func(dir, name string, args ...string) string {
		cmd := exec.CommandContext(ctx, name, args...)
		cmd.Dir = dir
		output, err := cmd.Output()
		if err != nil {
			return ""
		}
		return strings.TrimSpace(string(output))
	}
	if repo == "" {
		repo = run("", "git", "rev-parse", "--show-toplevel")
	}
	if repo == "" {
		return rust
	}
	if output := run(repo, "rustc", "-Vv"); output != "" {
		lines := strings.Split(output, "\n")
		rust.Rustc = lines[0]
		for _, line := range lines {
			if host, found := strings.CutPrefix(line, "host: "); found {
				rust.RustcHost = host
			}
		}
	}
	if output := run(repo, "cargo", "-V"); output != "" {
		rust.Cargo = output
	}
	if output := run(repo, "git", "rev-parse", "HEAD"); output != "" {
		rust.Commit = output
		rust.Dirty = run(repo, "git", "status", "--porcelain", "--untracked-files=no") != ""
	}
	if lock, err := os.ReadFile(filepath.Join(repo, "Cargo.lock")); err == nil {
		sum := sha256.Sum256(lock)
		rust.CargoLock = hex.EncodeToString(sum[:])
		rust.LockedTurbo = LockedVersion(string(lock), "ppmd-turbo")
		rust.LockedRust = LockedVersion(string(lock), "ppmd-rust")
	}
	return rust
}

// Release is where `cargo build --release -p <name>` puts a binary, relative
// to the checkout.
func Release(repo, name string) string {
	if runtime.GOOS == "windows" {
		name += ".exe"
	}
	return filepath.Join(repo, "target", "release", name)
}
