"""Tests for the manifest-driven native offload fleet controller."""

import base64
import json
import hashlib
import ntpath
import os
import posixpath
import re
import signal
import shutil
import sys
import tempfile
import time
import unittest
from dataclasses import replace
from pathlib import Path
from subprocess import PIPE, CompletedProcess, Popen, TimeoutExpired, run
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

    @unittest.skipUnless(sys.platform.startswith("linux"), "Linux installer test")
    def test_linux_installer_resolves_only_the_active_managed_install(self) -> None:
        result = run(
            [
                "sh",
                str(Path(__file__).parent / "tests" / "install_user_fork_paths.sh"),
            ],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

        installer = (Path(__file__).parent / "install-user-fork.sh").read_text()
        self.assertIn("SELECTED_CODEX_HASH", installer)
        self.assertIn("selected Codex binaries changed during the build", installer)
        self.assertIn(
            "UPSTREAM_VERSION=$(printf '%s\\n' \"$CURRENT_VERSION\"", installer
        )
        self.assertNotIn(
            "UPSTREAM_VERSION=$(printf '%s\\n' \"$BUILT_VERSION\"", installer
        )

    @unittest.skipUnless(sys.platform.startswith("linux"), "Linux installer test")
    def test_linux_installer_preserves_incompatible_daemons(self) -> None:
        daemon_result = run(
            [
                "sh",
                str(Path(__file__).parent / "tests" / "install_user_fork_daemon.sh"),
            ],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(daemon_result.returncode, 0, daemon_result.stderr)

    @unittest.skipUnless(sys.platform.startswith("linux"), "Linux installer test")
    def test_linux_installer_normalizes_tagged_release_lock(self) -> None:
        release_lock_result = run(
            [
                "sh",
                str(
                    Path(__file__).parent
                    / "tests"
                    / "install_user_fork_release_lock.sh"
                ),
            ],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(release_lock_result.returncode, 0, release_lock_result.stderr)

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

    def test_manifest_requires_filename_safe_release_name(self) -> None:
        for release_name in ("a", "fork.alpha-3_7", "a" * 64):
            with self.subTest(valid=release_name):
                raw = json.loads(fleet_release.DEFAULT_MANIFEST.read_text())
                raw["schemaVersion"] = 2
                raw["release"]["name"] = release_name
                with tempfile.TemporaryDirectory() as directory:
                    path = Path(directory) / "release.json"
                    path.write_text(json.dumps(raw))
                    release, _hosts = fleet_release.load_manifest(path)
                    self.assertEqual(release.name, release_name)

        for release_name in (
            "Uppercase",
            ".hidden",
            "trailing-",
            "two..dots",
            "../escape",
            "name with spaces",
            "safe'; Write-Output injected; #",
            "a" * 65,
        ):
            with self.subTest(invalid=release_name):
                raw = json.loads(fleet_release.DEFAULT_MANIFEST.read_text())
                raw["schemaVersion"] = 2
                raw["release"]["name"] = release_name
                with tempfile.TemporaryDirectory() as directory:
                    path = Path(directory) / "release.json"
                    path.write_text(json.dumps(raw))
                    with self.assertRaisesRegex(
                        fleet_release.FleetError,
                        "filename-safe lowercase identifier",
                    ):
                        fleet_release.load_manifest(path)

    def test_manifest_rejects_command_injection_boundaries(self) -> None:
        cases = (
            ("release.forkRef", "refs/heads/main", "abbreviated branch name"),
            ("release.forkRef", "fleet/main:evil", "abbreviated branch name"),
            ("release.forkRef", "fleet/main..evil", "abbreviated branch name"),
            ("release.forkRef", "fleet/main.lock", "abbreviated branch name"),
            ("release.forkRemote", "--upload-pack=payload", "option-like"),
            (
                "release.expectedCliVersion",
                "codex-cli 0.154.0'; Write-Output injected; #",
                "semantic version",
            ),
            ("hosts.0.sshTarget", "-oProxyCommand=payload", "option-like"),
            ("hosts.0.sshTarget", "host name", "whitespace"),
            ("hosts.0.rustToolchain", "1.95.0 && payload", "toolchain name"),
            (
                "hosts.1.rustTarget",
                "x&payload-pc-windows-msvc",
                "target triple",
            ),
            ("hosts.0.rustup", "/safe/rustup\npayload", "control characters"),
            (
                "hosts.1.cliBinary",
                "C:\\safe\\codex.exe\npayload",
                "control characters",
            ),
        )
        for key, value, message in cases:
            with self.subTest(key=key, value=value):
                raw = json.loads(fleet_release.DEFAULT_MANIFEST.read_text())
                raw["schemaVersion"] = 2
                section, field = key.rsplit(".", 1)
                if section == "release":
                    raw[section][field] = value
                else:
                    _, host_index = section.split(".")
                    raw["hosts"][int(host_index)][field] = value
                with tempfile.TemporaryDirectory() as directory:
                    path = Path(directory) / "release.json"
                    path.write_text(json.dumps(raw))
                    with self.assertRaisesRegex(fleet_release.FleetError, message):
                        fleet_release.load_manifest(path)

    def test_schema_v1_retains_original_manifest_acceptance_contract(self) -> None:
        raw = json.loads(fleet_release.DEFAULT_MANIFEST.read_text())
        self.assertEqual(raw["schemaVersion"], 1)
        raw["release"].update(
            {
                "name": "Previously Accepted Release",
                "forkRef": "refs/heads/previously-accepted",
                "forkRemote": "--previously-accepted-remote",
                "expectedCliVersion": "codex-cli previously-accepted",
            }
        )
        raw["hosts"][0]["sshTarget"] = "previously accepted target"
        raw["hosts"][0]["rustToolchain"] = "previously accepted toolchain"
        raw["hosts"][1]["rustTarget"] = "x&previous-pc-windows-msvc"
        raw["hosts"][1]["cliBinary"] = "C:\\previous\naccepted\\codex.exe"

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "release.json"
            path.write_text(json.dumps(raw))
            release, hosts = fleet_release.load_manifest(path)

        self.assertEqual(release.name, "Previously Accepted Release")
        self.assertEqual(release.fork_ref, "refs/heads/previously-accepted")
        self.assertEqual(hosts[0].ssh_target, "previously accepted target")
        self.assertEqual(hosts[1].rust_target, "x&previous-pc-windows-msvc")

    def test_schema_v1_unsafe_command_values_are_rejected_at_execution(self) -> None:
        raw = json.loads(fleet_release.DEFAULT_MANIFEST.read_text())
        raw["hosts"][0]["sshTarget"] = "-oProxyCommand=payload"
        raw["hosts"][0]["rustToolchain"] = "1.95.0 & whoami"
        with tempfile.TemporaryDirectory() as directory:
            manifest = Path(directory) / "release.json"
            manifest.write_text(json.dumps(raw))
            release, hosts = fleet_release.load_manifest(manifest)
            bundle = Path(directory) / "release.bundle"
            bundle.write_bytes(b"bundle")

            with mock.patch("fleet_release.subprocess.run") as subprocess_run:
                with self.assertRaisesRegex(fleet_release.FleetError, "option-like"):
                    fleet_release.remote(hosts[0], "true")
                with self.assertRaisesRegex(fleet_release.FleetError, "option-like"):
                    fleet_release.copy_bundle(hosts[0], release, bundle)
                subprocess_run.assert_not_called()

            with self.assertRaisesRegex(
                fleet_release.FleetError,
                "safe rustup toolchain name",
            ):
                fleet_release.build_script(hosts[0], release, False)

    def test_remote_transports_end_options_before_the_ssh_target(self) -> None:
        completed = CompletedProcess([], 0, "", "")
        with mock.patch(
            "fleet_release.subprocess.run",
            return_value=completed,
        ) as subprocess_run:
            fleet_release.remote(self.hosts[0], "true")
            ssh_command = subprocess_run.call_args.args[0]
            self.assertEqual(
                ssh_command[ssh_command.index("--") + 1],
                self.hosts[0].ssh_target,
            )

            with tempfile.TemporaryDirectory() as directory:
                bundle = Path(directory) / "release.bundle"
                bundle.write_bytes(b"bundle")
                fleet_release.copy_bundle(self.hosts[0], self.release, bundle)
            scp_command = subprocess_run.call_args.args[0]
            self.assertEqual(
                scp_command[scp_command.index("--") + 2].split(":", 1)[0],
                self.hosts[0].ssh_target,
            )

    def test_schema_v2_is_an_explicit_strict_manifest_contract(self) -> None:
        raw = json.loads(fleet_release.DEFAULT_MANIFEST.read_text())
        raw["schemaVersion"] = 2
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "release.json"
            path.write_text(json.dumps(raw))
            release, hosts = fleet_release.load_manifest(path)

        self.assertEqual(release, self.release)
        self.assertEqual(hosts, self.hosts)

    def test_build_commands_quote_composed_refspec_and_toolchain(self) -> None:
        mac_script = fleet_release.build_script(self.hosts[0], self.release, True)
        windows_script = fleet_release.build_script(self.hosts[1], self.release, True)
        refspec = (
            f"refs/heads/{self.release.fork_ref}:"
            f"refs/remotes/fleet/{self.release.fork_ref}"
        )

        self.assertIn(
            f"fetch '{self.hosts[1].bundle_path(self.release)}' '{refspec}'",
            windows_script,
        )
        self.assertIn(
            f"fetch {self.hosts[0].bundle_path(self.release)} {refspec}", mac_script
        )
        self.assertNotIn(f"{self.release.fork_ref}':refs/remotes", windows_script)
        self.assertIn("$toolchainArgument = '+' + '1.95.0'", windows_script)
        self.assertIn("$env:CODEX_FLEET_TOOLCHAIN = $toolchainArgument", windows_script)

    def test_windows_build_keeps_manifest_paths_out_of_cmd_source(self) -> None:
        hostile_build = r"C:\fleet\%FLEET_PERCENT%\!FLEET_DELAY!\build"
        host = replace(self.hosts[1], build_root=hostile_build)

        script = fleet_release.build_script(host, self.release, False)
        command_assignment = next(
            line for line in script.splitlines() if line.startswith("$command = ")
        )

        self.assertNotIn(hostile_build, command_assignment)
        self.assertIn("$env:CODEX_FLEET_BUILD_ROOT = $build", script)
        self.assertIn('"%CODEX_FLEET_BUILD_ROOT%\\codex-rs"', command_assignment)
        self.assertNotIn("call ", command_assignment.lower())
        self.assertIn("cmd.exe /d /v:off /s /c $command", script)

    def test_windows_target_lock_is_stable_across_binaries_and_aliases(self) -> None:
        windows = self.hosts[1]
        base = replace(
            windows,
            cli_binary=r"C:\fleet\cli\codex.exe",
            code_mode_host_binary=r"C:\fleet\host\codex-code-mode-host.exe",
        )
        shared_cli = replace(
            base,
            code_mode_host_binary=r"C:\other\host\codex-code-mode-host.exe",
        )
        shared_host = replace(base, cli_binary=r"C:\other\cli\codex.exe")
        aliased = replace(
            base,
            cli_binary=r"c:\FLEET\cli\.\CODEX.exe",
            code_mode_host_binary=(r"C:\fleet\host\nested\..\codex-code-mode-host.exe"),
        )

        unrelated = replace(
            base,
            cli_binary=r"D:\unrelated\cli\codex.exe",
            code_mode_host_binary=r"E:\unrelated\host\codex-code-mode-host.exe",
        )

        expected = (fleet_release.WINDOWS_INSTALLATION_LOCK_PATH,)
        self.assertEqual(fleet_release.target_lock_paths(base), expected)
        self.assertEqual(fleet_release.target_lock_paths(shared_cli), expected)
        self.assertEqual(fleet_release.target_lock_paths(shared_host), expected)
        self.assertEqual(fleet_release.target_lock_paths(aliased), expected)
        self.assertEqual(fleet_release.target_lock_paths(unrelated), expected)
        injected = r"C:\fleet-test\activation.lock"
        self.assertEqual(
            fleet_release.target_lock_paths(
                base,
                windows_installation_lock_path=injected,
            ),
            (injected,),
        )

    def test_rollback_namespace_is_scoped_to_the_full_target_pair(self) -> None:
        for host in self.hosts:
            path_module = ntpath if host.windows else posixpath
            directory = path_module.dirname(host.cli_binary)
            first = replace(
                host,
                cli_binary=path_module.join(directory, "first-codex"),
                code_mode_host_binary=path_module.join(directory, "first-host"),
            )
            second = replace(
                host,
                cli_binary=path_module.join(directory, "second-codex"),
                code_mode_host_binary=path_module.join(directory, "second-host"),
            )
            with self.subTest(platform=host.platform):
                self.assertNotEqual(
                    fleet_release.rollback_paths(first, self.release.name),
                    fleet_release.rollback_paths(second, self.release.name),
                )
                self.assertNotEqual(
                    fleet_release.transaction_path(first),
                    fleet_release.transaction_path(second),
                )
                self.assertEqual(
                    fleet_release.release_only_rollback_paths(first, self.release.name),
                    fleet_release.release_only_rollback_paths(
                        second, self.release.name
                    ),
                )

        windows = self.hosts[1]
        case_distinct = replace(
            windows,
            cli_binary=windows.cli_binary.replace("codex.exe", "CODEX.exe"),
        )
        self.assertNotEqual(
            fleet_release.rollback_paths(windows, self.release.name),
            fleet_release.rollback_paths(case_distinct, self.release.name),
        )
        mac_script = fleet_release.activation_script(self.hosts[0], self.release)
        windows_script = fleet_release.activation_script(self.hosts[1], self.release)
        self.assertIn("parent = path.parent.resolve(strict=True)", mac_script)
        self.assertIn("target_pair_token(target_cli, target_host)", mac_script)
        self.assertIn("Get-TargetSlotIdentity", windows_script)
        self.assertIn("IsCaseSensitiveDirectory($directory)", windows_script)
        self.assertIn("Get-PhysicalPathIdentity $directory", windows_script)

    def test_status_output_and_drift_detection(self) -> None:
        host = self.hosts[0]
        values = fleet_release.parse_status(
            "\n".join(
                [
                    "activationJournal=none",
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
        values["codeModeHostExecutable"] = "true"
        values["activationJournal"] = "pending"
        journal_drifted = fleet_release.Status(host, values)
        self.assertEqual(journal_drifted.drift(self.release), ("activationJournal",))

        for candidate_host in self.hosts:
            script = fleet_release.status_script(candidate_host)
            self.assertIn("activationJournal", script)
            self.assertIn(fleet_release.transaction_path(candidate_host), script)
            self.assertIn("-transaction.json", script)

    @unittest.skipUnless(os.name == "posix", "POSIX status script test")
    def test_status_ignores_journals_for_an_unrelated_target_pair(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target_directory = root / "bin"
            target_directory.mkdir()
            cli = target_directory / "codex"
            cli.write_text("#!/bin/sh\necho 'codex-cli expected'\n")
            cli.chmod(0o755)
            code_mode_host = target_directory / "codex-code-mode-host"
            code_mode_host.write_bytes(b"host")
            code_mode_host.chmod(0o755)
            unrelated = target_directory / ".fleet-transaction.unrelated.json"
            unrelated.write_text(
                json.dumps(
                    {
                        "targetCli": str(root / "other" / "codex"),
                        "targetHost": str(root / "other" / "host"),
                    }
                )
            )
            host = replace(
                self.hosts[0],
                cli_binary=str(cli),
                code_mode_host_binary=str(code_mode_host),
                source_root=str(root / "missing-source"),
                build_root=str(root / "missing-build"),
                server_root=str(root / "missing-server"),
            )

            result = run(
                ["sh", "-c", fleet_release.status_script(host)],
                check=False,
                capture_output=True,
                text=True,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                fleet_release.parse_status(result.stdout)["activationJournal"],
                "none",
            )

    @unittest.skipUnless(sys.platform == "win32", "Windows status script test")
    def test_windows_status_handles_absent_installation_parent(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            host = replace(
                self.hosts[1],
                cli_binary=str(root / "absent" / "codex.exe"),
                code_mode_host_binary=str(root / "absent" / "host.exe"),
                source_root=str(root / "missing-source"),
                build_root=str(root / "missing-build"),
                server_root=str(root / "missing-server"),
            )
            result = run(
                [
                    "powershell.exe",
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "-",
                ],
                check=False,
                capture_output=True,
                input=fleet_release.status_script(host),
                text=True,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                fleet_release.parse_status(result.stdout)["activationJournal"],
                "none",
            )

    def test_windows_remote_streams_large_scripts_over_stdin(self) -> None:
        script = "Write-Output 'Grüße 世界'\n" * 5000
        powershell, payload = fleet_release.windows_powershell_stdin(script)
        completed = CompletedProcess([], 0, "", "")
        with mock.patch(
            "fleet_release.subprocess.run",
            return_value=completed,
        ) as subprocess_run:
            result = fleet_release.remote(self.hosts[1], script)

        self.assertEqual(result, completed)
        command = subprocess_run.call_args.args[0]
        self.assertEqual(command[-len(powershell) :], powershell)
        self.assertLess(len(" ".join(powershell)), 4096)
        self.assertEqual(
            base64.b64decode(payload).decode("utf-16le"),
            script,
        )
        self.assertEqual(subprocess_run.call_args.kwargs["input"], payload)
        self.assertEqual(subprocess_run.call_args.kwargs["encoding"], "utf-8")

    @unittest.skipUnless(sys.platform == "win32", "Windows transport test")
    def test_windows_stdin_transport_preserves_unicode_and_failure(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            result_path = Path(directory) / "unicode-result.txt"
            script = (
                ("# large Unicode payload: 世界\n" * 5000)
                + "[System.IO.File]::WriteAllText("
                + fleet_release.quote_ps(str(result_path))
                + ", 'Grüße 世界', [System.Text.UTF8Encoding]::new($false))\n"
                + "exit 23\n"
            )
            command, payload = fleet_release.windows_powershell_stdin(script)

            result = run(
                command,
                check=False,
                capture_output=True,
                encoding="utf-8",
                input=payload,
                text=True,
            )

            self.assertEqual(result.returncode, 23, result.stderr)
            self.assertEqual(result_path.read_text(encoding="utf-8"), "Grüße 世界")

    @unittest.skipUnless(sys.platform == "win32", "Windows cmd transport test")
    def test_windows_build_command_does_not_rescan_environment_paths(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = (
                Path(directory)
                / "%CODEX_FLEET_HOSTILE_PERCENT%"
                / "!CODEX_FLEET_HOSTILE_DELAY!"
            )
            build = root / "build"
            cargo = root / "cargo.cmd"
            vsdev = root / "VsDevCmd.cmd"
            result_path = Path(directory) / "build-command-result.txt"
            (build / "codex-rs").mkdir(parents=True)
            vsdev.write_text(
                '@echo off\nset "CODEX_FLEET_VSDEV_READY=ready"\nexit /b 0\n'
            )
            cargo.write_text(
                "@echo off\n"
                'if not "%CODEX_FLEET_VSDEV_READY%"=="ready" exit /b 41\n'
                '> "%CODEX_FLEET_RESULT%" echo %CD%\n'
                '>> "%CODEX_FLEET_RESULT%" echo %*\n'
                "exit /b 0\n"
            )
            environment = os.environ.copy()
            environment.update(
                {
                    "CODEX_FLEET_VSDEVCMD": str(vsdev),
                    "CODEX_FLEET_BUILD_ROOT": str(build),
                    "CODEX_FLEET_CARGO": str(cargo),
                    "CODEX_FLEET_TOOLCHAIN": "+1.95.0",
                    "CODEX_FLEET_RESULT": str(result_path),
                    "CODEX_FLEET_HOSTILE_PERCENT": "expanded & exit /b 91",
                    "CODEX_FLEET_HOSTILE_DELAY": "expanded & exit /b 92",
                }
            )

            result = run(
                [
                    "cmd.exe",
                    "/d",
                    "/v:off",
                    "/s",
                    "/c",
                    fleet_release.WINDOWS_BUILD_COMMAND,
                ],
                check=False,
                capture_output=True,
                text=True,
                env=environment,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            command_result = result_path.read_text().splitlines()
            self.assertEqual(
                ntpath.normcase(command_result[0].strip()),
                ntpath.normcase(str(build / "codex-rs")),
            )
            self.assertEqual(
                command_result[1].strip(),
                '"+1.95.0" build --release --bin codex --bin codex-code-mode-host',
            )

    def test_activation_scripts_preserve_rollback_and_use_atomic_replacement(
        self,
    ) -> None:
        mac_script = fleet_release.activation_script(self.hosts[0], self.release)
        windows_script = fleet_release.activation_script(self.hosts[1], self.release)
        rollback_cli, rollback_host = fleet_release.rollback_paths(
            self.hosts[0], self.release.name
        )
        self.assertIn(rollback_cli, mac_script)
        self.assertIn(rollback_host, mac_script)
        self.assertIn("os.replace(candidate_cli, target_cli)", mac_script)
        self.assertIn("MoveFileEx", windows_script)
        self.assertIn("MOVEFILE_REPLACE_EXISTING", windows_script)
        self.assertIn("MOVEFILE_WRITE_THROUGH", windows_script)
        self.assertNotIn("MOVEFILE_COPY_ALLOWED", windows_script)
        self.assertIn(
            "stage and destination must be on the same volume",
            windows_script,
        )
        windows_rollback = fleet_release.rollback_paths(
            self.hosts[1], self.release.name
        )
        self.assertNotEqual(windows_rollback[0], windows_rollback[1])
        self.assertIn("Get-TargetPairToken", windows_script)
        self.assertIn("Get-TransactionPath", windows_script)
        self.assertIn("Move-RollbackStateToTargetPair", windows_script)
        self.assertIn("..fleet-transaction.json", mac_script)
        self.assertIn("..fleet-transaction.json", windows_script)
        self.assertIn("def pending_transaction(", mac_script)
        self.assertIn("Multiple activation transaction journals", mac_script)
        self.assertIn("Get-PendingTransaction", windows_script)
        self.assertIn("Get-RollbackPaths", windows_script)
        self.assertIn("Get-PhysicalPathIdentity", windows_script)
        self.assertIn("GetFileInformationByHandle", windows_script)
        self.assertIn("VolumeSerialNumber", windows_script)
        self.assertIn("FileIndexHigh", windows_script)
        self.assertIn("FileCaseSensitiveInfo", windows_script)
        self.assertNotIn("return $identity.ToUpperInvariant()", windows_script)
        self.assertIn(
            'require_clean_git(build, CONFIG["fork_commit"], "Build worktree")',
            mac_script,
        )
        self.assertIn("Assert-CleanGit $build", windows_script)
        self.assertIn('[target_cli, "agents", "--help"]', mac_script)
        self.assertIn("@('agents', '--help')", windows_script)
        self.assertIn('[target_host, "--help"]', mac_script)
        self.assertIn("Invoke-HealthProbe $targetHost", windows_script)
        self.assertIn(
            "Build receipt does not match the reviewed binary pair", mac_script
        )
        self.assertIn(
            "Build receipt does not match the reviewed binary pair", windows_script
        )
        self.assertIn("restore_pair", mac_script)
        self.assertIn("Restore-Pair $TransactionPath $transaction", windows_script)
        self.assertIn(
            'require_clean_git(server, CONFIG["server_commit"], "MCP server")',
            mac_script,
        )
        self.assertIn("Assert-CleanGit $server", windows_script)
        self.assertIn("ensure_daemon_runtime_compatible", mac_script)
        self.assertIn("disable_daemon_auto_update", mac_script)
        self.assertNotIn('daemon_helper="$canonical', mac_script)
        self.assertIn("Assert-DaemonRuntimeCompatible", windows_script)
        self.assertIn("Disable-DaemonAutoUpdate", windows_script)
        self.assertNotIn("Assert-DaemonRuntimeCompatible $sourceCli", windows_script)
        self.assertNotIn("Disable-DaemonAutoUpdate $sourceCli", windows_script)
        self.assertIn(self.release.runtime_version, windows_script)
        self.assertLess(
            windows_script.index(
                "Assert-DaemonRuntimeCompatible $reviewedCliStage.Path"
            ),
            windows_script.index("Publish-PreparedFile $candidateCli $targetCli"),
        )
        self.assertLess(
            windows_script.index(
                "Assert-DaemonRuntimeCompatible $reviewedCliStage.Path"
            ),
            windows_script.index(
                "Disable-DaemonAutoUpdate $reviewedCliStage.Path $codexHome"
            ),
        )
        self.assertLess(
            windows_script.index(
                "Disable-DaemonAutoUpdate $reviewedCliStage.Path $codexHome"
            ),
            windows_script.index("Publish-PreparedFile $candidateCli $targetCli"),
        )

        mac_main = mac_script.index("def validate_and_activate():")
        mac_disable = mac_script.index(
            "disable_daemon_auto_update(reviewed_cli, codex_home)", mac_main
        )
        mac_target_lease = mac_script.index(
            'target_lock_path,\n                        "installation operation"',
            mac_disable,
        )
        mac_lease = mac_script.index(
            'lock_path,\n                    "daemon operation"', mac_target_lease
        )
        mac_transaction = mac_script.index("activate_under_lock(", mac_lease)
        self.assertLess(mac_disable, mac_target_lease)
        self.assertLess(mac_target_lease, mac_lease)
        self.assertLess(mac_lease, mac_transaction)

        mac_transaction_definition = mac_script.index("def activate_under_lock(")
        mac_strict_precheck = mac_script.index(
            "strict=True", mac_transaction_definition
        )
        mac_replacement = mac_script.index(
            "os.replace(candidate_host, target_host)", mac_strict_precheck
        )
        mac_postcheck = mac_script.index("strict=True", mac_replacement)
        mac_rollback = mac_script.index("recover_transaction(", mac_postcheck)
        self.assertLess(mac_strict_precheck, mac_replacement)
        self.assertLess(mac_replacement, mac_postcheck)
        self.assertLess(mac_postcheck, mac_rollback)

        windows_disable = windows_script.index(
            "Disable-DaemonAutoUpdate $reviewedCliStage.Path $codexHome"
        )
        windows_lease = windows_script.index(
            "Acquire-ActivationLease $guardian",
            windows_disable,
        )
        windows_strict_precheck = windows_script.index(
            "Assert-DaemonRuntimeCompatible $candidateCli", windows_lease
        )
        windows_replacement = windows_script.index(
            "Publish-PreparedFile $candidateHost $targetHost"
        )
        windows_postcheck = windows_script.index(
            "Assert-DaemonRuntimeCompatible $targetCli", windows_replacement
        )
        windows_release = windows_script.index(
            "Complete-ActivationGuardian $guardian", windows_postcheck
        )
        self.assertLess(windows_disable, windows_lease)
        self.assertLess(windows_lease, windows_strict_precheck)
        self.assertLess(windows_strict_precheck, windows_replacement)
        self.assertLess(windows_replacement, windows_postcheck)
        self.assertLess(windows_postcheck, windows_release)
        self.assertLess(
            windows_script.index(
                "Recover-Transaction $transactionPath", windows_replacement
            ),
            windows_release,
        )
        windows_revalidation = windows_script.index(
            "Assert-CleanGit $build", windows_lease
        )
        windows_mutable_validation = windows_script.index(
            "foreach ($path in @($targetCli, $targetHost))", windows_lease
        )
        windows_recovery = windows_script.index(
            "$pendingTransaction = Get-PendingTransaction", windows_lease
        )
        self.assertLess(windows_lease, windows_recovery)
        self.assertLess(windows_recovery, windows_revalidation)
        self.assertLess(windows_revalidation, windows_mutable_validation)
        self.assertIn(
            "Activation binary pair changed while waiting for the operation lease",
            windows_script,
        )
        self.assertIn(
            "Write-TransactionState $transactionPath $transaction", windows_script
        )
        for phase in (
            "prepared",
            "cliReplaced",
            "pairReplaced",
            "hostRestored",
            "pairRestored",
            "cleanup",
            "committed",
        ):
            self.assertIn(f"'{phase}'", windows_script)
        windows_journal = windows_script.index(
            "Write-TransactionState $transactionPath $transaction", windows_lease
        )
        windows_backup_cli = windows_script.index(
            "Publish-VerifiedCopy $targetCli $backupCli", windows_journal
        )
        windows_prepared = windows_script.index(
            "Set-TransactionPhase $transactionPath $transaction 'prepared'",
            windows_backup_cli,
        )
        windows_cli_replace = windows_script.index(
            "Publish-PreparedFile $candidateCli $targetCli", windows_prepared
        )
        windows_cli_phase = windows_script.index(
            "Set-TransactionPhase $transactionPath $transaction 'cliReplaced'",
            windows_cli_replace,
        )
        windows_host_replace = windows_script.index(
            "Publish-PreparedFile $candidateHost $targetHost", windows_cli_phase
        )
        windows_pair_phase = windows_script.index(
            "Set-TransactionPhase $transactionPath $transaction 'pairReplaced'",
            windows_host_replace,
        )
        self.assertLess(windows_journal, windows_backup_cli)
        self.assertLess(windows_backup_cli, windows_prepared)
        self.assertLess(windows_prepared, windows_cli_replace)
        self.assertLess(windows_cli_replace, windows_cli_phase)
        self.assertLess(windows_cli_phase, windows_host_replace)
        self.assertLess(windows_host_replace, windows_pair_phase)
        self.assertEqual(windows_script.count("Assert-CleanGit $build"), 2)
        self.assertIn("$stream.Flush($true)", windows_script)
        self.assertIn(
            "New-PrivateReviewedCli $sourceCli $targetCli $reviewedSourceCliHash",
            windows_script,
        )
        self.assertNotIn(
            "public sealed class FleetDaemonOperationLease", windows_script
        )
        self.assertNotIn("public static class FleetKillOnCloseJob", windows_script)
        self.assertIn("public static class FleetGuardianJob", windows_script)
        self.assertIn("$stream.Lock(0, 1)", windows_script)
        self.assertIn("[System.Diagnostics.Stopwatch]::StartNew()", windows_script)
        self.assertNotIn("DateTime.UtcNow", windows_script)
        self.assertIn("JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE", windows_script)
        self.assertIn("AssignProcessToJobObject", windows_script)
        self.assertIn("TerminateJobObject", windows_script)
        self.assertIn("QueryInformationJobObject", windows_script)
        self.assertNotIn("$startInfo.ArgumentList", windows_script)
        self.assertIn("$startInfo.EnvironmentVariables", windows_script)
        self.assertIn("-EncodedCommand $encodedGuardianCommand", windows_script)
        self.assertIn("$jobInstalled = $true", windows_script)
        self.assertIn("$startupComplete = $true", windows_script)
        self.assertIn("[FleetGuardianJob]::TerminateAndWait()", windows_script)
        self.assertIn("$containmentCleared = $true", windows_script)
        self.assertIn("Wait-GuardianTask $process $connect 10", windows_script)
        self.assertIn("lease handshake'", windows_script)
        self.assertIn("completion handshake'", windows_script)
        self.assertIn("Stop-ActivationOnTimeout", windows_script)
        self.assertIn("Invoke-HealthProbe", windows_script)
        self.assertIn("[System.Diagnostics.Stopwatch]::StartNew()", windows_script)
        self.assertNotIn("Wait-Job -Timeout", windows_script)
        windows_main = windows_script.index("$canonical = ")
        windows_guardian_start = windows_script.index(
            "$guardian = Start-ActivationGuardian $targetLockPaths $daemonLockPath",
            windows_main,
        )
        self.assertLess(
            windows_guardian_start,
            windows_script.index("Assert-CleanGit $canonical", windows_guardian_start),
        )
        guardian_termination = windows_script.index(
            "[FleetGuardianJob]::TerminateAndWait()"
        )
        guardian_unlock = windows_script.index("$daemonLease.Unlock(0, 1)")
        self.assertLess(guardian_termination, guardian_unlock)
        self.assertIn("TryActiveProcesses(out activeProcesses)", windows_script)
        self.assertIn("while (-not $containmentCleared)", windows_script)
        self.assertIn("('.fleet-reviewed.' + [guid]::NewGuid()", windows_script)
        self.assertIn(
            "$prefix = '.fleet-reviewed.'",
            windows_script,
        )
        self.assertIn(
            "Remove-StaleReviewedCliStages $targetCli $reviewedCliStage.Directory",
            windows_script,
        )
        self.assertIn(
            "if ($_.FullName -eq $PreserveDirectory) { return }",
            windows_script,
        )
        self.assertIn(
            "$_.Attributes -band [System.IO.FileAttributes]::ReparsePoint",
            windows_script,
        )
        windows_private_cli = windows_script.index(
            "$reviewedCliStage = New-PrivateReviewedCli"
        )
        windows_first_daemon_execution = windows_script.index(
            "Assert-DaemonRuntimeCompatible $reviewedCliStage.Path"
        )
        self.assertLess(windows_private_cli, windows_first_daemon_execution)
        self.assertIn("Join-Path $codexHome 'app-server-daemon'", windows_script)
        self.assertIn("$targetLockPaths = @(", windows_script)
        self.assertLess(
            windows_script.index(
                "Acquire-OperationLease $targetLockPath ''installation operation''"
            ),
            windows_script.index(
                "Acquire-OperationLease $DaemonLockPath ''daemon operation''"
            ),
        )
        self.assertIn("os.set_inheritable(lock_file.fileno(), False)", mac_script)
        self.assertIn("signal.pthread_sigmask", mac_script)
        self.assertIn(
            "signal.pthread_sigmask(signal.SIG_UNBLOCK, HANDLED_SIGNALS)",
            mac_script,
        )
        self.assertIn("if pending_signal is not None:", mac_script)
        mac_private_cli = mac_script.index(
            "reviewed_directory, reviewed_cli, reviewed_ownership = "
            "stage_private_reviewed_cli("
        )
        self.assertLess(
            mac_private_cli,
            mac_script.index(
                "ensure_daemon_runtime_compatible(\n            reviewed_cli",
                mac_private_cli,
            ),
        )
        self.assertNotIn(
            "ensure_daemon_runtime_compatible(\n        source_cli",
            mac_script,
        )
        self.assertIn("wrapper_lock_fd = os.dup(lock_fd)", mac_script)
        self.assertIn(
            "pass_fds=LEASE_FDS",
            mac_script,
        )
        self.assertIn("HEALTH_PROBE_TIMEOUT_SECONDS = 30", mac_script)
        self.assertIn("time.monotonic() + timeout_seconds", mac_script)
        self.assertIn(
            "timeout_seconds=HEALTH_PROBE_TIMEOUT_SECONDS",
            mac_script,
        )
        mac_updater = mac_script[
            mac_script.index("def disable_daemon_auto_update(") : mac_script.index(
                "def copy_to_stage("
            )
        ]
        self.assertNotIn("timeout_seconds", mac_updater)
        windows_updater = windows_script[
            windows_script.index(
                "function Disable-DaemonAutoUpdate("
            ) : windows_script.index("function Publish-PreparedFile(")
        ]
        self.assertIn("Invoke-CodexWithHome", windows_updater)
        self.assertNotIn("Invoke-HealthProbe", windows_updater)
        self.assertIn("close_fds=True", mac_script)
        self.assertIn("start_new_session=True", mac_script)
        self.assertIn("os.killpg(process.pid, signal.SIGKILL)", mac_script)
        self.assertIn("LOCK_TIMEOUT_SECONDS = 75", mac_script)
        self.assertIn("LOCK_RETRY_SECONDS = 0.05", mac_script)
        self.assertNotIn("mkfifo", mac_script)
        self.assertNotIn("fleet-activation.", mac_script)
        self.assertIn(
            'set_transaction_phase(transaction_path, transaction, "prepared")',
            mac_script,
        )
        self.assertIn(
            'set_transaction_phase(transaction_path, transaction, "cliReplaced")',
            mac_script,
        )
        self.assertIn(
            'set_transaction_phase(transaction_path, transaction, "pairReplaced")',
            mac_script,
        )
        self.assertIn(
            'set_transaction_phase(transaction_path, transaction, "committed")',
            mac_script,
        )
        self.assertIn(
            "$targetPairRollback.Cli, $targetPairRollback.Host",
            windows_script,
        )
        self.assertIn(
            "[Environment]::ExpandEnvironmentVariables($_)",
            windows_script,
        )

    def test_windows_guardian_keeps_protocol_open_after_acquisition_error(
        self,
    ) -> None:
        guardian = fleet_release.WINDOWS_ACTIVATION_GUARDIAN
        acquisition_error = guardian.index(
            '$writer.WriteLine("error:$($_.Exception.Message)")'
        )
        completion_command = guardian.index("if ($command -eq 'complete')")

        self.assertEqual(
            guardian[acquisition_error:].splitlines()[1].strip(),
            "continue",
        )
        self.assertLess(completion_command, acquisition_error)

    def test_windows_write_through_publish_stages_per_destination_volume(self) -> None:
        host = replace(
            self.hosts[1],
            cli_binary=r"C:\fleet\cli\codex.exe",
            code_mode_host_binary=r"D:\fleet\host\codex-code-mode-host.exe",
        )
        script = fleet_release.activation_script(host, self.release)
        backup_cli, backup_host = fleet_release.rollback_paths(
            host,
            self.release.name,
        )

        self.assertNotEqual(ntpath.dirname(backup_cli), ntpath.dirname(backup_host))
        self.assertIn("Get-TargetPairToken", script)
        self.assertIn(
            "$stage = Join-Path (Split-Path -Parent $Target)",
            script,
        )
        self.assertIn(
            "[FleetFileOps]::PublishPreparedFile($Stage, $Destination)",
            script,
        )
        self.assertIn("MOVEFILE_WRITE_THROUGH", script)
        self.assertNotIn("MOVEFILE_COPY_ALLOWED", script)

    @unittest.skipUnless(sys.platform == "win32", "Windows path identity test")
    def test_windows_runtime_identity_handles_directory_junctions(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            real = root / "real"
            alias = root / "alias"
            real.mkdir()
            junction = run(
                ["cmd.exe", "/d", "/c", "mklink", "/J", str(alias), str(real)],
                check=False,
                capture_output=True,
                text=True,
            )
            if junction.returncode != 0:
                self.skipTest(f"directory junctions unavailable: {junction.stderr}")

            host = replace(
                self.hosts[1],
                cli_binary=str(real / "codex.exe"),
                code_mode_host_binary=str(real / "codex-code-mode-host.exe"),
            )
            script = fleet_release.activation_script(host, self.release)
            prelude = script.split("$canonical = ", 1)[0]
            alias_probe = prelude + (
                "Write-Output (Get-TargetPairToken "
                + fleet_release.quote_ps(str(real / "codex.exe"))
                + " "
                + fleet_release.quote_ps(str(real / "codex-code-mode-host.exe"))
                + ")\nWrite-Output (Get-TargetPairToken "
                + fleet_release.quote_ps(str(alias / "codex.exe"))
                + " "
                + fleet_release.quote_ps(str(alias / "codex-code-mode-host.exe"))
                + ")\n"
            )
            alias_result = run(
                [
                    "powershell.exe",
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "-",
                ],
                check=False,
                capture_output=True,
                input=alias_probe,
                text=True,
            )
            self.assertEqual(alias_result.returncode, 0, alias_result.stderr)
            self.assertEqual(len(alias_result.stdout.splitlines()), 2)
            self.assertEqual(*alias_result.stdout.splitlines())

    @unittest.skipUnless(sys.platform == "win32", "Windows path identity test")
    def test_windows_runtime_identity_preserves_case_sensitive_slots(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            case_directory = root / "case-sensitive"
            case_directory.mkdir()
            case_support = run(
                [
                    "fsutil.exe",
                    "file",
                    "setCaseSensitiveInfo",
                    str(case_directory),
                    "enable",
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            if case_support.returncode != 0:
                self.skipTest(
                    "case-sensitive directories unavailable: " + case_support.stderr
                )
            lower = case_directory / "codex.exe"
            upper = case_directory / "CODEX.exe"
            host_path = case_directory / "host.exe"
            lower.write_bytes(b"lower")
            upper.write_bytes(b"upper")
            host_path.write_bytes(b"host")
            host = replace(
                self.hosts[1],
                cli_binary=str(lower),
                code_mode_host_binary=str(host_path),
            )
            script = fleet_release.activation_script(host, self.release)
            prelude = script.split("$canonical = ", 1)[0]
            case_probe = prelude + (
                "Write-Output (Get-TargetPairToken "
                + fleet_release.quote_ps(str(lower))
                + " "
                + fleet_release.quote_ps(str(host_path))
                + ")\nWrite-Output (Get-TargetPairToken "
                + fleet_release.quote_ps(str(upper))
                + " "
                + fleet_release.quote_ps(str(host_path))
                + ")\n"
            )
            case_result = run(
                [
                    "powershell.exe",
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "-",
                ],
                check=False,
                capture_output=True,
                input=case_probe,
                text=True,
            )
            self.assertEqual(case_result.returncode, 0, case_result.stderr)
            self.assertEqual(len(case_result.stdout.splitlines()), 2)
            self.assertNotEqual(*case_result.stdout.splitlines())

    @unittest.skipUnless(sys.platform == "win32", "Windows cross-volume test")
    def test_windows_runtime_write_through_publish_crosses_source_volume(self) -> None:
        drives = run(
            [
                "powershell.exe",
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Get-PSDrive -PSProvider FileSystem | ForEach-Object Root",
            ],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(drives.returncode, 0, drives.stderr)
        writable_roots = []
        temporary_directories = []
        try:
            for value in drives.stdout.splitlines():
                try:
                    temporary = tempfile.TemporaryDirectory(dir=value.strip())
                except OSError:
                    continue
                temporary_directories.append(temporary)
                writable_roots.append(Path(temporary.name))
                if len(writable_roots) == 2:
                    break
            if len(writable_roots) < 2:
                self.skipTest("two writable filesystem volumes are unavailable")

            source = writable_roots[0] / "source.bin"
            target = writable_roots[1] / "published.bin"
            source.write_bytes(b"cross-volume write-through")
            host = replace(
                self.hosts[1],
                cli_binary=str(writable_roots[1] / "codex.exe"),
                code_mode_host_binary=str(writable_roots[1] / "host.exe"),
            )
            script = fleet_release.activation_script(host, self.release)
            prelude = script.split("$canonical = ", 1)[0]
            probe = prelude + (
                "Publish-VerifiedCopy "
                + fleet_release.quote_ps(str(source))
                + " "
                + fleet_release.quote_ps(str(target))
                + " (Hash "
                + fleet_release.quote_ps(str(source))
                + ") 'cross-volume probe'\n"
            )
            result = run(
                [
                    "powershell.exe",
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "-",
                ],
                check=False,
                capture_output=True,
                input=probe,
                text=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(target.read_bytes(), source.read_bytes())
        finally:
            for temporary in reversed(temporary_directories):
                temporary.cleanup()

    def test_windows_activation_temporary_paths_fit_legacy_max_path(self) -> None:
        release = fleet_release.Release(
            name="a" * 64,
            fork_commit=self.release.fork_commit,
            fork_ref=self.release.fork_ref,
            fork_remote=self.release.fork_remote,
            cli_version=self.release.cli_version,
            server_commit=self.release.server_commit,
            rusty_v8_version=self.release.rusty_v8_version,
        )
        host = self.hosts[1]
        directory = ntpath.dirname(host.cli_binary)
        rollback_paths = fleet_release.rollback_paths(host, release.name)
        longest_paths = (
            *rollback_paths,
            *fleet_release.target_lock_paths(host),
            ntpath.join(directory, ".fleet-stage." + "f" * 32),
            ntpath.join(directory, ".fleet-journal." + "f" * 32),
            ntpath.join(directory, ".fleet-reviewed." + "f" * 32, "codex.exe"),
        )

        self.assertTrue(all(len(path) < 260 for path in longest_paths), longest_paths)
        script = fleet_release.activation_script(host, release)
        self.assertIn("('.fleet-stage.' + [guid]::NewGuid()", script)
        self.assertIn("('.fleet-journal.' + [guid]::NewGuid()", script)
        self.assertIn("('.fleet-reviewed.' + [guid]::NewGuid()", script)
        self.assertNotIn('$stage = "$Target.', script)

    @unittest.skipUnless(
        os.name == "posix",
        "requires POSIX file locks",
    )
    def test_mac_activation_restores_pair_after_version_verification_failure(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            canonical = root / "canonical"
            build = root / "build"
            canonical.mkdir()
            (canonical / ".gitignore").write_text("codex-rs/target/\n")
            run(["git", "init", "-q", str(canonical)], check=True)
            run(
                [
                    "git",
                    "-C",
                    str(canonical),
                    "-c",
                    "user.name=Fleet Test",
                    "-c",
                    "user.email=fleet@example.invalid",
                    "add",
                    ".gitignore",
                ],
                check=True,
            )
            run(
                [
                    "git",
                    "-C",
                    str(canonical),
                    "-c",
                    "user.name=Fleet Test",
                    "-c",
                    "user.email=fleet@example.invalid",
                    "commit",
                    "-qm",
                    "test fixture",
                ],
                check=True,
            )
            commit = run(
                ["git", "-C", str(canonical), "rev-parse", "HEAD"],
                check=True,
                capture_output=True,
                text=True,
            ).stdout.strip()
            run(
                ["git", "-C", str(canonical), "worktree", "add", "-q", str(build)],
                check=True,
            )
            server = root / "server"
            run(["git", "clone", "-q", str(canonical), str(server)], check=True)
            source_dir = build / "codex-rs" / "target" / "release"
            target_dir = root / "bin"
            source_dir.mkdir(parents=True)
            target_dir.mkdir()
            source_cli = source_dir / "codex"
            source_host = source_dir / "codex-code-mode-host"
            target_cli = target_dir / "codex"
            target_host = target_dir / "codex-code-mode-host"
            source_cli.write_text(
                "#!/bin/sh\n"
                "if [ \"$1 $2 $3\" = 'app-server daemon version' ]; then "
                'echo \'{"status":"notRunning","appServerVersion":null}\'; exit 0; fi\n'
                "if [ \"$1 $2 $3\" = 'app-server daemon disable-auto-update' ]; then exit 0; fi\n"
                "if [ \"$1\" = --version ]; then echo 'codex-cli wrong'; exit 0; fi\n"
                "exit 1\n"
            )
            source_host.write_text("#!/bin/sh\nexit 0\n")
            target_cli.write_text("#!/bin/sh\necho 'codex-cli old'\n")
            target_host.write_text("#!/bin/sh\necho old-host\n")
            source_cli.chmod(0o751)
            source_host.chmod(0o711)
            target_cli.chmod(0o754)
            target_host.chmod(0o705)
            source_modes = (0o751, 0o711)
            original_modes = (0o754, 0o705)
            original_cli = target_cli.read_bytes()
            original_host = target_host.read_bytes()
            release = fleet_release.Release(
                name="test.release",
                fork_commit=commit,
                fork_ref="refs/tags/test",
                fork_remote="unused",
                cli_version="codex-cli expected",
                server_commit=commit,
                rusty_v8_version="150.4.0",
            )
            host = fleet_release.Host(
                name="mac",
                platform="darwin",
                ssh_target="unused",
                source_root=str(canonical),
                build_root=str(build),
                server_root=str(server),
                cli_binary=str(target_cli),
                code_mode_host_binary=str(target_host),
                rust_toolchain="unused",
                rust_target="aarch64-apple-darwin",
                rustup=None,
            )
            backup_cli, backup_host = (
                Path(path) for path in fleet_release.rollback_paths(host, release.name)
            )
            release_only_backups = tuple(
                Path(path)
                for path in fleet_release.release_only_rollback_paths(
                    host, release.name
                )
            )
            transaction_state = Path(fleet_release.transaction_path(host))
            activation_environment = os.environ.copy()
            activation_environment.update(
                {
                    "CODEX_HOME": str(root / "codex-home"),
                    "FAKE_UPDATER_LOG": str(root / "updater.log"),
                }
            )

            def activate(**overrides: str) -> CompletedProcess[str]:
                environment = activation_environment.copy()
                environment.update(overrides)
                return run(
                    ["sh", "-c", fleet_release.activation_script(host, release)],
                    check=False,
                    capture_output=True,
                    text=True,
                    env=environment,
                    timeout=10,
                )

            def local_status() -> fleet_release.Status:
                status_result = run(
                    ["sh", "-c", fleet_release.status_script(host)],
                    check=False,
                    capture_output=True,
                    text=True,
                )
                self.assertEqual(status_result.returncode, 0, status_result.stderr)
                return fleet_release.Status(
                    host,
                    fleet_release.parse_status(status_result.stdout),
                )

            write_build_receipt(host, release)

            result = activate()

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(target_cli.read_bytes(), original_cli)
            self.assertEqual(target_host.read_bytes(), original_host)
            self.assertEqual(
                (target_cli.stat().st_mode & 0o777, target_host.stat().st_mode & 0o777),
                original_modes,
            )
            self.assertFalse(backup_cli.exists())
            self.assertFalse(backup_host.exists())

            fail_agents = root / "fail-agents"
            signal_ignoring_daemon = root / "signal-ignoring-daemon.py"
            signal_ignoring_daemon.write_text(
                "import os\n"
                "import signal\n"
                "import time\n"
                "from pathlib import Path\n"
                "def record_term(_signum, _frame):\n"
                "    Path(os.environ['FAKE_TERM_SEEN_PATH']).write_text('term\\n')\n"
                "signal.signal(signal.SIGHUP, signal.SIG_IGN)\n"
                "signal.signal(signal.SIGINT, signal.SIG_IGN)\n"
                "signal.signal(signal.SIGTERM, record_term)\n"
                "Path(os.environ['FAKE_CHILD_PID_PATH']).write_text(f'{os.getpid()}\\n')\n"
                "Path(os.environ['FAKE_READY_PATH']).write_text('ready\\n')\n"
                "while not Path(os.environ['FAKE_RELEASE_PATH']).exists():\n"
                "    time.sleep(0.02)\n"
            )
            setsid_survivor = root / "setsid-survivor.py"
            setsid_survivor.write_text(
                "import os\n"
                "import time\n"
                "from pathlib import Path\n"
                "os.setsid()\n"
                "Path(os.environ['FAKE_SETSID_PID']).write_text(f'{os.getpid()}\\n')\n"
                "Path(os.environ['FAKE_SETSID_READY']).write_text('ready\\n')\n"
                "while not Path(os.environ['FAKE_SETSID_RELEASE']).exists():\n"
                "    time.sleep(0.02)\n"
            )
            source_cli.write_text(
                "#!/bin/sh\n"
                "if [ \"$1 $2 $3\" = 'app-server daemon version' ]; then\n"
                '  case "${FAKE_DAEMON_MODE:-not-running}" in\n'
                '    matching) echo \'{"status":"running","appServerVersion":"expected"}\' ;;\n'
                '    incompatible) echo \'{"status":"running","appServerVersion":"0.151.0"}\' ;;\n'
                "    race-incompatible|postcheck-unavailable|lease-gated|precheck-gated|ignore-term-gated|precheck-ignore-term-gated)\n"
                '      probe=$(cat "$FAKE_DAEMON_STATE")\n'
                "      probe=$((probe + 1))\n"
                '      printf \'%s\\n\' "$probe" >"$FAKE_DAEMON_STATE"\n'
                "      gate=0\n"
                '      case "$FAKE_DAEMON_MODE:$probe" in lease-gated:3|ignore-term-gated:3|precheck-gated:2|precheck-ignore-term-gated:2) gate=1 ;; esac\n'
                '      if [ "$gate" -eq 1 ]; then\n'
                f'        case "$FAKE_DAEMON_MODE" in *ignore-term*) exec python3 {signal_ignoring_daemon} ;; esac\n'
                '        [ -z "${FAKE_CHILD_PID_PATH:-}" ] || printf \'%s\\n\' "$$" >"$FAKE_CHILD_PID_PATH"\n'
                "        printf 'ready\\n' >\"$FAKE_READY_PATH\"\n"
                '        while [ ! -e "$FAKE_RELEASE_PATH" ]; do sleep 0.02; done\n'
                "      fi\n"
                '      if [ "$probe" -lt 3 ] || [ "$FAKE_DAEMON_MODE" = lease-gated ] || [ "$FAKE_DAEMON_MODE" = precheck-gated ] || [ "$FAKE_DAEMON_MODE" = ignore-term-gated ] || [ "$FAKE_DAEMON_MODE" = precheck-ignore-term-gated ]; then\n'
                '        echo \'{"status":"notRunning","appServerVersion":null}\'\n'
                '      elif [ "$FAKE_DAEMON_MODE" = race-incompatible ]; then\n'
                '        echo \'{"status":"running","appServerVersion":"0.151.0"}\'\n'
                "      else\n"
                "        exit 1\n"
                "      fi ;;\n"
                "    malformed) echo '{' ;;\n"
                "    unavailable) exit 1 ;;\n"
                '    *) echo \'{"status":"notRunning","appServerVersion":null}\' ;;\n'
                "  esac\n"
                "  exit 0\n"
                "fi\n"
                "if [ \"$1 $2 $3\" = 'app-server daemon disable-auto-update' ]; then\n"
                '  [ -z "${FAKE_UPDATER_LOG:-}" ] || printf \'disabled\\n\' >>"$FAKE_UPDATER_LOG"\n'
                '  if [ -n "${FAKE_UPDATER_GATE_READY:-}" ]; then\n'
                "    printf 'ready\\n' >\"$FAKE_UPDATER_GATE_READY\"\n"
                '    while [ ! -e "$FAKE_UPDATER_GATE_RELEASE" ]; do sleep 0.02; done\n'
                "  fi\n"
                '  [ "${FAKE_UPDATER_MODE:-success}" != failure ]\n'
                "  exit $?\n"
                "fi\n"
                "if [ \"$1\" = --version ]; then echo 'codex-cli expected'; exit 0; fi\n"
                f'if [ "$1" = agents ] && [ "$2" = --help ]; then\n'
                f'  [ "${{FAKE_HEALTH_HANG:-0}}" != 1 ] || exec python3 {signal_ignoring_daemon}\n'
                '  if [ -n "${FAKE_SETSID_READY:-}" ]; then\n'
                f"    python3 {setsid_survivor} >/dev/null 2>&1 &\n"
                '    while [ ! -e "$FAKE_SETSID_READY" ]; do sleep 0.02; done\n'
                "  fi\n"
                '  if [ -n "${FAKE_SURVIVOR_READY_PATH:-}" ]; then\n'
                '    (printf \'ready\\n\' >"$FAKE_SURVIVOR_READY_PATH"; while [ ! -e "$FAKE_SURVIVOR_RELEASE_PATH" ]; do sleep 0.02; done) >/dev/null 2>&1 &\n'
                '    while [ ! -e "$FAKE_SURVIVOR_READY_PATH" ]; do sleep 0.02; done\n'
                "  fi\n"
                f'  [ ! -e "{fail_agents}" ]; exit $?\n'
                "fi\n"
                "exit 1\n"
            )
            source_cli.chmod(source_modes[0])
            write_build_receipt(host, release)
            incompatible_daemon = activate(FAKE_DAEMON_MODE="incompatible")
            self.assertNotEqual(incompatible_daemon.returncode, 0)
            self.assertIn(
                "daemon version 0.151.0 is running", incompatible_daemon.stderr
            )
            self.assertEqual(target_cli.read_bytes(), original_cli)
            self.assertEqual(target_host.read_bytes(), original_host)
            self.assertFalse(backup_cli.exists())
            self.assertFalse(backup_host.exists())

            updater_failure = activate(FAKE_UPDATER_MODE="failure")
            self.assertNotEqual(updater_failure.returncode, 0)
            self.assertIn(
                "Could not disable the official Codex daemon updater",
                updater_failure.stderr,
            )
            self.assertEqual(target_cli.read_bytes(), original_cli)
            self.assertEqual(target_host.read_bytes(), original_host)

            invalid_codex_home = root / "invalid-codex-home"
            invalid_codex_home.write_text("not a directory\n")
            failed_before_lock = activate(CODEX_HOME=str(invalid_codex_home))
            self.assertNotEqual(failed_before_lock.returncode, 0)
            self.assertIn("Not a directory", failed_before_lock.stderr)
            self.assertEqual(target_cli.read_bytes(), original_cli)
            self.assertEqual(target_host.read_bytes(), original_host)

            default_home = root / "default-home"
            default_home.mkdir()
            fail_agents.write_text("force rollback after acquiring locks\n")
            whitespace_codex_home = activate(
                CODEX_HOME=" \t ",
                HOME=str(default_home),
            )
            self.assertNotEqual(whitespace_codex_home.returncode, 0)
            self.assertTrue(
                (
                    default_home
                    / ".codex"
                    / "app-server-daemon"
                    / "daemon.lock"
                ).is_file()
            )
            self.assertEqual(target_cli.read_bytes(), original_cli)
            self.assertEqual(target_host.read_bytes(), original_host)
            fail_agents.unlink()

            daemon_state = root / "daemon-state"
            daemon_state.write_text("0\n")
            raced_daemon = activate(
                FAKE_DAEMON_MODE="race-incompatible",
                FAKE_DAEMON_STATE=str(daemon_state),
            )
            self.assertNotEqual(raced_daemon.returncode, 0)
            self.assertIn("daemon version 0.151.0 is running", raced_daemon.stderr)
            self.assertEqual(target_cli.read_bytes(), original_cli)
            self.assertEqual(target_host.read_bytes(), original_host)
            self.assertEqual(daemon_state.read_text(), "3\n")
            self.assertFalse(backup_cli.exists())
            self.assertFalse(backup_host.exists())

            daemon_state.write_text("0\n")
            unavailable_postcheck = activate(
                FAKE_DAEMON_MODE="postcheck-unavailable",
                FAKE_DAEMON_STATE=str(daemon_state),
            )
            self.assertNotEqual(unavailable_postcheck.returncode, 0)
            self.assertIn(
                "Strict app-server daemon verification failed",
                unavailable_postcheck.stderr,
            )
            self.assertEqual(target_cli.read_bytes(), original_cli)
            self.assertEqual(target_host.read_bytes(), original_host)
            self.assertEqual(daemon_state.read_text(), "3\n")
            self.assertFalse(backup_cli.exists())
            self.assertFalse(backup_host.exists())

            import fcntl

            operation_lock = (
                Path(activation_environment["CODEX_HOME"])
                / "app-server-daemon"
                / "daemon.lock"
            )
            operation_lock.parent.mkdir(parents=True, exist_ok=True)
            target_locks = tuple(
                Path(path) for path in fleet_release.target_lock_paths(host)
            )
            self.assertEqual(len(target_locks), 1)
            installation_lock = target_locks[0]
            ready_path = root / "lease-ready"
            release_path = root / "lease-release"
            survivor_ready_path = root / "survivor-ready"
            survivor_release_path = root / "survivor-release"

            def wait_for_path(path: Path, timeout: float = 5.0) -> None:
                deadline = time.monotonic() + timeout
                while not path.exists() and time.monotonic() < deadline:
                    time.sleep(0.02)
                self.assertTrue(path.exists(), f"timed out waiting for {path}")

            def process_exists(pid: int) -> bool:
                try:
                    os.kill(pid, 0)
                    return True
                except ProcessLookupError:
                    return False

            def assert_operation_lock_available() -> None:
                with operation_lock.open("a+b") as contender:
                    fcntl.flock(
                        contender,
                        fcntl.LOCK_EX | fcntl.LOCK_NB,
                    )
                    fcntl.flock(contender, fcntl.LOCK_UN)

            def wait_for_operation_lock(timeout: float = 5.0) -> None:
                deadline = time.monotonic() + timeout
                while True:
                    try:
                        assert_operation_lock_available()
                        return
                    except BlockingIOError:
                        if time.monotonic() >= deadline:
                            raise
                        time.sleep(0.02)

            pair_ready = root / "target-pair-ready"
            pair_release = root / "target-pair-release"
            pair_updater_ready = root / "target-pair-updater-ready"
            pair_updater_release = root / "target-pair-updater-release"
            pair_daemon_state = root / "target-pair-daemon-state"
            pair_daemon_state.write_text("0\n")
            pair_environment = activation_environment.copy()
            pair_environment.update(
                {
                    "CODEX_HOME": str(root / "different-codex-home"),
                    "FAKE_DAEMON_MODE": "precheck-gated",
                    "FAKE_DAEMON_STATE": str(pair_daemon_state),
                    "FAKE_READY_PATH": str(pair_ready),
                    "FAKE_RELEASE_PATH": str(pair_release),
                    "FAKE_UPDATER_GATE_READY": str(pair_updater_ready),
                    "FAKE_UPDATER_GATE_RELEASE": str(pair_updater_release),
                }
            )
            target_alias = root / "bin-alias"
            target_alias.symlink_to(target_dir, target_is_directory=True)
            aliased_host = replace(
                host,
                cli_binary=str(target_alias / target_cli.name),
                code_mode_host_binary=str(target_alias / target_host.name),
            )
            pair_process = None
            with installation_lock.open("a+b") as pair_lock_file:
                fcntl.flock(pair_lock_file, fcntl.LOCK_EX)
                try:
                    pair_process = Popen(
                        [
                            "sh",
                            "-c",
                            fleet_release.activation_script(aliased_host, release),
                        ],
                        stdout=PIPE,
                        stderr=PIPE,
                        text=True,
                        env=pair_environment,
                    )
                    wait_for_path(pair_updater_ready)
                    pair_updater_release.touch()
                    time.sleep(0.2)
                    self.assertIsNone(pair_process.poll())
                    self.assertFalse(
                        pair_ready.exists(),
                        "a different CODEX_HOME bypassed the installation lock",
                    )
                    fcntl.flock(pair_lock_file, fcntl.LOCK_UN)
                    wait_for_path(pair_ready)
                    pair_release.touch()
                    pair_stdout, pair_stderr = pair_process.communicate(timeout=10)
                finally:
                    fcntl.flock(pair_lock_file, fcntl.LOCK_UN)
                    pair_updater_release.touch(exist_ok=True)
                    pair_release.touch(exist_ok=True)
                    if pair_process is not None and pair_process.poll() is None:
                        pair_process.kill()
                        pair_process.communicate(timeout=5)
            self.assertEqual(pair_process.returncode, 0, pair_stderr or pair_stdout)
            target_cli.write_bytes(original_cli)
            target_host.write_bytes(original_host)
            target_cli.chmod(original_modes[0])
            target_host.chmod(original_modes[1])
            for path in (backup_cli, backup_host, transaction_state):
                path.unlink(missing_ok=True)

            health_ready = root / "health-timeout-ready"
            health_release = root / "health-timeout-release"
            health_pid_path = root / "health-timeout-pid"
            health_term_seen = root / "health-timeout-term-seen"
            health_script = fleet_release.activation_script(host, release).replace(
                "HEALTH_PROBE_TIMEOUT_SECONDS = 30",
                "HEALTH_PROBE_TIMEOUT_SECONDS = 0.5",
            )
            health_environment = activation_environment.copy()
            health_environment.update(
                {
                    "FAKE_HEALTH_HANG": "1",
                    "FAKE_READY_PATH": str(health_ready),
                    "FAKE_RELEASE_PATH": str(health_release),
                    "FAKE_CHILD_PID_PATH": str(health_pid_path),
                    "FAKE_TERM_SEEN_PATH": str(health_term_seen),
                }
            )
            timed_out_health = run(
                ["sh", "-c", health_script],
                check=False,
                capture_output=True,
                text=True,
                env=health_environment,
                timeout=10,
            )
            self.assertNotEqual(timed_out_health.returncode, 0)
            self.assertIn("Installer health probe timed out", timed_out_health.stderr)
            wait_for_path(health_ready)
            wait_for_path(health_term_seen)
            health_pid = int(health_pid_path.read_text().strip())
            deadline = time.monotonic() + 5
            while process_exists(health_pid) and time.monotonic() < deadline:
                time.sleep(0.02)
            self.assertFalse(process_exists(health_pid))
            self.assertEqual(target_cli.read_bytes(), original_cli)
            self.assertEqual(target_host.read_bytes(), original_host)
            wait_for_operation_lock()

            setsid_ready = root / "setsid-ready"
            setsid_release = root / "setsid-release"
            setsid_pid_path = root / "setsid-pid"
            escaped = activate(
                FAKE_SETSID_READY=str(setsid_ready),
                FAKE_SETSID_RELEASE=str(setsid_release),
                FAKE_SETSID_PID=str(setsid_pid_path),
            )
            self.assertEqual(escaped.returncode, 0, escaped.stderr)
            self.assertEqual(
                (target_cli.stat().st_mode & 0o777, target_host.stat().st_mode & 0o777),
                source_modes,
            )
            self.assertEqual(
                (backup_cli.stat().st_mode & 0o777, backup_host.stat().st_mode & 0o777),
                original_modes,
            )
            wait_for_path(setsid_ready)
            escaped_pid = int(setsid_pid_path.read_text().strip())
            self.assertEqual(os.getsid(escaped_pid), escaped_pid)
            with operation_lock.open("a+b") as contender:
                with self.assertRaises(BlockingIOError):
                    fcntl.flock(contender, fcntl.LOCK_EX | fcntl.LOCK_NB)
            setsid_release.touch()
            deadline = time.monotonic() + 5
            while process_exists(escaped_pid) and time.monotonic() < deadline:
                time.sleep(0.02)
            self.assertFalse(process_exists(escaped_pid))
            wait_for_operation_lock()

            target_cli.write_bytes(original_cli)
            target_host.write_bytes(original_host)
            target_cli.chmod(0o755)
            target_host.chmod(0o755)
            for path in (backup_cli, backup_host, transaction_state):
                path.unlink(missing_ok=True)

            reviewed_ready = root / "reviewed-cli-ready"
            reviewed_release = root / "reviewed-cli-release"
            unreviewed_executed = root / "unreviewed-cli-executed"
            reviewed_gate = (
                "    )\n    try:\n        ensure_daemon_runtime_compatible(\n"
            )
            ordinary_script = fleet_release.activation_script(host, release)
            self.assertEqual(ordinary_script.count(reviewed_gate), 1)
            reviewed_race_script = ordinary_script.replace(
                reviewed_gate,
                "    )\n"
                '    Path(os.environ["FAKE_REVIEWED_READY"]).write_text("ready\\n")\n'
                '    while not Path(os.environ["FAKE_REVIEWED_RELEASE"]).exists():\n'
                "        time.sleep(0.02)\n"
                "    try:\n"
                "        ensure_daemon_runtime_compatible(\n",
            )

            def start_reviewed_gate(
                label: str,
            ) -> tuple[Popen[str], Path, Path]:
                ready = root / f"{label}-ready"
                release_gate = root / f"{label}-release"
                environment = activation_environment.copy()
                environment.update(
                    {
                        "FAKE_REVIEWED_READY": str(ready),
                        "FAKE_REVIEWED_RELEASE": str(release_gate),
                    }
                )
                process = Popen(
                    ["sh", "-c", reviewed_race_script],
                    stdout=PIPE,
                    stderr=PIPE,
                    text=True,
                    env=environment,
                )
                wait_for_path(ready)
                return process, ready, release_gate

            reviewed_pattern = f".{target_cli.name}.fleet-reviewed.*"
            stale_owner, _stale_ready, stale_release = start_reviewed_gate(
                "stale-reviewed-cli"
            )
            stale_directories = list(target_dir.glob(reviewed_pattern))
            self.assertEqual(len(stale_directories), 1)
            stale_owner.kill()
            stale_owner.communicate(timeout=5)
            self.assertEqual(stale_owner.returncode, -signal.SIGKILL)
            cross_release_stale = stale_directories[0].with_name(
                stale_directories[0].name.replace(
                    f".{release.name}.",
                    ".older.release.",
                )
            )
            stale_directories[0].rename(cross_release_stale)
            self.assertTrue(cross_release_stale.is_dir())

            stale_recovery = activate()

            self.assertEqual(stale_recovery.returncode, 0, stale_recovery.stderr)
            self.assertFalse(cross_release_stale.exists())
            stale_release.unlink(missing_ok=True)

            live_owner, _live_ready, live_release = start_reviewed_gate(
                "live-reviewed-cli"
            )
            live_directories = list(target_dir.glob(reviewed_pattern))
            self.assertEqual(len(live_directories), 1)

            concurrent_activation = activate()

            self.assertEqual(
                concurrent_activation.returncode,
                0,
                concurrent_activation.stderr,
            )
            self.assertTrue(
                live_directories[0].is_dir(),
                "a concurrent activation deleted a live reviewed CLI",
            )
            live_release.touch()
            live_stdout, live_stderr = live_owner.communicate(timeout=10)
            self.assertEqual(live_owner.returncode, 0, live_stderr or live_stdout)
            self.assertFalse(live_directories[0].exists())

            target_cli.write_bytes(original_cli)
            target_host.write_bytes(original_host)
            target_cli.chmod(0o755)
            target_host.chmod(0o755)
            for path in (
                backup_cli,
                backup_host,
                transaction_state,
            ):
                path.unlink(missing_ok=True)

            reviewed_race_environment = activation_environment.copy()
            reviewed_race_environment.update(
                {
                    "FAKE_REVIEWED_READY": str(reviewed_ready),
                    "FAKE_REVIEWED_RELEASE": str(reviewed_release),
                }
            )
            reviewed_updater_count = len(
                (root / "updater.log").read_text().splitlines()
            )
            reviewed_race = Popen(
                ["sh", "-c", reviewed_race_script],
                stdout=PIPE,
                stderr=PIPE,
                text=True,
                env=reviewed_race_environment,
            )
            wait_for_path(reviewed_ready)
            original_source_cli = source_cli.read_bytes()
            source_cli.write_text(
                f"#!/bin/sh\nprintf executed >{unreviewed_executed}\nexit 1\n"
            )
            source_cli.chmod(0o755)
            reviewed_release.touch()
            reviewed_stdout, reviewed_stderr = reviewed_race.communicate(timeout=10)
            self.assertNotEqual(reviewed_race.returncode, 0, reviewed_stdout)
            self.assertIn(
                "Build receipt does not match the reviewed binary pair",
                reviewed_stderr,
            )
            self.assertFalse(
                unreviewed_executed.exists(),
                "the mutable source CLI was executed after review",
            )
            self.assertEqual(
                len((root / "updater.log").read_text().splitlines()),
                reviewed_updater_count + 1,
            )
            self.assertEqual(
                list(target_dir.glob(reviewed_pattern)),
                [],
            )
            source_cli.write_bytes(original_source_cli)
            source_cli.chmod(0o755)
            write_build_receipt(host, release)

            original_source_host = source_host.read_bytes()
            updater_gate_ready = root / "updater-gate-ready"
            updater_gate_release = root / "updater-gate-release"
            staged_race_environment = activation_environment.copy()
            staged_race_environment.update(
                {
                    "FAKE_UPDATER_GATE_READY": str(updater_gate_ready),
                    "FAKE_UPDATER_GATE_RELEASE": str(updater_gate_release),
                }
            )
            staged_race = Popen(
                ["sh", "-c", fleet_release.activation_script(host, release)],
                stdout=PIPE,
                stderr=PIPE,
                text=True,
                env=staged_race_environment,
            )
            wait_for_path(updater_gate_ready)
            with operation_lock.open("a+b") as operation_lock_file:
                fcntl.flock(operation_lock_file, fcntl.LOCK_EX)
                try:
                    source_host.write_bytes(original_source_host + b"# raced stage\n")
                    source_host.chmod(0o755)
                    write_build_receipt(host, release)
                    updater_gate_release.touch()
                    time.sleep(0.1)
                    fcntl.flock(operation_lock_file, fcntl.LOCK_UN)
                    raced_stdout, raced_stderr = staged_race.communicate(timeout=10)
                finally:
                    fcntl.flock(operation_lock_file, fcntl.LOCK_UN)
                    updater_gate_release.touch(exist_ok=True)
                    if staged_race.poll() is None:
                        staged_race.kill()
                        staged_race.communicate(timeout=5)
            self.assertNotEqual(staged_race.returncode, 0, raced_stdout)
            self.assertIn(
                "Activation binary pair changed while waiting for the operation lease",
                raced_stderr,
            )
            self.assertEqual(target_cli.read_bytes(), original_cli)
            self.assertEqual(target_host.read_bytes(), original_host)
            source_host.write_bytes(original_source_host)
            source_host.chmod(0o755)
            write_build_receipt(host, release)

            spawn_window = (
                "            active_process = process\n"
                "            os.close(liveness_read)\n"
            )
            signal_window_script = fleet_release.activation_script(host, release)
            self.assertEqual(signal_window_script.count(spawn_window), 1)
            signal_window_script = signal_window_script.replace(
                spawn_window,
                "            active_process = process\n"
                "            if lease_lock_fds:\n"
                "                os.kill(os.getpid(), signal.SIGTERM)\n"
                "            os.close(liveness_read)\n",
            )
            immediate_term = run(
                ["sh", "-c", signal_window_script],
                check=False,
                capture_output=True,
                text=True,
                env=activation_environment,
                timeout=10,
            )
            self.assertEqual(immediate_term.returncode, 128 + signal.SIGTERM)
            self.assertIn("activation interrupted by signal", immediate_term.stderr)
            self.assertEqual(target_cli.read_bytes(), original_cli)
            self.assertEqual(target_host.read_bytes(), original_host)
            wait_for_operation_lock()

            for daemon_mode, supervisor_signal in (
                ("lease-gated", signal.SIGTERM),
                ("ignore-term-gated", signal.SIGTERM),
                ("precheck-ignore-term-gated", signal.SIGKILL),
            ):
                with self.subTest(
                    daemon_mode=daemon_mode,
                    supervisor_signal=supervisor_signal,
                ):
                    gate_ready = root / f"{daemon_mode}-ready"
                    gate_release = root / f"{daemon_mode}-release"
                    child_pid_path = root / f"{daemon_mode}-child-pid"
                    term_seen_path = root / f"{daemon_mode}-term-seen"
                    for path in (
                        gate_ready,
                        gate_release,
                        child_pid_path,
                        term_seen_path,
                    ):
                        path.unlink(missing_ok=True)
                    daemon_state.write_text("0\n")
                    environment = activation_environment.copy()
                    environment.update(
                        {
                            "FAKE_DAEMON_MODE": daemon_mode,
                            "FAKE_DAEMON_STATE": str(daemon_state),
                            "FAKE_READY_PATH": str(gate_ready),
                            "FAKE_RELEASE_PATH": str(gate_release),
                            "FAKE_CHILD_PID_PATH": str(child_pid_path),
                            "FAKE_TERM_SEEN_PATH": str(term_seen_path),
                        }
                    )
                    interrupted = Popen(
                        ["sh", "-c", fleet_release.activation_script(host, release)],
                        stdout=PIPE,
                        stderr=PIPE,
                        text=True,
                        env=environment,
                    )
                    lock_contender = None
                    try:
                        wait_for_path(gate_ready)
                        wait_for_path(child_pid_path)
                        child_pid = int(child_pid_path.read_text().strip())
                        with operation_lock.open("a+b") as contender:
                            with self.assertRaises(BlockingIOError):
                                fcntl.flock(
                                    contender,
                                    fcntl.LOCK_EX | fcntl.LOCK_NB,
                                )
                        if supervisor_signal == signal.SIGKILL:
                            contender_started = (
                                root / f"{daemon_mode}-contender-started"
                            )
                            contender_started.unlink(missing_ok=True)
                            contender_source = (
                                "import fcntl, os, sys\n"
                                "lock_path, started_path, child_pid = sys.argv[1:]\n"
                                "with open(lock_path, 'a+b') as lock_file:\n"
                                "    open(started_path, 'w').close()\n"
                                "    fcntl.flock(lock_file, fcntl.LOCK_EX)\n"
                                "    try:\n"
                                "        os.kill(int(child_pid), 0)\n"
                                "        print('child-alive')\n"
                                "    except ProcessLookupError:\n"
                                "        print('child-dead')\n"
                            )
                            lock_contender = Popen(
                                [
                                    sys.executable,
                                    "-c",
                                    contender_source,
                                    str(operation_lock),
                                    str(contender_started),
                                    str(child_pid),
                                ],
                                stdout=PIPE,
                                stderr=PIPE,
                                text=True,
                            )
                            wait_for_path(contender_started)
                        os.kill(interrupted.pid, supervisor_signal)
                        if supervisor_signal == signal.SIGKILL:
                            self.assertEqual(
                                interrupted.wait(timeout=2), -signal.SIGKILL
                            )
                            time.sleep(0.1)
                            self.assertIsNone(
                                lock_contender.poll(),
                                "daemon operation lock was released before child cleanup",
                            )
                        stdout, stderr = interrupted.communicate(timeout=10)
                        if lock_contender is not None:
                            contender_stdout, contender_stderr = (
                                lock_contender.communicate(timeout=5)
                            )
                            self.assertEqual(
                                (lock_contender.returncode, contender_stdout.strip()),
                                (0, "child-dead"),
                                contender_stderr,
                            )
                        if "ignore-term" in daemon_mode:
                            wait_for_path(term_seen_path)
                    finally:
                        gate_release.touch(exist_ok=True)
                        if interrupted.poll() is None:
                            interrupted.kill()
                            interrupted.communicate(timeout=5)
                        if lock_contender is not None and lock_contender.poll() is None:
                            lock_contender.kill()
                            lock_contender.communicate(timeout=5)
                    self.assertNotEqual(interrupted.returncode, 0, stdout)
                    self.assertEqual(target_cli.read_bytes(), original_cli, stderr)
                    self.assertEqual(target_host.read_bytes(), original_host, stderr)
                    assert_operation_lock_available()
                    child_deadline = time.monotonic() + 5
                    while (
                        process_exists(child_pid) and time.monotonic() < child_deadline
                    ):
                        time.sleep(0.02)
                    self.assertFalse(
                        process_exists(child_pid),
                        f"activation child {child_pid} survived supervisor death",
                    )
                    self.assertFalse(backup_cli.exists())
                    self.assertFalse(backup_host.exists())
                    self.assertEqual(
                        list(operation_lock.parent.glob("fleet-activation.*")),
                        [],
                    )

            kill_points = (
                (
                    "journal-created",
                    "            write_transaction(transaction_path, transaction)\n",
                    False,
                ),
                (
                    "first-backup-published",
                    "                durable_copy(target_cli, backup_cli)\n",
                    False,
                ),
                (
                    "rollback-copy-preparing-cross-release",
                    "        shutil.copy2(source, temporary)\n",
                    False,
                ),
                (
                    "prepared-phase-persisted",
                    '            set_transaction_phase(transaction_path, transaction, "prepared")\n',
                    False,
                ),
                (
                    "forward-cli-cross-release",
                    "            os.replace(candidate_cli, target_cli)\n",
                    False,
                ),
                (
                    "forward-host",
                    "            os.replace(candidate_host, target_host)\n",
                    False,
                ),
                (
                    "committed-phase-persisted",
                    '            set_transaction_phase(transaction_path, transaction, "committed")\n',
                    False,
                ),
                (
                    "rollback-host",
                    "        os.replace(restore_host, target_host)\n",
                    True,
                ),
                (
                    "rollback-cli",
                    "        os.replace(restore_cli, target_cli)\n",
                    True,
                ),
                (
                    "rollback-cleanup-started",
                    "        if (sha256(target_cli), sha256(target_host)) != old_hashes:\n"
                    '            raise ActivationFailure("rollback hash verification failed")\n'
                    '        set_transaction_phase(transaction_path, transaction, "cleanup")\n'
                    "        durable_unlink(backup_cli)\n",
                    True,
                ),
            )
            next_release = fleet_release.Release(
                name="next.release",
                fork_commit=release.fork_commit,
                fork_ref=release.fork_ref,
                fork_remote=release.fork_remote,
                cli_version=release.cli_version,
                server_commit=release.server_commit,
                rusty_v8_version=release.rusty_v8_version,
            )
            next_backups = tuple(
                Path(path)
                for path in fleet_release.rollback_paths(host, next_release.name)
            )
            for label, kill_after, fail_validation in kill_points:
                with self.subTest(crash_recovery=label):
                    target_cli.write_bytes(original_cli)
                    target_host.write_bytes(original_host)
                    target_cli.chmod(0o755)
                    target_host.chmod(0o755)
                    for path in (
                        backup_cli,
                        backup_host,
                        *next_backups,
                        transaction_state,
                    ):
                        path.unlink(missing_ok=True)
                    if fail_validation:
                        fail_agents.write_text("force rollback\n")
                    else:
                        fail_agents.unlink(missing_ok=True)
                    ordinary_script = fleet_release.activation_script(host, release)
                    self.assertEqual(ordinary_script.count(kill_after), 1)
                    indentation_width = len(kill_after) - len(kill_after.lstrip())
                    indentation = kill_after[:indentation_width]
                    crashing_script = ordinary_script.replace(
                        kill_after,
                        kill_after
                        + f"{indentation}os.kill(os.getpid(), signal.SIGKILL)\n",
                    )
                    crashed = run(
                        ["sh", "-c", crashing_script],
                        check=False,
                        capture_output=True,
                        text=True,
                        env=activation_environment,
                        timeout=10,
                    )
                    self.assertEqual(
                        crashed.returncode, -signal.SIGKILL, crashed.stderr
                    )
                    self.assertTrue(transaction_state.is_file())
                    if label == "forward-host":
                        crashed_status = local_status()
                        self.assertEqual(
                            crashed_status.values["activationJournal"], "pending"
                        )
                        self.assertIn(
                            "activationJournal",
                            crashed_status.drift(release),
                        )
                    fail_agents.unlink(missing_ok=True)

                    recovery_release = (
                        next_release if "cross-release" in label else release
                    )
                    recovered = run(
                        [
                            "sh",
                            "-c",
                            fleet_release.activation_script(host, recovery_release),
                        ],
                        check=False,
                        capture_output=True,
                        text=True,
                        env=activation_environment,
                        timeout=10,
                    )
                    expected_backups = tuple(
                        Path(path)
                        for path in fleet_release.rollback_paths(
                            host,
                            recovery_release.name,
                        )
                    )

                    self.assertEqual(recovered.returncode, 0, recovered.stderr)
                    self.assertEqual(target_cli.read_bytes(), source_cli.read_bytes())
                    self.assertEqual(target_host.read_bytes(), source_host.read_bytes())
                    self.assertEqual(expected_backups[0].read_bytes(), original_cli)
                    self.assertEqual(expected_backups[1].read_bytes(), original_host)
                    if recovery_release is next_release:
                        self.assertFalse(backup_cli.exists())
                        self.assertFalse(backup_host.exists())
                    self.assertFalse(transaction_state.exists())
                    self.assertEqual(
                        [
                            path.name
                            for path in target_dir.iterdir()
                            if path.name.startswith(
                                (
                                    ".fleet-stage.",
                                    ".fleet-journal.",
                                    f"{backup_cli.name}.preparing.",
                                    f"{backup_host.name}.preparing.",
                                )
                            )
                        ],
                        [],
                    )

            target_cli.write_bytes(original_cli)
            target_host.write_bytes(original_host)
            target_cli.chmod(0o755)
            target_host.chmod(0o755)
            for path in (backup_cli, backup_host, transaction_state):
                path.unlink(missing_ok=True)
            target_alias = root / "journal-bin-alias"
            target_alias.symlink_to(target_dir, target_is_directory=True)
            alias_cli = target_alias / target_cli.name
            alias_host = target_alias / target_host.name
            # The authenticated runtime namespace follows the physical target
            # slots, so a directory alias resolves to the canonical pair.
            alias_backup_cli, alias_backup_host = backup_cli, backup_host
            transaction_state.write_text(
                json.dumps(
                    {
                        "format": 1,
                        "release": release.name,
                        "targetCli": str(alias_cli),
                        "targetHost": str(alias_host),
                        "backupCli": str(alias_backup_cli),
                        "backupHost": str(alias_backup_host),
                        "oldCliHash": hashlib.sha256(original_cli).hexdigest(),
                        "oldHostHash": hashlib.sha256(original_host).hexdigest(),
                        "phase": "preparing",
                    }
                )
            )

            alias_recovery = activate()

            self.assertEqual(alias_recovery.returncode, 0, alias_recovery.stderr)
            self.assertFalse(transaction_state.exists())
            self.assertEqual(target_cli.read_bytes(), source_cli.read_bytes())
            self.assertEqual(target_host.read_bytes(), source_host.read_bytes())

            different_dir = root / "different-bin"
            different_dir.mkdir()
            different_cli = different_dir / target_cli.name
            different_host = different_dir / target_host.name
            different_cli.write_bytes(target_cli.read_bytes())
            different_host.write_bytes(target_host.read_bytes())
            different_host_config = replace(
                host,
                cli_binary=str(different_cli),
                code_mode_host_binary=str(different_host),
            )
            different_backup_cli, different_backup_host = (
                Path(path)
                for path in fleet_release.rollback_paths(
                    different_host_config,
                    release.name,
                )
            )
            transaction_state.write_text(
                json.dumps(
                    {
                        "format": 1,
                        "release": release.name,
                        "targetCli": str(different_cli),
                        "targetHost": str(different_host),
                        "backupCli": str(different_backup_cli),
                        "backupHost": str(different_backup_host),
                        "oldCliHash": hashlib.sha256(original_cli).hexdigest(),
                        "oldHostHash": hashlib.sha256(original_host).hexdigest(),
                        "phase": "preparing",
                    }
                )
            )
            active_pair = (target_cli.read_bytes(), target_host.read_bytes())

            different_target_recovery = activate()

            self.assertNotEqual(different_target_recovery.returncode, 0)
            self.assertIn(
                "Activation transaction does not match this target pair",
                different_target_recovery.stderr,
            )
            self.assertEqual(
                (target_cli.read_bytes(), target_host.read_bytes()),
                active_pair,
            )
            transaction_state.unlink()

            target_cli.write_bytes(original_cli)
            target_host.write_bytes(original_host)
            target_cli.chmod(0o755)
            target_host.chmod(0o755)
            for path in (backup_cli, backup_host):
                path.unlink(missing_ok=True)
            fsync_failure_script = fleet_release.activation_script(host, release)
            fsync_definition = "def fsync_directory(path):\n"
            self.assertEqual(fsync_failure_script.count(fsync_definition), 1)
            fsync_failure_script = fsync_failure_script.replace(
                fsync_definition,
                fsync_definition
                + '    transaction = Path(CONFIG["transaction_state"])\n'
                + "    if path == transaction.parent and not transaction.exists():\n"
                + '        raise OSError("simulated post-unlink parent fsync failure")\n',
            )

            post_unlink_fsync = run(
                ["sh", "-c", fsync_failure_script],
                check=False,
                capture_output=True,
                text=True,
                env=activation_environment,
                timeout=10,
            )

            self.assertEqual(
                post_unlink_fsync.returncode,
                0,
                post_unlink_fsync.stderr,
            )
            self.assertFalse(transaction_state.exists())
            self.assertEqual(target_cli.read_bytes(), source_cli.read_bytes())
            self.assertEqual(target_host.read_bytes(), source_host.read_bytes())

            target_cli.write_bytes(original_cli)
            target_host.write_bytes(original_host)
            target_cli.chmod(0o755)
            target_host.chmod(0o755)
            for path in (backup_cli, backup_host, *next_backups, transaction_state):
                path.unlink(missing_ok=True)
            release_only_journal = Path(
                fleet_release.release_only_transaction_path(host)
            )
            release_only_backups[0].write_bytes(original_cli)
            release_only_backups[1].write_bytes(original_host)
            target_cli.write_bytes(source_cli.read_bytes())
            release_only_journal.write_text(
                json.dumps(
                    {
                        "format": 1,
                        "release": release.name,
                        "targetCli": str(target_cli),
                        "targetHost": str(target_host),
                        "backupCli": str(release_only_backups[0]),
                        "backupHost": str(release_only_backups[1]),
                        "oldCliHash": hashlib.sha256(original_cli).hexdigest(),
                        "oldHostHash": hashlib.sha256(original_host).hexdigest(),
                        "phase": "cliReplaced",
                    }
                )
            )

            release_only_recovery = activate()

            self.assertEqual(
                release_only_recovery.returncode,
                0,
                release_only_recovery.stderr,
            )
            self.assertFalse(release_only_journal.exists())
            self.assertFalse(release_only_backups[0].exists())
            self.assertFalse(release_only_backups[1].exists())
            self.assertEqual(backup_cli.read_bytes(), original_cli)
            self.assertEqual(backup_host.read_bytes(), original_host)

            target_cli.write_bytes(original_cli)
            target_host.write_bytes(original_host)
            target_cli.chmod(0o755)
            target_host.chmod(0o755)
            backup_cli.unlink()
            backup_host.unlink()
            # Schema-v1 releases allowed uppercase and spaces. Their exact
            # legacy journal namespace remains recoverable during an upgrade.
            legacy_release = "Older Legacy Release"
            legacy_backup_cli = Path(f"{target_cli}.{legacy_release}-previous")
            legacy_backup_host = Path(f"{target_host}.{legacy_release}-previous")
            legacy_journal = Path(f"{target_cli}.{legacy_release}-transaction.json")
            legacy_backup_cli.write_bytes(original_cli)
            legacy_backup_host.write_bytes(original_host)
            target_cli.write_bytes(source_cli.read_bytes())
            legacy_journal.write_text(
                json.dumps(
                    {
                        "format": 1,
                        "release": legacy_release,
                        "targetCli": str(target_cli),
                        "targetHost": str(target_host),
                        "backupCli": str(legacy_backup_cli),
                        "backupHost": str(legacy_backup_host),
                        "oldCliHash": hashlib.sha256(original_cli).hexdigest(),
                        "oldHostHash": hashlib.sha256(original_host).hexdigest(),
                        "phase": "cliReplaced",
                    }
                )
            )

            legacy_status = local_status()
            self.assertEqual(legacy_status.values["activationJournal"], "pending")
            self.assertIn("activationJournal", legacy_status.drift(release))

            legacy_recovery = activate()

            self.assertEqual(legacy_recovery.returncode, 0, legacy_recovery.stderr)
            self.assertEqual(target_cli.read_bytes(), source_cli.read_bytes())
            self.assertEqual(target_host.read_bytes(), source_host.read_bytes())
            self.assertEqual(backup_cli.read_bytes(), original_cli)
            self.assertEqual(backup_host.read_bytes(), original_host)
            self.assertFalse(legacy_journal.exists())
            self.assertFalse(legacy_backup_cli.exists())
            self.assertFalse(legacy_backup_host.exists())

            for path in (backup_cli, backup_host):
                path.unlink(missing_ok=True)
            head_backup_cli = Path(f"{target_cli}.{release.name}-previous")
            head_backup_host = Path(f"{target_host}.{release.name}-previous")
            head_backup_cli.write_bytes(target_cli.read_bytes())
            head_backup_host.write_bytes(target_host.read_bytes())

            backup_only_upgrade = activate()

            self.assertEqual(
                backup_only_upgrade.returncode,
                0,
                backup_only_upgrade.stderr,
            )
            self.assertFalse(head_backup_cli.exists())
            self.assertFalse(head_backup_host.exists())
            self.assertEqual(backup_cli.read_bytes(), target_cli.read_bytes())
            self.assertEqual(backup_host.read_bytes(), target_host.read_bytes())

            for path in (backup_cli, backup_host):
                path.unlink()
            release_only_backups[0].write_bytes(target_cli.read_bytes())
            release_only_backups[1].write_bytes(target_host.read_bytes())

            release_only_upgrade = activate()

            self.assertEqual(
                release_only_upgrade.returncode,
                0,
                release_only_upgrade.stderr,
            )
            self.assertFalse(release_only_backups[0].exists())
            self.assertFalse(release_only_backups[1].exists())
            self.assertEqual(backup_cli.read_bytes(), target_cli.read_bytes())
            self.assertEqual(backup_host.read_bytes(), target_host.read_bytes())

            migration_states = (
                (True, True, True, False, "after CLI publish"),
                (True, True, True, True, "after host publish"),
                (False, True, True, True, "after source CLI unlink"),
                (False, False, True, True, "after source host unlink"),
            )
            migration_sources = (
                ("legacy", (head_backup_cli, head_backup_host)),
                ("release-only", release_only_backups),
            )
            active_contents = (target_cli.read_bytes(), target_host.read_bytes())
            all_migration_paths = (
                backup_cli,
                backup_host,
                head_backup_cli,
                head_backup_host,
                *release_only_backups,
            )
            for namespace, source_pair in migration_sources:
                for (
                    source_cli_present,
                    source_host_present,
                    current_cli_present,
                    current_host_present,
                    crash_point,
                ) in migration_states:
                    with self.subTest(
                        migration_namespace=namespace,
                        crash_point=crash_point,
                    ):
                        for path in all_migration_paths:
                            path.unlink(missing_ok=True)
                        for path, contents, present in zip(
                            source_pair,
                            active_contents,
                            (source_cli_present, source_host_present),
                            strict=True,
                        ):
                            if present:
                                path.write_bytes(contents)
                        for path, contents, present in zip(
                            (backup_cli, backup_host),
                            active_contents,
                            (current_cli_present, current_host_present),
                            strict=True,
                        ):
                            if present:
                                path.write_bytes(contents)

                        resumed_migration = activate()

                        self.assertEqual(
                            resumed_migration.returncode,
                            0,
                            resumed_migration.stderr,
                        )
                        self.assertEqual(
                            (backup_cli.read_bytes(), backup_host.read_bytes()),
                            active_contents,
                        )
                        self.assertFalse(source_pair[0].exists())
                        self.assertFalse(source_pair[1].exists())

            backup_cli.unlink()
            backup_host.unlink()
            head_backup_cli.write_bytes(target_cli.read_bytes())
            incomplete_legacy = activate()
            self.assertNotEqual(incomplete_legacy.returncode, 0)
            self.assertIn(
                "Release rollback pair is incomplete", incomplete_legacy.stderr
            )
            self.assertFalse(backup_cli.exists())
            target_cli.write_bytes(original_cli)
            target_host.write_bytes(original_host)
            head_backup_host.write_bytes(b"not-the-active-host")
            mismatched_legacy = activate()
            self.assertNotEqual(mismatched_legacy.returncode, 0)
            self.assertIn(
                "Release rollback pair does not match the active binaries",
                mismatched_legacy.stderr,
            )
            head_backup_cli.unlink()
            head_backup_host.unlink()

            active_pair = (target_cli.read_bytes(), target_host.read_bytes())
            legacy_one = Path(f"{target_cli}.one-transaction.json")
            legacy_two = Path(f"{target_cli}.two-transaction.json")
            legacy_one.write_text("{}\n")
            legacy_two.write_text("{}\n")
            ambiguous_recovery = activate()
            self.assertNotEqual(ambiguous_recovery.returncode, 0)
            self.assertIn(
                "Multiple activation transaction journals",
                ambiguous_recovery.stderr,
            )
            self.assertEqual(
                (target_cli.read_bytes(), target_host.read_bytes()),
                active_pair,
            )
            legacy_one.unlink()
            legacy_two.unlink()

            target_cli.write_bytes(original_cli)
            target_host.write_bytes(original_host)
            target_cli.chmod(0o755)
            target_host.chmod(0o755)
            for path in (backup_cli, backup_host, transaction_state):
                path.unlink(missing_ok=True)

            activation_process = None
            with operation_lock.open("a+b") as operation_lock_file:
                fcntl.flock(operation_lock_file, fcntl.LOCK_EX)
                daemon_state.write_text("0\n")
                environment = activation_environment.copy()
                environment.update(
                    {
                        "FAKE_DAEMON_MODE": "lease-gated",
                        "FAKE_DAEMON_STATE": str(daemon_state),
                        "FAKE_READY_PATH": str(ready_path),
                        "FAKE_RELEASE_PATH": str(release_path),
                        "FAKE_SURVIVOR_READY_PATH": str(survivor_ready_path),
                        "FAKE_SURVIVOR_RELEASE_PATH": str(survivor_release_path),
                    }
                )
                try:
                    activation_process = Popen(
                        ["sh", "-c", fleet_release.activation_script(host, release)],
                        stdout=PIPE,
                        stderr=PIPE,
                        text=True,
                        env=environment,
                    )
                    time.sleep(0.25)
                    self.assertFalse(
                        ready_path.exists(),
                        "activation bypassed the daemon operation lock",
                    )
                    fcntl.flock(operation_lock_file, fcntl.LOCK_UN)

                    wait_for_path(ready_path)
                    with operation_lock.open("a+b") as contender:
                        with self.assertRaises(BlockingIOError):
                            fcntl.flock(
                                contender,
                                fcntl.LOCK_EX | fcntl.LOCK_NB,
                            )

                    release_path.write_text("release\n")
                    stdout, stderr = activation_process.communicate(timeout=10)
                    wait_for_path(survivor_ready_path)
                    fcntl.flock(
                        operation_lock_file,
                        fcntl.LOCK_EX | fcntl.LOCK_NB,
                    )
                    fcntl.flock(operation_lock_file, fcntl.LOCK_UN)
                    survivor_release_path.write_text("release\n")
                    retry = CompletedProcess(
                        activation_process.args,
                        activation_process.returncode,
                        stdout,
                        stderr,
                    )
                finally:
                    fcntl.flock(operation_lock_file, fcntl.LOCK_UN)
                    release_path.touch(exist_ok=True)
                    survivor_release_path.touch(exist_ok=True)
                    if (
                        activation_process is not None
                        and activation_process.poll() is None
                    ):
                        activation_process.terminate()
                        try:
                            activation_process.communicate(timeout=5)
                        except TimeoutExpired:
                            activation_process.kill()
                            activation_process.communicate(timeout=5)
            self.assertEqual(retry.returncode, 0, retry.stderr)
            self.assertEqual(target_cli.read_bytes(), source_cli.read_bytes())
            self.assertEqual(target_host.read_bytes(), source_host.read_bytes())

            self.assertFalse(Path(fleet_release.transaction_path(host)).exists())
            self.assertEqual(
                [
                    path.name
                    for path in target_dir.iterdir()
                    if path.name.startswith((".fleet-stage.", ".fleet-journal."))
                ],
                [],
            )

            self.assertEqual(backup_cli.read_bytes(), original_cli)
            self.assertEqual(backup_host.read_bytes(), original_host)
            target_host.chmod(0o644)
            updater_calls_before = (root / "updater.log").read_text().splitlines()
            repair_mode = activate()

            self.assertEqual(repair_mode.returncode, 0, repair_mode.stderr)
            self.assertEqual(
                len((root / "updater.log").read_text().splitlines()),
                len(updater_calls_before) + 1,
            )
            self.assertTrue(os.access(target_host, os.X_OK))
            self.assertEqual(backup_cli.read_bytes(), original_cli)
            self.assertEqual(backup_host.read_bytes(), original_host)

            target_host.chmod(0o644)
            fail_agents.write_text("fail post-repair validation\n")
            failed_mode_repair = activate()

            self.assertNotEqual(failed_mode_repair.returncode, 0)
            self.assertFalse(os.access(target_host, os.X_OK))
            self.assertEqual(backup_cli.read_bytes(), original_cli)
            self.assertEqual(backup_host.read_bytes(), original_host)
            fail_agents.unlink()
            repaired_after_failure = activate()
            self.assertEqual(repaired_after_failure.returncode, 0)
            self.assertTrue(os.access(target_host, os.X_OK))

            active_cli = target_cli.read_bytes()
            active_host = target_host.read_bytes()
            Path(host.build_receipt()).unlink()
            missing_receipt = activate()
            self.assertNotEqual(missing_receipt.returncode, 0)
            self.assertIn("Missing reviewed build receipt", missing_receipt.stderr)
            self.assertEqual(target_cli.read_bytes(), active_cli)
            self.assertEqual(target_host.read_bytes(), active_host)
            write_build_receipt(host, release)
            backup_host.unlink()
            incomplete_rollback = activate()
            self.assertNotEqual(incomplete_rollback.returncode, 0)
            self.assertIn(
                "Release rollback pair is incomplete", incomplete_rollback.stderr
            )
            self.assertEqual(target_cli.read_bytes(), active_cli)
            self.assertEqual(target_host.read_bytes(), active_host)
            backup_host.write_bytes(original_host)
            (canonical / "untracked-internal").write_text("must block activation\n")
            source_cli.write_text(source_cli.read_text() + "# distinct candidate\n")
            dirty_source = activate()

            self.assertNotEqual(dirty_source.returncode, 0)
            self.assertIn(
                "Canonical source is not clean at the manifest commit",
                dirty_source.stderr,
            )
            self.assertEqual(target_cli.read_bytes(), active_cli)
            self.assertEqual(target_host.read_bytes(), active_host)

    @unittest.skipUnless(sys.platform == "win32", "Windows activation test")
    def test_windows_activation_job_kills_child_with_parent(self) -> None:
        import ctypes
        import msvcrt

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            ready = root / "child-ready"
            acquisition_error = root / "acquisition-error"
            release_parent = root / "release-parent"
            child_pid_path = root / "child-pid"
            child_script = root / "contained-child.ps1"
            child_script.write_text(
                f"[System.IO.File]::WriteAllText({fleet_release.quote_ps(str(ready))}, 'ready')\n"
                "Start-Sleep -Seconds 300\n"
            )
            host = fleet_release.Host(
                name="windows",
                platform="windows",
                ssh_target="unused",
                source_root=str(root / "source"),
                build_root=str(root / "build"),
                server_root=str(root / "server"),
                cli_binary=str(root / "bin" / "codex.exe"),
                code_mode_host_binary=str(root / "bin" / "codex-code-mode-host.exe"),
                rust_toolchain="unused",
                rust_target="x86_64-pc-windows-msvc",
                rustup=None,
            )
            installation_lock = root / "fleet-activation.lock"
            script = fleet_release.activation_script(
                host,
                self.release,
                windows_installation_lock_path=str(installation_lock),
            )
            guardian_start = (
                "$guardian = Start-ActivationGuardian "
                "$targetLockPaths $daemonLockPath\n"
            )
            self.assertEqual(script.count(guardian_start), 1)
            contained_child = (
                guardian_start
                + "$firstAcquireError = $null\n"
                + "try { Acquire-ActivationLease $guardian } catch { $firstAcquireError = $_.Exception.Message }\n"
                + "if ([string]::IsNullOrEmpty($firstAcquireError)) { throw 'expected the first guardian acquisition to fail' }\n"
                + "[System.IO.File]::WriteAllText("
                + fleet_release.quote_ps(str(acquisition_error))
                + ", $firstAcquireError)\n"
                + "Remove-Item -Force -LiteralPath $daemonLockDir\n"
                + "[void][System.IO.Directory]::CreateDirectory($daemonLockDir)\n"
                + "Acquire-ActivationLease $guardian\n"
                + "$child = Start-Process -FilePath 'powershell.exe' -ArgumentList @('-NoLogo', '-NoProfile', '-NonInteractive', '-File', "
                + fleet_release.quote_ps(str(child_script))
                + ") -PassThru\n"
                + "[System.IO.File]::WriteAllText("
                + fleet_release.quote_ps(str(child_pid_path))
                + ", [string]$child.Id)\n"
                + "while (-not (Test-Path -LiteralPath "
                + fleet_release.quote_ps(str(ready))
                + ")) { Start-Sleep -Milliseconds 10 }\n"
                + "while (-not (Test-Path -LiteralPath "
                + fleet_release.quote_ps(str(release_parent))
                + ")) { Start-Sleep -Milliseconds 10 }\n"
                + "Stop-Process -Id $PID -Force\n"
            )
            script = script.replace(guardian_start, contained_child)
            activation_script_path = root / "activation.ps1"
            activation_script_path.write_text(script)
            codex_home = root / "codex-home"
            codex_home.mkdir()
            (codex_home / "app-server-daemon").write_text("block directory creation\n")
            operation_locks = (
                installation_lock,
                codex_home / "app-server-daemon" / "daemon.lock",
            )
            environment = os.environ.copy()
            environment["CODEX_HOME"] = str(codex_home)
            parent = Popen(
                [
                    "powershell.exe",
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-File",
                    str(activation_script_path),
                ],
                stdout=PIPE,
                stderr=PIPE,
                text=True,
                env=environment,
            )
            try:
                deadline = time.monotonic() + 10
                while not ready.exists() and time.monotonic() < deadline:
                    if parent.poll() is not None:
                        _stdout, stderr = parent.communicate()
                        self.fail(f"activation guardian failed before ready: {stderr}")
                    time.sleep(0.02)
                self.assertTrue(ready.is_file(), "contained child did not become ready")
                self.assertTrue(
                    acquisition_error.read_text().strip(),
                    "guardian did not surface its first acquisition error",
                )
                child_pid = int(child_pid_path.read_text())

                for operation_lock in operation_locks:
                    with operation_lock.open("r+b", buffering=0) as contender:
                        contender.seek(0)
                        with self.assertRaises(OSError):
                            msvcrt.locking(contender.fileno(), msvcrt.LK_NBLCK, 1)

                release_parent.touch()
                with operation_locks[0].open("r+b", buffering=0) as contender:
                    deadline = time.monotonic() + 10
                    while True:
                        contender.seek(0)
                        try:
                            msvcrt.locking(contender.fileno(), msvcrt.LK_NBLCK, 1)
                            break
                        except OSError:
                            if time.monotonic() >= deadline:
                                self.fail(
                                    "guardian did not release the installation operation lock"
                                )
                            time.sleep(0.02)

                    process_handle = ctypes.windll.kernel32.OpenProcess(
                        0x1000,
                        False,
                        child_pid,
                    )
                    child_is_alive = False
                    if process_handle:
                        exit_code = ctypes.c_ulong()
                        self.assertTrue(
                            ctypes.windll.kernel32.GetExitCodeProcess(
                                process_handle,
                                ctypes.byref(exit_code),
                            )
                        )
                        child_is_alive = exit_code.value == 259
                        ctypes.windll.kernel32.CloseHandle(process_handle)
                    self.assertFalse(
                        child_is_alive,
                        "installation lock became available before the contained child died",
                    )
                    contender.seek(0)
                    msvcrt.locking(contender.fileno(), msvcrt.LK_UNLCK, 1)

                with operation_locks[1].open("r+b", buffering=0) as contender:
                    contender.seek(0)
                    msvcrt.locking(contender.fileno(), msvcrt.LK_NBLCK, 1)
                    contender.seek(0)
                    msvcrt.locking(contender.fileno(), msvcrt.LK_UNLCK, 1)

                _stdout, stderr = parent.communicate(timeout=10)
                self.assertNotEqual(parent.returncode, 0, stderr)
            finally:
                release_parent.touch(exist_ok=True)
                if parent.poll() is None:
                    parent.kill()
                    parent.communicate(timeout=5)

    @unittest.skipUnless(sys.platform == "win32", "Windows activation test")
    def test_windows_activation_restores_pair_after_version_verification_failure(
        self,
    ) -> None:
        fixture_cli = Path(
            os.environ.get("CODEX_FLEET_WINDOWS_FIXTURE_CLI", self.hosts[1].cli_binary)
        )
        fixture_host = Path(
            os.environ.get(
                "CODEX_FLEET_WINDOWS_FIXTURE_HOST",
                self.hosts[1].code_mode_host_binary,
            )
        )
        self.assertTrue(
            fixture_cli.is_file() and fixture_host.is_file(),
            "native activation qualification requires maintained fixtures; set "
            "CODEX_FLEET_WINDOWS_FIXTURE_CLI and CODEX_FLEET_WINDOWS_FIXTURE_HOST",
        )
        fixture_version = run(
            [str(fixture_cli), "--version"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            canonical = root / "canonical"
            build = root / "build"
            canonical.mkdir()
            (canonical / ".gitignore").write_text("codex-rs/target/\n")
            run(["git", "init", "-q", str(canonical)], check=True)
            run(
                [
                    "git",
                    "-C",
                    str(canonical),
                    "-c",
                    "user.name=Fleet Test",
                    "-c",
                    "user.email=fleet@example.invalid",
                    "add",
                    ".gitignore",
                ],
                check=True,
            )
            run(
                [
                    "git",
                    "-C",
                    str(canonical),
                    "-c",
                    "user.name=Fleet Test",
                    "-c",
                    "user.email=fleet@example.invalid",
                    "commit",
                    "-qm",
                    "test fixture",
                ],
                check=True,
            )
            commit = run(
                ["git", "-C", str(canonical), "rev-parse", "HEAD"],
                check=True,
                capture_output=True,
                text=True,
            ).stdout.strip()
            run(
                ["git", "-C", str(canonical), "worktree", "add", "-q", str(build)],
                check=True,
            )
            server = root / "server"
            run(["git", "clone", "-q", str(canonical), str(server)], check=True)
            source_dir = build / "codex-rs" / "target" / "release"
            target_dir = root / "bin"
            source_dir.mkdir(parents=True)
            target_dir.mkdir()
            source_cli = source_dir / "codex.exe"
            source_host = source_dir / "codex-code-mode-host.exe"
            target_cli = target_dir / "codex.exe"
            target_host = target_dir / "codex-code-mode-host.exe"
            original_cli = fixture_cli.read_bytes()
            original_host = fixture_host.read_bytes()
            target_cli.write_bytes(original_cli)
            target_host.write_bytes(original_host)
            source_cli.write_bytes(original_cli + b"fleet-source-cli")
            source_host.write_bytes(original_host + b"fleet-source-host")
            host = fleet_release.Host(
                name="windows",
                platform="windows",
                ssh_target="unused",
                source_root=str(canonical),
                build_root=str(build),
                server_root=str(server),
                cli_binary=str(target_cli),
                code_mode_host_binary=str(target_host),
                rust_toolchain="unused",
                rust_target="x86_64-pc-windows-msvc",
                rustup=None,
            )
            release = fleet_release.Release(
                name="test.release",
                fork_commit=commit,
                fork_ref="refs/tags/test",
                fork_remote="unused",
                cli_version="codex-cli intentionally-wrong",
                server_commit=commit,
                rusty_v8_version="150.4.0",
            )
            windows_environment = os.environ.copy()
            windows_environment["CODEX_HOME"] = str(root / "codex-home")

            def activation_command(current_release):
                return fleet_release.windows_powershell_stdin(
                    fleet_release.activation_script(
                        host,
                        current_release,
                        windows_installation_lock_path=str(root / "installation.lock"),
                    )
                )

            # The production namespace uses physical directory IDs and filesystem case
            # behavior, not Python's lexical path spelling. Use that same runtime here.
            prelude = fleet_release.activation_script(host, release).split(
                "$canonical = ", 1
            )[0]
            namespace_script = prelude + (
                "Get-RollbackPaths "
                + fleet_release.quote_ps(str(target_cli))
                + " "
                + fleet_release.quote_ps(str(target_host))
                + " "
                + fleet_release.quote_ps(release.name)
                + " 'target-pair' | ConvertTo-Json -Compress\n"
            )
            namespace_command, namespace_payload = (
                fleet_release.windows_powershell_stdin(namespace_script)
            )
            namespace_result = run(
                namespace_command,
                check=True,
                capture_output=True,
                encoding="utf-8",
                input=namespace_payload,
                text=True,
                env=windows_environment,
            )
            backup_paths = json.loads(namespace_result.stdout)
            backup_cli = Path(backup_paths["Cli"])
            backup_host = Path(backup_paths["Host"])
            write_build_receipt(host, release)

            command, payload = activation_command(release)
            failed = run(
                command,
                check=False,
                capture_output=True,
                encoding="utf-8",
                input=payload,
                text=True,
                env=windows_environment,
            )

            self.assertNotEqual(failed.returncode, 0)
            self.assertEqual(target_cli.read_bytes(), original_cli)
            self.assertEqual(target_host.read_bytes(), original_host)
            self.assertFalse(backup_cli.exists())
            self.assertFalse(backup_host.exists())

            release = fleet_release.Release(
                name=release.name,
                fork_commit=release.fork_commit,
                fork_ref=release.fork_ref,
                fork_remote=release.fork_remote,
                cli_version=fixture_version,
                server_commit=release.server_commit,
                rusty_v8_version=release.rusty_v8_version,
            )
            write_build_receipt(host, release)
            command, payload = activation_command(release)
            retry = run(
                command,
                check=False,
                capture_output=True,
                encoding="utf-8",
                input=payload,
                text=True,
                env=windows_environment,
            )

            self.assertEqual(retry.returncode, 0, retry.stderr)
            self.assertEqual(target_cli.read_bytes(), source_cli.read_bytes())
            self.assertEqual(target_host.read_bytes(), source_host.read_bytes())

            legacy_release = "Older Legacy Release"
            legacy_backup_cli = Path(f"{target_cli}.{legacy_release}-previous")
            legacy_backup_host = Path(f"{target_host}.{legacy_release}-previous")
            legacy_journal = Path(f"{target_cli}.{legacy_release}-transaction.json")
            legacy_backup_cli.write_bytes(original_cli)
            legacy_backup_host.write_bytes(original_host)
            target_host.write_bytes(original_host)
            legacy_journal.write_text(
                json.dumps(
                    {
                        "format": 1,
                        "release": legacy_release,
                        "targetCli": str(target_cli),
                        "targetHost": str(target_host),
                        "backupCli": str(legacy_backup_cli),
                        "backupHost": str(legacy_backup_host),
                        "oldCliHash": hashlib.sha256(original_cli).hexdigest(),
                        "oldHostHash": hashlib.sha256(original_host).hexdigest(),
                        "phase": "cliReplaced",
                    }
                )
            )
            command, payload = activation_command(release)
            legacy_recovery = run(
                command,
                check=False,
                capture_output=True,
                encoding="utf-8",
                input=payload,
                text=True,
                env=windows_environment,
            )

            self.assertEqual(legacy_recovery.returncode, 0, legacy_recovery.stderr)
            self.assertFalse(legacy_journal.exists())
            self.assertFalse(legacy_backup_cli.exists())
            self.assertFalse(legacy_backup_host.exists())
            self.assertEqual(target_cli.read_bytes(), source_cli.read_bytes())
            self.assertEqual(target_host.read_bytes(), source_host.read_bytes())

            for path in target_dir.glob(".fleet-rollback.*"):
                path.unlink()
            head_backup_cli = Path(f"{target_cli}.{release.name}-previous")
            head_backup_host = Path(f"{target_host}.{release.name}-previous")
            head_backup_cli.write_bytes(target_cli.read_bytes())
            head_backup_host.write_bytes(target_host.read_bytes())
            command, payload = activation_command(release)
            backup_only_upgrade = run(
                command,
                check=False,
                capture_output=True,
                encoding="utf-8",
                input=payload,
                text=True,
                env=windows_environment,
            )

            self.assertEqual(
                backup_only_upgrade.returncode,
                0,
                backup_only_upgrade.stderr,
            )
            self.assertFalse(head_backup_cli.exists())
            self.assertFalse(head_backup_host.exists())
            migrated_backups = tuple(target_dir.glob(".fleet-rollback.*"))
            self.assertEqual(len(migrated_backups), 2)

            for path in migrated_backups:
                path.unlink()
            release_only = tuple(
                Path(path)
                for path in fleet_release.release_only_rollback_paths(
                    host, release.name
                )
            )
            release_only[0].write_bytes(target_cli.read_bytes())
            release_only[1].write_bytes(target_host.read_bytes())
            current_namespace_upgrade = run(
                command,
                check=False,
                capture_output=True,
                encoding="utf-8",
                input=payload,
                text=True,
                env=windows_environment,
            )
            self.assertEqual(
                current_namespace_upgrade.returncode,
                0,
                current_namespace_upgrade.stderr,
            )
            self.assertFalse(release_only[0].exists())
            self.assertFalse(release_only[1].exists())

            migration_states = (
                (True, True, True, False, "after CLI publish"),
                (True, True, True, True, "after host publish"),
                (False, True, True, True, "after source CLI unlink"),
                (False, False, True, True, "after source host unlink"),
            )
            migration_sources = (
                ("legacy", (head_backup_cli, head_backup_host)),
                ("release-only", release_only),
            )
            active_contents = (target_cli.read_bytes(), target_host.read_bytes())
            all_migration_paths = (
                backup_cli,
                backup_host,
                head_backup_cli,
                head_backup_host,
                *release_only,
            )
            for namespace, source_pair in migration_sources:
                for (
                    source_cli_present,
                    source_host_present,
                    current_cli_present,
                    current_host_present,
                    crash_point,
                ) in migration_states:
                    with self.subTest(
                        migration_namespace=namespace,
                        crash_point=crash_point,
                    ):
                        for path in all_migration_paths:
                            path.unlink(missing_ok=True)
                        for path, contents, present in zip(
                            source_pair,
                            active_contents,
                            (source_cli_present, source_host_present),
                            strict=True,
                        ):
                            if present:
                                path.write_bytes(contents)
                        for path, contents, present in zip(
                            (backup_cli, backup_host),
                            active_contents,
                            (current_cli_present, current_host_present),
                            strict=True,
                        ):
                            if present:
                                path.write_bytes(contents)

                        resumed_migration = run(
                            command,
                            check=False,
                            capture_output=True,
                            encoding="utf-8",
                            input=payload,
                            text=True,
                            env=windows_environment,
                        )

                        self.assertEqual(
                            resumed_migration.returncode,
                            0,
                            resumed_migration.stderr,
                        )
                        self.assertEqual(
                            (backup_cli.read_bytes(), backup_host.read_bytes()),
                            active_contents,
                        )
                        self.assertFalse(source_pair[0].exists())
                        self.assertFalse(source_pair[1].exists())

            for path in target_dir.glob(".fleet-rollback.*"):
                path.unlink()
            head_backup_cli.write_bytes(target_cli.read_bytes())
            incomplete_legacy = run(
                command,
                check=False,
                capture_output=True,
                encoding="utf-8",
                input=payload,
                text=True,
                env=windows_environment,
            )
            self.assertNotEqual(incomplete_legacy.returncode, 0)
            self.assertIn(
                "Release rollback pair is incomplete", incomplete_legacy.stderr
            )

    def test_bundle_build_scripts_verify_the_pinned_commit(self) -> None:
        mac_script = fleet_release.build_script(self.hosts[0], self.release, True)
        windows_script = fleet_release.build_script(self.hosts[1], self.release, True)
        self.assertIn(self.release.fork_commit, mac_script)
        self.assertIn(self.release.fork_commit, windows_script)
        self.assertIn('"$toolchain_bin/cargo" build --release', mac_script)
        self.assertIn('normalize_release_lock "$build"', mac_script)
        self.assertIn("agents --help", mac_script)
        self.assertIn("agents --help", windows_script)
        self.assertIn(
            f"release_upstream_version={self.release.upstream_version}", mac_script
        )
        self.assertNotIn(re.escape(self.release.upstream_version), mac_script)
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
        self.assertIn('v8_directory="$fleet_directory/rusty-v8"', mac_script)
        self.assertIn("build-receipt-v1", mac_script)
        self.assertIn("refs/remotes/fleet/", windows_script)
        self.assertIn(
            "Existing build root is not the clean release worktree", windows_script
        )
        self.assertIn("cargo.exe", windows_script)
        self.assertIn("$ProgressPreference = 'SilentlyContinue'", windows_script)
        self.assertIn("$env:CODEX_FLEET_VSDEVCMD", windows_script)
        self.assertNotIn("call ", windows_script.lower())
        self.assertIn("Normalize-ReleaseLock $build", windows_script)
        self.assertIn(
            f"$releaseUpstreamVersion = '{self.release.upstream_version}'",
            windows_script,
        )
        self.assertNotIn(re.escape(self.release.upstream_version), windows_script)
        self.assertIn(
            "rusty_v8_ptrcomp_sandbox_release_x86_64-pc-windows-msvc.lib.gz",
            windows_script,
        )
        self.assertIn("RUSTY_V8_ARCHIVE", windows_script)
        self.assertIn("RUSTY_V8_SRC_BINDING_PATH", windows_script)
        self.assertIn("Get-FileHash", windows_script)
        self.assertIn("Join-Path ($build + '.fleet') 'rusty-v8'", windows_script)
        self.assertIn("build-receipt-v1", windows_script)

        for script, build_command, receipt_write in (
            (mac_script, '"$toolchain_bin/cargo" build', "printf '%s\\n'"),
            (
                windows_script,
                "cmd.exe /d /v:off /s /c $command",
                "$receiptLines = @(",
            ),
        ):
            build_finished = script.index(build_command)
            revalidation = script.rindex("Build root changed during build")
            receipt = script.index(receipt_write, revalidation)
            self.assertLess(build_finished, revalidation)
            self.assertLess(revalidation, receipt)

    @unittest.skipUnless(os.name == "posix", "POSIX build provenance test")
    def test_build_refuses_receipt_after_mid_build_head_change(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            build = root / "build"
            fake_bin = root / "fake-bin"
            source.mkdir()
            fake_bin.mkdir()
            (source / "codex-rs").mkdir()
            (source / "codex-rs" / "fixture").write_text("fixture\n")
            (source / ".gitignore").write_text("codex-rs/target/\n")
            run(["git", "init", "-q", str(source)], check=True)
            run(
                [
                    "git",
                    "-C",
                    str(source),
                    "-c",
                    "user.name=Fleet Test",
                    "-c",
                    "user.email=fleet@example.invalid",
                    "add",
                    ".",
                ],
                check=True,
            )
            run(
                [
                    "git",
                    "-C",
                    str(source),
                    "-c",
                    "user.name=Fleet Test",
                    "-c",
                    "user.email=fleet@example.invalid",
                    "commit",
                    "-qm",
                    "fixture",
                ],
                check=True,
            )
            commit = run(
                ["git", "-C", str(source), "rev-parse", "HEAD"],
                check=True,
                capture_output=True,
                text=True,
            ).stdout.strip()
            run(
                ["git", "-C", str(source), "branch", "fleet/provenance", commit],
                check=True,
            )
            run(
                ["git", "-C", str(source), "remote", "add", "fork", str(source)],
                check=True,
            )
            run(
                ["git", "-C", str(source), "worktree", "add", "-q", str(build)],
                check=True,
            )

            empty_hash = hashlib.sha256(b"").hexdigest()
            archive = "librusty_v8_ptrcomp_sandbox_release_aarch64-apple-darwin.a.gz"
            binding = "src_binding_ptrcomp_sandbox_release_aarch64-apple-darwin.rs"
            fake_curl = fake_bin / "curl"
            fake_curl.write_text(
                "#!/bin/sh\n"
                "url= destination=\n"
                'while [ "$#" -gt 0 ]; do\n'
                "  case $1 in -o) shift; destination=$1 ;; -*) ;; *) url=$1 ;; esac\n"
                "  shift\n"
                "done\n"
                "case $url in\n"
                f"  *.sha256) printf '%s  %s\\n%s  %s\\n' {empty_hash} {archive} {empty_hash} {binding} >\"$destination\" ;;\n"
                '  *) : >"$destination" ;;\n'
                "esac\n"
            )
            fake_curl.chmod(0o755)
            rustup = fake_bin / "rustup"
            rustup.write_text(f"#!/bin/sh\nprintf '%s\\n' {fake_bin / 'rustc'}\n")
            rustup.chmod(0o755)
            cargo = fake_bin / "cargo"
            cargo.write_text(
                "#!/bin/sh\n"
                "printf mutation >../mid-build-mutation\n"
                "git -C .. add mid-build-mutation\n"
                "git -C .. -c user.name='Fleet Test' -c user.email=fleet@example.invalid commit -qm mutation\n"
            )
            cargo.chmod(0o755)

            release = fleet_release.Release(
                name="provenance.test",
                fork_commit=commit,
                fork_ref="fleet/provenance",
                fork_remote=str(source),
                cli_version="codex-cli 1.2.3+provenance.test",
                server_commit=commit,
                rusty_v8_version="150.4.0",
            )
            host = fleet_release.Host(
                name="mac",
                platform="darwin",
                ssh_target="unused",
                source_root=str(source),
                build_root=str(build),
                server_root=str(source),
                cli_binary=str(root / "bin" / "codex"),
                code_mode_host_binary=str(root / "bin" / "codex-code-mode-host"),
                rust_toolchain="1.95.0",
                rust_target="aarch64-apple-darwin",
                rustup=str(rustup),
            )
            environment = os.environ.copy()
            environment["PATH"] = f"{fake_bin}{os.pathsep}{environment['PATH']}"

            result = run(
                ["sh", "-c", fleet_release.build_script(host, release, False)],
                check=False,
                capture_output=True,
                text=True,
                env=environment,
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("Build root changed during build", result.stderr)
            self.assertFalse(Path(host.build_receipt()).exists())

    @mock.patch("fleet_release.activation_script")
    @mock.patch("fleet_release.build_script", return_value="build script")
    @mock.patch("fleet_release.remote")
    @mock.patch("fleet_release.copy_bundle")
    @mock.patch("fleet_release.require_server")
    def test_stage_builds_without_activation(
        self,
        require_server: mock.Mock,
        copy_bundle: mock.Mock,
        remote: mock.Mock,
        build_script: mock.Mock,
        activation_script: mock.Mock,
    ) -> None:
        host = self.hosts[0]
        bundle = Path("release.bundle")
        remote.return_value = CompletedProcess(
            args=[], returncode=0, stdout="build-version=expected\n", stderr=""
        )

        fleet_release.apply_stage(host, self.release, bundle)

        require_server.assert_called_once_with(host, self.release)
        copy_bundle.assert_called_once_with(host, self.release, bundle)
        build_script.assert_called_once_with(host, self.release, True)
        remote.assert_called_once_with(host, "build script")
        activation_script.assert_not_called()

    @mock.patch("fleet_release.remote")
    def test_stage_gate_checks_only_the_server_checkout(
        self, remote: mock.Mock
    ) -> None:
        host = self.hosts[0]
        remote.return_value = CompletedProcess(
            args=[],
            returncode=0,
            stdout=(f"serverCommit={self.release.server_commit}\nserverStatus=\n"),
            stderr="",
        )

        fleet_release.require_server(host, self.release)

        remote.assert_called_once()
        gate_script = remote.call_args.args[1]
        self.assertIn(host.server_root, gate_script)
        self.assertNotIn(host.cli_binary, gate_script)
        self.assertNotIn(host.source_root, gate_script)
        self.assertNotIn(self.release.cli_version, gate_script)


if __name__ == "__main__":
    unittest.main()
