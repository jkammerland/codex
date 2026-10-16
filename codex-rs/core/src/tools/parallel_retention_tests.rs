//! Exercise the original dispatch handle across cancellation at both phases.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use super::*;
use crate::mcp_tool_call::dispatch;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::tools::registry::ToolRegistry;
use crate::tools::router::ToolRouter;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_extension_api::ExecutionControl;
use codex_extension_api::ExecutionMode;
use codex_extension_api::RetainedToolResults;
use codex_protocol::openai_models::ToolMode;
use pretty_assertions::assert_eq;
use tokio::sync::oneshot;

#[derive(Debug, PartialEq, Eq)]
enum LaunchStage {
    WaitingForAdmission,
    Dispatched,
}

struct RetainedLaunch {
    prepared: Mutex<Option<oneshot::Sender<()>>>,
    admit: Mutex<Option<oneshot::Receiver<()>>>,
    stage: Mutex<Option<oneshot::Sender<LaunchStage>>>,
    result: Mutex<Option<oneshot::Receiver<String>>>,
    observed: Mutex<Option<oneshot::Sender<String>>>,
    effects: Arc<AtomicUsize>,
}

impl ToolExecutor<ToolInvocation> for RetainedLaunch {
    fn tool_name(&self) -> codex_tools::ToolName {
        codex_tools::ToolName::plain("tracked_launch")
    }

    fn spec(&self) -> codex_tools::ToolSpec {
        codex_tools::ToolSpec::Function(codex_tools::ResponsesApiTool {
            name: "tracked_launch".into(),
            description: "Owned external launch fixture".into(),
            strict: false,
            defer_loading: None,
            parameters: codex_tools::JsonSchema::default(),
            output_schema: None,
        })
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            invocation
                .turn
                .extension_data
                .get_or_init(RetainedToolResults::default)
                .retain(invocation.call_id.clone());
            self.prepared
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(())
                .unwrap();
            let admit = self.admit.lock().unwrap().take().unwrap();
            admit.await.unwrap();
            let mut admission = Box::pin(dispatch::enter_execution(
                &invocation.session,
                &invocation.cancellation_token,
            ));
            let mut stage = self.stage.lock().unwrap().take();
            let _execution = match futures::poll!(admission.as_mut()) {
                std::task::Poll::Ready(result) => result,
                std::task::Poll::Pending => {
                    stage
                        .take()
                        .unwrap()
                        .send(LaunchStage::WaitingForAdmission)
                        .unwrap();
                    admission.await
                }
            }
            .map_err(FunctionCallError::RespondToModel)?;
            dispatch::begin_dispatch(
                &invocation.turn,
                &invocation.call_id,
                &invocation.cancellation_token,
            )
            .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
            self.effects.fetch_add(1, Ordering::SeqCst);
            if let Some(stage) = stage {
                stage.send(LaunchStage::Dispatched).unwrap();
            }
            // The original external response is independent of local turn
            // cancellation once dispatch has acquired its durable owner.
            let result = self.result.lock().unwrap().take().unwrap();
            let handle = result.await.unwrap();
            self.observed
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(handle.clone())
                .unwrap();
            Ok(Box::new(FunctionToolOutput::from_text(handle, Some(true)))
                as Box<dyn crate::tools::context::ToolOutput>)
        })
    }
}

impl CoreToolRuntime for RetainedLaunch {}

async fn retained_launch_cancellation(dispatched: bool) -> anyhow::Result<()> {
    let (mut session, turn) = crate::session::tests::make_session_and_context().await;
    let control = ExecutionControl::default();
    control.apply(session.thread_id, 1, ExecutionMode::Running);
    let mut extensions = codex_extension_api::ExtensionRegistryBuilder::new();
    extensions.execution_control(control.clone());
    session.services.extensions = Arc::new(extensions.build());
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let (prepared_tx, prepared_rx) = oneshot::channel();
    let (admit_tx, admit_rx) = oneshot::channel();
    let (stage_tx, stage_rx) = oneshot::channel();
    let (result_tx, result_rx) = oneshot::channel();
    let (observed_tx, observed_rx) = oneshot::channel();
    let effects = Arc::new(AtomicUsize::new(0));
    let handler: Arc<dyn CoreToolRuntime> = Arc::new(RetainedLaunch {
        prepared: Mutex::new(Some(prepared_tx)),
        admit: Mutex::new(Some(admit_rx)),
        stage: Mutex::new(Some(stage_tx)),
        result: Mutex::new(Some(result_rx)),
        observed: Mutex::new(Some(observed_tx)),
        effects: Arc::clone(&effects),
    });
    let router = Arc::new(ToolRouter::from_parts(
        ToolRegistry::from_tools([handler]),
        Vec::new(),
        ToolMode::Direct,
        BTreeMap::new(),
        /*tool_namespaces_info*/ None,
        &[],
    ));
    let step = StepContext::for_test(Arc::clone(&turn)).with_tool_router_for_test(router);
    let runtime = ToolCallRuntime::new(
        Arc::clone(&session),
        step,
        Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
    );
    let cancellation = CancellationToken::new();
    let response = tokio::spawn(runtime.handle_tool_call(
        ToolCall {
            tool_name: codex_tools::ToolName::plain("tracked_launch"),
            call_id: "launch".into(),
            payload: ToolPayload::Function {
                arguments: "{}".into(),
            },
            encrypted_function_args: None,
        },
        cancellation.clone(),
    ));
    prepared_rx.await?;
    if !dispatched {
        control.apply(session.thread_id, 2, ExecutionMode::Paused);
    }
    admit_tx.send(()).unwrap();
    assert_eq!(
        stage_rx.await?,
        if dispatched {
            LaunchStage::Dispatched
        } else {
            LaunchStage::WaitingForAdmission
        }
    );
    control.apply(session.thread_id, 3, ExecutionMode::Stopped);
    cancellation.cancel();
    tokio::time::timeout(Duration::from_secs(/*secs*/ 10), response).await???;
    assert_eq!(effects.load(Ordering::SeqCst), usize::from(dispatched));
    control.apply(session.thread_id, 4, ExecutionMode::Running);
    if dispatched {
        let mut changed = control.subscribe();
        result_tx.send("original-job-handle".into()).unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(/*secs*/ 10), observed_rx).await??,
            "original-job-handle"
        );
        tokio::time::timeout(Duration::from_secs(/*secs*/ 10), async {
            while control.snapshot(session.thread_id).unwrap().in_flight != 0 {
                changed.changed().await?;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
    } else {
        // The cancelled pending dispatch has been joined, so it cannot start
        // later and its external response receiver no longer has an owner.
        assert!(result_tx.send("must-not-launch".into()).is_err());
        assert_eq!(control.snapshot(session.thread_id).unwrap().in_flight, 0);
    }
    assert_eq!(effects.load(Ordering::SeqCst), usize::from(dispatched));
    Ok(())
}

#[tokio::test]
async fn stop_cancels_a_durable_launch_waiting_on_final_admission_before_resume()
-> anyhow::Result<()> {
    retained_launch_cancellation(/*dispatched*/ false).await
}

#[tokio::test]
async fn stop_preserves_the_original_dispatched_launch_until_its_handle_returns()
-> anyhow::Result<()> {
    retained_launch_cancellation(/*dispatched*/ true).await
}
