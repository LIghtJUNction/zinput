#!/usr/bin/env python3
"""Exercise the real Xlib/XIM protocol on an explicitly private Xvfb display."""
import argparse
import json
import os
from pathlib import Path
import select
import shlex
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]

class Output:
    def __init__(self, process):
        self.process = process
        self.pending = b""
        self.lines = []
        os.set_blocking(process.stdout.fileno(), False)
    def read(self, timeout=0):
        if select.select([self.process.stdout], [], [], timeout)[0]:
            self.pending += os.read(self.process.stdout.fileno(), 65536)
            pieces = self.pending.split(b"\n")
            self.pending = pieces.pop()
            self.lines.extend(piece.decode("utf-8", "replace") for piece in pieces)
        return self.lines
    def wait(self, predicate, start=0, timeout=10):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.read(max(0, min(0.1, deadline-time.monotonic())))
            for line in self.lines[start:]:
                if predicate(line):
                    return line
            if self.process.poll() is not None:
                break
        raise AssertionError("expected protocol output; received: " + repr(self.lines))

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    if os.environ.get("ZINPUT_X11_TEST_PRIVATE") != "1" or not os.environ.get("DISPLAY") or os.environ["DISPLAY"] in {":0", ":1"}:
        raise SystemExit("run under private xvfb-run with ZINPUT_X11_TEST_PRIVATE=1")
    env = dict(os.environ)
    env["LC_ALL"] = "C.UTF-8"
    name = "zinput-protocol-" + str(os.getpid())
    env["XMODIFIERS"] = "@im=" + name
    server = client = None
    checks = []
    def passed(label):
        checks.append(label)
        print("PASS " + label, flush=True)
    def run(*command):
        return subprocess.check_output(command, env=env, text=True, timeout=10).strip()
    def key(sequence):
        subprocess.run(["xdotool", "key", "--clearmodifiers", sequence], env=env, check=True, timeout=10)
    with tempfile.TemporaryDirectory(prefix="zinput-xim-") as directory:
        binary = str(Path(directory)/"client")
        flags = shlex.split(run("pkg-config", "--cflags", "--libs", "x11", "xext"))
        subprocess.run(["cc", "-std=c11", "-O2", "-Wall", "-Wextra", "-Werror",
            str(ROOT/"tests/client.c"), *flags, "-o", binary], check=True)
        rollback = str(Path(directory)/"popup-failure")
        flags = shlex.split(run("pkg-config", "--cflags", "--libs",
            "xcb-imdkit", "xcb", "xcb-shape", "xkbcommon-x11"))
        subprocess.run(["cc", "-std=c11", "-D_DEFAULT_SOURCE", "-O2", "-Wall",
            "-Wextra", "-Werror", str(ROOT/"tests/popup_failure.c"), *flags,
            "-o", rollback], check=True)
        assert run(rollback) == "POPUP_ROLLBACK"
        passed("failed popup creation rolls back and permits a clean retry")
        try:
            server = subprocess.Popen([args.server, name], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, env=env, bufsize=0)
            so = Output(server)
            so.wait(lambda line: line == "READY")
            client = subprocess.Popen([binary], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, env=env, bufsize=0)
            co = Output(client)
            ready = co.wait(lambda line: line.startswith("READY ")).split()
            first, second = ready[1:3]
            so.wait(lambda line: line.startswith("STATE ") and line.endswith(" true"))
            passed("XOpenIM and PreeditNothing XCreateIC")
            duplicate = subprocess.run([args.server, name], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, env=env, text=True, timeout=10)
            assert duplicate.returncode != 0 and "already owns" in duplicate.stdout, duplicate.stdout
            passed("existing XIM owner rejected")
            run(binary, "--hide-registration")
            duplicate = subprocess.run([args.server, name], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, env=env, text=True, timeout=10)
            assert duplicate.returncode != 0 and "already owns" in duplicate.stdout, duplicate.stdout
            passed("owner rejected even without XIM_SERVERS property")
            key("a")
            co.wait(lambda line: line == "TEXT 0 61")
            a_code = run(binary, "--keycode", "a")
            co.wait(lambda line: line.startswith("KEY 0 R " + a_code + " "))
            passed("ordinary press and paired release forwarded")
            start = len(co.read())
            key("t")
            chinese = "你好世界".encode().hex()
            co.wait(lambda line: line == "TEXT 0 " + chinese, start)
            key("a")
            co.wait(lambda line: line == "TEXT 0 61", start)
            t_code = run(binary, "--keycode", "t")
            assert not any(line.startswith("KEY 0 R " + t_code + " ") for line in co.lines[start:]), co.lines[start:]
            passed("Chinese commit and consumed key release filtering")
            start = len(co.read())
            key("r")
            co.wait(lambda line: line == "TEXT 0 " + "かなカナ".encode().hex(), start)
            passed("kana commit round-trips COMPOUND_TEXT")
            start = len(co.read())
            key("ctrl+a")
            co.wait(lambda line: line.startswith("KEY 0 P 61 ") and int(line.split()[4]) & 4, start)
            passed("Control shortcut state preserved")
            start = len(co.read())
            key("shift+Tab")
            co.wait(lambda line: line.startswith("KEY 0 P fe20 ") and int(line.split()[4]) & 1, start)
            passed("Shift Tab state preserved")
            start = len(so.read())
            key("d")
            so.wait(lambda line: line == "DUPLICATE_REJECTED", start)
            co.wait(lambda line: line == "TEXT 0 64")
            passed("forward token cannot be reused")
            start = len(so.read())
            key("v")
            so.wait(lambda line: line == "REJECTED", start)
            passed("invalid UTF8 cursor and oversized commit rejected")
            focus = run("xdotool", "getwindowfocus")
            start = len(so.read())
            key("c")
            so.wait(lambda line: line == "POPUP Presented", start)
            popup = run("xdotool", "search", "--onlyvisible", "--name", "^zinput-x11 candidate popup$")
            assert popup and "\n" not in popup
            assert run(binary, "--inspect-popup", popup) == "INPUT_TRANSPARENT"
            assert run("xdotool", "getwindowfocus") == focus
            geometry = dict(line.split("=", 1) for line in run("xdotool", "getwindowgeometry", "--shell", popup).splitlines())
            assert (int(geometry["X"]), int(geometry["Y"]), int(geometry["WIDTH"]), int(geometry["HEIGHT"])) == (60,84,80,30), geometry
            passed("popup uses IC spot and has no input region or focus change")
            start = len(so.read())
            run("xdotool", "windowfocus", "--sync", second)
            so.wait(lambda line: line.startswith("STATE ") and line.endswith(" true"), start)
            start = len(co.read())
            key("t")
            co.wait(lambda line: line == "TEXT 1 " + chinese, start)
            assert not any(line == "TEXT 0 " + chinese for line in co.lines[start:])
            passed("focus generation routes commit to the new XIC")
            start = len(so.read())
            key("c")
            so.wait(lambda line: line == "POPUP Presented", start)
            reset_state_start = len(so.read())
            start = len(co.read())
            key("F6")
            co.wait(lambda line: line == "RESET 1", start)
            so.wait(lambda line: line.startswith("STATE ") and line.endswith(" true"), reset_state_start)
            invisible = subprocess.run(["xdotool", "search", "--onlyvisible", "--name", "^zinput-x11 candidate popup$"], env=env, capture_output=True, text=True)
            assert not invisible.stdout.strip(), invisible.stdout
            passed("XResetIC invalidates composition and hides popup")
            start = len(co.read())
            key("t")
            co.wait(lambda line: line == "TEXT 1 " + chinese, start)
            passed("commit remains usable after reset")
            destroy_state_start = len(so.read())
            start = len(co.read())
            key("F7")
            co.wait(lambda line: line == "DESTROY 1", start)
            so.wait(lambda line: line.startswith("STATE ") and line.endswith(" false"), destroy_state_start)
            assert server.poll() is None
            passed("XIC destruction preserves server lifetime")
            report = {"checks": checks, "passed": len(checks), "scope": "real Xlib client and XIM server on private Xvfb",
                "server": str(Path(args.server).resolve()), "styles": ["PreeditNothing|StatusNothing"],
                "popup": {"override_redirect": True, "input_shape_rectangles": 0, "spot_geometry": [60,84,80,30]}}
            output = Path(args.output)
            output.parent.mkdir(parents=True, exist_ok=True)
            output.write_text(json.dumps(report, indent=2) + "\n")
            print(json.dumps(report))
        finally:
            for process in [client, server]:
                if process is not None and process.poll() is None:
                    process.terminate()
                    try: process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill(); process.wait()
if __name__ == "__main__":
    main()
