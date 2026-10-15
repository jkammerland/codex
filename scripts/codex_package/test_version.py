"""Maintained release packaging must preserve the compiled client version."""

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from codex_package.layout import build_package_dir
from codex_package.version import validate_maintained_release_version


class MaintainedReleaseVersionTest(unittest.TestCase):
    def test_unstamped_workspace_rejected_before_package_writes(self):
        for workspace in ("0.0.0", "0.0.0-alpha.1"):
            with (
                self.subTest(workspace=workspace),
                tempfile.TemporaryDirectory() as root,
            ):
                with patch(
                    "codex_package.layout.read_workspace_version",
                    return_value=workspace,
                ):
                    with self.assertRaisesRegex(ValueError, "nonzero Cargo"):
                        build_package_dir(
                            Path(root), "0.160.1+jkammerland.mcp.24", None, None, None
                        )
                self.assertEqual(list(Path(root).iterdir()), [])

    def test_explicit_lineage_version_accepted(self):
        validate_maintained_release_version("0.160.1+jkammerland.mcp.24", "0.160.1")

    def test_metadata_cannot_disguise_compiled_version(self):
        with self.assertRaisesRegex(ValueError, "must match"):
            validate_maintained_release_version("0.160.1+jkammerland.mcp.24", "0.159.2")

    def test_upstream_development_packages_remain_supported(self):
        validate_maintained_release_version("0.0.0", "0.0.0")
