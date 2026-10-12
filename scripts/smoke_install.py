#!/usr/bin/env python3
"""Check installation diagnostics in an empty home with synthetic login data."""

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


def write(path, text, executable=False):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    path.chmod(0o700 if executable else 0o600)


def run(binary, command, environment, expected_status):
    result = subprocess.run(
        [str(binary), command], env=environment, text=True, capture_output=True, timeout=30
    )
    if result.returncode != expected_status:
        raise AssertionError(f"{command} exited {result.returncode}:\n{result.stdout}\n{result.stderr}")
    return result.stdout + result.stderr


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path, help="path to the built redwax binary")
    parser.add_argument(
        "--install-resolver", action="store_true",
        help="also download and install the pinned real solver in the temporary home",
    )
    options = parser.parse_args()
    binary = options.binary.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="redwax-install-smoke-") as temporary:
        home = Path(temporary)
        environment = {
            key: value for key, value in os.environ.items()
            if not key.startswith(("REDWAX_", "XDG_"))
        }
        environment.update(
            HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
            XDG_CACHE_HOME=str(home / "cache"), XDG_DATA_HOME=str(home / "data"),
        )
        programs = home / "programs"
        write(programs / "mpv", '#!/bin/sh\necho "mpv smoke"\n', True)
        write(programs / "node", '#!/bin/sh\necho "v22.0.0"\n', True)
        write(programs / "yt-dlp", '#!/bin/sh\ncase "$*" in\n*--help*) echo "--cookies-from-browser --load-info-json --js-runtimes --remote-components" ;;\n*) echo "2026.08.19" ;;\nesac\n', True)
        environment["PATH"] = str(programs)

        missing_login = run(binary, "setup", environment, 1)
        assert "not signed in" in missing_login
        assert "solver library or core is missing" in missing_login

        config = home / "config/redwax"
        write(config / "config.toml", 'icons = "text"\nart = "halfcell"\n')
        synthetic_cookie = "SAPISID=synthetic-smoke"
        write(config / "browser.json", json.dumps({
            "cookie": synthetic_cookie,
            "x-goog-authuser": "0",
        }))
        write(config / "cookies.txt", '# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tTRUE\t1900000000\tSAPISID\tsynthetic-smoke\n.youtube.com\tTRUE\t/\tTRUE\t1900000000\tLOGIN_INFO\tsynthetic-login\n')
        missing_solver = run(binary, "setup", environment, 1)
        assert "saved login found" in missing_solver
        assert "Next, sign in:" not in missing_solver

        managed = home / ".local/share/redwax/yt-dlp"
        package = managed / "lib/python-smoke/site-packages/yt_dlp_ejs/yt/solver"
        write(managed / "bin/yt-dlp", (programs / "yt-dlp").read_text(), True)
        write(package / "lib.min.js", "const lib = {};\n")
        write(package / "core.min.js", "function jsc() { return {}; }\n")
        ready = run(binary, "setup", environment, 0)
        assert "native signature solver installed" in ready
        assert "optional and absent" in ready
        doctor = run(binary, "doctor", environment, 0)
        assert str(managed / "bin/yt-dlp") in doctor
        assert str(package / "lib.min.js") in doctor
        assert "proof tokens" in doctor and "optional provider absent" in doctor

        write(package / "core.min.js", "")
        incomplete = run(binary, "setup", environment, 1)
        assert "solver library or core is missing" in incomplete
        if options.install_resolver:
            shutil.rmtree(managed)
            environment["PATH"] = os.environ["PATH"]
            installer = Path(__file__).resolve().with_name("install_resolver.sh")
            subprocess.run([str(installer)], env=environment, check=True, timeout=300)
            run(binary, "setup", environment, 0)
            installed = run(binary, "doctor", environment, 0)
            assert str(managed / "bin/yt-dlp") in installed
        print("Clean-home installation smoke passed. Live login and playback need a real account.")


if __name__ == "__main__":
    main()
