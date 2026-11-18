#!/usr/bin/env python3
"""Exercise test-log capture through real local transports, then reload it in a new broker."""
import argparse
from contextlib import contextmanager
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
from urllib.request import Request, urlopen
import zipfile


def utc():
    return datetime.now(timezone.utc).isoformat()


def port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


@contextmanager
def broker(binary, root):
    root.mkdir()
    gelf, otlp = port(), port()
    env = dict(os.environ, LOGMON_CONFIG_DIR=str(root))
    with (root / "process.log").open("w") as diagnostics:
        process = subprocess.Popen(
            [binary, "--gelf-port", str(gelf), "--otlp-grpc-port", "0",
             "--otlp-http-port", str(otlp)], env=env, cwd=root,
            stdout=diagnostics, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 60
            while time.monotonic() < deadline and process.poll() is None:
                try:
                    with socket.socket(socket.AF_UNIX) as probe:
                        probe.connect(str(root / "logmon.sock"))
                    break
                except OSError:
                    time.sleep(.05)
            else:
                raise RuntimeError(f"broker failed to start in {root}")
            yield env, gelf, otlp, process
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()


def cli(binary, env, group, action, **kwargs):
    args = [binary, "--json", group, action]
    for field, value in kwargs.items():
        for item in value if isinstance(value, list) else [value]:
            raw = json.dumps(item) if isinstance(item, dict) else str(item)
            args.extend(["--" + field.replace("_", "-"), raw])
    output = subprocess.run(args, env=env, text=True, capture_output=True, timeout=60)
    if output.returncode:
        raise RuntimeError(f"{group}.{action}: {output.stderr} {output.stdout}")
    return json.loads(output.stdout)


def log(port_number, message, trace=None):
    payload = dict(version="1.1", host="fixture", short_message=message, level=6)
    if trace:
        payload["_trace_id"] = trace
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        sock.sendto(json.dumps(payload).encode(), ("127.0.0.1", port_number))


def span(port_number, trace, number, started, ended):
    item = {"traceId": trace, "spanId": f"{number:016x}", "name": "test.kernel",
            "startTimeUnixNano": str(started), "endTimeUnixNano": str(ended),
            "attributes": [{"key": "kernel.entry_point", "value": {"stringValue": "fixture-entry"}}]}
    body = {"resourceSpans": [{"scopeSpans": [{"spans": [item]}]}]}
    req = Request(f"http://127.0.0.1:{port_number}/v1/traces",
                  data=json.dumps(body).encode(), headers={"Content-Type": "application/json"})
    with urlopen(req, timeout=5) as response:
        assert response.status == 200


def scenario(binaries, root):
    daemon, frontend = binaries / "logmon-broker", binaries / "logmon-mcp"
    journal = root / "run.jsonl"
    with broker(daemon, root / "original") as (env, gelf, otlp, _):
        def control(action, **kw):
            return cli(frontend, env, "test-log", action, run_id="fixture", **kw)

        control("begin", jsonl_path=journal, grace_period_ms=1500,
                collect_distinct=["kernel.entry_point"], max_failures=1, max_rerun_cases=1)
        def start(name, number, **kw):
            trace = f"{number:032x}"
            identity = dict(execution_id=str(number), name=name, started_at=utc(),
                            selector={"field": "trace_id", "value": trace}, **kw)
            control("start", **identity)
            return identity, trace

        peer, other_trace = start("peer", 1)
        chain, trace = start("chain", 2)
        started = time.time_ns()
        log(gelf, "peer-context", other_trace)
        log(gelf, "chain-evidence", trace)
        log(gelf, "untagged-context")
        control("machine-load", ts=utc(), host="fixture", fields={"own": 17})
        ended = time.time_ns()
        span(otlp, trace, 1, started, ended)
        control("end", **chain, ended_at=utc(), verdict="FAIL",
                fields={"failed_test": "member", "message": "expected fixture assertion"})
        span(otlp, trace, 2, started, ended)
        control("end", **peer, ended_at=utc(), verdict="OK")
        rerun, _ = start("chain", 3, rerun_of="2")
        control("end", **rerun, ended_at=utc(), verdict="OK")
        result = control("close", wait_timeout_ms=10000)
        assert result["finalization"] == "settled" and result["persistence"] == "complete", result

    rows = [json.loads(line) for line in journal.read_text().splitlines()]
    summaries = {r["execution_id"]: r for r in rows if r["event"] == "summary-final"}
    assert len(summaries) == 3, rows
    summary = summaries["2"]["summary"]
    assert summary["spans"]["test.kernel"]["count"] == 2, summary
    assert summary["distinct_values"]["kernel.entry_point"]["values"] == ["fixture-entry"]
    assert summaries["3"]["rerun_of"] == "2"
    cases = {r["execution_id"]: r["case_path"] for r in rows if r["event"] == "case-ready"}
    assert set(cases) == {"2", "3"}, cases
    with broker(daemon, root / "postmortem") as (env, *_):
        for execution, relative in cases.items():
            assert not Path(relative).is_absolute() and ".." not in Path(relative).parts
            case = journal.parent / relative
            cli(frontend, env, "cases", "load", path=case)
            if execution == "2":
                with zipfile.ZipFile(case) as archive:
                    evidence = "\n".join(archive.read(f).decode() for f in archive.namelist())
                for token in ("peer-context", "chain-evidence", "untagged-context", "expected fixture assertion",
                              "Machine samples (1)", "fixture-entry"):
                    assert token in evidence, token
    print("live capture passed: grace-period span, peer and machine evidence, passing rerun, portable reload")


def store_scenario(binaries, source, root):
    from isolated_store import isolated_store
    journal = root / "dsl.jsonl"
    events = root / "delivery.jsonl"
    with broker(binaries / "logmon-broker", root / "dsl-broker") as (env, gelf, otlp, _):
        frontend = binaries / "logmon-mcp"
        cli(frontend, env, "test-log", "begin", run_id="dsl", jsonl_path=journal,
            grace_period_ms=10000, collect_distinct=["kernel.entry_point"])
        server_root = root / "store"
        with isolated_store(source, server_root, env, gelf, otlp, port) as (server_env, url):
            test = server_root / "case.tests"
            chain_source = '''test "chain_root" tags "fast smoke harness" expect_errors {
    run script { store.query("/"); 42 } { assert result == 42 }
}
test "chain_member" after "chain_root" tags "fast smoke harness" expect_errors {
    run script { store.query("/"); 42 } { assert result == 99 }
}
'''
            peer_source = '''test "peer" tags "fast smoke harness" expect_errors {
    run script { store.query("/"); 42 } { assert result == 42 }
}
'''
            def run_store(expected_code, **extra_env):
                args = [source / "target/debug/store-test", "--server", url,
                        "--no-auto-rebake", "--no-template", "--allow-stale",
                        "--max-parallel-tests", "2", test.name]
                output = server_root / "store-test.log"
                with output.open("w") as diagnostics:
                    proc = subprocess.Popen(args, cwd=server_root, stdout=diagnostics,
                        stderr=subprocess.STDOUT, env=dict(server_env, STORE_TEST_LOG_RUN_ID="dsl",
                        STORE_TEST_LOG_EVENTS_PATH=str(events), **extra_env))
                    try:
                        deadline = time.monotonic() + 180
                        if not extra_env:
                            while (time.monotonic() < deadline and proc.poll() is None and
                                   journal.read_text().count('"event":"start-test"') < 2):
                                time.sleep(.02)
                            cli(frontend, env, "test-log", "machine-load", run_id="dsl",
                                ts=utc(), host="fixture", fields={"own": 17})
                        proc.wait(timeout=max(1, deadline - time.monotonic()))
                    finally:
                        if proc.poll() is None:
                            proc.kill()
                            proc.wait()
                if proc.returncode != expected_code:
                    raise RuntimeError(f"unexpected Store exit {proc.returncode}: {output.read_text()[-8000:]}")
                return [json.loads(line) for line in events.read_text().splitlines()]

            test.write_text(chain_source + peer_source)
            delivery = run_store(1)
            ends = [r["event"] for r in delivery if r.get("delivery") == "confirmed"]
            assert sorted(r["verdict"] for r in ends) == ["FAIL", "OK"], ends
            failed = next(r for r in ends if r["verdict"] == "FAIL")
            assert failed["fields"]["failed_test"] == "chain_member", failed
            assert "99" in failed["fields"]["message"], failed
            peer = next(r for r in ends if r["verdict"] == "OK")
            test.write_text(chain_source.replace("result == 99", "result == 42"))
            rerun_delivery = run_store(0, STORE_TEST_LOG_RERUN_OF=failed["execution_id"])
            rerun = next(r["event"] for r in rerun_delivery if r.get("event", {}).get("rerun_of"))
            assert (rerun["verdict"], rerun["name"], rerun["rerun_of"]) == (
                "OK", failed["name"], failed["execution_id"]), rerun
            assert rerun["execution_id"] != failed["execution_id"]
        closed = cli(frontend, env, "test-log", "close", run_id="dsl", wait_timeout_ms=15000)
        assert closed["finalization"] == "settled", closed
        rows = [json.loads(line) for line in journal.read_text().splitlines()]
        summaries = [r for r in rows if r["event"] == "summary-final"]
        assert len(summaries) == 3, rows
        for summary in summaries:
            assert "store.query" in summary["summary"]["distinct_values"]["kernel.entry_point"]["values"], summary
        cases = {r["execution_id"]: r["case_path"] for r in rows if r["event"] == "case-ready"}
        assert set(cases) == {failed["execution_id"], rerun["execution_id"]}, cases
    with broker(binaries / "logmon-broker", root / "dsl-reader") as (env, *_):
        for execution, relative in cases.items():
            path = journal.parent / relative
            cli(frontend, env, "cases", "load", path=path)
            if execution == failed["execution_id"]:
                with zipfile.ZipFile(path) as archive:
                    doc = next(n for n in archive.namelist() if n.endswith(".md"))
                    report = archive.read(doc).decode()
                    for token in ("chain_member", peer["execution_id"], "Machine samples (1)", "99"):
                        assert token in report, token
    print("Store DSL passed: linked green rerun, failed chain member, peer/machine context, portable cases")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bin_dir", type=Path)
    parser.add_argument("--store-repo", type=Path, help="also run real store-test and store_server")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="lmtest-", dir="/tmp") as directory:
        scenario(args.bin_dir.resolve(), Path(directory))
        if args.store_repo:
            store_scenario(args.bin_dir.resolve(), args.store_repo.resolve(), Path(directory))
