"""Boot disposable Store and MQTT servers, with only private ports and data."""
from contextlib import contextmanager, ExitStack
from pathlib import Path
import json
import shutil
import tomllib

from frozen_dsl import private_checkout
import socket
import subprocess
import time


@contextmanager
def child(command, env, cwd, logfile):
    with logfile.open("w") as stream:
        process = subprocess.Popen(command, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                   stdout=stream, stderr=subprocess.STDOUT)
        try:
            yield process
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()


def wait_ready(process, port, log):
    deadline = time.monotonic() + 90
    while time.monotonic() < deadline and process.poll() is None:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=.1):
                return
        except OSError:
            time.sleep(.05)
    raise RuntimeError(f"server on port {port} failed to start: {log.read_text()[-4000:]}")


@contextmanager
def isolated_store(source, root, broker_env, gelf, otlp, port_allocator, checkout=False, assets=None, revision=None):
    """Yield (environment, URL) using source/target/debug/store_server and a private copy of data."""
    assets = assets or source
    if checkout:
        private_checkout(source, root, assets, revision)
    else:
        root.mkdir()
        shutil.copytree(assets / "data", root / "data")
    grpc, web, mqtt = (port_allocator() for _ in range(3))
    toml = root / "server.toml"
    candidate_config = toml.read_bytes() if checkout else None
    toml.write_text(f'''[store_server]
port = {grpc}
web_port = {web}
data = "data"
stores = []
mqtt_override = "127.0.0.1:{mqtt}"
[logging]
destination = "go"
level = "info"
[logging.gelf]
address = "127.0.0.1:{gelf}"
[logging.otel]
endpoint = "http://127.0.0.1:{otlp}"
transport = "http"
beacon_port = {int(broker_env["LOGMON_FIXTURE_BEACON_PORT"])}
span_queue_size = 16384
[logging.store_server]
filter = "warn,store_server=info"
[logging.store_test]
filter = "warn,store_test=info"
''')
    if checkout:
        # Retain the candidate's filters, queue sizes and kernel tunables. Replace
        # all external endpoints and disable loading the live home automation stores.
        config = tomllib.loads(candidate_config.decode())
        config["store_server"].update(port=grpc, web_port=web, data="data", stores=[],
                                      mqtt_override=f"127.0.0.1:{mqtt}")
        logger = config["logging"]
        logger["gelf"]["address"] = f"127.0.0.1:{gelf}"
        logger["otel"].update(endpoint=f"http://127.0.0.1:{otlp}", transport="http",
                              beacon_port=int(broker_env["LOGMON_FIXTURE_BEACON_PORT"]))
        logger.setdefault("tokio_console", {})["bind"] = f"127.0.0.1:{port_allocator()}"
        lines = []
        def write_section(name, table):
            lines.append(f"[{name}]")
            for key, value in table.items():
                if not isinstance(value, dict):
                    lines.append(f"{key} = {json.dumps(value)}")
            for key, value in table.items():
                if isinstance(value, dict):
                    write_section(f"{name}.{key}", value)
        for name, table in config.items():
            write_section(name, table)
        toml.write_text("\n".join(lines) + "\n")
    env = dict(broker_env, STORE_CONFIG=str(toml), STORE_DEV_TRACK="0",
               STORE_TEST_LOG_KERNEL_SPANS="1", OTEL_BSP_SCHEDULE_DELAY="100")
    if checkout:
        env["HT_BUDGET_FACTOR"] = "16"
        # The smoke fixture flushes every 100 ms. Real batches use the same
        # batching interval as the nightly (SDK default unless the caller sets it).
        if "OTEL_BSP_SCHEDULE_DELAY" not in broker_env:
            env.pop("OTEL_BSP_SCHEDULE_DELAY")
    mosquitto = shutil.which("mosquitto") or "/opt/homebrew/sbin/mosquitto"
    with ExitStack() as stack:
        mqtt_log = root / "mqtt.log"
        mq = stack.enter_context(child([mosquitto, "-p", str(mqtt)], env, root, mqtt_log))
        wait_ready(mq, mqtt, mqtt_log)
        server_log = root / "server.log"
        server = stack.enter_context(child(
            [assets / "target/debug/store_server", "--config", toml], env, root, server_log))
        wait_ready(server, grpc, server_log)
        yield env, f"http://127.0.0.1:{grpc}"
