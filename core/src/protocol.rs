// Wire protocol: newline-delimited JSON over stdio.
// TUI -> Core commands (stdin), Core -> TUI events (stdout).
mod commands;
mod common;
mod events;
mod version;

pub use commands::Command;
pub use common::{ClientInfo, ModelInfo};

#[cfg(test)]
pub use events::{begin_emit_capture, end_emit_capture};
pub use events::{emit, emit_aborted_done, emit_turn_rejected, install_runtime, Event};
pub use version::{CAPABILITIES, PROTOCOL_VERSION};

#[cfg(test)]
mod turn_terminal_tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn legacy_plain_init_still_decodes() {
        let command: Command = serde_json::from_str(r#"{"type":"init"}"#).unwrap();
        assert!(matches!(
            command,
            Command::Init {
                protocol_version: None,
                client: None
            }
        ));
    }

    #[test]
    fn versioned_init_decodes_client_capabilities() {
        let command: Command = serde_json::from_str(
            r#"{"type":"init","protocol_version":2,"client":{"name":"test","version":"1.0","capabilities":["run_ids"]}}"#,
        )
        .unwrap();
        match command {
            Command::Init {
                protocol_version,
                client,
            } => {
                assert_eq!(protocol_version, Some(2));
                let client = client.unwrap();
                assert_eq!(client.name, "test");
                assert_eq!(client.capabilities, ["run_ids"]);
            }
            _ => panic!("expected init"),
        }
    }

    #[test]
    fn every_command_fixture_deserializes_and_roundtrips_its_discriminator() {
        let fixtures = include_str!("../../protocol/fixtures/commands-v2.jsonl");
        let mut count = 0;
        for (index, line) in fixtures.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let original: Value = serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("fixture line {}: {error}", index + 1));
            let command: Command = serde_json::from_value(original.clone())
                .unwrap_or_else(|error| panic!("command line {}: {error}", index + 1));
            let encoded = serde_json::to_value(command).unwrap();
            assert_eq!(encoded["type"], original["type"], "line {}", index + 1);
            count += 1;
        }
        assert_eq!(count, 73, "fixture must cover every command variant");
    }

    #[test]
    fn command_deserialization_tolerates_unknown_optional_fields() {
        let command = serde_json::json!({
            "type":"init",
            "protocol_version":2,
            "future_optional_field":{"safe":true}
        });
        assert!(serde_json::from_value::<Command>(command).is_ok());
    }

    #[test]
    fn every_known_event_has_a_versioned_fixture() {
        let fixtures = include_str!("../../protocol/fixtures/events-v2.jsonl");
        let mut kinds = std::collections::HashSet::new();
        for (index, line) in fixtures.lines().enumerate() {
            let event: Value = serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("event fixture line {}: {error}", index + 1));
            let kind = event["type"]
                .as_str()
                .unwrap_or_else(|| panic!("event fixture line {} has no type", index + 1))
                .to_string();
            assert!(
                kinds.insert(kind.clone()),
                "duplicate event fixture: {kind}"
            );
            assert_eq!(event["protocol_version"], PROTOCOL_VERSION);
        }
        assert_eq!(kinds.len(), 105, "fixture must cover every known event");
    }
}
