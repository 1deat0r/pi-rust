//! Legacy coding-agent JSONL v3 compatibility.
//!
//! The coding-agent session manager persists one JSON object per line using a
//! v3 `type:"session"` header and typed entries. The harness session state
//! also stores internal records and lanes, so v3 persistence intentionally
//! writes only the upstream entry/fact surface; those internal bookkeeping
//! mutations remain in memory and are reconstructed from entries on reopen.

use serde_json::{Map, Value};

use super::super::types::{Entry, Fact, JsonlV4Header, Mutation};
use super::errors::{JsonlDecodeError, JsonlDecodeErrorKind};

pub(crate) fn parse_header(line: &str) -> Result<JsonlV4Header, JsonlDecodeError> {
    let value: Value = serde_json::from_str(line)
        .map_err(|_| JsonlDecodeError::new(JsonlDecodeErrorKind::Syntax, "is not valid JSON"))?;
    let object = value.as_object().ok_or_else(|| {
        JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "is not a JSON object")
    })?;
    if object.get("type").and_then(Value::as_str) != Some("session") {
        return Err(JsonlDecodeError::new(
            JsonlDecodeErrorKind::Schema,
            "is not a session header",
        ));
    }
    let version = object.get("version").and_then(Value::as_u64).unwrap_or(1);
    if !(1..=3).contains(&version) {
        return Err(JsonlDecodeError::new(
            JsonlDecodeErrorKind::Schema,
            "has unsupported session version",
        ));
    }
    let id = required_string(object, "id")?;
    let cwd = required_string(object, "cwd")?;
    let timestamp = required_string(object, "timestamp")?;
    let created_at = parse_timestamp(&timestamp)?;
    let parent_session_id = object
        .get("parentSession")
        .map(|value| {
            value.as_str().map(str::to_owned).ok_or_else(|| {
                JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid parentSession")
            })
        })
        .transpose()?;
    Ok(JsonlV4Header {
        kind: "header".to_string(),
        version: 4,
        id,
        created_at,
        cwd,
        parent_session_id,
        legacy_parent_session_path: None,
        metadata: None,
    })
}

pub(crate) fn encode_header(header: &JsonlV4Header) -> Result<String, serde_json::Error> {
    let mut value = serde_json::json!({
        "type": "session",
        "version": 3,
        "id": header.id,
        "timestamp": format_timestamp(header.created_at),
        "cwd": header.cwd,
    });
    if let Some(parent) = header
        .parent_session_id
        .as_deref()
        .or(header.legacy_parent_session_path.as_deref())
    {
        value["parentSession"] = Value::String(parent.to_string());
    }
    Ok(format!("{}\n", serde_json::to_string(&value)?))
}

pub(crate) fn parse_entry(line: &str, seq: u64) -> Result<Mutation, JsonlDecodeError> {
    let value: Value = serde_json::from_str(line)
        .map_err(|_| JsonlDecodeError::new(JsonlDecodeErrorKind::Syntax, "is not valid JSON"))?;
    let object = value.as_object().ok_or_else(|| {
        JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "is not a JSON object")
    })?;
    let entry_type = required_string(object, "type")?;
    let timestamp = parse_timestamp(&required_string(object, "timestamp")?)?;
    let id = required_string(object, "id")?;
    let parent_id = match object.get("parentId") {
        None | Some(Value::Null) => None,
        Some(Value::String(parent)) => Some(parent.clone()),
        Some(_) => {
            return Err(JsonlDecodeError::new(
                JsonlDecodeErrorKind::Schema,
                "has invalid parentId",
            ))
        }
    };

    let entry = match entry_type.as_str() {
        "message" => Entry::Message {
            id,
            seq,
            parent_id,
            timestamp,
            message: serde_json::from_value(object.get("message").cloned().ok_or_else(|| {
                JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid message")
            })?)
            .map_err(|_| {
                JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid message")
            })?,
            terminate: object.get("terminate").and_then(Value::as_bool),
        },
        "model_change" => Entry::ModelChange {
            id,
            seq,
            parent_id,
            timestamp,
            provider: required_string(object, "provider")?,
            model_id: required_string(object, "modelId")?,
        },
        "thinking_level_change" => Entry::ThinkingLevel {
            id,
            seq,
            parent_id,
            timestamp,
            thinking_level: required_string(object, "thinkingLevel")?,
        },
        "compaction" => Entry::Compaction {
            id,
            seq,
            parent_id,
            timestamp,
            summary: required_string(object, "summary")?,
            retained_tail: object
                .get("retainedTail")
                .cloned()
                .map(|value| serde_json::from_value(value).unwrap_or_default())
                .unwrap_or_default(),
            tokens_before: object
                .get("tokensBefore")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid tokensBefore")
                })?,
            details: object.get("details").cloned(),
            usage: object
                .get("usage")
                .cloned()
                .map(|value| serde_json::from_value(value).unwrap_or_default()),
        },
        "branch_summary" => Entry::BranchSummary {
            id,
            seq,
            parent_id,
            timestamp,
            from_id: required_string(object, "fromId")?,
            summary: required_string(object, "summary")?,
            details: object.get("details").cloned(),
            usage: object
                .get("usage")
                .cloned()
                .map(|value| serde_json::from_value(value).unwrap_or_default()),
        },
        "custom" => Entry::Custom {
            id,
            seq,
            parent_id,
            timestamp,
            custom_type: required_string(object, "customType")?,
            data: object.get("data").cloned(),
        },
        "session_info" => {
            return Ok(Mutation::Fact(Fact::Name {
                seq,
                name: object
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            }))
        }
        "label" => {
            return Ok(Mutation::Fact(Fact::Label {
                seq,
                target_id: required_string(object, "targetId")?,
                label: object
                    .get("label")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            }))
        }
        _ => {
            return Err(JsonlDecodeError::new(
                JsonlDecodeErrorKind::Schema,
                format!("has unknown entry type {entry_type}"),
            ))
        }
    };
    Ok(Mutation::Entry {
        // v3 has no persisted lane pointer. The loader advances the main
        // pointer after applying each entry so branched legacy trees remain
        // readable without imposing v4's lane-parent validation.
        lane: None,
        entry,
    })
}

pub(crate) fn encode_mutation(
    mutation: &Mutation,
    timestamp_ms: u64,
) -> Result<Option<String>, serde_json::Error> {
    let value = match mutation {
        Mutation::Entry { entry, .. } => {
            let mut value = serde_json::to_value(entry)?;
            if let Value::Object(object) = &mut value {
                if let Some(timestamp) = object.get("timestamp").and_then(Value::as_u64) {
                    object.insert(
                        "timestamp".to_string(),
                        Value::String(format_timestamp(timestamp)),
                    );
                }
                object.remove("seq");
            }
            value
        }
        Mutation::Fact(Fact::Name { seq, name }) => serde_json::json!({
            "type": "session_info",
            "id": format!("fact-name-{seq}"),
            "parentId": null,
            "timestamp": format_timestamp(timestamp_ms),
            "name": name,
        }),
        Mutation::Fact(Fact::Label {
            seq,
            target_id,
            label,
        }) => serde_json::json!({
            "type": "label",
            "id": format!("fact-label-{seq}"),
            "parentId": null,
            "timestamp": format_timestamp(timestamp_ms),
            "targetId": target_id,
            "label": label,
        }),
        // These are harness bookkeeping, not part of the upstream v3 file
        // contract. They remain in SessionState for the current process.
        Mutation::Record { .. } | Mutation::Lane { .. } => return Ok(None),
    };
    Ok(Some(format!("{}\n", serde_json::to_string(&value)?)))
}

fn required_string(object: &Map<String, Value>, field: &str) -> Result<String, JsonlDecodeError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, format!("has invalid {field}"))
        })
}

fn parse_timestamp(value: &str) -> Result<u64, JsonlDecodeError> {
    let (date, time) = value
        .strip_suffix('Z')
        .and_then(|value| value.split_once('T'))
        .ok_or_else(|| {
            JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid timestamp")
        })?;
    let mut date_parts = date.split('-');
    let year = parse_component(date_parts.next(), "timestamp")?;
    let month = parse_component(date_parts.next(), "timestamp")?;
    let day = parse_component(date_parts.next(), "timestamp")?;
    if date_parts.next().is_some() {
        return Err(JsonlDecodeError::new(
            JsonlDecodeErrorKind::Schema,
            "has invalid timestamp",
        ));
    }
    let (clock, fraction) = time.split_once('.').unwrap_or((time, "0"));
    let mut clock_parts = clock.split(':');
    let hour = parse_component(clock_parts.next(), "timestamp")?;
    let minute = parse_component(clock_parts.next(), "timestamp")?;
    let second = parse_component(clock_parts.next(), "timestamp")?;
    if clock_parts.next().is_some()
        || hour > 23
        || minute > 59
        || second > 59
        || fraction.is_empty()
        || !fraction.chars().all(|character| character.is_ascii_digit())
    {
        return Err(JsonlDecodeError::new(
            JsonlDecodeErrorKind::Schema,
            "has invalid timestamp",
        ));
    }
    let millis = fraction
        .chars()
        .take(3)
        .collect::<String>()
        .parse::<u64>()
        .unwrap_or(0)
        .saturating_mul(match fraction.len() {
            1 => 100,
            2 => 10,
            _ => 1,
        });
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return Err(JsonlDecodeError::new(
            JsonlDecodeErrorKind::Schema,
            "has invalid timestamp",
        ));
    }
    let days = days_from_civil(year, month, day);
    if days < 0 {
        return Err(JsonlDecodeError::new(
            JsonlDecodeErrorKind::Schema,
            "has unsupported timestamp",
        ));
    }
    Ok((((days as u64 * 24 + hour) * 60 + minute) * 60 + second) * 1000 + millis)
}

fn parse_component(value: Option<&str>, field: &str) -> Result<u64, JsonlDecodeError> {
    value
        .filter(|value| {
            !value.is_empty() && value.chars().all(|character| character.is_ascii_digit())
        })
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| {
            JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, format!("has invalid {field}"))
        })
}

fn days_in_month(year: u64, month: u64) -> u64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn days_from_civil(year: u64, month: u64, day: u64) -> i64 {
    let year = year as i64 - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_prime = month as i64 + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn format_timestamp(epoch_ms: u64) -> String {
    let seconds = epoch_ms / 1000;
    let millis = epoch_ms % 1000;
    let days = seconds / 86_400;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        seconds_of_day / 3600,
        (seconds_of_day / 60) % 60,
        seconds_of_day % 60
    )
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}
