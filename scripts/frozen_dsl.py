"""Read recorded nightly batch arguments as data and stage a private Store checkout."""
import hashlib
import json
from pathlib import Path
import re
import shlex
import shutil
import subprocess


def load_plan(nightly_run, source, only_batches):
    manifest = json.loads((nightly_run / "run.json").read_text())
    plan = {}
    for log in sorted((nightly_run / "raw").glob("6-dsl-*.log")):
        match = re.fullmatch(r"6-dsl-(\d+)\.log", log.name)
        if not match:
            continue
        number = int(match[1])
        if only_batches and number not in only_batches:
            continue
        with log.open() as stream:
            words = shlex.split(stream.readline().strip())
        if (len(words) < 3 or words[0] != "$" or Path(words[1]).name != "store-test"
                or "--max-parallel-tests" not in words
                or words[words.index("--max-parallel-tests") + 1] != "16"):
            raise ValueError(f"{log}: expected a 16-worker Store DSL batch")
        files = [word for word in words[2:] if word.startswith("tests/")]
        if not files or any(".." in Path(f).parts or not (source / f).is_file()
                            or not (source / f).resolve().is_relative_to(source / "tests") for f in files):
            raise ValueError(f"{log}: missing or unexpected DSL file")
        plan[number] = files
    if not plan or set(only_batches) - plan.keys():
        raise ValueError("one or more requested batch records are missing")
    return {"source_run": str(nightly_run), "selection_commit": manifest["commit"],
            "candidate_commit": subprocess.check_output(["git", "-C", source, "rev-parse", "HEAD"], text=True).strip(),
            "batches": plan}


def freeze_inputs(bins, source, dest):
    """Hold one broker, CLI, Store build and set of baked assets for every arm."""
    dest.mkdir()
    for sub in ("logmon", "store/target/debug", "store/data"):
        (dest / sub).mkdir(parents=True)
    for name in ("logmon-broker", "logmon-mcp"):
        shutil.copy2(bins / name, dest / "logmon" / name)
    for binary in (source / "target/debug").iterdir():
        if binary.is_file() and binary.stat().st_mode & 0o111:
            shutil.copy2(binary, dest / "store/target/debug" / binary.name)
    for name in ("store-test", "store_server"):
        if not (dest / "store/target/debug" / name).is_file():
            raise ValueError(f"missing candidate Store binary: {name}")
    shutil.copytree(source / "data", dest / "store/data", dirs_exist_ok=True)
    digests = {}
    for binary in [*sorted((dest / "logmon").iterdir()), *sorted((dest / "store/target/debug").iterdir())]:
        with binary.open("rb") as stream:
            digests[binary.name] = hashlib.file_digest(stream, "sha256").hexdigest()
    return digests


def private_checkout(source, root, assets=None, revision=None):
    subprocess.run(["git", "clone", "--quiet", "--local", "--no-hardlinks", str(source), str(root)],
                   check=True, stdout=subprocess.DEVNULL)
    if revision:
        subprocess.run(["git", "-C", root, "checkout", "--quiet", "--detach", revision], check=True)
    assets = assets or source
    # Runtime expects built tools and baked templates under its own checkout. Copy
    # the binaries and generated artifacts so any rebake stays in this arm.
    debug = root / "target/debug"
    debug.mkdir(parents=True)
    for binary in (assets / "target/debug").iterdir():
        if binary.is_file() and binary.stat().st_mode & 0o111:
            shutil.copy2(binary, debug / binary.name)
    for artifact in (assets / "data/stores/_templates").glob("*/template.artifact"):
        target = root / artifact.relative_to(assets)
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(artifact, target)
