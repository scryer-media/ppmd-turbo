// Command ppmd-turbo-bench measures ppmd-turbo against ppmd-rust, 7-Zip
// (7zz) and, for RAR rows, RARLAB's unrar on one host, and merges several
// hosts' reports.
//
//	ppmd-turbo-bench fixtures  [--profile quick|full] [--dir D] [--sevenzip 7zz] [--corpus-tool B] [--only a,b]
//	ppmd-turbo-bench toolchain [--driver B] [--sevenzip 7zz] [--unrar U] [--repo R]
//	ppmd-turbo-bench run       [--profile quick|full|fleet] [--list] [--out DIR] [--dir D] [--machine LABEL]
//	                           [--driver B] [--sevenzip 7zz] [--unrar U] [--rar-corpus DIR]
//	                           [--repeats N] [--warmups N] [--only SUBSTR,...] [--pin-cpus 0-7] [--timeout 1h]
//	ppmd-turbo-bench report    --input raw.json --out report.json [--md report.md]
//	ppmd-turbo-bench merge     --out merged.md report.json...
//
// Exit status: 0 success; 1 a measured run failed (or missing RSS); 2 usage;
// 3 a prerequisite is missing (7zz, ppmd-bench, fixtures).
package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"runtime"
	"strings"
	"time"

	"github.com/scryer-media/ppmd-turbo/bench/ppmd-turbo-bench/internal/fixtures"
	"github.com/scryer-media/ppmd-turbo/bench/ppmd-turbo-bench/internal/host"
	"github.com/scryer-media/ppmd-turbo/bench/ppmd-turbo-bench/internal/report"
	"github.com/scryer-media/ppmd-turbo/bench/ppmd-turbo-bench/internal/suite"
	"github.com/scryer-media/ppmd-turbo/bench/ppmd-turbo-bench/internal/toolchain"
)

const (
	exitOK           = 0
	exitFailed       = 1
	exitUsage        = 2
	exitPrerequisite = 3
)

const usage = `usage: ppmd-turbo-bench <fixtures|toolchain|run|report|merge> [flags]
run "ppmd-turbo-bench <command> -h" for a command's flags`

// prerequisite marks an error that is a missing input, not a failed run.
type prerequisite struct{ error }

func main() {
	if len(os.Args) < 2 {
		fmt.Fprintln(os.Stderr, usage)
		os.Exit(exitUsage)
	}
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt)
	defer stop()
	var code int
	switch os.Args[1] {
	case "fixtures":
		code = cmdFixtures(ctx, os.Args[2:])
	case "toolchain":
		code = cmdToolchain(ctx, os.Args[2:])
	case "run":
		code = cmdRun(ctx, os.Args[2:])
	case "report":
		code = cmdReport(os.Args[2:])
	case "merge":
		code = cmdMerge(os.Args[2:])
	case "-h", "--help", "help":
		fmt.Println(usage)
	default:
		fmt.Fprintf(os.Stderr, "unknown command %q\n%s\n", os.Args[1], usage)
		code = exitUsage
	}
	stop()
	os.Exit(code)
}

func env(name, fallback string) string {
	if value := os.Getenv(name); value != "" {
		return value
	}
	return fallback
}

func fail(err error) int {
	fmt.Fprintln(os.Stderr, "ppmd-turbo-bench:", err)
	var missing prerequisite
	if errors.As(err, &missing) {
		return exitPrerequisite
	}
	return exitFailed
}

func parse(set *flag.FlagSet, args []string) bool {
	set.SetOutput(os.Stderr)
	return set.Parse(args) == nil
}

// repoRoot is --repo, else the git checkout around the working directory,
// else the working directory: the base of every default path.
func repoRoot(ctx context.Context, explicit string) string {
	if explicit != "" {
		return explicit
	}
	if output, err := exec.CommandContext(ctx, "git", "rev-parse", "--show-toplevel").Output(); err == nil {
		if top := strings.TrimSpace(string(output)); top != "" {
			return top
		}
	}
	dir, _ := os.Getwd()
	return dir
}

func defaultFixtureDir(root, corpus string) string {
	return env("PPMD_BENCH_FIXTURES", filepath.Join(root, "bench", "fixtures", corpus))
}

func findSevenZip(ctx context.Context, explicit string, allowP7zip bool) (toolchain.Reference, error) {
	path, err := toolchain.Find(explicit, "PPMD_BENCH_SEVENZIP", "7zz", "7zz.exe", "7z", "7za")
	if err != nil {
		return toolchain.Reference{}, prerequisite{err}
	}
	if path == "" {
		return toolchain.Reference{}, prerequisite{toolchain.ErrNoSevenZip}
	}
	reference, err := toolchain.ProbeSevenZip(ctx, path, allowP7zip)
	if err != nil {
		return toolchain.Reference{}, prerequisite{err}
	}
	return reference, nil
}

func cmdFixtures(ctx context.Context, args []string) int {
	set := flag.NewFlagSet("fixtures", flag.ContinueOnError)
	profile := set.String("profile", "full", "corpus profile: quick or full (fleet runs over full)")
	dir := set.String("dir", "", "corpus directory (default <repo>/bench/fixtures/<profile>, or $PPMD_BENCH_FIXTURES)")
	sevenZip := set.String("sevenzip", "", "7zz to write the .7z archives with (default $PPMD_BENCH_SEVENZIP, then PATH)")
	allowP7zip := set.Bool("allow-p7zip", false, "accept a p7zip build")
	tool := set.String("corpus-tool", env("PPMD_BENCH_CORPUS_TOOL", ""), "ppmd-corpus binary (default <repo>/target/release/ppmd-corpus)")
	repo := set.String("repo", env("PPMD_BENCH_REPO", ""), "ppmd-turbo checkout (default: the git checkout around the working directory)")
	only := set.String("only", "", "comma-separated substrings; generate only matching entries")
	if !parse(set, args) {
		return exitUsage
	}
	if *profile != "quick" && *profile != "full" {
		fmt.Fprintf(os.Stderr, "fixtures: unknown corpus profile %q (want quick or full)\n", *profile)
		return exitUsage
	}
	root := repoRoot(ctx, *repo)
	if *dir == "" {
		*dir = defaultFixtureDir(root, *profile)
	}
	if *tool == "" {
		*tool = toolchain.Release(root, "ppmd-corpus")
	}
	if _, err := os.Stat(*tool); err != nil {
		return fail(prerequisite{fmt.Errorf("%s: %w (cargo build --locked --release -p ppmd-corpus)", *tool, err)})
	}
	reference, err := findSevenZip(ctx, *sevenZip, *allowP7zip)
	if err != nil {
		return fail(err)
	}
	var names []string
	if *only != "" {
		names = strings.Split(*only, ",")
	}
	manifest, err := fixtures.Generate(ctx, fixtures.Options{Tool: *tool, Dir: *dir, Profile: *profile, SevenZip: reference.Path, Only: names, Log: os.Stderr})
	if err != nil {
		return fail(err)
	}
	fmt.Printf("%s: %d archives, %d raw streams, %d sources (7zz %s)\n", filepath.Join(*dir, fixtures.ManifestName),
		len(manifest.Archives), len(manifest.Raw), len(manifest.Sources), manifest.SevenZip.Version)
	return exitOK
}

type toolFlags struct {
	driver, sevenZip, unrar, repo *string
	allowP7zip                    *bool
}

func addToolFlags(set *flag.FlagSet) toolFlags {
	return toolFlags{
		driver:     set.String("driver", env("PPMD_BENCH_DRIVER", ""), "ppmd-bench binary (default <repo>/target/release/ppmd-bench)"),
		sevenZip:   set.String("sevenzip", "", "official 7zz (default $PPMD_BENCH_SEVENZIP, then 7zz/7z/7za on PATH)"),
		unrar:      set.String("unrar", "", "RARLAB unrar for the RAR rows' reference (default $PPMD_BENCH_UNRAR, then unrar on PATH; optional)"),
		repo:       set.String("repo", env("PPMD_BENCH_REPO", ""), "ppmd-turbo checkout, for default paths and rustc/commit provenance (default: the git checkout around the working directory)"),
		allowP7zip: set.Bool("allow-p7zip", false, "accept a p7zip build as 7zz"),
	}
}

func (f toolFlags) collect(ctx context.Context) (toolchain.Toolchain, suite.Tools, error) {
	root := repoRoot(ctx, *f.repo)
	chain := toolchain.Toolchain{Rust: toolchain.ProbeRust(ctx, root)}
	driverPath := *f.driver
	if driverPath == "" {
		driverPath = toolchain.Release(root, "ppmd-bench")
	}
	driver, err := toolchain.ProbeDriver(ctx, driverPath)
	if err != nil {
		return chain, suite.Tools{}, prerequisite{fmt.Errorf("%w (cargo build --locked --release -p ppmd-bench)", err)}
	}
	chain.Driver = driver
	tools := suite.Tools{Driver: driverPath, Turbo: map[string]bool{}}
	for _, op := range []string{suite.OpDecode7z, suite.OpDecodeRAR, suite.OpEncode7z} {
		tools.Turbo[op] = driver.Supports(suite.VariantTurbo, op)
	}
	sevenZip, err := findSevenZip(ctx, *f.sevenZip, *f.allowP7zip)
	if err != nil {
		return chain, suite.Tools{}, err
	}
	chain.SevenZip, tools.SevenZip, tools.SevenZipRAR = sevenZip, sevenZip.Path, sevenZip.DecodesRAR
	unrarPath, err := toolchain.Find(*f.unrar, "PPMD_BENCH_UNRAR", "unrar", "unrar.exe")
	if err != nil {
		return chain, suite.Tools{}, prerequisite{err}
	}
	if unrarPath != "" {
		unrar, err := toolchain.ProbeUnrar(ctx, unrarPath)
		if err != nil {
			return chain, suite.Tools{}, prerequisite{err}
		}
		chain.Unrar, tools.Unrar = &unrar, unrar.Path
	}
	return chain, tools, nil
}

func cmdToolchain(ctx context.Context, args []string) int {
	set := flag.NewFlagSet("toolchain", flag.ContinueOnError)
	tools := addToolFlags(set)
	if !parse(set, args) {
		return exitUsage
	}
	chain, _, err := tools.collect(ctx)
	if err != nil {
		return fail(err)
	}
	data, _ := json.MarshalIndent(struct {
		Machine   host.Machine        `json:"machine"`
		Toolchain toolchain.Toolchain `json:"toolchain"`
	}{host.Collect(ctx, env("PPMD_BENCH_MACHINE", defaultLabel())), chain}, "", "  ")
	fmt.Println(string(data))
	return exitOK
}

// defaultLabel names the host generically, by OS and architecture. A report
// never carries the machine's own hostname; pass -machine or
// PPMD_BENCH_MACHINE for a more specific label such as an instance type.
func defaultLabel() string {
	return runtime.GOOS + "-" + runtime.GOARCH
}

func filterScenarios(scenarios []suite.Scenario, only string) []suite.Scenario {
	if only == "" {
		return scenarios
	}
	var kept []suite.Scenario
	for _, scenario := range scenarios {
		for _, part := range strings.Split(only, ",") {
			if part != "" && strings.Contains(scenario.ID, part) {
				kept = append(kept, scenario)
				break
			}
		}
	}
	return kept
}

func minutes(seconds float64) string {
	return fmt.Sprintf("%.1f min", seconds/60)
}

func cmdRun(ctx context.Context, args []string) int {
	set := flag.NewFlagSet("run", flag.ContinueOnError)
	tools := addToolFlags(set)
	profileName := set.String("profile", suite.ProfileFull, "run profile: quick (the quick corpus, 1 repeat), full (the full corpus, 5 repeats + 1 warmup) or fleet (the full corpus, 3 repeats + 1 warmup)")
	list := set.Bool("list", false, "print the planned scenarios, their row and process counts and the projected duration, then exit")
	machine := set.String("machine", env("PPMD_BENCH_MACHINE", defaultLabel()), "host label in the report and the default results directory (e.g. c7i.4xlarge-us-east-1)")
	out := set.String("out", "", "results directory (default <repo>/bench/results/<machine>-<profile>)")
	dir := set.String("dir", "", "corpus directory (default <repo>/bench/fixtures/<the profile's corpus>, or $PPMD_BENCH_FIXTURES)")
	rarCorpus := set.String("rar-corpus", env("PPMD_BENCH_RAR_CORPUS", ""), "directory holding rarpar's RARLAB-written PPMd archives (rar4_ppm_*.rar); RAR rows are skipped without it")
	repeats := set.Int("repeats", -1, "measured runs per variant (default: the profile's)")
	warmups := set.Int("warmups", -1, "discarded runs per variant before the measured ones (default: the profile's)")
	only := set.String("only", "", "comma-separated substrings; run only scenarios whose id contains one")
	pin := set.String("pin-cpus", env("PPMD_BENCH_PIN_CPUS", ""), "inclusive CPU range to confine every process to (Linux taskset, Windows affinity)")
	timeout := set.Duration("timeout", time.Hour, "per-process bound; a run past it is recorded as DNF")
	if !parse(set, args) {
		return exitUsage
	}
	profile, err := suite.ProfileByName(*profileName)
	if err != nil {
		fmt.Fprintf(os.Stderr, "run: %v\n", err)
		return exitUsage
	}
	if *repeats < 0 {
		*repeats = profile.Repeats
	}
	if *warmups < 0 {
		*warmups = profile.Warmups
	}
	if *repeats < 1 {
		fmt.Fprintln(os.Stderr, "run: --repeats must be at least 1")
		return exitUsage
	}
	root := repoRoot(ctx, *tools.repo)
	if *dir == "" {
		*dir = defaultFixtureDir(root, profile.Corpus)
	}
	if *out == "" {
		*out = filepath.Join(root, "bench", "results", *machine+"-"+profile.Name)
	}
	manifest, err := fixtures.Load(*dir)
	if err != nil {
		return fail(prerequisite{fmt.Errorf("fixtures in %s: %w (run `ppmd-turbo-bench fixtures --profile %s` first)", *dir, err, profile.Corpus)})
	}
	if manifest.Profile != profile.Corpus {
		fmt.Fprintf(os.Stderr, "run: note: profile %s over the %s corpus in %s\n", profile.Name, manifest.Profile, *dir)
	}
	rar, err := fixtures.FindRAR(*rarCorpus, profile.LargeRAR)
	switch {
	case errors.Is(err, fixtures.ErrNoRARCorpus):
		fmt.Fprintln(os.Stderr, "run: note: no --rar-corpus, so no RAR rows (see docs/benchmarking.md)")
	case err != nil:
		return fail(prerequisite{err})
	}
	manifest.RAR = rar
	chain, binaries, err := tools.collect(ctx)
	if err != nil {
		return fail(err)
	}
	absolute, err := filepath.Abs(*dir)
	if err != nil {
		return fail(err)
	}
	// --list plans against a scratch path it never creates.
	scratch := filepath.Join(os.TempDir(), "ppmd-turbo-bench-list-scratch")
	if !*list {
		scratch = filepath.Join(*out, "scratch")
		if err := os.MkdirAll(scratch, 0o755); err != nil {
			return fail(err)
		}
	}
	scratch, _ = filepath.Abs(scratch)
	scenarios, err := suite.Plan(manifest, absolute, scratch, binaries, profile)
	if err != nil {
		return fail(prerequisite{err})
	}
	scenarios = filterScenarios(scenarios, *only)
	rows := 0
	for _, scenario := range scenarios {
		rows += len(scenario.Variants)
	}
	projected := suite.Projected(scenarios, *repeats, *warmups)
	planLine := fmt.Sprintf("profile %s over the %s corpus: %d scenarios, %d rows, %d processes at %d repeat(s) + %d warmup(s), projected %s on a fleet x86 host",
		profile.Name, manifest.Profile, len(scenarios), rows, suite.Processes(scenarios, *repeats, *warmups), *repeats, *warmups, minutes(projected))
	if *list {
		for _, scenario := range scenarios {
			var names []string
			for _, run := range scenario.Variants {
				names = append(names, run.Variant)
			}
			fmt.Printf("%-40s %s\n", scenario.ID, strings.Join(names, " "))
		}
		fmt.Println(planLine)
		withTurbo := binaries
		withTurbo.Turbo = map[string]bool{suite.OpDecode7z: true, suite.OpDecodeRAR: true, suite.OpEncode7z: true}
		if all, err := suite.Plan(manifest, absolute, scratch, withTurbo, profile); err == nil {
			all = filterScenarios(all, *only)
			fmt.Printf("with ppmd-turbo rows for every op: %d processes, projected %s\n",
				suite.Processes(all, *repeats, *warmups), minutes(suite.Projected(all, *repeats, *warmups)))
		}
		return exitOK
	}
	fmt.Fprintf(os.Stderr, "run: %s\n", planLine)
	raw := &suite.Raw{
		SchemaVersion: 1, Schema: suite.RawSchema, StartedUTC: time.Now().UTC().Format(time.RFC3339),
		Machine: host.Collect(ctx, *machine), Toolchain: chain, Fixtures: manifest, RunProfile: profile.Name,
		Warmups: *warmups, Repeats: *repeats, PinCPUs: *pin, TimeoutSeconds: timeout.Seconds(),
		Scenarios: scenarios, Runs: []suite.RunRecord{},
	}
	suite.Execute(ctx, raw, suite.Options{Warmups: *warmups, Repeats: *repeats, PinCPUs: *pin, Timeout: *timeout, Log: os.Stderr, Driver: binaries.Driver})
	raw.FinishedUTC = time.Now().UTC().Format(time.RFC3339)
	_ = os.RemoveAll(scratch)
	if err := suite.Write(filepath.Join(*out, "raw.json"), raw); err != nil {
		return fail(err)
	}
	built := report.Build(raw)
	if err := writeReport(built, filepath.Join(*out, "report.json"), filepath.Join(*out, "report.md")); err != nil {
		return fail(err)
	}
	fmt.Printf("wrote %s, %s, %s\n", filepath.Join(*out, "raw.json"), filepath.Join(*out, "report.json"), filepath.Join(*out, "report.md"))
	if ctx.Err() != nil {
		fmt.Fprintln(os.Stderr, "run: interrupted")
		return exitFailed
	}
	if len(built.Failures) > 0 {
		fmt.Fprintf(os.Stderr, "run: %d failed runs (see report.md)\n", len(built.Failures))
		return exitFailed
	}
	return exitOK
}

func writeReport(built *report.Report, jsonPath, mdPath string) error {
	if err := report.Write(jsonPath, built); err != nil {
		return err
	}
	return os.WriteFile(mdPath, []byte(report.Markdown(built)), 0o644)
}

func cmdReport(args []string) int {
	set := flag.NewFlagSet("report", flag.ContinueOnError)
	input := set.String("input", "", "raw.json from run")
	out := set.String("out", "", "report.json to write")
	md := set.String("md", "", "report.md to write (default: next to --out)")
	if !parse(set, args) {
		return exitUsage
	}
	if *input == "" || *out == "" {
		fmt.Fprintln(os.Stderr, "report: --input and --out are required")
		return exitUsage
	}
	raw, err := suite.Load(*input)
	if err != nil {
		return fail(prerequisite{err})
	}
	if *md == "" {
		*md = strings.TrimSuffix(*out, filepath.Ext(*out)) + ".md"
	}
	built := report.Build(raw)
	if err := writeReport(built, *out, *md); err != nil {
		return fail(err)
	}
	fmt.Printf("wrote %s, %s\n", *out, *md)
	return exitOK
}

func cmdMerge(args []string) int {
	set := flag.NewFlagSet("merge", flag.ContinueOnError)
	out := set.String("out", "", "merged report.md to write")
	if !parse(set, args) {
		return exitUsage
	}
	if *out == "" || set.NArg() == 0 {
		fmt.Fprintln(os.Stderr, "merge: --out and at least one report.json are required")
		return exitUsage
	}
	var reports []*report.Report
	for _, path := range set.Args() {
		loaded, err := report.Load(path)
		if err != nil {
			return fail(prerequisite{err})
		}
		reports = append(reports, loaded)
	}
	if err := os.WriteFile(*out, []byte(report.Merge(reports)), 0o644); err != nil {
		return fail(err)
	}
	fmt.Printf("wrote %s (%d hosts)\n", *out, len(reports))
	return exitOK
}
