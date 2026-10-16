//! Pooled-host creation lets the owning server choose runtime defaults.

use super::*;

impl AppServerSession {
    pub(crate) async fn start_thread_with_host_defaults(
        &mut self,
        local_settings: &LocalSettings,
        config: &Config,
        cwd: Option<&std::path::Path>,
    ) -> Result<AppServerStartedThread> {
        let params = host_start_params(cwd, self.history_support);
        let (response, history_support, _) = request_thread_start_with_history_fallback(
            &self.request_handle(),
            self.next_request_id(),
            params,
        )
        .await?;
        self.history_support = history_support;
        started_thread_from_start_response(
            response,
            local_settings,
            config,
            ThreadParamsMode::Remote,
        )
        .await
    }

    pub(crate) async fn fork_thread_with_host_defaults(
        &mut self,
        local_settings: &LocalSettings,
        config: &Config,
        thread_id: ThreadId,
        cwd: Option<&std::path::Path>,
    ) -> Result<AppServerStartedThread> {
        let mut params = host_fork_params(thread_id, cwd, self.history_support);
        let request_id = self.next_request_id();
        let mut response: ThreadForkResponse = match self
            .client
            .request_typed(ClientRequest::ThreadFork {
                request_id,
                params: params.clone(),
            })
            .await
        {
            Ok(response) => response,
            Err(TypedRequestError::Server { source, .. })
                if params.exclude_turns && is_history_pagination_unsupported(&source) =>
            {
                self.history_support = ThreadHistorySupport::LegacyOnly;
                params.exclude_turns = false;
                let request_id = self.next_request_id();
                self.client
                    .request_typed(ClientRequest::ThreadFork { request_id, params })
                    .await?
            }
            Err(error) => return Err(error.into()),
        };
        self.hydrate_initial_thread_history(
            &mut response.thread,
            None,
            None,
            Some(config),
            Some(local_settings),
            HistoryHydrationScope::Initial,
        )
        .await?;
        started_thread_from_fork_response(
            response,
            local_settings,
            config,
            ThreadParamsMode::Remote,
        )
        .await
    }
}

fn host_start_params(
    cwd: Option<&std::path::Path>,
    history: ThreadHistorySupport,
) -> ThreadStartParams {
    ThreadStartParams {
        cwd: cwd.map(|path| path.to_string_lossy().into_owned()),
        history_mode: (history == ThreadHistorySupport::Paginated)
            .then_some(ThreadHistoryMode::Paginated),
        ..Default::default()
    }
}

fn host_fork_params(
    thread_id: ThreadId,
    cwd: Option<&std::path::Path>,
    history: ThreadHistorySupport,
) -> ThreadForkParams {
    ThreadForkParams {
        thread_id: thread_id.to_string(),
        cwd: cwd.map(|path| path.to_string_lossy().into_owned()),
        exclude_turns: history == ThreadHistorySupport::Paginated,
        ..Default::default()
    }
}
