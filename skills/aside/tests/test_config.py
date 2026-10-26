"""Account-aware config readers."""
from __future__ import annotations

import asyncio
import json
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
            for d, model in ((u0, "m0"), (u1, "m1")):
                d.mkdir(parents=True)
                (d / "models.json").write_text(json.dumps({
                    "providers": {"gw": {"baseUrl": f"http://127.0.0.1:1/{model}", "models": [{"id": model}]}}
                }))
            with mock.patch.object(aside, "CONFIG_DIR", u0):
                self.assertEqual(aside.config_dir(), u0)
                self.assertEqual(aside.config_dir("u1"), u1)
                self.assertEqual(aside.config_dir("1"), u1)
                self.assertEqual(aside.models(), ["gw/m0"])
                self.assertEqual(aside.models(account="u1"), ["gw/m1"])
                self.assertEqual(aside.providers(account="u1"), {"gw": "http://127.0.0.1:1/m1"})
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
                with mock.patch.object(Path, "read_text", side_effect=FileNotFoundError):
                    self.assertEqual(aside._provider_config(), ({}, None))
                with mock.patch.object(Path, "read_text", side_effect=PermissionError):
                    self.assertEqual(aside._provider_config(), ({}, "models.json could not be read"))

                models.unlink()
                models.mkdir()
                self.assertEqual(aside.models(), [])
                self.assertEqual(aside.providers(), {})
                with (
                    mock.patch.object(aside, "_bin", return_value="/synthetic/aside"),
                    mock.patch.object(aside, "_app_running", return_value=True),
                    mock.patch.object(aside.urllib.request, "build_opener") as build_opener,
                ):
                    report = asyncio.run(aside.health(timeout=0.25))
                self.assertFalse(report["ok"])
                self.assertEqual(report["config_error"], "models.json could not be read")
                build_opener.assert_not_called()

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
                "unspecified": {"baseUrl": "http://0.0.0.0:9005"},
                "remote-lookalike": {"baseUrl": "https://127.0.0.1.attacker.invalid"},
                "userinfo": {"baseUrl": "http://attacker@127.0.0.1:9002"},
                "query": {"baseUrl": "http://localhost:9003?target=remote"},
                "path": {"baseUrl": "http://localhost:9004/proxy"},
            }}))
            opened: list[tuple[str, float]] = []
            read_limits: list[tuple[str, int]] = []
            handlers: list[tuple[object, ...]] = []

            class Response:
                status = 200

                def __init__(self, url: str, body: bytes):
                    self.url = url
                    self.body = body

                def __enter__(self):
                    return self

                def __exit__(self, *_args):
                    return False

                def read(self, limit: int) -> bytes:
                    read_limits.append((self.url, limit))
                    return self.body[:limit]

            class Opener:
                def open(self, url: str, timeout: float):
                    opened.append((url, timeout))
                    if "9001" in url:
                        raise aside.urllib.error.HTTPError(url, 302, "redirect", {}, None)
                    body = {
                        "http://127.0.0.1:8787/health": b'{"ok":true}',
                        "http://127.0.0.2:8788/health": b'{"ok":true}',
                        "https://localhost:9443/health": b'{"ok":true}' + b"x" * 500,
                        "http://[::1]:9000/health": b"not json",
                    }[url]
                    return Response(url, body)

            def build_opener(*new_handlers):
                handlers.append(new_handlers)
                return Opener()

            with (
                mock.patch.object(aside, "CONFIG_DIR", config),
                mock.patch.object(aside, "_bin", return_value="/synthetic/aside"),
                mock.patch.object(aside, "_app_running", return_value=True),
                mock.patch.object(aside.urllib.request, "build_opener", side_effect=build_opener),
                mock.patch.object(aside.urllib.request, "urlopen", side_effect=AssertionError("urlopen bypass")),
            ):
                report = asyncio.run(aside.health(timeout=0.25))

            self.assertEqual(
                set(report["local_providers"]),
                {"ipv4", "ipv4-alt", "localhost", "malformed", "redirect"},
            )
            self.assertEqual(opened, [
                ("http://127.0.0.1:8787/health", 0.25),
                ("http://127.0.0.2:8788/health", 0.25),
                ("https://localhost:9443/health", 0.25),
                ("http://[::1]:9000/health", 0.25),
                ("http://127.0.0.1:9001/health", 0.25),
            ])
            self.assertEqual(read_limits, [
                ("http://127.0.0.1:8787/health", 201),
                ("http://127.0.0.2:8788/health", 201),
                ("https://localhost:9443/health", 201),
                ("http://[::1]:9000/health", 201),
            ])
            self.assertTrue(report["local_providers"]["ipv4"]["ok"])
            self.assertEqual(report["local_providers"]["localhost"]["status"], "response too large")
            self.assertEqual(report["local_providers"]["malformed"]["status"], "invalid health response")
            self.assertEqual(report["local_providers"]["redirect"]["status"], "redirect refused")
            self.assertTrue(all("response" not in info for info in report["local_providers"].values()))
            self.assertFalse(report["ok"])
            self.assertEqual(len(handlers), 5)
            self.assertTrue(all(
                len(handler_group) == 2
                and isinstance(handler_group[0], aside.urllib.request.ProxyHandler)
                and isinstance(handler_group[1], aside.urllib.request.HTTPRedirectHandler)
                and handler_group[1].redirect_request(None, None, 302, "", {}, "https://remote") is None
                for handler_group in handlers
            ))


if __name__ == "__main__":
    unittest.main()
