#!/usr/bin/env python3
"""Build the reference engine and test its generated ABI on the current OS."""

import json
import os
from pathlib import Path
import subprocess
import sys


def main():
    crate = Path(__file__).resolve().parent.parent
    extra = sys.argv[1:]
    built = subprocess.run(
        ["cargo", "build", "-p", "zinput", "--features", "plugins", "--example",
         "ime_engine", "--message-format=json", *extra],
        cwd=crate, text=True, stdout=subprocess.PIPE, check=False,
    )
    library = None
    for line in built.stdout.splitlines():
        message = json.loads(line)
        if message.get("reason") == "compiler-message":
            rendered = message.get("message", {}).get("rendered")
            if rendered:
                print(rendered, file=sys.stderr, end="")
        if (message.get("reason") == "compiler-artifact"
                and message.get("target", {}).get("name") == "ime_engine"):
            for name in message.get("filenames", []):
                if Path(name).suffix in (".so", ".dylib", ".dll"):
                    library = Path(name).resolve()
    if built.returncode:
        return built.returncode
    if library is None or not library.is_file():
        raise RuntimeError("Cargo did not emit the ime_engine native library")
    env = os.environ.copy()
    env["ZINPUT_TEST_PLUGIN"] = str(library)
    print(f"Testing native engine: {library}", flush=True)
    return subprocess.run(
        ["cargo", "test", "-p", "zinput", "--features", "plugins", "--test",
         "plugin_native", *extra, "--", "--ignored"],
        cwd=crate, env=env, check=False,
    ).returncode


if __name__ == "__main__":
    sys.exit(main())
