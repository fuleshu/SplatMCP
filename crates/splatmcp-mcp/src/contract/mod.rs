//! The MCP-side contract: typed results, structured errors and capability negotiation.
//!
//! Three rules shape this module, and they are what a client branches on instead of parsing prose.
//!
//! 1. **Every reply is structured as well as readable.** A tool returns the same data twice: once
//!    as the JSON `structuredContent` a client can read by field, and once as the compact text a
//!    model reads. Nothing structured is stringified into a lone text block.
//! 2. **Every failure carries a stable code, a layer, a retryability answer and the commit state.**
//!    A timeout after a mutation stays `outcome: unknown`; it is never dressed up as a rollback.
//! 3. **Capabilities come from the app.** The limits a caller is held to are the ones the
//!    component that enforces them reports, so a tool description never becomes the manual.
//!
//! The result envelope is deliberately small: a correlation id, the status, and the payload. It is
//! applied to the operations that report state (capabilities, captures) rather than wrapped around
//! every legacy reply, which would break the existing field names for no gain.

pub mod envelope;
pub mod error;
pub mod limits;

pub use envelope::{Correlation, Envelope, Status, ToolOutput};
pub use error::{ErrorCategory, ErrorCode, ErrorLayer, Failure, OutcomeState, classify};
pub use limits::{ReportedLimits, capture_limits, describe_budget};
