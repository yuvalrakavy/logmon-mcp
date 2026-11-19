#!/usr/bin/env python3
"""Poll live CLI progress on eight isolated domains while eight producers per domain send test results."""
import argparse
import asyncio
from collections import defaultdict
from datetime import datetime, timezone
import json
import math
from pathlib import Path
import runpy
import socket
import tempfile
import time

# Use the same ephemeral receiver/socket setup as the capture integration test.
fixture = runpy.run_path(str(Path(__file__).with_name("test-nightly-capture.py")))
VERDICTS = {"OK", "FAIL", "TIMEOUT", "ABORTED", "NO_VERDICT"}


def stamp():
    return datetime.now(timezone.utc).isoformat()


def distribution(values):
    samples = sorted(values)
    return {"samples": len(samples), **{name: round(samples[math.ceil(p * len(samples)) - 1], 2)
            for name, p in (("p50_ms", .5), ("p99_ms", .99), ("max_ms", 1))}}


async def run_arm(binaries, directory, env, domains, per_worker, poll, interval):
    samples = defaultdict(list)
    rpc_times = []
    ports = {}
    run_id = "observed" if poll else "baseline"

    async def cli(label, args):
        start = time.monotonic()
        proc = await asyncio.create_subprocess_exec(
            str(binaries / "logmon-mcp"), "--json", *args, env=env,
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
        stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=20)
        assert proc.returncode == 0, (args, stdout, stderr)
        if label:
            samples[label].append(1000 * (time.monotonic() - start))
        return json.loads(stdout)

    async def worker(domain, worker_id):
        read, write = await asyncio.open_unix_connection(str(Path(env["LOGMON_CONFIG_DIR"]) / "logmon.sock"))
        seq = 0

        async def call(method, params):
            nonlocal seq
            seq += 1
            start = time.monotonic()
            write.write(json.dumps(dict(id=seq, jsonrpc="2.0", method=method, params=params)).encode() + b"\n")
            await write.drain()
            while True:
                raw = await asyncio.wait_for(read.readline(), 20)
                assert raw, "broker disconnected"
                reply = json.loads(raw)
                if reply.get("id") == seq:
                    assert not reply.get("error"), reply
                    if method != "session.start":
                        rpc_times.append(1000 * (time.monotonic() - start))
                        assert reply["result"]["status"] == "accepted", reply
                    return

        try:
            await call("session.start", dict(name=f"{domain}-producer-{worker_id}", domain=domain, protocol_version=1))
            for i in range(per_worker):
                key = f"{worker_id * per_worker + i + 1:032x}"
                identity = dict(run_id=run_id, execution_id=key, name=f"worker-{worker_id}/test-{i}",
                                started_at=stamp(), selector=dict(field="trace_id", value=key))
                await call("test_log.start", identity)
                fixture["log"](ports[domain][0], "status benchmark", key)
                if i % 8 == 0:
                    ns = time.time_ns()
                    await asyncio.to_thread(fixture["span"], ports[domain][1], key, i + 1, ns - 1000, ns)
                await asyncio.sleep(.1)
                await call("test_log.end", dict(identity, ended_at=stamp(), verdict="OK"))
        finally:
            write.close()
            await write.wait_closed()

    finished = asyncio.Event()

    async def dashboard():
        while not finished.is_set():
            listing = await cli("domains_list", ["domains", "list"])
            rows = listing if isinstance(listing, list) else listing["domains"]
            active = [r["name"] for r in rows if r["name"] in domains and r.get("test_log")]
            assert set(active) == set(domains), rows
            details = await asyncio.gather(*(cli("status", ["--domain", d, "test-log", "status"]) for d in active))
            for detail in details:
                assert detail["schema_version"] == 1
                e = detail["test_log"]["executions"]
                assert e["started"] == e["ended"] + e["in_flight"]
                assert set(e["by_verdict"]) == VERDICTS and sum(e["by_verdict"].values()) == e["ended"]
                assert e["planned"] == per_worker * 8
            history = await cli("test_log_list", ["--domain", active[0], "test-log", "list"])
            assert history["schema_version"] == 1 and history["runs"][0]["run_id"] == run_id
            try:
                await asyncio.wait_for(finished.wait(), interval)
            except asyncio.TimeoutError:
                pass

    for d in domains:
        if not poll:
            # Choose UDP and TCP ports independently. The reported receiver
            # configuration can retain a literal zero for an ephemeral bind.
            with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp:
                udp.bind(("127.0.0.1", 0))
                gelf = udp.getsockname()[1]
            http = fixture["port"]()
            await cli(None, ["domains", "create", "--name", d, "--gelf-port", str(gelf),
                             "--otlp-grpc-port", "0", "--otlp-http-port", str(http)])
    lanes = await cli(None, ["domains", "list"])
    for row in lanes if isinstance(lanes, list) else lanes["domains"]:
        ports[row["name"]] = (row["gelf_port"], row["otlp_http_port"])
    for d in domains:
        result = await cli(None, ["--domain", d, "test-log", "begin", "--run-id", run_id, "--jsonl-path",
                                  str(directory / f"{d}-{run_id}.jsonl"), "--grace-period-ms", "10",
                                  "--planned-executions", str(per_worker * 8), "--summary-sample-percent", "0"])
        assert result["status"] == "accepted", result

    started = time.monotonic()
    tasks = asyncio.gather(*(worker(d, i) for d in domains for i in range(8)))
    observer = asyncio.create_task(dashboard()) if poll else None
    try:
        await tasks
        elapsed = time.monotonic() - started
    finally:
        finished.set()
        if observer:
            await observer

    for d in domains:
        close = await cli(None, ["--domain", d, "test-log", "close", "--run-id", run_id, "--wait-timeout-ms", "3000"])
        assert close["finalization"] == "settled" and close["persistence"] == "complete", close
        status = await cli(None, ["--domain", d, "test-log", "status", "--run-id", run_id])
        detail = status["test_log"]
        assert detail["executions"]["by_verdict"]["OK"] == per_worker * 8 and detail["state"] == "closed"
        assert (await cli(None, ["--domain", d, "test-log", "status"]))["test_log"] is None
        runs = (await cli(None, ["--domain", d, "test-log", "list"]))["runs"]
        assert runs[0]["run_id"] == run_id and runs[0]["executions"] == detail["executions"]

    return dict(executions=per_worker * 8 * len(domains), elapsed_s=round(elapsed, 3),
                lifecycle_rpc=distribution(rpc_times), cli={k: distribution(v) for k, v in samples.items()})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--per-worker", type=int, default=160)
    args = parser.parse_args()
    if args.per_worker < 1:
        parser.error("--per-worker must be positive")
    bins = args.binaries.resolve()
    with tempfile.TemporaryDirectory(prefix="progress-", dir="/tmp") as temp:
        directory = Path(temp)
        with fixture["broker"](bins / "logmon-broker", directory / "daemon") as (env, *_):
            results = {arm: asyncio.run(run_arm(bins, directory, env, [f"ci-{i}" for i in range(8)],
                                                args.per_worker, arm == "observed", 2))
                       for arm in ("baseline", "observed")}
    print(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
