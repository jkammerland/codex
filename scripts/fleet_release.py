#!/usr/bin/env python3
"""Synchronize the maintained Codex fork across native offload hosts."""

import argparse
import base64
import hashlib
import json
import ntpath
import posixpath
import re
import shlex
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_MANIFEST = ROOT / "fleet" / "release.json"
COMMIT = re.compile(r"[0-9a-f]{40}")
SEMVER = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+")
RELEASE_NAME = re.compile(r"[a-z0-9]+(?:[._-][a-z0-9]+)*")
FORK_REF_COMPONENT = re.compile(r"[A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9_-])?")
RUST_TOOLCHAIN = re.compile(r"[A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9])?")
RUST_TARGET = re.compile(r"[A-Za-z0-9_]+(?:[A-Za-z0-9_.-]*[A-Za-z0-9_])?")
CLI_VERSION = re.compile(
    r"codex-cli [0-9]+\.[0-9]+\.[0-9]+"
    r"(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
)
COMMAND_CONTROL = re.compile(r"[\x00-\x1f\x7f]")
STATUS_FIELDS = frozenset(
    {
        "activationJournal",
        "cliVersion",
        "cliSha256",
        "codeModeHostSha256",
        "codeModeHostExecutable",
        "receiptForkCommit",
        "receiptCliVersion",
        "receiptCliSha256",
        "receiptCodeModeHostSha256",
        "serverCommit",
        "serverStatus",
        "sourceCommit",
        "sourceStatus",
    }
)
WINDOWS_STDIN_BOOTSTRAP = (
    "$payload = [Console]::In.ReadToEnd(); "
    "$source = [Text.Encoding]::Unicode.GetString("
    "[Convert]::FromBase64String($payload)); "
    "& ([ScriptBlock]::Create($source))"
)
INSTALLATION_LOCK_NAME = ".fleet-installation.lock"
WINDOWS_INSTALLATION_LOCK_PATH = r"%LOCALAPPDATA%\OpenAI\Codex\fleet-activation.lock"
WINDOWS_BUILD_COMMAND = (
    'ver >nul && "%CODEX_FLEET_VSDEVCMD%" -arch=amd64 '
    '&& cd /d "%CODEX_FLEET_BUILD_ROOT%\\codex-rs" '
    '&& "%CODEX_FLEET_CARGO%" "%CODEX_FLEET_TOOLCHAIN%" build --release '
    "--bin codex --bin codex-code-mode-host"
)
SSH_OPTIONS = (
    "-T",
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=8",
    "-o",
    "ControlMaster=no",
    "-o",
    "ControlPath=none",
    "-o",
    "ForwardAgent=no",
    "-o",
    "ClearAllForwardings=yes",
)


class FleetError(RuntimeError):
    """The requested fleet state cannot be safely verified or applied."""


@dataclass(frozen=True)
class Release:
    name: str
    fork_commit: str
    fork_ref: str
    fork_remote: str
    cli_version: str
    server_commit: str
    rusty_v8_version: str

    @property
    def marker(self) -> str:
        return f"+{self.name}"

    @property
    def runtime_version(self) -> str:
        prefix = "codex-cli "
        if not self.cli_version.startswith(prefix):
            raise FleetError("expectedCliVersion must start with 'codex-cli '")
        return self.cli_version.removeprefix(prefix)

    @property
    def upstream_version(self) -> str:
        return self.runtime_version.partition("+")[0]


@dataclass(frozen=True)
class Host:
    name: str
    platform: str
    ssh_target: str
    source_root: str
    build_root: str
    server_root: str
    cli_binary: str
    code_mode_host_binary: str
    rust_toolchain: str
    rust_target: str
    rustup: str | None

    @property
    def windows(self) -> bool:
        return self.platform == "windows"

    def build_binary(self, name: str) -> str:
        separator = "\\" if self.windows else "/"
        extension = ".exe" if self.windows else ""
        return separator.join(
            [
                self.build_root,
                "codex-rs",
                "target",
                "release",
                f"{name}{extension}",
            ]
        )

    def bundle_path(self, release: Release) -> str:
        dirname = ntpath.dirname if self.windows else posixpath.dirname
        separator = "\\" if self.windows else "/"
        return f"{dirname(self.source_root)}{separator}codex-{release.name}.bundle"

    def build_receipt(self) -> str:
        separator = "\\" if self.windows else "/"
        return f"{self.build_root}.fleet{separator}build-receipt-v1"


@dataclass(frozen=True)
class Status:
    host: Host
    values: dict[str, str]
    error: str | None = None

    def drift(self, release: Release) -> tuple[str, ...]:
        if self.error:
            return ("transport",)
        expected = {
            "activationJournal": "none",
            "cliVersion": release.cli_version,
            "serverCommit": release.server_commit,
            "sourceCommit": release.fork_commit,
            "receiptForkCommit": release.fork_commit,
            "receiptCliVersion": release.cli_version,
            "codeModeHostExecutable": "true",
        }
        drift = [
            key for key, value in expected.items() if self.values.get(key) != value
        ]
        drift.extend(
            key for key in ("serverStatus", "sourceStatus") if self.values.get(key, "")
        )
        drift.extend(
            active_key
            for active_key, receipt_key in (
                ("cliSha256", "receiptCliSha256"),
                ("codeModeHostSha256", "receiptCodeModeHostSha256"),
            )
            if self.values.get(active_key) != self.values.get(receipt_key)
        )
        return tuple(drift)


def required(value: dict[str, Any], key: str) -> str:
    result = value.get(key)
    if not isinstance(result, str) or not result:
        raise FleetError(f"{key} must be a non-empty string")
    return result


def command_value(name: str, value: str, *, reject_option: bool = False) -> str:
    if COMMAND_CONTROL.search(value):
        raise FleetError(f"{name} must not contain control characters")
    if reject_option and value.startswith("-"):
        raise FleetError(f"{name} must not be option-like")
    return value


def fork_ref(value: str) -> str:
    components = value.split("/")
    if (
        len(value) > 255
        or value.startswith("refs/")
        or ".." in value
        or any(
            FORK_REF_COMPONENT.fullmatch(component) is None
            or component.lower().endswith(".lock")
            for component in components
        )
    ):
        raise FleetError("forkRef must be a safe abbreviated branch name")
    return value


def absolute(platform: str, name: str, value: str) -> None:
    is_absolute = (
        bool(re.match(r"^[A-Za-z]:[\\/]", value))
        if platform == "windows"
        else value.startswith("/")
    )
    if not is_absolute:
        raise FleetError(f"{name} must be an absolute {platform} path")


def host_from_raw(raw: Any, *, strict: bool = False) -> Host:
    if not isinstance(raw, dict):
        raise FleetError("every host must be an object")
    platform = required(raw, "platform")
    if platform not in {"darwin", "windows"}:
        raise FleetError("host platform must be darwin or windows")
    paths = {
        key: required(raw, key)
        for key in (
            "sourceRoot",
            "buildRoot",
            "serverRoot",
            "cliBinary",
            "codeModeHostBinary",
        )
    }
    for name, path in paths.items():
        if strict:
            command_value(name, path)
        absolute(platform, name, path)
    rustup = raw.get("rustup")
    if rustup is not None:
        if not isinstance(rustup, str) or not rustup:
            raise FleetError("rustup must be a non-empty string when provided")
        if strict:
            command_value("rustup", rustup)
        absolute(platform, "rustup", rustup)
    if platform == "darwin" and rustup is None:
        raise FleetError("darwin hosts require rustup")
    rust_target = required(raw, "rustTarget")
    if strict and RUST_TARGET.fullmatch(rust_target) is None:
        raise FleetError("rustTarget must be a safe Rust target triple")
    expected_target_suffix = (
        "-pc-windows-msvc" if platform == "windows" else "-apple-darwin"
    )
    if not rust_target.endswith(expected_target_suffix):
        raise FleetError(
            f"rustTarget for {platform} must end with {expected_target_suffix}"
        )
    rust_toolchain = required(raw, "rustToolchain")
    if strict and RUST_TOOLCHAIN.fullmatch(rust_toolchain) is None:
        raise FleetError("rustToolchain must be a safe rustup toolchain name")
    ssh_target = required(raw, "sshTarget")
    if strict:
        ssh_target = command_value("sshTarget", ssh_target, reject_option=True)
    if strict and any(character.isspace() for character in ssh_target):
        raise FleetError("sshTarget must not contain whitespace")
    return Host(
        name=(
            command_value("name", required(raw, "name"))
            if strict
            else required(raw, "name")
        ),
        platform=platform,
        ssh_target=ssh_target,
        source_root=paths["sourceRoot"],
        build_root=paths["buildRoot"],
        server_root=paths["serverRoot"],
        cli_binary=paths["cliBinary"],
        code_mode_host_binary=paths["codeModeHostBinary"],
        rust_toolchain=rust_toolchain,
        rust_target=rust_target,
        rustup=rustup,
    )


def load_manifest(path: Path) -> tuple[Release, tuple[Host, ...]]:
    try:
        raw = json.loads(path.read_text())
    except OSError as error:
        raise FleetError(f"could not read manifest {path}: {error}") from error
    except json.JSONDecodeError as error:
        raise FleetError(f"invalid JSON in {path}: {error}") from error
    if not isinstance(raw, dict) or raw.get("schemaVersion") not in {1, 2}:
        raise FleetError("manifest schemaVersion must be 1 or 2")
    strict = raw["schemaVersion"] == 2
    release_raw = raw.get("release")
    if not isinstance(release_raw, dict):
        raise FleetError("release must be an object")
    commits = {
        key: required(release_raw, key) for key in ("forkCommit", "mcpServerCommit")
    }
    if any(COMMIT.fullmatch(commit) is None for commit in commits.values()):
        raise FleetError("release commits must be 40-character lowercase Git commits")
    rusty_v8_version = required(release_raw, "rustyV8Version")
    if SEMVER.fullmatch(rusty_v8_version) is None:
        raise FleetError("rustyV8Version must contain three numeric components")
    release_name = required(release_raw, "name")
    if strict and (
        len(release_name) > 64 or RELEASE_NAME.fullmatch(release_name) is None
    ):
        raise FleetError(
            "release.name must be a filename-safe lowercase identifier of at most 64 characters"
        )
    fork_remote = required(release_raw, "forkRemote")
    cli_version = required(release_raw, "expectedCliVersion")
    if strict:
        fork_remote = command_value("forkRemote", fork_remote, reject_option=True)
        cli_version = command_value("expectedCliVersion", cli_version)
    if strict and CLI_VERSION.fullmatch(cli_version) is None:
        raise FleetError("expectedCliVersion must be a codex-cli semantic version")
    release = Release(
        name=release_name,
        fork_commit=commits["forkCommit"],
        fork_ref=(
            fork_ref(required(release_raw, "forkRef"))
            if strict
            else required(release_raw, "forkRef")
        ),
        fork_remote=fork_remote,
        cli_version=cli_version,
        server_commit=commits["mcpServerCommit"],
        rusty_v8_version=rusty_v8_version,
    )
    release.upstream_version
    hosts_raw = raw.get("hosts")
    if not isinstance(hosts_raw, list) or not hosts_raw:
        raise FleetError("hosts must be a non-empty array")
    hosts = tuple(host_from_raw(host, strict=strict) for host in hosts_raw)
    if len({host.name for host in hosts}) != len(hosts):
        raise FleetError("host names must be unique")
    return release, hosts


def selected_hosts(
    hosts: tuple[Host, ...], names: list[str] | None
) -> tuple[Host, ...]:
    if not names:
        return hosts
    index = {host.name: host for host in hosts}
    try:
        return tuple(index[name] for name in names)
    except KeyError as error:
        raise FleetError(f"unknown host {error.args[0]!r}") from error


def quote_ps(value: str) -> str:
    return "'" + value.replace("'", "''") + "'"


def _lexical_target_pair_token(host: Host) -> str:
    path_module = ntpath if host.windows else posixpath
    slots = (
        path_module.normpath(path_module.dirname(host.cli_binary)),
        path_module.basename(host.cli_binary),
        path_module.normpath(path_module.dirname(host.code_mode_host_binary)),
        path_module.basename(host.code_mode_host_binary),
    )
    return hashlib.sha256("\0".join(slots).encode("utf-8")).hexdigest()


def rollback_paths(host: Host, release_name: str) -> tuple[str, str]:
    path_module = ntpath if host.windows else posixpath
    token = hashlib.sha256(
        f"{_lexical_target_pair_token(host)}\0{release_name}".encode("utf-8")
    ).hexdigest()
    cli = path_module.join(
        path_module.dirname(host.cli_binary),
        f".fleet-rollback.{token}.cli",
    )
    code_mode_host = path_module.join(
        path_module.dirname(host.code_mode_host_binary),
        f".fleet-rollback.{token}.host",
    )
    return cli, code_mode_host


def transaction_path(host: Host) -> str:
    path_module = ntpath if host.windows else posixpath
    return path_module.join(
        path_module.dirname(host.cli_binary),
        f".fleet-transaction.{_lexical_target_pair_token(host)}.json",
    )


def release_only_rollback_paths(host: Host, release_name: str) -> tuple[str, str]:
    path_module = ntpath if host.windows else posixpath
    token = hashlib.sha256(release_name.encode("utf-8")).hexdigest()
    return (
        path_module.join(
            path_module.dirname(host.cli_binary),
            f".fleet-rollback.{token}.cli",
        ),
        path_module.join(
            path_module.dirname(host.code_mode_host_binary),
            f".fleet-rollback.{token}.host",
        ),
    )


def release_only_transaction_path(host: Host) -> str:
    return f"{host.cli_binary}..fleet-transaction.json"


def target_lock_paths(
    host: Host, *, windows_installation_lock_path: str | None = None
) -> tuple[str, ...]:
    if host.windows:
        # A single per-user lease is intentionally independent of manifest paths.
        # Windows junctions and 8.3 names can give the same directory several
        # lexical names; locking each spelling can make one guardian deadlock on
        # a lock it already owns.
        return (windows_installation_lock_path or WINDOWS_INSTALLATION_LOCK_PATH,)
    path_module = ntpath if host.windows else posixpath
    paths_by_identity: dict[str, str] = {}
    # Transaction and staging cleanup is directory-wide, so every target directory
    # participates in the lease even when two manifests share only one binary.
    for target in (host.cli_binary, host.code_mode_host_binary):
        directory = path_module.normpath(path_module.dirname(target))
        identity = ntpath.normcase(directory) if host.windows else directory
        paths_by_identity.setdefault(
            identity,
            path_module.join(directory, INSTALLATION_LOCK_NAME),
        )
    return tuple(paths_by_identity[key] for key in sorted(paths_by_identity))


def windows_powershell_stdin(script: str) -> tuple[list[str], str]:
    bootstrap = base64.b64encode(WINDOWS_STDIN_BOOTSTRAP.encode("utf-16le")).decode(
        "ascii"
    )
    payload = base64.b64encode(script.encode("utf-16le")).decode("ascii")
    return (
        [
            "powershell.exe",
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
            bootstrap,
        ],
        payload,
    )


def remote(host: Host, script: str) -> subprocess.CompletedProcess[str]:
    ssh_target = command_value("sshTarget", host.ssh_target, reject_option=True)
    if any(character.isspace() for character in ssh_target):
        raise FleetError("sshTarget must not contain whitespace")
    if host.windows:
        powershell, payload = windows_powershell_stdin(script)
        command = ["ssh", *SSH_OPTIONS, "--", ssh_target, *powershell]
        return subprocess.run(
            command,
            check=False,
            capture_output=True,
            encoding="utf-8",
            input=payload,
            text=True,
        )
    return subprocess.run(
        ["ssh", *SSH_OPTIONS, "--", ssh_target, "sh -s"],
        check=False,
        capture_output=True,
        input=script,
        text=True,
    )


def error_message(action: str, result: subprocess.CompletedProcess[str]) -> str:
    output = "\n".join(part for part in (result.stdout, result.stderr) if part).strip()
    return f"{action} failed with exit code {result.returncode}: {output}"


def status_script(host: Host) -> str:
    pending_transaction = transaction_path(host)
    if host.windows:
        return f"""$ErrorActionPreference = 'Stop'
Add-Type @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;

public static class FleetStatusFileIdentity {{
    private const uint FILE_SHARE_READ = 0x00000001;
    private const uint FILE_SHARE_WRITE = 0x00000002;
    private const uint FILE_SHARE_DELETE = 0x00000004;
    private const uint OPEN_EXISTING = 3;
    private const uint FILE_FLAG_BACKUP_SEMANTICS = 0x02000000;

    [DllImport("kernel32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
    private static extern SafeFileHandle CreateFile(
        string name, uint access, uint share, IntPtr security, uint creation,
        uint flags, IntPtr template
    );

    [DllImport("kernel32.dll", SetLastError=true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool GetFileInformationByHandle(
        SafeFileHandle handle, out ByHandleFileInformation information
    );

    [StructLayout(LayoutKind.Sequential)]
    private struct ByHandleFileInformation {{
        public uint FileAttributes;
        public System.Runtime.InteropServices.ComTypes.FILETIME CreationTime;
        public System.Runtime.InteropServices.ComTypes.FILETIME LastAccessTime;
        public System.Runtime.InteropServices.ComTypes.FILETIME LastWriteTime;
        public uint VolumeSerialNumber;
        public uint FileSizeHigh;
        public uint FileSizeLow;
        public uint NumberOfLinks;
        public uint FileIndexHigh;
        public uint FileIndexLow;
    }}

    public static string Get(string path) {{
        using (SafeFileHandle handle = CreateFile(
            path, 0, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            IntPtr.Zero, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, IntPtr.Zero
        )) {{
            if (handle.IsInvalid) throw new Win32Exception(Marshal.GetLastWin32Error());
            ByHandleFileInformation information;
            if (!GetFileInformationByHandle(handle, out information))
                throw new Win32Exception(Marshal.GetLastWin32Error());
            return information.VolumeSerialNumber.ToString("X8") + ":" +
                information.FileIndexHigh.ToString("X8") +
                information.FileIndexLow.ToString("X8");
        }}
    }}
}}
'@
function Field($Name, $Value) {{ Write-Output "$Name=$Value" }}
function GitValue($Path, [string[]]$Arguments) {{
  if (-not (Test-Path -LiteralPath (Join-Path $Path '.git'))) {{ return 'missing' }}
  $value = ((& git -C $Path @Arguments 2>$null | Out-String).Trim())
  if ($LASTEXITCODE -ne 0) {{ throw "git failed in ${{Path}}: $($Arguments -join ' ')" }}
  return $value
}}
function HashValue($Path) {{
  if (-not (Test-Path -LiteralPath $Path)) {{ return 'missing' }}
  return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}}
function ReceiptValue($Path, $Name) {{
  if (-not (Test-Path -LiteralPath $Path)) {{ return 'missing' }}
  $prefix = "$Name="
  $matches = @(Get-Content -LiteralPath $Path | Where-Object {{ $_.StartsWith($prefix, [StringComparison]::Ordinal) }})
  if ($matches.Count -ne 1) {{ return 'invalid' }}
  return $matches[0].Substring($prefix.Length)
}}
function Test-SameStatusPath($Left, $Right) {{
  if ((Test-Path -LiteralPath $Left) -and (Test-Path -LiteralPath $Right)) {{
    return [FleetStatusFileIdentity]::Get($Left) -eq [FleetStatusFileIdentity]::Get($Right)
  }}
  return [StringComparer]::OrdinalIgnoreCase.Equals(
    [System.IO.Path]::GetFullPath($Left),
    [System.IO.Path]::GetFullPath($Right)
  )
}}
function PendingJournalValue($TargetCli, $TargetHost, $FixedJournal) {{
  if (Test-Path -LiteralPath $FixedJournal -PathType Leaf) {{ return 'pending' }}
  $directory = Split-Path -Parent $TargetCli
  if (-not (Test-Path -LiteralPath $directory -PathType Container)) {{ return 'none' }}
  $legacyPrefix = (Split-Path -Leaf $TargetCli) + '.'
  $journals = @(Get-ChildItem -File -Force -LiteralPath $directory | Where-Object {{
    ($_.Name.StartsWith($legacyPrefix, [StringComparison]::OrdinalIgnoreCase) -and
     $_.Name.EndsWith('-transaction.json', [StringComparison]::OrdinalIgnoreCase)) -or
    ($_.Name.StartsWith('.fleet-transaction.', [StringComparison]::OrdinalIgnoreCase) -and
     $_.Name.EndsWith('.json', [StringComparison]::OrdinalIgnoreCase))
  }})
  foreach ($journal in $journals) {{
    try {{
      $transaction = Get-Content -Raw -LiteralPath $journal.FullName | ConvertFrom-Json -ErrorAction Stop
      if (($transaction.targetCli -is [string]) -and
          ($transaction.targetHost -is [string]) -and
          (Test-SameStatusPath $transaction.targetCli $TargetCli) -and
          (Test-SameStatusPath $transaction.targetHost $TargetHost)) {{
        return 'pending'
      }}
    }} catch {{
      # A journal is relevant only after its stored target pair authenticates.
    }}
  }}
  return 'none'
}}
$cli = {quote_ps(host.cli_binary)}
$codeModeHost = {quote_ps(host.code_mode_host_binary)}
$fixedJournal = {quote_ps(pending_transaction)}
$receipt = {quote_ps(host.build_receipt())}
$server = {quote_ps(host.server_root)}
$source = {quote_ps(host.source_root)}
if (Test-Path -LiteralPath $cli) {{
  $cliVersion = ((& $cli --version 2>&1 | Out-String).Trim())
  if ($LASTEXITCODE -ne 0) {{ throw 'Codex version check failed' }}
}} else {{ $cliVersion = 'missing' }}
Field 'activationJournal' (PendingJournalValue $cli $codeModeHost $fixedJournal)
Field 'cliVersion' $cliVersion
Field 'cliSha256' (HashValue $cli)
Field 'codeModeHostSha256' (HashValue $codeModeHost)
Field 'codeModeHostExecutable' $(if (Test-Path -LiteralPath $codeModeHost -PathType Leaf) {{ 'true' }} else {{ 'false' }})
Field 'receiptForkCommit' (ReceiptValue $receipt 'forkCommit')
Field 'receiptCliVersion' (ReceiptValue $receipt 'cliVersion')
Field 'receiptCliSha256' (ReceiptValue $receipt 'cliSha256')
Field 'receiptCodeModeHostSha256' (ReceiptValue $receipt 'hostSha256')
Field 'serverCommit' (GitValue $server @('rev-parse', 'HEAD'))
Field 'serverStatus' (GitValue $server @('status', '--porcelain'))
Field 'sourceCommit' (GitValue $source @('rev-parse', 'HEAD'))
Field 'sourceStatus' (GitValue $source @('status', '--porcelain'))
"""
    return f"""set -u
field() {{ printf '%s=%s\\n' "$1" "$2"; }}
git_value() {{ path=$1; shift; if [ -d "$path/.git" ]; then git -C "$path" "$@" 2>/dev/null; else printf missing; fi; }}
hash_value() {{ if [ -f "$1" ]; then shasum -a 256 "$1" | awk '{{print $1}}'; else printf missing; fi; }}
executable_value() {{ if [ -x "$1" ]; then printf true; else printf false; fi; }}
receipt_value() {{ if [ ! -f "$2" ]; then printf missing; return; fi; value=$(awk -v key="$1" 'index($0, key "=") == 1 {{ count++; value=substr($0, length(key) + 2) }} END {{ if (count != 1) exit 1; print value }}' "$2") || {{ printf invalid; return; }}; printf '%s' "$value"; }}
pending_journal_value() {{
  python3 - "$1" "$2" "$3" <<'PY'
import json
import sys
from pathlib import Path

target_cli, target_host, fixed = map(Path, sys.argv[1:])
if fixed.is_file():
    print("pending", end="")
    raise SystemExit
directory = target_cli.parent
if not directory.is_dir():
    print("none", end="")
    raise SystemExit
candidates = set(directory.glob(f"{{target_cli.name}}.*-transaction.json"))
candidates.update(directory.glob(".fleet-transaction.*.json"))
targets = (target_cli.resolve(strict=False), target_host.resolve(strict=False))
for candidate in candidates:
    try:
        transaction = json.loads(candidate.read_text(encoding="utf-8"))
        stored = (
            Path(transaction["targetCli"]).resolve(strict=False),
            Path(transaction["targetHost"]).resolve(strict=False),
        )
    except (OSError, json.JSONDecodeError, KeyError, TypeError):
        continue
    if stored == targets:
        print("pending", end="")
        raise SystemExit
print("none", end="")
PY
}}
cli={shlex.quote(host.cli_binary)}
code_mode_host={shlex.quote(host.code_mode_host_binary)}
fixed_journal={shlex.quote(pending_transaction)}
receipt={shlex.quote(host.build_receipt())}
if [ -x "$cli" ]; then cli_version=$("$cli" --version 2>&1) || exit 1; else cli_version=missing; fi
server_commit=$(git_value {shlex.quote(host.server_root)} rev-parse HEAD) || exit 1
server_status=$(git_value {shlex.quote(host.server_root)} status --porcelain) || exit 1
source_commit=$(git_value {shlex.quote(host.source_root)} rev-parse HEAD) || exit 1
source_status=$(git_value {shlex.quote(host.source_root)} status --porcelain) || exit 1
field activationJournal "$(pending_journal_value "$cli" "$code_mode_host" "$fixed_journal")"
field cliVersion "$cli_version"
field cliSha256 "$(hash_value "$cli")"
field codeModeHostSha256 "$(hash_value "$code_mode_host")"
field codeModeHostExecutable "$(executable_value "$code_mode_host")"
field receiptForkCommit "$(receipt_value forkCommit "$receipt")"
field receiptCliVersion "$(receipt_value cliVersion "$receipt")"
field receiptCliSha256 "$(receipt_value cliSha256 "$receipt")"
field receiptCodeModeHostSha256 "$(receipt_value hostSha256 "$receipt")"
field serverCommit "$server_commit"
field serverStatus "$server_status"
field sourceCommit "$source_commit"
field sourceStatus "$source_status"
"""


def parse_status(output: str) -> dict[str, str]:
    return {
        key: value.strip()
        for line in output.splitlines()
        for key, separator, value in [line.partition("=")]
        if separator and key in STATUS_FIELDS
    }


def host_status(host: Host) -> Status:
    result = remote(host, status_script(host))
    if result.returncode:
        return Status(host, {}, error_message("status", result))
    values = parse_status(result.stdout)
    if len(values) != len(STATUS_FIELDS):
        return Status(host, values, "status output was incomplete")
    return Status(host, values)


def server_status_script(host: Host) -> str:
    if host.windows:
        return f"""$ErrorActionPreference = 'Stop'
$server = {quote_ps(host.server_root)}
if (-not (Test-Path -LiteralPath (Join-Path $server '.git'))) {{
  Write-Output 'serverCommit=missing'
  Write-Output 'serverStatus=missing'
  exit 0
}}
$commit = ((& git -C $server rev-parse HEAD 2>$null | Out-String).Trim())
if ($LASTEXITCODE -ne 0) {{ throw 'Could not inspect MCP server commit' }}
$status = ((& git -C $server status --porcelain 2>$null | Out-String).Trim())
if ($LASTEXITCODE -ne 0) {{ throw 'Could not inspect MCP server status' }}
Write-Output "serverCommit=$commit"
Write-Output "serverStatus=$status"
"""
    return f"""set -u
server={shlex.quote(host.server_root)}
if [ ! -e "$server/.git" ]; then
  printf 'serverCommit=missing\\nserverStatus=missing\\n'
  exit 0
fi
commit=$(git -C "$server" rev-parse HEAD) || exit 1
status=$(git -C "$server" status --porcelain) || exit 1
printf 'serverCommit=%s\\nserverStatus=%s\\n' "$commit" "$status"
"""


def require_server(host: Host, release: Release) -> None:
    result = remote(host, server_status_script(host))
    values = parse_status(result.stdout) if result.returncode == 0 else {}
    if (
        result.returncode
        or set(values) != {"serverCommit", "serverStatus"}
        or values.get("serverCommit") != release.server_commit
        or values.get("serverStatus")
    ):
        detail = (
            error_message("MCP server status", result)
            if result.returncode
            else json.dumps(values, sort_keys=True)
        )
        raise FleetError(f"{host.name}: MCP server release gate failed: {detail}")


def copy_bundle(host: Host, release: Release, bundle: Path) -> None:
    ssh_target = command_value("sshTarget", host.ssh_target, reject_option=True)
    if any(character.isspace() for character in ssh_target):
        raise FleetError("sshTarget must not contain whitespace")
    destination = host.bundle_path(release).replace("\\", "/")
    result = subprocess.run(
        ["scp", *SSH_OPTIONS, "--", str(bundle), f"{ssh_target}:{destination}"],
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode:
        raise FleetError(error_message(f"copying bundle to {host.name}", result))


def build_script(host: Host, release: Release, use_bundle: bool) -> str:
    if RUST_TOOLCHAIN.fullmatch(host.rust_toolchain) is None:
        raise FleetError("rustToolchain must be a safe rustup toolchain name")
    source = host.source_root
    build = host.build_root
    staged_ref = f"refs/remotes/fleet/{release.fork_ref}"
    v8_profile = "ptrcomp_sandbox_release"
    v8_archive = (
        f"rusty_v8_{v8_profile}_{host.rust_target}.lib.gz"
        if host.windows
        else f"librusty_v8_{v8_profile}_{host.rust_target}.a.gz"
    )
    v8_binding = f"src_binding_{v8_profile}_{host.rust_target}.rs"
    v8_checksums = f"rusty_v8_{v8_profile}_{host.rust_target}.sha256"
    v8_base_url = (
        "https://github.com/openai/codex/releases/download/"
        f"rusty-v8-v{release.rusty_v8_version}"
    )
    source_ref = f"refs/heads/{release.fork_ref}" if use_bundle else release.fork_ref
    refspec = f"{source_ref}:{staged_ref}"
    fetch_source = host.bundle_path(release) if use_bundle else "fork"
    fetch = (
        f"git -C {shlex.quote(source)} fetch {shlex.quote(fetch_source)} "
        f"{shlex.quote(refspec)}"
    )
    if host.windows:
        ps_fetch = f"git -C $source fetch {quote_ps(fetch_source)} {quote_ps(refspec)}"
        return f"""$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$releaseUpstreamVersion = {quote_ps(release.upstream_version)}
$expectedCliVersion = {quote_ps(release.cli_version)}
function Normalize-ReleaseLock($Path) {{
  $status = @(& git -C $Path status --short --untracked-files=all)
  if ($LASTEXITCODE -ne 0) {{ throw 'Could not inspect the build root status' }}
  if ($status.Count -eq 0) {{ return }}
  if ($status.Count -ne 1 -or $status[0].Trim() -ne 'M codex-rs/Cargo.lock') {{ throw 'Build root contains changes other than the generated release lockfile' }}
  $changedLines = @(& git -C $Path diff --unified=0 -- codex-rs/Cargo.lock | Where-Object {{ ($_ -match '^[+-]') -and ($_ -notmatch '^(---|\\+\\+\\+)') }})
  $validChangedLines = @(
    '-version = "0.0.0"',
    '+version = "0.0.0"',
    "-version = `"$releaseUpstreamVersion`"",
    "+version = `"$releaseUpstreamVersion`""
  )
  $invalidLines = @($changedLines | Where-Object {{ $_ -notin $validChangedLines }})
  if ($changedLines.Count -eq 0 -or $invalidLines.Count -ne 0) {{ throw 'Build root Cargo.lock contains unexpected changes' }}
  & git -C $Path restore --worktree -- codex-rs/Cargo.lock
  if ($LASTEXITCODE -ne 0) {{ throw 'Could not normalize the generated release lockfile' }}
}}
function Assert-ReleaseWorktree($Path, $ExpectedCommit, $Failure) {{
  if (-not (Test-Path -LiteralPath (Join-Path $Path '.git'))) {{ throw $Failure }}
  $status = ((& git -C $Path status --porcelain 2>$null | Out-String).Trim())
  if ($LASTEXITCODE -ne 0) {{ throw $Failure }}
  $commit = ((& git -C $Path rev-parse HEAD 2>$null | Out-String).Trim())
  if ($LASTEXITCODE -ne 0 -or -not [string]::IsNullOrEmpty($status) -or $commit -ne $ExpectedCommit) {{ throw $Failure }}
}}
$source = {quote_ps(source)}; $build = {quote_ps(build)}
if (Test-Path -LiteralPath $source) {{
  if (-not (Test-Path -LiteralPath (Join-Path $source '.git'))) {{ throw 'Source root must be a Git checkout' }}
  $sourceStatus = ((& git -C $source status --porcelain 2>$null | Out-String).Trim())
  if ($LASTEXITCODE -ne 0 -or -not [string]::IsNullOrEmpty($sourceStatus)) {{ throw 'Source root must be a clean Git checkout' }}
}} else {{ git clone --origin fork {quote_ps(release.fork_remote)} $source; if ($LASTEXITCODE -ne 0) {{ throw 'Clone failed' }} }}
{ps_fetch}; if ($LASTEXITCODE -ne 0) {{ throw 'Fetch failed' }}
git -C $source switch -C {quote_ps(release.fork_ref)} {quote_ps(staged_ref)}; if ($LASTEXITCODE -ne 0) {{ throw 'Switch failed' }}
$sourceCommit = ((& git -C $source rev-parse HEAD 2>$null | Out-String).Trim())
if ($LASTEXITCODE -ne 0 -or $sourceCommit -ne {quote_ps(release.fork_commit)}) {{ throw 'Source commit does not match manifest' }}
if (Test-Path -LiteralPath $build) {{
  Normalize-ReleaseLock $build
  Assert-ReleaseWorktree $build {quote_ps(release.fork_commit)} 'Existing build root is not the clean release worktree'
}} else {{ git -C $source worktree add --detach $build {quote_ps(release.fork_commit)}; if ($LASTEXITCODE -ne 0) {{ throw 'Worktree creation failed' }} }}
$vswhere = Join-Path ${"{"}env:ProgramFiles(x86){"}"} 'Microsoft Visual Studio\\Installer\\vswhere.exe'
$vs = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if ([string]::IsNullOrEmpty($vs)) {{ throw 'No x64 MSVC toolchain found' }}
$v8Directory = Join-Path ($build + '.fleet') 'rusty-v8'
New-Item -ItemType Directory -Force -Path $v8Directory | Out-Null
$v8BaseUrl = {quote_ps(v8_base_url)}
$v8ArchiveName = {quote_ps(v8_archive)}
$v8BindingName = {quote_ps(v8_binding)}
$v8ChecksumsName = {quote_ps(v8_checksums)}
foreach ($name in @($v8ArchiveName, $v8BindingName, $v8ChecksumsName)) {{
  $destination = Join-Path $v8Directory $name
  $partial = "$destination.partial-$PID-$([guid]::NewGuid().ToString('N'))"
  try {{
    Invoke-WebRequest -UseBasicParsing -Uri "$v8BaseUrl/$name" -OutFile $partial
    Move-Item -Force -LiteralPath $partial -Destination $destination
  }} finally {{
    if (Test-Path -LiteralPath $partial) {{ Remove-Item -Force -LiteralPath $partial }}
  }}
}}
$checksumLines = @(Get-Content -LiteralPath (Join-Path $v8Directory $v8ChecksumsName) | Where-Object {{ -not [string]::IsNullOrWhiteSpace($_) }})
if ($checksumLines.Count -ne 2) {{ throw 'Expected exactly two rusty_v8 checksums' }}
foreach ($line in $checksumLines) {{
  if ($line -notmatch '^([0-9a-fA-F]{{64}})\\s+\\*?(.+)$') {{ throw "Invalid rusty_v8 checksum line: $line" }}
  $expectedHash = $Matches[1]
  $artifactPath = Join-Path $v8Directory $Matches[2]
  if (-not (Test-Path -LiteralPath $artifactPath)) {{ throw "Missing rusty_v8 checksum target: $artifactPath" }}
  $actualHash = (Get-FileHash -LiteralPath $artifactPath -Algorithm SHA256).Hash
  if ($actualHash -ne $expectedHash) {{ throw "rusty_v8 checksum mismatch: $artifactPath" }}
}}
$env:RUSTY_V8_ARCHIVE = Join-Path $v8Directory $v8ArchiveName
$env:RUSTY_V8_SRC_BINDING_PATH = Join-Path $v8Directory $v8BindingName
$cargo = Join-Path $env:USERPROFILE '.cargo\\bin\\cargo.exe'
$toolchainArgument = '+' + {quote_ps(host.rust_toolchain)}
$env:CODEX_FLEET_VSDEVCMD = Join-Path $vs 'Common7\\Tools\\VsDevCmd.bat'
$env:CODEX_FLEET_BUILD_ROOT = $build
$env:CODEX_FLEET_CARGO = $cargo
$env:CODEX_FLEET_TOOLCHAIN = $toolchainArgument
$command = {quote_ps(WINDOWS_BUILD_COMMAND)}
cmd.exe /d /v:off /s /c $command; if ($LASTEXITCODE -ne 0) {{ throw 'Cargo build failed' }}
Normalize-ReleaseLock $build
Assert-ReleaseWorktree $build {quote_ps(release.fork_commit)} 'Build root changed during build; refusing to write a receipt'
$version = & {quote_ps(host.build_binary("codex"))} --version
if ($version -ne $expectedCliVersion) {{ throw "Unexpected fork version: $version" }}
& {quote_ps(host.build_binary("codex"))} agents --help *> $null
if ($LASTEXITCODE -ne 0) {{ throw 'Built fork does not support codex agents' }}
& {quote_ps(host.build_binary("codex-code-mode-host"))} --help *> $null
if ($LASTEXITCODE -ne 0) {{ throw 'Built code-mode host is not executable' }}
$receipt = {quote_ps(host.build_receipt())}
$receiptPartial = "$receipt.partial-$PID-$([guid]::NewGuid().ToString('N'))"
$receiptLines = @(
  'format=v1',
  'forkCommit={release.fork_commit}',
  ('cliVersion=' + $expectedCliVersion),
  "cliSha256=$((Get-FileHash -LiteralPath {quote_ps(host.build_binary("codex"))} -Algorithm SHA256).Hash.ToLowerInvariant())",
  "hostSha256=$((Get-FileHash -LiteralPath {quote_ps(host.build_binary("codex-code-mode-host"))} -Algorithm SHA256).Hash.ToLowerInvariant())"
)
try {{
  [System.IO.File]::WriteAllLines($receiptPartial, $receiptLines, [System.Text.UTF8Encoding]::new($false))
  Move-Item -Force -LiteralPath $receiptPartial -Destination $receipt
}} finally {{
  if (Test-Path -LiteralPath $receiptPartial) {{ Remove-Item -Force -LiteralPath $receiptPartial }}
}}
Write-Output "build-version=$version"
"""
    assert host.rustup is not None
    return f"""set -eu
release_upstream_version={shlex.quote(release.upstream_version)}
expected_cli_version={shlex.quote(release.cli_version)}
normalize_release_lock() {{
  status=$(git -C "$1" status --short --untracked-files=all)
  [ -z "$status" ] && return
  [ "$status" = ' M codex-rs/Cargo.lock' ] || {{ echo 'Build root contains changes other than the generated release lockfile' >&2; exit 1; }}
  changed_lines=$(git -C "$1" diff --unified=0 -- codex-rs/Cargo.lock | sed -n -e '/^---/d' -e '/^+++/d' -e '/^[+-]/p')
  [ -n "$changed_lines" ] || {{ echo 'Build root Cargo.lock has no recoverable release-version changes' >&2; exit 1; }}
  invalid_lines=$(printf '%s\\n' "$changed_lines" | while IFS= read -r line; do
    if [ "$line" != '-version = "0.0.0"' ] &&
       [ "$line" != '+version = "0.0.0"' ] &&
       [ "$line" != "-version = \\"$release_upstream_version\\"" ] &&
       [ "$line" != "+version = \\"$release_upstream_version\\"" ]; then
      printf '%s\\n' "$line"
    fi
  done)
  [ -z "$invalid_lines" ] || {{ echo 'Build root Cargo.lock contains unexpected changes' >&2; exit 1; }}
  git -C "$1" restore --worktree -- codex-rs/Cargo.lock
}}
assert_release_worktree() {{
  [ -e "$1/.git" ] || {{ echo "$3" >&2; exit 1; }}
  status=$(git -C "$1" status --porcelain) || {{ echo "$3" >&2; exit 1; }}
  commit=$(git -C "$1" rev-parse HEAD) || {{ echo "$3" >&2; exit 1; }}
  [ -z "$status" ] && [ "$commit" = "$2" ] || {{ echo "$3" >&2; exit 1; }}
}}
source={shlex.quote(source)}; build={shlex.quote(build)}
if [ -e "$source" ]; then
  [ -d "$source/.git" ] || exit 1
  source_status=$(git -C "$source" status --porcelain) || exit 1
  [ -z "$source_status" ] || exit 1
else
  git clone --origin fork {shlex.quote(release.fork_remote)} "$source"
fi
{fetch}
git -C "$source" switch -C {shlex.quote(release.fork_ref)} {shlex.quote(staged_ref)}
source_commit=$(git -C "$source" rev-parse HEAD) || exit 1
source_status=$(git -C "$source" status --porcelain) || exit 1
[ "$source_commit" = {release.fork_commit} ]
[ -z "$source_status" ]
if [ -e "$build" ]; then
  normalize_release_lock "$build"
  assert_release_worktree "$build" {release.fork_commit} 'Existing build root is not the clean release worktree'
else
  git -C "$source" worktree add --detach "$build" {release.fork_commit}
fi
cd "$build/codex-rs"
fleet_directory="$build.fleet"
v8_directory="$fleet_directory/rusty-v8"
receipt="$fleet_directory/build-receipt-v1"
v8_base_url={shlex.quote(v8_base_url)}
v8_archive_name={shlex.quote(v8_archive)}
v8_binding_name={shlex.quote(v8_binding)}
v8_checksums_name={shlex.quote(v8_checksums)}
mkdir -p "$v8_directory"
cleanup_v8_downloads() {{ rm -f "$v8_directory"/*.partial.$$ "$receipt.partial.$$"; }}
trap cleanup_v8_downloads EXIT HUP INT TERM
download_v8_artifact() {{
  destination="$v8_directory/$1"
  partial="$destination.partial.$$"
  rm -f "$partial"
  curl -fsSL "$v8_base_url/$1" -o "$partial"
  mv -f "$partial" "$destination"
}}
download_v8_artifact "$v8_archive_name"
download_v8_artifact "$v8_binding_name"
download_v8_artifact "$v8_checksums_name"
[ "$(grep -cve '^[[:space:]]*$' "$v8_directory/$v8_checksums_name")" -eq 2 ]
(cd "$v8_directory" && tr -d '\\r' < "$v8_checksums_name" | shasum -a 256 -c -)
rustc=$({shlex.quote(host.rustup)} which --toolchain {shlex.quote(host.rust_toolchain)} rustc)
toolchain_bin=$(dirname "$rustc")
RUSTY_V8_ARCHIVE="$v8_directory/$v8_archive_name" \\
RUSTY_V8_SRC_BINDING_PATH="$v8_directory/$v8_binding_name" \\
PATH="$toolchain_bin:$PATH" \\
  "$toolchain_bin/cargo" build --release --bin codex --bin codex-code-mode-host
normalize_release_lock "$build"
assert_release_worktree "$build" {release.fork_commit} 'Build root changed during build; refusing to write a receipt'
version=$(target/release/codex --version)
target/release/codex agents --help >/dev/null
target/release/codex-code-mode-host --help >/dev/null
[ "$version" = "$expected_cli_version" ] || {{ echo "Unexpected fork version: $version" >&2; exit 1; }}
cli_hash=$(shasum -a 256 target/release/codex | awk '{{print $1}}')
host_hash=$(shasum -a 256 target/release/codex-code-mode-host | awk '{{print $1}}')
printf '%s\\n' \
  'format=v1' \
  'forkCommit={release.fork_commit}' \
  "cliVersion=$expected_cli_version" \
  "cliSha256=$cli_hash" \
  "hostSha256=$host_hash" > "$receipt.partial.$$"
mv -f "$receipt.partial.$$" "$receipt"
printf 'build-version=%s\\n' "$version"
"""


POSIX_ACTIVATION_SUPERVISOR = r'''import errno
import fcntl
import hashlib
import json
import os
import re
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import time
from contextlib import ExitStack
from pathlib import Path


LOCK_TIMEOUT_SECONDS = 75
LOCK_RETRY_SECONDS = 0.05
HEALTH_PROBE_TIMEOUT_SECONDS = 30
PROCESS_TERM_GRACE_SECONDS = 2
PROCESS_KILL_GRACE_SECONDS = 2

COMMAND_WRAPPER_SOURCE = r"""import json
import os
import select
import signal
import subprocess
import sys
import threading
import time

LIVENESS_FD = int(sys.argv[1])
LEASE_FDS = tuple(json.loads(sys.argv[2]))
COMMAND = json.loads(sys.argv[3])
TERM_GRACE_SECONDS = 0.5
KILL_GRACE_SECONDS = 0.5
POLL_SECONDS = 0.02
child = None
pending_signal = None

HANDLED_SIGNALS = (signal.SIGHUP, signal.SIGINT, signal.SIGTERM)

for lock_fd in LEASE_FDS:
    if lock_fd >= 0:
        os.set_inheritable(lock_fd, False)


def group_exists(process_group):
    try:
        os.killpg(process_group, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


def wait_for_group(process, timeout):
    deadline = time.monotonic() + timeout
    while group_exists(process.pid) and time.monotonic() < deadline:
        try:
            process.wait(timeout=min(POLL_SECONDS, max(0, deadline - time.monotonic())))
        except subprocess.TimeoutExpired:
            pass
        if group_exists(process.pid):
            time.sleep(POLL_SECONDS)
    return not group_exists(process.pid)


def stop_group(process, initial_signal=signal.SIGTERM):
    try:
        os.killpg(process.pid, initial_signal)
    except ProcessLookupError:
        pass
    if not wait_for_group(process, TERM_GRACE_SECONDS):
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        wait_for_group(process, KILL_GRACE_SECONDS)
    try:
        process.wait(timeout=0)
    except (subprocess.TimeoutExpired, ChildProcessError):
        pass


def interrupted(signum, _frame):
    global pending_signal
    if child is None:
        pending_signal = signum
        return
    for handled_signal in HANDLED_SIGNALS:
        signal.signal(handled_signal, signal.SIG_IGN)
    if child is not None:
        stop_group(child, signum)
    raise SystemExit(128 + signum)


for handled_signal in HANDLED_SIGNALS:
    signal.signal(handled_signal, interrupted)
if hasattr(signal, "pthread_sigmask"):
    signal.pthread_sigmask(signal.SIG_UNBLOCK, HANDLED_SIGNALS)

if select.select([LIVENESS_FD], [], [], 0)[0]:
    raise SystemExit(125)

child = subprocess.Popen(
    COMMAND,
    stdin=subprocess.DEVNULL,
    close_fds=True,
    pass_fds=LEASE_FDS,
    start_new_session=True,
)
if pending_signal is not None:
    interrupted(pending_signal, None)


def watch_supervisor():
    try:
        os.read(LIVENESS_FD, 1)
    finally:
        stop_group(child)
        os._exit(125)


threading.Thread(target=watch_supervisor, daemon=True).start()
try:
    returncode = child.wait()
    if group_exists(child.pid):
        stop_group(child)
except BaseException:
    stop_group(child)
    raise
finally:
    os.close(LIVENESS_FD)
raise SystemExit(returncode)
"""


class ActivationFailure(RuntimeError):
    pass


class ActivationInterrupted(RuntimeError):
    def __init__(self, signum):
        super().__init__(f"activation interrupted by signal {signum}")
        self.signum = signum


active_process = None
lease_lock_fds = ()


def process_group_exists(process_group):
    try:
        os.killpg(process_group, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


def wait_for_process_group_exit(process, timeout):
    deadline = time.monotonic() + timeout
    while process_group_exists(process.pid) and time.monotonic() < deadline:
        try:
            process.wait(
                timeout=min(
                    LOCK_RETRY_SECONDS,
                    max(0, deadline - time.monotonic()),
                )
            )
        except subprocess.TimeoutExpired:
            pass
        if process_group_exists(process.pid):
            time.sleep(LOCK_RETRY_SECONDS)
    return not process_group_exists(process.pid)


def stop_process_group(process, initial_signal=signal.SIGTERM):
    try:
        os.killpg(process.pid, initial_signal)
    except ProcessLookupError:
        pass
    if not wait_for_process_group_exit(process, PROCESS_TERM_GRACE_SECONDS):
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        if not wait_for_process_group_exit(process, PROCESS_KILL_GRACE_SECONDS):
            raise ActivationFailure(
                f"activation process group {process.pid} did not stop after SIGKILL"
            )
    try:
        process.wait(timeout=0)
    except (subprocess.TimeoutExpired, ChildProcessError):
        pass


def interrupt_activation(signum, _frame):
    for handled_signal in (signal.SIGHUP, signal.SIGINT, signal.SIGTERM):
        signal.signal(handled_signal, signal.SIG_IGN)
    if active_process is not None:
        try:
            os.killpg(active_process.pid, signum)
        except ProcessLookupError:
            pass
    raise ActivationInterrupted(signum)


def run_process(arguments, *, codex_home=None, timeout_seconds=None):
    global active_process
    environment = os.environ.copy()
    if codex_home is not None:
        environment["CODEX_HOME"] = str(codex_home)
    command = [str(argument) for argument in arguments]
    handled_signals = {signal.SIGHUP, signal.SIGINT, signal.SIGTERM}
    previous_signal_mask = None
    if hasattr(signal, "pthread_sigmask"):
        previous_signal_mask = signal.pthread_sigmask(
            signal.SIG_BLOCK,
            handled_signals,
        )
    liveness_read = None
    liveness_write = None
    wrapper_lock_fds = []
    process = None
    try:
        try:
            liveness_read, liveness_write = os.pipe()
            os.set_inheritable(liveness_read, False)
            os.set_inheritable(liveness_write, False)
            for lock_fd in lease_lock_fds:
                wrapper_lock_fd = os.dup(lock_fd)
                os.set_inheritable(wrapper_lock_fd, False)
                wrapper_lock_fds.append(wrapper_lock_fd)
            inherited_fds = (liveness_read, *wrapper_lock_fds)
            process = subprocess.Popen(
                [
                    sys.executable,
                    "-c",
                    COMMAND_WRAPPER_SOURCE,
                    str(liveness_read),
                    json.dumps(wrapper_lock_fds),
                    json.dumps(command),
                ],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                env=environment,
                close_fds=True,
                pass_fds=inherited_fds,
                start_new_session=True,
            )
            active_process = process
            os.close(liveness_read)
            liveness_read = None
            for wrapper_lock_fd in wrapper_lock_fds:
                os.close(wrapper_lock_fd)
            wrapper_lock_fds.clear()
        finally:
            if previous_signal_mask is not None:
                signal.pthread_sigmask(signal.SIG_SETMASK, previous_signal_mask)
    except BaseException:
        if process is not None:
            stop_process_group(process)
            active_process = None
        if liveness_read is not None:
            os.close(liveness_read)
        if liveness_write is not None:
            os.close(liveness_write)
        for wrapper_lock_fd in wrapper_lock_fds:
            os.close(wrapper_lock_fd)
        raise
    deadline = (
        None if timeout_seconds is None else time.monotonic() + timeout_seconds
    )
    try:
        timeout = None if deadline is None else max(0, deadline - time.monotonic())
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired as error:
        stop_process_group(process)
        raise ActivationFailure(
            f"Installer health probe timed out: {' '.join(command)}"
        ) from error
    except BaseException:
        stop_process_group(process)
        raise
    finally:
        active_process = None
        os.close(liveness_write)
    return subprocess.CompletedProcess(arguments, process.returncode, stdout, stderr)


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def unlink_if_present(path):
    if path is None:
        return
    try:
        path.unlink()
    except FileNotFoundError:
        pass


def fsync_directory(path):
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def durable_unlink(path):
    try:
        path.unlink()
    except FileNotFoundError:
        return
    fsync_directory(path.parent)


def write_transaction(path, transaction):
    descriptor, temporary_name = tempfile.mkstemp(
        dir=path.parent,
        prefix=".fleet-journal.",
    )
    temporary = Path(temporary_name)
    try:
        output = os.fdopen(descriptor, "w", encoding="utf-8")
        descriptor = None
        with output:
            json.dump(transaction, output, sort_keys=True)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
        fsync_directory(path.parent)
    finally:
        if descriptor is not None:
            os.close(descriptor)
        unlink_if_present(temporary)


def set_transaction_phase(path, transaction, phase):
    transaction["phase"] = phase
    write_transaction(path, transaction)


def durable_copy(source, destination):
    descriptor, temporary_name = tempfile.mkstemp(
        dir=destination.parent,
        prefix=f"{destination.name}.preparing.",
    )
    os.close(descriptor)
    temporary = Path(temporary_name)
    try:
        shutil.copy2(source, temporary)
        with temporary.open("rb") as copied:
            os.fsync(copied.fileno())
        os.replace(temporary, destination)
        fsync_directory(destination.parent)
    finally:
        unlink_if_present(temporary)


def require_clean_git(path, expected_commit, label):
    commit = run_process(
        ["git", "-C", path, "rev-parse", "HEAD"],
        timeout_seconds=HEALTH_PROBE_TIMEOUT_SECONDS,
    )
    status = run_process(
        ["git", "-C", path, "status", "--porcelain"],
        timeout_seconds=HEALTH_PROBE_TIMEOUT_SECONDS,
    )
    if commit.returncode != 0 or status.returncode != 0:
        raise ActivationFailure(f"Could not inspect {label}")
    if commit.stdout.strip() != expected_commit or status.stdout.strip():
        raise ActivationFailure(f"{label} is not clean at the manifest commit")


def read_receipt(path):
    values = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        if "=" not in line:
            continue
        key, value = line.split("=", 1)
        if key in values:
            raise ActivationFailure("Invalid build receipt")
        values[key] = value
    return values


def daemon_socket_is_confirmed_absent(path):
    try:
        mode = path.lstat().st_mode
    except FileNotFoundError:
        return True
    if not stat.S_ISSOCK(mode):
        return False
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.settimeout(0.25)
    try:
        client.connect(str(path))
    except OSError as error:
        return error.errno in (errno.ENOENT, errno.ECONNREFUSED)
    finally:
        client.close()
    return False


def ensure_daemon_runtime_compatible(codex, expected_version, codex_home, *, strict):
    result = run_process(
        [codex, "app-server", "daemon", "version"],
        codex_home=codex_home,
        timeout_seconds=HEALTH_PROBE_TIMEOUT_SECONDS,
    )
    if result.returncode != 0:
        socket_path = codex_home / "app-server-control" / "app-server-control.sock"
        if not strict and daemon_socket_is_confirmed_absent(socket_path):
            return
        if strict:
            raise ActivationFailure(
                "Strict app-server daemon verification failed while the operation lease was held; refusing activation."
            )
        raise ActivationFailure(
            f"Could not verify the app-server daemon at {socket_path}; refusing to replace Codex."
        )
    try:
        daemon = json.loads(result.stdout)
    except (TypeError, json.JSONDecodeError) as error:
        message = (
            "Strict app-server daemon verification returned invalid status; refusing activation."
            if strict
            else "Could not parse app-server daemon status; refusing to replace Codex."
        )
        raise ActivationFailure(message) from error
    if not isinstance(daemon, dict):
        message = (
            "Strict app-server daemon verification returned invalid status; refusing activation."
            if strict
            else "Could not parse app-server daemon status; refusing to replace Codex."
        )
        raise ActivationFailure(message)
    daemon_status = daemon.get("status")
    daemon_version = daemon.get("appServerVersion")
    if not isinstance(daemon_status, str) or (
        daemon_version is not None and not isinstance(daemon_version, str)
    ):
        message = (
            "Strict app-server daemon verification returned invalid status; refusing activation."
            if strict
            else "Could not parse app-server daemon status; refusing to replace Codex."
        )
        raise ActivationFailure(message)
    if daemon_status == "notRunning":
        return
    if daemon_status != "running":
        raise ActivationFailure(
            f"Unexpected app-server daemon status '{daemon_status}'; refusing activation."
        )
    if daemon_version != expected_version:
        raise ActivationFailure(
            f"App-server daemon version {daemon_version or 'unknown'} is running; "
            f"the new Codex runtime is {expected_version}."
        )


def disable_daemon_auto_update(codex, codex_home):
    result = run_process(
        [codex, "app-server", "daemon", "disable-auto-update"],
        codex_home=codex_home,
    )
    if result.returncode != 0:
        raise ActivationFailure(
            "Could not disable the official Codex daemon updater; refusing activation."
        )


def copy_to_stage(source, target, label, expected_hash=None):
    descriptor, stage_name = tempfile.mkstemp(
        dir=target.parent,
        prefix=".fleet-stage.",
    )
    os.close(descriptor)
    stage = Path(stage_name)
    try:
        shutil.copy2(source, stage)
        with stage.open("rb") as copied:
            os.fsync(copied.fileno())
        copied_hash = sha256(stage)
        if expected_hash is None:
            expected_hash = sha256(source)
        if copied_hash != expected_hash:
            raise ActivationFailure(f"{label} staging verification failed")
        return stage
    except BaseException:
        unlink_if_present(stage)
        raise


def stage_private_reviewed_cli(source, target, expected_hash):
    directory = Path(
        tempfile.mkdtemp(
            dir=target.parent,
            prefix=f".{target.name}.fleet-reviewed.{CONFIG['release_name']}.",
        )
    )
    directory.chmod(0o700)
    candidate = directory / source.name
    ownership = None
    try:
        ownership = (directory / "owner.lock").open("a+b", buffering=0)
        os.set_inheritable(ownership.fileno(), False)
        fcntl.flock(ownership, fcntl.LOCK_EX | fcntl.LOCK_NB)
        shutil.copy2(source, candidate)
        with candidate.open("rb") as copied:
            os.fsync(copied.fileno())
        if sha256(candidate) != expected_hash:
            raise ActivationFailure("Reviewed CLI changed while it was staged")
        return directory, candidate, ownership
    except BaseException:
        if ownership is not None:
            ownership.close()
        shutil.rmtree(directory, ignore_errors=True)
        raise


def restore_pair(
    target_cli,
    target_host,
    backup_cli,
    backup_host,
    old_hashes,
    transaction_path,
    transaction,
):
    restore_cli = None
    restore_host = None
    try:
        restore_cli = copy_to_stage(
            backup_cli,
            target_cli,
            "CLI rollback",
            old_hashes[0],
        )
        restore_host = copy_to_stage(
            backup_host,
            target_host,
            "code-mode host rollback",
            old_hashes[1],
        )
        os.replace(restore_host, target_host)
        restore_host = None
        fsync_directory(target_host.parent)
        set_transaction_phase(transaction_path, transaction, "hostRestored")
        os.replace(restore_cli, target_cli)
        restore_cli = None
        fsync_directory(target_cli.parent)
        set_transaction_phase(transaction_path, transaction, "pairRestored")
        if (sha256(target_cli), sha256(target_host)) != old_hashes:
            raise ActivationFailure("rollback hash verification failed")
        set_transaction_phase(transaction_path, transaction, "cleanup")
        durable_unlink(backup_cli)
        durable_unlink(backup_host)
        durable_unlink(transaction_path)
    finally:
        unlink_if_present(restore_cli)
        unlink_if_present(restore_host)


def target_slot_identity(path):
    parent = path.parent.resolve(strict=True)
    return f"{parent}\0{path.name}"


def target_pair_token(target_cli, target_host):
    identity = "\0".join(
        (target_slot_identity(target_cli), target_slot_identity(target_host))
    )
    return hashlib.sha256(identity.encode("utf-8")).hexdigest()


def rollback_paths(target_cli, target_host, release_name, *, namespace="target-pair"):
    if namespace == "legacy":
        return (
            Path(f"{target_cli}.{release_name}-previous"),
            Path(f"{target_host}.{release_name}-previous"),
        )
    if namespace == "release-only":
        token_source = release_name
    elif namespace == "target-pair":
        token_source = f"{target_pair_token(target_cli, target_host)}\0{release_name}"
    else:
        raise ActivationFailure(f"Unknown rollback namespace: {namespace}")
    token = hashlib.sha256(token_source.encode("utf-8")).hexdigest()
    return (
        target_cli.parent / f".fleet-rollback.{token}.cli",
        target_host.parent / f".fleet-rollback.{token}.host",
    )


def transaction_path_for_targets(target_cli, target_host, *, namespace="target-pair"):
    if namespace == "release-only":
        return Path(f"{target_cli}..fleet-transaction.json")
    if namespace != "target-pair":
        raise ActivationFailure(f"Unknown transaction namespace: {namespace}")
    return target_cli.parent / f".fleet-transaction.{target_pair_token(target_cli, target_host)}.json"


def expected_transaction(
    target_cli,
    target_host,
    release_name,
    *,
    namespace="target-pair",
):
    backup_cli, backup_host = rollback_paths(
        target_cli,
        target_host,
        release_name,
        namespace=namespace,
    )
    return {
        "format": 1,
        "release": release_name,
        "targetCli": str(target_cli),
        "targetHost": str(target_host),
        "backupCli": str(backup_cli),
        "backupHost": str(backup_host),
    }


def physical_path(path):
    return Path(path).resolve(strict=False)


def same_physical_path(left, right):
    return physical_path(left) == physical_path(right)


def cleanup_preparing_copies(*destinations):
    for destination in destinations:
        prefix = f"{destination.name}.preparing."
        for candidate in destination.parent.iterdir():
            if candidate.name.startswith(prefix) and (
                candidate.is_symlink() or not candidate.is_dir()
            ):
                candidate.unlink()


def cleanup_stale_transaction_stages(
    target_cli,
    target_host,
    backup_cli,
    backup_host,
    transaction_path,
):
    directories = {
        target_cli.parent,
        target_host.parent,
        backup_cli.parent,
        backup_host.parent,
        transaction_path.parent,
    }
    for directory in directories:
        for candidate in directory.iterdir():
            if candidate.name.startswith((".fleet-stage.", ".fleet-journal.")):
                candidate.unlink()
    cleanup_preparing_copies(backup_cli, backup_host)


def cleanup_stale_reviewed_stages(target_cli, preserve_directory):
    prefix = f".{target_cli.name}.fleet-reviewed."
    for directory in target_cli.parent.iterdir():
        if not directory.name.startswith(prefix):
            continue
        if directory == preserve_directory:
            continue
        if directory.is_symlink() or not directory.is_dir():
            directory.unlink()
            continue
        ownership = None
        try:
            ownership = (directory / "owner.lock").open("a+b", buffering=0)
            os.set_inheritable(ownership.fileno(), False)
            try:
                fcntl.flock(ownership, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                continue
            shutil.rmtree(directory)
        finally:
            if ownership is not None:
                ownership.close()


def pending_transaction(target_cli, target_host, current_transaction_path):
    legacy_prefix = f"{target_cli.name}."
    legacy_suffix = "-transaction.json"
    release_only_path = transaction_path_for_targets(
        target_cli,
        target_host,
        namespace="release-only",
    )
    journals = []
    for candidate in target_cli.parent.iterdir():
        namespace = None
        if candidate == current_transaction_path:
            namespace = "target-pair"
        elif candidate == release_only_path:
            namespace = "release-only"
        elif candidate.name.startswith(legacy_prefix) and candidate.name.endswith(
            legacy_suffix
        ):
            namespace = "legacy"
        elif candidate.name.startswith(".fleet-transaction.") and candidate.name.endswith(
            ".json"
        ):
            # A target-pair journal created through another alias has a different
            # lexical name. Only adopt it when its stored targets authenticate to
            # this physical pair; unrelated journals in the shared directory are
            # ignored.
            try:
                transaction = json.loads(candidate.read_text(encoding="utf-8"))
                stored_cli = Path(transaction["targetCli"])
                stored_host = Path(transaction["targetHost"])
            except (OSError, json.JSONDecodeError, KeyError, TypeError):
                continue
            if same_physical_path(stored_cli, target_cli) and same_physical_path(
                stored_host, target_host
            ):
                namespace = "target-pair"
        if namespace is not None and candidate.is_file():
            journals.append((candidate, namespace))
    if len(journals) > 1:
        raise ActivationFailure(
            "Multiple activation transaction journals require manual recovery: "
            + ", ".join(str(path) for path, _namespace in sorted(journals))
        )
    if not journals:
        return None, None
    return journals[0]


def recover_transaction(
    target_cli,
    target_host,
    transaction_path,
    *,
    namespace="target-pair",
):
    if not transaction_path.exists():
        return
    try:
        transaction = json.loads(transaction_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ActivationFailure(
            f"Invalid activation transaction state: {transaction_path}"
        ) from error
    transaction_release = transaction.get("release") if isinstance(transaction, dict) else None
    if (
        not isinstance(transaction, dict)
        or not isinstance(transaction_release, str)
        or not transaction_release
    ):
        raise ActivationFailure(
            f"Activation transaction does not match this target pair: {transaction_path}"
        )
    stored_path_keys = ("targetCli", "targetHost", "backupCli", "backupHost")
    if any(not isinstance(transaction.get(key), str) for key in stored_path_keys):
        raise ActivationFailure(
            f"Activation transaction does not match this target pair: {transaction_path}"
        )
    stored_target_cli = Path(transaction["targetCli"])
    stored_target_host = Path(transaction["targetHost"])
    stored_identity = expected_transaction(
        stored_target_cli,
        stored_target_host,
        transaction_release,
        namespace=namespace,
    )
    current_identity = expected_transaction(
        target_cli,
        target_host,
        transaction_release,
        namespace=namespace,
    )
    if not same_physical_path(stored_target_cli, target_cli) or not same_physical_path(
        stored_target_host, target_host
    ):
        raise ActivationFailure(
            f"Activation transaction does not match this target pair: {transaction_path}"
        )
    if namespace == "legacy" and not same_physical_path(
        transaction_path,
        Path(f"{stored_target_cli}.{transaction_release}-transaction.json"),
    ):
        raise ActivationFailure(
            f"Activation transaction does not match this target pair: {transaction_path}"
        )
    if namespace in {"target-pair", "release-only"} and not same_physical_path(
        transaction_path,
        transaction_path_for_targets(
            stored_target_cli,
            stored_target_host,
            namespace=namespace,
        ),
    ):
        raise ActivationFailure(
            f"Activation transaction does not match this target pair: {transaction_path}"
        )
    if any(
        not same_physical_path(transaction[key], stored_identity[key])
        or not same_physical_path(transaction[key], current_identity[key])
        for key in ("backupCli", "backupHost")
    ) or any(
        transaction.get(key) != stored_identity[key] for key in ("format", "release")
    ):
        raise ActivationFailure(
            f"Activation transaction does not match this target pair: {transaction_path}"
        )
    backup_cli = Path(transaction["backupCli"])
    backup_host = Path(transaction["backupHost"])
    # These exact destinations came from the pending journal and have now been
    # authenticated against the configured physical target pair. This also
    # cleans interrupted rollback copies from an older release when the restart
    # requests a different release.
    cleanup_preparing_copies(backup_cli, backup_host)
    phase = transaction.get("phase")
    old_hashes = (transaction.get("oldCliHash"), transaction.get("oldHostHash"))
    if phase not in {
        "preparing",
        "prepared",
        "cliReplaced",
        "pairReplaced",
        "hostRestored",
        "pairRestored",
        "cleanup",
        "committed",
    } or any(
        not isinstance(value, str) or len(value) != 64 for value in old_hashes
    ):
        raise ActivationFailure(
            f"Invalid activation transaction state: {transaction_path}"
        )
    if not target_cli.is_file() or not target_host.is_file():
        raise ActivationFailure(
            f"Activation transaction targets are incomplete: {transaction_path}"
        )
    if phase == "committed":
        new_hashes = (transaction.get("newCliHash"), transaction.get("newHostHash"))
        if any(not isinstance(value, str) or len(value) != 64 for value in new_hashes):
            raise ActivationFailure(
                f"Invalid activation transaction state: {transaction_path}"
            )
        if (sha256(target_cli), sha256(target_host)) != new_hashes:
            raise ActivationFailure(
                f"Committed activation has an unexpected target pair: {transaction_path}"
            )
        unlink_if_present(transaction_path)
        return
    if phase == "preparing":
        if (sha256(target_cli), sha256(target_host)) != old_hashes:
            raise ActivationFailure(
                f"Preparing activation changed its target pair: {transaction_path}"
            )
        durable_unlink(backup_cli)
        durable_unlink(backup_host)
        durable_unlink(transaction_path)
        return
    if phase in {"pairRestored", "cleanup"}:
        if (sha256(target_cli), sha256(target_host)) != old_hashes:
            raise ActivationFailure(
                f"Restored activation has an unexpected target pair: {transaction_path}"
            )
        if phase != "cleanup":
            set_transaction_phase(transaction_path, transaction, "cleanup")
        durable_unlink(backup_cli)
        durable_unlink(backup_host)
        durable_unlink(transaction_path)
        return
    if not backup_cli.is_file() or not backup_host.is_file():
        raise ActivationFailure(
            f"Activation recovery pair is incomplete: {backup_cli}, {backup_host}"
        )
    if (sha256(backup_cli), sha256(backup_host)) != old_hashes:
        raise ActivationFailure(
            f"Activation recovery pair has unexpected contents: {backup_cli}, {backup_host}"
        )
    restore_pair(
        target_cli,
        target_host,
        backup_cli,
        backup_host,
        old_hashes,
        transaction_path,
        transaction,
    )


def migrate_rollback_state(
    target_cli,
    target_host,
    release_name,
    source_hashes,
):
    target_hashes = (sha256(target_cli), sha256(target_host))
    current_pair = rollback_paths(target_cli, target_host, release_name)
    release_only_pair = rollback_paths(
        target_cli,
        target_host,
        release_name,
        namespace="release-only",
    )
    legacy_pair = rollback_paths(
        target_cli,
        target_host,
        release_name,
        namespace="legacy",
    )
    migration_pairs = [release_only_pair]
    if same_physical_path(legacy_pair[0].parent, target_cli.parent) and same_physical_path(
        legacy_pair[1].parent, target_host.parent
    ):
        migration_pairs.append(legacy_pair)

    def presence(pair):
        return tuple(path.is_file() for path in pair)

    current_presence = presence(current_pair)
    if current_presence[0] != current_presence[1]:
        matching_source = None
        for pair in migration_pairs:
            if presence(pair) != (True, True):
                continue
            if all(
                not is_present or sha256(destination) == sha256(source)
                for source, destination, is_present in zip(
                    pair,
                    current_pair,
                    current_presence,
                    strict=True,
                )
            ):
                matching_source = pair
                break
        if matching_source is None:
            raise ActivationFailure(
                f"Release rollback pair is incomplete: "
                f"{current_pair[0]}, {current_pair[1]}"
            )
        for source, destination, is_present in zip(
            matching_source,
            current_pair,
            current_presence,
            strict=True,
        ):
            if not is_present:
                durable_copy(source, destination)
        current_presence = presence(current_pair)

    partial_migration_pairs = []
    for pair in migration_pairs:
        pair_presence = presence(pair)
        if pair_presence[0] == pair_presence[1]:
            continue
        if current_presence != (True, True) or any(
            is_present and sha256(source) != sha256(destination)
            for source, destination, is_present in zip(
                pair,
                current_pair,
                pair_presence,
                strict=True,
            )
        ):
            raise ActivationFailure(
                f"Release rollback pair is incomplete: {pair[0]}, {pair[1]}"
            )
        partial_migration_pairs.append((pair, pair_presence))

    pair_hashes = []
    for pair in (current_pair, *migration_pairs):
        if presence(pair) == (True, True):
            pair_hashes.append((pair, tuple(sha256(path) for path in pair)))

    if target_hashes != source_hashes:
        for pair, hashes in pair_hashes:
            if hashes != target_hashes:
                raise ActivationFailure(
                    f"Release rollback pair does not match the active binaries: "
                    f"{pair[0]}, {pair[1]}"
                )
    elif pair_hashes and any(hashes != pair_hashes[0][1] for _pair, hashes in pair_hashes):
        raise ActivationFailure("Release rollback namespaces disagree")

    # A crash after either legacy file was removed leaves an authenticated
    # partial source and a complete destination. Finish that cleanup only after
    # the destination has passed the same semantic validation as a normal pair.
    for migration_pair, pair_presence in partial_migration_pairs:
        for source, is_present in zip(migration_pair, pair_presence, strict=True):
            if is_present:
                durable_unlink(source)

    for migration_pair in migration_pairs:
        if presence(migration_pair) != (True, True):
            continue
        migration_hashes = tuple(sha256(path) for path in migration_pair)
        for source, destination, expected_hash in zip(
            migration_pair,
            current_pair,
            migration_hashes,
            strict=True,
        ):
            if destination.is_file():
                if sha256(destination) != expected_hash:
                    raise ActivationFailure(
                        "Release rollback migration destination has unexpected contents: "
                        f"{destination}"
                    )
            else:
                durable_copy(source, destination)
        if tuple(sha256(path) for path in current_pair) != migration_hashes:
            raise ActivationFailure("Release rollback migration verification failed")
        durable_unlink(migration_pair[0])
        durable_unlink(migration_pair[1])
    return current_pair


def acquire_operation_lock(lock_path, description):
    lock_file = lock_path.open("a+b", buffering=0)
    os.set_inheritable(lock_file.fileno(), False)
    deadline = time.monotonic() + LOCK_TIMEOUT_SECONDS
    try:
        while True:
            try:
                fcntl.flock(lock_file, fcntl.LOCK_EX | fcntl.LOCK_NB)
                return lock_file
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    raise ActivationFailure(
                        f"timed out waiting for {description} lock {lock_path}"
                    )
                time.sleep(LOCK_RETRY_SECONDS)
    except BaseException:
        lock_file.close()
        raise


def validate_release_inputs():
    canonical = Path(CONFIG["canonical"])
    build = Path(CONFIG["build"])
    server = Path(CONFIG["server"])
    source_cli = Path(CONFIG["source_cli"])
    source_host = Path(CONFIG["source_host"])
    receipt = Path(CONFIG["receipt"])
    require_clean_git(canonical, CONFIG["fork_commit"], "Canonical source")
    require_clean_git(build, CONFIG["fork_commit"], "Build worktree")
    require_clean_git(server, CONFIG["server_commit"], "MCP server")
    if not source_cli.is_file() or not source_host.is_file():
        raise ActivationFailure("Activation binary pair is incomplete")
    if not os.access(source_cli, os.X_OK) or not os.access(source_host, os.X_OK):
        raise ActivationFailure("Activation binary pair is incomplete")
    if not receipt.is_file():
        raise ActivationFailure("Missing reviewed build receipt")
    source_hashes = (sha256(source_cli), sha256(source_host))
    expected_receipt = {
        "format": "v1",
        "forkCommit": CONFIG["fork_commit"],
        "cliVersion": CONFIG["cli_version"],
        "cliSha256": source_hashes[0],
        "hostSha256": source_hashes[1],
    }
    if read_receipt(receipt) != expected_receipt:
        raise ActivationFailure("Build receipt does not match the reviewed binary pair")
    return source_hashes


def activate_under_lock(
    codex_home,
    candidate_cli,
    candidate_host,
    source_hashes,
    backup_cli,
    backup_host,
    transaction_path,
):
    target_cli = Path(CONFIG["target_cli"])
    target_host = Path(CONFIG["target_host"])
    target_hashes = (sha256(target_cli), sha256(target_host))
    source_modes = (
        stat.S_IMODE(candidate_cli.stat().st_mode),
        stat.S_IMODE(candidate_host.stat().st_mode),
    )
    old_modes = (
        stat.S_IMODE(target_cli.stat().st_mode),
        stat.S_IMODE(target_host.stat().st_mode),
    )
    old_hashes = target_hashes
    transaction = None
    repaired_modes = False
    failure = None
    rollback_failure = None

    try:
        ensure_daemon_runtime_compatible(
            candidate_cli,
            CONFIG["runtime_version"],
            codex_home,
            strict=True,
        )
        if target_hashes == source_hashes and (
            not os.access(target_cli, os.X_OK) or not os.access(target_host, os.X_OK)
        ):
            repaired_modes = True
            target_cli.chmod(source_modes[0])
            target_host.chmod(source_modes[1])
        if target_hashes == source_hashes and backup_cli.exists() != backup_host.exists():
            raise ActivationFailure("Release rollback pair is incomplete")
        if target_hashes != source_hashes:
            transaction = expected_transaction(
                target_cli,
                target_host,
                CONFIG["release_name"],
            )
            transaction.update(
                {
                    "release": CONFIG["release_name"],
                    "phase": "preparing",
                    "oldCliHash": old_hashes[0],
                    "oldHostHash": old_hashes[1],
                    "newCliHash": source_hashes[0],
                    "newHostHash": source_hashes[1],
                }
            )
            write_transaction(transaction_path, transaction)
            try:
                durable_copy(target_cli, backup_cli)
                durable_copy(target_host, backup_host)
            except BaseException as error:
                raise ActivationFailure(
                    "Failed to create the release rollback pair"
                ) from error
            if (sha256(backup_cli), sha256(backup_host)) != old_hashes:
                raise ActivationFailure("Release rollback pair hash verification failed")
            set_transaction_phase(transaction_path, transaction, "prepared")
            os.replace(candidate_cli, target_cli)
            candidate_cli = None
            fsync_directory(target_cli.parent)
            set_transaction_phase(transaction_path, transaction, "cliReplaced")
            os.replace(candidate_host, target_host)
            candidate_host = None
            fsync_directory(target_host.parent)
            set_transaction_phase(transaction_path, transaction, "pairReplaced")
        if (sha256(target_cli), sha256(target_host)) != source_hashes:
            raise ActivationFailure("Activated binary hash verification failed")
        version = run_process(
            [target_cli, "--version"],
            timeout_seconds=HEALTH_PROBE_TIMEOUT_SECONDS,
        )
        if version.returncode != 0 or version.stdout.strip() != CONFIG["cli_version"]:
            raise ActivationFailure(
                f"Unexpected fork version: {version.stdout.strip() or 'missing'}"
            )
        agents = run_process(
            [target_cli, "agents", "--help"],
            timeout_seconds=HEALTH_PROBE_TIMEOUT_SECONDS,
        )
        if agents.returncode != 0:
            raise ActivationFailure("Activated fork does not support codex agents")
        code_mode_host = run_process(
            [target_host, "--help"],
            timeout_seconds=HEALTH_PROBE_TIMEOUT_SECONDS,
        )
        if code_mode_host.returncode != 0:
            raise ActivationFailure("Activated code-mode host is not executable")
        ensure_daemon_runtime_compatible(
            target_cli,
            CONFIG["runtime_version"],
            codex_home,
            strict=True,
        )
        if transaction is not None:
            # The committed marker is durable before journal removal. Journal
            # unlinking is deliberately not followed by a parent fsync: if its
            # directory entry reappears after a crash, recovery sees the durable
            # committed phase and only finishes cleanup. Once this point is
            # reached, cleanup cannot turn a successful activation into a
            # reported rollback failure.
            set_transaction_phase(transaction_path, transaction, "committed")
            try:
                unlink_if_present(transaction_path)
            except OSError as error:
                print(
                    f"Committed activation journal remains for recovery: {error}",
                    file=sys.stderr,
                )
    except BaseException as error:
        failure = error
        for handled_signal in (signal.SIGHUP, signal.SIGINT, signal.SIGTERM):
            signal.signal(handled_signal, signal.SIG_IGN)
        if transaction_path.exists():
            try:
                recover_transaction(target_cli, target_host, transaction_path)
            except BaseException as error:
                rollback_failure = error
        if repaired_modes:
            try:
                target_cli.chmod(old_modes[0])
                target_host.chmod(old_modes[1])
            except OSError as error:
                rollback_failure = rollback_failure or error
    if failure is not None:
        if rollback_failure is not None:
            raise ActivationFailure(
                f"Activation failed: {failure}; rollback failed: {rollback_failure}; "
                f"recovery copies: {backup_cli}, {backup_host}"
            ) from failure
        raise failure
    unlink_if_present(candidate_host)
    print(f"activated-version={version.stdout.strip()}")


def validate_and_activate():
    global lease_lock_fds
    source_cli = Path(CONFIG["source_cli"])
    source_host = Path(CONFIG["source_host"])
    target_cli = Path(CONFIG["target_cli"])
    target_host = Path(CONFIG["target_host"])
    target_lock_paths = tuple(
        sorted(
            {Path(path).resolve(strict=False) for path in CONFIG["target_locks"]},
            key=lambda path: os.fsencode(path),
        )
    )
    configured_codex_home = os.environ.get("CODEX_HOME")
    codex_home = (
        Path(configured_codex_home)
        if configured_codex_home and configured_codex_home.strip()
        else Path.home() / ".codex"
    )

    lock_directory = codex_home / "app-server-daemon"
    previous_umask = os.umask(0o077)
    try:
        lock_directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    finally:
        os.umask(previous_umask)
    lock_path = lock_directory / "daemon.lock"
    reviewed_source_hashes = validate_release_inputs()
    reviewed_directory, reviewed_cli, reviewed_ownership = stage_private_reviewed_cli(
        source_cli,
        target_cli,
        reviewed_source_hashes[0],
    )
    try:
        ensure_daemon_runtime_compatible(
            reviewed_cli,
            CONFIG["runtime_version"],
            codex_home,
            strict=False,
        )
        disable_daemon_auto_update(reviewed_cli, codex_home)

        with ExitStack() as operation_locks:
            target_lock_files = [
                operation_locks.enter_context(
                    acquire_operation_lock(
                        target_lock_path,
                        "installation operation",
                    )
                )
                for target_lock_path in target_lock_paths
            ]
            daemon_lock_file = operation_locks.enter_context(
                acquire_operation_lock(
                    lock_path,
                    "daemon operation",
                )
            )
            lease_lock_fds = tuple(
                lock_file.fileno()
                for lock_file in (*target_lock_files, daemon_lock_file)
            )
            candidate_host = None
            try:
                cleanup_stale_reviewed_stages(target_cli, reviewed_directory)
                if not target_cli.is_file() or not target_host.is_file():
                    raise ActivationFailure("Activation binary pair is incomplete")
                backup_cli, backup_host = rollback_paths(
                    target_cli,
                    target_host,
                    CONFIG["release_name"],
                )
                transaction_path = transaction_path_for_targets(target_cli, target_host)
                release_only_backups = rollback_paths(
                    target_cli,
                    target_host,
                    CONFIG["release_name"],
                    namespace="release-only",
                )
                legacy_backups = rollback_paths(
                    target_cli,
                    target_host,
                    CONFIG["release_name"],
                    namespace="legacy",
                )
                cleanup_stale_transaction_stages(
                    target_cli,
                    target_host,
                    backup_cli,
                    backup_host,
                    transaction_path,
                )
                cleanup_preparing_copies(*release_only_backups)
                if same_physical_path(
                    legacy_backups[0].parent, target_cli.parent
                ) and same_physical_path(legacy_backups[1].parent, target_host.parent):
                    cleanup_preparing_copies(*legacy_backups)
                pending_path, namespace = pending_transaction(
                    target_cli,
                    target_host,
                    transaction_path,
                )
                if pending_path is not None:
                    recover_transaction(
                        target_cli,
                        target_host,
                        pending_path,
                        namespace=namespace,
                    )
                backup_cli, backup_host = migrate_rollback_state(
                    target_cli,
                    target_host,
                    CONFIG["release_name"],
                    reviewed_source_hashes,
                )
                source_hashes = validate_release_inputs()
                if source_hashes != reviewed_source_hashes:
                    raise ActivationFailure(
                        "Activation binary pair changed while waiting for the operation lease"
                    )
                if sha256(reviewed_cli) != source_hashes[0]:
                    raise ActivationFailure("Private reviewed CLI hash changed")
                candidate_host = copy_to_stage(
                    source_host,
                    target_host,
                    "bound code-mode host",
                    source_hashes[1],
                )
                activate_under_lock(
                    codex_home,
                    reviewed_cli,
                    candidate_host,
                    source_hashes,
                    backup_cli,
                    backup_host,
                    transaction_path,
                )
                reviewed_cli = None
                candidate_host = None
            finally:
                unlink_if_present(candidate_host)
                lease_lock_fds = ()
    finally:
        lease_lock_fds = ()
        reviewed_ownership.close()
        shutil.rmtree(reviewed_directory, ignore_errors=True)


for handled_signal in (signal.SIGHUP, signal.SIGINT, signal.SIGTERM):
    signal.signal(handled_signal, interrupt_activation)

try:
    validate_and_activate()
except ActivationInterrupted as error:
    print(str(error), file=sys.stderr)
    raise SystemExit(128 + error.signum)
except BaseException as error:
    print(str(error), file=sys.stderr)
    raise SystemExit(1)
'''


WINDOWS_ACTIVATION_GUARDIAN = r"""$ErrorActionPreference = 'Stop'
$ParentPid = [int]$env:CODEX_FLEET_GUARDIAN_PARENT_PID
$PipeName = $env:CODEX_FLEET_GUARDIAN_PIPE_NAME
$TargetLockPaths = @(
  $env:CODEX_FLEET_GUARDIAN_TARGET_LOCK_PATHS -split "`n" |
    Where-Object { -not [string]::IsNullOrEmpty($_) } |
    ForEach-Object {
      [Environment]::ExpandEnvironmentVariables($_)
    } |
    Sort-Object -Unique
)
$DaemonLockPath = $env:CODEX_FLEET_GUARDIAN_DAEMON_LOCK_PATH
$ScriptPath = $env:CODEX_FLEET_GUARDIAN_SCRIPT
Remove-Item -Force -ErrorAction SilentlyContinue -LiteralPath $ScriptPath
Add-Type @'
using System;
using System.ComponentModel;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Threading;
using Microsoft.Win32.SafeHandles;

public static class FleetGuardianJob {
    private const uint JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x00002000;
    private const int JobObjectBasicAccountingInformation = 1;
    private const int JobObjectExtendedLimitInformation = 9;
    private static SafeFileHandle job;
    private static Process parent;

    [StructLayout(LayoutKind.Sequential)]
    private struct IoCounters {
        public ulong ReadOperationCount;
        public ulong WriteOperationCount;
        public ulong OtherOperationCount;
        public ulong ReadTransferCount;
        public ulong WriteTransferCount;
        public ulong OtherTransferCount;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct BasicLimitInformation {
        public long PerProcessUserTimeLimit;
        public long PerJobUserTimeLimit;
        public uint LimitFlags;
        public UIntPtr MinimumWorkingSetSize;
        public UIntPtr MaximumWorkingSetSize;
        public uint ActiveProcessLimit;
        public UIntPtr Affinity;
        public uint PriorityClass;
        public uint SchedulingClass;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct ExtendedLimitInformation {
        public BasicLimitInformation BasicLimitInformation;
        public IoCounters IoInfo;
        public UIntPtr ProcessMemoryLimit;
        public UIntPtr JobMemoryLimit;
        public UIntPtr PeakProcessMemoryUsed;
        public UIntPtr PeakJobMemoryUsed;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct BasicAccountingInformation {
        public long TotalUserTime;
        public long TotalKernelTime;
        public long ThisPeriodTotalUserTime;
        public long ThisPeriodTotalKernelTime;
        public uint TotalPageFaultCount;
        public uint TotalProcesses;
        public uint ActiveProcesses;
        public uint TotalTerminatedProcesses;
    }

    [DllImport("kernel32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
    private static extern SafeFileHandle CreateJobObject(IntPtr attributes, string name);

    [DllImport("kernel32.dll", SetLastError=true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool SetInformationJobObject(SafeFileHandle handle, int informationClass, IntPtr information, uint informationLength);

    [DllImport("kernel32.dll", SetLastError=true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool QueryInformationJobObject(SafeFileHandle handle, int informationClass, IntPtr information, uint informationLength, out uint returnLength);

    [DllImport("kernel32.dll", SetLastError=true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool AssignProcessToJobObject(SafeFileHandle handle, IntPtr process);

    [DllImport("kernel32.dll", SetLastError=true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool TerminateJobObject(SafeFileHandle handle, uint exitCode);

    public static void Install(int parentPid) {
        parent = Process.GetProcessById(parentPid);
        job = CreateJobObject(IntPtr.Zero, null);
        if (job.IsInvalid) {
            throw new Win32Exception(Marshal.GetLastWin32Error(), "failed to create activation containment job");
        }
        ExtendedLimitInformation limits = new ExtendedLimitInformation();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        int size = Marshal.SizeOf(limits);
        IntPtr buffer = Marshal.AllocHGlobal(size);
        try {
            Marshal.StructureToPtr(limits, buffer, false);
            if (!SetInformationJobObject(job, JobObjectExtendedLimitInformation, buffer, (uint)size)) {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "failed to configure activation containment job");
            }
        } finally {
            Marshal.FreeHGlobal(buffer);
        }
        if (!AssignProcessToJobObject(job, parent.Handle)) {
            throw new Win32Exception(Marshal.GetLastWin32Error(), "failed to contain the activation process");
        }
    }

    public static bool ParentExited() {
        parent.Refresh();
        return parent.HasExited;
    }

    private static bool TryActiveProcesses(out uint activeProcesses) {
        activeProcesses = UInt32.MaxValue;
        int size = Marshal.SizeOf(typeof(BasicAccountingInformation));
        IntPtr buffer = Marshal.AllocHGlobal(size);
        try {
            uint returned;
            if (!QueryInformationJobObject(job, JobObjectBasicAccountingInformation, buffer, (uint)size, out returned)) {
                return false;
            }
            activeProcesses = ((BasicAccountingInformation)Marshal.PtrToStructure(buffer, typeof(BasicAccountingInformation))).ActiveProcesses;
            return true;
        } finally {
            Marshal.FreeHGlobal(buffer);
        }
    }

    public static void TerminateAndWait() {
        while (job == null || job.IsInvalid) {
            Thread.Sleep(1000);
        }
        while (true) {
            TerminateJobObject(job, 125);
            uint activeProcesses;
            if (TryActiveProcesses(out activeProcesses) && activeProcesses == 0) {
                break;
            }
            Thread.Sleep(10);
        }
        job.Dispose();
        job = null;
    }
}
'@

function Wait-TaskOrParentExit($Task) {
  while (-not $Task.IsCompleted) {
    if ([FleetGuardianJob]::ParentExited()) { return $false }
    Start-Sleep -Milliseconds 10
  }
  [void]$Task.GetAwaiter().GetResult()
  return $true
}

function Acquire-OperationLease($Path, $Description) {
  [void][System.IO.Directory]::CreateDirectory((Split-Path -Parent $Path))
  $stream = [System.IO.FileStream]::new(
    $Path,
    [System.IO.FileMode]::OpenOrCreate,
    [System.IO.FileAccess]::ReadWrite,
    [System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete
  )
  $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
  try {
    while ($true) {
      try {
        $stream.Lock(0, 1)
        return $stream
      } catch [System.IO.IOException] {
        if ([FleetGuardianJob]::ParentExited()) { throw "activation parent exited while waiting for $Description lock" }
        if ($stopwatch.Elapsed -ge [TimeSpan]::FromSeconds(75)) { throw "timed out waiting for $Description lock $Path" }
        Start-Sleep -Milliseconds 50
      }
    }
  } catch {
    $stream.Dispose()
    throw
  }
}

$targetLeases = [System.Collections.Generic.List[System.IO.FileStream]]::new()
$daemonLease = $null
$pipe = $null
$jobInstalled = $false
$startupComplete = $false
try {
  [FleetGuardianJob]::Install($ParentPid)
  $jobInstalled = $true
  $pipe = [System.IO.Pipes.NamedPipeServerStream]::new(
    $PipeName,
    [System.IO.Pipes.PipeDirection]::InOut,
    1,
    [System.IO.Pipes.PipeTransmissionMode]::Byte,
    [System.IO.Pipes.PipeOptions]::Asynchronous
  )
  if (-not (Wait-TaskOrParentExit $pipe.WaitForConnectionAsync())) { return }
  $reader = [System.IO.StreamReader]::new($pipe)
  $writer = [System.IO.StreamWriter]::new($pipe)
  $writer.AutoFlush = $true
  $writer.WriteLine('ready')
  $startupComplete = $true
  while (-not [FleetGuardianJob]::ParentExited()) {
    $read = $reader.ReadLineAsync()
    if (-not (Wait-TaskOrParentExit $read)) { break }
    $command = $read.GetAwaiter().GetResult()
    if ($null -eq $command) { break }
    if ($command -eq 'complete') {
      $writer.WriteLine('completing')
      break
    }
    if ($command -ne 'acquire' -or $targetLeases.Count -ne 0 -or $null -ne $daemonLease) {
      $writer.WriteLine('error:invalid guardian command')
      continue
    }
    try {
      foreach ($targetLockPath in $TargetLockPaths) {
        $targetLeases.Add((Acquire-OperationLease $targetLockPath 'installation operation'))
      }
      $daemonLease = Acquire-OperationLease $DaemonLockPath 'daemon operation'
      $writer.WriteLine('acquired')
    } catch {
      if ($null -eq $daemonLease) {
        for ($index = $targetLeases.Count - 1; $index -ge 0; $index--) {
          try { $targetLeases[$index].Unlock(0, 1) } finally { $targetLeases[$index].Dispose() }
        }
        $targetLeases.Clear()
      }
      $writer.WriteLine("error:$($_.Exception.Message)")
      continue
    }
  }
} finally {
  if ($jobInstalled -and $startupComplete) {
    while (-not [FleetGuardianJob]::ParentExited()) {
      Start-Sleep -Milliseconds 10
    }
  }
  $containmentCleared = -not $jobInstalled
  try {
    if ($jobInstalled) {
      [FleetGuardianJob]::TerminateAndWait()
      $containmentCleared = $true
    }
  } finally {
    if ($null -ne $daemonLease -or $targetLeases.Count -ne 0) {
      while (-not $containmentCleared) { Start-Sleep -Seconds 1 }
      if ($null -ne $daemonLease) {
        try { $daemonLease.Unlock(0, 1) } finally { $daemonLease.Dispose() }
      }
      for ($index = $targetLeases.Count - 1; $index -ge 0; $index--) {
        try { $targetLeases[$index].Unlock(0, 1) } finally { $targetLeases[$index].Dispose() }
      }
    }
    if ($null -ne $pipe) { $pipe.Dispose() }
  }
}
"""


def activation_script(
    host: Host,
    release: Release,
    *,
    windows_installation_lock_path: str | None = None,
) -> str:
    source_cli = host.build_binary("codex")
    source_host = host.build_binary("codex-code-mode-host")
    receipt = host.build_receipt()
    backup_cli, backup_host = rollback_paths(host, release.name)
    transaction_state = transaction_path(host)
    target_locks = target_lock_paths(
        host,
        windows_installation_lock_path=windows_installation_lock_path,
    )
    target_locks_ps = ", ".join(quote_ps(path) for path in target_locks)
    if host.windows:
        return f"""$ErrorActionPreference = 'Stop'
Add-Type @'
using System;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;

public static class FleetFileOps {{
    private const uint FILE_SHARE_READ = 0x00000001;
    private const uint FILE_SHARE_WRITE = 0x00000002;
    private const uint FILE_SHARE_DELETE = 0x00000004;
    private const uint OPEN_EXISTING = 3;
    private const uint FILE_FLAG_BACKUP_SEMANTICS = 0x02000000;
    private const uint MOVEFILE_REPLACE_EXISTING = 0x00000001;
    private const uint MOVEFILE_WRITE_THROUGH = 0x00000008;
    private const int FileCaseSensitiveInfo = 23;
    private const uint FILE_CS_FLAG_CASE_SENSITIVE_DIR = 0x00000001;

    [DllImport("kernel32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool MoveFileEx(string existing, string destination, uint flags);

    [DllImport("kernel32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
    private static extern SafeFileHandle CreateFile(
        string name,
        uint access,
        uint share,
        IntPtr security,
        uint creation,
        uint flags,
        IntPtr template
    );

    [DllImport("kernel32.dll", SetLastError=true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool GetFileInformationByHandle(
        SafeFileHandle handle,
        out ByHandleFileInformation information
    );

    [DllImport("kernel32.dll", SetLastError=true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool GetFileInformationByHandleEx(
        SafeFileHandle handle,
        int informationClass,
        out FileCaseSensitiveInformation information,
        uint size
    );

    [StructLayout(LayoutKind.Sequential)]
    private struct ByHandleFileInformation {{
        public uint FileAttributes;
        public System.Runtime.InteropServices.ComTypes.FILETIME CreationTime;
        public System.Runtime.InteropServices.ComTypes.FILETIME LastAccessTime;
        public System.Runtime.InteropServices.ComTypes.FILETIME LastWriteTime;
        public uint VolumeSerialNumber;
        public uint FileSizeHigh;
        public uint FileSizeLow;
        public uint NumberOfLinks;
        public uint FileIndexHigh;
        public uint FileIndexLow;
    }}

    [StructLayout(LayoutKind.Sequential)]
    private struct FileCaseSensitiveInformation {{
        public uint Flags;
    }}

    private static SafeFileHandle OpenIdentityHandle(string path) {{
        SafeFileHandle handle = CreateFile(
            path,
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            IntPtr.Zero,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            IntPtr.Zero
        );
        if (handle.IsInvalid) {{
            throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
        }}
        return handle;
    }}

    public static string GetFileIdentity(string path) {{
        using (SafeFileHandle handle = OpenIdentityHandle(path)) {{
            ByHandleFileInformation information;
            if (!GetFileInformationByHandle(handle, out information)) {{
                throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
            }}
            return String.Format(
                "{{0:X8}}:{{1:X8}}{{2:X8}}",
                information.VolumeSerialNumber,
                information.FileIndexHigh,
                information.FileIndexLow
            );
        }}
    }}

    public static bool IsCaseSensitiveDirectory(string path) {{
        using (SafeFileHandle handle = OpenIdentityHandle(path)) {{
            FileCaseSensitiveInformation information;
            if (!GetFileInformationByHandleEx(
                handle,
                FileCaseSensitiveInfo,
                out information,
                (uint)Marshal.SizeOf(typeof(FileCaseSensitiveInformation))
            )) {{
                int error = Marshal.GetLastWin32Error();
                if (error == 1 || error == 50 || error == 87) {{
                    return false;
                }}
                throw new System.ComponentModel.Win32Exception(error);
            }}
            return (information.Flags & FILE_CS_FLAG_CASE_SENSITIVE_DIR) != 0;
        }}
    }}

    public static void PublishPreparedFile(string stage, string destination) {{
        if (!MoveFileEx(
            stage,
            destination,
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH
        )) {{
            throw new System.ComponentModel.Win32Exception(
                Marshal.GetLastWin32Error(),
                "write-through atomic publish failed; stage and destination must be on the same volume"
            );
        }}
    }}
}}

'@
function Stop-ActivationOnTimeout($Description) {{
  [Console]::Error.WriteLine("$Description timed out; terminating the contained activation")
  Stop-Process -Id $PID -Force
  throw "$Description timed out"
}}
function Wait-GuardianTask($Process, $Task, $TimeoutSeconds, $Description) {{
  $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
  while (-not $Task.IsCompleted) {{
    if ($Process.HasExited) {{ throw "activation guardian exited: $($Process.StandardError.ReadToEnd())" }}
    if ($stopwatch.Elapsed -ge [TimeSpan]::FromSeconds($TimeoutSeconds)) {{
      Stop-ActivationOnTimeout $Description
    }}
    Start-Sleep -Milliseconds 10
  }}
  return $Task.GetAwaiter().GetResult()
}}
function Start-ActivationGuardian([string[]]$TargetLockPaths, $DaemonLockPath) {{
  $scriptPath = Join-Path ([System.IO.Path]::GetTempPath()) ("codex-fleet-guardian-$([guid]::NewGuid().ToString('N')).ps1")
  [System.IO.File]::WriteAllText($scriptPath, {quote_ps(WINDOWS_ACTIVATION_GUARDIAN)})
  $pipeName = "codex-fleet-guardian-$([guid]::NewGuid().ToString('N'))"
  $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
  $startInfo.FileName = [System.Diagnostics.Process]::GetCurrentProcess().MainModule.FileName
  $startInfo.UseShellExecute = $false
  $startInfo.CreateNoWindow = $true
  $startInfo.RedirectStandardInput = $true
  $startInfo.RedirectStandardOutput = $true
  $startInfo.RedirectStandardError = $true
  $startInfo.EnvironmentVariables['CODEX_FLEET_GUARDIAN_PARENT_PID'] = [string]$PID
  $startInfo.EnvironmentVariables['CODEX_FLEET_GUARDIAN_PIPE_NAME'] = $pipeName
  $startInfo.EnvironmentVariables['CODEX_FLEET_GUARDIAN_TARGET_LOCK_PATHS'] = $TargetLockPaths -join "`n"
  $startInfo.EnvironmentVariables['CODEX_FLEET_GUARDIAN_DAEMON_LOCK_PATH'] = $DaemonLockPath
  $startInfo.EnvironmentVariables['CODEX_FLEET_GUARDIAN_SCRIPT'] = $scriptPath
  $guardianCommand = '& $env:CODEX_FLEET_GUARDIAN_SCRIPT'
  $encodedGuardianCommand = [Convert]::ToBase64String(
    [Text.Encoding]::Unicode.GetBytes($guardianCommand)
  )
  $startInfo.Arguments = "-NoLogo -NoProfile -NonInteractive -EncodedCommand $encodedGuardianCommand"
  try {{
    $process = [System.Diagnostics.Process]::Start($startInfo)
  }} catch {{
    Remove-Item -Force -ErrorAction SilentlyContinue -LiteralPath $scriptPath
    throw
  }}
  $pipe = [System.IO.Pipes.NamedPipeClientStream]::new(
    '.',
    $pipeName,
    [System.IO.Pipes.PipeDirection]::InOut,
    [System.IO.Pipes.PipeOptions]::Asynchronous
  )
  $connect = $pipe.ConnectAsync()
  [void](Wait-GuardianTask $process $connect 10 'activation guardian connection')
  $reader = [System.IO.StreamReader]::new($pipe)
  $writer = [System.IO.StreamWriter]::new($pipe)
  $writer.AutoFlush = $true
  $ready = Wait-GuardianTask $process ($reader.ReadLineAsync()) 10 'activation guardian readiness handshake'
  if ($ready -ne 'ready') {{ throw 'activation guardian failed its readiness handshake' }}
  return [pscustomobject]@{{ Process = $process; Pipe = $pipe; Reader = $reader; Writer = $writer }}
}}
function Acquire-ActivationLease($Guardian) {{
  $Guardian.Writer.WriteLine('acquire')
  $response = Wait-GuardianTask $Guardian.Process ($Guardian.Reader.ReadLineAsync()) 80 'activation guardian lease handshake'
  if ($response -ne 'acquired') {{ throw "activation guardian failed to acquire operation locks: $response" }}
}}
function Complete-ActivationGuardian($Guardian) {{
  if ($null -eq $Guardian) {{ return }}
  try {{
    $Guardian.Writer.WriteLine('complete')
    $response = Wait-GuardianTask $Guardian.Process ($Guardian.Reader.ReadLineAsync()) 10 'activation guardian completion handshake'
    if ($response -ne 'completing') {{ throw "activation guardian failed its completion handshake: $response" }}
  }} finally {{
    $Guardian.Pipe.Dispose()
    $Guardian.Process.Dispose()
  }}
}}
function Hash($Path) {{ (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash }}
function Get-PhysicalPathIdentity($Path) {{
  $current = [System.IO.Path]::GetFullPath([string]$Path)
  $suffix = [System.Collections.Generic.Stack[string]]::new()
  while (-not (Test-Path -LiteralPath $current)) {{
    $parent = [System.IO.Path]::GetDirectoryName($current)
    if ([string]::IsNullOrEmpty($parent) -or $parent -eq $current) {{
      throw "Could not resolve physical path identity: $Path"
    }}
    $suffix.Push([System.IO.Path]::GetFileName($current))
    $current = $parent
  }}
  $identity = [FleetFileOps]::GetFileIdentity($current)
  if ($suffix.Count -ne 0) {{
    $caseSensitive = [FleetFileOps]::IsCaseSensitiveDirectory($current)
    while ($suffix.Count -ne 0) {{
      $component = $suffix.Pop()
      if (-not $caseSensitive) {{ $component = $component.ToUpperInvariant() }}
      $identity += "|$($component.Length):$component"
      # A nonexistent child has no queryable case-sensitivity flag. Preserve
      # subsequent component case rather than risk conflating distinct paths.
      $caseSensitive = $true
    }}
  }}
  return $identity
}}
function Test-SamePhysicalPath($Left, $Right) {{
  return (Get-PhysicalPathIdentity $Left) -ceq (Get-PhysicalPathIdentity $Right)
}}
function Assert-CleanGit($Path, $ExpectedCommit, $Label) {{
  if (-not (Test-Path -LiteralPath (Join-Path $Path '.git'))) {{ throw "$Label is not a Git checkout" }}
  $commit = Invoke-HealthProbe 'git' $null @('-C', $Path, 'rev-parse', 'HEAD')
  $status = Invoke-HealthProbe 'git' $null @('-C', $Path, 'status', '--porcelain')
  if ($commit -ne $ExpectedCommit -or -not [string]::IsNullOrEmpty($status)) {{ throw "$Label is not clean at the manifest commit" }}
}}
function Receipt-Value($Path, $Name) {{
  $prefix = "$Name="
  $matches = @(Get-Content -LiteralPath $Path | Where-Object {{ $_.StartsWith($prefix, [StringComparison]::Ordinal) }})
  if ($matches.Count -ne 1) {{ throw "Invalid build receipt field: $Name" }}
  return $matches[0].Substring($prefix.Length)
}}
function Invoke-CodexWithHome($Codex, $CodexHome, [string[]]$Arguments) {{
  $hadCodexHome = Test-Path Env:CODEX_HOME
  $previousCodexHome = $env:CODEX_HOME
  try {{
    $env:CODEX_HOME = $CodexHome
    $output = ((& $Codex @Arguments 2>$null | Out-String).Trim())
    $exitCode = $LASTEXITCODE
  }} finally {{
    if ($hadCodexHome) {{ $env:CODEX_HOME = $previousCodexHome }}
    else {{ Remove-Item Env:CODEX_HOME -ErrorAction SilentlyContinue }}
  }}
  if ($exitCode -ne 0) {{ throw "Codex command failed: $($Arguments -join ' ')" }}
  return $output
}}
function Invoke-HealthProbe($Executable, $CodexHome, [string[]]$Arguments) {{
  $argumentsJson = ConvertTo-Json -Compress -InputObject @($Arguments)
  $probeHome = if ($null -eq $CodexHome) {{ '' }} else {{ [string]$CodexHome }}
  $job = Start-Job -ScriptBlock {{
    param($ProbeExecutable, $ProbeCodexHome, $ProbeArgumentsJson)
    if (-not [string]::IsNullOrEmpty($ProbeCodexHome)) {{
      $env:CODEX_HOME = $ProbeCodexHome
    }}
    $probeArguments = @(ConvertFrom-Json -InputObject $ProbeArgumentsJson)
    $probeOutput = ((& $ProbeExecutable @probeArguments 2>$null | Out-String).Trim())
    [pscustomobject]@{{ Output = $probeOutput; ExitCode = $LASTEXITCODE }}
  }} -ArgumentList @($Executable, $probeHome, $argumentsJson)
  try {{
    $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
    while ($job.State -eq 'NotStarted' -or $job.State -eq 'Running') {{
      if ($stopwatch.Elapsed -ge [TimeSpan]::FromSeconds(30)) {{
        Stop-ActivationOnTimeout "installer health probe $Executable"
      }}
      Start-Sleep -Milliseconds 20
    }}
    if ($job.State -ne 'Completed') {{
      throw "Installer health probe failed to run: $Executable"
    }}
    $probe = Receive-Job -Job $job
    if ($null -eq $probe -or $probe.ExitCode -ne 0) {{
      throw "Installer health probe failed: $Executable $($Arguments -join ' ')"
    }}
    return [string]$probe.Output
  }} finally {{
    Remove-Job -Force -Job $job -ErrorAction SilentlyContinue
  }}
}}
function Assert-DaemonRuntimeCompatible($Codex, $ExpectedVersion, $CodexHome) {{
  try {{
    $rawStatus = Invoke-HealthProbe $Codex $CodexHome @('app-server', 'daemon', 'version')
    $daemon = $rawStatus | ConvertFrom-Json -ErrorAction Stop
  }} catch {{
    throw "Could not verify the app-server daemon; refusing to replace Codex: $($_.Exception.Message)"
  }}
  if ($daemon.status -eq 'notRunning') {{ return }}
  if ($daemon.status -ne 'running') {{
    throw "Unexpected app-server daemon status '$($daemon.status)'; refusing to replace Codex"
  }}
  if (-not ($daemon.appServerVersion -is [string]) -or $daemon.appServerVersion -ne $ExpectedVersion) {{
    $actualVersion = if ($daemon.appServerVersion -is [string]) {{ $daemon.appServerVersion }} else {{ 'unknown' }}
    throw "App-server daemon version $actualVersion is running; the new Codex runtime is $ExpectedVersion. The daemon was left untouched"
  }}
}}
function Disable-DaemonAutoUpdate($Codex, $CodexHome) {{
  try {{
    [void](Invoke-CodexWithHome -Codex $Codex -CodexHome $CodexHome -Arguments @('app-server', 'daemon', 'disable-auto-update'))
  }} catch {{
    throw "Could not disable the official Codex daemon updater; refusing activation: $($_.Exception.Message)"
  }}
}}
function Publish-PreparedFile($Stage, $Destination) {{
  [FleetFileOps]::PublishPreparedFile($Stage, $Destination)
}}
function New-VerifiedStage($Source, $Target, $ExpectedHash, $Label) {{
  $stage = Join-Path (Split-Path -Parent $Target) ('.fleet-stage.' + [guid]::NewGuid().ToString('N'))
  try {{
    Copy-Item -LiteralPath $Source -Destination $stage
    $stream = [System.IO.File]::Open($stage, [System.IO.FileMode]::Open, [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::Read)
    try {{ $stream.Flush($true) }} finally {{ $stream.Dispose() }}
    if ((Hash $stage) -ne $ExpectedHash) {{ throw "$Label staging verification failed" }}
    return $stage
  }} catch {{
    Remove-Item -Force -ErrorAction SilentlyContinue -LiteralPath $stage
    throw
  }}
}}
function New-PrivateReviewedCli($Source, $Target, $ExpectedHash) {{
  $directory = Join-Path (Split-Path -Parent $Target) ('.fleet-reviewed.' + [guid]::NewGuid().ToString('N'))
  $ownership = $null
  $ownershipLocked = $false
  try {{
    [void][System.IO.Directory]::CreateDirectory($directory)
    $ownership = [System.IO.FileStream]::new(
      (Join-Path $directory 'owner.lock'),
      [System.IO.FileMode]::OpenOrCreate,
      [System.IO.FileAccess]::ReadWrite,
      [System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete
    )
    $ownership.Lock(0, 1)
    $ownershipLocked = $true
    $candidate = Join-Path $directory (Split-Path -Leaf $Source)
    Copy-Item -LiteralPath $Source -Destination $candidate
    $stream = [System.IO.File]::Open($candidate, [System.IO.FileMode]::Open, [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::Read)
    try {{ $stream.Flush($true) }} finally {{ $stream.Dispose() }}
    if ((Hash $candidate) -ne $ExpectedHash) {{ throw 'Reviewed CLI changed while it was staged' }}
    return [pscustomobject]@{{ Directory = $directory; Path = $candidate; Ownership = $ownership }}
  }} catch {{
    if ($null -ne $ownership) {{
      if ($ownershipLocked) {{ $ownership.Unlock(0, 1) }}
      $ownership.Dispose()
    }}
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue -LiteralPath $directory
    throw
  }}
}}
function Publish-VerifiedCopy($Source, $Destination, $ExpectedHash, $Label) {{
  $stage = New-VerifiedStage $Source $Destination $ExpectedHash $Label
  try {{
    Publish-PreparedFile $stage $Destination
    $stage = $null
  }} finally {{
    if ($null -ne $stage) {{ Remove-Item -Force -ErrorAction SilentlyContinue -LiteralPath $stage }}
  }}
}}
function Write-TransactionState($Path, $Transaction) {{
  $stage = Join-Path (Split-Path -Parent $Path) ('.fleet-journal.' + [guid]::NewGuid().ToString('N'))
  try {{
    $bytes = [System.Text.Encoding]::UTF8.GetBytes((ConvertTo-Json -Compress -InputObject $Transaction) + "`n")
    $stream = [System.IO.FileStream]::new($stage, [System.IO.FileMode]::CreateNew, [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
    try {{
      $stream.Write($bytes, 0, $bytes.Length)
      $stream.Flush($true)
    }} finally {{
      $stream.Dispose()
    }}
    Publish-PreparedFile $stage $Path
    $stage = $null
  }} finally {{
    if ($null -ne $stage) {{ Remove-Item -Force -ErrorAction SilentlyContinue -LiteralPath $stage }}
  }}
}}
function Set-TransactionPhase($Path, $Transaction, $Phase) {{
  $Transaction.phase = $Phase
  Write-TransactionState $Path $Transaction
}}
function Remove-StaleFleetStages($Directory) {{
  Get-ChildItem -Force -LiteralPath $Directory | Where-Object {{
    $_.Name.StartsWith('.fleet-stage.', [StringComparison]::OrdinalIgnoreCase) -or
    $_.Name.StartsWith('.fleet-journal.', [StringComparison]::OrdinalIgnoreCase)
  }} | ForEach-Object {{
    Remove-Item -Force -LiteralPath $_.FullName
  }}
}}
function Remove-StaleReviewedCliStages($Target, $PreserveDirectory) {{
  $directory = Split-Path -Parent $Target
  $prefix = '.fleet-reviewed.'
  Get-ChildItem -Force -LiteralPath $directory | Where-Object {{
    $_.Name.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)
  }} | ForEach-Object {{
    if ($_.FullName -eq $PreserveDirectory) {{ return }}
    if (-not $_.PSIsContainer -or
        ($_.Attributes -band [System.IO.FileAttributes]::ReparsePoint)) {{
      Remove-Item -Force -LiteralPath $_.FullName
      return
    }}
    $ownership = $null
    try {{
      $ownership = [System.IO.FileStream]::new(
        (Join-Path $_.FullName 'owner.lock'),
        [System.IO.FileMode]::OpenOrCreate,
        [System.IO.FileAccess]::ReadWrite,
        [System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete
      )
      try {{ $ownership.Lock(0, 1) }} catch [System.IO.IOException] {{ return }}
      $ownership.Unlock(0, 1)
      $ownership.Dispose()
      $ownership = $null
      Remove-Item -Recurse -Force -LiteralPath $_.FullName
    }} finally {{
      if ($null -ne $ownership) {{ $ownership.Dispose() }}
    }}
  }}
}}
function New-Transaction($TargetCli, $TargetHost, $BackupCli, $BackupHost, $OldCliHash, $OldHostHash, $NewCliHash, $NewHostHash) {{
  [pscustomobject]@{{
    format = 1
    release = {quote_ps(release.name)}
    targetCli = $TargetCli
    targetHost = $TargetHost
    backupCli = $BackupCli
    backupHost = $BackupHost
    oldCliHash = $OldCliHash
    oldHostHash = $OldHostHash
    newCliHash = $NewCliHash
    newHostHash = $NewHostHash
    phase = 'preparing'
  }}
}}
function Get-Sha256Text($Text) {{
  $hasher = [System.Security.Cryptography.SHA256]::Create()
  try {{
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($Text)
    return ([BitConverter]::ToString($hasher.ComputeHash($bytes))).Replace('-', '').ToLowerInvariant()
  }} finally {{
    $hasher.Dispose()
  }}
}}
function Get-TargetSlotIdentity($Target) {{
  $directory = Split-Path -Parent $Target
  $name = Split-Path -Leaf $Target
  if (-not [FleetFileOps]::IsCaseSensitiveDirectory($directory)) {{
    $name = $name.ToUpperInvariant()
  }}
  return "$(Get-PhysicalPathIdentity $directory)|$($name.Length):$name"
}}
function Get-TargetPairToken($TargetCli, $TargetHost) {{
  return Get-Sha256Text ((Get-TargetSlotIdentity $TargetCli) + "`0" + (Get-TargetSlotIdentity $TargetHost))
}}
function Get-TransactionPath($TargetCli, $TargetHost, $Namespace) {{
  if ($Namespace -eq 'release-only') {{ return "$TargetCli..fleet-transaction.json" }}
  if ($Namespace -ne 'target-pair') {{ throw "Unknown transaction namespace: $Namespace" }}
  $token = Get-TargetPairToken $TargetCli $TargetHost
  return Join-Path (Split-Path -Parent $TargetCli) ".fleet-transaction.$token.json"
}}
function Get-RollbackPaths($TargetCli, $TargetHost, $ReleaseName, $Namespace) {{
  if ($Namespace -eq 'legacy') {{
    return [pscustomobject]@{{
      Cli = "$TargetCli.$ReleaseName-previous"
      Host = "$TargetHost.$ReleaseName-previous"
    }}
  }}
  if ($Namespace -eq 'release-only') {{ $tokenSource = $ReleaseName }}
  elseif ($Namespace -eq 'target-pair') {{
    $tokenSource = (Get-TargetPairToken $TargetCli $TargetHost) + "`0" + $ReleaseName
  }} else {{ throw "Unknown rollback namespace: $Namespace" }}
  $token = Get-Sha256Text $tokenSource
  return [pscustomobject]@{{
    Cli = Join-Path (Split-Path -Parent $TargetCli) ".fleet-rollback.$token.cli"
    Host = Join-Path (Split-Path -Parent $TargetHost) ".fleet-rollback.$token.host"
  }}
}}
function Restore-Pair($TransactionPath, $Transaction) {{
  $restoreHost = New-VerifiedStage $Transaction.backupHost $Transaction.targetHost $Transaction.oldHostHash 'Code-mode host rollback'
  $restoreCli = $null
  try {{
    $restoreCli = New-VerifiedStage $Transaction.backupCli $Transaction.targetCli $Transaction.oldCliHash 'CLI rollback'
    Publish-PreparedFile $restoreHost $Transaction.targetHost
    $restoreHost = $null
    Set-TransactionPhase $TransactionPath $Transaction 'hostRestored'
    Publish-PreparedFile $restoreCli $Transaction.targetCli
    $restoreCli = $null
    Set-TransactionPhase $TransactionPath $Transaction 'pairRestored'
    if ((Hash $Transaction.targetCli) -ne $Transaction.oldCliHash -or (Hash $Transaction.targetHost) -ne $Transaction.oldHostHash) {{
      throw 'Rollback hash verification failed'
    }}
    Set-TransactionPhase $TransactionPath $Transaction 'cleanup'
    Remove-Item -Force -LiteralPath $Transaction.backupCli, $Transaction.backupHost
    Remove-Item -Force -LiteralPath $TransactionPath
  }} finally {{
    if ($null -ne $restoreHost) {{ Remove-Item -Force -ErrorAction SilentlyContinue -LiteralPath $restoreHost }}
    if ($null -ne $restoreCli) {{ Remove-Item -Force -ErrorAction SilentlyContinue -LiteralPath $restoreCli }}
  }}
}}
function Get-PendingTransaction($TransactionPath, $TargetCli, $TargetHost) {{
  $journals = [System.Collections.Generic.List[object]]::new()
  if (Test-Path -LiteralPath $TransactionPath) {{
    $journals.Add([pscustomobject]@{{ Path = $TransactionPath; Namespace = 'target-pair' }})
  }}
  $releaseOnlyPath = Get-TransactionPath $TargetCli $TargetHost 'release-only'
  if ($releaseOnlyPath -ne $TransactionPath -and (Test-Path -LiteralPath $releaseOnlyPath)) {{
    $journals.Add([pscustomobject]@{{ Path = $releaseOnlyPath; Namespace = 'release-only' }})
  }}
  $prefix = (Split-Path -Leaf $TargetCli) + '.'
  Get-ChildItem -File -Force -LiteralPath (Split-Path -Parent $TargetCli) | Where-Object {{
    $_.FullName -ne $TransactionPath -and $_.FullName -ne $releaseOnlyPath -and
    $_.Name.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase) -and
    $_.Name.EndsWith('-transaction.json', [StringComparison]::OrdinalIgnoreCase)
  }} | ForEach-Object {{
    $journals.Add([pscustomobject]@{{ Path = $_.FullName; Namespace = 'legacy' }})
  }}
  Get-ChildItem -File -Force -LiteralPath (Split-Path -Parent $TargetCli) | Where-Object {{
    $_.FullName -ne $TransactionPath -and
    $_.Name.StartsWith('.fleet-transaction.', [StringComparison]::OrdinalIgnoreCase) -and
    $_.Name.EndsWith('.json', [StringComparison]::OrdinalIgnoreCase)
  }} | ForEach-Object {{
    try {{
      $stored = Get-Content -Raw -LiteralPath $_.FullName | ConvertFrom-Json -ErrorAction Stop
      if ((Test-SamePhysicalPath $stored.targetCli $TargetCli) -and
          (Test-SamePhysicalPath $stored.targetHost $TargetHost)) {{
        $journals.Add([pscustomobject]@{{ Path = $_.FullName; Namespace = 'target-pair' }})
      }}
    }} catch {{
      # An unauthenticated journal for another target pair is not ours to recover.
    }}
  }}
  if ($journals.Count -gt 1) {{
    throw "Multiple activation transaction journals require manual recovery: $($journals.Path -join ', ')"
  }}
  if ($journals.Count -eq 0) {{ return $null }}
  return $journals[0]
}}
function Recover-Transaction($TransactionPath, $TargetCli, $TargetHost, $Namespace) {{
  if (-not (Test-Path -LiteralPath $TransactionPath)) {{ return }}
  try {{
    $transaction = Get-Content -Raw -LiteralPath $TransactionPath | ConvertFrom-Json -ErrorAction Stop
  }} catch {{
    throw "Invalid activation transaction state: $TransactionPath"
  }}
  if ($transaction.format -ne 1 -or
      -not ($transaction.release -is [string]) -or
      $transaction.release.Length -eq 0 -or
      -not ($transaction.targetCli -is [string]) -or
      -not ($transaction.targetHost -is [string]) -or
      -not ($transaction.backupCli -is [string]) -or
      -not ($transaction.backupHost -is [string]) -or
      $transaction.oldCliHash -notmatch '^[0-9A-Fa-f]{{64}}$' -or
      $transaction.oldHostHash -notmatch '^[0-9A-Fa-f]{{64}}$' -or
      $transaction.phase -notin @('preparing', 'prepared', 'cliReplaced', 'pairReplaced', 'hostRestored', 'pairRestored', 'cleanup', 'committed')) {{
    throw "Invalid activation transaction state: $TransactionPath"
  }}
  if (-not (Test-SamePhysicalPath $transaction.targetCli $TargetCli) -or
      -not (Test-SamePhysicalPath $transaction.targetHost $TargetHost)) {{
    throw "Activation transaction does not match this target pair: $TransactionPath"
  }}
  $storedRollback = Get-RollbackPaths $transaction.targetCli $transaction.targetHost $transaction.release $Namespace
  $currentRollback = Get-RollbackPaths $TargetCli $TargetHost $transaction.release $Namespace
  $storedTransactionPath = if ($Namespace -eq 'legacy') {{
    "$($transaction.targetCli).$($transaction.release)-transaction.json"
  }} else {{
    Get-TransactionPath $transaction.targetCli $transaction.targetHost $Namespace
  }}
  if (-not (Test-SamePhysicalPath $TransactionPath $storedTransactionPath) -or
      -not (Test-SamePhysicalPath $transaction.backupCli $storedRollback.Cli) -or
      -not (Test-SamePhysicalPath $transaction.backupHost $storedRollback.Host) -or
      -not (Test-SamePhysicalPath $transaction.backupCli $currentRollback.Cli) -or
      -not (Test-SamePhysicalPath $transaction.backupHost $currentRollback.Host)) {{
    throw "Activation transaction does not match this target pair: $TransactionPath"
  }}
  $BackupCli = $transaction.backupCli
  $BackupHost = $transaction.backupHost
  if (-not (Test-Path -LiteralPath $TargetCli) -or -not (Test-Path -LiteralPath $TargetHost)) {{
    throw "Activation transaction targets are incomplete: $TransactionPath"
  }}
  if ($transaction.phase -eq 'committed') {{
    if ($transaction.newCliHash -notmatch '^[0-9A-Fa-f]{{64}}$' -or
        $transaction.newHostHash -notmatch '^[0-9A-Fa-f]{{64}}$') {{
      throw "Invalid activation transaction state: $TransactionPath"
    }}
    if ((Hash $TargetCli) -ne $transaction.newCliHash -or (Hash $TargetHost) -ne $transaction.newHostHash) {{
      throw "Committed activation has an unexpected target pair: $TransactionPath"
    }}
    Remove-Item -Force -ErrorAction SilentlyContinue -LiteralPath $TransactionPath
    return
  }}
  if ($transaction.phase -eq 'preparing') {{
    if ((Hash $TargetCli) -ne $transaction.oldCliHash -or (Hash $TargetHost) -ne $transaction.oldHostHash) {{
      throw "Preparing activation changed its target pair: $TransactionPath"
    }}
    Remove-Item -Force -ErrorAction SilentlyContinue -LiteralPath $BackupCli, $BackupHost
    Remove-Item -Force -LiteralPath $TransactionPath
    return
  }}
  if ($transaction.phase -in @('pairRestored', 'cleanup')) {{
    if ((Hash $TargetCli) -ne $transaction.oldCliHash -or (Hash $TargetHost) -ne $transaction.oldHostHash) {{
      throw "Restored activation has an unexpected target pair: $TransactionPath"
    }}
    if ($transaction.phase -ne 'cleanup') {{ Set-TransactionPhase $TransactionPath $transaction 'cleanup' }}
    Remove-Item -Force -ErrorAction SilentlyContinue -LiteralPath $BackupCli, $BackupHost
    Remove-Item -Force -LiteralPath $TransactionPath
    return
  }}
  if (-not (Test-Path -LiteralPath $BackupCli) -or -not (Test-Path -LiteralPath $BackupHost)) {{
    throw "Activation recovery pair is incomplete: $BackupCli, $BackupHost"
  }}
  if ((Hash $BackupCli) -ne $transaction.oldCliHash -or (Hash $BackupHost) -ne $transaction.oldHostHash) {{
    throw "Activation recovery pair has unexpected contents: $BackupCli, $BackupHost"
  }}
  Restore-Pair $TransactionPath $transaction
}}
function Move-RollbackStateToTargetPair($TargetCli, $TargetHost, $ReleaseName, $SourceCliHash, $SourceHostHash) {{
  $targetCliHash = Hash $TargetCli
  $targetHostHash = Hash $TargetHost
  $current = Get-RollbackPaths $TargetCli $TargetHost $ReleaseName 'target-pair'
  $releaseOnly = Get-RollbackPaths $TargetCli $TargetHost $ReleaseName 'release-only'
  $migrationPairs = [System.Collections.Generic.List[object]]::new()
  $migrationPairs.Add($releaseOnly)
  $legacy = Get-RollbackPaths $TargetCli $TargetHost $ReleaseName 'legacy'
  if ((Test-SamePhysicalPath (Split-Path -Parent $legacy.Cli) (Split-Path -Parent $TargetCli)) -and
      (Test-SamePhysicalPath (Split-Path -Parent $legacy.Host) (Split-Path -Parent $TargetHost))) {{
    $migrationPairs.Add($legacy)
  }}
  $currentCliExists = Test-Path -LiteralPath $current.Cli -PathType Leaf
  $currentHostExists = Test-Path -LiteralPath $current.Host -PathType Leaf
  if ($currentCliExists -ne $currentHostExists) {{
    $matchingSource = $null
    foreach ($pair in $migrationPairs) {{
      if ((Test-Path -LiteralPath $pair.Cli -PathType Leaf) -and
          (Test-Path -LiteralPath $pair.Host -PathType Leaf) -and
          (-not $currentCliExists -or (Hash $current.Cli) -eq (Hash $pair.Cli)) -and
          (-not $currentHostExists -or (Hash $current.Host) -eq (Hash $pair.Host))) {{
        $matchingSource = $pair
        break
      }}
    }}
    if ($null -eq $matchingSource) {{
      throw "Release rollback pair is incomplete: $($current.Cli), $($current.Host)"
    }}
    if (-not $currentCliExists) {{
      Publish-VerifiedCopy $matchingSource.Cli $current.Cli (Hash $matchingSource.Cli) 'CLI rollback migration'
    }}
    if (-not $currentHostExists) {{
      Publish-VerifiedCopy $matchingSource.Host $current.Host (Hash $matchingSource.Host) 'Code-mode host rollback migration'
    }}
    $currentCliExists = $true
    $currentHostExists = $true
  }}

  $partialMigrationPairs = [System.Collections.Generic.List[object]]::new()
  foreach ($pair in $migrationPairs) {{
    $cliExists = Test-Path -LiteralPath $pair.Cli -PathType Leaf
    $hostExists = Test-Path -LiteralPath $pair.Host -PathType Leaf
    if ($cliExists -eq $hostExists) {{ continue }}
    if (-not $currentCliExists -or -not $currentHostExists -or
        ($cliExists -and (Hash $pair.Cli) -ne (Hash $current.Cli)) -or
        ($hostExists -and (Hash $pair.Host) -ne (Hash $current.Host))) {{
      throw "Release rollback pair is incomplete: $($pair.Cli), $($pair.Host)"
    }}
    $partialMigrationPairs.Add([pscustomobject]@{{
      Pair = $pair
      CliExists = $cliExists
      HostExists = $hostExists
    }})
  }}

  $pairHashes = [System.Collections.Generic.List[object]]::new()
  foreach ($pair in @($current) + @($migrationPairs)) {{
    if ((Test-Path -LiteralPath $pair.Cli -PathType Leaf) -and
        (Test-Path -LiteralPath $pair.Host -PathType Leaf)) {{
      $pairHashes.Add([pscustomobject]@{{
        Pair = $pair
        Cli = (Hash $pair.Cli)
        Host = (Hash $pair.Host)
      }})
    }}
  }}
  if ($targetCliHash -ne $SourceCliHash -or $targetHostHash -ne $SourceHostHash) {{
    foreach ($entry in $pairHashes) {{
      if ($entry.Cli -ne $targetCliHash -or $entry.Host -ne $targetHostHash) {{
        throw "Release rollback pair does not match the active binaries: $($entry.Pair.Cli), $($entry.Pair.Host)"
      }}
    }}
  }} elseif ($pairHashes.Count -gt 1) {{
    foreach ($entry in $pairHashes | Select-Object -Skip 1) {{
      if ($entry.Cli -ne $pairHashes[0].Cli -or $entry.Host -ne $pairHashes[0].Host) {{
        throw 'Release rollback namespaces disagree'
      }}
    }}
  }}
  foreach ($entry in $partialMigrationPairs) {{
    if ($entry.CliExists) {{ Remove-Item -Force -LiteralPath $entry.Pair.Cli }}
    if ($entry.HostExists) {{ Remove-Item -Force -LiteralPath $entry.Pair.Host }}
  }}
  foreach ($pair in $migrationPairs) {{
    if (-not (Test-Path -LiteralPath $pair.Cli -PathType Leaf) -or
        -not (Test-Path -LiteralPath $pair.Host -PathType Leaf)) {{ continue }}
    $rollbackCliHash = Hash $pair.Cli
    $rollbackHostHash = Hash $pair.Host
    if (-not (Test-Path -LiteralPath $current.Cli -PathType Leaf)) {{
      Publish-VerifiedCopy $pair.Cli $current.Cli $rollbackCliHash 'CLI rollback migration'
    }}
    if (-not (Test-Path -LiteralPath $current.Host -PathType Leaf)) {{
      Publish-VerifiedCopy $pair.Host $current.Host $rollbackHostHash 'Code-mode host rollback migration'
    }}
    if ((Hash $current.Cli) -ne $rollbackCliHash -or (Hash $current.Host) -ne $rollbackHostHash) {{
      throw 'Release rollback migration verification failed'
    }}
    Remove-Item -Force -LiteralPath $pair.Cli
    Remove-Item -Force -LiteralPath $pair.Host
  }}
  return $current
}}
$canonical = {quote_ps(host.source_root)}; $build = {quote_ps(host.build_root)}; $sourceCli = {quote_ps(source_cli)}; $sourceHost = {quote_ps(source_host)}; $receipt = {quote_ps(receipt)}
$targetCli = {quote_ps(host.cli_binary)}; $targetHost = {quote_ps(host.code_mode_host_binary)}
$release = {quote_ps(release.name)}
$targetLockPaths = @({target_locks_ps})
$server = {quote_ps(host.server_root)}
$codexHome = if ([string]::IsNullOrWhiteSpace($env:CODEX_HOME)) {{ Join-Path $HOME '.codex' }} else {{ $env:CODEX_HOME }}
$daemonLockDir = Join-Path $codexHome 'app-server-daemon'
$daemonLockPath = Join-Path $daemonLockDir 'daemon.lock'
$guardian = Start-ActivationGuardian $targetLockPaths $daemonLockPath
try {{
Assert-CleanGit $canonical {quote_ps(release.fork_commit)} 'Canonical checkout'
Assert-CleanGit $build {quote_ps(release.fork_commit)} 'Build worktree'
Assert-CleanGit $server {quote_ps(release.server_commit)} 'MCP server'
foreach ($path in @($sourceCli, $sourceHost)) {{ if (-not (Test-Path -LiteralPath $path)) {{ throw "Missing binary: $path" }} }}
if (-not (Test-Path -LiteralPath $receipt)) {{ throw 'Missing reviewed build receipt' }}
$reviewedSourceCliHash = Hash $sourceCli; $reviewedSourceHostHash = Hash $sourceHost
if ((Receipt-Value $receipt 'format') -ne 'v1' -or
    (Receipt-Value $receipt 'forkCommit') -ne {quote_ps(release.fork_commit)} -or
    (Receipt-Value $receipt 'cliVersion') -ne {quote_ps(release.cli_version)} -or
    (Receipt-Value $receipt 'cliSha256') -ne $reviewedSourceCliHash -or
    (Receipt-Value $receipt 'hostSha256') -ne $reviewedSourceHostHash) {{ throw 'Build receipt does not match the reviewed binary pair' }}
$reviewedCliStage = New-PrivateReviewedCli $sourceCli $targetCli $reviewedSourceCliHash
try {{
Assert-DaemonRuntimeCompatible $reviewedCliStage.Path {quote_ps(release.runtime_version)} $codexHome
Disable-DaemonAutoUpdate $reviewedCliStage.Path $codexHome
Acquire-ActivationLease $guardian
Remove-StaleReviewedCliStages $targetCli $reviewedCliStage.Directory
$transactionPath = Get-TransactionPath $targetCli $targetHost 'target-pair'
$releaseOnlyTransactionPath = Get-TransactionPath $targetCli $targetHost 'release-only'
$targetPairRollback = Get-RollbackPaths $targetCli $targetHost $release 'target-pair'
$releaseOnlyRollback = Get-RollbackPaths $targetCli $targetHost $release 'release-only'
$legacyRollback = Get-RollbackPaths $targetCli $targetHost $release 'legacy'
$stageDirectoryIdentities = [System.Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
foreach ($stagePath in @(
  $targetCli, $targetHost,
  $targetPairRollback.Cli, $targetPairRollback.Host,
  $releaseOnlyRollback.Cli, $releaseOnlyRollback.Host,
  $legacyRollback.Cli, $legacyRollback.Host,
  $transactionPath, $releaseOnlyTransactionPath
)) {{
  $stageDirectory = Split-Path -Parent $stagePath
  if ($stageDirectoryIdentities.Add((Get-PhysicalPathIdentity $stageDirectory))) {{
    Remove-StaleFleetStages $stageDirectory
  }}
}}
$pendingTransaction = Get-PendingTransaction $transactionPath $targetCli $targetHost
if ($null -ne $pendingTransaction) {{
  Recover-Transaction $pendingTransaction.Path $targetCli $targetHost $pendingTransaction.Namespace
}}
$rollback = Move-RollbackStateToTargetPair $targetCli $targetHost $release $reviewedSourceCliHash $reviewedSourceHostHash
$backupCli = $rollback.Cli
$backupHost = $rollback.Host
Assert-CleanGit $canonical {quote_ps(release.fork_commit)} 'Canonical checkout'
Assert-CleanGit $build {quote_ps(release.fork_commit)} 'Build worktree'
Assert-CleanGit $server {quote_ps(release.server_commit)} 'MCP server'
foreach ($path in @($sourceCli, $sourceHost)) {{ if (-not (Test-Path -LiteralPath $path)) {{ throw "Missing binary: $path" }} }}
if (-not (Test-Path -LiteralPath $receipt)) {{ throw 'Missing reviewed build receipt' }}
$sourceCliHash = Hash $sourceCli; $sourceHostHash = Hash $sourceHost
if ((Receipt-Value $receipt 'format') -ne 'v1' -or
    (Receipt-Value $receipt 'forkCommit') -ne {quote_ps(release.fork_commit)} -or
    (Receipt-Value $receipt 'cliVersion') -ne {quote_ps(release.cli_version)} -or
    (Receipt-Value $receipt 'cliSha256') -ne $sourceCliHash -or
    (Receipt-Value $receipt 'hostSha256') -ne $sourceHostHash) {{ throw 'Build receipt does not match the reviewed binary pair' }}
if ($sourceCliHash -ne $reviewedSourceCliHash -or $sourceHostHash -ne $reviewedSourceHostHash) {{
  throw 'Activation binary pair changed while waiting for the operation lease'
}}
if ((Hash $reviewedCliStage.Path) -ne $sourceCliHash) {{ throw 'Private reviewed CLI hash changed' }}
foreach ($path in @($targetCli, $targetHost)) {{ if (-not (Test-Path -LiteralPath $path)) {{ throw "Missing binary: $path" }} }}
$candidateCli = $reviewedCliStage.Path
$candidateHost = $null
try {{
  $candidateHost = New-VerifiedStage $sourceHost $targetHost $sourceHostHash 'Bound code-mode host'
  Assert-DaemonRuntimeCompatible $candidateCli {quote_ps(release.runtime_version)} $codexHome
  $needsActivation = (Hash $targetCli) -ne $sourceCliHash -or (Hash $targetHost) -ne $sourceHostHash
  if (-not $needsActivation -and
      (Test-Path -LiteralPath $backupCli) -ne (Test-Path -LiteralPath $backupHost)) {{
    throw 'Release rollback pair is incomplete'
  }}
  $transactionStarted = $false
  try {{
    if ($needsActivation) {{
      $oldCliHash = Hash $targetCli; $oldHostHash = Hash $targetHost
      $transaction = New-Transaction $targetCli $targetHost $backupCli $backupHost $oldCliHash $oldHostHash $sourceCliHash $sourceHostHash
      Write-TransactionState $transactionPath $transaction
      $transactionStarted = $true
      Publish-VerifiedCopy $targetCli $backupCli $oldCliHash 'CLI rollback pair'
      Publish-VerifiedCopy $targetHost $backupHost $oldHostHash 'Code-mode host rollback pair'
      Set-TransactionPhase $transactionPath $transaction 'prepared'
      Publish-PreparedFile $candidateCli $targetCli
      $candidateCli = $null
      Set-TransactionPhase $transactionPath $transaction 'cliReplaced'
      Publish-PreparedFile $candidateHost $targetHost
      $candidateHost = $null
      Set-TransactionPhase $transactionPath $transaction 'pairReplaced'
      if ((Hash $targetCli) -ne $sourceCliHash -or (Hash $backupCli) -ne $oldCliHash) {{ throw 'CLI replacement verification failed' }}
      if ((Hash $targetHost) -ne $sourceHostHash -or (Hash $backupHost) -ne $oldHostHash) {{ throw 'Code-mode host replacement verification failed' }}
    }}
    $version = Invoke-HealthProbe $targetCli $codexHome @('--version')
    if ($version -ne {quote_ps(release.cli_version)}) {{ throw "Unexpected fork version: $version" }}
    [void](Invoke-HealthProbe $targetCli $codexHome @('agents', '--help'))
    [void](Invoke-HealthProbe $targetHost $codexHome @('--help'))
    Assert-DaemonRuntimeCompatible $targetCli {quote_ps(release.runtime_version)} $codexHome
    if (Test-Path -LiteralPath $transactionPath) {{
      Set-TransactionPhase $transactionPath $transaction 'committed'
      Remove-Item -Force -ErrorAction SilentlyContinue -LiteralPath $transactionPath
    }}
  }} catch {{
    $activationError = $_.Exception.Message
    $rollbackErrors = [System.Collections.Generic.List[string]]::new()
    if (Test-Path -LiteralPath $transactionPath) {{
      try {{ Recover-Transaction $transactionPath $targetCli $targetHost 'target-pair' }}
      catch {{ $rollbackErrors.Add($_.Exception.Message) }}
    }}
    if ($rollbackErrors.Count -ne 0) {{ throw "Activation failed: $activationError; rollback failed: $($rollbackErrors -join '; '); recovery copies: $backupCli, $backupHost" }}
    if ($transactionStarted) {{ throw "Activation failed and both binaries were restored: $activationError" }}
    throw $activationError
  }}
}} finally {{
  if ($null -ne $candidateCli) {{ Remove-Item -Force -ErrorAction SilentlyContinue -LiteralPath $candidateCli }}
  if ($null -ne $candidateHost) {{ Remove-Item -Force -ErrorAction SilentlyContinue -LiteralPath $candidateHost }}
}}
Write-Output "activated-version=$version"
}} finally {{
  $reviewedCliStage.Ownership.Unlock(0, 1)
  $reviewedCliStage.Ownership.Dispose()
  Remove-Item -Recurse -Force -ErrorAction SilentlyContinue -LiteralPath $reviewedCliStage.Directory
}}
}} finally {{
  Complete-ActivationGuardian $guardian
}}
"""
    supervisor_config = {
        "canonical": host.source_root,
        "build": host.build_root,
        "server": host.server_root,
        "source_cli": source_cli,
        "source_host": source_host,
        "target_cli": host.cli_binary,
        "target_host": host.code_mode_host_binary,
        "backup_cli": backup_cli,
        "backup_host": backup_host,
        "transaction_state": transaction_state,
        "target_locks": target_locks,
        "receipt": receipt,
        "release_name": release.name,
        "fork_commit": release.fork_commit,
        "server_commit": release.server_commit,
        "cli_version": release.cli_version,
        "runtime_version": release.runtime_version,
    }
    supervisor_source = (
        f"CONFIG = {supervisor_config!r}\n" + POSIX_ACTIVATION_SUPERVISOR
    )
    return f"exec python3 -c {shlex.quote(supervisor_source)}\n"


def print_status(statuses: list[Status], release: Release) -> bool:
    print("host\tcli\tserver\tsource\tresult")
    clean = True
    for status in statuses:
        drift = status.drift(release)
        clean &= not drift
        values = status.values
        print(
            "\t".join(
                [
                    status.host.name,
                    values.get("cliVersion", "unavailable"),
                    values.get("serverCommit", "unavailable"),
                    values.get("sourceCommit", "unavailable"),
                    "ok" if not drift else ",".join(drift),
                ]
            )
        )
    return clean


def apply_stage(host: Host, release: Release, bundle: Path | None) -> None:
    require_server(host, release)
    if bundle:
        copy_bundle(host, release, bundle)
    result = remote(host, build_script(host, release, bundle is not None))
    if result.returncode:
        raise FleetError(f"{host.name}: {error_message('building fork', result)}")
    print(f"{host.name}: {result.stdout.strip()}")


def apply_rollout(host: Host, release: Release, bundle: Path | None) -> None:
    apply_stage(host, release, bundle)
    result = remote(host, activation_script(host, release))
    if result.returncode:
        raise FleetError(f"{host.name}: {error_message('activating fork', result)}")
    print(f"{host.name}: {result.stdout.strip()}")


def args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    commands = parser.add_subparsers(dest="command", required=True)
    for command in ("status", "plan"):
        child = commands.add_parser(command)
        child.add_argument("--host", action="append")
    for command in ("activate", "stage", "rollout"):
        child = commands.add_parser(command)
        child.add_argument("--host", action="append", required=True)
        child.add_argument("--apply", action="store_true")
        if command in {"stage", "rollout"}:
            child.add_argument("--bundle", type=Path)
    return parser.parse_args()


def main() -> int:
    parsed = args()
    try:
        release, hosts = load_manifest(parsed.manifest)
        hosts = selected_hosts(hosts, parsed.host)
        if parsed.command == "plan":
            for host in hosts:
                print(
                    f"{host.name}: gate server → stage {release.fork_commit} → build {host.build_root} → atomic activate → status"
                )
            return 0
        if parsed.command == "status":
            return (
                0 if print_status([host_status(host) for host in hosts], release) else 2
            )
        if not parsed.apply:
            raise FleetError(
                f"{parsed.command} changes remote state; re-run with --apply"
            )
        if parsed.command in {"stage", "rollout"}:
            bundle = parsed.bundle
            if bundle and not bundle.is_file():
                raise FleetError(f"release bundle does not exist: {bundle}")
            for host in hosts:
                if parsed.command == "stage":
                    apply_stage(host, release, bundle)
                else:
                    apply_rollout(host, release, bundle)
            if parsed.command == "stage":
                return 0
        else:
            for host in hosts:
                result = remote(host, activation_script(host, release))
                if result.returncode:
                    raise FleetError(
                        f"{host.name}: {error_message('activating fork', result)}"
                    )
                print(f"{host.name}: {result.stdout.strip()}")
        statuses = [host_status(host) for host in hosts]
        return 0 if print_status(statuses, release) else 2
    except FleetError as error:
        print(f"fleet-release: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
