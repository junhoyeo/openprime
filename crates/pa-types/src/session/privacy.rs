//! Reserved, data-following admission privacy vocabulary and strict row decoding.
//! This module owns only JSON shape. Files, leases, admission and telemetry live
//! in their owning crates. A generic custom message never grants provenance.
use std::collections::BTreeSet;
use std::fmt;

use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{json, Map, Value};

use super::{CustomEntry, EntryBase, FileEntry};

pub const ADMISSION_PRIVACY_CUSTOM_TYPE: &str = "admission_privacy_state";
pub const ADMISSION_PRIVACY_FORMAT: &str = "openprime.session-admission-privacy";

/// The only valid current carrier is restrictive. There is no false/reset variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionPrivacy {
    RestrictedV1,
}

/// Static diagnostics never echo an offending row or preference value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PrivacyRowError {
    #[error("session privacy row is malformed or has duplicate JSON keys")]
    InvalidJson,
    #[error("session privacy row must be an object")]
    NotObject,
    #[error("reserved session privacy metadata has an invalid envelope")]
    InvalidEnvelope,
    #[error("reserved session privacy metadata has an unsupported policy")]
    InvalidPolicy,
    #[error("reserved session privacy name cannot be used as a custom message")]
    ReservedCollision,
}

/// Check one known metadata/custom-message intake position. Never recursively
/// interpret tool arguments, results, notice details or other application data.
#[must_use]
pub fn contains_reserved_privacy_name(value: &Value) -> bool {
    value.as_object().and_then(|object| object.get("customType"))
        .and_then(Value::as_str) == Some(ADMISSION_PRIVACY_CUSTOM_TYPE)
}

/// Decode a complete JSONL row before ordinary Value normalization can lose duplicates.
///
/// # Errors
/// Returns a static error for malformed/duplicate JSON or an invalid reserved carrier.
pub fn decode_privacy_row(line: &[u8]) -> Result<(Value, Option<AdmissionPrivacy>), PrivacyRowError> {
    let strict: UniqueValue = serde_json::from_slice(line).map_err(|_| PrivacyRowError::InvalidJson)?;
    let value = strict.0;
    let object = value.as_object().ok_or(PrivacyRowError::NotObject)?;
    if object.get("customType").and_then(Value::as_str) != Some(ADMISSION_PRIVACY_CUSTOM_TYPE) {
        if object.get("type").and_then(Value::as_str) == Some("message")
            && object.get("message").is_some_and(|message|
                message.get("role").and_then(Value::as_str) == Some("custom")
                    && contains_reserved_privacy_name(message))
        {
            return Err(PrivacyRowError::ReservedCollision);
        }
        return Ok((value, None));
    }
    validate_carrier(object)?;
    Ok((value, Some(AdmissionPrivacy::RestrictedV1)))
}

fn exact_keys(object: &Map<String, Value>, expected: &[&str]) -> bool {
    object.len() == expected.len() && expected.iter().all(|key| object.contains_key(*key))
}

fn nonempty_string(value: Option<&Value>) -> bool {
    value.and_then(Value::as_str).is_some_and(|text| !text.is_empty() && !text.contains('\0'))
}

fn validate_carrier(object: &Map<String, Value>) -> Result<(), PrivacyRowError> {
    if !exact_keys(object, &["type", "id", "parentId", "timestamp", "customType", "data"])
        || object.get("type").and_then(Value::as_str) != Some("custom")
        || !nonempty_string(object.get("id"))
        || !nonempty_string(object.get("timestamp"))
        || !(object.get("parentId") == Some(&Value::Null) || nonempty_string(object.get("parentId")))
    {
        return Err(PrivacyRowError::InvalidEnvelope);
    }
    let data = object.get("data").and_then(Value::as_object).ok_or(PrivacyRowError::InvalidPolicy)?;
    if !exact_keys(data, &["format", "version", "min_queue_snapshot_version", "aggregate_restricted"])
        || data.get("format").and_then(Value::as_str) != Some(ADMISSION_PRIVACY_FORMAT)
        || data.get("version").and_then(Value::as_u64) != Some(1)
        || data.get("min_queue_snapshot_version").and_then(Value::as_u64) != Some(3)
        || data.get("aggregate_restricted") != Some(&Value::Bool(true))
    {
        return Err(PrivacyRowError::InvalidPolicy);
    }
    Ok(())
}

impl AdmissionPrivacy {
    /// Produce the actual shared FileEntry representation. Parent is the selected
    /// destination leaf; a hidden row must not detach the conversation chain.
    ///
    /// # Errors
    /// Returns an error if the supplied existing entry-base fields are invalid.
    pub fn entry(self, base: EntryBase) -> Result<FileEntry, PrivacyRowError> {
        let entry = FileEntry::Custom {
            base,
            payload: CustomEntry {
                custom_type: ADMISSION_PRIVACY_CUSTOM_TYPE.to_string(),
                data: Some(json!({"format":ADMISSION_PRIVACY_FORMAT,"version":1,
                    "min_queue_snapshot_version":3,"aggregate_restricted":true})),
                rest: Map::default(),
            },
        };
        let value = serde_json::to_value(&entry).map_err(|_| PrivacyRowError::InvalidEnvelope)?;
        validate_carrier(value.as_object().ok_or(PrivacyRowError::InvalidEnvelope)?)?;
        Ok(entry)
    }
}

struct UniqueValue(Value);

impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UniqueVisitor;
        impl<'de> Visitor<'de> for UniqueVisitor {
            type Value = UniqueValue;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str("unique-key JSON") }
            fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> { Ok(UniqueValue(value.into())) }
            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> { Ok(UniqueValue(value.into())) }
            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> { Ok(UniqueValue(value.into())) }
            fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(value).map(|value|UniqueValue(Value::Number(value)))
                    .ok_or_else(||E::custom("invalid number"))
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> { Ok(UniqueValue(value.into())) }
            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> { Ok(UniqueValue(value.into())) }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> { Ok(UniqueValue(Value::Null)) }
            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> { Ok(UniqueValue(Value::Null)) }
            fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
                let mut array=Vec::new();
                while let Some(value)=sequence.next_element::<UniqueValue>()? { array.push(value.0); }
                Ok(UniqueValue(Value::Array(array)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut object=Map::new();
                let mut keys=BTreeSet::new();
                while let Some(key)=map.next_key::<String>()? {
                    if !keys.insert(key.clone()) { return Err(serde::de::Error::custom("duplicate key")); }
                    let value=map.next_value::<UniqueValue>()?;
                    object.insert(key,value.0);
                }
                Ok(UniqueValue(Value::Object(object)))
            }
        }
        deserializer.deserialize_any(UniqueVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn carrier() -> Value {
        serde_json::to_value(AdmissionPrivacy::RestrictedV1.entry(EntryBase {
            id:Some("privacy-row".into()), parent_id:Some("selected-leaf".into()),
            timestamp:Some("2026-10-30T00:00:00.000Z".into()), rest:Map::default(),
        }).unwrap()).unwrap()
    }
    #[test]
    fn actual_file_entry_serializer_roundtrips_the_reserved_carrier() {
        let expected=carrier();
        let (value,policy)=decode_privacy_row(&serde_json::to_vec(&expected).unwrap()).unwrap();
        assert_eq!((value.clone(),policy),(expected,Some(AdmissionPrivacy::RestrictedV1)));
        let entry:FileEntry=serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(entry).unwrap(),value);
        assert_eq!(value["parentId"],"selected-leaf");
    }
    #[test]
    fn duplicate_keys_refuse_before_map_normalization() {
        let line=carrier().to_string();
        for mutated in [line.replacen("\"version\":1","\"version\":1,\"version\":1",1),
            line.replacen("\"type\":\"custom\"","\"type\":\"custom\",\"type\":\"message\"",1)] {
            assert_eq!(decode_privacy_row(mutated.as_bytes()).unwrap_err(),PrivacyRowError::InvalidJson);
        }
    }
    #[test]
    fn policy_types_and_message_lookalikes_never_gain_authority() {
        for (key,value) in [("version",json!(true)),("version",json!(1.0)),
            ("min_queue_snapshot_version",json!("3")),("aggregate_restricted",json!(false)),
            ("aggregate_restricted",Value::Null)] {
            let mut row=carrier();row["data"][key]=value;
            assert!(decode_privacy_row(&serde_json::to_vec(&row).unwrap()).is_err());
        }
        let row=json!({"type":"message","message":{"role":"custom",
            "customType":ADMISSION_PRIVACY_CUSTOM_TYPE,"content":"synthetic"}});
        assert_eq!(decode_privacy_row(&serde_json::to_vec(&row).unwrap()).unwrap_err(),PrivacyRowError::ReservedCollision);
        let row=json!({"type":"message","message":{"role":"user","content":ADMISSION_PRIVACY_CUSTOM_TYPE}});
        assert_eq!(decode_privacy_row(&serde_json::to_vec(&row).unwrap()).unwrap().1,None);
    }
    #[test]
    fn reserved_name_inside_application_data_is_not_a_policy_carrier() {
        let data=json!({"customType":ADMISSION_PRIVACY_CUSTOM_TYPE,"enabled":false});
        let rows=[
            json!({"type":"message","message":{"role":"assistant","content":[
                {"type":"toolCall","name":"edit","arguments":data}]}}),
            json!({"type":"message","message":{"role":"toolResult","details":data}}),
            json!({"type":"custom_message","customType":"ordinary_notice","content":"text","details":data}),
        ];
        for row in rows {
            let (decoded,policy)=decode_privacy_row(&serde_json::to_vec(&row).unwrap()).unwrap();
            assert_eq!((decoded,policy),(row,None));
        }
        let lookalike=json!({"type":"custom_message","customType":ADMISSION_PRIVACY_CUSTOM_TYPE,
            "content":"not metadata"});
        assert!(decode_privacy_row(&serde_json::to_vec(&lookalike).unwrap()).is_err());
    }

}
