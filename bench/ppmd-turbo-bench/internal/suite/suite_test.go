package suite

import (
	"strings"
	"testing"

	"github.com/scryer-media/ppmd-turbo/bench/ppmd-turbo-bench/internal/fixtures"
)

// fakeManifest is a corpus shaped like ppmd-corpus's, without files: the
// planner needs only the records.
func fakeManifest(corpus string) *fixtures.Manifest {
	m := &fixtures.Manifest{Schema: fixtures.Schema, Profile: corpus}
	size := int64(1 << 20)
	if corpus == "full" {
		size = 16 << 20
	}
	for _, kind := range []string{"text", "binary", "random"} {
		m.Archives = append(m.Archives, fixtures.Archive{Name: kind + ".o6.m16m", File: kind + ".o6.m16m.7z", Kind: kind,
			Order: 6, Mem: 16 << 20, RequestedMem: 16 << 20, PackedLen: size / 4, UnpackedLen: size, PayloadCRC32: "00000000"})
	}
	m.Raw = []fixtures.Raw{{Name: "text.o8.m1g.ppmd-rust", File: "text.o8.m1g.ppmd-rust.ppmd", Kind: "text", Order: 8, Mem: 1 << 30,
		PackedLen: size / 6, UnpackedLen: size}}
	m.Sources = []fixtures.Source{{Name: "fixture-text-1m.bin", File: "fixture-text-1m.bin", Kind: "text", Size: size}}
	m.RAR = []fixtures.RARFile{{Name: "rar4_ppm_solid_restart.rar", Path: "/corpus/rar4_ppm_solid_restart.rar", Unpacked: 1_600_000, Bytes: 1_238_185}}
	return m
}

func plan(t *testing.T, profileName string, tools Tools) []Scenario {
	t.Helper()
	profile, err := ProfileByName(profileName)
	if err != nil {
		t.Fatal(err)
	}
	scenarios, err := Plan(fakeManifest(profile.Corpus), "/corpus", "/scratch", tools, profile)
	if err != nil {
		t.Fatal(err)
	}
	return scenarios
}

func TestPlanRolesAndReferences(t *testing.T) {
	tools := Tools{Driver: "ppmd-bench", SevenZip: "7zz", SevenZipRAR: true, Turbo: map[string]bool{}}
	ids := map[string]bool{}
	for _, scenario := range plan(t, ProfileQuick, tools) {
		if ids[scenario.ID] {
			t.Fatalf("duplicate scenario %s", scenario.ID)
		}
		ids[scenario.ID] = true
		roles := map[string]string{}
		for _, run := range scenario.Variants {
			roles[run.Variant] = run.Role
			if run.Variant == VariantTurbo {
				t.Errorf("%s: a ppmd-turbo row although the driver provides nothing", scenario.ID)
			}
		}
		if roles[VariantPPMdRust] != RoleContender {
			t.Errorf("%s: no ppmd-rust contender", scenario.ID)
		}
		switch scenario.Op {
		case OpDecode7z, OpEncode7z:
			if strings.HasPrefix(scenario.ID, "decode-7z-raw/") {
				if _, ok := roles[VariantSevenZip]; ok {
					t.Errorf("%s: 7zz cannot read a raw stream", scenario.ID)
				}
			} else if roles[VariantSevenZip] != RoleReference {
				t.Errorf("%s: 7zz is not the reference", scenario.ID)
			}
		case OpDecodeRAR:
			if roles[VariantSevenZip] != RoleReference {
				t.Errorf("%s: without unrar, 7zz must be the reference", scenario.ID)
			}
		default:
			t.Errorf("%s: unexpected op %s", scenario.ID, scenario.Op)
		}
	}
	for _, want := range []string{"decode-7z/text.o6.m16m", "decode-7z-raw/text.o8.m1g", "decode-rar/rar4_ppm_solid_restart", "encode-7z/text-1m.o6.m16m"} {
		if !ids[want] {
			t.Errorf("missing %s in %v", want, ids)
		}
	}
}

func TestUnrarTakesOverTheRARReference(t *testing.T) {
	tools := Tools{Driver: "ppmd-bench", SevenZip: "7zz", SevenZipRAR: true, Unrar: "unrar", Turbo: map[string]bool{OpDecodeRAR: true}}
	for _, scenario := range plan(t, ProfileQuick, tools) {
		if scenario.Op != OpDecodeRAR {
			continue
		}
		roles := map[string]string{}
		for _, run := range scenario.Variants {
			roles[run.Variant] = run.Role
		}
		if roles[VariantUnrar] != RoleReference || roles[VariantSevenZip] != RoleSecondary || roles[VariantTurbo] != RoleCandidate {
			t.Fatalf("%s: roles %v", scenario.ID, roles)
		}
	}
}

func TestRARRowsWithoutAReference(t *testing.T) {
	tools := Tools{Driver: "ppmd-bench", SevenZip: "7zz", Turbo: map[string]bool{}}
	for _, scenario := range plan(t, ProfileQuick, tools) {
		if scenario.Op == OpDecodeRAR && (len(scenario.Variants) != 1 || scenario.Variants[0].Variant != VariantPPMdRust) {
			t.Fatalf("%s: %+v", scenario.ID, scenario.Variants)
		}
	}
}

func TestNoProfileBenchmarksTheCarryLessEncoder(t *testing.T) {
	all := Tools{Driver: "ppmd-bench", SevenZip: "7zz", Unrar: "unrar", Turbo: map[string]bool{OpDecode7z: true, OpDecodeRAR: true, OpEncode7z: true}}
	for _, name := range Profiles {
		for _, scenario := range plan(t, name, all) {
			if scenario.Op == OpEncode7z && strings.Contains(strings.Join(scenario.Variants[0].Args, " "), "rar") {
				t.Errorf("%s: %s encodes for RAR", name, scenario.ID)
			}
			if strings.Contains(scenario.ID, "encode-rar") {
				t.Errorf("%s: %s", name, scenario.ID)
			}
			for _, run := range scenario.Variants {
				if run.JSON && run.Args[0] != scenario.Op {
					t.Errorf("%s: %s runs ppmd-bench %s", name, scenario.ID, run.Args[0])
				}
			}
		}
	}
}

func TestEncodeRowsWriteToScratch(t *testing.T) {
	tools := Tools{Driver: "ppmd-bench", SevenZip: "7zz", Turbo: map[string]bool{OpEncode7z: true}}
	for _, scenario := range plan(t, ProfileFull, tools) {
		if scenario.Op != OpEncode7z {
			continue
		}
		seen := map[string]bool{}
		for _, run := range scenario.Variants {
			if !strings.HasPrefix(run.Output, "/scratch/") || seen[run.Output] {
				t.Errorf("%s / %s: output %q", scenario.ID, run.Variant, run.Output)
			}
			seen[run.Output] = true
			if run.Variant == VariantSevenZip && run.Dir != "/corpus" {
				t.Errorf("%s: 7zz a runs in %q", scenario.ID, run.Dir)
			}
		}
	}
}

func TestProfilesAndCounts(t *testing.T) {
	for name, want := range map[string][2]int{ProfileQuick: {1, 0}, ProfileFull: {5, 1}, ProfileFleet: {3, 1}} {
		profile, err := ProfileByName(name)
		if err != nil || profile.Repeats != want[0] || profile.Warmups != want[1] {
			t.Errorf("%s: %+v %v", name, profile, err)
		}
	}
	if _, err := ProfileByName("nightly"); err == nil {
		t.Error("unknown profile accepted")
	}
	scenarios := []Scenario{{Variants: make([]Run, 3)}, {Variants: make([]Run, 2)}}
	if got := Processes(scenarios, 3, 1); got != 20 {
		t.Fatalf("Processes = %d, want 20", got)
	}
	one := []Scenario{{Op: OpDecode7z, BytesIn: 4_500_000, BytesOut: 200_000_000, Variants: make([]Run, 1)}}
	if got, want := Projected(one, 1, 0), (1+1+processSeconds)*X86Slowdown; got < want-1e-9 || got > want+1e-9 {
		t.Fatalf("Projected = %v, want %v", got, want)
	}
}

func TestMemLabel(t *testing.T) {
	for mem, want := range map[int64]string{1 << 30: "1g", 16 << 20: "16m", 64 << 10: "64k", 1000: "1000"} {
		if got := MemLabel(mem); got != want {
			t.Errorf("MemLabel(%d) = %s, want %s", mem, got, want)
		}
	}
}

func TestOrderAlternates(t *testing.T) {
	if got := orderFor(3, 0); got[0] != 0 || got[2] != 2 {
		t.Fatalf("pass 0: %v", got)
	}
	if got := orderFor(3, 1); got[0] != 2 || got[2] != 0 {
		t.Fatalf("pass 1: %v", got)
	}
}
