#!/usr/bin/env python3
"""Paired pilot using a private broker, Store server, and 16 Store DSL workers."""
import argparse
from collections import Counter
from datetime import datetime
import importlib.util
import json
import math
import os
from pathlib import Path
import subprocess
import tempfile
import time

from isolated_store import isolated_store

spec = importlib.util.spec_from_file_location("capture_fixture", Path(__file__).with_name("test-nightly-capture.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


def ack_stats(records):
    outcomes = Counter(row.get("transport", "unmeasured") for row in records)
    timings = sorted(row["latency_us"] for row in records if row.get("transport") == "acknowledged")
    return {"outcomes": dict(outcomes), "p99_acknowledged_ms":
            timings[math.ceil(.99 * len(timings)) - 1] / 1000 if timings else None}


def process_stats(pid):
    rss, cpu = subprocess.check_output(
        ["ps", "-o", "rss=", "-o", "time=", "-p", str(pid)], text=True
    ).split()
    # BSD ps uses [[hours:]minutes:]seconds.hundredths for cumulative process time.
    seconds = 0.0
    for part in cpu.split(":"):
        seconds = seconds * 60 + float(part)
    return int(rss), seconds


class IngestRates:
    def __init__(self, initial_status):
        self.first = self.previous = self.sample(initial_status)
        self.peak = {"logs": None, "spans": None}
        self.intervals = 0

    @staticmethod
    def sample(status):
        return (time.monotonic(), {"logs": status["store"]["total_received"],
                                   "spans": status.get("spans_received")})

    def observe(self, status):
        current = self.sample(status)
        elapsed = current[0] - self.previous[0]
        for kind, value in current[1].items():
            previous = self.previous[1][kind]
            if value is not None and previous is not None and elapsed > 0:
                rate = (value - previous) / elapsed
                self.peak[kind] = max(self.peak[kind] or 0, rate)
        self.previous = current
        self.intervals += 1

    def report(self):
        elapsed = self.previous[0] - self.first[0]
        result = {"intervals": self.intervals, "window_s": elapsed}
        for kind, value in self.previous[1].items():
            first = self.first[1][kind]
            count = None if value is None or first is None else value - first
            result[kind] = {"received": count, "mean_per_s": None if count is None else count / elapsed,
                            "peak_interval_per_s": self.peak[kind]}
        return result


def summary_stats(journal):
    counts = Counter()
    for line in journal.read_text().splitlines():
        row = json.loads(line)
        if row["event"] != "summary-final":
            continue
        counts[row["summary_status"]] += 1
        if summary := row.get("summary"):
            counts["truncated_attribute_sets"] += sum(
                bool(v.get("truncated")) for v in summary["distinct_values"].values())
            counts["executions_over_span_name_limit"] += bool(summary["span_overflow"]["count"])
            counts["omitted_attribute_keys"] += summary["omitted_attribute_keys"]
    return dict(counts)


def timed_arm(bins, source, root, files, label, percent, expected_failures):
    root.mkdir()
    run_id = label
    events = root / "runner.jsonl"
    journal = root / "test-log.jsonl"
    with fixture.broker(bins / "logmon-broker", root / "broker") as (env, gelf, otlp, daemon):
        frontend = bins / "logmon-mcp"
        def control(op, **params):
            return fixture.cli(frontend, env, "test-log", op, run_id=run_id, **params)
        with isolated_store(source, root / "store", env, gelf, otlp, fixture.port) as (server_env, url):
            cwd = root / "store"
            if not files:
                path = cwd / "pilot.tests"
                path.write_text("".join('test "pilot_%d" tags "fast smoke harness" expect_errors {\n'
                    '    run script { store.query("/"); 42 } { assert result == 42 }\n}\n' % i for i in range(32)))
                files = [path]
            control("begin", jsonl_path=journal, grace_period_ms=1000, collect_distinct=["kernel.entry_point"],
                    summary_sample_percent=percent)
            rates = IngestRates(fixture.cli(frontend, env, "status", "get"))
            rss, cpu_before = process_stats(daemon.pid)
            load_at_start = os.getloadavg()
            with (root/"store-test.log").open("w") as log:
                start = time.monotonic()
                proc = subprocess.Popen([source/"target/debug/store-test", "--server", url, "--no-auto-rebake",
                        "--allow-stale", "--no-template", "--max-parallel-tests", "16", *files],
                        env=dict(server_env, STORE_TEST_LOG_RUN_ID=run_id, STORE_TEST_LOG_EVENTS_PATH=str(events)),
                        stdout=log, stderr=subprocess.STDOUT, cwd=cwd)
                next_rate = start + 5
                try:
                    while proc.poll() is None:
                        live_rss, _ = process_stats(daemon.pid)
                        rss = max(rss, live_rss)
                        if time.monotonic() >= next_rate:
                            rates.observe(fixture.cli(frontend, env, "status", "get"))
                            next_rate = time.monotonic() + 5
                        try:
                            proc.wait(timeout=.25)
                        except subprocess.TimeoutExpired:
                            if time.monotonic() - start > 1200:
                                raise RuntimeError("pilot batch exceeded 20 minutes")
                finally:
                    if proc.poll() is None:
                        proc.kill()
                        proc.wait()
                wall = time.monotonic() - start
            if proc.returncode not in (0, 1):
                raise RuntimeError((root/"store-test.log").read_text()[-2500:])
            rates.observe(fixture.cli(frontend, env, "status", "get"))
            _, cpu_after = process_stats(daemon.pid)
            begin_close = time.monotonic()
            close = control("close", wait_timeout_ms=20000)
            close_seconds = time.monotonic() - begin_close
            if close.get("finalization") != "settled" or close.get("persistence") != "complete":
                raise RuntimeError(str(close))
            rows = [json.loads(line) for line in events.read_text().splitlines()]
            end = [row for row in rows if "event" in row]
            starts = [row for row in rows if "start" in row]
            failures = [row for row in end if row["event"]["verdict"] != "OK"]
            if not end or len(failures) != expected_failures or bool(proc.returncode) != bool(failures):
                raise RuntimeError(f"unexpected Store result: exit={proc.returncode}, completed={len(end)}, "
                                   f"failures={len(failures)}, expected={expected_failures}")
            interval = []
            for row in end:
                event = row["event"]
                interval.extend([(datetime.fromisoformat(event["started_at"].replace("Z","+00:00")),1),
                                 (datetime.fromisoformat(event["ended_at"].replace("Z","+00:00")),-1)])
            active = peak = 0
            for _, delta in sorted(interval):
                active += delta
                peak = max(peak, active)
            status = fixture.cli(frontend, env, "status", "get")
            return dict(run=label, percent=percent, wall_s=wall, close_s=close_seconds, completed=len(end),
                        peak_chains=peak, peak_rss_kib=rss, journal_bytes=journal.stat().st_size,
                        start=ack_stats(starts), end=ack_stats(end),
                        daemon_cpu_s=cpu_after-cpu_before, loadavg_start=load_at_start,
                        loadavg_end=os.getloadavg(), ingestion=rates.report(), summaries=summary_stats(journal),
                        fail_end=ack_stats(failures),
                        receiver_drops=status["receiver_drops"])


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bin_dir", type=Path)
    parser.add_argument("store_repo", type=Path)
    parser.add_argument("--file", type=Path, action="append", default=[])
    parser.add_argument("--pairs", type=int, default=3)
    parser.add_argument("--expect-failures", type=int, default=0,
                        help="expected red chains per arm for a representative DSL workload")
    args = parser.parse_args()
    if args.pairs < 1 or args.expect_failures < 0:
        parser.error("pairs must be positive and expected failures nonnegative")
    with tempfile.TemporaryDirectory(prefix="lmbench-", dir="/tmp") as root:
        for pair in range(args.pairs):
            for pct in ([0,100] if pair%2==0 else [100,0]):
                name = f"p{pair+1}-s{pct}"
                print(json.dumps(timed_arm(args.bin_dir.resolve(), args.store_repo.resolve(),
                      Path(root)/name, [f.resolve() for f in args.file], name, pct, args.expect_failures)), flush=True)
