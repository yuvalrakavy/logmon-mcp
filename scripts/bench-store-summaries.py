#!/usr/bin/env python3
"""Paired pilot using a private broker, Store server, and 16 Store DSL workers."""
import argparse
from collections import Counter
import csv
from datetime import datetime
import importlib.util
import json
import math
import os
from pathlib import Path
import re
import shutil
import socket
from frozen_dsl import freeze_inputs, load_plan
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


def sampled_name(name, percent):
    code = 0xcbf29ce484222325
    for byte in name.encode("utf-8"):
        code = ((code ^ byte) * 0x100000001b3) & ((1 << 64) - 1)
    return percent >= 100 or (percent > 0 and float(code) < percent * .01 * float((1 << 64) - 1))


def summary_stats(journal, percent=None):
    counts = Counter()
    for line in journal.read_text().splitlines():
        row = json.loads(line)
        if row["event"] in ("case-ready", "case-error", "late-telemetry", "test-log-gap"):
            counts[row["event"]] += 1
        if row["event"] != "summary-final":
            continue
        counts[row["summary_status"]] += 1
        if percent is not None and (row["summary_status"] == "collected") != sampled_name(row["name"], percent):
            raise RuntimeError(f"incorrect stable-name sampling for {row['name']} at {percent}%")
        if summary := row.get("summary"):
            counts["truncated_attribute_sets"] += sum(
                bool(v.get("truncated")) for v in summary["distinct_values"].values())
            counts["executions_over_span_name_limit"] += bool(summary["span_overflow"]["count"])
            counts["omitted_attribute_keys"] += summary["omitted_attribute_keys"]
    return dict(counts)


def exporter_warnings(runner_log):
    pattern = r"telemetry availability — (\S+): (\d+) export failure\(s\), (\d+) batch\(es\) discarded"
    totals = {}
    for service, failures, discarded in re.findall(pattern, runner_log.read_text()):
        count = totals.setdefault(service, {"export_failures": 0, "discarded_batches": 0})
        count["export_failures"] += int(failures)
        count["discarded_batches"] += int(discarded)
    return totals


def replay_unconfirmed(frontend, env, run_id, rows):
    batch = []
    size = 0
    def deliver(events):
        response = fixture.cli(frontend, env, "test-log", "replay", run_id=run_id, events=events)
        replies = response.get("results", [])
        if len(replies) != len(events) or any(r["status"] not in ("applied", "already_applied") for r in replies):
            raise RuntimeError(f"replay failed: {response}")
    for row in rows:
        if row.get("delivery") != "unconfirmed" or "event" not in row:
            continue
        event = row["event"]
        length = len(json.dumps(event).encode())
        if length > 96 * 1024:
            raise RuntimeError(f"unconfirmed end exceeds replay limit: {event['execution_id']}")
        if batch and (len(batch) == 100 or size + length > 96 * 1024):
            deliver(batch)
            batch = []
            size = 0
        batch.append(event)
        size += length
    if batch:
        deliver(batch)


def timed_arm(bins, source, root, files, label, percent, expected_failures, plan=None, prewarm=False, ack_ms=100, assets=None):
    root.mkdir()
    assets = assets or source
    run_id = label
    events = root / "runner.jsonl"
    journal = root / "test-log.jsonl"
    with fixture.broker(bins / "logmon-broker", root / "broker") as (env, gelf, otlp, daemon):
        frontend = bins / "logmon-mcp"
        def control(op, **params):
            return fixture.cli(frontend, env, "test-log", op, run_id=run_id, **params)
        with isolated_store(source, root / "store", env, gelf, otlp, fixture.port, checkout=bool(plan), assets=assets,
                            revision=plan["candidate_commit"] if plan else None) as (server_env, url):
            cwd = root / "store"
            if not files and not plan:
                path = cwd / "pilot.tests"
                path.write_text("".join('test "pilot_%d" tags "fast smoke harness" expect_errors {\n'
                    '    run script { store.query("/"); 42 } { assert result == 42 }\n}\n' % i for i in range(32)))
                files = [path.name]  # stable chain names across disposable run directories
            runner = (cwd if plan else assets) / "target/debug/store-test"
            if prewarm:
                # The scheduled nightly bakes templates before timing the DSL phase.
                # Keep that work out of both arms and let a failure stop the trial.
                with (root / "template-prep.log").open("w") as log:
                    subprocess.run([runner, "--server", url, "--allow-stale", "rebake-templates"],
                                   cwd=cwd, env=server_env, stdout=log, stderr=subprocess.STDOUT,
                                   timeout=3600, check=True)
            settings = {"lifecycle_ack_timeout_ms": ack_ms}
            if plan:
                settings.update(max_failures=50, max_rerun_cases=10, max_slow_pass_cases=20)
            control("begin", **settings, jsonl_path=journal, grace_period_ms=10000 if plan else 1000, collect_distinct=["kernel.entry_point"],
                    summary_sample_percent=percent)
            rates = IngestRates(fixture.cli(frontend, env, "status", "get"))
            rss, cpu_before = process_stats(daemon.pid)
            load_at_start = os.getloadavg()
            batches = plan["batches"] if plan else {1: files}
            runner_env = dict(server_env, STORE_TEST_LOG_RUN_ID=run_id, STORE_TEST_LOG_EVENTS_PATH=str(events))
            runner_env["STORE_TEST_LOG_ACK_TIMEOUT_MS"] = str(ack_ms)
            if plan:
                estimates = {}
                history = Path(plan["source_run"]) / "status.tsv"
                if history.is_file():
                    with history.open() as stream:
                        for row in csv.DictReader(stream, delimiter="\t"):
                            if row.get("id", "").startswith("dsl:"):
                                try:
                                    estimates[row["id"]] = round(float(row["seconds"]) * 1000)
                                except (ValueError, TypeError):
                                    pass
                estimates_path = root / "estimates.json"
                estimates_path.write_text(json.dumps(estimates))
                runner_env["STORE_TEST_LOG_ESTIMATES_PATH"] = str(estimates_path)
            batch_walls, statuses = {}, []
            start = time.monotonic()
            next_rate = start + 5
            next_load = start
            with (root/"store-test.log").open("w") as log:
                for number, batch_files in batches.items():
                    batch_start = time.monotonic()
                    options = (["--results-out", str(root / f"dsl-{number:03}.jsonl"), "-v", "failed"]
                               if plan else ["--no-auto-rebake", "--no-template"])
                    proc = subprocess.Popen([runner, "--server", url, "--allow-stale", "--max-parallel-tests",
                                             "16", *options, *batch_files], env=runner_env,
                                            stdout=log, stderr=subprocess.STDOUT, cwd=cwd)
                    try:
                        while proc.poll() is None:
                            live_rss, _ = process_stats(daemon.pid)
                            rss = max(rss, live_rss)
                            if time.monotonic() >= next_rate:
                                rates.observe(fixture.cli(frontend, env, "status", "get"))
                                next_rate = time.monotonic() + 5
                            if plan and time.monotonic() >= next_load:
                                control("machine-load", ts=fixture.utc(), host=socket.gethostname(),
                                        fields={"loadavg_1m": os.getloadavg()[0]})
                                next_load = time.monotonic() + 60
                            try:
                                proc.wait(timeout=.25)
                            except subprocess.TimeoutExpired:
                                if time.monotonic() - batch_start > 1800:
                                    raise RuntimeError(f"batch {number} exceeded 30 minutes")
                    finally:
                        if proc.poll() is None:
                            proc.kill()
                            proc.wait()
                    if proc.returncode not in (0, 1):
                        raise RuntimeError((root/"store-test.log").read_text()[-2500:])
                    statuses.append(proc.returncode)
                    batch_walls[number] = time.monotonic() - batch_start
            wall = time.monotonic() - start
            rates.observe(fixture.cli(frontend, env, "status", "get"))
            _, cpu_after = process_stats(daemon.pid)
            local_rows = [json.loads(line) for line in events.read_text().splitlines()]
            replay_unconfirmed(frontend, env, run_id, local_rows)
            begin_close = time.monotonic()
            close = control("close", wait_timeout_ms=20000)
            close_seconds = time.monotonic() - begin_close
            (root / "close.json").write_text(json.dumps(close, indent=2))
            if close.get("finalization") != "settled" or close.get("persistence") != "complete":
                raise RuntimeError(str(close))
            rows = local_rows
            end = [row for row in rows if "event" in row]
            starts = [row for row in rows if "start" in row]
            failures = [row for row in end if row["event"]["verdict"] != "OK"]
            if not end or (expected_failures is not None and len(failures) != expected_failures) or any(statuses) != bool(failures):
                raise RuntimeError(f"unexpected Store result: exit={statuses}, completed={len(end)}, "
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
            summaries = summary_stats(journal, percent)
            finalized = summaries.get("collected", 0) + summaries.get("sampled_out", 0)
            if finalized != len(end):
                raise RuntimeError(f"{finalized} final summaries for {len(end)} ended executions")
            return dict(run=label, percent=percent, wall_s=wall, close_s=close_seconds, completed=len(end),
                        batch_wall_s=batch_walls, peak_chains=peak, peak_rss_kib=rss, journal_bytes=journal.stat().st_size,
                        start=ack_stats(starts), end=ack_stats(end),
                        daemon_cpu_s=cpu_after-cpu_before, loadavg_start=load_at_start,
                        loadavg_end=os.getloadavg(), ingestion=rates.report(), summaries=summaries,
                        fail_end=ack_stats(failures),
                        receiver_drops=status["receiver_drops"], trace_ingest=status.get("trace_ingest"),
                        sender_exporter_warnings=exporter_warnings(root/"store-test.log"))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bin_dir", type=Path)
    parser.add_argument("store_repo", type=Path)
    parser.add_argument("--file", type=Path, action="append", default=[])
    parser.add_argument("--nightly-run", type=Path,
                        help="frozen run directory containing run.json and raw/6-dsl-NNN.log")
    parser.add_argument("--batch", type=int, action="append", default=[],
                        help="select a recorded batch (repeatable); default replays all recorded batches")
    parser.add_argument("--records", type=Path, help="new directory for plan, measurements and each arm's files")
    parser.add_argument("--plan-only", action="store_true", help="validate and print the frozen plan without running")
    parser.add_argument("--ack-ms", type=int, default=100, help="lifecycle acknowledgement deadline")
    parser.add_argument("--summary-percent", type=float, default=100,
                        help="compare this stable-name sample percentage against zero (default 100)")
    parser.add_argument("--single-arm", type=float, help="functional rehearsal at this sampling percentage")
    parser.add_argument("--pairs", type=int, default=3)
    parser.add_argument("--expect-failures", type=int, default=None,
                        help="expected red chains per arm for a representative DSL workload")
    args = parser.parse_args()
    if args.pairs < 1 or args.ack_ms < 1 or (args.expect_failures or 0) < 0:
        parser.error("pairs and ack-ms must be positive and expected failures nonnegative")
    if not 0 < args.summary_percent <= 100 or (args.single_arm is not None and not 0 <= args.single_arm <= 100):
        parser.error("summary-percent must be in (0, 100] and single-arm in [0, 100]")
    if args.nightly_run and args.file or not args.nightly_run and (args.batch or args.plan_only):
        parser.error("use --batch/--plan-only with --nightly-run and --file only with a smoke run")
    source = args.store_repo.resolve()
    plan = load_plan(args.nightly_run.resolve(), source, args.batch) if args.nightly_run else None
    if args.plan_only:
        print(json.dumps(plan, indent=2))
        raise SystemExit
    if plan and not args.records:
        parser.error("frozen nightly replay requires --records to retain the evidence")
    if args.records:
        args.records = args.records.resolve()
        args.records.mkdir(parents=True)
        if plan:
            (args.records / "plan.json").write_text(json.dumps(plan, indent=2))
    expected = args.expect_failures if plan else args.expect_failures or 0
    with tempfile.TemporaryDirectory(prefix="lmbench-frozen-", dir="/tmp") as inputs:
        frozen = Path(inputs) / "inputs"
        checksums = freeze_inputs(args.bin_dir.resolve(), source, frozen, full_workspace=bool(plan))
        for pair in range(1 if args.single_arm is not None else args.pairs):
            percentages = [args.single_arm] if args.single_arm is not None else ([0, args.summary_percent] if pair % 2 == 0 else [args.summary_percent, 0])
            for pct in percentages:
                with tempfile.TemporaryDirectory(prefix="lmbench-", dir="/tmp") as root:
                    name = f"p{pair+1}-s{pct:g}"
                    arm = Path(root)/name
                    try:
                        result = timed_arm(frozen / "logmon", source, arm,
                                           [f.resolve() for f in args.file], name, pct, expected, plan,
                                           prewarm=bool(plan) and args.single_arm is None, ack_ms=args.ack_ms,
                                           assets=frozen / "store")
                    finally:
                        if args.records and arm.exists():
                            destination = args.records / name
                            destination.mkdir()
                            for item in arm.iterdir():
                                if item.is_file():
                                    shutil.copy2(item, destination / item.name)
                                elif item.name == "cases":
                                    shutil.copytree(item, destination / item.name)
                            for rel in ("broker/process.log", "store/server.log"):
                                if (arm / rel).is_file():
                                    shutil.copy2(arm / rel, destination / Path(rel).name)
                    result["binary_sha256"] = checksums
                    line = json.dumps(result)
                    print(line, flush=True)
                    if args.records:
                        with (args.records / "measurements.jsonl").open("a") as out:
                            print(line, file=out)
