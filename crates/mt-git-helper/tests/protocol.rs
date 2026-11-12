use mt_git_helper::{MAX_FRAME_BYTES, encode_frame, read_frame, write_frame};
use serde_json::Value;
use std::io::{self, Cursor};

#[test]
fn malformed_truncated_and_oversized_frames_are_rejected() {
    assert!(
        read_frame::<Value>(&mut Cursor::new(Vec::<u8>::new()))
            .unwrap()
            .is_none()
    );
    for bytes in [vec![0], vec![0, 0, 0], vec![0, 0, 0, 2, b'{']] {
        assert_eq!(
            read_frame::<Value>(&mut Cursor::new(bytes))
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
    for bytes in [
        vec![0, 0, 0, 0],
        vec![0, 0, 0, 1, b'{'],
        vec![0, 0, 0, 1, 255],
        ((MAX_FRAME_BYTES + 1) as u32).to_be_bytes().to_vec(),
    ] {
        assert_eq!(
            read_frame::<Value>(&mut Cursor::new(bytes))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
    let exact = "x".repeat(MAX_FRAME_BYTES - 2);
    let frame = encode_frame(&exact).unwrap();
    assert_eq!(frame.len(), MAX_FRAME_BYTES + 4);
    assert_eq!(
        read_frame::<String>(&mut Cursor::new(frame))
            .unwrap()
            .unwrap(),
        exact
    );
    let oversized = "x".repeat(MAX_FRAME_BYTES - 1);
    let mut output = Vec::new();
    assert!(write_frame(&mut output, &oversized).is_err());
    assert!(
        output.is_empty(),
        "failed encoding must not emit partial output"
    );
}

#[test]
fn cursor_wire_is_deterministic_preserves_order_and_rejects_invalid_oids() {
    use mt_project::git::GitLogCursor;
    use serde_json::json;
    let a = "1111111111111111111111111111111111111111";
    let b = "2222222222222222222222222222222222222222";
    let c = "3333333333333333333333333333333333333333";
    let cursor: GitLogCursor = serde_json::from_value(json!({
        "loaded":[b,a], "frontier":[c,b,a]
    }))
    .unwrap();
    let wire = serde_json::to_value(&cursor).unwrap();
    assert_eq!(wire, json!({"loaded":[a,b],"frontier":[c,b,a]}));
    assert_eq!(
        cursor,
        serde_json::from_value::<GitLogCursor>(wire).unwrap()
    );
    for field in ["loaded", "frontier"] {
        let mut value = json!({"loaded":[],"frontier":[]});
        value[field] = json!(["not-an-oid"]);
        assert!(
            serde_json::from_value::<GitLogCursor>(value)
                .unwrap_err()
                .to_string()
                .contains(&format!("GitLogCursor.{field}[0]"))
        );
    }
    assert!(serde_json::from_value::<GitLogCursor>(json!({"loaded":[12],"frontier":[]})).is_err());
    assert!(
        serde_json::from_value::<GitLogCursor>(json!({"loaded":[],"frontier":[],"unknown":[]}))
            .is_err()
    );
}

#[test]
fn typed_worktree_failures_reconstruct_original_errors() {
    use mt_git_helper::GitFailure;
    use mt_project::git::WorktreeRemoveError;
    for original in [
        WorktreeRemoveError::InUse {
            path: "/repo/wt".into(),
            busy: "/repo/wt/sub".into(),
        },
        WorktreeRemoveError::Leftover {
            path: "/repo/wt".into(),
            detail: "permission denied".into(),
        },
    ] {
        let failure = GitFailure::from_error(original.clone().into());
        let wire = serde_json::to_vec(&failure).unwrap();
        let restored: GitFailure = serde_json::from_slice(&wire).unwrap();
        assert_eq!(
            restored.into_error().downcast_ref::<WorktreeRemoveError>(),
            Some(&original)
        );
    }
}

#[test]
fn v5_external_enums_decode_raw_data_without_value_materialization() {
    use mt_git_helper::{Operation, PROTOCOL_VERSION, Reply, Request, Response};
    assert_eq!(PROTOCOL_VERSION, 5);
    let request = Request {
        protocol_version: PROTOCOL_VERSION,
        id: 9,
        operation: Operation::Status {
            repo: "/repo".into(),
        },
    };
    assert_eq!(
        serde_json::to_string(&request.operation).unwrap(),
        r#"{"status":{"repo":"/repo"}}"#
    );
    assert!(serde_json::from_str::<Operation>(r#"{"kind":"status","repo":"/repo"}"#).is_err());
    let json = r#"{"protocolVersion":5,"id":9,"result":{"Ok":{"data":{"value":["one","two"]}}}}"#;
    let response: Response = mt_git_helper::decode_json(json.as_bytes()).unwrap();
    assert_eq!(
        response.result.unwrap().decode::<Vec<String>>().unwrap(),
        ["one", "two"]
    );
    let invalid = Reply::Data {
        value: serde_json::value::RawValue::from_string(r#"["wrong"]"#.into()).unwrap(),
    };
    assert!(invalid.decode::<Vec<u64>>().unwrap_err().len() < 256);
}

#[test]
fn limits_apply_before_typed_decode_and_symmetrically_on_encode() {
    use mt_git_helper::{
        MAX_COLLECTION_ITEMS, MAX_IDENTIFIER_BYTES, MAX_PATH_BYTES, Operation, decode_json,
    };
    for repo in ["r".repeat(MAX_PATH_BYTES), "界".repeat(MAX_PATH_BYTES / 3)] {
        let operation = Operation::Status { repo };
        let frame = encode_frame(&operation).unwrap();
        assert!(
            read_frame::<Operation>(&mut Cursor::new(frame))
                .unwrap()
                .is_some()
        );
    }
    let operation = Operation::Status {
        repo: "r".repeat(MAX_PATH_BYTES + 1),
    };
    assert!(encode_frame(&operation).is_err());
    let wire = serde_json::to_vec(&operation).unwrap();
    assert!(decode_json::<Operation>(&wire).is_err());
    let operation = Operation::CommitDetail {
        repo: "/repo".into(),
        hash: "h".repeat(MAX_IDENTIFIER_BYTES + 1),
    };
    assert!(encode_frame(&operation).is_err());
    let wire = format!("[{}null]", "null,".repeat(MAX_COLLECTION_ITEMS));
    assert!(decode_json::<serde::de::IgnoredAny>(wire.as_bytes()).is_err());
    let operation = Operation::PanelSnapshot {
        repo: "/repo".into(),
        recent_limit: usize::MAX,
    };
    assert!(encode_frame(&operation).is_err());
}

#[test]
fn near_frame_limit_diff_is_not_truncated() {
    use mt_git_helper::{PROTOCOL_VERSION, Reply, Response};
    let content = "x".repeat(MAX_FRAME_BYTES - 1024);
    let diff = mt_project::git::GitDiffResult {
        old_content: String::new(),
        new_content: content,
        hunks: Vec::new(),
        is_binary: false,
        too_large: false,
        encoding: None,
    };
    let response = Response {
        protocol_version: PROTOCOL_VERSION,
        id: 1,
        result: Ok(Reply::Data {
            value: serde_json::value::to_raw_value(&diff).unwrap(),
        }),
    };
    drop(diff);
    let frame = encode_frame(&response).unwrap();
    drop(response);
    let response = read_frame::<Response>(&mut Cursor::new(frame))
        .unwrap()
        .unwrap();
    let diff: mt_project::git::GitDiffResult = response.result.unwrap().decode().unwrap();
    assert_eq!(diff.new_content.len(), MAX_FRAME_BYTES - 1024);
    assert!(!diff.too_large);
}

#[test]
fn error_caps_are_utf8_safe_and_unknown_survives_anyhow() {
    use mt_git_helper::{GitFailure, MAX_ERROR_BYTES};
    let unknown = GitFailure::outcome_unknown("界".repeat(MAX_ERROR_BYTES));
    let GitFailure::OutcomeUnknown { message } = &unknown else {
        panic!("typed unknown")
    };
    assert!(message.len() <= MAX_ERROR_BYTES);
    assert!(unknown.into_error().is::<GitFailure>());
    let oversized = GitFailure::Message {
        message: "x".repeat(MAX_ERROR_BYTES + 1),
    };
    assert!(
        serde_json::from_slice::<GitFailure>(&serde_json::to_vec(&oversized).unwrap()).is_err()
    );
    let failure = GitFailure::from_error(mt_project::git::StaleDiff.into());
    assert_eq!(failure, GitFailure::StaleDiff);
    assert!(failure.into_error().is::<mt_project::git::StaleDiff>());
}

#[test]
fn repository_blockers_preserve_all_owner_ids_and_bound_wire_metadata() {
    use mt_git_helper::{GitFailure, MAX_ERROR_BYTES, MAX_IDENTIFIER_BYTES, MAX_PATH_BYTES};
    let failure = GitFailure::RepositoryBlocked {
        repo: "/original/repository".into(),
        owner_project_ids: vec!["owner-a".into(), "owner-b".into()],
    };
    let frame = encode_frame(&failure).unwrap();
    let restored = read_frame::<GitFailure>(&mut Cursor::new(frame))
        .unwrap()
        .unwrap();
    assert_eq!(restored, failure);
    let text = restored.to_string();
    assert!(text.contains("/original/repository"));
    assert!(text.contains("owner-a") && text.contains("owner-b"));
    let error = restored.into_error();
    assert_eq!(error.downcast_ref::<GitFailure>(), Some(&failure));
    assert_eq!(GitFailure::from_error(error), failure);
    for value in [
        serde_json::json!({"repo": "x".repeat(MAX_PATH_BYTES + 1), "ownerProjectIds": []}),
        serde_json::json!({"repo": "/repo", "ownerProjectIds": ["x".repeat(MAX_IDENTIFIER_BYTES + 1)]}),
        serde_json::json!({"repo": "/repo", "ownerProjectIds": vec![""; MAX_ERROR_BYTES / 4 + 1]}),
    ] {
        let wire = serde_json::to_vec(&serde_json::json!({"repositoryBlocked": value})).unwrap();
        assert!(serde_json::from_slice::<GitFailure>(&wire).is_err());
        assert!(mt_git_helper::decode_json::<GitFailure>(&wire).is_err());
    }
    for owner_project_ids in [
        vec!["x".repeat(MAX_IDENTIFIER_BYTES + 1)],
        vec![String::new(); MAX_ERROR_BYTES / 4 + 1],
    ] {
        let oversized = GitFailure::RepositoryBlocked {
            repo: "/repo".into(),
            owner_project_ids,
        };
        assert!(serde_json::to_vec(&oversized).is_err());
        assert!(encode_frame(&oversized).is_err());
    }
}

#[test]
fn preflight_rejects_amplifying_collection_before_deserializer_runs() {
    struct MustNotDeserialize;
    impl<'de> serde::Deserialize<'de> for MustNotDeserialize {
        fn deserialize<D: serde::Deserializer<'de>>(_: D) -> Result<Self, D::Error> {
            panic!("typed payload must not be materialized before preflight rejects it")
        }
    }
    let wire = format!("[{}0]", "0,".repeat(mt_git_helper::MAX_COLLECTION_ITEMS));
    assert!(mt_git_helper::decode_json::<MustNotDeserialize>(wire.as_bytes()).is_err());
}

#[test]
fn encoding_metadata_is_required_canonical_and_safety_preserving() {
    use mt_project::encoding::{TextEncoding, encoding_rs};
    use mt_project::git::GitDiffResult;
    for encoding in [
        TextEncoding::Utf8,
        TextEncoding::Utf16Le,
        TextEncoding::Utf16Be,
        TextEncoding::Legacy(encoding_rs::GB18030),
        TextEncoding::Legacy(encoding_rs::SHIFT_JIS),
        TextEncoding::Legacy(encoding_rs::WINDOWS_1252),
        TextEncoding::Lossy,
    ] {
        let mut diff = mt_project::git::diff_two_texts("a\n".into(), "b\n".into());
        diff.encoding = Some(encoding);
        let bytes = serde_json::to_vec(&diff).unwrap();
        assert_eq!(
            mt_git_helper::decode_json::<GitDiffResult>(&bytes)
                .unwrap()
                .encoding,
            Some(encoding)
        );
    }
    let mut value =
        serde_json::to_value(mt_project::git::diff_two_texts("a".into(), "b".into())).unwrap();
    for name in [
        "unknown",
        "replacement",
        "UTF-7",
        "utf-8",
        "unicode",
        "iso-2022-cn",
        "windows-1252",
    ] {
        value["encoding"] = name.into();
        assert!(
            mt_git_helper::decode_json::<GitDiffResult>(&serde_json::to_vec(&value).unwrap())
                .is_err(),
            "{name}"
        );
    }
    value.as_object_mut().unwrap().remove("encoding");
    assert!(
        mt_git_helper::decode_json::<GitDiffResult>(&serde_json::to_vec(&value).unwrap()).is_err()
    );
}

#[test]
fn conflict_failures_restore_concrete_types() {
    use mt_git_helper::GitFailure;
    use mt_project::git::{SegmentBlocker, StaleConflictFile};
    for blocker in [
        SegmentBlocker::DeleteModify,
        SegmentBlocker::NotConflicted,
        SegmentBlocker::Unreadable,
        SegmentBlocker::TooLarge,
        SegmentBlocker::Binary,
        SegmentBlocker::Lossy,
        SegmentBlocker::NoMarkers,
    ] {
        let failure = GitFailure::from_error(blocker.into());
        let decoded: GitFailure =
            mt_git_helper::decode_json(&serde_json::to_vec(&failure).unwrap()).unwrap();
        assert_eq!(
            decoded.into_error().downcast_ref::<SegmentBlocker>(),
            Some(&blocker)
        );
    }
    assert!(
        GitFailure::from_error(StaleConflictFile.into())
            .into_error()
            .is::<StaleConflictFile>()
    );
}

#[test]
fn dense_conflict_choices_keep_full_file_capacity_without_array_exemption() {
    use mt_git_helper::Operation;
    let count = mt_git_helper::MAX_COLLECTION_ITEMS + 1;
    let operation = Operation::WriteConflictResolution {
        repo: "/repo".into(),
        snapshot: mt_project::git::ConflictSnapshot {
            path: "file".into(),
            digest: "proof".into(),
        },
        picks: vec![Some(mt_project::git::ConflictPick::TheirsThenOurs); count],
        mark_resolved: true,
    };
    let encoded = encode_frame(&operation).unwrap();
    let decoded: Operation = read_frame(&mut Cursor::new(encoded)).unwrap().unwrap();
    let Operation::WriteConflictResolution { picks, .. } = decoded else {
        panic!("wrong operation")
    };
    assert_eq!(picks.len(), count);
    assert!(
        picks
            .iter()
            .all(|pick| *pick == Some(mt_project::git::ConflictPick::TheirsThenOurs))
    );
    let invalid = r#"{"writeConflictResolution":{"repo":"/r","snapshot":{"path":"f","digest":"p"},"picks":"x","markResolved":false}}"#;
    assert!(mt_git_helper::decode_json::<Operation>(invalid.as_bytes()).is_err());
}
