"""Exercise Python project mode through its bundled CLI and resident."""

import os
from pathlib import Path
import tempfile
import time
import unittest

import membrick


class TestPythonResidentLifecycle(unittest.TestCase):
    def test_client_and_session_leases_keep_resident_warm_then_release_and_restart(self):
        scripts = Path(__import__("sysconfig").get_path("scripts"))
        executable = scripts / ("membrick.exe" if os.name == "nt" else "membrick")
        self.assertTrue(executable.is_file(), f"bundled resident CLI missing: {executable}")

        with tempfile.TemporaryDirectory(prefix="membrick-python-resident-") as temporary:
            root = Path(temporary)
            client = membrick.Memory.init(path=str(root))
            log = root / ".membrick" / "runtime" / "resident.log"

            def add(request_id, content):
                receipt = client.add(
                    [{"role": "user", "content": content}],
                    user_id="python-resident-user",
                    request_id=request_id,
                )
                self.assertEqual(receipt["request_id"], request_id)
                self.assertEqual(receipt["user_id"], "python-resident-user")

            def wait_for_shutdowns(expected):
                deadline = time.monotonic() + 45
                contents = ""
                while time.monotonic() < deadline:
                    try:
                        contents = log.read_text()
                    except FileNotFoundError:
                        pass
                    if contents.count("shutdown complete; pending writes flushed") >= expected:
                        return contents
                    time.sleep(0.05)
                self.fail(f"resident did not shut down {expected} times: {contents}")

            def shutdown_count():
                try:
                    return log.read_text().count("shutdown complete; pending writes flushed")
                except FileNotFoundError:
                    return 0

            add("python-resident-before-idle", "The Python SDK starts the bundled resident.")
            time.sleep(6)
            self.assertEqual(shutdown_count(), 0, "the live Memory client should keep the resident warm")

            session = client.session("python-resident-user", "python-resident-session")
            client.close()
            time.sleep(6)
            self.assertEqual(
                shutdown_count(),
                0,
                "closing Memory must not release an active session's lease",
            )
            session.close()
            first_log = wait_for_shutdowns(1)
            self.assertIn("no client connections remain; shutting down resident", first_log)

            client = membrick.Memory(path=str(root))
            add("python-resident-after-idle", "A new Python client restarts the resident.")
            client.close()
            final_log = wait_for_shutdowns(2)
            self.assertGreaterEqual(final_log.count("Membrick resident IPC listening"), 2)


if __name__ == "__main__":
    unittest.main()
