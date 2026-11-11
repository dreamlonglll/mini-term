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
