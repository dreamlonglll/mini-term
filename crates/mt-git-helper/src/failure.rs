use mt_project::git::{StaleDiff, WorktreeRemoveError};
use serde::{Deserialize, Serialize};

/// Business failures retain their concrete type across the helper boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum GitFailure {
    Message { message: String },
    StaleDiff,
    WorktreeInUse { path: String, busy: String },
    WorktreeLeftover { path: String, detail: String },
}

impl GitFailure {
    pub fn from_error(error: anyhow::Error) -> Self {
        if error.is::<StaleDiff>() {
            return Self::StaleDiff;
        }
        match error.downcast_ref::<WorktreeRemoveError>() {
            Some(WorktreeRemoveError::InUse { path, busy }) =>
                Self::WorktreeInUse { path: path.clone(), busy: busy.clone() },
            Some(WorktreeRemoveError::Leftover { path, detail }) =>
                Self::WorktreeLeftover { path: path.clone(), detail: detail.clone() },
            None => Self::Message { message: format!("{error:#}") },
        }
    }

    pub fn into_error(self) -> anyhow::Error {
        match self {
            Self::Message { message } => anyhow::Error::msg(message),
            Self::StaleDiff => StaleDiff.into(),
            Self::WorktreeInUse { path, busy } => WorktreeRemoveError::InUse { path, busy }.into(),
            Self::WorktreeLeftover { path, detail } => WorktreeRemoveError::Leftover { path, detail }.into(),
        }
    }
}

impl From<String> for GitFailure {
    fn from(message: String) -> Self { Self::Message { message } }
}

impl From<&str> for GitFailure {
    fn from(message: &str) -> Self { Self::from(message.to_owned()) }
}

impl std::fmt::Display for GitFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message { message } => formatter.write_str(message),
            Self::StaleDiff => StaleDiff.fmt(formatter),
            Self::WorktreeInUse { path, busy } =>
                write!(formatter, "worktree 目录正被占用,未做任何改动: {path}(占用处: {busy})"),
            Self::WorktreeLeftover { path, detail } =>
                write!(formatter, "worktree 登记已删除,但目录未能删除: {path}: {detail}"),
        }
    }
}

impl std::error::Error for GitFailure {}
