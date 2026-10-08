// Package suite is the benchmark matrix and its runner: which scenario runs
// which variants with which arguments, and the interleaved loop that measures
// every run as its own child process.
package suite

import (
	"fmt"
	"path/filepath"
	"strconv"
	"strings"

	"github.com/scryer-media/ppmd-turbo/bench/ppmd-turbo-bench/internal/fixtures"
)

// Variant names.
const (
	VariantTurbo    = "ppmd-turbo"
	VariantPPMdRust = "ppmd-rust"
	VariantSevenZip = "7zz"
	VariantUnrar    = "unrar"
)

// Roles. The candidate is this crate; the contender is ppmd-rust, the other
// Rust implementation; the reference is the tool every ratio divides by (7zz
// for 7z rows, unrar for RAR rows). A failing candidate, contender or
// reference run fails the whole run; a failing secondary run (7zz on a RAR
// row that unrar references) is reported and does not.
const (
	RoleCandidate = "candidate"
	RoleContender = "contender"
	RoleReference = "reference"
	RoleSecondary = "secondary"
)

// Ops: the ppmd-bench subcommand a scenario measures. There is no encode-rar:
// the carry-less encoder is correctness-only and never benchmarked.
const (
	OpDecode7z  = "decode-7z"
	OpDecodeRAR = "decode-rar"
	OpEncode7z  = "encode-7z"
)

// Groups, in report order.
var Groups = []string{
	"7z decode: text",
	"7z decode: other payloads",
	"7z decode: raw stream",
	"RAR decode",
	"7z encode",
}

// Scenario is one row group of the matrix: one operation on one input, run
// by every variant.
type Scenario struct {
	ID    string `json:"id"`
	Group string `json:"group"`
	Op    string `json:"op"`
	// Fixture is the archive, raw stream, RAR file or source name.
	Fixture string `json:"fixture"`
	Kind    string `json:"kind,omitempty"`
	Order   int    `json:"order"`
	Mem     int64  `json:"mem"`
	// BytesIn / BytesOut are the operation's input and output: packed and
	// unpacked bytes for a decode, source bytes (and nothing known yet) for
	// an encode.
	BytesIn  int64 `json:"bytes_in"`
	BytesOut int64 `json:"bytes_out"`
	// CRC32 is the expected CRC-32 of a decode's output, or of the source
	// an encode's output must decode back to.
	CRC32 string `json:"crc32,omitempty"`
	// ExpectedOut is an encode's expected output size, from a corpus
	// archive of the same payload and order, for the projection only.
	ExpectedOut int64  `json:"expected_out,omitempty"`
	Note        string `json:"note,omitempty"`
	Variants    []Run  `json:"variants"`
}

// Run is one variant's command for a scenario.
type Run struct {
	Variant string   `json:"variant"`
	Role    string   `json:"role"`
	Tool    string   `json:"tool"`
	Args    []string `json:"args"`
	// Dir is the working directory (7zz a stores paths relative to it).
	Dir string `json:"dir,omitempty"`
	// Output is the file an encode writes, removed after every run.
	Output string `json:"output,omitempty"`
	// JSON marks a ppmd-bench run whose last stdout line is its result.
	JSON bool `json:"json"`
}

// Tools are the binaries a plan runs.
type Tools struct {
	Driver   string
	SevenZip string
	// SevenZipRAR is set when the 7zz build has the RAR codecs.
	SevenZipRAR bool
	// Unrar is optional; without it a RAR-capable 7zz is the RAR rows'
	// reference, and without either the RAR rows have none.
	Unrar string
	// Turbo lists the ppmd-bench ops ppmd-turbo provides; only those get
	// candidate rows.
	Turbo map[string]bool
}

// EncodeSetting is one encode row's model order and memory size.
type EncodeSetting struct {
	Order int
	Mem   int64
}

// Run profiles.
const (
	ProfileQuick = "quick" // the small corpus, one repeat: a smoke run
	ProfileFull  = "full"  // the full corpus, 5 repeats, 1 warmup
	ProfileFleet = "fleet" // the full corpus less its non-text memory sweep, 3 repeats, 1 warmup: one AWS host in under an hour
)

// Profiles lists the run profiles in the order the help text gives them.
var Profiles = []string{ProfileQuick, ProfileFull, ProfileFleet}

// RunProfile is a profile's defaults. Corpus is the fixture profile its
// default --dir holds.
type RunProfile struct {
	Name    string
	Corpus  string
	Repeats int
	Warmups int
	// Encodes are the encode rows' settings, over every source.
	Encodes []EncodeSetting
	// LargeRAR adds the RAR archives too large for a smoke run.
	LargeRAR bool
	// TextMemorySweep keeps the memory sweep (models above 16 MiB) for the
	// text payload only: the other payloads' large-model rows cost the most
	// and say the least.
	TextMemorySweep bool
}

// ProfileByName returns a run profile.
func ProfileByName(name string) (RunProfile, error) {
	full := []EncodeSetting{{2, 16 << 20}, {6, 16 << 20}, {8, 256 << 20}, {16, 256 << 20}, {32, 256 << 20}}
	switch name {
	case ProfileQuick:
		return RunProfile{Name: name, Corpus: "quick", Repeats: 1, Warmups: 0, Encodes: []EncodeSetting{{6, 16 << 20}}}, nil
	case ProfileFull:
		return RunProfile{Name: name, Corpus: "full", Repeats: 5, Warmups: 1, Encodes: full, LargeRAR: true}, nil
	case ProfileFleet:
		return RunProfile{Name: name, Corpus: "full", Repeats: 3, Warmups: 1, Encodes: full, LargeRAR: true, TextMemorySweep: true}, nil
	}
	return RunProfile{}, fmt.Errorf("unknown profile %q (want %s)", name, strings.Join(Profiles, ", "))
}

// Processes is how many processes a planned matrix launches.
func Processes(scenarios []Scenario, repeats, warmups int) int {
	n := 0
	for _, scenario := range scenarios {
		n += len(scenario.Variants) * (repeats + warmups)
	}
	return n
}

// The projection's cost model. PPMd's time follows the coded bits, so a
// decode or encode costs about its compressed size over a symbol-coding
// rate, plus a pass over the plain bytes. The rates are the slowest
// ppmd-rust and 7zz showed on the full corpus on an Apple M5 Max (random
// and mixed payloads at order 16 over a 256 MiB model, text at order 32);
// low orders and small models run up to ten times faster, so the model
// overstates most rows. X86Slowdown scales it to a fleet x86 host. A
// ppmd-turbo row is costed like ppmd-rust, which only overstates it.
const (
	codedBytesPerSecond = 1.6e6
	plainBytesPerSecond = 100e6
	processSeconds      = 0.01
	// X86Slowdown is the single-thread gap assumed between the M5 Max and
	// a fleet x86 host.
	X86Slowdown = 1.6
	// encodeRatio is the compressed fraction assumed for an encode whose
	// output size is not known before it runs.
	encodeRatio = 0.5
)

// Projected is the planned matrix's expected seconds on a fleet x86 host.
func Projected(scenarios []Scenario, repeats, warmups int) float64 {
	total := 0.0
	for _, scenario := range scenarios {
		coded := float64(scenario.BytesIn)
		plain := float64(scenario.BytesOut)
		if scenario.Op == OpEncode7z {
			coded, plain = float64(scenario.ExpectedOut), float64(scenario.BytesIn)
			if scenario.ExpectedOut == 0 {
				coded = float64(scenario.BytesIn) * encodeRatio
			}
		}
		one := coded/codedBytesPerSecond + plain/plainBytesPerSecond + processSeconds
		total += one * X86Slowdown * float64(len(scenario.Variants)*(repeats+warmups))
	}
	return total
}

// Plan builds the matrix over the corpus in dir. scratch is where encode
// rows write their output.
func Plan(manifest *fixtures.Manifest, dir, scratch string, tools Tools, profile RunProfile) ([]Scenario, error) {
	p := planner{dir: dir, scratch: scratch, tools: tools}
	for _, archive := range manifest.Archives {
		if profile.TextMemorySweep && archive.Kind != "text" && archive.Mem > 16<<20 {
			continue
		}
		p.decodeArchive(archive)
	}
	for _, raw := range manifest.Raw {
		p.decodeRaw(raw)
	}
	for _, file := range manifest.RAR {
		p.decodeRAR(file)
	}
	for _, source := range manifest.Sources {
		for _, setting := range profile.Encodes {
			p.encode(source, setting, expectedSize(manifest, source.Name, setting.Order))
		}
	}
	return p.scenarios, p.err
}

type planner struct {
	dir       string
	scratch   string
	tools     Tools
	scenarios []Scenario
	err       error
}

// MemLabel renders a memory size as the corpus names do: 16m, 1g, 64k.
func MemLabel(mem int64) string {
	switch {
	case mem >= 1<<30 && mem%(1<<30) == 0:
		return strconv.FormatInt(mem>>30, 10) + "g"
	case mem >= 1<<20 && mem%(1<<20) == 0:
		return strconv.FormatInt(mem>>20, 10) + "m"
	case mem >= 1<<10 && mem%(1<<10) == 0:
		return strconv.FormatInt(mem>>10, 10) + "k"
	}
	return strconv.FormatInt(mem, 10)
}

// ours is the candidate and contender runs of a ppmd-bench op.
func (p *planner) ours(op string, args []string, output func(variant string) string) []Run {
	var runs []Run
	for _, variant := range []struct{ name, role string }{{VariantTurbo, RoleCandidate}, {VariantPPMdRust, RoleContender}} {
		if variant.name == VariantTurbo && !p.tools.Turbo[op] {
			continue
		}
		full := append([]string{op, "--impl", variant.name}, args...)
		run := Run{Variant: variant.name, Role: variant.role, Tool: p.tools.Driver, JSON: true}
		if output != nil {
			run.Output = output(variant.name)
			full = append(full, "--out", run.Output)
		}
		run.Args = full
		runs = append(runs, run)
	}
	return runs
}

func (p *planner) decodeArchive(a fixtures.Archive) {
	path := filepath.Join(p.dir, a.File)
	group := "7z decode: other payloads"
	if a.Kind == "text" {
		group = "7z decode: text"
	}
	note := ""
	if a.Mem != a.RequestedMem {
		note = fmt.Sprintf("7zz wrote mem %s for the requested %s", MemLabel(a.Mem), MemLabel(a.RequestedMem))
	}
	variants := p.ours(OpDecode7z, []string{"--in", path}, nil)
	variants = append(variants, Run{Variant: VariantSevenZip, Role: RoleReference, Tool: p.tools.SevenZip,
		Args: []string{"t", "-bso0", "-bsp0", path}})
	p.scenarios = append(p.scenarios, Scenario{
		ID: "decode-7z/" + a.Name, Group: group, Op: OpDecode7z, Fixture: a.File, Kind: a.Kind, Order: a.Order, Mem: a.Mem,
		BytesIn: a.PackedLen, BytesOut: a.UnpackedLen, CRC32: a.PayloadCRC32, Note: note, Variants: variants,
	})
}

func (p *planner) decodeRaw(r fixtures.Raw) {
	path := filepath.Join(p.dir, r.File)
	args := []string{"--in", path, "--order", strconv.Itoa(r.Order), "--mem", strconv.FormatInt(r.Mem, 10),
		"--size", strconv.FormatInt(r.UnpackedLen, 10)}
	p.scenarios = append(p.scenarios, Scenario{
		ID: "decode-7z-raw/" + strings.TrimSuffix(r.Name, ".ppmd-rust"), Group: "7z decode: raw stream", Op: OpDecode7z,
		Fixture: r.File, Kind: r.Kind, Order: r.Order, Mem: r.Mem, BytesIn: r.PackedLen, BytesOut: r.UnpackedLen, CRC32: r.PayloadCRC32,
		Note:     "a raw stream at a memory size 7zz would shrink for this input, so no 7zz row; ppmd-rust is the reference when ppmd-turbo runs",
		Variants: p.ours(OpDecode7z, args, nil),
	})
}

func (p *planner) decodeRAR(file fixtures.RARFile) {
	variants := p.ours(OpDecodeRAR, []string{"--in", file.Path}, nil)
	sevenZipRole := RoleReference
	note := file.Note
	if p.tools.Unrar != "" {
		variants = append(variants, Run{Variant: VariantUnrar, Role: RoleReference, Tool: p.tools.Unrar,
			Args: []string{"t", "-idq", file.Path}})
		sevenZipRole = RoleSecondary
	}
	if p.tools.SevenZipRAR {
		variants = append(variants, Run{Variant: VariantSevenZip, Role: sevenZipRole, Tool: p.tools.SevenZip,
			Args: []string{"t", "-bso0", "-bsp0", file.Path}})
	}
	switch {
	case p.tools.Unrar == "" && p.tools.SevenZipRAR:
		note += "; no unrar on this host, so 7zz is the reference"
	case p.tools.Unrar == "":
		note += "; no unrar on this host and its 7zz has no RAR codecs, so no reference row"
	}
	p.scenarios = append(p.scenarios, Scenario{
		ID: "decode-rar/" + strings.TrimSuffix(file.Name, ".rar"), Group: "RAR decode", Op: OpDecodeRAR, Fixture: file.Name,
		BytesIn: file.Bytes, BytesOut: file.Unpacked, Note: note, Variants: variants,
	})
}

// expectedSize is the packed size of the corpus's largest-memory archive of
// payload at order, or 0.
func expectedSize(manifest *fixtures.Manifest, payload string, order int) int64 {
	var best fixtures.Archive
	for _, archive := range manifest.Archives {
		if archive.Payload == payload && archive.Order == order && archive.Mem >= best.Mem {
			best = archive
		}
	}
	return best.PackedLen
}

func (p *planner) encode(source fixtures.Source, setting EncodeSetting, expected int64) {
	stem := strings.TrimSuffix(strings.TrimPrefix(source.Name, "fixture-"), ".bin")
	label := fmt.Sprintf("%s.o%d.m%s", stem, setting.Order, MemLabel(setting.Mem))
	slug := fmt.Sprintf("%d-%s", len(p.scenarios), label)
	input := filepath.Join(p.dir, source.File)
	args := []string{"--in", input, "--order", strconv.Itoa(setting.Order), "--mem", strconv.FormatInt(setting.Mem, 10)}
	variants := p.ours(OpEncode7z, args, func(variant string) string {
		return filepath.Join(p.scratch, slug+"-"+variant+".ppmd")
	})
	out := filepath.Join(p.scratch, slug+"-7zz.7z")
	method := fmt.Sprintf("-m0=PPMd:o=%d:mem=%db", setting.Order, setting.Mem)
	variants = append(variants, Run{Variant: VariantSevenZip, Role: RoleReference, Tool: p.tools.SevenZip, Dir: p.dir, Output: out,
		Args: []string{"a", "-t7z", "-bso0", "-bsp0", "-y", "-mhc=off", "-mtm=off", "-mtc=off", "-mta=off", method, out, source.File}})
	p.scenarios = append(p.scenarios, Scenario{
		ID: "encode-7z/" + label, Group: "7z encode", Op: OpEncode7z, Fixture: source.Name, Kind: source.Kind,
		Order: setting.Order, Mem: setting.Mem, BytesIn: source.Size, CRC32: source.CRC32, ExpectedOut: expected,
		Note:     "ppmd-bench writes the raw stream, 7zz a one-file .7z around it (about 130 bytes of container); size ratio = 7zz / contender bytes",
		Variants: variants,
	})
}
