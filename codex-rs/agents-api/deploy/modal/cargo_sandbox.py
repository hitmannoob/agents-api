"""Build and test the workspace in a Modal Sandbox instead of on this machine.

One Sandbox is created and reused across calls. Cargo's target directory and
registry live on Modal Volumes, so a new Sandbox rebuilds incrementally. The
working tree (tracked and untracked files, without ignored ones) is copied in
before each command, so uncommitted changes are what gets built.

Usage:
  cargo_sandbox.py up                 create the Sandbox, or reuse the running one
  cargo_sandbox.py sync               copy the working tree into the Sandbox
                                      (--head: the committed HEAD instead)
  cargo_sandbox.py run -- ARGS...     sync, then run `cargo ARGS...` in codex-rs,
                                      streaming its output; exits with cargo's status
  cargo_sandbox.py sh -- COMMAND      run a shell command in codex-rs (after a sync)
  cargo_sandbox.py status             show the Sandbox and every running one in the app
  cargo_sandbox.py stop               terminate the Sandbox

Examples:
  cargo_sandbox.py run -- build -p codex-code-mode-host -p codex-app-server -p codex-agents-api --bins
  cargo_sandbox.py run -- test -p codex-agents-api
  cargo_sandbox.py run --no-sync -- test -p codex-agents-api --lib
  cargo_sandbox.py run --head -- test -p codex-agents-api    (baseline at HEAD)

Options go before `--`; everything after it is the command.

The Sandbox has 16 CPUs and 32 GiB and terminates itself after --hours (default
3, at most 6) even if `stop` is never called. Its ID is kept in
~/.agents-api-cargo-sandbox.json. Run with the Python that has the `modal`
package installed and authenticated.
"""

import argparse
import io
import json
import os
import pathlib
import shlex
import subprocess
import sys
import tarfile

import modal

APP = "agents-api-cargo"
STATE = pathlib.Path.home() / ".agents-api-cargo-sandbox.json"
TARGET_VOLUME = "agents-api-cargo-target"
REGISTRY_VOLUME = "agents-api-cargo-registry"
TARGET = "/cache/target"
# The rust image's CARGO_HOME is /usr/local/cargo.
REGISTRY = "/usr/local/cargo/registry"
SOURCE = "/work/codex"
MAX_HOURS = 6

REPO = pathlib.Path(
    subprocess.run(
        ["git", "rev-parse", "--show-toplevel"],
        cwd=os.path.dirname(os.path.abspath(__file__)),
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
)

# Fetch the prebuilt V8 that codex-code-mode-host links, as deploy/modal/serve.py
# does, checked against the repository's trusted checksums. It is kept on the
# target volume per version, so it is downloaded once.
V8 = """
version="$(python3 .github/scripts/rusty_v8_bazel.py resolved-v8-crate-version)"
case "$(uname -m)" in
  x86_64) target=x86_64-unknown-linux-gnu ;;
  aarch64) target=aarch64-unknown-linux-gnu ;;
  *) echo "Unsupported V8 build architecture: $(uname -m)" >&2; exit 1 ;;
esac
profile=ptrcomp_sandbox_release
base_url="https://github.com/openai/codex/releases/download/rusty-v8-v${version}"
binding_dir="${CARGO_TARGET_DIR}/rusty_v8/${version}"
archive_name="librusty_v8_${profile}_${target}.a.gz"
binding_name="src_binding_${profile}_${target}.rs"
checksums_name="rusty_v8_${profile}_${target}.sha256"
if [ ! -s "${binding_dir}/.verified" ]; then
  trusted_checksums="third_party/v8/rusty_v8_${version//./_}_release_manifests.sha256"
  expected="$(awk -v name="${checksums_name}" '$2 == name { print $1 }' "${trusted_checksums}")"
  if [[ ! "${expected}" =~ ^[0-9a-f]{64}$ ]]; then
    echo "No unique trusted V8 checksum for ${target} in ${trusted_checksums}" >&2
    exit 1
  fi
  mkdir -p "${binding_dir}"
  curl -fsSL "${base_url}/${checksums_name}" -o "${binding_dir}/${checksums_name}"
  printf '%s  %s\\n' "${expected}" "${binding_dir}/${checksums_name}" | sha256sum --check -
  curl -fsSL "${base_url}/${archive_name}" -o "${binding_dir}/${archive_name}"
  curl -fsSL "${base_url}/${binding_name}" -o "${binding_dir}/${binding_name}"
  if [[ "$(wc -l < "${binding_dir}/${checksums_name}")" -ne 2 ]]; then
    echo "Expected exactly two V8 checksums for ${target}" >&2
    exit 1
  fi
  (cd "${binding_dir}" && tr -d '\\r' < "${checksums_name}" | sha256sum --check -)
  echo ok > "${binding_dir}/.verified"
fi
export RUSTY_V8_ARCHIVE="${binding_dir}/${archive_name}" RUSTY_V8_SRC_BINDING_PATH="${binding_dir}/${binding_name}"
"""

# A non-login shell: a login shell resets PATH and loses the Rust toolchain.
PRELUDE = f"""
set -euo pipefail
exec 2>&1
export CARGO_HOME=/usr/local/cargo RUSTUP_HOME=/usr/local/rustup PATH=/usr/local/cargo/bin:$PATH
export CARGO_TARGET_DIR={TARGET} CARGO_INCREMENTAL=0 CARGO_TERM_COLOR=never
# Some API tests recurse deeply on the default 2 MiB test-thread stack.
export RUST_MIN_STACK=8388608
cd {SOURCE}
{V8}
cd codex-rs
"""


def image():
    """Rust 1.95 (the repository's toolchain) with the build's system packages,
    as in deploy/modal/serve.py."""
    return modal.Image.from_registry("rust:1.95-bookworm", add_python="3.12").apt_install(
        "pkg-config",
        "libssl-dev",
        "libcap-dev",
        "libdbus-1-dev",
        "clang",
        "cmake",
        "git",
        "python3",
        "procps",
        "curl",
    )


def app():
    return modal.App.lookup(APP, create_if_missing=True)


def load():
    try:
        return json.loads(STATE.read_text())
    except (FileNotFoundError, json.JSONDecodeError):
        return None


def running(sandbox_id):
    """The Sandbox with this ID if it is still running, else None."""
    try:
        sandbox = modal.Sandbox.from_id(sandbox_id)
    except Exception:  # Unknown or expired.
        return None
    return sandbox if sandbox.poll() is None else None


def up(hours):
    state = load()
    if state and (sandbox := running(state["sandbox_id"])):
        return sandbox
    hours = min(hours, MAX_HOURS)
    volumes = {
        TARGET: modal.Volume.from_name(TARGET_VOLUME, create_if_missing=True, version=2),
        REGISTRY: modal.Volume.from_name(REGISTRY_VOLUME, create_if_missing=True, version=2),
    }
    with modal.enable_output():
        sandbox = modal.Sandbox.create(
            "sleep",
            "infinity",
            app=app(),
            image=image(),
            volumes=volumes,
            cpu=16,
            memory=32768,
            timeout=int(hours * 3600),
        )
    STATE.write_text(json.dumps({"sandbox_id": sandbox.object_id, "app": APP}))
    print("sandbox", sandbox.object_id, f"(terminates itself after {hours} h)", flush=True)
    return sandbox


def exec_streaming(sandbox, script):
    """Run a bash script, printing its combined output as it arrives."""
    process = sandbox.exec("bash", "-c", script, timeout=MAX_HOURS * 3600)
    for line in process.stdout:
        print(line, end="", flush=True)
    return process.wait()


def sync(sandbox, head=False):
    """Copy tracked and untracked, not ignored, files, as tar. Modification
    times are kept, so cargo rebuilds only what changed. With `head`, copy the
    committed HEAD instead (`git archive`), for a baseline."""
    if head:
        data = subprocess.run(
            ["git", "archive", "--format=tar.gz", "HEAD"], cwd=REPO, check=True, capture_output=True
        ).stdout
        return unpack(sandbox, data)
    names = subprocess.run(
        ["git", "ls-files", "-co", "--exclude-standard", "-z"],
        cwd=REPO,
        check=True,
        capture_output=True,
    ).stdout.decode().split("\0")
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w:gz") as tar:
        for name in filter(None, names):
            path = REPO / name
            # A deleted file that is still tracked is not sent.
            if path.exists() or path.is_symlink():
                tar.add(path, arcname=name, recursive=False)
    return unpack(sandbox, buffer.getvalue())


def unpack(sandbox, data):
    sandbox.filesystem.write_bytes(data, "/tmp/codex.tar.gz")
    status = exec_streaming(
        sandbox,
        f"set -e; rm -rf {SOURCE}.new && mkdir -p {SOURCE}.new && "
        f"tar -xzf /tmp/codex.tar.gz -C {SOURCE}.new && rm -rf {SOURCE} && "
        f"mv {SOURCE}.new {SOURCE} && rm /tmp/codex.tar.gz",
    )
    if status != 0:
        sys.exit(f"sync failed: {status}")
    print(f"synced {len(data) // 1024} KiB", flush=True)


def stop():
    state = load()
    if state and (sandbox := running(state["sandbox_id"])):
        sandbox.terminate()
        print("terminated", sandbox.object_id)
    else:
        print("no running sandbox recorded")
    STATE.unlink(missing_ok=True)


def status():
    state = load()
    print("recorded:", state["sandbox_id"] if state else None)
    left = list(modal.Sandbox.list(app_id=app().app_id))
    print(f"running in app {APP}: {len(left)}")
    for sandbox in left:
        print(" ", sandbox.object_id)


def main():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("command", choices=("up", "sync", "run", "sh", "status", "stop"))
    parser.add_argument("--hours", type=float, default=3, help=f"Sandbox lifetime (default 3, max {MAX_HOURS})")
    parser.add_argument("--no-sync", action="store_true", help="run without copying the tree first")
    parser.add_argument("--head", action="store_true", help="copy the committed HEAD, not the working tree")
    # Everything after `--` is the command, so its own options never reach
    # this parser.
    argv = sys.argv[1:]
    split = argv.index("--") if "--" in argv else len(argv)
    args = parser.parse_args(argv[:split])
    rest = argv[split + 1 :]
    if args.command == "stop":
        return stop()
    if args.command == "status":
        return status()
    sandbox = up(args.hours)
    if args.command == "up":
        return
    if args.command == "sync" or not args.no_sync:
        sync(sandbox, head=args.head)
    if args.command == "sync":
        return
    if not rest:
        sys.exit(f"{args.command} needs arguments after --")
    command = " ".join(shlex.quote(arg) for arg in rest)
    if args.command == "run":
        command = "cargo " + command
    else:
        command = " ".join(rest)
    sys.exit(exec_streaming(sandbox, PRELUDE + command + "\n"))


if __name__ == "__main__":
    main()
