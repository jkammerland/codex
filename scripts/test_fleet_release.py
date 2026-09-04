"""Tests for the manifest-driven native offload fleet controller."""

import json
import hashlib
import os
import re
import shutil
import sys
import tempfile
import unittest
from pathlib import Path
from subprocess import CompletedProcess, run
from unittest import mock


sys.path.insert(0, str(Path(__file__).parent))
import fleet_release


def write_build_receipt(
    host: fleet_release.Host, release: fleet_release.Release
) -> None:
    cli = Path(host.build_binary("codex"))
    code_mode_host = Path(host.build_binary("codex-code-mode-host"))
    receipt = Path(host.build_receipt())
    receipt.parent.mkdir(parents=True, exist_ok=True)
    receipt.write_text(
        "\n".join(
            [
                "format=v1",
                f"forkCommit={release.fork_commit}",
                f"cliVersion={release.cli_version}",
                f"cliSha256={hashlib.sha256(cli.read_bytes()).hexdigest()}",
                f"hostSha256={hashlib.sha256(code_mode_host.read_bytes()).hexdigest()}",
                "",
            ]
        )
    )


class FleetReleaseTest(unittest.TestCase):
    def setUp(self) -> None:
        self.release, self.hosts = fleet_release.load_manifest(
            fleet_release.DEFAULT_MANIFEST
        )

    def test_checked_in_manifest_is_valid(self) -> None:
        self.assertEqual(self.release.marker, "+jkammerland.mcp.18")
        self.assertEqual(self.release.fork_ref, "fleet/jkammerland.mcp.18-source")
        self.assertEqual(
            self.release.runtime_version,
            "0.154.0-alpha.3+jkammerland.mcp.18",
        )
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
                    "cliSha256=cli-hash",
                    "codeModeHostSha256=host-hash",
                    "codeModeHostExecutable=true",
                    f"receiptForkCommit={self.release.fork_commit}",
                    f"receiptCliVersion={self.release.cli_version}",
                    "receiptCliSha256=cli-hash",
                    "receiptCodeModeHostSha256=host-hash",
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
        values["cliVersion"] = self.release.cli_version
        values["cliSha256"] = "different-cli-hash"
        hash_drifted = fleet_release.Status(host, values)
        self.assertEqual(hash_drifted.drift(self.release), ("cliSha256",))
        values["cliSha256"] = "cli-hash"
        values["codeModeHostExecutable"] = "false"
        mode_drifted = fleet_release.Status(host, values)
        self.assertEqual(mode_drifted.drift(self.release), ("codeModeHostExecutable",))
