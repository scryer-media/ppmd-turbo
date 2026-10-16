#!/usr/bin/env python3
"""Turn the live Nextest selection under cfg(miri) into one Miri job per test.

Usage: miri-matrix.py LISTING.json GITHUB_OUTPUT

LISTING.json is `cargo miri nextest list --message-format json` with the
selection filter applied. Each test the filter matches and Miri does not
ignore becomes one matrix entry, keyed by binary id and test name, because
the integration-test binaries can share test names with each other.
"""

import json
import sys
from pathlib import Path

# The binaries the Miri lane may schedule. A test binary that starts
# compiling tests under cfg(miri) is not picked up until it is added here
# and to the ci.yml filter, so the lane's reach is a decision, not a drift.
ALLOWED_BINARIES = {
    "ppmd-turbo",
    "ppmd-turbo::coder_differential",
    "ppmd-turbo::encode_carryless",
}

# Every module that holds `unsafe` must keep tests in the selection: the
# arena's raw parts (alloc), the fallible allocation (arena), and the model
# that addresses the arena without bounds checks in release builds.
REQUIRED_LIB_PREFIXES = ("alloc::tests::", "arena::tests::", "model::tests::")

# GitHub's limit on the jobs one matrix may expand to.
MAX_JOBS = 256

listing = json.loads(Path(sys.argv[1]).read_text())
entries = [
    {"binary": binary, "test": name}
    for binary, suite in listing["rust-suites"].items()
    for name, case in suite["testcases"].items()
    if case["filter-match"]["status"] == "matches" and not case["ignored"]
]
keys = [(entry["binary"], entry["test"]) for entry in entries]
lib_tests = {entry["test"] for entry in entries if entry["binary"] == "ppmd-turbo"}

problems = []
if not entries:
    problems.append("the selection is empty")
if len(keys) != len(set(keys)):
    problems.append("the selection has duplicate tests")
if len(entries) > MAX_JOBS:
    problems.append(f"{len(entries)} tests exceed the {MAX_JOBS}-job matrix limit")
unexpected = sorted({entry["binary"] for entry in entries} - ALLOWED_BINARIES)
if unexpected:
    problems.append(f"unexpected binaries selected: {unexpected}")
for prefix in REQUIRED_LIB_PREFIXES:
    if not any(name.startswith(prefix) for name in lib_tests):
        problems.append(f"no library test under {prefix}")
if problems:
    raise SystemExit("Miri discovery failed: " + "; ".join(problems))

entries.sort(key=lambda entry: (entry["binary"], entry["test"]))
with Path(sys.argv[2]).open("a") as output:
    output.write("matrix=" + json.dumps({"include": entries}, separators=(",", ":")) + "\n")
print(f"Scheduled {len(entries)} separate Miri jobs")
