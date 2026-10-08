// Package report turns raw.json into report.json and report.md, and merges
// several hosts' report.json into one cross-host report.md.
package report

import (
	"encoding/json"
	"fmt"
	"os"
	"sort"
	"strings"

	"github.com/scryer-media/ppmd-turbo/bench/ppmd-turbo-bench/internal/host"
	"github.com/scryer-media/ppmd-turbo/bench/ppmd-turbo-bench/internal/procmeasure"
	"github.com/scryer-media/ppmd-turbo/bench/ppmd-turbo-bench/internal/suite"
	"github.com/scryer-media/ppmd-turbo/bench/ppmd-turbo-bench/internal/toolchain"
)

// Schema identifies report.json.
const Schema = "ppmd-turbo-bench/report/1"

// Orientation is stated in every report header.
const Orientation = "ratio = reference / contender, >1 = the contender better. The reference is 7zz on 7z rows and unrar on RAR rows (7zz when the host has no unrar and its 7zz has the RAR codecs, else none); on a raw-stream row, which 7zz cannot write, ppmd-rust is ppmd-turbo's reference. Every ratio divides the medians: above 1.000 the contender is faster (wall, CPU), smaller (output size) or lower (peak RSS); below 1.000 it is slower, larger or higher."

// Stat is a median with its range.
type Stat struct {
	Median float64 `json:"median"`
	Min    float64 `json:"min"`
	Max    float64 `json:"max"`
	N      int     `json:"n"`
}

func stat(values []float64) Stat {
	if len(values) == 0 {
		return Stat{}
	}
	sorted := append([]float64(nil), values...)
	sort.Float64s(sorted)
	middle := len(sorted) / 2
	median := sorted[middle]
	if len(sorted)%2 == 0 {
		median = (sorted[middle-1] + sorted[middle]) / 2
	}
	return Stat{Median: median, Min: sorted[0], Max: sorted[len(sorted)-1], N: len(sorted)}
}

// Row is one variant's summary of one scenario over the measured runs.
type Row struct {
	Scenario  string `json:"scenario"`
	Group     string `json:"group"`
	Op        string `json:"op"`
	Variant   string `json:"variant"`
	Role      string `json:"role"`
	Wall      Stat   `json:"wall_seconds"`
	CPU       Stat   `json:"cpu_seconds"`
	RSS       Stat   `json:"max_rss_bytes"`
	RSSSource string `json:"rss_source"`
	Load      Stat   `json:"load_average"`
	BytesIn   int64  `json:"bytes_in"`
	BytesOut  int64  `json:"bytes_out"`
	// ThroughputMiBs is plain bytes per wall second: output for a decode,
	// input for an encode.
	ThroughputMiBs float64 `json:"throughput_mib_s,omitempty"`
	// InprocSeconds is ppmd-bench's own timing of the codec work alone.
	InprocSeconds Stat     `json:"inproc_seconds"`
	OK            int      `json:"ok"`
	Failed        int      `json:"failed"`
	DNF           string   `json:"dnf,omitempty"`
	Failures      []string `json:"failures,omitempty"`
	Extra         string   `json:"extra,omitempty"`
}

// Ratio compares one variant against its scenario's reference.
type Ratio struct {
	Scenario  string   `json:"scenario"`
	Group     string   `json:"group"`
	Variant   string   `json:"variant"`
	Reference string   `json:"reference"`
	Wall      *float64 `json:"wall,omitempty"`
	CPU       *float64 `json:"cpu,omitempty"`
	RSS       *float64 `json:"rss,omitempty"`
	// Size is the output-size ratio of an encode.
	Size *float64 `json:"size,omitempty"`
}

// Report is report.json.
type Report struct {
	SchemaVersion int                       `json:"schema_version"`
	Schema        string                    `json:"schema"`
	Orientation   string                    `json:"orientation"`
	StartedUTC    string                    `json:"started_utc"`
	FinishedUTC   string                    `json:"finished_utc"`
	Machine       host.Machine              `json:"machine"`
	Toolchain     toolchain.Toolchain       `json:"toolchain"`
	Corpus        string                    `json:"corpus"`
	RunProfile    string                    `json:"run_profile"`
	Warmups       int                       `json:"warmups"`
	Repeats       int                       `json:"repeats"`
	Scenarios     []suite.Scenario          `json:"scenarios"`
	Rows          []Row                     `json:"rows"`
	Ratios        []Ratio                   `json:"ratios"`
	RSS           []procmeasure.RSSScenario `json:"rss_scenarios"`
	// Failures lists every failed or unfinished candidate, contender or
	// reference run; SecondaryFailures the ones that do not fail the run.
	Failures          []string `json:"failures"`
	SecondaryFailures []string `json:"secondary_failures"`
}

// Reference is the variant a scenario's ratios divide by: its reference
// role, else ppmd-rust when ppmd-turbo is there to compare, else none.
func Reference(scenario suite.Scenario) string {
	for _, run := range scenario.Variants {
		if run.Role == suite.RoleReference {
			return run.Variant
		}
	}
	var turbo, rust bool
	for _, run := range scenario.Variants {
		turbo = turbo || run.Variant == suite.VariantTurbo
		rust = rust || run.Variant == suite.VariantPPMdRust
	}
	if turbo && rust {
		return suite.VariantPPMdRust
	}
	return ""
}

// Build summarises raw runs.
func Build(raw *suite.Raw) *Report {
	report := &Report{
		SchemaVersion: 1, Schema: Schema, Orientation: Orientation,
		StartedUTC: raw.StartedUTC, FinishedUTC: raw.FinishedUTC, Machine: raw.Machine, Toolchain: raw.Toolchain,
		RunProfile: raw.RunProfile, Warmups: raw.Warmups, Repeats: raw.Repeats, Scenarios: raw.Scenarios,
		Rows: []Row{}, Ratios: []Ratio{}, RSS: []procmeasure.RSSScenario{},
		Failures: []string{}, SecondaryFailures: []string{},
	}
	if raw.Fixtures != nil {
		report.Corpus = raw.Fixtures.Profile
	}
	type key struct{ scenario, variant string }
	grouped := map[key][]suite.RunRecord{}
	for _, run := range raw.Runs {
		if run.Status != suite.StatusOK {
			line := fmt.Sprintf("%s / %s (repeat %d%s): %s %s", run.Scenario, run.Variant, run.Repeat, warmupTag(run.Warmup), run.Failure, firstNonEmpty(run.Error, run.StderrLine))
			if run.Role == suite.RoleSecondary {
				report.SecondaryFailures = append(report.SecondaryFailures, line)
			} else {
				report.Failures = append(report.Failures, line)
			}
		}
		if run.Warmup && run.Status == suite.StatusOK {
			continue
		}
		k := key{run.Scenario, run.Variant}
		grouped[k] = append(grouped[k], run)
	}
	for _, scenario := range raw.Scenarios {
		byVariant := map[string]*Row{}
		for _, variant := range scenario.Variants {
			runs := grouped[key{scenario.ID, variant.Variant}]
			if len(runs) == 0 {
				continue
			}
			row := summarize(scenario, runs)
			report.Rows = append(report.Rows, row)
			byVariant[variant.Variant] = &row
		}
		referenceName := Reference(scenario)
		reference := byVariant[referenceName]
		for _, variant := range scenario.Variants {
			ours := byVariant[variant.Variant]
			if ours == nil || variant.Variant == referenceName || ours.OK == 0 {
				continue
			}
			if reference != nil && reference.OK > 0 {
				ratio := Ratio{Scenario: scenario.ID, Group: scenario.Group, Variant: variant.Variant, Reference: referenceName,
					Wall: divide(reference.Wall.Median, ours.Wall.Median), CPU: divide(reference.CPU.Median, ours.CPU.Median),
					RSS: divide(reference.RSS.Median, ours.RSS.Median)}
				if scenario.Op == suite.OpEncode7z {
					ratio.Size = divide(float64(reference.BytesOut), float64(ours.BytesOut))
				}
				report.Ratios = append(report.Ratios, ratio)
			}
			if variant.Role == suite.RoleSecondary {
				continue
			}
			rss := procmeasure.RSSScenario{Scenario: scenario.ID, Variant: variant.Variant, CandidateSource: ours.RSSSource,
				CandidateMedianBytes: int64(ours.RSS.Median), CandidateMinBytes: int64(ours.RSS.Min), CandidateMaxBytes: int64(ours.RSS.Max)}
			if reference != nil && reference.OK > 0 {
				rss.ReferenceMedianBytes, rss.ReferenceMinBytes, rss.ReferenceMaxBytes = int64(reference.RSS.Median), int64(reference.RSS.Min), int64(reference.RSS.Max)
				rss.ReferenceSource = reference.RSSSource
			}
			procmeasure.CompleteRSSScenario(&rss)
			report.RSS = append(report.RSS, rss)
		}
	}
	procmeasure.SortRSSScenarios(report.RSS)
	return report
}

func warmupTag(warmup bool) string {
	if warmup {
		return ", warmup"
	}
	return ""
}

func firstNonEmpty(values ...string) string {
	for _, value := range values {
		if value != "" {
			return value
		}
	}
	return ""
}

func divide(a, b float64) *float64 {
	if a <= 0 || b <= 0 {
		return nil
	}
	value := a / b
	return &value
}

func summarize(scenario suite.Scenario, runs []suite.RunRecord) Row {
	first := runs[0]
	row := Row{Scenario: scenario.ID, Group: scenario.Group, Op: scenario.Op, Variant: first.Variant, Role: first.Role,
		BytesIn: scenario.BytesIn, BytesOut: scenario.BytesOut}
	var wall, cpu, rss, load, out, inproc, alloc []float64
	var sources []string
	verified := ""
	for _, run := range runs {
		if run.Status != suite.StatusOK {
			row.Failed++
			if run.Status == suite.StatusDNF {
				row.DNF = run.Failure
			}
			row.Failures = append(row.Failures, run.Failure)
			continue
		}
		row.OK++
		wall = append(wall, run.WallSeconds)
		cpu = append(cpu, run.UserSeconds+run.SysSeconds)
		rss = append(rss, float64(run.MaxRSSBytes))
		if run.LoadAverage >= 0 {
			load = append(load, run.LoadAverage)
		}
		out = append(out, float64(run.BytesOut))
		if seconds, ok := run.Result["inproc_seconds"].(float64); ok {
			inproc = append(inproc, seconds)
		}
		if bytes, ok := run.Result["peak_alloc_bytes"].(float64); ok {
			alloc = append(alloc, bytes)
		}
		sources = append(sources, run.RSSSource)
		if run.Verified != "" {
			verified = run.Verified
		}
	}
	row.Wall, row.CPU, row.RSS, row.Load, row.InprocSeconds = stat(wall), stat(cpu), stat(rss), stat(load), stat(inproc)
	if len(out) > 0 {
		row.BytesOut = int64(stat(out).Median)
	}
	row.RSSSource = procmeasure.JoinSources(sources)
	var extra []string
	if row.InprocSeconds.N > 0 {
		extra = append(extra, fmt.Sprintf("in-process %.3fs", row.InprocSeconds.Median))
	}
	if len(alloc) > 0 {
		extra = append(extra, fmt.Sprintf("peak heap %s MiB", procmeasure.MiB(int64(stat(alloc).Median))))
	}
	if verified != "" {
		extra = append(extra, "decodes back: "+verified)
	}
	row.Extra = strings.Join(extra, "; ")
	if row.Wall.Median > 0 {
		bytes := row.BytesOut
		if scenario.Op == suite.OpEncode7z {
			bytes = row.BytesIn
		}
		if bytes > 0 {
			row.ThroughputMiBs = float64(bytes) / (1 << 20) / row.Wall.Median
		}
	}
	return row
}

// Write saves report.json.
func Write(path string, report *Report) error {
	data, err := json.MarshalIndent(report, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(path, append(data, '\n'), 0o644)
}

// Load reads a report.json.
func Load(path string) (*Report, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var report Report
	if err := json.Unmarshal(data, &report); err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	if report.SchemaVersion != 1 || report.Schema != Schema {
		return nil, fmt.Errorf("%s: schema %q version %d, want %q version 1", path, report.Schema, report.SchemaVersion, Schema)
	}
	return &report, nil
}
