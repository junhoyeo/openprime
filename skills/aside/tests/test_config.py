"""Account-aware config readers."""
from __future__ import annotations

import asyncio
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

import aside  # noqa: E402


class AccountConfigTest(unittest.TestCase):
    def test_models_and_providers_follow_the_selected_account(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            u0 = Path(tmp) / "u" / "0"
            u1 = Path(tmp) / "u" / "1"
            for d, model, port in ((u0, "m0", 1000), (u1, "m1", 1001)):
                d.mkdir(parents=True)
                (d / "models.json").write_text(json.dumps({
                    "providers": {"gw": {"baseUrl": f"http://127.0.0.1:{port}", "models": [{"id": model}]}}
                }))
            with mock.patch.object(aside, "CONFIG_DIR", u0):
                self.assertEqual(aside.config_dir(), u0)
                self.assertEqual(aside.config_dir("u1"), u1)
                self.assertEqual(aside.config_dir("1"), u1)
                self.assertEqual(aside.models(), ["gw/m0"])
                self.assertEqual(aside.models(account="u1"), ["gw/m1"])
                self.assertEqual(aside.providers(account="u1"), {"gw": "http://127.0.0.1:1001"})
                with self.assertRaises(aside.AsideError):
                    aside.config_dir("bogus")

    def test_config_readers_ignore_malformed_shapes_and_filesystem_errors(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            config = Path(tmp) / "u" / "0"
            config.mkdir(parents=True)
            models = config / "models.json"
            with mock.patch.object(aside, "CONFIG_DIR", config):
                malformed = (
                    (None, "models.json root must be an object"),
                    ("invalid", "models.json root must be an object"),
                    (42, "models.json root must be an object"),
                    ([], "models.json root must be an object"),
                    ({"providers": None}, "models.json providers must be an object"),
                    ({"providers": []}, "models.json providers must be an object"),
                    ({"providers": {"broken": []}},
                     "models.json contains invalid provider or model entries"),
                )
                for payload, expected_error in malformed:
                    with self.subTest(payload=payload):
                        models.write_text(json.dumps(payload))
                        self.assertEqual(aside.models(), [])
                        self.assertEqual(aside.providers(), {})
                        self.assertEqual(aside._provider_config()[1], expected_error)

                models.write_text(json.dumps({"providers": {
                    "gw": {"baseUrl": 123, "models": [None, {"id": None}, {"id": "valid"}]},
                }}))
                self.assertEqual(aside.models(), ["gw/valid"])
                self.assertEqual(aside.providers(), {"gw": ""})
                with mock.patch.object(aside.os, "open", side_effect=FileNotFoundError):
                    self.assertEqual(aside._provider_config(), ({}, None))
                with mock.patch.object(aside.os, "open", side_effect=PermissionError):
                    self.assertEqual(aside._provider_config(), ({}, "models.json could not be read"))

                models.write_text("{}")
                with mock.patch.object(aside._json, "loads", side_effect=RecursionError):
                    self.assertEqual(aside._provider_config(), ({}, "models.json nesting is too deep"))
                models.write_text(" " * 1_048_577)
                self.assertEqual(aside._provider_config(), ({}, "models.json is too large"))

                models.unlink()
                models.mkdir()
                self.assertEqual(aside.models(), [])
                self.assertEqual(aside.providers(), {})
                self.assertEqual(aside._provider_config(), ({}, "models.json must be a regular file"))
                models.rmdir()
                if hasattr(os, "mkfifo"):
                    os.mkfifo(models)
                    self.assertEqual(aside._provider_config(), ({}, "models.json must be a regular file"))
                    models.unlink()
                models.mkdir()
                with (
                    mock.patch.object(aside, "_bin", return_value="/synthetic/aside"),
                    mock.patch.object(aside, "_app_running", return_value=True),
                    mock.patch.object(aside.asyncio, "open_connection") as open_connection,
                ):
                    report = asyncio.run(aside.health(timeout=0.25))
                self.assertFalse(report["ok"])
                self.assertEqual(report["config_error"], "models.json must be a regular file")
                open_connection.assert_not_called()

    def test_health_probes_only_exact_loopback_without_redirects_and_bounds_reads(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            config = Path(tmp) / "u" / "0"
            config.mkdir(parents=True)
            (config / "models.json").write_text(json.dumps({"providers": {
                "ipv4": {"baseUrl": "http://127.0.0.1:8787"},
                "ipv4-alt": {"baseUrl": "http://127.0.0.2:8788"},
                "localhost": {"baseUrl": "https://localhost:9443"},
                "malformed": {"baseUrl": "http://[::1]:9000"},
                "redirect": {"baseUrl": "http://127.0.0.1:9001"},
                "slow": {"baseUrl": "http://127.0.0.3:9006"},
                "unspecified": {"baseUrl": "http://0.0.0.0:9005"},
                "remote": {"baseUrl": "https://api.example.com"},
                "remote-lookalike": {"baseUrl": "https://127.0.0.1.attacker.invalid"},
                "bad-port": {"baseUrl": "http://127.0.0.1:bad"},
                "userinfo": {"baseUrl": "http://attacker@127.0.0.1:9002"},
                "query": {"baseUrl": "http://localhost:9003?target=remote"},
                "path": {"baseUrl": "http://localhost:9004/proxy"},
            }}))
            connections: list[tuple[str, int, bool, str | None]] = []
            writers = []

            class Writer:
                def __init__(self):
                    self.data = b""
                    self.closed = False

                def write(self, data: bytes) -> None:
                    self.data += data

                async def drain(self) -> None:
                    return None

                def close(self) -> None:
                    self.closed = True

            async def open_connection(host, port, *, ssl=None, server_hostname=None, limit=None):
                connections.append((host, port, ssl is not None, server_hostname))
                reader = asyncio.StreamReader(limit=limit)
                writer = Writer()
                writers.append(writer)
                response = {
                    ("127.0.0.1", 8787): b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}",
                    ("127.0.0.2", 8788): b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}",
                    ("localhost", 9443): (
                        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n"
                        + b"c9\r\n" + b"x" * 201 + b"\r\n0\r\n\r\n"
                    ),
                    ("::1", 9000): b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nnot json",
                    ("127.0.0.1", 9001): (
                        b"HTTP/1.1 302 Found\r\nLocation: https://remote.invalid/\r\nContent-Length: 0\r\n\r\n"
                    ),
                }.get((host, port))
                if response is not None:
                    reader.feed_data(response)
                    reader.feed_eof()
                return reader, writer

            with (
                mock.patch.object(aside, "CONFIG_DIR", config),
                mock.patch.object(aside, "_bin", return_value="/synthetic/aside"),
                mock.patch.object(aside, "_app_running", return_value=True),
                mock.patch.object(aside.asyncio, "open_connection", side_effect=open_connection),
            ):
                report = asyncio.run(aside.health(timeout=0.05))

            self.assertEqual(
                set(report["local_providers"]),
                {"ipv4", "ipv4-alt", "localhost", "malformed", "redirect", "slow"},
            )
            self.assertEqual(set(connections), {
                ("127.0.0.1", 8787, False, None),
                ("127.0.0.2", 8788, False, None),
                ("localhost", 9443, True, "localhost"),
                ("::1", 9000, False, None),
                ("127.0.0.1", 9001, False, None),
                ("127.0.0.3", 9006, False, None),
            })
            self.assertEqual(report["config_error"], "models.json contains invalid provider or model entries")
            self.assertIsNotNone(aside._provider_origin("https://api.example.com"))
            for invalid_origin in (
                "http://0.0.0.0:9005",
                "http://127.0.0.1:bad",
                "http://attacker@127.0.0.1:9002",
                "http://localhost:9003?target=remote",
                "http://localhost:9004/proxy",
            ):
                self.assertIsNone(aside._provider_origin(invalid_origin))
            self.assertTrue(report["local_providers"]["ipv4"]["ok"])
            self.assertTrue(report["local_providers"]["ipv4-alt"]["ok"])
            self.assertEqual(report["local_providers"]["localhost"]["status"], "response too large")
            self.assertEqual(report["local_providers"]["malformed"]["status"], "invalid health response")
            self.assertEqual(report["local_providers"]["redirect"]["status"], "redirect refused")
            self.assertEqual(report["local_providers"]["slow"]["status"], "timed out")
            self.assertTrue(all("response" not in info for info in report["local_providers"].values()))
            self.assertFalse(report["ok"])
            self.assertTrue(writers and all(writer.closed for writer in writers))
            self.assertTrue(all(
                writer.data.startswith(b"GET /health HTTP/1.1\r\n")
                and b"Connection: close\r\n" in writer.data
                for writer in writers
            ))


if __name__ == "__main__":
    unittest.main()
