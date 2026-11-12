use mt_project::git::{SegmentBlocker, StaleConflictFile, StaleDiff, WorktreeRemoveError};
use serde::{Deserialize, Serialize};

/// Business failures retain their concrete type across the helper boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum GitFailure {
    Message {
        #[serde(deserialize_with = "bounded_error")]
        message: String,
    },
    OutcomeUnknown {
        #[serde(deserialize_with = "bounded_error")]
        message: String,
    },
    Service {
        #[serde(deserialize_with = "bounded_identifier")]
        code: String,
        #[serde(deserialize_with = "bounded_error")]
        message: String,
    },
    StaleDiff,
    StaleConflictFile,
    SegmentBlocked {
        blocker: SegmentBlocker,
    },
    WorktreeInUse {
        #[serde(deserialize_with = "bounded_path")]
        path: String,
        #[serde(deserialize_with = "bounded_error")]
        busy: String,
    },
    WorktreeLeftover {
        #[serde(deserialize_with = "bounded_path")]
        path: String,
        #[serde(deserialize_with = "bounded_error")]
        detail: String,
    },
}

impl GitFailure {
    pub fn from_error(error: anyhow::Error) -> Self {
        if let Some(failure) = error.downcast_ref::<GitFailure>() {
            return failure.clone();
        }
        if error.is::<StaleDiff>() {
            return Self::StaleDiff;
        }
        if error.is::<StaleConflictFile>() {
            return Self::StaleConflictFile;
        }
        if let Some(blocker) = error.downcast_ref::<SegmentBlocker>() {
            return Self::SegmentBlocked { blocker: *blocker };
        }
        match error.downcast_ref::<WorktreeRemoveError>() {
            Some(WorktreeRemoveError::InUse { path, busy }) => Self::WorktreeInUse {
                path: capped(path, crate::MAX_PATH_BYTES),
                busy: capped(busy, crate::MAX_ERROR_BYTES),
            },
            Some(WorktreeRemoveError::Leftover { path, detail }) => Self::WorktreeLeftover {
                path: capped(path, crate::MAX_PATH_BYTES),
                detail: capped(detail, crate::MAX_ERROR_BYTES),
            },
            None => Self::Message {
                message: bounded_format(format_args!("{error:#}")),
            },
        }
    }

    pub fn into_error(self) -> anyhow::Error {
        match self {
            Self::Message { message } => anyhow::Error::msg(message),
            unknown @ Self::OutcomeUnknown { .. } => unknown.into(),
            service @ Self::Service { .. } => service.into(),
            Self::StaleDiff => StaleDiff.into(),
            Self::StaleConflictFile => StaleConflictFile.into(),
            Self::SegmentBlocked { blocker } => blocker.into(),
            Self::WorktreeInUse { path, busy } => WorktreeRemoveError::InUse { path, busy }.into(),
            Self::WorktreeLeftover { path, detail } => {
                WorktreeRemoveError::Leftover { path, detail }.into()
            }
        }
    }
}

impl From<String> for GitFailure {
    fn from(message: String) -> Self {
        Self::Message {
            message: capped(&message, crate::MAX_ERROR_BYTES),
        }
    }
}

impl From<&str> for GitFailure {
    fn from(message: &str) -> Self {
        Self::from(message.to_owned())
    }
}

impl std::fmt::Display for GitFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message { message } => formatter.write_str(message),
            Self::OutcomeUnknown { message } => {
                write!(formatter, "remote outcome unknown: {message}")
            }
            Self::Service { code, message } => write!(formatter, "{code}: {message}"),
            Self::StaleDiff => StaleDiff.fmt(formatter),
            Self::StaleConflictFile => StaleConflictFile.fmt(formatter),
            Self::SegmentBlocked { blocker } => blocker.fmt(formatter),
            Self::WorktreeInUse { path, busy } => write!(
                formatter,
                "worktree 目录正被占用,未做任何改动: {path}(占用处: {busy})"
            ),
            Self::WorktreeLeftover { path, detail } => write!(
                formatter,
                "worktree 登记已删除,但目录未能删除: {path}: {detail}"
            ),
        }
    }
}

impl std::error::Error for GitFailure {}

impl GitFailure {
    pub fn outcome_unknown(message: impl std::fmt::Display) -> Self {
        Self::OutcomeUnknown {
            message: bounded_format(format_args!("{message}")),
        }
    }
    pub fn service(code: &str, message: impl std::fmt::Display) -> Self {
        Self::Service {
            code: capped(code, crate::MAX_IDENTIFIER_BYTES),
            message: bounded_format(format_args!("{message}")),
        }
    }
}

fn capped(text: &str, cap: usize) -> String {
    let mut end = text.len().min(cap);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn bounded_format(args: std::fmt::Arguments<'_>) -> String {
    struct Output(String);
    impl std::fmt::Write for Output {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            let left = crate::MAX_ERROR_BYTES - self.0.len();
            let mut end = text.len().min(left);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            self.0.push_str(&text[..end]);
            if end < text.len() {
                Err(std::fmt::Error)
            } else {
                Ok(())
            }
        }
    }
    let mut output = Output(String::new());
    let _ = std::fmt::write(&mut output, args);
    output.0
}

fn bounded_string<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
    cap: usize,
) -> Result<String, D::Error> {
    struct Text(usize);
    impl serde::de::Visitor<'_> for Text {
        type Value = String;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("bounded Git error field")
        }
        fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<String, E> {
            if text.len() > self.0 {
                return Err(E::custom("Git error field exceeds limit"));
            }
            Ok(text.to_owned())
        }
    }
    deserializer.deserialize_str(Text(cap))
}
fn bounded_error<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    bounded_string(d, crate::MAX_ERROR_BYTES)
}
fn bounded_path<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    bounded_string(d, crate::MAX_PATH_BYTES)
}
fn bounded_identifier<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    bounded_string(d, crate::MAX_IDENTIFIER_BYTES)
}
