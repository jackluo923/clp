//! Library-first implementation of CLP structured archives.
//!
//! The public API is intentionally independent of command-line parsing, logging setup, and
//! process-global state. The `clp-s` command-line program is a separate adapter over this crate.

#![forbid(unsafe_code)]

pub mod archive;
mod extract;
mod extraction_plan;
pub mod ingest;
pub mod json;
pub mod json_number;
mod log_order;
#[cfg(feature = "network")]
pub mod network;
mod ordered_merge;
mod record;
pub mod search;
pub mod timestamp;
pub mod timestamp_catalog;
pub mod writer;

pub use extract::ArchiveReader;
pub use extract::ExtractionError;
pub use extract::ExtractionLimits;
pub use extract::ExtractionMode;
pub use extract::ExtractionOptions;
pub use extract::ExtractionResource;
pub use extract::ExtractionStats;
pub use extract::JsonlRecord;
pub use extract::JsonlRecordSink;
pub use extract::OrderedRetentionLimits;
pub use extract::extract_jsonl;
pub use extract::extract_jsonl_records;
pub use extraction_plan::ExtractionOp;
pub use extraction_plan::ExtractionPlan;
pub use extraction_plan::ExtractionPlanError;
pub use extraction_plan::ExtractionPlanLimits;
pub use extraction_plan::ExtractionPlanResource;
pub use extraction_plan::ExtractionPosition;
pub use log_order::LOG_EVENT_IDX_KEY;
pub use log_order::LogOrderColumn;
pub use log_order::LogOrderCursor;
pub use log_order::LogOrderError;
pub use log_order::LogOrderLocator;
pub use log_order::locate_log_order_column;
pub use ordered_merge::OrderedMergeError;
pub use ordered_merge::OrderedMergeInvariant;
pub use ordered_merge::OrderedMergeLimits;
pub use ordered_merge::OrderedMergeResource;
pub use ordered_merge::OrderedMergeTable;
pub use ordered_merge::OrderedRow;
pub use ordered_merge::OrderedRowMerge;
pub use record::RecordBindError;
pub use record::RecordCompileError;
pub use record::RecordError;
pub use record::RecordLimits;
pub use record::RecordProgram;
pub use record::RecordResource;
pub use record::RecordScratch;
pub use record::RecordWriter;
