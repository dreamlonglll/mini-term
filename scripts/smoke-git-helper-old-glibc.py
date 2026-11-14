#!/usr/bin/env python3
"""Run inside a native-architecture glibc < 2.34 container with system Git.

Usage: python3 scripts/smoke-git-helper-old-glibc.py /path/to/mt-git-helper
The helper and this script may be read-only bind mounts; fixtures live in /tmp.
"""
import json
import os
import platform
import signal
import struct
import subprocess
import sys
import tempfile
from pathlib import Path


def timeout(signum, frame):
    raise TimeoutError("Helper framing smoke exceeded 60 seconds")


def smoke(binary):
    libc, version = platform.libc_ver()
    assert libc == "glibc" and tuple(map(int, version.split("."))) < (2, 34), (
        "This compatibility gate requires glibc < 2.34, got %s %s" % (libc, version)
    )
    print("Compatibility runtime: %s %s; %s" % (
        libc, version, subprocess.check_output(["git", "--version"], text=True).strip()
    ), flush=True)
    signal.signal(signal.SIGALRM, timeout)
    signal.alarm(60)
    with tempfile.TemporaryDirectory(prefix="mt-helper-old-glibc-") as directory:
        root = Path(directory)
        repo = root / "repo"
        env = dict(os.environ, HOME=directory, XDG_CONFIG_HOME=directory,
                   GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null",
                   GIT_TERMINAL_PROMPT="0", GIT_EDITOR="true")
        for key in ("GIT_DIR", "GIT_COMMON_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE",
                    "GIT_CONFIG_PARAMETERS", "GIT_CONFIG_COUNT"):
            env.pop(key, None)

        def git(*args):
            return subprocess.check_output(["git", "-C", str(repo), *args], env=env, text=True)

        repo.mkdir()
        git("init", "-q")
        git("config", "user.name", "Helper smoke")
        git("config", "user.email", "helper-smoke@example.invalid")
        (repo / "tracked.txt").write_text("before\n")
        git("add", "tracked.txt")
        git("commit", "-qm", "initial")
        (repo / "tracked.txt").write_text("after\n")
        (repo / "staged.txt").write_text("staged\n")
        git("add", "staged.txt")
        (repo / "untracked.txt").write_text("untracked\n")
        assert set(git("status", "--porcelain").splitlines()) == {
            " M tracked.txt", "A  staged.txt", "?? untracked.txt"
        }
        child = subprocess.Popen([binary, "--root", directory], env=env,
                                 stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        try:
            def exact(length):
                data = b""
                while len(data) < length:
                    part = child.stdout.read(length - len(data))
                    assert part, "Unexpected helper EOF"
                    data += part
                return data

            def frame():
                length, = struct.unpack(">I", exact(4))
                assert 0 < length <= 16 * 1024 * 1024, "Invalid helper frame size"
                return json.loads(exact(length))

            hello = frame()
            assert hello["protocolVersion"] == 5 and hello["helperVersion"], hello

            def request(identifier, operation):
                payload = json.dumps({"protocolVersion": 5, "id": identifier,
                                      "operation": operation}).encode()
                child.stdin.write(struct.pack(">I", len(payload)) + payload)
                child.stdin.flush()
                response = frame()
                assert response["protocolVersion"] == 5 and response["id"] == identifier, response
                assert set(response["result"]) == {"Ok"}, response
                return response["result"]["Ok"]

            discovered = request(1, "discover")
            assert {item["path"] for item in discovered["repositories"]["repos"]} == {str(repo)}, discovered
            status = request(2, {"status": {"repo": str(repo)}})
            files = {item["path"]: item for item in status["status"]["files"]}
            assert set(files) == {"tracked.txt", "staged.txt", "untracked.txt"}, status
            assert files["tracked.txt"]["unstagedStatus"] == "modified", status
            assert files["staged.txt"]["stagedStatus"] == "added", status
            assert files["untracked.txt"]["unstagedStatus"] == "untracked", status
            child.stdin.close()
            assert child.stdout.read() == b"", "Unexpected trailing protocol output"
            assert child.wait(timeout=10) == 0, "Helper failed on clean protocol EOF"
        finally:
            if child.poll() is None:
                child.kill()
            child.wait()
        signal.alarm(0)
        print("PASS: real framed discover/status with tracked, staged and untracked files", flush=True)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("Usage: smoke-git-helper-old-glibc.py <helper-binary>")
    smoke(str(Path(sys.argv[1]).resolve(strict=True)))
