//! Explicit ownership of model choices, independent of their resolved values.

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum TeamSelectionOrigin {
    Inherited,
    Configured,
    Role,
    Explicit,
    Default,
    /// Old persisted settings do not establish how their values were selected.
    #[default]
    Legacy,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum TeamSelection {
    #[default]
    Keep,
    Default,
    Value {
        value: String,
    },
    Inherit,
}

impl TeamSelection {
    pub fn origin(&self) -> Option<TeamSelectionOrigin> {
        match self {
            Self::Keep => None,
            Self::Default => Some(TeamSelectionOrigin::Default),
            Self::Value { .. } => Some(TeamSelectionOrigin::Explicit),
            Self::Inherit => Some(TeamSelectionOrigin::Inherited),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamModelSelection {
    #[ts(type = "number")]
    pub revision: i64,
    pub model: TeamSelection,
    pub effort: TeamSelection,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamModelPolicy {
    pub model_origin: TeamSelectionOrigin,
    pub effort_origin: TeamSelectionOrigin,
    pub selection: Option<TeamModelSelection>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub superseded_revision: Option<i64>,
}

impl TeamModelPolicy {
    /// A same-value choice still changes ownership. Revisions identify intent, not value equality.
    pub fn apply(&mut self, selection: &TeamModelSelection) -> Result<(), &'static str> {
        if self
            .superseded_revision
            .is_some_and(|revision| selection.revision <= revision)
        {
            return Err("model selection was superseded by a local edit");
        }
        if let Some(applied) = &self.selection {
            if selection.revision < applied.revision {
                return Err("model selection revision is stale");
            }
            if selection.revision == applied.revision {
                return if selection == applied {
                    Ok(())
                } else {
                    Err("model selection revision was reused with different policy")
                };
            }
        }
        if let Some(origin) = selection.model.origin() {
            self.model_origin = origin;
        }
        if let Some(origin) = selection.effort.origin() {
            self.effort_origin = origin;
        }
        self.selection = Some(selection.clone());
        Ok(())
    }
}

#[cfg(test)]
#[path = "team_model_tests.rs"]
mod tests;
