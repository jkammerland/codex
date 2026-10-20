use std::future::Future;
use std::io;
use std::process::Output;
use std::process::Stdio;
use std::time::Duration;

use codex_protocol::shell_environment::scrub_non_inheritable_env_vars;
#[cfg(windows)]
use codex_utils_pty::JobObject;
#[cfg(unix)]
use codex_utils_pty::process_group::kill_process_group;
use futures::FutureExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::process::Child;
use tokio::process::Command;
use tokio::time::timeout;

struct KillGitProcessTreeOnDrop {
    #[cfg(unix)]
    process_id: u32,
    #[cfg(windows)]
    job: Option<JobObject>,
    #[cfg(unix)]
    armed: bool,
}

#[cfg(unix)]
impl Drop for KillGitProcessTreeOnDrop {
    fn drop(&mut self) {
        if self.armed {
            let _ = kill_process_group(self.process_id);
        }
    }
}

fn spawn_git_command(command: &mut Command) -> Option<(Child, KillGitProcessTreeOnDrop)> {
    scrub_non_inheritable_env_vars(command.as_std_mut());
    #[cfg(unix)]
    command.process_group(0);
    command.kill_on_drop(true);

    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(windows)]
    let (child, job) = JobObject::spawn_background(command).ok()?;
    #[cfg(not(windows))]
    let child = command.spawn().ok()?;

    let process_tree = KillGitProcessTreeOnDrop {
        #[cfg(unix)]
        process_id: child.id()?,
        #[cfg(windows)]
        job,
        #[cfg(unix)]
        armed: true,
    };

    Some((child, process_tree))
}

async fn wait_for_git_command_with_timeout_output(
    child: Child,
    process_tree: KillGitProcessTreeOnDrop,
    timeout_duration: Duration,
) -> Option<Output> {
    #[cfg(unix)]
    let mut process_tree = process_tree;

    let result = timeout(timeout_duration, child.wait_with_output()).await;

    match result {
        Ok(Ok(output)) => {
            #[cfg(windows)]
            if let Some(job) = &process_tree.job {
                job.preserve_descendants().ok()?;
            }

            #[cfg(unix)]
            {
                process_tree.armed = false;
            }
            Some(output)
        }
        _ => None,
    }
}

pub(crate) async fn run_git_command_with_timeout_output(
    command: &mut Command,
    timeout_duration: Duration,
) -> Option<Output> {
    let (child, process_tree) = spawn_git_command(command)?;
    wait_for_git_command_with_timeout_output(child, process_tree, timeout_duration).await
}

/// Runs an owned Git process tree, draining its direct child and output pipes before return.
///
/// A ready cancellation skips spawning. Later cancellation returns `Interrupted`, and timeout
/// returns `TimedOut`, after cleanup. Windows containment is required for this helper. Callers
/// must await it through cancellation instead of dropping its future to observe cleanup.
pub async fn run_git_command_with_timeout_and_cancellation_output(
    command: &mut Command,
    timeout_duration: Duration,
    cancelled: impl Future<Output = ()> + Send,
) -> io::Result<Output> {
    tokio::pin!(cancelled);
    if cancelled.as_mut().now_or_never().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Git command cancelled",
        ));
    }

    scrub_non_inheritable_env_vars(command.as_std_mut());
    #[cfg(unix)]
    command.process_group(0);
    command
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(windows)]
    let job = JobObject::create_without_breakaway()?;
    #[cfg(windows)]
    let child = match job.spawn_contained_owned(command).await {
        Ok(child) => child,
        Err(error) => {
            tokio::task::spawn_blocking(move || job.terminate_and_wait())
                .await
                .map_err(io::Error::other)??;
            return Err(error);
        }
    };
    #[cfg(not(windows))]
    let mut child = command.spawn()?;
    let process_tree = KillGitProcessTreeOnDrop {
        #[cfg(unix)]
        process_id: child
            .id()
            .ok_or_else(|| io::Error::other("missing Git process id"))?,
        #[cfg(unix)]
        armed: true,
        #[cfg(windows)]
        job: Some(job),
    };

    #[cfg(windows)]
    {
        wait_for_owned_windows_git_command(child, process_tree, timeout_duration, cancelled).await
    }
    #[cfg(unix)]
    {
        let mut process_tree = process_tree;

        // Keep the Unix group leader unreaped while descendants still hold output pipes.
        // Its numeric group ID cannot be reused before cancellation has signalled the group.
        let output = async {
            let (stdout, stderr) = tokio::join!(
                read_owned_pipe(child.stdout.take()),
                read_owned_pipe(child.stderr.take()),
            );
            (child.wait().await, stdout, stderr)
        };
        tokio::pin!(output);
        let stop = tokio::select! {
            biased;
            () = &mut cancelled => Err(io::ErrorKind::Interrupted),
            result = &mut output => Ok(result),
            () = tokio::time::sleep(timeout_duration) => Err(io::ErrorKind::TimedOut),
        };
        match stop {
            Ok((status, stdout, stderr)) => {
                // The wait has reaped the leader. Never signal its numeric ID after this point,
                // including on wait errors; successful Unix commands preserve existing semantics.
                process_tree.armed = false;
                Ok(Output {
                    status: status?,
                    stdout: stdout?,
                    stderr: stderr?,
                })
            }
            Err(reason) => {
                // Retain the child and both readers until termination releases all pipe ends.
                terminate_owned_git_process_tree(process_tree).await?;
                let (status, stdout, stderr) = output.await;
                status?;
                stdout?;
                stderr?;
                let message = match reason {
                    io::ErrorKind::Interrupted => "Git command cancelled",
                    _ => "Git command timed out",
                };
                Err(io::Error::new(reason, message))
            }
        }
    }
}

#[cfg(windows)]
async fn wait_for_owned_windows_git_command(
    mut child: Child,
    process_tree: KillGitProcessTreeOnDrop,
    timeout_duration: Duration,
    cancelled: impl Future<Output = ()>,
) -> io::Result<Output> {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let finish = async {
        tokio::pin!(cancelled);
        let stop = tokio::select! {
            biased;
            () = &mut cancelled => Err(io::ErrorKind::Interrupted),
            status = child.wait() => Ok(status),
            () = tokio::time::sleep(timeout_duration) => Err(io::ErrorKind::TimedOut),
        };

        // The job handle retains process identity after the direct child exits. Terminate
        // remaining members before waiting for EOF: descendants may retain inherited pipes.
        terminate_owned_git_process_tree(process_tree).await?;
        match stop {
            Ok(status) => status.map(|status| (status, None)),
            Err(reason) => child.wait().await.map(|status| (status, Some(reason))),
        }
    };
    let (finish, stdout, stderr) =
        tokio::join!(finish, read_owned_pipe(stdout), read_owned_pipe(stderr),);
    let (status, reason) = finish?;
    let stdout = stdout?;
    let stderr = stderr?;
    if let Some(reason) = reason {
        let message = match reason {
            io::ErrorKind::Interrupted => "Git command cancelled",
            _ => "Git command timed out",
        };
        return Err(io::Error::new(reason, message));
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

async fn read_owned_pipe(pipe: Option<impl AsyncRead + Unpin>) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    if let Some(mut pipe) = pipe {
        pipe.read_to_end(&mut output).await?;
    }
    Ok(output)
}

async fn terminate_owned_git_process_tree(
    mut process_tree: KillGitProcessTreeOnDrop,
) -> io::Result<()> {
    #[cfg(unix)]
    {
        kill_process_group(process_tree.process_id)?;
        process_tree.armed = false;
        Ok(())
    }
    #[cfg(windows)]
    {
        let job = process_tree
            .job
            .take()
            .ok_or_else(|| io::Error::other("Git process is not contained"))?;
        tokio::task::spawn_blocking(move || job.terminate_and_wait())
            .await
            .map_err(io::Error::other)?
    }
}

#[cfg(test)]
#[path = "git_process_tests.rs"]
mod tests;
