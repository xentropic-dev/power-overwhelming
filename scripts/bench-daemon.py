"""Measure the daemon socket path from input to PTY output."""

import argparse
import base64
import json
import math
import os
from pathlib import Path
import socket
import statistics
import subprocess
import tempfile
import time
from contextlib import contextmanager


@contextmanager
def connection(path, message):
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(5)
        client.connect(str(path))
        client.sendall(json.dumps(message).encode() + b"\n")
        with client.makefile("rb") as stream:
            yield stream


def event(stream):
    result = json.loads(stream.readline())
    if result["event"] == "error":
        raise RuntimeError(result["message"])
    return result


def request(path, message):
    with connection(path, message) as stream:
        return event(stream)


def elapsed(start):
    return (time.perf_counter() - start) * 1000


def measure(binary, plugin_dir, publish_artifacts, samples, run):
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        path = root / "power.sock"
        env = dict(os.environ, POWER_SOCKET=str(path), POWER_STATE_DIR=str(root / "state"))
        env["SHELL"] = "/bin/sh"
        env.pop("POWER_PLUGIN_DIR", None)
        if plugin_dir:
            env["POWER_PLUGIN_DIR"] = str(plugin_dir)

        start = time.perf_counter()
        daemon = subprocess.Popen([binary, "daemon"], env=env)
        try:
            while True:
                try:
                    request(path, {"request": "list"})
                    break
                except (FileNotFoundError, ConnectionRefusedError):
                    pass
                if daemon.poll() is not None or elapsed(start) > 5000:
                    raise RuntimeError(f"Daemon did not start: {binary}")
                time.sleep(0.0002)
            samples["startup"].append(elapsed(start))
            session = request(
                path, {"request": "launch", "worktree": directory, "agent": "shell"}
            )["session"]["id"]
            asset = root / "fixture.png"
            asset.touch()

            for attach in range(10):
                start = time.perf_counter()
                with connection(path, {"request": "subscribe", "session": session}) as stream:
                    if event(stream)["event"] != "snapshot":
                        raise RuntimeError("Expected a terminal snapshot")
                    samples["reattach"].append(elapsed(start))
                    if publish_artifacts:
                        request(
                            path,
                            {
                                "request": "publish",
                                "session": session,
                                "cwd": directory,
                                "target": str(asset),
                            },
                        )
                    for seq in range(5):
                        number = f"{run:04}{attach:02}{seq:02}"
                        marker = f"marker{number}".encode()
                        # The command echo does not contain the marker until printf runs.
                        command = f"printf 'marker%s\\n' {number}\n"
                        start = time.perf_counter()
                        request(
                            path,
                            {
                                "request": "input",
                                "session": session,
                                "data": base64.b64encode(command.encode()).decode(),
                            },
                        )
                        output = bytearray()
                        while marker not in output:
                            output.extend(base64.b64decode(event(stream).get("data", "")))
                        samples["input to PTY output"].append(elapsed(start))
            request(path, {"request": "end", "session": session})
        finally:
            daemon.terminate()
            try:
                daemon.wait(timeout=5)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait()


def percentile(samples, fraction):
    return sorted(samples)[math.ceil(len(samples) * fraction) - 1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--reference", type=Path)
    parser.add_argument("--plugin-dir", type=Path)
    parser.add_argument("--runs", type=int, default=30)
    parser.add_argument("--max-pty-p99-regression-ms", type=float)
    args = parser.parse_args()
    if args.runs < 1:
        parser.error("--runs must be at least one")
    if args.max_pty_p99_regression_ms is not None and args.reference is None:
        parser.error("--max-pty-p99-regression-ms requires --reference")

    runs = []
    if args.reference:
        runs.append((args.reference.resolve(), None, "reference", {}))
    runs.append(
        (
            args.binary.resolve(),
            args.plugin_dir.resolve() if args.plugin_dir else None,
            "candidate",
            {},
        )
    )
    for _, _, _, samples in runs:
        samples.update({"startup": [], "reattach": [], "input to PTY output": []})
    for run in range(args.runs):
        # Alternate ordering to spread machine load across both executables.
        for binary, plugin_dir, _, samples in runs[:: (-1 if run % 2 else 1)]:
            measure(binary, plugin_dir, args.plugin_dir is not None, samples, run)

    for _, _, label, samples in runs:
        for name, values in samples.items():
            print(
                f"{label} {name}: median {statistics.median(values):.3f} ms, "
                f"p95 {percentile(values, 0.95):.3f} ms, "
                f"p99 {percentile(values, 0.99):.3f} ms ({len(values)} samples)"
            )
    if args.max_pty_p99_regression_ms is not None:
        reference, candidate = [
            percentile(samples["input to PTY output"], 0.99) for _, _, _, samples in runs
        ]
        if candidate - reference > args.max_pty_p99_regression_ms:
            parser.exit(1, "Candidate exceeded the PTY p99 latency budget\n")


if __name__ == "__main__":
    main()
