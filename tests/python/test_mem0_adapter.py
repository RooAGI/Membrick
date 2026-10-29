"""Contract tests for the documented, scoped Mem0-style Python adapter."""

import os
import json
import socket
import subprocess
import tempfile
import time
import unittest
import urllib.request
import uuid

import membrick


class Mem0AdapterContractTests(unittest.TestCase):
    TOKEN = "mem0-adapter-test-token"

    @classmethod
    def _repo_root(cls):
        return os.path.dirname(
            os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        )

    @classmethod
    def setUpClass(cls):
        cls.binary = os.path.join(cls._repo_root(), "target", "debug", "server")
        if not os.path.isfile(cls.binary):
            raise unittest.SkipTest("target/debug/server is not built")
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
            probe.bind(("127.0.0.1", 0))
            cls.port = probe.getsockname()[1]
        cls.tmpdir = tempfile.mkdtemp(prefix="mem0-adapter-test-")
        project_root = os.path.realpath(cls.tmpdir)
        state_dir = os.path.join(project_root, ".membrick")
        os.makedirs(state_dir)
        with open(os.path.join(state_dir, "server.toml"), "w", encoding="utf-8") as settings:
            settings.write(f"project_root = {json.dumps(project_root)}\n")
        env = {
            key: value
            for key, value in os.environ.items()
            if key not in ("JWT_SECRET", "SERVER_TOKEN", "SERVER_TENANT_ID")
        }
        cls.proc = subprocess.Popen(
            [
                cls.binary,
                "--project-root",
                project_root,
                "--bind",
                f"127.0.0.1:{cls.port}",
                "--server-token",
                cls.TOKEN,
            ],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        cls.base_url = f"http://127.0.0.1:{cls.port}"
        deadline = time.time() + 30
        while time.time() < deadline:
            if cls.proc.poll() is not None:
                raise RuntimeError(
                    f"test server exited during startup ({cls.proc.returncode})"
                )
            try:
                with urllib.request.urlopen(f"{cls.base_url}/health", timeout=2) as response:
                    if response.status == 200:
                        break
            except OSError:
                time.sleep(0.2)
        else:
            cls.proc.terminate()
            raise RuntimeError("test server did not become ready within 30 seconds")

    @classmethod
    def tearDownClass(cls):
        if hasattr(cls, "proc"):
            cls.proc.terminate()
            try:
                cls.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                cls.proc.kill()
        if hasattr(cls, "tmpdir"):
            import shutil

            shutil.rmtree(cls.tmpdir, ignore_errors=True)

    def setUp(self):
        self.client = membrick.Memory(
            base_url=self.base_url, api_key=self.TOKEN
        )
        self.memory = membrick.Mem0Adapter(self.client)
        self.user_id = f"mem0-user-{uuid.uuid4().hex}"
        self.run_id = f"mem0-run-{uuid.uuid4().hex}"

    def add_one(self, content, run_id=None):
        return self.memory.add(
            [{"role": "user", "content": content}],
            user_id=self.user_id,
            run_id=run_id,
            infer=False,
        )

    def test_supported_crud_shapes_and_explicit_ownership(self):
        content = f"Mem0 adapter contract keeps the cobalt lantern {uuid.uuid4().hex}"
        request_id = f"mem0-add-{uuid.uuid4().hex}"
        receipt = self.memory.add(
            [{"role": "user", "content": content}],
            user_id=self.user_id,
            run_id=self.run_id,
            request_id=request_id,
            infer=False,
        )
        self.assertEqual(set(receipt), {"results"})
        self.assertEqual(len(receipt["results"]), 1)
        added = receipt["results"][0]
        self.assertEqual(added["memory"], content)
        self.assertEqual(added["session_id"], self.run_id)
        self.assertNotIn("event", added)
        self.assertTrue(added["id"])
        repeated = self.memory.add(
            [{"role": "user", "content": content}],
            user_id=self.user_id,
            run_id=self.run_id,
            request_id=request_id,
            infer=False,
        )
        self.assertEqual(repeated, receipt)

    def test_get_all_uses_mem0_page_envelope_and_keeps_user_scope(self):
        self.add_one(f"First page marker {uuid.uuid4().hex}")
        self.add_one(f"Second page marker {uuid.uuid4().hex}")

        first = self.memory.get_all(
            filters={"user_id": self.user_id}, page=1, page_size=1
        )
        self.assertEqual(first["count"], 2)
        self.assertEqual(len(first["results"]), 1)
        self.assertIsNone(first["previous"])
        self.assertTrue(first["next"].startswith("membrick://"))

        second = self.memory.get_all(
            filters={"user_id": self.user_id}, page=2, page_size=1
        )
        self.assertEqual(len(second["results"]), 1)
        self.assertIsNone(second["next"])
        self.assertTrue(second["previous"].startswith("membrick://"))
        self.assertNotEqual(first["results"][0]["id"], second["results"][0]["id"])

    def test_get_all_pages_across_multiple_service_cursor_pages(self):
        token = uuid.uuid4().hex
        messages = [
            {"role": "user", "content": f"Large get_all page {token} item {index}"}
            for index in range(125)
        ]
        added = self.memory.add(messages, user_id=self.user_id, infer=False)
        self.assertEqual(len(added["results"]), 125)

        page = self.memory.get_all(
            filters={"user_id": self.user_id}, page=3, page_size=50
        )
        self.assertEqual(page["count"], 125)
        self.assertEqual(len(page["results"]), 25)
        self.assertTrue(page["previous"].startswith("membrick://"))
        self.assertIsNone(page["next"])

    def test_search_accepts_mem0_user_filter_call_shape(self):
        phrase = f"Mem0 filter shape marker {uuid.uuid4().hex}"
        self.add_one(phrase)
        response = self.memory.search(
            phrase,
            filters={"user_id": self.user_id},
            top_k=10,
        )
        self.assertIn("results", response)
        self.assertTrue(any(phrase in row["memory"] for row in response["results"]))

    def test_search_run_id_is_not_attributed_to_hits_from_other_sessions(self):
        common = f"Cross-session run attribution {uuid.uuid4().hex}"
        first_run = f"run-{uuid.uuid4().hex}"
        second_run = f"run-{uuid.uuid4().hex}"
        self.add_one(f"{common} first detail", first_run)
        self.add_one(f"{common} second detail", second_run)
        response = self.memory.search(
            common, user_id=self.user_id, run_id=first_run, top_k=10
        )
        self.assertTrue(response["results"])
        self.assertIn(second_run, {item["session_id"] for item in response["results"]})
        for item in response["results"]:
            if item["session_id"] == first_run:
                self.assertEqual(item["run_id"], first_run)
            else:
                self.assertNotIn("run_id", item)

    def test_search_rejects_limit_when_top_k_was_explicitly_supplied(self):
        with self.assertRaisesRegex(ValueError, "either top_k or the legacy limit"):
            self.memory.search("query", user_id=self.user_id, top_k=10, limit=5)

    def test_search_get_update_delete_shapes(self):
        content = f"The cobalt lantern {uuid.uuid4().hex}"
        self.memory.add(
            [{"role": "user", "content": content}],
            user_id=self.user_id,
            request_id=f"mem0-crud-{uuid.uuid4().hex}",
            infer=False,
        )
        self.client.refresh()
        hits = self.memory.search(content, user_id=self.user_id, limit=5)
        self.assertIn("results", hits)
        self.assertGreaterEqual(len(hits["results"]), 1)
        hit = hits["results"][0]
        self.assertTrue({"id", "memory", "score", "user_id", "metadata"}.issubset(hit))
        self.assertIn("cobalt lantern", hit["memory"])

        memory_id = next(hit["id"] for hit in hits["results"] if content in hit["memory"])
        record = self.memory.get(memory_id, user_id=self.user_id)
        self.assertEqual(record["id"], memory_id)
        self.assertEqual(record["memory"], content)
        with self.assertRaisesRegex(ValueError, "404"):
            self.memory.get(memory_id, user_id=self.user_id + "-other")

        updated = self.memory.update(
            memory_id,
            data="The cobalt lantern was replaced with a silver compass.",
            user_id=self.user_id,
        )
        self.assertIn("silver compass", updated["memory"])
        self.assertEqual(
            self.memory.delete(memory_id, user_id=self.user_id),
            {"message": "Memory deleted successfully!"},
        )
        with self.assertRaisesRegex(ValueError, "404"):
            self.memory.get(memory_id, user_id=self.user_id)
        with self.assertRaisesRegex(ValueError, "404"):
            self.memory.delete(memory_id, user_id=self.user_id)

    def test_add_waits_for_visibility_and_returns_a_result(self):
        content = f"Mem0 adapter synchronous add visibility {uuid.uuid4().hex}"
        result = self.memory.add(
            [{"role": "user", "content": content}],
            user_id=self.user_id,
            infer=False,
        )

        self.assertEqual(len(result["results"]), 1)
        self.assertEqual(result["results"][0]["memory"], content)

    def test_add_accepts_mem0_platform_string_and_single_message_inputs(self):
        text = f"String message input {uuid.uuid4().hex}"
        from_string = self.memory.add(text, user_id=self.user_id)
        self.assertEqual(from_string["results"][0]["memory"], text)

        message = {"role": "user", "content": f"Single dict input {uuid.uuid4().hex}"}
        from_dict = self.memory.add(message, user_id=self.user_id, infer=False)
        self.assertEqual(from_dict["results"][0]["memory"], message["content"])

    def test_explicit_request_id_retry_is_stable_without_run_id(self):
        content = f"Mem0 adapter durable retry without run id {uuid.uuid4().hex}"
        request_id = f"mem0-retry-{uuid.uuid4().hex}"
        messages = [{"role": "user", "content": content}]

        first = self.memory.add(
            messages,
            user_id=self.user_id,
            request_id=request_id,
            infer=False,
        )
        retry = self.memory.add(
            messages,
            user_id=self.user_id,
            request_id=request_id,
            infer=False,
        )

        self.assertEqual(retry, first)
        self.assertEqual(len(retry["results"]), 1)
        self.assertEqual(retry["results"][0]["memory"], content)
        self.assertTrue(retry["results"][0]["session_id"])
        self.assertNotIn("run_id", retry["results"][0])

    def test_explicit_request_id_retries_share_identity_with_memory_client(self):
        content = f"Cross-surface durable retry {uuid.uuid4().hex}"
        request_id = f"cross-surface-retry-{uuid.uuid4().hex}"
        messages = [{"role": "user", "content": content}]

        native = self.client.add(
            messages,
            user_id=self.user_id,
            request_id=request_id,
            wait_for_visibility=True,
        )
        adapted = self.memory.add(
            messages,
            user_id=self.user_id,
            request_id=request_id,
            infer=False,
        )

        self.assertEqual(adapted["results"][0]["session_id"], native["session_id"])
        self.assertNotIn("run_id", adapted["results"][0])
        records = self.client.list(
            self.user_id, session_id=native["session_id"]
        )["data"]
        self.assertEqual(len(records), 1)
        self.assertEqual(records[0]["content"], f"user: {content}")
        self.assertEqual(records[0]["id"], adapted["results"][0]["id"])

    def test_omitted_run_id_recovers_pre_upgrade_session_through_adapter(self):
        content = f"Pre-upgrade Mem0 retry {uuid.uuid4().hex}"
        request_id = f"pre-upgrade-{uuid.uuid4().hex}"
        legacy_session_id = f"old-random-session-{uuid.uuid4().hex}"
        messages = [{"role": "user", "content": content}]

        original = self.client.add(
            messages,
            user_id=self.user_id,
            request_id=request_id,
            session_id=legacy_session_id,
            wait_for_visibility=True,
        )
        recovered = self.memory.add(
            messages,
            user_id=self.user_id,
            request_id=request_id,
            infer=False,
        )

        self.assertEqual(recovered["results"][0]["session_id"], legacy_session_id)
        self.assertEqual(recovered["results"][0]["id"], original["adjudication"]["doc_ids"][0])
        self.assertNotIn("run_id", recovered["results"][0])
        records = self.client.list(self.user_id, session_id=legacy_session_id)["data"]
        self.assertEqual(len(records), 1)

    def test_metadata_round_trips_json_and_update_replaces_it(self):
        content = f"Metadata preservation {uuid.uuid4().hex}"
        metadata = {
            "nested": {"enabled": True, "labels": ["one", "two"]},
            "count": 7,
            "nullable": None,
        }
        result = self.memory.add(
            [{"role": "user", "content": content}],
            user_id=self.user_id,
            metadata=metadata,
            infer=False,
        )
        record = result["results"][0]
        self.assertEqual(record["metadata"], metadata)
        self.assertEqual(self.memory.get(record["id"], self.user_id)["metadata"], metadata)
        with self.assertRaisesRegex(ValueError, "404"):
            self.memory.update(
                record["id"], text="unauthorized change", user_id=self.user_id + "-other",
                metadata={"attack": True},
            )
        self.assertEqual(self.memory.get(record["id"], self.user_id)["metadata"], metadata)

        replacement = {"source": "manual", "confidence": 0.75}
        updated = self.memory.update(
            record["id"], text="Updated metadata memory", user_id=self.user_id,
            metadata=replacement,
        )
        self.assertEqual(updated["metadata"], replacement)
        preserved = self.memory.update(
            record["id"], text="Metadata remains after text update", user_id=self.user_id,
        )
        self.assertEqual(preserved["metadata"], replacement)
        filtered = self.memory.search(
            "Updated metadata memory", user_id=self.user_id,
            filters={"source": "manual"},
        )
        self.assertEqual([item["id"] for item in filtered["results"]], [record["id"]])

    def test_metadata_participates_in_add_idempotency_fingerprint(self):
        request_id = f"metadata-retry-{uuid.uuid4().hex}"
        messages = [{"role": "user", "content": f"stable body {uuid.uuid4().hex}"}]
        self.memory.add(messages, user_id=self.user_id, request_id=request_id,
                        metadata={"revision": 1}, infer=False)
        with self.assertRaises(ValueError):
            self.memory.add(messages, user_id=self.user_id, request_id=request_id,
                            metadata={"revision": 2}, infer=False)

    def test_expiration_date_matches_platform_calendar_date_shape(self):
        content = f"Mem0 adapter expiration date {uuid.uuid4().hex}"
        result = self.memory.add(
            [{"role": "user", "content": content}],
            user_id=self.user_id,
            run_id=self.run_id,
            request_id=f"mem0-expiration-{uuid.uuid4().hex}",
            expiration_date="2027-01-01",
            infer=False,
        )

        record = result["results"][0]
        self.assertEqual(record["expiration_date"], "2027-01-01")
        stored = self.client.get(record["id"], self.user_id)
        self.assertEqual(
            stored["expires_at_ms"], 1798848000000,  # 2027-01-02T00:00:00Z
        )
        with self.assertRaisesRegex(ValueError, "Unix epoch"):
            self.memory.add(
                [{"role": "user", "content": f"pre-epoch {uuid.uuid4().hex}"}],
                user_id=self.user_id,
                request_id=f"mem0-pre-epoch-{uuid.uuid4().hex}",
                expiration_date="1969-12-31",
                infer=False,
            )

    def test_update_without_expiration_preserves_existing_expiration(self):
        receipt = self.memory.add(
            [{"role": "user", "content": f"Expiring memory {uuid.uuid4().hex}"}],
            user_id=self.user_id,
            request_id=f"mem0-expiry-preserve-{uuid.uuid4().hex}",
            expiration_date="2030-04-12",
            infer=False,
        )
        memory_id = receipt["results"][0]["id"]
        before = self.client.get(memory_id, self.user_id)
        updated = self.memory.update(
            memory_id,
            text="Updated expiring memory",
            user_id=self.user_id,
        )
        after = self.client.get(memory_id, self.user_id)
        self.assertEqual(updated["expiration_date"], "2030-04-12")
        self.assertEqual(after["expires_at_ms"], before["expires_at_ms"])

    def test_metadata_filters_and_reserved_key_rejection(self):
        run_id = f"mem0-filter-run-{uuid.uuid4().hex}"
        receipt = self.add_one(f"Filter marker {uuid.uuid4().hex}", run_id=run_id)
        self.client.refresh()

        found = self.memory.search(
            "Filter marker",
            user_id=self.user_id,
            filters={"nonexistent": "value"},
        )
        self.assertFalse(found["results"])
        for reserved in ("user_id",):
            with self.subTest(reserved=reserved):
                with self.assertRaisesRegex(ValueError, reserved):
                    self.memory.search(
                        "Filter marker",
                        user_id=self.user_id,
                        filters={reserved: "anything"},
                    )
        scoped = self.memory.search(
            "Filter marker",
            user_id=self.user_id,
            filters={"run_id": run_id},
        )
        self.assertEqual([item["id"] for item in scoped["results"]],
                         [receipt["results"][0]["id"]])
        for unsupported in ("agent_id", "app_id"):
            with self.subTest(unsupported=unsupported):
                with self.assertRaisesRegex(NotImplementedError, unsupported):
                    self.memory.search(
                        "Filter marker",
                        user_id=self.user_id,
                        filters={unsupported: "anything"},
                    )
    def test_nested_filters_apply_to_search_and_get_all(self):
        token = uuid.uuid4().hex
        selected = self.memory.add(
            [{"role": "user", "content": f"Nested filter marker {token} selected"}],
            user_id=self.user_id,
            metadata={"kind": "nested-test", "priority": 4, "tags": ["rust", "needle"]},
            infer=False,
        )["results"][0]
        self.memory.add(
            [{"role": "user", "content": f"Nested filter marker {token} rejected"}],
            user_id=self.user_id,
            metadata={"kind": "nested-test", "priority": 1, "tags": ["rust"]},
            infer=False,
        )

        filters = {
            "AND": [
                {"user_id": self.user_id},
                {"metadata": {"kind": "nested-test"}},
                {"OR": [{"priority": {"gte": 3}}, {"tags": {"contains": "needle"}}]},
                {"NOT": {"priority": {"lt": 0}}},
            ]
        }
        searched = self.memory.search(
            f"Nested filter marker {token}", filters=filters, top_k=1
        )
        self.assertEqual([item["id"] for item in searched["results"]], [selected["id"]])

        listed = self.memory.get_all(filters=filters, page_size=1)
        self.assertEqual(listed["count"], 1)
        self.assertEqual([item["id"] for item in listed["results"]], [selected["id"]])

    def test_run_id_predicates_keep_or_and_not_semantics(self):
        token = uuid.uuid4().hex
        run_a = f"mem0-run-a-{token}"
        run_b = f"mem0-run-b-{token}"
        added_a = self.memory.add(
            [{"role": "user", "content": f"Boolean run filter marker {token} A"}],
            user_id=self.user_id,
            run_id=run_a,
            metadata={"case": token, "branch": "a"},
            infer=False,
        )["results"][0]
        added_b = self.memory.add(
            [{"role": "user", "content": f"Boolean run filter marker {token} B"}],
            user_id=self.user_id,
            run_id=run_b,
            metadata={"case": token, "branch": "b"},
            infer=False,
        )["results"][0]

        or_filter = {
            "AND": [
                {"user_id": self.user_id},
                {"case": token},
                {"OR": [{"run_id": run_a}, {"branch": "b"}]},
            ]
        }
        search = self.memory.search(
            f"Boolean run filter marker {token}", filters=or_filter, top_k=10
        )
        self.assertEqual(
            {item["id"] for item in search["results"]},
            {added_a["id"], added_b["id"]},
        )
        other_session_hit = next(
            item for item in search["results"] if item["id"] == added_b["id"]
        )
        self.assertNotIn("run_id", other_session_hit)

        not_filter = {
            "AND": [
                {"user_id": self.user_id},
                {"case": token},
                {"NOT": {"run_id": run_a}},
            ]
        }
        listed = self.memory.get_all(filters=not_filter, page_size=10)
        self.assertEqual([item["id"] for item in listed["results"]], [added_b["id"]])
        self.assertNotIn("run_id", listed["results"][0])

    def test_search_returns_hit_record_data_without_followup_get(self):
        token = uuid.uuid4().hex
        added = self.memory.add(
            [{"role": "user", "content": f"Hit hydration marker {token}"}],
            user_id=self.user_id,
            metadata={"kind": "hydration", "token": token},
            infer=False,
        )["results"][0]
        result = self.memory.search(
            f"Hit hydration marker {token}", user_id=self.user_id, top_k=1
        )["results"][0]
        self.assertEqual(result["id"], added["id"])
        self.assertEqual(result["metadata"], {"kind": "hydration", "token": token})
        self.assertIn("score", result)

    def test_nested_filters_keep_owner_scope_mandatory(self):
        token = uuid.uuid4().hex
        self.memory.add(
            [{"role": "user", "content": f"Shared query {token} alice"}],
            user_id=self.user_id,
            metadata={"visible": True},
            infer=False,
        )
        other_user = self.user_id + "-other"
        self.memory.add(
            [{"role": "user", "content": f"Shared query {token} other"}],
            user_id=other_user,
            metadata={"visible": True},
            infer=False,
        )
        response = self.memory.search(
            f"Shared query {token}", user_id=self.user_id,
            filters={"OR": [{"user_id": self.user_id}, {"visible": True}]}, top_k=10,
        )
        self.assertTrue(response["results"])
        self.assertTrue(all(item["user_id"] == self.user_id for item in response["results"]))

    def test_negative_user_id_filter_cannot_establish_owner_scope(self):
        with self.assertRaisesRegex(ValueError, "requires user_id"):
            self.memory.search("query", filters={"NOT": {"user_id": self.user_id}})
        with self.assertRaisesRegex(ValueError, "exactly one equality filter for user_id"):
            self.memory.get_all(filters={"NOT": {"user_id": self.user_id}})
        with self.assertRaisesRegex(ValueError, "exactly one equality filter for user_id"):
            self.memory.get_all(
                filters={
                    "OR": [
                        {"user_id": self.user_id},
                        {"branch": "shared"},
                    ]
                }
            )

    def test_filter_validation_rejects_unsupported_fields_and_operators(self):
        with self.assertRaisesRegex(NotImplementedError, "categories"):
            self.memory.search("query", user_id=self.user_id, filters={"categories": "work"})
        with self.assertRaisesRegex(ValueError, "requires an array"):
            self.memory.search("query", user_id=self.user_id, filters={"priority": {"in": "high"}})
        for malformed in (
            {"priority": {"gtee": 3}},
            {"priority": {"$gte": 3}},
            {"priority": {">=": 3}},
        ):
            with self.subTest(malformed=malformed):
                with self.assertRaisesRegex(ValueError, "operator"):
                    self.memory.search(
                        "query", user_id=self.user_id, filters=malformed
                    )
        for malformed_owner in (
            {"user_id": {"ne": self.user_id}},
            {"user_id": {"eq": ""}},
        ):
            with self.subTest(malformed_owner=malformed_owner):
                with self.assertRaisesRegex(NotImplementedError, "positive nonempty user_id"):
                    self.memory.search(
                        "query", filters=malformed_owner
                    )

    def test_unsupported_mem0_arguments_raise_not_implemented(self):
        message = [{"role": "user", "content": "unsupported option check"}]
        for option, value in (
            ("agent_id", "agent"),
            ("app_id", "app"),
        ):
            with self.subTest(option=option):
                with self.assertRaisesRegex(NotImplementedError, option):
                    self.memory.add(message, user_id=self.user_id, **{option: value})
        for infer in (None, True):
            with self.subTest(infer=infer):
                with self.assertRaisesRegex(NotImplementedError, "infer=False"):
                    self.memory.add(message, user_id=self.user_id, infer=infer)
        # The adapter defaults to infer=False so the common SDK-style call works.
        self.assertTrue(self.memory.add(message, user_id=self.user_id)["results"])
        self.assertTrue(
            self.memory.add(message, user_id=self.user_id, infer=False)["results"]
        )
        for option in ("agent_id", "app_id"):
            with self.subTest(search_option=option):
                with self.assertRaisesRegex(NotImplementedError, option):
                    self.memory.search("query", user_id=self.user_id, **{option: "x"})

    def test_required_owner_invalid_inputs_and_transport_errors(self):
        message = [{"role": "user", "content": "owner is required"}]
        with self.assertRaises(TypeError):
            self.memory.add(message)
        with self.assertRaisesRegex(ValueError, "requires user_id"):
            self.memory.search("query")
        with self.assertRaises(ValueError):
            self.memory.search("query", user_id=self.user_id, limit=0)
        with self.assertRaises(ValueError):
            self.memory.add(
                [{"content": "missing role"}], user_id=self.user_id, infer=False
            )

        rejected = membrick.Mem0Adapter(
            membrick.Memory(base_url=self.base_url, api_key="wrong-token")
        )
        with self.assertRaisesRegex(ValueError, "401"):
            rejected.search("query", user_id=self.user_id)

        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
            probe.bind(("127.0.0.1", 0))
            unused_port = probe.getsockname()[1]
        unavailable = membrick.Mem0Adapter(
            membrick.Memory(base_url=f"http://127.0.0.1:{unused_port}")
        )
        with self.assertRaises(ConnectionError):
            unavailable.search("query", user_id=self.user_id)

    def test_run_id_preserves_session_and_resolves_follow_up(self):
        self.add_one(
            "The Quartz database listens on port 5432 for connections.", self.run_id
        )
        self.client.refresh()
        first = self.memory.search(
            "Quartz database", user_id=self.user_id, run_id=self.run_id
        )
        self.assertTrue(first["results"])
        follow_up = self.memory.search(
            "tell me more about it", user_id=self.user_id, run_id=self.run_id
        )
        self.assertTrue(follow_up["results"])
        self.assertIn("Quartz", follow_up["results"][0]["memory"])


if __name__ == "__main__":
    unittest.main()
