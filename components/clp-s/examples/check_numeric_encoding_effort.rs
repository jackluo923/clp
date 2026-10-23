//! Verify production numeric encoding menus and their stored sizes on extracted columns.
//! Usage: check_numeric_encoding_effort FLOAT_MANIFEST INTEGER_MANIFEST OUTDIR
//! Every candidate is Zstd-compressed, inflated, decoded, and checked bit-for-bit. This is a
//! size/correctness check, not a timing benchmark; no experimental codecs are imported.
#![allow(dead_code)]
#[path = "../src/adaptive_float.rs"]
mod adaptive_float;
#[path = "../src/adaptive_int.rs"]
mod adaptive_int;
#[path = "../src/delta_blocks.rs"]
mod delta_blocks;
#[cfg(test)]
#[path = "../src/encoded_numeric.rs"]
mod encoded_numeric;
#[path = "../src/encoding_effort.rs"]
mod encoding_effort;

use encoding_effort::EncodingEffort;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, io::Write, path::Path};

fn integer_name(payload: &[u8]) -> &'static str {
    match payload[0] {
        0 => "plain",
        1 => "for",
        2 => "legacy_delta",
        3 => "dictionary",
        4 => "rle",
        5 => "constant",
        6 => match payload[9] {
            64 => "fixed_delta",
            255 => "local_packed_delta",
            _ => "packed_delta",
        },
        tag => panic!("unknown integer scheme {tag}"),
    }
}

fn float_name(payload: &[u8], dictionaries: &mut usize) -> String {
    match payload[0] {
        0 => "plain".into(),
        1 => "byte_stream_split".into(),
        2 => {
            *dictionaries += 1;
            if *dictionaries == 1 {
                "dictionary"
            } else {
                "sorted_dictionary"
            }
            .into()
        }
        3 => "constant".into(),
        4 => "rle".into(),
        5 => {
            let arithmetic = if payload[1] & 0x40 != 0 {
                "multiply"
            } else {
                "divide"
            };
            let encoding = if payload[1] & 0x20 != 0 {
                "byte_planes"
            } else {
                integer_name(&payload[2..])
            };
            format!(
                "decimal_{arithmetic}_{encoding}_scale{}{}",
                payload[1] & 0x0f,
                if payload[1] & 0x80 != 0 {
                    "_float32"
                } else {
                    ""
                }
            )
        }
        tag => panic!("unknown float scheme {tag}"),
    }
}

fn decode_and_check(payload: &[u8], raw: &[u8], is_float: bool, mixed: bool) {
    let payload = if mixed {
        assert_eq!(payload[0], 0, "CAN clock has no large unsigned bitmap");
        &payload[1..]
    } else {
        payload
    };
    let mut restored = Vec::new();
    if is_float {
        adaptive_float::decode_to_le(payload, raw.len() / 8, &mut restored).unwrap();
    } else {
        adaptive_int::decode_to_le(payload, raw.len() / 8, &mut restored).unwrap();
    }
    assert_eq!(
        restored, raw,
        "decoded bits differ from the extracted column"
    );
}

#[derive(Default)]
struct Totals {
    columns: u64,
    rows: u64,
    baseline_bytes: u64,
    stored_bytes: u64,
    candidates: u64,
    improved_columns: u64,
    new_float_winners: u64,
    winners: BTreeMap<String, u64>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        return Err(
            "usage: check_numeric_encoding_effort FLOAT_MANIFEST INTEGER_MANIFEST OUTDIR".into(),
        );
    }
    let out = Path::new(&args[3]);
    fs::create_dir_all(out)?;
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out.join("checks.jsonl"))?;
    let mut totals: BTreeMap<String, Totals> = BTreeMap::new();
    let mut compressor = zstd::bulk::Compressor::new(3)?;
    let mut decompressor = zstd::bulk::Decompressor::new()?;
    let mut total_columns = 0;
    for (manifest_path, is_float) in [(&args[1], true), (&args[2], false)] {
        let manifest: Value = serde_json::from_slice(&fs::read(manifest_path)?)?;
        for column in manifest["columns"]
            .as_array()
            .ok_or("manifest columns missing")?
        {
            let raw = fs::read(column["path"].as_str().ok_or("column path missing")?)?;
            assert_eq!(raw.len() % 8, 0);
            let rows = raw.len() / 8;
            assert_eq!(
                rows as u64,
                column["rows"].as_u64().ok_or("column rows missing")?
            );
            let hash = format!("{:x}", Sha256::digest(&raw));
            if let Some(expected) = column["sha256"].as_str() {
                assert_eq!(hash, expected);
            }
            let mixed = !is_float && column["mixed_integer"].as_bool().unwrap_or(false);
            let original = fs::read(
                column["encoded_path"]
                    .as_str()
                    .ok_or("encoded_path missing")?,
            )?;
            decode_and_check(&original, &raw, is_float, mixed);
            let baseline_bytes = column["current_compressed_bytes"]
                .as_u64()
                .ok_or("baseline bytes missing")?;
            assert!(!column["shared_frame"].as_bool().unwrap_or(true));
            assert_eq!(
                compressor.compress(&original)?.len().min(original.len()) as u64,
                baseline_bytes,
                "original frame size changed for {}",
                column["id"]
            );
            let float_bits: Vec<_> = if is_float {
                raw.chunks_exact(8)
                    .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
                    .collect()
            } else {
                Vec::new()
            };
            let integers: Vec<_> = if !is_float {
                raw.chunks_exact(8)
                    .map(|b| i64::from_le_bytes(b.try_into().unwrap()))
                    .collect()
            } else {
                Vec::new()
            };
            for effort in [
                EncodingEffort::Fast,
                EncodingEffort::Balanced,
                EncodingEffort::Exhaustive,
            ] {
                let candidates: Box<dyn Iterator<Item = Vec<u8>> + '_> = if is_float {
                    Box::new(adaptive_float::candidates_with_effort(&float_bits, effort))
                } else {
                    Box::new(adaptive_int::candidates_with_effort(&integers, effort))
                };
                let mut trials = Vec::new();
                let mut best: Option<(usize, String, bool)> = None;
                let mut dictionaries = 0;
                for payload in candidates {
                    let name = if is_float {
                        float_name(&payload, &mut dictionaries)
                    } else {
                        integer_name(&payload).into()
                    };
                    let extended_float = is_float && payload[0] == 5 && payload[1] & 0x60 != 0;
                    let payload = if mixed {
                        let mut wrapped = Vec::with_capacity(payload.len() + 1);
                        wrapped.push(0);
                        wrapped.extend_from_slice(&payload);
                        wrapped
                    } else {
                        payload
                    };
                    let compressed = compressor.compress(&payload)?;
                    // Inflate every trial, including candidates whose final stored form is raw.
                    let inflated = decompressor.decompress(&compressed, payload.len())?;
                    assert_eq!(inflated, payload, "Zstd payload mismatch");
                    decode_and_check(&inflated, &raw, is_float, mixed);
                    let size = compressed.len().min(payload.len());
                    if best
                        .as_ref()
                        .is_none_or(|(previous, _, _)| size < *previous)
                    {
                        best = Some((size, name.clone(), extended_float));
                    }
                    trials.push(json!({"encoding":name,"encoded_bytes":payload.len(),
                        "zstd_bytes":compressed.len(),"stored_bytes":size,
                        "raw_fallback":compressed.len()>=payload.len(),
                        "matches_original_payload":payload==original,"bit_exact":true}));
                }
                let (size, winner, new_float) = best.ok_or("no candidate")?;
                if (!is_float && effort == EncodingEffort::Balanced)
                    || (is_float && effort == EncodingEffort::Fast)
                {
                    assert_eq!(
                        size as u64, baseline_bytes,
                        "baseline menu regression: {}",
                        column["id"]
                    );
                }
                if is_float {
                    assert!(size as u64 <= baseline_bytes);
                }
                let format = column["format"].as_str().ok_or("column format missing")?;
                let kind = if is_float { "float" } else { "integer" };
                let key = format!("{kind}/{format}/{effort}");
                let aggregate = totals.entry(key).or_default();
                aggregate.columns += 1;
                aggregate.rows += rows as u64;
                aggregate.baseline_bytes += baseline_bytes;
                aggregate.stored_bytes += size as u64;
                aggregate.candidates += trials.len() as u64;
                aggregate.improved_columns += u64::from((size as u64) < baseline_bytes);
                aggregate.new_float_winners += u64::from(new_float);
                *aggregate.winners.entry(winner.clone()).or_default() += 1;
                let record = json!({"id":column["id"],"key":column["key"],"kind":kind,"format":format,
                    "rows":rows,"sha256":hash,"effort":effort.to_string(),"baseline_bytes":baseline_bytes,
                    "stored_bytes":size,"winner":winner,"new_float_winner":new_float,
                    "candidate_count":trials.len(),"candidates":trials});
                writeln!(output, "{record}")?;
                output.flush()?;
                eprintln!(
                    "{kind} {} {effort}: {baseline_bytes} -> {size} bytes ({winner})",
                    column["id"]
                );
            }
            total_columns += 1;
        }
    }
    let groups: BTreeMap<_, _> = totals.into_iter().map(|(key, total)| (key, json!({
        "columns":total.columns,"rows":total.rows,"baseline_bytes":total.baseline_bytes,
        "stored_bytes":total.stored_bytes,"savings_bytes":i128::from(total.baseline_bytes)-i128::from(total.stored_bytes),
        "savings_percent":100.0*(1.0-total.stored_bytes as f64/total.baseline_bytes as f64),
        "candidate_count":total.candidates,"improved_columns":total.improved_columns,
        "new_float_winners":total.new_float_winners,"winners":total.winners
    }))).collect();
    let summary = json!({"column_count":total_columns,"zstd_level":3,"all_candidates_bit_exact":true,
        "timing_benchmark":false,"float_manifest":args[1],"integer_manifest":args[2],"groups":groups});
    let mut summary_file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out.join("summary.json"))?;
    serde_json::to_writer_pretty(&mut summary_file, &summary)?;
    writeln!(summary_file)?;
    println!(
        "PASS: {total_columns} columns across all three efforts; {}",
        out.join("summary.json").display()
    );
    Ok(())
}
