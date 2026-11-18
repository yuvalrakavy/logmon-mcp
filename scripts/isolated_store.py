"""Boot disposable Store and MQTT servers, with only private ports and data."""
from contextlib import contextmanager, ExitStack
from pathlib import Path
import shutil
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
def isolated_store(source, root, broker_env, gelf, otlp, port_allocator):
    """Yield (environment, URL) using source/target/debug/store_server and a private copy of data."""
    root.mkdir()
    shutil.copytree(source / "data", root / "data")
    grpc, web, mqtt = (port_allocator() for _ in range(3))
    toml = root / "server.toml"
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
span_queue_size = 16384
[logging.store_server]
filter = "warn,store_server=info"
[logging.store_test]
filter = "warn,store_test=info"
''')
    env = dict(broker_env, STORE_CONFIG=str(toml), STORE_DEV_TRACK="0",
               STORE_TEST_LOG_KERNEL_SPANS="1", OTEL_BSP_SCHEDULE_DELAY="100")
    mosquitto = shutil.which("mosquitto") or "/opt/homebrew/sbin/mosquitto"
    with ExitStack() as stack:
        mqtt_log = root / "mqtt.log"
        mq = stack.enter_context(child([mosquitto, "-p", str(mqtt)], env, root, mqtt_log))
        wait_ready(mq, mqtt, mqtt_log)
        server_log = root / "server.log"
        server = stack.enter_context(child(
            [source / "target/debug/store_server", "--config", toml], env, root, server_log))
        wait_ready(server, grpc, server_log)
        yield env, f"http://127.0.0.1:{grpc}"
