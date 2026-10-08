package suite

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"time"

	"github.com/scryer-media/ppmd-turbo/bench/ppmd-turbo-bench/internal/fixtures"
	"github.com/scryer-media/ppmd-turbo/bench/ppmd-turbo-bench/internal/host"
	"github.com/scryer-media/ppmd-turbo/bench/ppmd-turbo-bench/internal/procmeasure"
	"github.com/scryer-media/ppmd-turbo/bench/ppmd-turbo-bench/internal/toolchain"
)

// RawSchema identifies raw.json.
const RawSchema = "ppmd-turbo-bench/raw/1"

// Statuses.
const (
	StatusOK     = "ok"
	StatusFailed = "failed"
	StatusDNF    = "dnf"
)

// RunRecord is one measured process run.
type RunRecord struct {
	Scenario string `json:"scenario"`
	Group    string `json:"group"`
	Op       string `json:"op"`
	Variant  string `json:"variant"`
	Role     string `json:"role"`
	Tool     string `json:"tool"`
	Warmup   bool   `json:"warmup"`
	Repeat   int    `json:"repeat"`
	Position int    `json:"position"`
	Command  string `json:"command"`
	// LoadAverage is the host's one-minute load just before the run (-1
	// where the OS has none).
	LoadAverage float64 `json:"load_average"`
	procmeasure.Measurement
	BytesIn  int64 `json:"bytes_in"`
	BytesOut int64 `json:"bytes_out"`
	// Result is ppmd-bench's own JSON line.
	Result map[string]any `json:"result,omitempty"`
	// Verified is the untimed ppmd-rust decode of an encode's output (first
	// measured repeat only): "ok" or the failure.
	Verified   string `json:"verified,omitempty"`
	Status     string `json:"status"`
	Failure    string `json:"failure,omitempty"`
	Error      string `json:"error,omitempty"`
	StderrLine string `json:"stderr_line,omitempty"`
}

// Raw is raw.json: everything a run measured.
type Raw struct {
	SchemaVersion  int                 `json:"schema_version"`
	Schema         string              `json:"schema"`
	StartedUTC     string              `json:"started_utc"`
	FinishedUTC    string              `json:"finished_utc"`
	Machine        host.Machine        `json:"machine"`
	Toolchain      toolchain.Toolchain `json:"toolchain"`
	Fixtures       *fixtures.Manifest  `json:"fixtures"`
	RunProfile     string              `json:"run_profile"`
	Warmups        int                 `json:"warmups"`
	Repeats        int                 `json:"repeats"`
	PinCPUs        string              `json:"pin_cpus,omitempty"`
	TimeoutSeconds float64             `json:"timeout_seconds,omitempty"`
	Scenarios      []Scenario          `json:"scenarios"`
	Runs           []RunRecord         `json:"runs"`
}

// Options control Execute.
type Options struct {
	Warmups int
	Repeats int
	PinCPUs string
	Timeout time.Duration
	Log     io.Writer
	// Driver is ppmd-bench, used for the untimed check of encode output.
	Driver string
}

// orderFor alternates the variant order every pass (A B C, C B A, ...), so
// no variant always runs first on a cold or last on a warm cache.
func orderFor(n, pass int) []int {
	order := make([]int, n)
	for i := range order {
		if pass%2 == 0 {
			order[i] = i
		} else {
			order[i] = n - 1 - i
		}
	}
	return order
}

// Execute runs every scenario, interleaving its variants, and appends a
// record per run.
func Execute(ctx context.Context, raw *Raw, options Options) {
	logf := func(format string, args ...any) {
		if options.Log != nil {
			fmt.Fprintf(options.Log, format+"\n", args...)
		}
	}
	for index, scenario := range raw.Scenarios {
		if ctx.Err() != nil {
			return
		}
		logf("[%d/%d] %s", index+1, len(raw.Scenarios), scenario.ID)
		total := options.Warmups + options.Repeats
		for pass := range total {
			warmup := pass < options.Warmups
			repeat := pass - options.Warmups
			if warmup {
				repeat = pass
			}
			for position, variant := range orderFor(len(scenario.Variants), pass) {
				run := scenario.Variants[variant]
				record := measure(ctx, scenario, run, options)
				record.Warmup, record.Repeat, record.Position = warmup, repeat, position
				if !warmup && repeat == 0 && run.JSON && scenario.Op == OpEncode7z && record.Status == StatusOK {
					record.Verified = verifyEncode(ctx, options.Driver, scenario, run.Output)
					if record.Verified != "ok" {
						record.Status, record.Failure, record.Error = StatusFailed, "output-does-not-decode", record.Verified
					}
				}
				if run.Output != "" {
					_ = os.Remove(run.Output)
				}
				line := fmt.Sprintf("  %-12s %-6s wall %.3fs rss %s MiB", run.Variant, record.Status, record.WallSeconds, procmeasure.MiB(record.MaxRSSBytes))
				if warmup {
					line += " (warmup)"
				}
				if record.Error != "" {
					line += " " + record.Error
				}
				logf("%s", line)
				raw.Runs = append(raw.Runs, record)
			}
		}
	}
}

func measure(ctx context.Context, scenario Scenario, run Run, options Options) RunRecord {
	command := procmeasure.Command{Path: run.Tool, Args: run.Args, Dir: run.Dir, PinCPUs: options.PinCPUs, Timeout: options.Timeout}
	record := RunRecord{
		Scenario: scenario.ID, Group: scenario.Group, Op: scenario.Op, Variant: run.Variant, Role: run.Role,
		Tool: run.Tool, Command: command.Describe(), LoadAverage: host.LoadAverage(),
		BytesIn: scenario.BytesIn, BytesOut: scenario.BytesOut,
	}
	if run.Output != "" {
		_ = os.Remove(run.Output)
	}
	result := procmeasure.Run(ctx, command)
	record.Measurement = result.Measurement
	switch {
	case result.Failure == "timeout":
		record.Status, record.Failure = StatusDNF, "timeout"
	case result.Failure == "signal":
		record.Status, record.Failure = StatusDNF, "signal"
	case result.Failure != "":
		record.Status, record.Failure = StatusFailed, result.Failure
	case result.ExitCode == 3 && run.JSON:
		record.Status, record.Failure = StatusFailed, "not-implemented"
	case result.ExitCode != 0:
		record.Status, record.Failure = StatusFailed, fmt.Sprintf("exit-%d", result.ExitCode)
	default:
		record.Status = StatusOK
	}
	if result.Err != nil {
		record.Error = result.Err.Error()
	}
	if record.Status != StatusOK {
		record.StderrLine = lastLine(result.Stderr, result.Stdout)
	}
	if run.JSON && record.Status == StatusOK {
		object, err := toolchain.LastJSON([]byte(result.Stdout))
		if err != nil {
			record.Status, record.Failure, record.Error = StatusFailed, "bad-json", err.Error()
		} else {
			record.Result = object
			checkResult(scenario, &record)
		}
	}
	if scenario.Op == OpEncode7z && record.Status == StatusOK {
		if info, err := os.Stat(run.Output); err == nil {
			record.BytesOut = info.Size()
		} else {
			record.Status, record.Failure, record.Error = StatusFailed, "no-output", fmt.Sprintf("%s was not written", run.Output)
		}
	}
	if record.Status == StatusOK && (record.MaxRSSBytes <= 0 || record.RSSSource == "") {
		record.Status, record.Failure = StatusFailed, procmeasure.FailureMissingRSS
		record.Error = "the process exited but its peak RSS was not captured"
	}
	return record
}

// checkResult holds a decode's output to the corpus: its length and CRC-32
// must be the payload's. (ppmd-bench checks a RAR member against the CRC in
// its header itself.)
func checkResult(scenario Scenario, record *RunRecord) {
	if scenario.Op == OpEncode7z {
		return
	}
	got := int64(number(record.Result["bytes_out"]))
	if got != scenario.BytesOut {
		record.Status, record.Failure = StatusFailed, "short-output"
		record.Error = fmt.Sprintf("decoded %d bytes, the fixture holds %d", got, scenario.BytesOut)
		return
	}
	if crc, _ := record.Result["crc32"].(string); scenario.CRC32 != "" && crc != scenario.CRC32 {
		record.Status, record.Failure = StatusFailed, "crc-mismatch"
		record.Error = fmt.Sprintf("output CRC-32 %s, the payload's is %s", crc, scenario.CRC32)
	}
}

func number(value any) float64 {
	if f, ok := value.(float64); ok {
		return f
	}
	return 0
}

// verifyEncode decodes an encode's raw stream with ppmd-rust, untimed, and
// compares the CRC-32 with the source's.
func verifyEncode(ctx context.Context, driver string, scenario Scenario, output string) string {
	args := []string{OpDecode7z, "--impl", VariantPPMdRust, "--in", output, "--order", strconv.Itoa(scenario.Order),
		"--mem", strconv.FormatInt(scenario.Mem, 10), "--size", strconv.FormatInt(scenario.BytesIn, 10)}
	stdout, err := exec.CommandContext(ctx, driver, args...).Output()
	if err != nil {
		return fmt.Sprintf("ppmd-rust decode: %v", err)
	}
	object, err := toolchain.LastJSON(stdout)
	if err != nil {
		return fmt.Sprintf("ppmd-rust decode: %v", err)
	}
	if crc, _ := object["crc32"].(string); crc != scenario.CRC32 {
		return fmt.Sprintf("ppmd-rust decodes it to CRC-32 %s, the source's is %s", crc, scenario.CRC32)
	}
	return "ok"
}

func lastLine(texts ...string) string {
	for _, text := range texts {
		lines := strings.Split(strings.TrimSpace(text), "\n")
		for i := len(lines) - 1; i >= 0; i-- {
			if line := strings.TrimSpace(lines[i]); line != "" {
				return line
			}
		}
	}
	return ""
}

// Write saves raw.json.
func Write(path string, raw *Raw) error {
	data, err := json.MarshalIndent(raw, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(path, append(data, '\n'), 0o644)
}

// Load reads raw.json.
func Load(path string) (*Raw, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var raw Raw
	if err := json.Unmarshal(data, &raw); err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	if raw.Schema != RawSchema {
		return nil, fmt.Errorf("%s: schema %q, want %q", path, raw.Schema, RawSchema)
	}
	return &raw, nil
}
