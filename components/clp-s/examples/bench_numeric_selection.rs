//! Compare adaptive numeric candidate selection without timing input IO or validation.
//!
//! Usage: bench_numeric_selection MANIFEST COLUMN_INDEX integer|float EFFORT MODE [ZSTD_LEVEL]
//! EFFORT: fast|balanced|exhaustive|all (all is for verification).
//! MODE: verify (ordered candidate SHA-256), timing (generation plus Zstd), or encode (generation only).
//!
//! A baseline executable can be built from an otherwise identical copy of this example whose
//! four source-module paths point to a frozen source directory. Both copies must use the same
//! Cargo profile, compiler, and dependencies. The paired Python driver alternates execution order.
#![allow(dead_code)]
#[path = "../src/adaptive_float.rs"]
mod adaptive_float;
#[path = "../src/adaptive_int.rs"]
mod adaptive_int;
#[path = "../src/delta_blocks.rs"]
mod delta_blocks;
#[path = "../src/encoding_effort.rs"]
mod encoding_effort;
#[cfg(test)]
#[path = "../src/encoded_numeric.rs"]
mod encoded_numeric;

use encoding_effort::EncodingEffort;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{fs, hint::black_box, time::Instant};

fn measure(
    integers: &[i64],
    floats: &[u64],
    kind: &str,
    effort: EncodingEffort,
    mixed: bool,
    mode: &str,
    level: i32,
) -> Result<Value, Box<dyn std::error::Error>> {
    // The writer reuses its Zstd context. Construct it outside the measurement.
    let mut compressor = zstd::bulk::Compressor::new(level)?;
    let start = Instant::now();
    let mut candidates: Box<dyn Iterator<Item = Vec<u8>> + '_> = match kind {
        "integer" => Box::new(adaptive_int::candidates_with_effort(integers, effort)),
        "float" => Box::new(adaptive_float::candidates_with_effort(floats, effort)),
        _ => unreachable!(),
    };
    let mut generation_seconds = start.elapsed().as_secs_f64();
    let mut compression_seconds = 0.0;
    let mut encoded_bytes = 0usize;
    let mut best_bytes = usize::MAX;
    let mut winner = 0usize;
    let mut count = 0usize;
    let mut checks = Vec::new();
    loop {
        let next_start = Instant::now();
        let Some(payload) = candidates.next() else {
            generation_seconds += next_start.elapsed().as_secs_f64();
            break;
        };
        let scheme = payload[0];
        // Match the MixedInteger no-unsigned-values header used by these CAN columns.
        let wire = if mixed {
            let mut wire = Vec::with_capacity(payload.len() + 1);
            wire.push(0);
            wire.extend_from_slice(&payload);
            wire
        } else {
            payload
        };
        generation_seconds += next_start.elapsed().as_secs_f64();
        encoded_bytes += wire.len();
        if mode != "encode" {
            let compression_start = Instant::now();
            let compressed = compressor.compress(&wire)?;
            compression_seconds += compression_start.elapsed().as_secs_f64();
            let stored_bytes = compressed.len().min(wire.len());
            if stored_bytes < best_bytes {
                best_bytes = stored_bytes;
                winner = count;
            }
            if mode == "verify" {
                // Verification is a separate invocation; hashing is never timed.
                checks.push(json!({
                    "ordinal": count, "scheme": scheme, "encoded_bytes": wire.len(),
                    "sha256": format!("{:x}", Sha256::digest(&wire)),
                    "zstd_bytes": compressed.len(), "stored_bytes": stored_bytes,
                }));
            }
            black_box(&compressed);
        }
        black_box(&wire);
        count += 1;
    }
    let drop_start = Instant::now();
    drop(candidates);
    generation_seconds += drop_start.elapsed().as_secs_f64();
    let elapsed_seconds = start.elapsed().as_secs_f64();
    if mode == "verify" {
        Ok(json!({"effort": effort.to_string(), "candidates": checks,
                  "candidate_count": count, "best_bytes": best_bytes, "winner": winner}))
    } else {
        Ok(json!({"effort": effort.to_string(), "mode": mode,
                  "elapsed_seconds": elapsed_seconds, "generation_seconds": generation_seconds,
                  "compression_seconds": compression_seconds, "encoded_bytes": encoded_bytes,
                  "candidate_count": count,
                  "best_bytes": if mode == "encode" { None } else { Some(best_bytes) },
                  "winner": if mode == "encode" { None } else { Some(winner) }}))
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = std::env::args().collect();
    if !(6..=7).contains(&arguments.len()) {
        return Err("usage: bench_numeric_selection MANIFEST COLUMN_INDEX integer|float EFFORT verify|timing|encode [ZSTD_LEVEL]".into());
    }
    let index: usize = arguments[2].parse()?;
    let kind = arguments[3].as_str();
    if !matches!(kind, "integer" | "float") {
        return Err("kind must be integer or float".into());
    }
    let mode = arguments[5].as_str();
    if !matches!(mode, "verify" | "timing" | "encode") {
        return Err("mode must be verify, timing, or encode".into());
    }
    let efforts = match arguments[4].as_str() {
        "fast" => vec![EncodingEffort::Fast],
        "balanced" => vec![EncodingEffort::Balanced],
        "exhaustive" => vec![EncodingEffort::Exhaustive],
        "all" if mode == "verify" => vec![EncodingEffort::Fast, EncodingEffort::Balanced, EncodingEffort::Exhaustive],
        _ => return Err("invalid effort, or all used outside verification".into()),
    };
    let level: i32 = arguments.get(6).map(|s| s.parse()).transpose()?.unwrap_or(3);
    let manifest: Value = serde_json::from_slice(&fs::read(&arguments[1])?)?;
    let column = manifest["columns"].get(index).ok_or("column index out of range")?;
    let bytes = fs::read(column["path"].as_str().ok_or("missing column path")?)?;
    let hash = format!("{:x}", Sha256::digest(&bytes));
    if let Some(expected) = column["sha256"].as_str() {
        assert_eq!(hash, expected, "input digest changed");
    }
    let rows = column["rows"].as_u64().ok_or("missing row count")? as usize;
    assert_eq!(bytes.len(), rows * 8, "input length changed");
    let integers: Vec<_> = if kind == "integer" {
        bytes.chunks_exact(8).map(|v| i64::from_le_bytes(v.try_into().unwrap())).collect()
    } else { Vec::new() };
    let floats: Vec<_> = if kind == "float" {
        bytes.chunks_exact(8).map(|v| u64::from_le_bytes(v.try_into().unwrap())).collect()
    } else { Vec::new() };
    drop(bytes);
    let mixed = kind == "integer" && column["mixed_integer"].as_bool().unwrap_or(false);
    // Fixture-specific guard: prefix 0 represents no UInt64-only rows.
    if mixed && integers.iter().any(|&value| value < 0) {
        return Err("mixed benchmark fixture must contain only nonnegative Int64 values".into());
    }
    let results: Vec<_> = efforts.into_iter().map(|effort| {
        measure(&integers, &floats, kind, effort, mixed, mode, level)
    }).collect::<Result<_, _>>()?;
    println!("{}", json!({"id": column["id"], "format": column["format"], "kind": kind,
                          "rows": rows, "input_sha256": hash, "zstd_level": level,
                          "results": results}));
    Ok(())
}
