package report

import (
	"strings"
	"testing"

	"github.com/scryer-media/ppmd-turbo/bench/ppmd-turbo-bench/internal/fixtures"
	"github.com/scryer-media/ppmd-turbo/bench/ppmd-turbo-bench/internal/host"
	"github.com/scryer-media/ppmd-turbo/bench/ppmd-turbo-bench/internal/procmeasure"
	"github.com/scryer-media/ppmd-turbo/bench/ppmd-turbo-bench/internal/suite"
)

func record(scenario suite.Scenario, variant, role string, wall float64, rss int64, status string) suite.RunRecord {
	return suite.RunRecord{
		Scenario: scenario.ID, Group: scenario.Group, Op: scenario.Op, Variant: variant, Role: role, Status: status,
		BytesIn: scenario.BytesIn, BytesOut: scenario.BytesOut,
		Measurement: procmeasure.Measurement{WallSeconds: wall, UserSeconds: wall, MaxRSSBytes: rss, RSSSource: procmeasure.RSSSourceRusage},
		Result:      map[string]any{"inproc_seconds": wall / 2, "peak_alloc_bytes": float64(16 << 20)},
	}
}

func sampleRaw() *suite.Raw {
	decode := suite.Scenario{ID: "decode-7z/text-1m.o6.m16m", Group: "7z decode: text", Op: suite.OpDecode7z, BytesIn: 1 << 18, BytesOut: 1 << 20,
		Variants: []suite.Run{{Variant: suite.VariantTurbo, Role: suite.RoleCandidate}, {Variant: suite.VariantPPMdRust, Role: suite.RoleContender},
			{Variant: suite.VariantSevenZip, Role: suite.RoleReference}}}
	raw := suite.Scenario{ID: "decode-7z-raw/text-1m.o8.m1g", Group: "7z decode: raw stream", Op: suite.OpDecode7z, BytesIn: 1 << 18, BytesOut: 1 << 20,
		Variants: []suite.Run{{Variant: suite.VariantTurbo, Role: suite.RoleCandidate}, {Variant: suite.VariantPPMdRust, Role: suite.RoleContender}}}
	r := &suite.Raw{Schema: suite.RawSchema, Machine: host.Machine{Label: "test-host", OS: "linux", Architecture: "arm64"},
		Fixtures: &fixtures.Manifest{Profile: "quick"}, RunProfile: "quick", Repeats: 3, Scenarios: []suite.Scenario{decode, raw}}
	for _, wall := range []float64{1.0, 1.2, 1.1} {
		r.Runs = append(r.Runs,
			record(decode, suite.VariantTurbo, suite.RoleCandidate, wall, 20<<20, suite.StatusOK),
			record(decode, suite.VariantPPMdRust, suite.RoleContender, wall*3, 20<<20, suite.StatusOK),
			record(decode, suite.VariantSevenZip, suite.RoleReference, wall*2, 40<<20, suite.StatusOK),
			record(raw, suite.VariantTurbo, suite.RoleCandidate, wall, 10<<20, suite.StatusOK),
			record(raw, suite.VariantPPMdRust, suite.RoleContender, wall*4, 10<<20, suite.StatusOK))
	}
	return r
}

func TestRatiosAreReferenceOverContender(t *testing.T) {
	built := Build(sampleRaw())
	if len(built.Failures) != 0 {
		t.Fatalf("failures %v", built.Failures)
	}
	got := map[string]Ratio{}
	for _, ratio := range built.Ratios {
		got[ratio.Scenario+" "+ratio.Variant] = ratio
	}
	check := func(key, reference string, wall, rss float64) {
		t.Helper()
		ratio, ok := got[key]
		if !ok {
			t.Fatalf("no ratio for %s in %v", key, got)
		}
		if ratio.Reference != reference || *ratio.Wall < wall-1e-9 || *ratio.Wall > wall+1e-9 || *ratio.RSS != rss {
			t.Fatalf("%s: %+v (wall %v rss %v), want reference %s wall %v rss %v", key, ratio, *ratio.Wall, *ratio.RSS, reference, wall, rss)
		}
	}
	check("decode-7z/text-1m.o6.m16m ppmd-turbo", suite.VariantSevenZip, 2, 2)
	check("decode-7z/text-1m.o6.m16m ppmd-rust", suite.VariantSevenZip, 2.0/3, 2)
	check("decode-7z-raw/text-1m.o8.m1g ppmd-turbo", suite.VariantPPMdRust, 4, 1)
	if len(built.Ratios) != 3 {
		t.Fatalf("ratios %v", built.Ratios)
	}
	md := Markdown(built)
	for _, want := range []string{Orientation, "## 7z decode: text", "## 7z decode: raw stream", "7zz (reference)", "| 2.000 |", "## Peak RSS per scenario"} {
		if !strings.Contains(md, want) {
			t.Errorf("report.md lacks %q:\n%s", want, md)
		}
	}
}

func TestFailuresAndSecondaries(t *testing.T) {
	r := sampleRaw()
	failed := record(r.Scenarios[0], suite.VariantPPMdRust, suite.RoleContender, 0, 0, suite.StatusFailed)
	failed.Failure = "crc-mismatch"
	secondary := record(r.Scenarios[0], suite.VariantSevenZip, suite.RoleSecondary, 0, 0, suite.StatusFailed)
	secondary.Failure = "exit-2"
	r.Runs = append(r.Runs, failed, secondary)
	built := Build(r)
	if len(built.Failures) != 1 || !strings.Contains(built.Failures[0], "crc-mismatch") || len(built.SecondaryFailures) != 1 {
		t.Fatalf("failures %v secondary %v", built.Failures, built.SecondaryFailures)
	}
}

func TestMergeOneColumnPerHost(t *testing.T) {
	a, b := Build(sampleRaw()), Build(sampleRaw())
	b.Machine.Label = "other-host"
	merged := Merge([]*Report{a, b})
	if !strings.Contains(merged, "| scenario | variant | test-host | other-host |") {
		t.Fatalf("merged:\n%s", merged)
	}
}
