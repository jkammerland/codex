"""Tests for the manifest-driven native offload fleet controller."""

import json
import sys
import tempfile
import unittest
from pathlib import Path


sys.path.insert(0, str(Path(__file__).parent))
import fleet_release


class FleetReleaseTest(unittest.TestCase):
    def setUp(self) -> None:
        self.release, self.hosts = fleet_release.load_manifest(
            fleet_release.DEFAULT_MANIFEST
        )

    def test_checked_in_manifest_is_valid(self) -> None:
        self.assertEqual(self.release.marker, "+jkammerland.mcp.5")
        self.assertEqual([host.name for host in self.hosts], ["mac", "windows"])

    def test_manifest_rejects_relative_host_paths(self) -> None:
        raw = json.loads(fleet_release.DEFAULT_MANIFEST.read_text())
        raw["hosts"][0]["sourceRoot"] = "repos/codex"
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "release.json"
            path.write_text(json.dumps(raw))
            with self.assertRaisesRegex(
                fleet_release.FleetError, "absolute darwin path"
            ):
                fleet_release.load_manifest(path)

    def test_status_output_and_drift_detection(self) -> None:
        host = self.hosts[0]
        values = fleet_release.parse_status(
            "\n".join(
                [
                    f"cliVersion={self.release.cli_version}",
                    f"serverCommit={self.release.server_commit}",
                    "serverStatus=",
                    f"sourceCommit={self.release.fork_commit}",
                    "sourceStatus=",
                ]
            )
        )
        status = fleet_release.Status(host, values)
        self.assertEqual(status.drift(self.release), ())
        values["cliVersion"] = "codex-cli 0.147.0"
        drifted = fleet_release.Status(host, values)
        self.assertEqual(drifted.drift(self.release), ("cliVersion",))

    def test_activation_scripts_preserve_rollback_and_use_atomic_replacement(
        self,
    ) -> None:
        mac_script = fleet_release.activation_script(self.hosts[0], self.release)
        windows_script = fleet_release.activation_script(self.hosts[1], self.release)
        self.assertIn(".0.147.0-upstream", mac_script)
        self.assertIn("mv -f", mac_script)
        self.assertIn("ReplaceFile", windows_script)
        self.assertIn(".0.147.0-upstream", windows_script)

    def test_bundle_build_scripts_verify_the_pinned_commit(self) -> None:
        mac_script = fleet_release.build_script(self.hosts[0], self.release, True)
        windows_script = fleet_release.build_script(self.hosts[1], self.release, True)
        self.assertIn(self.release.fork_commit, mac_script)
        self.assertIn(self.release.fork_commit, windows_script)
        self.assertIn("cargo build --release", mac_script)
        self.assertIn("cargo.exe", windows_script)


if __name__ == "__main__":
    unittest.main()
