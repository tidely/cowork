#!/usr/bin/env python3
"""Bounded Linux perf capture of live generation on an owned, isolated Ollama.

Run with uv run --no-project python scripts/profile-generation.py --help.
All output goes under ignored target/; no packages or models are downloaded.
"""

import argparse
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def stop(process):
    """Terminate only a process group we created, including its children."""
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        process.wait(timeout=3)
    except subprocess.TimeoutExpired:
        pass
    # The parent may have exited while a runner/window still lives in its group.
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait(timeout=3)


def run(command, env, timeout, output=None):
    process = subprocess.Popen(
        command,
        cwd=ROOT,
        env=env,
        stdout=output if output is not None else subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        start_new_session=True,
    )
    try:
        text, _ = process.communicate(timeout=timeout)
        if process.returncode:
            if text:
                print(text, file=sys.stderr)
            raise RuntimeError(f"command exited {process.returncode}: {command[0]}")
        return text
    finally:
        stop(process)


def perf_path(requested):
    candidates = [requested, shutil.which("perf")]
    candidates += [str(p) for p in Path("/usr/lib/linux-tools").glob("*/perf")]
    for path in candidates:
        if path and Path(path).is_file():
            result = subprocess.run(
                [path, "--version"], capture_output=True, timeout=5, check=False
            )
            if result.returncode == 0:
                return path
    raise RuntimeError("perf not found; supply --perf with its installed path")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--history", type=int, choices=[0, 100, 1000], default=1000)
    parser.add_argument("--variant", choices=["short", "markdown-long"], default="short")
    parser.add_argument("--reasoning", action="store_true")
    parser.add_argument("--num-predict", type=int, default=128)
    parser.add_argument("--port", type=int, default=11435)
    parser.add_argument("--perf", help="installed perf executable")
    args = parser.parse_args()
    if not 1 <= args.num_predict <= 256:
        parser.error("--num-predict must be in 1..=256")
    if not 1024 <= args.port <= 65535 or args.port == 11434:
        parser.error("choose an isolated unprivileged port, not Ollama's default 11434")
    perf = perf_path(args.perf)
    ollama = shutil.which("ollama")
    if ollama is None:
        parser.error("ollama must already be installed with qwen3.8:27b")

    env = os.environ.copy()
    env["CARGO_PROFILE_RELEASE_DEBUG"] = "1"
    artifacts = run(
        ["cargo", "test", "--release", "-p", "cowork", "generation_profile",
         "--no-run", "--message-format=json"],
        env, 300,
    )
    executable = None
    for line in artifacts.splitlines():
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if (message.get("reason") == "compiler-artifact"
                and message.get("target", {}).get("name") == "cowork"
                and message.get("executable")):
            executable = message["executable"]
    if executable is None:
        raise RuntimeError("Cargo did not report the cowork test executable")

    with socket.socket() as probe:
        if probe.connect_ex(("127.0.0.1", args.port)) == 0:
            raise RuntimeError(f"port {args.port} occupied; refusing to reuse someone else's server")
    output_root = ROOT / "target" / "generation-profiles"
    output_root.mkdir(parents=True, exist_ok=True)
    output = Path(tempfile.mkdtemp(prefix=f"{args.variant}-h{args.history}-", dir=output_root))
    control = output / "control"
    os.mkfifo(control)
    base_url = f"http://127.0.0.1:{args.port}"
    env.update(
        OLLAMA_HOST=f"127.0.0.1:{args.port}",
        OLLAMA_API_BASE_URL=base_url,
        OLLAMA_KEEP_ALIVE="0",
        OLLAMA_NO_CLOUD="true",
        OLLAMA_NOPRUNE="true",
        COWORK_GENERATION_PROFILE_CONTROL=str(control),
        COWORK_GENERATION_PROFILE_HISTORY=str(args.history),
        COWORK_GENERATION_PROFILE_VARIANT=args.variant,
        COWORK_GENERATION_PROFILE_REASONING=str(args.reasoning).lower(),
        COWORK_GENERATION_PROFILE_NUM_PREDICT=str(args.num_predict),
    )
    print(f"Profiling with own Ollama at {base_url}; output: {output}", flush=True)
    capture_error = None
    with (output / "ollama.log").open("w") as log:
        server = subprocess.Popen(
            [ollama, "serve"], env=env, stdout=log, stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        try:
            deadline = time.monotonic() + 10
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            while True:
                try:
                    with opener.open(base_url + "/api/tags", timeout=1) as response:
                        models = json.load(response)["models"]
                    break
                except (OSError, urllib.error.URLError):
                    if server.poll() is not None or time.monotonic() >= deadline:
                        raise RuntimeError(f"Ollama startup failed; see {output / 'ollama.log'}")
                    time.sleep(0.1)
            if not any(model["name"] == "qwen3.8:27b" for model in models):
                raise RuntimeError("qwen3.8:27b is not installed; refusing to download it")
            with (output / "run.log").open("w") as log:
                try:
                    run(
                        [perf, "record", "-D", "-1", f"--control=fifo:{control}",
                         "-e", "cpu-clock:u", "-F", "499", "--call-graph", "dwarf,16384",
                         "-o", str(output / "perf.data"), "--", executable,
                         "generation_profile", "--ignored", "--nocapture", "--test-threads=1"],
                        env, 120, log,
                    )
                except RuntimeError as error:
                    # A failed generation may still have a useful CPU capture.
                    capture_error = error
        finally:
            stop(server)
            control.unlink(missing_ok=True)
            print((output / "run.log").read_text() if (output / "run.log").exists() else "No capture")
    for flag, filename in [("--no-children", "self.txt"), ("--children", "inclusive.txt")]:
        text = run(
            [perf, "report", "--stdio", "--no-inline", flag, "--call-graph", "none",
             "--sort", "symbol", "--percent-limit", "0.5", "-i", str(output / "perf.data")],
            env, 60,
        )
        demangler = shutil.which("c++filt")
        if demangler:
            result = subprocess.run(
                [demangler, "-s", "rust"], input=text, capture_output=True,
                text=True, timeout=10, check=False,
            )
            if result.returncode == 0:
                text = result.stdout
        (output / filename).write_text(text)
    print(f"Capture and reports: {output}")
    if capture_error:
        raise capture_error


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, subprocess.TimeoutExpired, KeyboardInterrupt) as error:
        print(f"Profile failed: {error}", file=sys.stderr)
        sys.exit(1)
