//! Enrollment contributes stable authorization, rather than repeating policy on mail.

use std::sync::Weak;

use codex_extension_api::ContextContributor;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::PreviousWorldStateSection;
use codex_extension_api::RenderedWorldStateFragment;
use codex_extension_api::WorldStateContributionInput;
use codex_extension_api::WorldStateSectionContribution;
use codex_extension_api::input_text;
use codex_protocol::ThreadId;
use serde_json::Value;

use crate::TeamHostService;

const START: &str = "<team_authorization>";
const END: &str = "</team_authorization>";
const AUTHORIZED: &str = "Team communication, member management, and Board cleanup through team tools are authorized without per-action confirmation. Models, execution controls, and approvals remain user-owned.";
const REVOKED: &str =
    "Team membership has ended. The previous team authorization no longer applies.";

pub(crate) struct TeamContext(pub(crate) Weak<TeamHostService>);

impl ContextContributor for TeamContext {
    fn contribute_world_state<'a>(
        &'a self,
        input: WorldStateContributionInput<'a>,
    ) -> ExtensionFuture<'a, Vec<WorldStateSectionContribution>> {
        let root = self
            .0
            .upgrade()
            .and_then(|service| service.gate.root(input.thread_id));
        let previous = input
            .previous_world_state
            .and_then(|state| state.get("team_authorization"))
            .cloned();
        Box::pin(async move { vec![authorization_section(root, previous)] })
    }
}

fn authorization_section(
    root: Option<ThreadId>,
    stored: Option<Value>,
) -> WorldStateSectionContribution {
    // Keep a non-null revocation snapshot: null deletes world-state metadata.
    let current = root.map_or(Value::Bool(false), |root| Value::String(root.to_string()));
    let previously_enrolled = stored.is_some();
    let retained_text = match stored {
        Some(Value::Bool(false)) => REVOKED,
        _ => AUTHORIZED,
    };
    WorldStateSectionContribution::new("team_authorization", move |previous| {
        let changed = match previous {
            PreviousWorldStateSection::Known(previous) => previous != &current,
            PreviousWorldStateSection::Absent => root.is_some() || previously_enrolled,
            PreviousWorldStateSection::Unknown => true,
        };
        if root.is_none()
            && !previously_enrolled
            && matches!(previous, PreviousWorldStateSection::Absent)
        {
            return (None, None);
        }
        let fragment = changed.then(|| {
            RenderedWorldStateFragment::new(
                "developer",
                (START, END),
                input_text(if root.is_none() { REVOKED } else { AUTHORIZED }),
            )
        });
        (Some(current.clone()), fragment)
    })
    .with_retained_fragment_matcher(move |role, text| {
        role == "developer"
            && text.trim_start().starts_with(START)
            && text.trim_end().ends_with(END)
            // Match the stored state, so changes can revoke it and compaction
            // cannot mistake an older grant for a retained revocation.
            && text.contains(retained_text)
    })
    .with_legacy_matcher(|role, text| {
        role == "developer"
            && text.trim_start().starts_with(START)
            && text.trim_end().ends_with(END)
            && text.contains(AUTHORIZED)
    })
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
