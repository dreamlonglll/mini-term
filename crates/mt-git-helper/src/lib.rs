//! Small, synchronous remote Git helper. The selected workspace is a repository
//! discovery context, not an OS sandbox: Git configuration, filters and hooks run
//! with the helper user's privileges. Requests are sequential and never replayed.
//!
//! Frames are a four-byte big-endian JSON byte length followed by UTF-8 JSON.
//! Struct fields and enum kinds use camelCase; `Response.result` uses serde's
//! external Result representation (`{"Ok": reply}` / `{"Err": typed_failure}`).
//!
//! Protocol v5 adds atomic multi-hunk/line writes and compact conflict snapshots.
//! Typed query/mutation operations cover the desktop Git backend:
//! - PanelSnapshot → git::RepoPanelSnapshot; ChangesSnapshot → ChangesSnapshot.
//! - Log → HistoryPage (the next cursor is opaque, store and send it unchanged).
//! - CommitDetail → git::CommitDetail; CommitFiles → Vec<git::CommitFileInfo>;
//!   CommitDiff → git::GitDiffResult; CommitWith → git::CommitOutcome.
//! - Checkout → Option<git::RemoteCheckout> (None for a local checkout);
//!   DeleteBranch → git::DeleteOutcome; Fetch → git::FetchOutcome;
//!   Pull → git::PullOutcome; PushPreview → PushPreview; Push → git::PushOutcome.
//! - ListStashes → Vec<git::StashInfo>; SaveStash → git::StashSaveOutcome;
//!   ApplyStash → git::StashApplyOutcome; OperationContinue → git::RepoOpState.
//! - Discard, all hunk operations, CreateBranch, RenameBranch, DropStash,
//!   OperationAbort, ResolveConflict and MarkResolved → ().
//!
//! Operations and replies use serde's external enum representation, so decoding
//! never buffers an internally tagged enum's complete payload.
//! Branch/sync/stash conflict and warning outcomes are returned unchanged; a
//! successful RPC transport alone does not mean the requested mutation finished.
//!
//! `Reply::Data` retains raw JSON until its operation-specific typed decode; neither
//! endpoint builds a JSON Value tree. Frame preflight bounds container cardinality,
//! total nodes, paths and identifiers before allocating typed collections. Payload
//! strings remain bounded by the 16 MiB frame, not an arbitrary smaller diff cap.
//! Failures cap diagnostic fields at 16 KiB. A dispatched write whose response cannot
//! be encoded is `OutcomeUnknown`, not a known Git rejection. Barrier receipts are
//! valid only on the original sequential helper stream and for its last write ID;
//! reads or receipts from a newly started helper never prove the old process ended.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use mt_project::git::{
    self, ConflictSide, GitDiffResult, GitLogCursor, LogFilter, PullMode, PushOptions, RepoOpKind,
    StashSaveOptions,
};
mod failure;
mod limits;
mod operations;
mod server;
pub use failure::GitFailure;
pub use limits::{
    MAX_COLLECTION_ITEMS, MAX_ERROR_BYTES, MAX_IDENTIFIER_BYTES, MAX_PATH_BYTES, decode_json,
};
pub use operations::{GitMutation, GitQuery};
pub use server::serve;

pub const PROTOCOL_VERSION: u32 = 5;
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    pub protocol_version: u32,
    pub helper_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub protocol_version: u32,
    pub id: u64,
    pub operation: Operation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Response {
    pub protocol_version: u32,
    pub id: u64,
    pub result: Result<Reply, GitFailure>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DiffSide {
    Staged,
    Unstaged,
    Working,
}

impl From<DiffSide> for mt_project::git::DiffSide {
    fn from(value: DiffSide) -> Self {
        match value {
            DiffSide::Staged => Self::Staged,
            DiffSide::Unstaged => Self::Unstaged,
            DiffSide::Working => Self::Working,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Operation {
    Discover,
    Barrier {
        completed_id: u64,
    },
    Status {
        repo: String,
    },
    Diff {
        repo: String,
        path: String,
        old_path: Option<String>,
        side: DiffSide,
    },
    Stage {
        repo: String,
        paths: Vec<String>,
    },
    Unstage {
        repo: String,
        paths: Vec<String>,
    },
    StageAll {
        repo: String,
    },
    UnstageAll {
        repo: String,
    },
    Commit {
        repo: String,
        message: String,
    },
    PanelSnapshot {
        repo: String,
        recent_limit: usize,
    },
    ChangesSnapshot {
        repo: String,
    },
    Log {
        repo: String,
        after: Option<GitLogCursor>,
        limit: usize,
        branch: Option<String>,
        filter: Option<LogFilter>,
    },
    CommitDetail {
        repo: String,
        hash: String,
    },
    CommitFiles {
        repo: String,
        hash: String,
    },
    CommitDiff {
        repo: String,
        hash: String,
        path: String,
        old_path: Option<String>,
    },
    CommitWith {
        repo: String,
        message: String,
        amend: bool,
    },
    Discard {
        repo: String,
        paths: Vec<String>,
    },
    StageHunk {
        repo: String,
        path: String,
        diff: GitDiffResult,
        hunk: usize,
    },
    UnstageHunk {
        repo: String,
        path: String,
        diff: GitDiffResult,
        hunk: usize,
    },
    DiscardHunk {
        repo: String,
        path: String,
        diff: GitDiffResult,
        hunk: usize,
    },
    StageHunks {
        repo: String,
        path: String,
        diff: GitDiffResult,
        picks: Vec<git::HunkPick>,
    },
    UnstageHunks {
        repo: String,
        path: String,
        diff: GitDiffResult,
        picks: Vec<git::HunkPick>,
    },
    DiscardHunks {
        repo: String,
        path: String,
        diff: GitDiffResult,
        picks: Vec<git::HunkPick>,
    },
    WriteConflictResolution {
        repo: String,
        snapshot: git::ConflictSnapshot,
        #[serde(with = "conflict_picks")]
        picks: Vec<Option<git::ConflictPick>>,
        mark_resolved: bool,
    },
    Checkout {
        repo: String,
        branch: String,
        remote: bool,
    },
    CreateBranch {
        repo: String,
        name: String,
        start: Option<String>,
        checkout: bool,
    },
    RenameBranch {
        repo: String,
        old: String,
        name: String,
    },
    DeleteBranch {
        repo: String,
        name: String,
        force: bool,
    },
    Fetch {
        repo: String,
    },
    Pull {
        repo: String,
        mode: Option<PullMode>,
    },
    PushPreview {
        repo: String,
        branch: Option<String>,
        limit: usize,
    },
    Push {
        repo: String,
        options: PushOptions,
    },
    ListStashes {
        repo: String,
    },
    SaveStash {
        repo: String,
        options: StashSaveOptions,
    },
    ApplyStash {
        repo: String,
        index: usize,
        hash: String,
        pop: bool,
        reinstate: bool,
    },
    DropStash {
        repo: String,
        index: usize,
        hash: String,
    },
    OperationContinue {
        repo: String,
        #[serde(rename = "operationKind")]
        kind: RepoOpKind,
    },
    OperationAbort {
        repo: String,
        #[serde(rename = "operationKind")]
        kind: RepoOpKind,
    },
    ResolveConflict {
        repo: String,
        path: String,
        side: ConflictSide,
    },
    MarkResolved {
        repo: String,
        path: String,
    },
    Query {
        repo: String,
        query: GitQuery,
    },
    Mutate {
        repo: String,
        mutation: GitMutation,
    },
}

impl Operation {
    /// Exhaustive classification: a new operation must explicitly choose whether
    /// an interrupted request can have changed the repository.
    pub fn is_write(&self) -> bool {
        match self {
            Self::Barrier { .. } => false,
            Self::Discover
            | Self::Status { .. }
            | Self::Diff { .. }
            | Self::Query { .. }
            | Self::PanelSnapshot { .. }
            | Self::ChangesSnapshot { .. }
            | Self::Log { .. }
            | Self::CommitDetail { .. }
            | Self::CommitFiles { .. }
            | Self::CommitDiff { .. }
            | Self::PushPreview { .. }
            | Self::ListStashes { .. } => false,
            Self::Stage { .. }
            | Self::Unstage { .. }
            | Self::StageAll { .. }
            | Self::UnstageAll { .. }
            | Self::Commit { .. }
            | Self::CommitWith { .. }
            | Self::Discard { .. }
            | Self::StageHunk { .. }
            | Self::UnstageHunk { .. }
            | Self::DiscardHunk { .. }
            | Self::StageHunks { .. }
            | Self::UnstageHunks { .. }
            | Self::DiscardHunks { .. }
            | Self::WriteConflictResolution { .. }
            | Self::Checkout { .. }
            | Self::CreateBranch { .. }
            | Self::RenameBranch { .. }
            | Self::DeleteBranch { .. }
            | Self::Fetch { .. }
            | Self::Pull { .. }
            | Self::Push { .. }
            | Self::SaveStash { .. }
            | Self::Mutate { .. }
            | Self::ApplyStash { .. }
            | Self::DropStash { .. }
            | Self::OperationContinue { .. }
            | Self::OperationAbort { .. }
            | Self::ResolveConflict { .. }
            | Self::MarkResolved { .. } => true,
        }
    }

    pub fn expected_reply_kind(&self) -> &'static str {
        match self {
            Self::Discover => "repositories",
            Self::Barrier { .. } => "completed",
            Self::Status { .. } => "status",
            Self::Diff { .. } => "diff",
            Self::Query { .. } | Self::Mutate { .. } => "data",
            Self::Stage { .. }
            | Self::Unstage { .. }
            | Self::StageAll { .. }
            | Self::UnstageAll { .. } => "updated",
            Self::Commit { .. } => "committed",
            Self::PanelSnapshot { .. }
            | Self::ChangesSnapshot { .. }
            | Self::Log { .. }
            | Self::CommitDetail { .. }
            | Self::CommitFiles { .. }
            | Self::CommitDiff { .. }
            | Self::CommitWith { .. }
            | Self::Discard { .. }
            | Self::StageHunk { .. }
            | Self::UnstageHunk { .. }
            | Self::DiscardHunk { .. }
            | Self::StageHunks { .. }
            | Self::UnstageHunks { .. }
            | Self::DiscardHunks { .. }
            | Self::WriteConflictResolution { .. }
            | Self::Checkout { .. }
            | Self::CreateBranch { .. }
            | Self::RenameBranch { .. }
            | Self::DeleteBranch { .. }
            | Self::Fetch { .. }
            | Self::Pull { .. }
            | Self::PushPreview { .. }
            | Self::Push { .. }
            | Self::ListStashes { .. }
            | Self::SaveStash { .. }
            | Self::ApplyStash { .. }
            | Self::DropStash { .. }
            | Self::OperationContinue { .. }
            | Self::OperationAbort { .. }
            | Self::ResolveConflict { .. }
            | Self::MarkResolved { .. } => "data",
        }
    }

    /// Smart checkout can run stash, checkout and restore, each with its own
    /// core timeout. Never time out the transport before that business budget.
    pub fn needs_smart_checkout_timeout(&self) -> bool {
        matches!(
            self,
            Self::Mutate {
                mutation: GitMutation::CheckoutWithStash { .. },
                ..
            }
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Reply {
    Completed {
        completed_id: u64,
    },
    Repositories {
        repos: Vec<mt_project::git::GitRepoInfo>,
    },
    Status {
        files: Vec<mt_project::git::ChangeFileStatus>,
    },
    Diff {
        diff: mt_project::git::GitDiffResult,
    },
    Updated,
    Committed {
        output: String,
    },
    Data {
        value: Box<serde_json::value::RawValue>,
    },
}

impl Reply {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Completed { .. } => "completed",
            Self::Repositories { .. } => "repositories",
            Self::Status { .. } => "status",
            Self::Diff { .. } => "diff",
            Self::Updated => "updated",
            Self::Committed { .. } => "committed",
            Self::Data { .. } => "data",
        }
    }

    /// Decode new desktop operations without duplicating protocol data types or
    /// silently accepting a mismatched original reply variant.
    pub fn decode<T: DeserializeOwned>(self) -> Result<T, String> {
        match self {
            Self::Data { value } => decode_json(value.get().as_bytes()).map_err(|error| {
                format!(
                    "invalid Git reply data for {} ({:?}, line {}, column {})",
                    std::any::type_name::<T>(),
                    error.classify(),
                    error.line(),
                    error.column()
                )
            }),
            other => Err(format!(
                "expected Git data reply, received {}",
                other.kind()
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangesSnapshot {
    pub files: Vec<git::ChangeFileStatus>,
    pub conflicts: Vec<git::ConflictInfo>,
    pub operation: git::RepoOpState,
    pub target: git::CommitTarget,
    pub has_head: bool,
    pub amend: Option<git::AmendInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryPage {
    pub commits: Vec<git::GitCommitInfo>,
    pub next: Option<GitLogCursor>,
    pub scanned: usize,
    pub tags: Vec<git::TagInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PushPreview {
    pub plan: git::PushPlan,
    pub outgoing: git::OutgoingCommits,
}

// Serialize directly into the bounded frame buffer, not an unbounded intermediate
// JSON String. A failed encoding never writes a partial frame to the transport.
struct FrameBuffer(Vec<u8>);

impl Write for FrameBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > (MAX_FRAME_BYTES + 4).saturating_sub(self.0.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "JSON frame exceeds MAX_FRAME_BYTES",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn encode_frame<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    let mut buffer = FrameBuffer(vec![0; 4]);
    serde_json::to_writer(&mut buffer, value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    limits::validate_json(&buffer.0[4..])
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let length = (buffer.0.len() - 4) as u32;
    buffer.0[..4].copy_from_slice(&length.to_be_bytes());
    Ok(buffer.0)
}

/// Clean EOF is accepted only before a frame header. Partial headers/payloads,
/// zero-length frames, oversized frames and invalid JSON are protocol errors.
pub fn read_frame<T: DeserializeOwned>(reader: &mut impl Read) -> io::Result<Option<T>> {
    let mut header = [0; 4];
    loop {
        match reader.read(&mut header[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    reader.read_exact(&mut header[1..])?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid JSON frame length",
        ));
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload)?;
    decode_json(&payload)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub fn write_frame<T: Serialize>(writer: &mut impl Write, value: &T) -> io::Result<()> {
    writer.write_all(&encode_frame(value)?)
}

// A supported 8 MiB file can contain more tiny conflict blocks than the generic
// JSON collection cap. One ASCII byte per choice retains that capability
// without relaxing preflight allocation guards for any array.
mod conflict_picks {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        picks: &[Option<git::ConflictPick>],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let encoded: String = picks
            .iter()
            .map(|pick| match pick {
                None => '.',
                Some(git::ConflictPick::Ours) => 'o',
                Some(git::ConflictPick::Theirs) => 't',
                Some(git::ConflictPick::OursThenTheirs) => 'b',
                Some(git::ConflictPick::TheirsThenOurs) => 'r',
            })
            .collect();
        serializer.serialize_str(&encoded)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<Option<git::ConflictPick>>, D::Error> {
        struct Picks;
        impl serde::de::Visitor<'_> for Picks {
            type Value = Vec<Option<git::ConflictPick>>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("compact conflict choices")
            }
            fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<Self::Value, E> {
                // The three marker lines occupy at least 23 bytes.
                if text.len() > git::MAX_SEGMENT_FILE_BYTES as usize / 23 {
                    return Err(E::custom("too many conflict choices"));
                }
                text.bytes()
                    .map(|byte| {
                        Ok(match byte {
                            b'.' => None,
                            b'o' => Some(git::ConflictPick::Ours),
                            b't' => Some(git::ConflictPick::Theirs),
                            b'b' => Some(git::ConflictPick::OursThenTheirs),
                            b'r' => Some(git::ConflictPick::TheirsThenOurs),
                            _ => return Err(E::custom("invalid conflict choice")),
                        })
                    })
                    .collect()
            }
        }
        deserializer.deserialize_str(Picks)
    }
}
