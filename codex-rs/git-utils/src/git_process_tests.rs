use super::run_git_command_with_timeout_and_cancellation_output;
use super::spawn_git_command;
use super::wait_for_git_command_with_timeout_output;
use pretty_assertions::assert_eq;
use std::io;
use std::io::Read;
use std::io::Write;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
#[cfg(windows)]
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[derive(Clone, Copy)]
enum GitWrapperLifetime {
    WaitForChild,
    ExitBeforeTimeout,
}

async fn assert_timed_out_git_wrapper_does_not_leave_child_process_running(
    wrapper_lifetime: GitWrapperLifetime,
) {
    let temp_dir = tempfile::tempdir().expect("create temp dir");
    let child_pid_file = temp_dir.path().join("child.pid");
    let child_ready_file = temp_dir.path().join("child-ready");
    let release_child_file = temp_dir.path().join("release-child");
    let child_survived_file = temp_dir.path().join("child-survived");
    let release_wrapper_file = temp_dir.path().join("release-wrapper");
    #[cfg(unix)]
    let mut command = {
        let mut command = Command::new("/bin/sh");
        let wrapper_command = match wrapper_lifetime {
            GitWrapperLifetime::WaitForChild => {
                r#"( : > "$CHILD_READY_FILE"; while [ ! -f "$RELEASE_CHILD_FILE" ]; do sleep 0.01; done; sleep 1; : > "$CHILD_SURVIVED_FILE"; sleep 60 ) & child_pid=$!; printf '%s\n' "$child_pid" > "$CHILD_PID_FILE"; wait "$child_pid""#
            }
            GitWrapperLifetime::ExitBeforeTimeout => {
                r#"( : > "$CHILD_READY_FILE"; while [ ! -f "$RELEASE_CHILD_FILE" ]; do sleep 0.01; done; sleep 1; : > "$CHILD_SURVIVED_FILE"; sleep 60 ) & child_pid=$!; printf '%s\n' "$child_pid" > "$CHILD_PID_FILE"; while [ ! -f "$RELEASE_WRAPPER_FILE" ]; do sleep 0.01; done"#
            }
        };
        command.args(["-c", wrapper_command]);
        command
    };
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("powershell.exe");
        let child_command = "Set-Content -LiteralPath $env:CHILD_READY_FILE -Value ready; while (-not (Test-Path $env:RELEASE_CHILD_FILE)) { Start-Sleep -Milliseconds 25 }; Start-Sleep -Seconds 1; Set-Content -LiteralPath $env:CHILD_SURVIVED_FILE -Value survived; Start-Sleep -Seconds 60";
        let wrapper_command = match wrapper_lifetime {
            GitWrapperLifetime::WaitForChild => format!(
                "$child = Start-Process -FilePath powershell.exe -ArgumentList @('-NoProfile', '-NonInteractive', '-Command', '{child_command}') -PassThru -NoNewWindow; [System.IO.File]::WriteAllText($env:CHILD_PID_FILE, [string]$child.Id); Wait-Process -Id $child.Id"
            ),
            GitWrapperLifetime::ExitBeforeTimeout => format!(
                "$child = Start-Process -FilePath powershell.exe -ArgumentList @('-NoProfile', '-NonInteractive', '-Command', '{child_command}') -PassThru -NoNewWindow; [System.IO.File]::WriteAllText($env:CHILD_PID_FILE, [string]$child.Id); while (-not (Test-Path $env:RELEASE_WRAPPER_FILE)) {{ Start-Sleep -Milliseconds 25 }}"
            ),
        };
        command
            .args(["-NoProfile", "-NonInteractive", "-Command"])
            .arg(wrapper_command);
        command
    };
    command
        .env("CHILD_PID_FILE", &child_pid_file)
        .env("CHILD_READY_FILE", &child_ready_file)
        .env("RELEASE_CHILD_FILE", &release_child_file)
        .env("CHILD_SURVIVED_FILE", &child_survived_file)
        .env("RELEASE_WRAPPER_FILE", &release_wrapper_file);

    let (mut wrapper, process_tree) = spawn_git_command(&mut command).expect("spawn Git wrapper");
    let child_pid = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(child_pid) = std::fs::read_to_string(&child_pid_file)
                && !child_pid.trim().is_empty()
                && child_ready_file.exists()
            {
                break child_pid.trim().to_string();
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("wait for Git wrapper child readiness");

    if matches!(wrapper_lifetime, GitWrapperLifetime::ExitBeforeTimeout) {
        std::fs::write(&release_wrapper_file, "release").expect("release Git wrapper");
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if wrapper
                    .try_wait()
                    .expect("check Git wrapper state")
                    .is_some()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("wait for Git wrapper exit");
    }

    let output =
        wait_for_git_command_with_timeout_output(wrapper, process_tree, Duration::from_millis(100))
            .await;
    assert_eq!(output, None);

    std::fs::write(&release_child_file, "release").expect("release Git wrapper child");
    tokio::time::sleep(Duration::from_secs(3)).await;
    if !child_survived_file.exists() {
        return;
    }

    #[cfg(unix)]
    let _ = std::process::Command::new("kill")
        .args(["-KILL", &child_pid])
        .status();
    #[cfg(windows)]
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &child_pid, "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    panic!("Git wrapper child process {child_pid} survived timeout cleanup");
}

#[tokio::test]
async fn timed_out_git_wrapper_does_not_leave_child_process_running() {
    assert_timed_out_git_wrapper_does_not_leave_child_process_running(
        GitWrapperLifetime::WaitForChild,
    )
    .await;
}

#[tokio::test]
async fn timed_out_exited_git_wrapper_does_not_leave_child_process_running() {
    assert_timed_out_git_wrapper_does_not_leave_child_process_running(
        GitWrapperLifetime::ExitBeforeTimeout,
    )
    .await;
}

const OWNED_GIT_ROLE: &str = "CODEX_TEST_OWNED_GIT_ROLE";
const OWNED_GIT_ADDRESS: &str = "CODEX_TEST_OWNED_GIT_ADDRESS";
const OWNED_GIT_WRAPPER_RELEASE: &str = "CODEX_TEST_OWNED_GIT_WRAPPER_RELEASE";
const OWNED_GIT_DIRECTORY: &str = "CODEX_TEST_OWNED_GIT_DIRECTORY";
const OWNED_GIT_DETACHED_PIPES: &str = "CODEX_TEST_OWNED_GIT_DETACHED_PIPES";
const CANCELLED_GIT_TEST: &str =
    "git_process::tests::cancelled_git_command_waits_for_descendant_exit";
const FIXTURE_DEADLINE: Duration = Duration::from_secs(30);

// The same substantive test supplies bounded child roles, so an ordinary test run always
// executes the parent assertions instead of registering a fixture-only passing test.
fn run_owned_git_child_role(role: &str) -> io::Result<()> {
    match role {
        "wrapper" => {
            let mut command = std::process::Command::new(std::env::current_exe()?);
            command
                .args(["--exact", CANCELLED_GIT_TEST, "--nocapture"])
                .env(OWNED_GIT_ROLE, "descendant")
                .stdin(Stdio::null());
            if std::env::var_os(OWNED_GIT_DETACHED_PIPES).is_some() {
                command.stdout(Stdio::null()).stderr(Stdio::null());
            }
            let mut child = command.spawn()?;
            if let Ok(address) = std::env::var(OWNED_GIT_WRAPPER_RELEASE) {
                let mut release = std::net::TcpStream::connect(address)?;
                release.write_all(b"W")?;
                let mut message = [0];
                release.read_exact(&mut message)?;
                assert_eq!(message, *b"E");
                println!("wrapper exits successfully before descendant");
                return Ok(());
            }
            let status = child.wait()?;
            Err(io::Error::other(format!(
                "fixture descendant exited before cancellation: {status}"
            )))
        }
        "descendant" => {
            let directory = std::env::var_os(OWNED_GIT_DIRECTORY)
                .expect("descendant fixture directory is configured");
            std::env::set_current_dir(&directory)?;
            let mut options = std::fs::OpenOptions::new();
            options.create(true).truncate(true).read(true).write(true);
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                options.share_mode(0);
            }
            let _file = options.open("held.lock")?;
            println!("descendant holds stdout");
            eprintln!("descendant holds stderr");

            // Readiness follows resource acquisition. The parent never releases this gate;
            // only terminating this descendant can close its socket and file handles.
            let address =
                std::env::var(OWNED_GIT_ADDRESS).expect("descendant fixture address is configured");
            let mut connection = std::net::TcpStream::connect(address)?;
            connection.write_all(b"R")?;
            let mut release = [0];
            connection.read_exact(&mut release)?;
            Err(io::Error::other("descendant unexpectedly released"))
        }
        _ => Err(io::Error::other(format!("unexpected fixture role {role}"))),
    }
}

#[tokio::test]
async fn cancelled_git_command_waits_for_descendant_exit() -> io::Result<()> {
    let role = std::env::var(OWNED_GIT_ROLE).ok();
    if let Some(role @ ("wrapper" | "descendant")) = role.as_deref() {
        return run_owned_git_child_role(role);
    }

    #[cfg(windows)]
    if role.is_none() {
        // Native test runners can themselves own jobs. Exercise nested containment without
        // depending on the runner's policy or allowing an uncontained compatibility fallback.
        let outer_job = codex_utils_pty::JobObject::create_without_breakaway()?;
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args(["--exact", CANCELLED_GIT_TEST, "--nocapture"])
            .env(OWNED_GIT_ROLE, "test-root")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = tokio::time::timeout(
            FIXTURE_DEADLINE,
            outer_job.spawn_contained(&mut command)?.wait_with_output(),
        )
        .await
        .expect("nested-job cancellation regression completed")?;
        assert!(
            output.status.success(),
            "nested-job regression failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        return Ok(());
    }

    #[cfg(windows)]
    let detached_pipe_cases = [false, true];
    #[cfg(not(windows))]
    let detached_pipe_cases = [false];
    for detached_pipes in detached_pipe_cases {
        assert_owned_git_descendant_cleanup(detached_pipes, OwnedGitFinish::Cancel).await?;
    }
    Ok(())
}

enum OwnedGitFinish {
    Cancel,
    #[cfg(windows)]
    SuccessfulWrapperExit,
}

async fn assert_owned_git_descendant_cleanup(
    detached_pipes: bool,
    finish: OwnedGitFinish,
) -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    #[cfg(windows)]
    let wrapper_release = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["--exact", CANCELLED_GIT_TEST, "--nocapture"])
        .env(OWNED_GIT_ROLE, "wrapper")
        .env(OWNED_GIT_ADDRESS, listener.local_addr()?.to_string())
        .env(OWNED_GIT_DIRECTORY, directory.path())
        .env_remove(OWNED_GIT_WRAPPER_RELEASE);
    #[cfg(windows)]
    if matches!(finish, OwnedGitFinish::SuccessfulWrapperExit) {
        command.env(
            OWNED_GIT_WRAPPER_RELEASE,
            wrapper_release.local_addr()?.to_string(),
        );
    }
    if detached_pipes {
        command.env(OWNED_GIT_DETACHED_PIPES, "1");
    } else {
        command.env_remove(OWNED_GIT_DETACHED_PIPES);
    }
    let (cancel, cancellation) = tokio::sync::oneshot::channel::<()>();
    let running = tokio::spawn(async move {
        run_git_command_with_timeout_and_cancellation_output(
            &mut command,
            FIXTURE_DEADLINE,
            async {
                let _ = cancellation.await;
            },
        )
        .await
    });

    let (mut connection, _) = tokio::time::timeout(FIXTURE_DEADLINE, listener.accept())
        .await
        .expect("descendant connected after acquiring resources")?;
    let mut ready = [0];
    tokio::time::timeout(FIXTURE_DEADLINE, connection.read_exact(&mut ready))
        .await
        .expect("descendant reported readiness")?;
    assert_eq!(ready, *b"R");
    #[cfg(windows)]
    assert!(
        std::fs::remove_file(directory.path().join("held.lock")).is_err(),
        "the descendant must hold an exclusive file handle before cancellation"
    );

    match finish {
        OwnedGitFinish::Cancel => {
            cancel
                .send(())
                .expect("Git cancellation receiver remains live");
            let error = tokio::time::timeout(FIXTURE_DEADLINE, running)
                .await
                .expect("Git cancellation drained the process tree")
                .expect("Git cancellation task did not panic")
                .expect_err("cancellation reports Interrupted");
            assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        }
        #[cfg(windows)]
        OwnedGitFinish::SuccessfulWrapperExit => {
            let (mut release, _) = tokio::time::timeout(FIXTURE_DEADLINE, wrapper_release.accept())
                .await
                .expect("wrapper connected to its exit gate")?;
            let mut wrapper_ready = [0];
            release.read_exact(&mut wrapper_ready).await?;
            assert_eq!(wrapper_ready, *b"W");
            release.write_all(b"E").await?;
            let output = tokio::time::timeout(FIXTURE_DEADLINE, running)
                .await
                .expect("successful wrapper drained its remaining descendant")
                .expect("owned Git task did not panic")?;
            assert!(output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stdout)
                    .contains("wrapper exits successfully before descendant")
            );
        }
    }

    // Delete immediately, before any other awaited observation can hide a cleanup race.
    // On Windows this requires both the descendant's file and cwd handles to be gone.
    directory.close()?;
    let closed = tokio::time::timeout(FIXTURE_DEADLINE, connection.read(&mut ready))
        .await
        .expect("terminated descendant closed its readiness socket");
    match closed {
        Ok(bytes) => assert_eq!(bytes, 0),
        // Forced Windows process termination can reset TCP instead of sending FIN. This
        // observation follows successful process cleanup and immediate directory deletion.
        #[cfg(windows)]
        Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

#[cfg(windows)]
#[tokio::test]
async fn owned_git_command_cleans_descendants_after_successful_wrapper_exit() -> io::Result<()> {
    for detached_pipes in [false, true] {
        assert_owned_git_descendant_cleanup(detached_pipes, OwnedGitFinish::SuccessfulWrapperExit)
            .await?;
    }
    Ok(())
}

#[tokio::test]
async fn already_cancelled_git_command_does_not_spawn() {
    let directory = tempfile::tempdir().expect("create command fixture directory");
    let mut command = Command::new(directory.path().join("nonexistent-command"));
    let error = run_git_command_with_timeout_and_cancellation_output(
        &mut command,
        FIXTURE_DEADLINE,
        std::future::ready(()),
    )
    .await
    .expect_err("ready cancellation prevents even a spawn attempt");
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
}

#[tokio::test]
async fn owned_git_command_preserves_successful_output() -> io::Result<()> {
    if std::env::var(OWNED_GIT_ROLE).as_deref() == Ok("success") {
        println!("owned Git stdout");
        eprintln!("owned Git stderr");
        return Ok(());
    }
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args([
            "--exact",
            "git_process::tests::owned_git_command_preserves_successful_output",
            "--nocapture",
        ])
        .env(OWNED_GIT_ROLE, "success");
    let output = run_git_command_with_timeout_and_cancellation_output(
        &mut command,
        FIXTURE_DEADLINE,
        std::future::pending(),
    )
    .await?;
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("owned Git stdout"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("owned Git stderr"));
    Ok(())
}

#[tokio::test]
async fn owned_git_command_timeout_reports_timed_out() -> io::Result<()> {
    if std::env::var(OWNED_GIT_ROLE).as_deref() == Ok("timeout") {
        return std::future::pending().await;
    }
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args([
            "--exact",
            "git_process::tests::owned_git_command_timeout_reports_timed_out",
        ])
        .env(OWNED_GIT_ROLE, "timeout");
    let error = tokio::time::timeout(
        FIXTURE_DEADLINE,
        run_git_command_with_timeout_and_cancellation_output(
            &mut command,
            Duration::ZERO,
            std::future::pending(),
        ),
    )
    .await
    .expect("timeout drained its direct child and pipes")
    .expect_err("owned command reaches the timeout");
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    Ok(())
}
