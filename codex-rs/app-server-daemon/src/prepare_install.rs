//! Installs complete CLI packages after confirmation. Initial starts preserve
//! existing selections; explicit replacements migrate to dedicated packages and stay pinned.

use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use codex_install_context::CodexPackageManifest;
use codex_install_context::InstallContext;

use crate::Daemon;
use crate::install_lock::acquire_install_lock;
use crate::managed_install;
use crate::runtime_identity::DaemonOwnership;
use crate::settings::DaemonSettings;

/// The complete CLI package offered for a daemon installation.
pub struct InstallRequest {
    pub source: PathBuf,
    pub version: String,
    pub destination: PathBuf,
    pub installed_version: Option<String>,
    pub restart_required: bool,
}

/// Prepare a missing package while the caller holds the daemon operation lock.
pub(super) async fn prepare(daemon: &Daemon, settings: &DaemonSettings) -> Result<()> {
    let package_layout = InstallContext::current().package_layout.as_ref();
    let raw_package =
        if package_layout.is_none() && !codex_build_info::CODEX_FORK_BUILD_ID.is_empty() {
            Some(materialize_raw_fork_package(
                daemon
                    .settings_file
                    .parent()
                    .and_then(Path::parent)
                    .context("daemon settings path has no Codex home")?,
                &std::env::current_exe()?,
            )?)
        } else {
            None
        };
    let source = package_layout
        .map(|layout| layout.package_dir.as_path())
        .or_else(|| raw_package.as_ref().map(tempfile::TempDir::path));
    // Keep package replacement state out of the CLI dispatcher's async stack frame.
    Box::pin(prepare_from_package(
        daemon,
        settings,
        InstallMode::Missing,
        source,
        &std::env::current_exe()?,
        |_| Ok(true),
    ))
    .await
    .map(|_| ())
}

/// Select this CLI's complete package and pin it, restarting only a running daemon.
/// Returns None when the user cancels without changing the installation.
pub async fn update_from_cli(
    confirm: impl FnOnce(&InstallRequest) -> Result<bool>,
) -> Result<Option<crate::UpdateOutput>> {
    crate::ensure_supported_platform()?;
    #[cfg(windows)]
    crate::backend::windows::ensure_not_elevated()?;
    let daemon = Daemon::from_environment()?;
    let settings = daemon.load_settings().await?;
    let package_layout = InstallContext::current().package_layout.as_ref();
    let raw_package =
        if package_layout.is_none() && !codex_build_info::CODEX_FORK_BUILD_ID.is_empty() {
            Some(materialize_raw_fork_package(
                daemon
                    .settings_file
                    .parent()
                    .and_then(Path::parent)
                    .context("daemon settings path has no Codex home")?,
                &std::env::current_exe()?,
            )?)
        } else {
            None
        };
    let source = package_layout
        .map(|layout| layout.package_dir.as_path())
        .or_else(|| raw_package.as_ref().map(tempfile::TempDir::path));
    if !Box::pin(prepare_from_package(
        &daemon,
        &settings,
        InstallMode::Replace,
        source,
        &std::env::current_exe()?,
        confirm,
    ))
    .await?
    {
        return Ok(None);
    }
    let managed_codex_path = daemon.current_managed_codex_bin()?;
    Ok(Some(crate::UpdateOutput {
        status: crate::UpdateStatus::Updated,
        installed_version: Some(managed_install::managed_codex_version(&managed_codex_path).await?),
        running_version: crate::client::probe(&daemon.socket_path)
            .await
            .ok()
            .map(|info| info.app_server_version),
        managed_codex_path,
        message: "The CLI package is selected and pinned. Run `codex app-server daemon update` to return to production updates.".to_string(),
    }))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InstallMode {
    Missing,
    Replace,
}

fn materialize_raw_fork_package(temp_root: &Path, running_exe: &Path) -> Result<tempfile::TempDir> {
    let package = tempfile::TempDir::new_in(temp_root)?;
    for directory in ["bin", "codex-path", "codex-resources"] {
        std::fs::create_dir_all(package.path().join(directory))?;
    }
    let context = InstallContext::current();
    let entrypoint = if cfg!(windows) {
        "bin/codex.exe"
    } else {
        "bin/codex"
    };
    let code_mode_host = if cfg!(windows) {
        "bin/codex-code-mode-host.exe"
    } else {
        "bin/codex-code-mode-host"
    };
    copy_package_executable(running_exe, &package.path().join(entrypoint))?;
    copy_package_executable(
        &context.code_mode_host_program(),
        &package.path().join(code_mode_host),
    )?;
    let rg = context.rg_command();
    let rg = if rg.is_file() {
        rg
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join(&rg))
            .find(|path| path.is_file())
            .context("maintained fork raw package is missing rg")?
    };
    copy_package_executable(
        &rg,
        &package.path().join(if cfg!(windows) {
            "codex-path/rg.exe"
        } else {
            "codex-path/rg"
        }),
    )?;
    if cfg!(target_os = "linux") {
        let bwrap = [Path::new("/usr/bin/bwrap"), Path::new("/bin/bwrap")]
            .into_iter()
            .find(|path| path.is_file())
            .context("maintained fork raw package requires bwrap")?;
        copy_package_executable(bwrap, &package.path().join("codex-resources/bwrap"))?;
    }
    std::fs::write(
        package.path().join("codex-package.json"),
        serde_json::to_vec(&serde_json::json!({
            "layoutVersion": 1,
            "version": codex_build_info::CODEX_CLI_VERSION,
            "target": platform_target()?,
            "variant": "codex",
            "entrypoint": entrypoint,
            "resourcesDir": "codex-resources",
            "pathDir": "codex-path"
        }))?,
    )?;
    Ok(package)
}

fn copy_package_executable(source: &Path, destination: &Path) -> Result<()> {
    anyhow::ensure!(
        source.is_file(),
        "maintained fork raw package is missing {}",
        source.display()
    );
    std::fs::copy(source, destination)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(destination, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

async fn prepare_from_package(
    daemon: &Daemon,
    settings: &DaemonSettings,
    mode: InstallMode,
    source: Option<&Path>,
    running_exe: &Path,
    confirm: impl FnOnce(&InstallRequest) -> Result<bool>,
) -> Result<bool> {
    let home = daemon
        .settings_file
        .parent()
        .and_then(Path::parent)
        .context("daemon settings path has no Codex home")?;
    let previous_root = managed_install::package_root(home);
    let root = home.join("packages/app-server-daemon");
    anyhow::ensure!(
        daemon.managed_codex_bin.starts_with(&previous_root),
        "daemon package location changed; retry the command"
    );
    if mode == InstallMode::Missing {
        if previous_root != root
            || daemon.running_backend_instance(settings).await?.is_some()
            || crate::client::probe(&daemon.socket_path).await.is_ok()
        {
            return Ok(true);
        }
        if !matches!(root.join("current").symlink_metadata(), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
            || ["daemon.pid", "daemon.stderr.log", "daemon-updater.pid", "daemon-updater.stderr.log"]
                .iter().any(|name| !matches!(home.join("app-server-daemon").join(name).symlink_metadata(), Err(error) if error.kind() == std::io::ErrorKind::NotFound))
        {
            daemon.ensure_managed_codex_bin()?;
            return Ok(true);
        }
    } else {
        anyhow::ensure!(
            previous_root.join("current").symlink_metadata().is_ok(),
            "no daemon package is selected; run `codex app-server daemon start` first"
        );
    }
    std::fs::create_dir_all(&root)?;
    anyhow::ensure!(
        managed_install::package_root(home) == previous_root,
        "daemon package location changed; retry the command"
    );
    let backend = daemon.running_backend_instance(settings).await?;
    anyhow::ensure!(
        backend.is_some() || crate::client::probe(&daemon.socket_path).await.is_err(),
        "app server is running but is not managed by codex app-server daemon"
    );
    if backend.is_some() {
        let info = crate::client::probe(&daemon.socket_path).await?;
        crate::runtime_identity::validate_probe(&info, DaemonOwnership::ManagedExisting)?;
    }
    let selected = managed_install::managed_codex_bin(home);
    let previous_release = previous_root.join("current").canonicalize().ok();
    if mode == InstallMode::Missing {
        if selected.is_file() {
            return Ok(true);
        }
        anyhow::ensure!(
            backend.is_none()
                && matches!(root.join("current").symlink_metadata(), Err(error) if error.kind() == std::io::ErrorKind::NotFound),
            "the selected daemon package is incomplete; repair its installation before starting"
        );
    }
    let source = source.context(
        "this CLI has no complete local package; install a packaged Codex CLI or use the standalone installer",
    )?;
    anyhow::ensure!(
        !root.canonicalize()?.starts_with(source.canonicalize()?),
        "CODEX_HOME must be outside the source CLI package"
    );
    let manifest_bytes = std::fs::read(source.join("codex-package.json"))?;
    let manifest: CodexPackageManifest = serde_json::from_slice(&manifest_bytes)?;
    let version = manifest.version.to_string();
    let target = platform_target()?;
    let metadata: serde_json::Value = serde_json::from_slice(&manifest_bytes)?;
    let entrypoint = if cfg!(windows) {
        "bin/codex.exe"
    } else {
        "bin/codex"
    };
    anyhow::ensure!(
        metadata["target"] == target && metadata["entrypoint"] == entrypoint,
        "the CLI package does not match this platform or executable"
    );
    validate_package(source)?;
    // A local build may replace the executable while confirmation is pending.
    let running_identity = managed_install::executable_identity(running_exe).await?;
    if mode == InstallMode::Replace {
        if !confirm(&InstallRequest {
            source: source.to_path_buf(),
            version: version.clone(),
            destination: root.clone(),
            installed_version: tokio::time::timeout(
                std::time::Duration::from_secs(5),
                managed_install::managed_codex_version(&selected),
            )
            .await
            .ok()
            .and_then(Result::ok),
            restart_required: backend.is_some(),
        })? {
            return Ok(false);
        }
    } else {
        daemon.diagnostic(format_args!(
            "Installing daemon from CLI version {version} into {}...",
            root.display()
        ));
    }
    // Confirmation must not block lifecycle commands. Recheck the approved
    // selection and running state once this operation owns both locks.
    let _operation_lock = if mode == InstallMode::Replace {
        Some(daemon.acquire_operation_lock().await?)
    } else {
        None // Initial startup already holds the operation lock.
    };
    let current_settings = if mode == InstallMode::Replace {
        Some(daemon.load_settings().await?)
    } else {
        None
    };
    let settings = current_settings.as_ref().unwrap_or(settings);
    let _install_lock = acquire_install_lock(&root).await?;
    anyhow::ensure!(
        managed_install::package_root(home) == previous_root,
        "daemon package location changed; retry the command"
    );
    if mode == InstallMode::Missing && managed_install::managed_codex_bin(home).is_file() {
        return Ok(true);
    }
    anyhow::ensure!(
        previous_root.join("current").canonicalize().ok() == previous_release,
        "daemon selection changed while awaiting confirmation; retry the command"
    );
    let current_backend = daemon.running_backend_instance(settings).await?;
    anyhow::ensure!(
        current_backend.is_some() == backend.is_some(),
        "daemon running state changed while awaiting confirmation; retry the command"
    );
    let backend = current_backend;
    let stable = stable_version(&version).is_some();
    let releases = root.join("releases");
    std::fs::create_dir_all(&releases)?;
    let stage = tempfile::Builder::new()
        .prefix(".staging.")
        .tempdir_in(&releases)?;
    let source_digest = package_tree(source, /*destination*/ None)?;
    package_tree(source, Some(stage.path()))?;
    validate_package(stage.path())?;
    let staged_exe = if cfg!(target_os = "macos") && stage.path().join("CodexCLI.app").is_dir() {
        stage.path().join("CodexCLI.app/Contents/MacOS/codex")
    } else {
        stage.path().join(entrypoint)
    };
    let binary_version =
        managed_install::managed_codex_version(&stage.path().join(entrypoint)).await?;
    let fork_compatibility = !codex_build_info::CODEX_FORK_BUILD_ID.is_empty()
        && binary_version == codex_build_info::CODEX_CLI_VERSION;
    anyhow::ensure!(
        !stable || version == binary_version || fork_compatibility,
        "the CLI package version does not match its executable"
    );
    if fork_compatibility {
        let mut staged_metadata: serde_json::Value = serde_json::from_slice(&manifest_bytes)?;
        staged_metadata["version"] = serde_json::Value::String(binary_version.clone());
        std::fs::write(
            stage.path().join("codex-package.json"),
            serde_json::to_vec(&staged_metadata)?,
        )?;
    }
    let digest = package_tree(stage.path(), /*destination*/ None)?;
    anyhow::ensure!(
        package_tree(source, /*destination*/ None)? == source_digest
            && (fork_compatibility
                || std::fs::read(stage.path().join("codex-package.json"))? == manifest_bytes)
            && managed_install::executable_identity(&staged_exe).await? == running_identity,
        "the CLI package changed while preparing the daemon or differs from the running executable"
    );
    let package_is_stable = stable && !fork_compatibility;
    let name = if package_is_stable && mode == InstallMode::Missing {
        format!("{version}-{target}")
    } else {
        format!("local-{digest}-{target}")
    };
    let release = releases.join(&name);
    if release.try_exists()? {
        anyhow::ensure!(
            !release.symlink_metadata()?.file_type().is_symlink()
                && package_tree(&release, /*destination*/ None)? == digest,
            "an existing daemon release has different contents; refusing to overwrite it"
        );
    } else {
        #[cfg(unix)]
        if !stage.path().join("codex").exists() {
            std::os::unix::fs::symlink("bin/codex", stage.path().join("codex"))?;
        }
        std::fs::rename(stage.path(), &release)?;
    }
    let standalone = home.join("packages/standalone");
    let canonical_source = source.canonicalize()?;
    let follows_latest = mode == InstallMode::Missing
        && package_is_stable
        && (standalone.join("current").canonicalize().ok().as_deref()
            != Some(canonical_source.as_path())
            || std::fs::read_to_string(standalone.join("auto-update-version"))
                .ok()
                .as_deref()
                == canonical_source.file_name().and_then(|name| name.to_str()));
    anyhow::ensure!(
        managed_install::package_root(home) == previous_root
            && (backend.is_some() || crate::client::probe(&daemon.socket_path).await.is_err()),
        "daemon package location or socket ownership changed while preparing its package; retry the command"
    );
    #[cfg(windows)]
    if backend.is_some() {
        crate::backend::windows::ensure_detached_launch(&release.join(entrypoint))?;
    }
    #[cfg(windows)]
    windows::validate_selection(&root)?;
    if mode == InstallMode::Replace {
        let stopped = async {
            crate::backend::pid_update_loop_backend(daemon.backend_paths(settings))
                .stop()
                .await?;
            anyhow::ensure!(
                managed_install::package_root(home) == previous_root
                    && previous_root.join("current").canonicalize().ok() == previous_release,
                "daemon selection changed while preparing its package; retry the command"
            );
            if let Some(backend) = &backend {
                if let Err(error) = crate::thread_recovery::discard_pending(daemon) {
                    eprintln!(
                        "warning: failed to clear stale daemon recovery before replacement: {error}"
                    );
                }
                backend
                    .stop_with_grace(settings.shutdown_grace_seconds)
                    .await?;
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(error) = stopped {
            if let Err(restore_error) = async {
                daemon
                    .current_installation()?
                    .ensure_managed_updater(settings)
                    .await
            }
            .await
            {
                eprintln!(
                    "warning: failed to restore the daemon updater after replacement failed: {restore_error:#}"
                );
            }
            return Err(error);
        }
    }
    let marker = root.join("auto-update-version");
    if follows_latest {
        let temporary = tempfile::NamedTempFile::new_in(&root)?;
        std::fs::write(temporary.path(), &name)?;
        temporary.persist(marker)?;
    } else if marker.exists() {
        std::fs::remove_file(marker)?;
    }
    #[cfg(unix)]
    {
        let temporary = tempfile::TempDir::new_in(&root)?;
        let link = temporary.path().join("current");
        std::os::unix::fs::symlink(&release, &link)?;
        std::fs::rename(link, root.join("current"))?;
    }
    #[cfg(windows)]
    windows::select_release(&root, &release)?;
    if backend.is_some() {
        let selected = Daemon {
            pid_file: daemon.pid_file.with_file_name(crate::DAEMON_PID_FILE_NAME),
            update_pid_file: daemon
                .update_pid_file
                .with_file_name(crate::DAEMON_UPDATE_PID_FILE_NAME),
            managed_codex_bin: root.join("current").join(entrypoint),
            ..daemon.clone()
        };
        selected
            .start_managed_backend_and_validate(settings, &selected.managed_codex_bin)
            .await
            .context(
                "daemon package selected but could not start; retry with `codex app-server daemon start`",
            )?;
    }
    Ok(true)
}

/// Hash the complete tree and optionally copy those same bytes. Relative file
/// links are materialized; escaping links and directory links are rejected.
fn package_tree(root: &Path, destination: Option<&Path>) -> Result<String> {
    let canonical_root = root.canonicalize()?;
    let mut hasher = blake3::Hasher::new();
    let mut paths = vec![root.to_path_buf()];
    while let Some(path) = paths.pop() {
        let relative = path.strip_prefix(root)?;
        // The Unix installer adds this alias outside the package layout.
        if cfg!(unix)
            && relative == Path::new("codex")
            && std::fs::read_link(&path).ok().as_deref() == Some(Path::new("bin/codex"))
        {
            continue;
        }
        anyhow::ensure!(
            path.canonicalize()?.starts_with(&canonical_root),
            "package link escapes its root"
        );
        let metadata = path.metadata()?;
        hasher.update(relative.as_os_str().as_encoded_bytes());
        hasher.update(&[0]);
        if metadata.is_dir() {
            anyhow::ensure!(
                !path.symlink_metadata()?.file_type().is_symlink(),
                "package contains a directory link"
            );
            hasher.update(b"directory");
            if let Some(destination) = destination
                && !relative.as_os_str().is_empty()
            {
                std::fs::create_dir(destination.join(relative))?;
            }
            let mut entries = std::fs::read_dir(&path)?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<std::io::Result<Vec<_>>>()?;
            entries.sort();
            paths.extend(entries);
        } else {
            anyhow::ensure!(metadata.is_file(), "package contains an unsupported file");
            let bytes = std::fs::read(&path)?;
            hasher.update(b"file");
            hasher.update(blake3::hash(&bytes).as_bytes());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                hasher.update(&(metadata.permissions().mode() & 0o777).to_le_bytes());
            }
            if let Some(destination) = destination {
                let target = destination.join(relative);
                std::fs::write(&target, bytes)?;
                std::fs::set_permissions(target, metadata.permissions())?;
            }
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn stable_version(value: &str) -> Option<semver::Version> {
    let version = semver::Version::parse(value).ok()?;
    (version.pre.is_empty()
        && version.build.is_empty()
        && (version.major, version.minor, version.patch) != (0, 0, 0))
        .then_some(version)
}

fn validate_package(root: &Path) -> Result<()> {
    let mut names = vec![
        "codex-package.json",
        if cfg!(windows) {
            "bin/codex.exe"
        } else {
            "bin/codex"
        },
        if cfg!(windows) {
            "bin/codex-code-mode-host.exe"
        } else {
            "bin/codex-code-mode-host"
        },
        if cfg!(windows) {
            "codex-path/rg.exe"
        } else {
            "codex-path/rg"
        },
    ];
    if cfg!(windows) {
        names.extend([
            "codex-resources/codex-command-runner.exe",
            "codex-resources/codex-windows-sandbox-setup.exe",
        ]);
    } else if cfg!(target_os = "linux") {
        names.push("codex-resources/bwrap");
    }
    for name in names {
        if !root.join(name).is_file() {
            return Err(anyhow!(
                "local Codex package is missing {name}; reinstall the CLI or use the standalone installer"
            ));
        }
        #[cfg(unix)]
        if name != "codex-package.json" {
            use std::os::unix::fs::PermissionsExt;
            if std::fs::metadata(root.join(name))?.permissions().mode() & 0o111 == 0 {
                return Err(anyhow!(
                    "local Codex package file {name} is not executable; reinstall the CLI or use the standalone installer"
                ));
            }
        }
    }
    Ok(())
}

fn platform_target() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("linux", "aarch64") if cfg!(target_env = "gnu") => Ok("aarch64-unknown-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-musl"),
        ("linux", "x86_64") if cfg!(target_env = "gnu") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("windows", "aarch64") => Ok("aarch64-pc-windows-msvc"),
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc"),
        (os, arch) => Err(anyhow!("unsupported packaged daemon platform {os}/{arch}")),
    }
}

#[cfg(windows)]
#[path = "prepare_install_windows.rs"]
mod windows;

#[cfg(test)]
#[path = "prepare_install_tests.rs"]
mod tests;
