//! The wire format is a compatibility surface: the host and every guest block
//! agree on it, and they are compiled separately. These tests pin the actual
//! JSON, not just round-tripping, because a field rename that still round-trips
//! within one build would silently break every already-compiled block.

use cuttlefish_abi::{
    error_codes, Command, Envelope, Event, JobError, JobStatus, MediaKind, TokenAction, Usage,
};

#[test]
fn infer_command_serializes_to_the_expected_json() {
    let cmd = Command::Infer {
        prompt: "hi".into(),
        max_tokens: 32,
        images: Vec::new(),
    };
    assert_eq!(
        serde_json::to_string(&cmd).unwrap(),
        r#"{"cmd":"infer","prompt":"hi","max_tokens":32,"images":[]}"#
    );
}

#[test]
fn every_command_round_trips() {
    let cases = vec![
        Command::Infer {
            prompt: "p".into(),
            max_tokens: 1,
            images: Vec::new(),
        },
        Command::Infer {
            prompt: "look".into(),
            max_tokens: 8,
            images: vec![1, 2],
        },
        Command::SliceBytes {
            handle: 2,
            offset: 0,
            len: 512,
        },
        Command::PageText { handle: 3, page: 0 },
        Command::PageImage { handle: 3, page: 1 },
        Command::Open {
            path: "/a/b".into(),
        },
        Command::Slice {
            handle: 7,
            offset: 4096,
            len: 1024,
        },
        Command::Emit {
            progress: serde_json::json!({"done": 2}),
        },
        Command::Done {
            result: serde_json::json!({"summary": "s"}),
        },
        Command::Fail {
            code: "bad".into(),
            message: "why".into(),
        },
    ];
    for cmd in cases {
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(
            serde_json::from_str::<Command>(&json).unwrap(),
            cmd,
            "round trip failed for {json}"
        );
    }
}

#[test]
fn every_event_round_trips() {
    let cases = vec![
        Event::InferDone {
            text: "yo".into(),
            tokens_out: 2,
        },
        Event::Opened {
            handle: 3,
            len: 900,
            kind: MediaKind::Text,
        },
        Event::Sliced {
            text: "abc".into(),
            next_offset: 3,
        },
        Event::Emitted,
    ];
    for ev in cases {
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(
            serde_json::from_str::<Event>(&json).unwrap(),
            ev,
            "round trip failed for {json}"
        );
    }
}

#[test]
fn slice_offsets_survive_beyond_u32() {
    // Offsets are u64 precisely so a job can address a file larger than the
    // guest could ever hold. If these narrow to u32, large-file support breaks
    // silently at 4 GiB rather than failing loudly.
    let cmd = Command::Slice {
        handle: 1,
        offset: u64::from(u32::MAX) + 1,
        len: 8,
    };
    let back: Command = serde_json::from_str(&serde_json::to_string(&cmd).unwrap()).unwrap();
    assert_eq!(back, cmd);
}

#[test]
fn envelope_omits_absent_optional_fields() {
    let env = Envelope {
        status: JobStatus::Completed,
        result: Some(serde_json::json!({"summary": "s"})),
        error: None,
        usage: Usage::default(),
    };
    let json = serde_json::to_string(&env).unwrap();
    assert!(
        !json.contains("error"),
        "an absent error must not serialize at all: {json}"
    );
}

#[test]
fn failed_envelope_carries_a_code_and_no_result() {
    let env = Envelope {
        status: JobStatus::Failed,
        result: None,
        error: Some(JobError {
            code: error_codes::CAPABILITY_DENIED.into(),
            message: "read not permitted".into(),
        }),
        usage: Usage::default(),
    };
    let json = serde_json::to_string(&env).unwrap();
    assert!(json.contains(r#""status":"failed""#), "{json}");
    assert!(json.contains(r#""code":"capability_denied""#), "{json}");
    assert!(
        !json.contains(r#""result""#),
        "a failed job must never carry a partial result: {json}"
    );
}

#[test]
fn token_action_maps_to_the_abi_integers() {
    // These integers cross the wasm boundary as a raw i32 return value, so the
    // mapping is part of the ABI and cannot be reordered.
    assert_eq!(TokenAction::Continue.as_i32(), 0);
    assert_eq!(TokenAction::Stop.as_i32(), 1);
    assert_eq!(TokenAction::from_i32(0), TokenAction::Continue);
    assert_eq!(TokenAction::from_i32(1), TokenAction::Stop);
}

#[test]
fn unknown_token_action_values_stop_rather_than_continue() {
    // A guest returning garbage must not be read as "keep generating" — fail
    // closed, the same posture as the capability checks.
    assert_eq!(TokenAction::from_i32(42), TokenAction::Stop);
    assert_eq!(TokenAction::from_i32(-1), TokenAction::Stop);
}

#[test]
fn terminal_statuses_are_exactly_the_finished_ones() {
    // Clients poll until a status is terminal. A terminal status missing from
    // `is_terminal` leaves them polling forever, so this pins the whole set
    // rather than spot-checking one value.
    assert!(JobStatus::Completed.is_terminal());
    assert!(JobStatus::Failed.is_terminal());
    assert!(JobStatus::Cancelled.is_terminal());
    assert!(!JobStatus::Queued.is_terminal());
    assert!(!JobStatus::Running.is_terminal());
}

#[test]
fn job_status_serializes_as_snake_case() {
    // The daemon's HTTP clients match on these strings.
    let json = serde_json::to_string(&JobStatus::Cancelled).unwrap();
    assert_eq!(json, r#""cancelled""#);
}

#[test]
fn interrupted_is_not_terminal() {
    assert!(!JobStatus::Interrupted.is_terminal());
}

#[test]
fn an_older_block_still_deserializes_the_new_fields() {
    // Blocks are compiled separately and ship independently, so a block built
    // before images and media kinds existed must keep working. Both fields
    // default, which is what makes that true — this test is the guard on it.
    let cmd: Command =
        serde_json::from_str(r#"{"cmd":"infer","prompt":"hi","max_tokens":4}"#).unwrap();
    assert_eq!(
        cmd,
        Command::Infer {
            prompt: "hi".into(),
            max_tokens: 4,
            images: Vec::new()
        }
    );

    let ev: Event = serde_json::from_str(r#"{"event":"opened","handle":1,"len":10}"#).unwrap();
    assert_eq!(
        ev,
        Event::Opened {
            handle: 1,
            len: 10,
            kind: MediaKind::Text
        }
    );
}

#[test]
fn media_kinds_round_trip() {
    for kind in [
        MediaKind::Text,
        MediaKind::Binary,
        MediaKind::Image {
            format: "jpeg".into(),
        },
        MediaKind::Document {
            pages: 3,
            has_text_layer: false,
        },
    ] {
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(
            serde_json::from_str::<MediaKind>(&json).unwrap(),
            kind,
            "{json}"
        );
    }
}

#[test]
fn types_round_trip_through_their_string_form() {
    use cuttlefish_abi::Ty;
    use std::collections::BTreeMap;

    let mut fields = BTreeMap::new();
    fields.insert("path".to_string(), Ty::Text);
    fields.insert("pages".to_string(), Ty::List(Box::new(Ty::Text)));

    for ty in [
        Ty::Text,
        Ty::Bytes,
        Ty::Image,
        Ty::Document,
        Ty::Json,
        Ty::List(Box::new(Ty::Text)),
        Ty::List(Box::new(Ty::List(Box::new(Ty::Bytes)))),
        Ty::Record(fields.clone()),
        Ty::Record(BTreeMap::new()),
    ] {
        let text = ty.to_string();
        assert_eq!(
            text.parse::<Ty>().unwrap(),
            ty,
            "round trip failed for {text}"
        );

        // And through JSON, which is how it actually crosses the wasm boundary.
        let json = serde_json::to_string(&ty).unwrap();
        assert_eq!(serde_json::from_str::<Ty>(&json).unwrap(), ty, "{json}");
    }
}

#[test]
fn a_record_containing_a_list_survives_field_splitting() {
    use cuttlefish_abi::Ty;
    // The comma inside `[a, b]` must not be read as a field separator — a naive
    // split(',') cuts this in the wrong place and produces nonsense.
    let ty: Ty = "{chunks: [text], name: text}".parse().unwrap();
    match ty {
        Ty::Record(fields) => {
            assert_eq!(fields.len(), 2, "got {fields:?}");
            assert_eq!(fields["chunks"], Ty::List(Box::new(Ty::Text)));
        }
        other => panic!("expected a record, got {other:?}"),
    }
}

#[test]
fn assignability_is_not_equality() {
    use cuttlefish_abi::Ty;
    use std::collections::BTreeMap;

    // Everything fits json — it is the top type.
    assert!(Ty::Text.assignable_to(&Ty::Json));
    assert!(Ty::List(Box::new(Ty::Image)).assignable_to(&Ty::Json));

    // But json does not fit something specific: that would defeat the check.
    assert!(!Ty::Json.assignable_to(&Ty::Text));

    let mut produced = BTreeMap::new();
    produced.insert("a".into(), Ty::Text);
    produced.insert("extra".into(), Ty::Text);
    let mut required = BTreeMap::new();
    required.insert("a".into(), Ty::Text);

    // Extra fields are fine — a producer adding one must not break a consumer.
    assert!(Ty::Record(produced).assignable_to(&Ty::Record(required.clone())));

    // A missing field is not.
    assert!(!Ty::Record(BTreeMap::new()).assignable_to(&Ty::Record(required)));

    // Mismatched kinds never fit.
    assert!(!Ty::Text.assignable_to(&Ty::Image));
    assert!(!Ty::List(Box::new(Ty::Text)).assignable_to(&Ty::List(Box::new(Ty::Image))));
}

#[test]
fn an_unknown_type_is_rejected_by_name() {
    use cuttlefish_abi::Ty;
    let err = "nonsense".parse::<Ty>().unwrap_err();
    assert!(err.contains("nonsense"), "{err}");
}
