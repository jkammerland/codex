//! A row picker changes its explicit target without borrowing the foreground task's settings.

use super::*;
use crate::app::agents_hosts_models::ModelAction;
use crate::app::agents_hosts_models::ModelPickerContext;

const TARGET_MODEL_LOADING_VIEW_ID: &str = "agents-model-loading";

impl ChatWidget {
    pub(crate) fn begin_agents_model_popup(&mut self, request_id: uuid::Uuid) {
        self.bottom_pane
            .dismiss_view_by_id(TARGET_MODEL_LOADING_VIEW_ID);
        self.agents_model_picker = None;
        self.agents_model_load_request_id = Some(request_id);
        self.show_selection_view(SelectionViewParams {
            view_id: Some(TARGET_MODEL_LOADING_VIEW_ID),
            title: Some("Loading task models".into()),
            subtitle: Some("Fetching choices from the owning machine. Esc cancels.".into()),
            on_cancel: Some(Box::new(move |sender| {
                sender.unscoped().send(AppEvent::AgentsHostModel(Box::new(
                    ModelAction::CancelRequest(request_id),
                )))
            })),
            ..Default::default()
        });
    }

    pub(crate) fn cancel_agents_model_loading(&mut self, request_id: uuid::Uuid) {
        if self.agents_model_load_request_id == Some(request_id) {
            self.agents_model_load_request_id = None;
            self.bottom_pane
                .dismiss_view_by_id(TARGET_MODEL_LOADING_VIEW_ID);
        }
    }

    pub(crate) fn finish_agents_model_loading(&mut self, request_id: uuid::Uuid) -> bool {
        if self.agents_model_load_request_id != Some(request_id) {
            return false;
        }
        let present = self.bottom_pane.active_view_id() == Some(TARGET_MODEL_LOADING_VIEW_ID);
        self.cancel_agents_model_loading(request_id);
        present
    }

    pub(crate) fn clear_closed_agents_model_popup(&mut self, context: &ModelPickerContext) {
        if self
            .agents_model_picker
            .as_ref()
            .is_some_and(|current| current.request_id == context.request_id)
            && self.bottom_pane.no_modal_or_popup_active()
        {
            self.agents_model_picker = None;
        }
    }

    pub(super) fn target_model_cancel_action(&self) -> Option<SelectionAction> {
        let context = self.agents_model_picker.clone()?;
        Some(Box::new(move |sender| {
            sender
                .unscoped()
                .send(AppEvent::AgentsHostModel(Box::new(ModelAction::Dismiss(
                    context.clone(),
                ))))
        }))
    }

    pub(crate) fn open_agents_model_popup(&mut self, context: ModelPickerContext) {
        if !context.presets.iter().any(|preset| preset.show_in_picker) {
            self.agents_model_picker = None;
            self.add_info_message(
                "This machine has no models available in its picker.".into(),
                /*hint*/ None,
            );
            return;
        }
        let presets = context.presets.clone();
        self.agents_model_picker = Some(context);
        self.open_model_popup_with_presets(presets);
    }

    pub(super) fn picker_models(&self) -> Vec<ModelPreset> {
        self.agents_model_picker.as_ref().map_or_else(
            || self.model_catalog.try_list_models().unwrap_or_default(),
            |context| context.presets.clone(),
        )
    }

    pub(super) fn picker_current_model(&self) -> &str {
        self.agents_model_picker.as_ref().map_or_else(
            || self.current_model(),
            |context| context.current_model.as_str(),
        )
    }

    pub(super) fn picker_current_effort(&self) -> Option<ReasoningEffortConfig> {
        self.agents_model_picker.as_ref().map_or_else(
            || self.effective_reasoning_effort(),
            |context| context.current_effort.clone(),
        )
    }

    pub(super) fn target_model_selection_actions(
        &self,
        model: String,
        effort: Option<ReasoningEffortConfig>,
    ) -> Option<Vec<SelectionAction>> {
        let context = self.agents_model_picker.clone()?;
        Some(vec![Box::new(move |sender| {
            sender
                .unscoped()
                .send(AppEvent::AgentsHostModel(Box::new(ModelAction::Apply {
                    context: context.clone(),
                    model: model.clone(),
                    effort: effort.clone(),
                })));
        })])
    }
}
