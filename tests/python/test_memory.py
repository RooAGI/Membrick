import os
import shutil
import socket
import subprocess
import tempfile
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from threading import Thread
import urllib.request

import membrick


def _status_server(status):
    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            self.rfile.read(int(self.headers.get("Content-Length", "0")))
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"error":"test response"}')

        def log_message(self, *_args):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, f"http://127.0.0.1:{server.server_port}"


class MemoryBindingTests(unittest.TestCase):
    def test_public_surface_has_one_client_and_no_embedded_binding(self):
        self.assertTrue(hasattr(membrick, "Memory"))
        self.assertTrue(hasattr(membrick, "MemorySession"))
        self.assertTrue(hasattr(membrick, "version"))
        self.assertFalse(hasattr(membrick, "EmbeddedMemory"))
        self.assertFalse(hasattr(membrick, "MemoryClient"))
        self.assertFalse(hasattr(membrick, "RemoteMemory"))
        self.assertFalse(hasattr(membrick, "IndexStore"))

    def test_version_matches_package_metadata(self):
        import importlib.metadata
        self.assertEqual(membrick.version(), importlib.metadata.version("membrick"))

    def test_project_and_remote_options_are_exclusive(self):
        with self.assertRaises(ValueError):
            membrick.Memory(path="/tmp/project", base_url="http://localhost:1")

    def test_memory_constructor_requires_explicit_connection_and_does_not_create_state(self):
        with tempfile.TemporaryDirectory(prefix="membrick-constructor-") as directory:
            with self.assertRaises(membrick.ConfigurationError):
                membrick.Memory()
            with self.assertRaises(membrick.ConfigurationError) as ctx:
                membrick.Memory(path=directory)
            self.assertIn("Memory.init", str(ctx.exception))
            self.assertFalse(os.path.exists(os.path.join(directory, ".membrick")))

    def test_explicit_create_initializes_project_state(self):
        with tempfile.TemporaryDirectory(prefix="membrick-init-") as directory:
            membrick.Memory.init(path=directory)
            self.assertTrue(os.path.isfile(os.path.join(directory, ".membrick", "server.toml")))

    def test_missing_resident_executable_is_unavailable_before_queue_dispatch(self):
        with tempfile.TemporaryDirectory(prefix="membrick-no-cli-") as directory:
            missing_cli = os.path.join(directory, "missing-membrick")
            memory = membrick.Memory.init(path=directory, resident_executable=missing_cli)
            with self.assertRaises(membrick.UnavailableError):
                memory.add(
                    [{"role": "user", "content": "must not be queued"}],
                    "resident-executable-test",
                    request_id="missing-cli-request",
                )
            self.assertFalse(os.path.exists(os.path.join(
                directory, ".membrick", "runtime", "requests"
            )))

    def test_memory_errors_preserve_builtin_catch_compatibility(self):
        self.assertTrue(issubclass(membrick.MembrickError, Exception))
        self.assertTrue(issubclass(membrick.RejectedError, ValueError))
        self.assertTrue(issubclass(membrick.ConfigurationError, ValueError))
        self.assertTrue(issubclass(membrick.UnavailableError, ConnectionError))
        self.assertTrue(issubclass(membrick.UncertainError, RuntimeError))


class TestRemoteLifecycle(unittest.TestCase):
    """End-to-end remote mode against a real server binary (Luyi gate #2).

    Starts `target/debug/server` on a free loopback port with
    `--server-token` auth, runs a full CRUD lifecycle through
    `membrick.Memory(base_url=..., api_key=...)`, then checks the
    unauthorized and missing-record paths. Hermetic: localhost only.
    Skips cleanly when the server binary has not been built
    (`cargo build --bin server`).
    """

    TOKEN = "remote-test-secret"

    @classmethod
    def _repo_root(cls):
        return os.path.dirname(
            os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        )

    @classmethod
    def setUpClass(cls):
        cls.binary = os.path.join(cls._repo_root(), "target", "debug", "server")
        if not os.path.isfile(cls.binary):
            raise unittest.SkipTest(
                "server binary not built; run `cargo build --bin server`"
            )
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
            probe.bind(("127.0.0.1", 0))
            cls.port = probe.getsockname()[1]
        # Use the canonical temp root because the server rejects paths that
        # traverse symlinks (for example macOS's /var -> /private/var).
        cls.tmpdir = tempfile.mkdtemp(
            prefix="membrick-remote-test-", dir=os.path.realpath(tempfile.gettempdir())
        )
        # Scrub ambient auth config so the test controls the server's
        # credentials exactly (JWT auth is env-only; token via CLI flag).
        env = {
            k: v
            for k, v in os.environ.items()
            if k not in ("JWT_SECRET", "SERVER_TOKEN", "SERVER_TENANT_ID")
        }
        cls.proc = subprocess.Popen(
            [
                cls.binary,
                "--bind", f"127.0.0.1:{cls.port}",
                "--index", cls.tmpdir,
                "--project-root", cls.tmpdir,
                "--server-token", cls.TOKEN,
            ],
            cwd=cls.tmpdir,
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        cls.base_url = f"http://127.0.0.1:{cls.port}"
        deadline = time.time() + 30
        while time.time() < deadline:
            if cls.proc.poll() is not None:
                raise RuntimeError(
                    f"server exited during startup (code {cls.proc.returncode})"
                )
            try:
                with urllib.request.urlopen(
                    f"{cls.base_url}/health", timeout=2
                ) as response:
                    if response.status == 200:
                        break
            except OSError:
                time.sleep(0.2)
        else:
            cls.proc.terminate()
            raise RuntimeError("server did not become ready within 30s")

    @classmethod
    def tearDownClass(cls):
        if hasattr(cls, "proc"):
            cls.proc.terminate()
            try:
                cls.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                cls.proc.kill()
        if hasattr(cls, "tmpdir"):
            shutil.rmtree(cls.tmpdir, ignore_errors=True)

    def _memory(self, api_key=TOKEN):
        return membrick.Memory(base_url=self.base_url, api_key=api_key)

    def test_remote_crud_lifecycle(self):
        memory = self._memory()

        added = memory.add(
            [{"role": "user", "content": "Remote memory about submarines"}],
            "remote-user",
            request_id="remote-1",
            session_id="remote-session",
        )
        self.assertTrue(added["success"])

        # Remote adds are durably accepted before the server publishes the
        # next immutable search snapshot. Flush explicitly for read-after-write.
        memory.refresh()
        found = memory.search("submarines", "remote-user", 5)
        self.assertGreaterEqual(len(found), 1)
        record = found[0]
        self.assertEqual(record["user_id"], "remote-user")

        fetched = memory.get(record["id"], "remote-user")
        self.assertEqual(fetched["id"], record["id"])

        listed = memory.list("remote-user")
        self.assertEqual(len(listed["data"]), 1)

        updated = memory.update(
            record["id"], "remote-user", "Remote memory about sailboats"
        )
        self.assertIn("sailboats", updated["content"])

        self.assertTrue(memory.delete(record["id"], "remote-user"))
        self.assertIsNone(memory.get(record["id"], "remote-user"))
        memory.refresh()

    def test_durable_retry_without_session_keeps_identity(self):
        memory = self._memory()
        user_id = f"retry-user-{time.time_ns()}"
        request_id = f"retry-request-{time.time_ns()}"
        messages = [{"role": "user", "content": "One durable retry without a session."}]

        first = memory.add(messages, user_id, request_id=request_id)
        retry = memory.add(messages, user_id, request_id=request_id)

        self.assertEqual(first["request_id"], retry["request_id"])
        self.assertEqual(first["session_id"], retry["session_id"])
        memory.refresh()
        records = memory.list(user_id, session_id=first["session_id"])["data"]
        self.assertEqual(len(records), 1)

    def test_remote_wrong_api_key_is_unauthorized(self):
        memory = self._memory(api_key="wrong-key")
        with self.assertRaises(membrick.RejectedError) as ctx:
            memory.list("remote-user")
        self.assertIsInstance(ctx.exception, ValueError)
        self.assertEqual(ctx.exception.status, 401)
        self.assertIn("401", str(ctx.exception))

    def test_unavailable_transport_raises_typed_subclass(self):
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
            probe.bind(("127.0.0.1", 0))
            unused_port = probe.getsockname()[1]
        memory = membrick.Memory(base_url=f"http://127.0.0.1:{unused_port}")
        with self.assertRaises(membrick.UnavailableError) as ctx:
            memory.search("query", "remote-user")
        self.assertIsInstance(ctx.exception, ConnectionError)

    def test_uncertain_response_raises_typed_subclass(self):
        server, base_url = _status_server(503)
        self.addCleanup(server.shutdown)
        self.addCleanup(server.server_close)
        memory = membrick.Memory(base_url=base_url)
        with self.assertRaises(membrick.UncertainError) as ctx:
            memory.add(
                [{"role": "user", "content": "the server may have accepted this"}],
                "remote-user",
                request_id="uncertain-request",
            )
        self.assertIsInstance(ctx.exception, RuntimeError)

    def test_remote_missing_record_returns_none(self):
        memory = self._memory()
        self.assertIsNone(memory.get("does-not-exist", "remote-user"))
        self.assertIsNone(memory.update("does-not-exist", "remote-user", "x"))
        self.assertFalse(memory.delete("remote-user", "does-not-exist"))
