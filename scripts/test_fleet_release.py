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
        self.assertEqual(self.release.rusty_v8_version, "150.4.0")
        self.assertEqual([host.name for host in self.hosts], ["mac", "windows"])
        self.assertEqual(
            [host.rust_target for host in self.hosts],
            ["aarch64-apple-darwin", "x86_64-pc-windows-msvc"],
        )

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

    def test_manifest_rejects_invalid_v8_version_and_platform_target(self) -> None:
        for key, value, message in (
            ("release.rustyV8Version", "150.4", "three numeric components"),
            ("hosts.0.rustTarget", "x86_64-unknown-linux-gnu", "apple-darwin"),
            ("hosts.1.rustTarget", "aarch64-apple-darwin", "pc-windows-msvc"),
        ):
            with self.subTest(key=key):
                raw = json.loads(fleet_release.DEFAULT_MANIFEST.read_text())
                if key == "release.rustyV8Version":
                    raw["release"]["rustyV8Version"] = value
                else:
                    host_index = int(key.split(".")[1])
                    raw["hosts"][host_index]["rustTarget"] = value
                with tempfile.TemporaryDirectory() as directory:
                    path = Path(directory) / "release.json"
                    path.write_text(json.dumps(raw))
                    with self.assertRaisesRegex(fleet_release.FleetError, message):
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
        self.assertIn(".jkammerland.mcp.5-previous", mac_script)
        self.assertIn("mv -f", mac_script)
        self.assertIn("ReplaceFile", windows_script)
        self.assertIn(".jkammerland.mcp.5-previous", windows_script)

    def test_bundle_build_scripts_verify_the_pinned_commit(self) -> None:
        mac_script = fleet_release.build_script(self.hosts[0], self.release, True)
        windows_script = fleet_release.build_script(self.hosts[1], self.release, True)
        self.assertIn(self.release.fork_commit, mac_script)
        self.assertIn(self.release.fork_commit, windows_script)
        self.assertIn('"$toolchain_bin/cargo" build --release', mac_script)
        self.assertIn('normalize_release_lock "$build"', mac_script)
        self.assertIn("0\\.0\\.0|0\\.147\\.0", mac_script)
        self.assertIn("refs/remotes/fleet/", mac_script)
        self.assertIn("rustup which --toolchain", mac_script)
        self.assertIn('if [ -e "$build" ]', mac_script)
        self.assertIn(
            "https://github.com/openai/codex/releases/download/rusty-v8-v150.4.0",
            mac_script,
        )
        self.assertIn(
            "librusty_v8_ptrcomp_sandbox_release_aarch64-apple-darwin.a.gz",
            mac_script,
        )
        self.assertIn("RUSTY_V8_ARCHIVE", mac_script)
        self.assertIn("RUSTY_V8_SRC_BINDING_PATH", mac_script)
        self.assertIn("shasum -a 256 -c -", mac_script)
        self.assertIn('v8_directory="$build.fleet/rusty-v8"', mac_script)
        self.assertIn("refs/remotes/fleet/", windows_script)
        self.assertIn(
            "Existing build root is not the clean release worktree", windows_script
        )
        self.assertIn("cargo.exe", windows_script)
        self.assertIn("$ProgressPreference = 'SilentlyContinue'", windows_script)
        self.assertIn(
            r'''$command = 'call "{0}\Common7\Tools\VsDevCmd.bat"''',
            windows_script,
        )
        self.assertNotIn(r"""call "0\Common7""", windows_script)
        self.assertIn("Normalize-ReleaseLock $build", windows_script)
        self.assertIn("0\\.0\\.0|0\\.147\\.0", windows_script)
        self.assertIn(
            "rusty_v8_ptrcomp_sandbox_release_x86_64-pc-windows-msvc.lib.gz",
            windows_script,
        )
        self.assertIn("RUSTY_V8_ARCHIVE", windows_script)
        self.assertIn("RUSTY_V8_SRC_BINDING_PATH", windows_script)
        self.assertIn("Get-FileHash", windows_script)
        self.assertIn("Join-Path ($build + '.fleet') 'rusty-v8'", windows_script)


if __name__ == "__main__":
    unittest.main()
