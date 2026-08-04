use std::{fs, path::Path};

use qq_protocol::{PROTOCOL_VERSION, RunPromptIdentity, SessionEventEnvelope};

#[test]
fn harbor_trace_fixtures_match_the_current_durable_wire_shapes() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("benchmarks/harbor/tests/fixtures");
    let mut paths = fs::read_dir(&fixtures)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("jsonl"))
        .collect::<Vec<_>>();
    paths.sort();
    assert!(!paths.is_empty());

    for path in paths {
        let mut saw_trial = false;
        let mut saw_event = false;
        let mut saw_outcome = false;
        for (index, line) in fs::read_to_string(&path).unwrap().lines().enumerate() {
            let record: serde_json::Value = serde_json::from_str(line).unwrap_or_else(|error| {
                panic!("{}:{} is invalid JSON: {error}", path.display(), index + 1)
            });
            match record.get("type").and_then(serde_json::Value::as_str) {
                Some("trial") => {
                    assert!(!saw_trial && !saw_event && !saw_outcome);
                    assert_eq!(record["protocol_version"], PROTOCOL_VERSION);
                    assert_eq!(record["workspace_identity"].as_str().unwrap().len(), 64);
                    saw_trial = true;
                }
                Some("event") => {
                    assert!(saw_trial && !saw_outcome);
                    serde_json::from_value::<SessionEventEnvelope>(record["envelope"].clone())
                        .unwrap_or_else(|error| {
                            panic!(
                                "{}:{} is not a current session event: {error}",
                                path.display(),
                                index + 1
                            )
                        });
                    saw_event = true;
                }
                Some("outcome") => {
                    assert!(saw_trial && saw_event && !saw_outcome);
                    serde_json::from_value::<RunPromptIdentity>(record["prompt_identity"].clone())
                        .unwrap_or_else(|error| {
                            panic!(
                                "{}:{} has an invalid prompt identity: {error}",
                                path.display(),
                                index + 1
                            )
                        });
                    saw_outcome = true;
                }
                kind => panic!(
                    "{}:{} has unknown record type {kind:?}",
                    path.display(),
                    index + 1
                ),
            }
        }
        assert!(
            saw_trial && saw_event && saw_outcome,
            "{} must contain one complete trial stream",
            path.display()
        );
    }
}
