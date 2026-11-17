"""Managed-environment provider: runs hosted environments as Modal Sandboxes.

The Agents API service starts this process and talks to it in JSON lines on
stdin and stdout. Each request carries an `id` that its response echoes;
requests run concurrently, so responses may arrive out of order.

  {"id": 1, "op": "provision", "environment_id": "env_…", "registry_url": "https://…/registry",
   "token": "…", "container_size": "medium", "packages": {"system": [], "python": [], "npm": []},
   "env": {"KEY": "value"}, "files": [{"path": "/workspace/…", "data": "<base64>"}],
   "setup_commands": [{"command": "…", "cwd": "/workspace"}],
   "network": {"access": "restricted", "allowed_domains": ["pypi.org"]}, "lifetime_seconds": 86400}
  → {"id": 1, "ok": true, "sandbox_id": "sb-…"}

  {"id": 2, "op": "destroy", "sandbox_id": "sb-…"}   → {"id": 2, "ok": true}
  {"id": 3, "op": "status", "sandbox_id": "sb-…"}    → {"id": 3, "ok": true, "running": true}
  {"id": 4, "op": "snapshot", "sandbox_id": "sb-…", "ttl_seconds": 604800}
  → {"id": 4, "ok": true, "snapshot_id": "im-…"}

A provision request with a "snapshot_id" restores that snapshot instead of
installing packages, writing files, and running setup; it still applies the
network policy and starts the executor.

A failure answers {"id": n, "ok": false, "error": "…"}. A provisioned Sandbox
installs the requested packages, writes the files, runs the setup commands in
order in `/workspace` with full network access, then applies the network
policy (Modal's outbound domain allowlist; the registry's host stays reachable
so the executor can connect), and finally runs the stock `codex exec-server`,
which dials the service's registry with the environment's own connect token.
The `env` values reach the whole Sandbox as a Modal secret, for its commands;
the token reaches only the executor's process, never setup or the command
line. The service binds the token to the first executor that registers.

Usage: provider.py --image-id IM_ID   (the image `serve.py build` prints)
       provider.py --build            (build the image from this checkout's HEAD)
"""

import argparse
import base64
import json
import posixpath
import sys
import tempfile
import threading
from concurrent.futures import ThreadPoolExecutor
from urllib.parse import urlsplit

import modal

APP = "agents-api-environments"
WORKSPACE = "/workspace"
CODEX = "/opt/agents-api/bin/codex"

# CPU cores and memory (MiB) for each public container size.
SIZES = {"small": (1, 2048), "medium": (2, 4096), "large": (4, 8192)}

# Run after setup: the executor, with its token from its own Modal secret.
EXECUTOR = """
set -euo pipefail
mkdir -p {workspace}
cd {workspace}
exec env CODEX_API_KEY="$AGENTS_API_ENVIRONMENT_TOKEN" {codex} exec-server \\
  --remote "$AGENTS_API_REGISTRY_URL" --environment-id "$AGENTS_API_ENVIRONMENT_ID" {trusted} \\
  > /tmp/executor.log 2>&1
"""


def packaged(base, packages):
    """`base` with the requested packages installed. Modal caches each layer by
    its contents, so sessions asking for the same packages share them."""
    image = base
    system = packages.get("system") or []
    npm = packages.get("npm") or []
    python = packages.get("python") or []
    if system or npm:
        image = image.apt_install(*system, *(["nodejs", "npm"] if npm else []))
    if python:
        image = image.pip_install(*python)
    if npm:
        image = image.run_commands("npm install --global " + " ".join(npm))
    return image


class Provider:
    def __init__(self, base):
        self.base = base
        self.app = modal.App.lookup(APP, create_if_missing=True)

    def provision(self, request):
        cpu, memory = SIZES[request.get("container_size") or "medium"]
        registry = request["registry_url"]
        host = urlsplit(registry).hostname or ""
        # The stock CLI sends its key only to OpenAI or loopback hosts unless
        # the registry's host is trusted explicitly.
        trusted = "" if host.endswith(("openai.com", "openai.org")) else f"--trusted-registry-host {host}"
        # The session's own variables are for its commands; the connect
        # token reaches only the executor's process, never setup or PID 1.
        secret = modal.Secret.from_dict(request.get("env") or {})
        executor_secret = modal.Secret.from_dict(
            {
                "AGENTS_API_ENVIRONMENT_TOKEN": request["token"],
                "AGENTS_API_REGISTRY_URL": registry,
                "AGENTS_API_ENVIRONMENT_ID": request["environment_id"],
            }
        )
        network = request.get("network") or {"access": "enabled"}
        # A Sandbox created with open networking cannot be restricted later,
        # so one that will be starts with an explicit allow-all policy.
        restricted = network["access"] != "enabled"
        policy = (
            {"outbound_cidr_allowlist": ["0.0.0.0/0"], "outbound_domain_allowlist": ["*"]}
            if restricted
            else {}
        )
        snapshot = request.get("snapshot_id")
        image = modal.Image.from_id(snapshot) if snapshot else packaged(self.base, request.get("packages") or {})
        sandbox = modal.Sandbox.create(
            **policy,
            app=self.app,
            image=image,
            secrets=[secret],
            cpu=cpu,
            memory=memory,
            timeout=int(request.get("lifetime_seconds") or 24 * 3600),
            workdir=WORKSPACE,
            tags={"agents-api-environment": request["environment_id"]},
        )
        sandbox_id = sandbox.object_id
        try:
            sandbox.exec("mkdir", "-p", WORKSPACE).wait()
            for file in [] if snapshot else request.get("files") or []:
                sandbox.exec("mkdir", "-p", posixpath.dirname(file["path"])).wait()
                sandbox.filesystem.write_bytes(base64.b64decode(file["data"]), file["path"])
            for step in [] if snapshot else request.get("setup_commands") or []:
                process = sandbox.exec(
                    "bash", "-lc", step["command"], workdir=step.get("cwd") or WORKSPACE
                )
                code = process.wait()
                if code != 0:
                    # Setup output can echo confidential values, so only the
                    # exit status is reported.
                    raise RuntimeError(f"a setup command exited with status {code}")
            if restricted:
                # Applied before the executor starts, so its connection to the
                # registry is never cut. No CIDR is allowed on its own.
                allowed = [host, *(network.get("allowed_domains") or [])]
                sandbox._experimental_set_outbound_network_policy(
                    outbound_cidr_allowlist=[], outbound_domain_allowlist=allowed
                )
            sandbox.exec(
                "bash",
                "-c",
                EXECUTOR.format(workspace=WORKSPACE, codex=CODEX, trusted=trusted),
                secrets=[executor_secret],
            )
        except Exception:
            sandbox.terminate()
            raise
        finally:
            # Each handle holds a connection until detached; this process
            # outlives many Sandboxes.
            sandbox.detach()
        return {"sandbox_id": sandbox_id}

    def destroy(self, request):
        sandbox = modal.Sandbox.from_id(request["sandbox_id"])
        try:
            sandbox.terminate()
        finally:
            sandbox.detach()
        return {}

    def snapshot(self, request):
        sandbox = modal.Sandbox.from_id(request["sandbox_id"])
        try:
            image = sandbox.snapshot_filesystem(ttl=int(request.get("ttl_seconds") or 7 * 24 * 3600))
            return {"snapshot_id": image.object_id}
        finally:
            sandbox.detach()

    def status(self, request):
        sandbox = modal.Sandbox.from_id(request["sandbox_id"])
        try:
            return {"running": sandbox.poll() is None}
        finally:
            sandbox.detach()


def main():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--image-id", help="the image `serve.py build` printed")
    group.add_argument("--build", action="store_true", help="build the image from HEAD")
    args = parser.parse_args()
    if args.image_id:
        base = modal.Image.from_id(args.image_id)
    else:
        import serve

        base = serve.image(serve.archive(tempfile.mkdtemp()))
    provider = Provider(base)
    lock = threading.Lock()

    def answer(response):
        with lock:
            sys.stdout.write(json.dumps(response) + "\n")
            sys.stdout.flush()

    def handle(request):
        try:
            if request.get("op") not in ("provision", "destroy", "status", "snapshot"):
                raise ValueError(f"unknown op {request.get('op')!r}")
            result = getattr(provider, request["op"])(request)
            answer({"id": request["id"], "ok": True, **result})
        except Exception as error:  # Reported to the service, never fatal.
            answer({"id": request.get("id"), "ok": False, "error": str(error)})

    with ThreadPoolExecutor(max_workers=32) as pool:
        for line in sys.stdin:
            if line.strip():
                pool.submit(handle, json.loads(line))


if __name__ == "__main__":
    main()
