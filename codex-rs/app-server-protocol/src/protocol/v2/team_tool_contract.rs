//! One compact native/MCP contract, with complete variants for code-mode discovery.

use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;

use super::TeamAddress;

pub const TEAM_TOOL_DESCRIPTION: &str = "Collaborate with enrolled teammates and manage members or Board cleanup. Send defaults to request/nextTurn; reply preserves its original destination. Board reads default to unread pages; acknowledge explicitly. Mentions and Wake subscriptions deliver notifications; posting does not subscribe. Results distinguish queued intent from confirmed state. Use status for progress; delivery is event-driven.";

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(untagged)]
#[ts(export_to = "v2/")]
pub enum TeamAgentTarget {
    Address(TeamAddress),
    Alias(String),
}

impl From<TeamAddress> for TeamAgentTarget {
    fn from(address: TeamAddress) -> Self {
        Self::Address(address)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamAgentMentionParams {
    pub target: TeamAgentTarget,
    #[ts(optional = nullable)]
    pub display: Option<String>,
}

/// Construct a complete object before exposing it as a tagged-union alternative.
pub fn team_tool_object(properties: Value, required: &[&str]) -> Value {
    json!({"type":"object", "properties":properties, "required":required, "additionalProperties":false})
}

/// Distribute inherited fields into small nested unions. Code-mode renders each
/// alternative independently, so sibling properties cannot carry required fields.
pub fn team_tool_complete_schema(schema: Value) -> Value {
    match schema {
        Value::Object(mut object) => {
            if let Some(Value::Array(alternatives)) = object.remove("oneOf") {
                let schema_type = object.get("type").cloned();
                let base = Value::Object(object);
                let mut complete = json!({"oneOf":alternatives.into_iter().map(|alternative| {
                    let mut merged = base.clone();
                    merge_schema(&mut merged, alternative);
                    team_tool_complete_schema(merged)
                }).collect::<Vec<_>>()});
                if let Some(schema_type) = schema_type {
                    complete["type"] = schema_type;
                }
                return complete;
            }
            for value in object.values_mut() {
                *value = team_tool_complete_schema(value.take());
            }
            Value::Object(object)
        }
        Value::Array(values) => {
            Value::Array(values.into_iter().map(team_tool_complete_schema).collect())
        }
        value => value,
    }
}

fn merge_schema(base: &mut Value, refinement: Value) {
    match (base, refinement) {
        (Value::Object(base), Value::Object(refinement)) => {
            for (key, value) in refinement {
                match (key.as_str(), base.get_mut(&key), &value) {
                    ("required", Some(Value::Array(existing)), Value::Array(required)) => {
                        for field in required {
                            if !existing.contains(field) {
                                existing.push(field.clone());
                            }
                        }
                    }
                    ("properties", Some(Value::Object(properties)), Value::Object(refinements)) => {
                        for (name, refinement) in refinements {
                            if let Some(property) = properties.get_mut(name) {
                                merge_schema(property, refinement.clone());
                            } else {
                                properties.insert(name.clone(), refinement.clone());
                            }
                        }
                    }
                    _ => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, refinement) => *base = refinement,
    }
}

fn address_schema() -> Value {
    team_tool_object(
        json!({"machineId":{"type":"string"},"serverId":{"type":"string"},"threadId":{"type":"string"}}),
        &["machineId", "serverId", "threadId"],
    )
}

fn target_schema() -> Value {
    json!({"oneOf":[{"type":"string","description":"Exact enrolled alias."},address_schema()]})
}

fn action(name: &str, mut properties: Value, required: &[&str]) -> Value {
    properties["type"] = json!({"type":"string","const":name});
    let mut fields = vec!["type"];
    fields.extend(required);
    team_tool_object(properties, &fields)
}

pub fn team_agent_tool_schema() -> Value {
    let cursor = json!({"type":"string"});
    let limit = json!({"type":"integer","minimum":1,"maximum":1000,"default":20});
    let body = json!({"type":"string"});
    let delivery = json!({"type":"string","enum":["nextTurn","safeBoundary"],"default":"nextTurn"});
    json!({"type":"object","oneOf":[
        action("directory", json!({"query":{"type":"string","description":"Filter alias, address, environment or capability."},"details":{"type":"boolean","default":false},"cursor":cursor,"limit":limit}), &[]),
        action("inbox", json!({"cursor":cursor,"limit":limit}), &[]),
        action("send", json!({"recipient":target_schema(),"body":body,"kind":{"type":"string","enum":["request","reply","note"],"default":"request"},"replyTo":{"type":"string"},"delivery":delivery}), &["recipient","body"]),
        action("reply", json!({"messageId":{"type":"string"},"body":body,"delivery":delivery}), &["messageId","body"]),
        action("publish", json!({"body":body,"topic":{"type":"string","description":"Required for a new root post."},"conversationId":{"type":"string","description":"Set to reply in an existing conversation."},"replyTo":{"type":"string","description":"Optional specific message in that conversation."},"mentions":{"type":"array","items":team_tool_object(json!({"target":target_schema(),"display":{"type":"string","description":"Exact @label in body; defaults to @alias. Use target user for a human mention."}}), &["target"])}}), &["body"]),
        action("boardAction", json!({"request":super::team_board_tool_schema()}), &["request"]),
        action("member", json!({"request":super::team_member_action_schema()}), &["request"]),
        action("cleanup", json!({"request":super::team_cleanup_schema()}), &["request"]),
        action("messageStatus", json!({"query":team_tool_object(json!({"messageId":{"type":"string"},"cursor":cursor,"limit":limit}), &[])}), &["query"]),
        action("explainAction", json!({"query":team_tool_object(json!({"action":{"type":"string","enum":["enroll","queueMessage","processMessages","post","subscribe","selectModel","pause","resume","stop","leave"]},"target":address_schema()}), &["action"])}), &["query"])
    ]})
}

/// Common fields stay discoverable without repeating the complete legacy record
/// payloads. Structured entries retain additional operation-specific fields.
pub fn team_agent_output_schema() -> Value {
    let address = address_schema();
    let entry = json!({"type":"object","properties":{
        "id":{"type":"string"},"appendSeq":{"type":"integer"},"body":{"type":"string"},
        "author":{"type":"object"},"replyTo":{"type":["string","null"]},"createdAt":{"type":"integer"}
    },"additionalProperties":true});
    let page = json!({"type":"object","properties":{
        "revision":{"type":"integer"},"snapshotThrough":{"type":"integer"},"conversationId":{"type":"string"},
        "topic":{"type":["string","null"]},"root":{"type":["object","null"]},"readSeq":{"type":"integer"},"unreadCount":{"type":"integer"},
        "subscriptionMode":{"type":"string","enum":["off","watch","wake"]},"topicSubscriptionMode":{"type":"string","enum":["off","watch","wake"]},
        "data":{"type":"array","items":entry},"nextCursor":{"type":["string","null"]},"acknowledgementToken":{"type":["string","null"]}
    },"required":["conversationId","data","nextCursor","acknowledgementToken"],"additionalProperties":true});
    json!({"type":"object","properties":{
        "type":{"type":"string"},"id":{"type":"string"},"operationId":{"type":"string"},"state":{"type":"string"},"revision":{"type":"integer"},
        "waitingReason":{"type":["string","null"]},"result":{"type":["object","null"]},"freshness":{"type":"string"},
        "route":{"type":"object","properties":{"type":{"type":"string","enum":["direct","board"]},"recipient":address,"conversationId":{"type":["string","null"]},"topic":{"type":["string","null"]}},"additionalProperties":true},
        "delivery":{"type":"string","enum":["nextTurn","safeBoundary"]},
        "subscriptionMode":{"type":"string","enum":["off","watch","wake"]},
        "topicSubscriptionMode":{"type":"string","enum":["off","watch","wake"]},
        "notificationPolicy":{"type":"string"},"lastSynchronizedAt":{"type":["integer","null"]},
        "data":{"type":"array","items":{"oneOf":[{"type":"string"},{"type":"object","properties":{"id":{"type":"string"},"alias":{"type":"string"},"address":address,"body":{"type":"string"},"unreadCount":{"type":"integer"}},"additionalProperties":true}]}},
        "nextCursor":{"type":["string","null"]},"page":page
    },"additionalProperties":true})
}

#[cfg(test)]
#[path = "team_tool_contract_tests.rs"]
mod tests;
