#!/usr/bin/env python3
"""Check attribution across nextest, tracing-init, the live broker and a portable case."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import zipfile

spec = importlib.util.spec_from_file_location("fixture", Path(__file__).with_name("test-nightly-capture.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


def run(args, cwd, env, expected=0):
    result = subprocess.run(args, cwd=cwd, env=env, capture_output=True, text=True, timeout=600)
    if result.returncode != expected:
        raise RuntimeError(f"{args}: exit {result.returncode}\n{result.stdout}\n{result.stderr}")


def scenario(root, binaries, tracing_init, wrapper, target):
    (root / "src").mkdir()
    (root / ".config").mkdir()
    (root / ".config/nextest.toml").write_text(
        'experimental = ["wrapper-scripts"]\n[scripts.wrapper.logmon]\n'
        'command = "logmon-nextest-wrapper"\n[[profile.default.scripts]]\n'
        'filter = "all()"\nrun-wrapper = "logmon"\n')
    (root / "Cargo.toml").write_text(f'''[package]
name = "nightly-wire-check"
version = "0.0.0"
edition = "2021"

[dev-dependencies]
tracing-init = {{ path = {json.dumps(str(tracing_init))}, features = ["logmon-test"] }}
tokio = {{ version = "1", features = ["macros", "rt-multi-thread"] }}
tracing = "0.1"
''')
    (root / "src/lib.rs").write_text('''
#[cfg(test)]
mod tests {
    #[tracing_init::logmon_test(flavor = "multi_thread", worker_threads = 2)]
    async fn handles_request() {
        std::thread::spawn(|| {
            let _entered = tracing::info_span!("wire_thread").entered();
            tracing::info!("wire thread evidence");
        }).join().unwrap();
        tokio::spawn(async {
            let _entered = tracing::info_span!("wire_task").entered();
            tracing::info!("wire task evidence");
        }).await.unwrap();
        assert!(std::env::var_os("EXPECTED_RED").is_none(), "wire assert evidence");
    }
}
''')
    base = dict(os.environ, CARGO_TARGET_DIR=str(target), CARGO_NET_OFFLINE="true",
                PATH=str(wrapper.parent) + os.pathsep + os.environ["PATH"])
    run(["cargo", "test", "--no-run"], root, base)
    journal = root / "run.jsonl"
    cli = binaries / "logmon-mcp"
    with fixture.broker(binaries / "logmon-broker", root / "daemon") as (daemon_env, gelf, otlp, _):
        env = dict(base, RUST_LOG="info", LOGMON_CONFIG_DIR=daemon_env["LOGMON_CONFIG_DIR"],
                   STORE_TEST_LOG_RUN_ID="wire", STORE_TEST_LOG_EVENTS_PATH=str(root / "deliveries"),
                   LOGMON_TEST_GELF_ADDR=f"127.0.0.1:{gelf}",
                   LOGMON_TEST_OTLP_ENDPOINT=f"http://127.0.0.1:{otlp}")
        fixture.cli(cli, env, "test-log", "begin", run_id="wire", jsonl_path=journal, grace_period_ms=2000)
        command = ["cargo", "nextest", "run"]
        run(command, root, dict(env, EXPECTED_RED="yes"), expected=100)
        deliveries = [json.loads(line) for line in (root / "deliveries").read_text().splitlines()]
        original = next(row["event"]["execution_id"] for row in deliveries if row.get("event"))
        run(command, root, dict(env, STORE_TEST_LOG_RERUN_OF=original))
        close = fixture.cli(cli, env, "test-log", "close", run_id="wire", wait_timeout_ms=15000)
        assert close["finalization"] == "settled" and close["persistence"] == "complete", close
    rows = [json.loads(line) for line in journal.read_text().splitlines()]
    summaries = [r for r in rows if r["event"] == "summary-final"]
    assert len(summaries) == 2, summaries
    for row in summaries:
        spans = row["summary"]["spans"]
        assert all(spans.get(name, {}).get("count") == 1 for name in ("wire_thread", "wire_task")), row
    assert any(r.get("rerun_of") == original and r["execution_id"] != original for r in summaries)
    cases = [r for r in rows if r["event"] == "case-ready"]
    assert len(cases) == 2, cases
    with fixture.broker(binaries / "logmon-broker", root / "postmortem") as (env, *_):
        for row in cases:
            case = root / row["case_path"]
            fixture.cli(cli, env, "cases", "load", path=case)
            with zipfile.ZipFile(case) as z:
                content = b"\n".join(z.read(name) for name in z.namelist())
            for token in (b"wire thread evidence", b"wire task evidence"):
                assert token in content, (row, token)
            if row["execution_id"] == original:
                assert b"wire assert evidence" in content
    print("Rust wire check passed: failed assertion, task and thread attributed, linked green rerun, two portable cases")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("binaries", "tracing_init", "wrapper", "target"):
        parser.add_argument(name, type=lambda value: Path(value).resolve())
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="lm-rust-", dir="/tmp") as temp:
        scenario(Path(temp), args.binaries, args.tracing_init, args.wrapper, args.target)
