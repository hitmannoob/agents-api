"""Real-provider interoperability with an executor on separate caller compute.

The service comes from serve.py's STATE_DIR. Only the executor runs in a new
Modal Sandbox, connecting through the public https tunnel without rewriting
the returned remote_url. This covers inventory row BEH-018.

Usage: python executor_interop.py STATE_DIR [--model M] [--json out.json]
Requires modal and the pinned SDK (openai==3.17.0). Outcomes match
tests/real_provider.py: PASS, FAIL (contract/harness), MODEL (prompt miss), and
SKIP (missing prerequisite). Exits non-zero only on FAIL.
"""

import argparse
import json
import os
import sys
import time
from urllib.parse import urlsplit

import modal
import openai

import serve

CONNECT_TIMEOUT = 60
TURN_TIMEOUT = 240
SANDBOX_TIMEOUT = 15 * 60


class Fail(Exception):
    """The service or executor did not meet the contract."""


class Model(Exception):
    """The model did not follow the prompt."""


class Skip(Exception):
    """A prerequisite is missing."""


def expect(condition, message):
    if not condition:
        raise Fail(message)


class Run:
    def __init__(self, state, model):
        self.state, self.model = state, model
        self.client = self.session = self.sandbox = self.executor = None
        self.uploaded = self.queued_turn_id = None
        self.credentials = {}

    def read(self, name):
        try:
            with open(os.path.join(self.state, name), encoding="utf-8") as handle:
                value = handle.read().strip()
        except FileNotFoundError:
            raise Skip(f"STATE_DIR/{name} is missing") from None
        if not value:
            raise Skip(f"STATE_DIR/{name} is empty")
        if name in ("token", "environment-key"):
            self.credentials[name] = value
        return value

    def detail(self, value):
        text = str(value)
        for secret in self.credentials.values():
            text = text.replace(secret, "[redacted]")
        return " ".join(text.splitlines())

    @property
    def sessions(self):
        return self.client.beta.agents.sessions

    @property
    def environments(self):
        return self.client.beta.agents.environments

    def create(self):
        if openai.__version__ != "3.17.0":
            raise Skip("requires the pinned SDK openai==3.17.0")
        url = self.read("url").rstrip("/")
        token = self.read("token")
        self.read("environment-key")
        self.service_sandbox_id = self.read("sandbox-id")
        self.host = urlsplit(url).hostname
        expect(url.startswith("https://") and self.host, "STATE_DIR/url must be https")
        self.client = openai.OpenAI(
            base_url=f"{url}/v1",
            api_key=token,
            max_retries=0,
            timeout=60,
            _strict_response_validation=True,
        )
        self.session = self.sessions.create(
            agent={"model": self.model},
            environment={"type": "self_hosted", "workspace_directory": "/work"},
        )
        environment = self.session.environment
        expect(environment.type == "self_hosted", "environment is not self_hosted")
        expect(environment.workspace_directory == "/work", "wrong workspace directory")
        # Do not fix up http here: this is the public-tunnel URL regression check.
        expect(
            environment.remote_url.startswith("https://")
            and urlsplit(environment.remote_url).hostname == self.host,
            f"remote_url must use https on the tunnel host: {environment.remote_url!r}",
        )
        return "self_hosted /work; remote_url uses https on the tunnel host"

    def command(self, *args):
        expect(self.sandbox.poll() is None, "executor Sandbox exited")
        process = self.sandbox.exec(*args, timeout=30)
        process.wait()
        return process.returncode, process.stdout.read(), process.stderr.read()

    def start_executor(self):
        self.executor = self.sandbox.exec(
            "/opt/agents-api/bin/codex",
            "exec-server",
            "--remote",
            self.session.environment.remote_url,
            "--environment-id",
            self.session.environment.id,
            "--trusted-registry-host",
            self.host,
            workdir="/work",
            timeout=SANDBOX_TIMEOUT,
        )

    def wait_environment(self, status):
        deadline = time.monotonic() + CONNECT_TIMEOUT
        while True:
            expect(self.sandbox.poll() is None, "executor Sandbox exited")
            if status == "connected":
                code = self.executor.poll()
                expect(code is None, f"exec-server exited with code {code}")
            current = self.environments.retrieve(self.session.environment.id)
            if current.status == status:
                return
            expect(
                time.monotonic() < deadline,
                f"environment did not become {status} in {CONNECT_TIMEOUT}s: {current.status}",
            )
            time.sleep(0.5)

    def connect(self):
        self.sandbox = modal.Sandbox.create(
            "sleep",
            "infinity",
            app=modal.App.lookup(serve.APP, create_if_missing=True),
            image=serve.image(serve.archive(self.state)),
            secrets=[
                modal.Secret.from_dict(
                    {"CODEX_API_KEY": self.credentials["environment-key"]}
                )
            ],
            cpu=2,
            timeout=SANDBOX_TIMEOUT,
        )
        expect(
            self.sandbox.object_id != self.service_sandbox_id,
            "executor must run in its own Sandbox",
        )
        code, _, _ = self.command("mkdir", "-p", "/work")
        expect(code == 0, "could not create /work in the executor Sandbox")
        self.start_executor()
        self.wait_environment("connected")
        return "separate executor Sandbox connected through the public tunnel"

    def message(self, text):
        self.sessions.events.create(
            self.session.id,
            events=[
                {
                    "type": "agent.session.input.message",
                    "input": [
                        {
                            "role": "user",
                            "content": [{"type": "input_text", "text": text}],
                        }
                    ],
                }
            ],
        )

    def settle(self, turns=1):
        """Match real_provider.py: count finished turns, not just idle status."""
        deadline = time.monotonic() + TURN_TIMEOUT
        while True:
            session = self.sessions.retrieve(self.session.id)
            finished = [
                turn
                for turn in self.sessions.turns.list(self.session.id)
                if turn.status in ("completed", "failed", "cancelled")
            ]
            if len(finished) >= turns and session.status != "in_progress":
                expect(session.status == "idle", f"session is {session.status}")
                for turn in finished:
                    expect(
                        turn.status == "completed",
                        f"turn {turn.id} {turn.status}: {turn.error}",
                    )
                return session, finished
            expect(
                time.monotonic() < deadline,
                f"session did not settle in {TURN_TIMEOUT}s: {session.status}",
            )
            time.sleep(1)

    def result(self, path, expected):
        code, content, _ = self.command("cat", path)
        if code != 0 or content.strip() != expected:
            raise Model(f"{path} does not hold {expected!r}")

    def task(self):
        data = b"a,b\n1,10\n2,32\n3,0\n4,58\n"
        self.uploaded = self.client.files.create(
            file=("data.csv", data), purpose="user_data"
        )
        copied = self.environments.files.create(
            self.session.environment.id,
            type="file_id",
            path="/work/data.csv",
            file_id=self.uploaded.id,
        )
        expect(copied.size_bytes == len(data), "environment-files copy has wrong size")
        self.client.files.delete(self.uploaded.id)
        self.uploaded = None
        self.message(
            "Using shell commands, sum column b of data.csv, write only the number to result.txt, and reply with it."
        )
        self.settle()
        commands = [
            item
            for item in self.sessions.items.list(self.session.id)
            if item.type == "command_execution"
        ]
        if not commands:
            raise Model("ran no command")
        expect(
            all(item.status in ("completed", "failed") for item in commands),
            "command_execution items did not finish",
        )
        self.result("/work/result.txt", "100")
        return f"environment-files copy; {len(commands)} command_execution items; result.txt = 100"

    def disconnect(self):
        code, _, _ = self.command("pkill", "-f", "exec-server")
        expect(code == 0, "pkill did not stop exec-server")
        self.wait_environment("disconnected")
        return "executor loss reported as disconnected"

    def waiting_input(self):
        self.message(
            'Using a shell command, write only the word "again" to /work/again.txt.'
        )
        deadline = time.monotonic() + CONNECT_TIMEOUT
        while True:
            current = self.sessions.retrieve(self.session.id)
            if current.status == "requires_action":
                break
            expect(current.status != "failed", "offline input failed instead of waiting")
            expect(time.monotonic() < deadline, "offline input never required action")
            time.sleep(0.5)
        # records.rs decorates sessions with this exact required_actions shape.
        expect(
            [action.model_dump() for action in current.required_actions]
            == [
                {
                    "type": "environment_connection",
                    "environment_id": self.session.environment.id,
                }
            ],
            "requires_action did not name the waiting environment_connection",
        )
        queued = [
            turn
            for turn in self.sessions.turns.list(self.session.id)
            if turn.status == "queued"
        ]
        expect(len(queued) == 1, f"expected one queued turn, got {len(queued)}")
        self.queued_turn_id = queued[0].id
        return "requires_action with environment_connection; waiting turn queued"

    def reconnect(self):
        self.start_executor()
        self.wait_environment("connected")
        current, turns = self.settle(turns=2)
        expect(
            any(turn.id == self.queued_turn_id for turn in turns),
            "the waiting turn's ID changed on reconnection",
        )
        expect(current.required_actions == [], "environment_connection was not cleared")
        self.result("/work/again.txt", "again")
        return "connected; the same queued turn completed; again.txt = again"

    def delete(self):
        self.sessions.delete(self.session.id)
        self.session = None
        expect(self.sandbox.poll() is None, "session deletion stopped caller compute")
        return "session deleted; executor Sandbox still running"

    def cleanup(self):
        errors = []
        try:
            if self.client is not None:
                if self.uploaded is not None:
                    try:
                        self.client.files.delete(self.uploaded.id)
                    except Exception as error:
                        errors.append(f"upload cleanup: {error}")
                if self.session is not None:
                    try:
                        current = self.sessions.retrieve(self.session.id)
                        if current.status in ("in_progress", "requires_action"):
                            self.sessions.events.create(
                                self.session.id,
                                events=[{"type": "agent.session.input.cancel"}],
                            )
                            deadline = time.monotonic() + CONNECT_TIMEOUT
                            while current.status in ("in_progress", "requires_action"):
                                expect(time.monotonic() < deadline, "cleanup cancel did not settle")
                                time.sleep(0.5)
                                current = self.sessions.retrieve(self.session.id)
                        self.sessions.delete(self.session.id)
                    except Exception as error:
                        errors.append(f"session cleanup: {error}")
        finally:
            try:
                if self.sandbox is not None:
                    self.sandbox.terminate()
            except Exception as error:
                errors.append(f"executor Sandbox cleanup: {error}")
            finally:
                if self.client is not None:
                    try:
                        self.client.close()
                    except Exception as error:
                        errors.append(f"client cleanup: {error}")
        return errors


CHECKS = ("create", "connect", "task", "disconnect", "waiting_input", "reconnect", "delete")


def main():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("state", help="serve.py's endpoint state directory")
    parser.add_argument("--model", default="openai/gpt-5-mini")
    parser.add_argument("--json", help="write the report to this file")
    args = parser.parse_args()
    run = Run(args.state, args.model)
    results = []

    def record(name, outcome, detail, started):
        result = {
            "scenario": name,
            "outcome": outcome,
            "detail": run.detail(detail),
            "seconds": round(time.monotonic() - started, 1),
        }
        results.append(result)
        print(
            f"{outcome:5} {name:13} {result['seconds']:6}s {result['detail']}",
            flush=True,
        )

    blocked = None
    try:
        for name in CHECKS:
            started = time.monotonic()
            if blocked:
                outcome, detail = "SKIP", f"prerequisite {blocked} did not pass"
            else:
                try:
                    detail = getattr(run, name)()
                    outcome = "PASS"
                except Model as error:
                    outcome, detail = "MODEL", str(error)
                except Skip as error:
                    outcome, detail = "SKIP", str(error)
                except Fail as error:
                    outcome, detail = "FAIL", str(error)
                except openai.APIStatusError as error:
                    outcome, detail = "FAIL", f"{error.status_code}: {error.message}"
                except Exception as error:
                    outcome, detail = "FAIL", f"{type(error).__name__}: {error}"
                if outcome in ("FAIL", "SKIP"):
                    blocked = name
            record(name, outcome, detail, started)
    finally:
        started = time.monotonic()
        errors = run.cleanup()
        if errors:
            record("cleanup", "FAIL", "; ".join(errors), started)
    report = {"model": run.model, "results": results}
    if args.json:
        with open(args.json, "w", encoding="utf-8") as handle:
            json.dump(report, handle, indent=2)
    sys.exit(1 if any(result["outcome"] == "FAIL" for result in results) else 0)


if __name__ == "__main__":
    main()
