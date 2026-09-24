"""Account-aware config readers."""
from __future__ import annotations

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


if __name__ == "__main__":
    unittest.main()
