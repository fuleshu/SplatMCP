//! Loopback bridge between the SplatMCP desktop app and the SplatMCP MCP server.
//!
//! The MCP server is spawned by an MCP client and speaks MCP over stdio, so it
//! cannot reach the desktop window directly. The desktop app therefore hosts a
//! small line-delimited JSON service on `127.0.0.1` and publishes its port plus a
//! random token in `bridge.json` inside the app data directory. The MCP server
//! reads that file, connects, and asks the app to move the camera, capture a frame
//! or load splat bytes into the viewer.
//!
//! Modules:
//! - [`protocol`] - request/response types shared by both sides
//! - [`wire`] - one request per line framing with a hard size limit
//! - [`server`] - the service the app hosts
//! - [`client`] - the side the MCP server uses
//! - [`paths`] - app data location both sides agree on

pub mod client;
pub mod paths;
pub mod protocol;
pub mod server;
pub mod wire;

pub use client::BridgeClient;
pub use paths::{app_data_dir, bridge_descriptor_path, ensure_app_data_dir, settings_path};
pub use protocol::{
    AssetInfoReply, AssetQueryRequest, AssetRegisterReply, AssetRegisterRequest,
    AssetReleaseRequest, AssetStatsSummary, AssetSummary, AssetUploadBeginRequest,
    AssetUploadChunkRequest, AssetUploadReply, AssetUploadRequest, AttributePatchParams,
    AuthoringNote, BatchOpParams, BatchPointParams, BoundsInfo, BoundsSummary, BridgeDescriptor,
    CameraRequest, CameraState, CaptureRequest, CaptureResult, CaptureViewOutcomeReply,
    CaptureViewReply, CaptureViewRequest, CaptureViewsReply, CaptureViewsRequest,
    CommitPreviewRequest,
    ComponentSummary, ComponentsReply, ComponentsRequest, ContactSheetReply,
    DistributionSummary, DocumentPlyReply,
    DocumentReply, DocumentSummary, DocumentTargetRequest, EditBatchReply, EditBatchRequest,
    ExportSummary, GetPlyRequest, HistoryReply, HistoryStepSummary, InspectRequest, InspectResult,
    InspectionSummary, JobAdmissionReply, JobCancelRequest, JobFailureSummary, JobListReply,
    JobListRequest, JobLogSummary, JobStatsSummary, JobStatusReply, JobStatusRequest,
    JobSubmitRequest, JobSummary, LoadPlyRequest, Method, PROTOCOL_VERSION, PlyImportSummary,
    PreviewSummary, PublicationCapabilitiesReply, PublicationFailureSummary,
    PublicationOutcomeSummary, PublicationRequestSummary, PublicationStatusReply,
    PublicationStatusRequest, PythonCancelRequest, PythonErrorReport, PythonJobQuery,
    PythonRunRequest, ReloadRequest, Request, Response, RetentionSummary, RevisionSummary,
    SelectionParams, SelectionSummary, SetComponentRequest, SideEffectSummary, StepSummary,
    TransformSummary, ViewerStatus,
};
pub use protocol::{
    asset_load_params, capture_view_request, capture_views_request, load_ply_params,
    replace_ply_params,
};
pub use server::{BridgeServer, BridgeService, Handler};
pub use wire::{MAX_FRAME_BYTES, read_line_limited, read_message, write_message};

use thiserror::Error;

/// Everything that can go wrong while using the bridge.
#[derive(Debug, Error)]
pub enum BridgeError {
    #[error("bridge io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("bridge protocol error: {0}")]
    Protocol(String),
    #[error("bridge frame exceeded the {max} byte limit")]
    FrameTooLarge { max: usize },
    #[error(
        "the desktop app rejected the bridge token; restart SplatMCP so it publishes a fresh bridge.json"
    )]
    Unauthorized,
    #[error("no running SplatMCP desktop app: {path} was not found")]
    AppNotRunning { path: String },
    #[error("the desktop app did not answer within {timeout_ms} ms")]
    Timeout { timeout_ms: u64 },
    #[error("the desktop app reported: {0}")]
    Remote(String),
    #[error("unsupported bridge protocol version {found}, this build speaks {expected}")]
    UnsupportedProtocol { found: u32, expected: u32 },
}

pub type Result<T> = std::result::Result<T, BridgeError>;

impl BridgeError {
    /// True when the failure means "start the desktop app", which callers turn
    /// into an actionable tool error.
    pub fn is_app_missing(&self) -> bool {
        matches!(
            self,
            BridgeError::AppNotRunning { .. }
                | BridgeError::Unauthorized
                | BridgeError::UnsupportedProtocol { .. }
        )
    }

    /// Maps a socket error to a timeout when the socket ran out of patience.
    pub(crate) fn from_io(error: std::io::Error, timeout: std::time::Duration) -> Self {
        match error.kind() {
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => BridgeError::Timeout {
                timeout_ms: timeout.as_millis() as u64,
            },
            _ => BridgeError::Io(error),
        }
    }
}
