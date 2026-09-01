"""Tests for the manifest-driven native offload fleet controller."""

import json
import hashlib
import os
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
        self.assertEqual(self.release.marker, "+jkammerland.mcp.17")
        self.assertEqual(self.release.fork_ref, "fleet/jkammerland.mcp.17-source")
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

    def test_windows_status_script_delimits_path_before_colon(self) -> None:
        script = fleet_release.status_script(self.hosts[1])

        self.assertIn('"git failed in ${Path}:', script)
        self.assertNotIn('"git failed in $Path:', script)

    def test_activation_scripts_preserve_rollback_and_use_atomic_replacement(
        self,
    ) -> None:
        mac_script = fleet_release.activation_script(self.hosts[0], self.release)
        windows_script = fleet_release.activation_script(self.hosts[1], self.release)
        self.assertIn(".jkammerland.mcp.17-previous", mac_script)
        self.assertIn("mv -f", mac_script)
        self.assertIn("ReplaceFile", windows_script)
        self.assertIn(".jkammerland.mcp.17-previous", windows_script)
        self.assertIn("Build worktree is not clean at the manifest commit", mac_script)
        self.assertIn("Assert-CleanGit $build", windows_script)
        self.assertIn("agents --help", mac_script)
        self.assertIn("agents --help", windows_script)
        self.assertIn('"$target_host" --help', mac_script)
        self.assertIn("& $targetHost --help", windows_script)
        self.assertIn(
            "Build receipt does not match the reviewed binary pair", mac_script
        )
        self.assertIn(
            "Build receipt does not match the reviewed binary pair", windows_script
        )
        self.assertIn("rollback_pair", mac_script)
        self.assertIn("Restore-One $targetHost $backupHost", windows_script)
        self.assertIn(
            "MCP server is not clean at the compatible manifest commit", mac_script
        )
        self.assertIn("Assert-CleanGit $server", windows_script)

    @unittest.skipUnless(shutil.which("shasum"), "requires shasum")
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
            source_cli.write_text("#!/bin/sh\necho 'codex-cli wrong'\n")
            source_host.write_text("#!/bin/sh\nexit 0\n")
            target_cli.write_text("#!/bin/sh\necho 'codex-cli old'\n")
            target_host.write_text("#!/bin/sh\necho old-host\n")
            for path in (source_cli, source_host, target_cli, target_host):
                path.chmod(0o755)
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
            write_build_receipt(host, release)

            result = run(
                ["sh", "-c", fleet_release.activation_script(host, release)],
                check=False,
                capture_output=True,
                text=True,
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(target_cli.read_bytes(), original_cli)
            self.assertEqual(target_host.read_bytes(), original_host)
            self.assertFalse(Path(f"{target_cli}.test.release-previous").exists())
            self.assertFalse(Path(f"{target_host}.test.release-previous").exists())

            fail_agents = root / "fail-agents"
            source_cli.write_text(
                "#!/bin/sh\n"
                "if [ \"$1\" = --version ]; then echo 'codex-cli expected'; exit 0; fi\n"
                f'if [ "$1" = agents ] && [ "$2" = --help ]; then [ ! -e "{fail_agents}" ]; exit $?; fi\n'
                "exit 1\n"
            )
            source_cli.chmod(0o755)
            write_build_receipt(host, release)
            retry = run(
                ["sh", "-c", fleet_release.activation_script(host, release)],
                check=False,
                capture_output=True,
                text=True,
            )

            self.assertEqual(retry.returncode, 0, retry.stderr)
            self.assertEqual(target_cli.read_bytes(), source_cli.read_bytes())
            self.assertEqual(target_host.read_bytes(), source_host.read_bytes())

            backup_cli = Path(f"{target_cli}.test.release-previous")
            backup_host = Path(f"{target_host}.test.release-previous")
            self.assertEqual(backup_cli.read_bytes(), original_cli)
            self.assertEqual(backup_host.read_bytes(), original_host)
            target_host.chmod(0o644)
            repair_mode = run(
                ["sh", "-c", fleet_release.activation_script(host, release)],
                check=False,
                capture_output=True,
                text=True,
            )

            self.assertEqual(repair_mode.returncode, 0, repair_mode.stderr)
            self.assertTrue(os.access(target_host, os.X_OK))
            self.assertEqual(backup_cli.read_bytes(), original_cli)
            self.assertEqual(backup_host.read_bytes(), original_host)

            target_host.chmod(0o644)
            fail_agents.write_text("fail post-repair validation\n")
            failed_mode_repair = run(
                ["sh", "-c", fleet_release.activation_script(host, release)],
                check=False,
                capture_output=True,
                text=True,
            )

            self.assertNotEqual(failed_mode_repair.returncode, 0)
            self.assertFalse(os.access(target_host, os.X_OK))
            self.assertEqual(backup_cli.read_bytes(), original_cli)
            self.assertEqual(backup_host.read_bytes(), original_host)
            fail_agents.unlink()
            repaired_after_failure = run(
                ["sh", "-c", fleet_release.activation_script(host, release)],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(repaired_after_failure.returncode, 0)
            self.assertTrue(os.access(target_host, os.X_OK))

            active_cli = target_cli.read_bytes()
            active_host = target_host.read_bytes()
            Path(host.build_receipt()).unlink()
            missing_receipt = run(
                ["sh", "-c", fleet_release.activation_script(host, release)],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(missing_receipt.returncode, 0)
            self.assertIn("Missing reviewed build receipt", missing_receipt.stderr)
            self.assertEqual(target_cli.read_bytes(), active_cli)
            self.assertEqual(target_host.read_bytes(), active_host)
            write_build_receipt(host, release)
            Path(f"{target_host}.test.release-previous").unlink()
            incomplete_rollback = run(
                ["sh", "-c", fleet_release.activation_script(host, release)],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(incomplete_rollback.returncode, 0)
            self.assertIn(
                "Release rollback pair is incomplete", incomplete_rollback.stderr
            )
            self.assertEqual(target_cli.read_bytes(), active_cli)
            self.assertEqual(target_host.read_bytes(), active_host)
            Path(f"{target_host}.test.release-previous").write_bytes(original_host)
            (canonical / "untracked-internal").write_text("must block activation\n")
            source_cli.write_text(source_cli.read_text() + "# distinct candidate\n")
            dirty_source = run(
                ["sh", "-c", fleet_release.activation_script(host, release)],
                check=False,
                capture_output=True,
                text=True,
            )

            self.assertNotEqual(dirty_source.returncode, 0)
            self.assertIn(
                "Canonical source is not clean at the manifest commit",
                dirty_source.stderr,
            )
            self.assertEqual(target_cli.read_bytes(), active_cli)
            self.assertEqual(target_host.read_bytes(), active_host)

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
        if not fixture_cli.is_file() or not fixture_host.is_file():
            self.skipTest("maintained Windows Codex fixture binaries are unavailable")
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
            write_build_receipt(host, release)

            failed = run(
                [
                    "powershell.exe",
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    fleet_release.activation_script(host, release),
                ],
                check=False,
                capture_output=True,
                text=True,
            )

            self.assertNotEqual(failed.returncode, 0)
            self.assertEqual(target_cli.read_bytes(), original_cli)
            self.assertEqual(target_host.read_bytes(), original_host)
            self.assertFalse(Path(f"{target_cli}.test.release-previous").exists())
            self.assertFalse(Path(f"{target_host}.test.release-previous").exists())

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
            retry = run(
                [
                    "powershell.exe",
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    fleet_release.activation_script(host, release),
                ],
                check=False,
                capture_output=True,
                text=True,
            )

            self.assertEqual(retry.returncode, 0, retry.stderr)
            self.assertEqual(target_cli.read_bytes(), source_cli.read_bytes())
            self.assertEqual(target_host.read_bytes(), source_host.read_bytes())

    def test_bundle_build_scripts_verify_the_pinned_commit(self) -> None:
        mac_script = fleet_release.build_script(self.hosts[0], self.release, True)
        windows_script = fleet_release.build_script(self.hosts[1], self.release, True)
        self.assertIn(self.release.fork_commit, mac_script)
        self.assertIn(self.release.fork_commit, windows_script)
        self.assertIn('"$toolchain_bin/cargo" build --release', mac_script)
        self.assertIn('normalize_release_lock "$build"', mac_script)
        self.assertIn("agents --help", mac_script)
        self.assertIn("agents --help", windows_script)
        self.assertIn("0\\.0\\.0|0\\.0\\.0", mac_script)
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
        self.assertIn(
            r'''$command = 'call "{0}\Common7\Tools\VsDevCmd.bat"''',
            windows_script,
        )
        self.assertNotIn(r"""call "0\Common7""", windows_script)
        self.assertIn("Normalize-ReleaseLock $build", windows_script)
        self.assertIn("0\\.0\\.0|0\\.0\\.0", windows_script)
        self.assertIn(
            "rusty_v8_ptrcomp_sandbox_release_x86_64-pc-windows-msvc.lib.gz",
            windows_script,
        )
        self.assertIn("RUSTY_V8_ARCHIVE", windows_script)
        self.assertIn("RUSTY_V8_SRC_BINDING_PATH", windows_script)
        self.assertIn("Get-FileHash", windows_script)
        self.assertIn("Join-Path ($build + '.fleet') 'rusty-v8'", windows_script)
        self.assertIn("build-receipt-v1", windows_script)

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


if __name__ == "__main__":
    unittest.main()
