package report

import (
	"fmt"
	"sort"
	"strings"

	"github.com/scryer-media/ppmd-turbo/bench/ppmd-turbo-bench/internal/procmeasure"
	"github.com/scryer-media/ppmd-turbo/bench/ppmd-turbo-bench/internal/suite"
)

func seconds(s Stat) string {
	if s.N == 0 {
		return "-"
	}
	if s.N == 1 {
		return fmt.Sprintf("%.3f", s.Median)
	}
	return fmt.Sprintf("%.3f [%.3f–%.3f]", s.Median, s.Min, s.Max)
}

func ratioText(value *float64) string {
	if value == nil {
		return "-"
	}
	return fmt.Sprintf("%.3f", *value)
}

func rssText(s Stat) string {
	if s.N == 1 {
		return procmeasure.MiB(int64(s.Median))
	}
	return procmeasure.MiBRange(int64(s.Median), int64(s.Min), int64(s.Max))
}

// Markdown renders one host's report.md.
func Markdown(report *Report) string {
	var b strings.Builder
	m := report.Machine
	fmt.Fprintf(&b, "# ppmd-turbo bench: %s\n\n", m.Label)
	fmt.Fprintf(&b, "%s\n\n", Orientation)
	fmt.Fprintf(&b, "Each cell is the median [min–max] over %d measured run(s) (%d warmup(s) discarded); variants are interleaved, their order reversed every repeat. Every run is its own process: wall time from the harness's clock, CPU (user+sys) and peak RSS from the kernel's accounting of the exited child.\n\n", report.Repeats, report.Warmups)
	fmt.Fprintln(&b, "## Host")
	fmt.Fprintln(&b)
	fmt.Fprintf(&b, "- label: %s\n", m.Label)
	if m.InstanceType != "" {
		fmt.Fprintf(&b, "- instance type: %s\n", m.InstanceType)
	}
	fmt.Fprintf(&b, "- OS / arch: %s / %s (%s)\n", m.OS, m.Architecture, m.Kernel)
	fmt.Fprintf(&b, "- CPU: %s, %d logical cores\n", m.CPU, m.CPUCount)
	if m.MemoryBytes > 0 {
		fmt.Fprintf(&b, "- memory: %.1f GiB\n", float64(m.MemoryBytes)/(1<<30))
	}
	fmt.Fprintf(&b, "- run: %s to %s, run profile %s over the %s corpus, %d repeat(s), %d warmup(s)\n\n", report.StartedUTC,
		report.FinishedUTC, report.RunProfile, report.Corpus, report.Repeats, report.Warmups)
	t := report.Toolchain
	fmt.Fprintln(&b, "## Toolchain")
	fmt.Fprintln(&b)
	fmt.Fprintf(&b, "- ppmd-bench %v (sha256 %s); ppmd-turbo provides: %s\n", t.Driver.Info["version"], short(t.Driver.SHA256), turboOps(report))
	if t.Rust.LockedRust != "" {
		fmt.Fprintf(&b, "- ppmd-rust %s (Cargo.lock)\n", t.Rust.LockedRust)
	}
	fmt.Fprintf(&b, "- 7zz: %s (sha256 %s); provenance: %s; RAR codecs: %t\n", t.SevenZip.Banner, short(t.SevenZip.SHA256), t.SevenZip.Provenance, t.SevenZip.DecodesRAR)
	if t.Unrar != nil {
		fmt.Fprintf(&b, "- unrar: %s (sha256 %s); provenance: %s\n", t.Unrar.Banner, short(t.Unrar.SHA256), t.Unrar.Provenance)
	} else {
		fmt.Fprintln(&b, "- unrar: not on this host")
	}
	dirty := ""
	if t.Rust.Dirty {
		dirty = " (dirty)"
	}
	fmt.Fprintf(&b, "- rustc: %s; commit %s%s\n\n", t.Rust.Rustc, short(t.Rust.Commit), dirty)

	if len(report.Failures) > 0 {
		fmt.Fprintln(&b, "## Failures")
		fmt.Fprintln(&b)
		for _, failure := range report.Failures {
			fmt.Fprintf(&b, "- %s\n", failure)
		}
		fmt.Fprintln(&b)
	}

	ratios := map[string]map[string]Ratio{}
	for _, ratio := range report.Ratios {
		if ratios[ratio.Scenario] == nil {
			ratios[ratio.Scenario] = map[string]Ratio{}
		}
		ratios[ratio.Scenario][ratio.Variant] = ratio
	}
	notes := map[string]string{}
	for _, scenario := range report.Scenarios {
		notes[scenario.ID] = scenario.Note
	}
	for _, group := range suite.Groups {
		var rows []Row
		for _, row := range report.Rows {
			if row.Group == group {
				rows = append(rows, row)
			}
		}
		if len(rows) == 0 {
			continue
		}
		fmt.Fprintf(&b, "## %s\n\n", group)
		encode := rows[0].Op == suite.OpEncode7z
		header := "| scenario | variant | wall s | CPU s | peak RSS MiB | MiB/s |"
		rule := "|---|---|---|---|---|---|"
		if encode {
			header += " output bytes | size ratio |"
			rule += "---|---|"
		}
		fmt.Fprintln(&b, header+" wall ratio | CPU ratio | RSS ratio | load | notes |")
		fmt.Fprintln(&b, rule+"---|---|---|---|---|")
		var groupNotes []string
		lastScenario := ""
		for _, row := range rows {
			ratio, hasRatio := ratios[row.Scenario][row.Variant]
			name := ""
			if row.Scenario != lastScenario {
				name = row.Scenario
				lastScenario = row.Scenario
				if note := notes[row.Scenario]; note != "" {
					groupNotes = append(groupNotes, fmt.Sprintf("`%s`: %s", row.Scenario, note))
				}
			}
			throughput := "-"
			if row.ThroughputMiBs > 0 {
				throughput = fmt.Sprintf("%.1f", row.ThroughputMiBs)
			}
			line := fmt.Sprintf("| %s | %s | %s | %s | %s | %s |", name, variantLabel(row), seconds(row.Wall), seconds(row.CPU), rssText(row.RSS), throughput)
			if encode {
				size := "-"
				if hasRatio {
					size = ratioText(ratio.Size)
				}
				line += fmt.Sprintf(" %d | %s |", row.BytesOut, size)
			}
			wall, cpu, rss := "-", "-", "-"
			if hasRatio {
				wall, cpu, rss = ratioText(ratio.Wall), ratioText(ratio.CPU), ratioText(ratio.RSS)
			}
			load := "-"
			if row.Load.N > 0 {
				load = fmt.Sprintf("%.2f", row.Load.Median)
			}
			extra := row.Extra
			if row.Failed > 0 {
				extra = strings.TrimSpace(fmt.Sprintf("%s FAILED %d/%d: %s", extra, row.Failed, row.Failed+row.OK, strings.Join(dedupe(row.Failures), ",")))
			}
			line += fmt.Sprintf(" %s | %s | %s | %s | %s |", wall, cpu, rss, load, dash(extra))
			fmt.Fprintln(&b, line)
		}
		fmt.Fprintln(&b)
		for _, note := range groupNotes {
			fmt.Fprintf(&b, "- %s\n", note)
		}
		if len(groupNotes) > 0 {
			fmt.Fprintln(&b)
		}
	}
	procmeasure.RenderRSSSummary(&b, report.RSS)
	if len(report.SecondaryFailures) > 0 {
		fmt.Fprintln(&b, "## Secondary failures")
		fmt.Fprintln(&b)
		fmt.Fprintln(&b, "Runs that are informational and do not fail the run.")
		fmt.Fprintln(&b)
		for _, failure := range dedupe(report.SecondaryFailures) {
			fmt.Fprintf(&b, "- %s\n", failure)
		}
		fmt.Fprintln(&b)
	}
	return b.String()
}

func turboOps(report *Report) string {
	var ops []string
	for _, op := range []string{suite.OpDecode7z, suite.OpDecodeRAR, suite.OpEncode7z} {
		if report.Toolchain.Driver.Supports(suite.VariantTurbo, op) {
			ops = append(ops, op)
		}
	}
	if len(ops) == 0 {
		return "nothing yet (no ppmd-turbo rows)"
	}
	return strings.Join(ops, ", ")
}

func variantLabel(row Row) string {
	switch row.Role {
	case suite.RoleReference:
		return row.Variant + " (reference)"
	case suite.RoleSecondary:
		return row.Variant + " (secondary)"
	}
	return row.Variant
}

func dedupe(values []string) []string {
	seen := map[string]bool{}
	var out []string
	for _, value := range values {
		if !seen[value] {
			seen[value] = true
			out = append(out, value)
		}
	}
	return out
}

func dash(value string) string {
	if value == "" {
		return "-"
	}
	return value
}

func short(digest string) string {
	if len(digest) > 12 {
		return digest[:12]
	}
	return digest
}

// Merge renders a cross-host report.md from several hosts' reports: per
// scenario and contender, each host's median wall time and its wall and RSS
// ratios against that host's reference, plus encode size ratios.
func Merge(reports []*Report) string {
	var b strings.Builder
	fmt.Fprintln(&b, "# ppmd-turbo bench: cross-host summary")
	fmt.Fprintln(&b)
	fmt.Fprintln(&b, Orientation)
	fmt.Fprintln(&b, "Each host is compared with its own reference; cells are `median wall s / wall ratio / RSS ratio` (encode rows add `/ size ratio`).")
	fmt.Fprintln(&b)
	fmt.Fprintln(&b, "## Hosts")
	fmt.Fprintln(&b)
	fmt.Fprintln(&b, "| label | instance | OS/arch | CPU | cores | 7zz | unrar | profile | failures |")
	fmt.Fprintln(&b, "|---|---|---|---|---|---|---|---|---|")
	for _, r := range reports {
		m := r.Machine
		unrar := "-"
		if r.Toolchain.Unrar != nil {
			unrar = r.Toolchain.Unrar.Version
		}
		fmt.Fprintf(&b, "| %s | %s | %s/%s | %s | %d | %s | %s | %s | %d |\n", m.Label, dash(m.InstanceType), m.OS, m.Architecture, m.CPU,
			m.CPUCount, r.Toolchain.SevenZip.Version, unrar, r.RunProfile, len(r.Failures))
	}
	fmt.Fprintln(&b)

	type cellKey struct {
		host              int
		scenario, variant string
	}
	cells := map[cellKey]string{}
	groupOf := map[string]string{}
	var order []string
	seen := map[string]bool{}
	for index, r := range reports {
		walls := map[string]Stat{}
		for _, row := range r.Rows {
			walls[row.Scenario+"\x00"+row.Variant] = row.Wall
		}
		for _, ratio := range r.Ratios {
			key := ratio.Scenario + "\x00" + ratio.Variant
			if !seen[key] {
				seen[key] = true
				order = append(order, key)
				groupOf[key] = ratio.Group
			}
			cell := fmt.Sprintf("%.3f / %s / %s", walls[key].Median, ratioText(ratio.Wall), ratioText(ratio.RSS))
			if ratio.Size != nil {
				cell += " / " + ratioText(ratio.Size)
			}
			cells[cellKey{index, ratio.Scenario, ratio.Variant}] = cell
		}
	}
	groupIndex := map[string]int{}
	for i, group := range suite.Groups {
		groupIndex[group] = i
	}
	sort.SliceStable(order, func(i, j int) bool { return groupIndex[groupOf[order[i]]] < groupIndex[groupOf[order[j]]] })
	currentGroup := ""
	for i, key := range order {
		group := groupOf[key]
		if group != currentGroup {
			currentGroup = group
			fmt.Fprintf(&b, "## %s\n\n", group)
			header, rule := "| scenario | variant |", "|---|---|"
			for _, r := range reports {
				header += " " + r.Machine.Label + " |"
				rule += "---|"
			}
			fmt.Fprintln(&b, header)
			fmt.Fprintln(&b, rule)
		}
		scenario, variant, _ := strings.Cut(key, "\x00")
		line := fmt.Sprintf("| %s | %s |", scenario, variant)
		for index := range reports {
			line += " " + dash(cells[cellKey{index, scenario, variant}]) + " |"
		}
		fmt.Fprintln(&b, line)
		if i+1 == len(order) || groupOf[order[i+1]] != group {
			fmt.Fprintln(&b)
		}
	}
	for _, r := range reports {
		if len(r.Failures) > 0 {
			fmt.Fprintf(&b, "## Failures on %s\n\n", r.Machine.Label)
			for _, failure := range r.Failures {
				fmt.Fprintf(&b, "- %s\n", failure)
			}
			fmt.Fprintln(&b)
		}
	}
	return b.String()
}
