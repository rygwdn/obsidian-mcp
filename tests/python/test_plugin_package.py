import importlib.util
import json
import tempfile
import unittest
from pathlib import Path
from zipfile import ZipFile

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("package_plugin", ROOT / "scripts/package-chatgpt-plugin.py")
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class PluginPackageTests(unittest.TestCase):
    def test_private_archive_declares_real_connection_contract_and_contains_no_runtime(self):
        with tempfile.TemporaryDirectory() as folder:
            output = MODULE.build("https://fixture.example.test/mcp", "fixture-public-client", Path(folder)/"plugin.zip")
            with ZipFile(output) as archive:
                names = archive.namelist()
                self.assertEqual(len(names), 3)
                self.assertTrue(all(n.startswith("personal-obsidian-vault/") for n in names))
                manifest = json.loads(archive.read("personal-obsidian-vault/plugin.json"))
                self.assertLessEqual(len(manifest["extensions"]["com.openai"]["interface"]["shortDescription"]), 30)
                connection = json.loads(archive.read("personal-obsidian-vault/mcp.json"))["mcpServers"]["obsidian-vault"]
                self.assertEqual(connection["type"], "streamable-http")
                self.assertEqual(connection["url"], "https://fixture.example.test/mcp")
                auth = connection["extensions"]["com.openai"]["auth"]
                self.assertEqual(auth["type"], "oauth")
                self.assertEqual(auth["client"]["mode"], "provided")
                self.assertEqual(auth["client"]["tokenEndpointAuthMethod"], "none")
                self.assertEqual(auth["defaultScopes"], ["vault:read", "vault:write"])
                self.assertNotIn("clientSecret", auth["client"])
                self.assertIn(b"expected_revision", archive.read("personal-obsidian-vault/skills/obsidian-vault/SKILL.md"))

    def test_rejects_non_https_credentialed_and_non_mcp_endpoints(self):
        with tempfile.TemporaryDirectory() as folder:
            for endpoint in ["http://fixture.test/mcp", "https://user:password@fixture.test/mcp", "https://fixture.test/", "https://fixture.test/mcp?token=bad", "https://fixture.test/mcp#bad"]:
                with self.subTest(endpoint=endpoint), self.assertRaises(ValueError):
                    MODULE.build(endpoint, "fixture-client", Path(folder)/"plugin.zip")


if __name__ == "__main__":
    unittest.main()
