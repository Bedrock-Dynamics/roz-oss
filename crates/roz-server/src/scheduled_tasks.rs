use std::collections::BTreeMap;

use roz_core::embodiment::binding::ControlInterfaceManifest;
use roz_core::phases::PhaseSpec;
use roz_core::tasks::DelegationScope;
use serde::{Deserialize, Serialize};
use tonic::Status;
use uuid::Uuid;

use crate::grpc::roz_v1::ScheduledTaskTemplate;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StoredScheduledTaskTemplate {
    pub prompt: String,
    pub environment_id: Uuid,
    pub host_id: String,
    pub timeout_secs: Option<i32>,
    pub control_interface_manifest: Option<ControlInterfaceManifest>,
    pub delegation_scope: Option<DelegationScope>,
    pub phases: Vec<PhaseSpec>,
    pub parent_task_id: Option<Uuid>,
}

impl StoredScheduledTaskTemplate {
    pub fn from_proto(template: ScheduledTaskTemplate) -> Result<Self, Status> {
        let environment_id = Uuid::parse_str(&template.environment_id)
            .map_err(|_| Status::invalid_argument("task_template.environment_id is not a valid UUID"))?;
        let host_id = template.host_id.trim().to_string();
        if host_id.is_empty() {
            return Err(Status::invalid_argument("task_template.host_id is required"));
        }

        let timeout_secs = template
            .timeout_secs
            .map(|value| {
                i32::try_from(value).map_err(|_| Status::invalid_argument("task_template.timeout_secs is too large"))
            })
            .transpose()?;
        let control_interface_manifest = template
            .control_interface_manifest
            .map(prost_struct_to_json)
            .map(serde_json::from_value::<ControlInterfaceManifest>)
            .transpose()
            .map_err(|error| {
                Status::invalid_argument(format!("invalid task_template.control_interface_manifest: {error}"))
            })?;
        let delegation_scope = template
            .delegation_scope
            .map(prost_struct_to_json)
            .map(serde_json::from_value::<DelegationScope>)
            .transpose()
            .map_err(|error| Status::invalid_argument(format!("invalid task_template.delegation_scope: {error}")))?;
        let phases = template
            .phases
            .into_iter()
            .map(prost_struct_to_json)
            .map(serde_json::from_value::<PhaseSpec>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| Status::invalid_argument(format!("invalid task_template.phases: {error}")))?;
        let parent_task_id = template
            .parent_task_id
            .map(|value| {
                Uuid::parse_str(&value)
                    .map_err(|_| Status::invalid_argument("task_template.parent_task_id is not a valid UUID"))
            })
            .transpose()?;

        Ok(Self {
            prompt: template.prompt,
            environment_id,
            host_id,
            timeout_secs,
            control_interface_manifest,
            delegation_scope,
            phases,
            parent_task_id,
        })
    }

    pub fn from_json_value(value: serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value)
    }

    pub fn to_json_value(&self) -> Result<serde_json::Value, Status> {
        serde_json::to_value(self)
            .map_err(|error| Status::internal(format!("failed to serialize task_template: {error}")))
    }

    pub fn to_proto(&self) -> ScheduledTaskTemplate {
        ScheduledTaskTemplate {
            prompt: self.prompt.clone(),
            environment_id: self.environment_id.to_string(),
            host_id: self.host_id.clone(),
            timeout_secs: self.timeout_secs.and_then(|value| u32::try_from(value).ok()),
            control_interface_manifest: self
                .control_interface_manifest
                .clone()
                .and_then(|value| serde_json::to_value(value).ok())
                .and_then(json_to_prost_struct),
            delegation_scope: self
                .delegation_scope
                .clone()
                .and_then(|value| serde_json::to_value(value).ok())
                .and_then(json_to_prost_struct),
            phases: self
                .phases
                .iter()
                .filter_map(|value| serde_json::to_value(value).ok())
                .filter_map(json_to_prost_struct)
                .collect(),
            parent_task_id: self.parent_task_id.map(|value| value.to_string()),
        }
    }
}

fn prost_struct_to_json(s: prost_types::Struct) -> serde_json::Value {
    let map: serde_json::Map<String, serde_json::Value> =
        s.fields.into_iter().map(|(k, v)| (k, prost_value_to_json(v))).collect();
    serde_json::Value::Object(map)
}

fn prost_value_to_json(v: prost_types::Value) -> serde_json::Value {
    match v.kind {
        Some(prost_types::value::Kind::NumberValue(n)) => serde_json::Number::from_f64(n)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Some(prost_types::value::Kind::StringValue(s)) => serde_json::Value::String(s),
        Some(prost_types::value::Kind::BoolValue(b)) => serde_json::Value::Bool(b),
        Some(prost_types::value::Kind::StructValue(s)) => prost_struct_to_json(s),
        Some(prost_types::value::Kind::ListValue(l)) => {
            serde_json::Value::Array(l.values.into_iter().map(prost_value_to_json).collect())
        }
        Some(prost_types::value::Kind::NullValue(_)) | None => serde_json::Value::Null,
    }
}

fn json_to_prost_struct(value: serde_json::Value) -> Option<prost_types::Struct> {
    let serde_json::Value::Object(map) = value else {
        return None;
    };
    Some(prost_types::Struct {
        fields: map
            .into_iter()
            .map(|(key, value)| (key, json_to_prost_value(value)))
            .collect::<BTreeMap<_, _>>(),
    })
}

fn json_to_prost_value(value: serde_json::Value) -> prost_types::Value {
    let kind = match value {
        serde_json::Value::Null => Some(prost_types::value::Kind::NullValue(0)),
        serde_json::Value::Bool(value) => Some(prost_types::value::Kind::BoolValue(value)),
        serde_json::Value::Number(value) => Some(prost_types::value::Kind::NumberValue(
            value.as_f64().unwrap_or_default(),
        )),
        serde_json::Value::String(value) => Some(prost_types::value::Kind::StringValue(value)),
        serde_json::Value::Array(values) => Some(prost_types::value::Kind::ListValue(prost_types::ListValue {
            values: values.into_iter().map(json_to_prost_value).collect(),
        })),
        serde_json::Value::Object(map) => Some(prost_types::value::Kind::StructValue(prost_types::Struct {
            fields: map
                .into_iter()
                .map(|(key, value)| (key, json_to_prost_value(value)))
                .collect::<BTreeMap<_, _>>(),
        })),
    };
    prost_types::Value { kind }
}
