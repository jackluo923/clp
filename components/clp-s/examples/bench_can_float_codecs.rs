//! Measure experimental float transforms against CLP's real adaptive float candidates.
//! Usage: bench_can_float_codecs MANIFEST OUTDIR [all|long|wide] [REPEATS] [MAX_COLUMNS]
//! No archive format or production candidate menu is changed by this example.
#![allow(dead_code)]
#[path = "../src/encoding_effort.rs"]
mod encoding_effort;
#[path = "../src/adaptive_float.rs"]
mod adaptive_float;
#[path = "../src/adaptive_int.rs"]
mod adaptive_int;
#[path = "../src/delta_blocks.rs"]
mod delta_blocks;
#[cfg(test)]
#[path = "../src/encoded_numeric.rs"]
mod encoded_numeric;
mod float_codec_trials;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, io::Write, path::Path, time::Instant};

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
        _ => panic!("unknown integer scheme"),
    }
}

fn baseline_name(payload: &[u8], dictionaries: &mut usize) -> String {
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
        5 => format!("existing_decimal_{}", integer_name(&payload[2..])),
        _ => panic!("unknown float scheme"),
    }
}

fn measure(
    bits: &[u64],
    payload: Vec<u8>,
    experimental: bool,
    compressor: &mut zstd::bulk::Compressor<'_>,
    decompressor: &mut zstd::bulk::Decompressor<'_>,
) -> Value {
    let t = Instant::now();
    let compressed = compressor.compress(&payload).unwrap();
    let compress_seconds = t.elapsed().as_secs_f64();
    // Same raw-frame fallback as the archive writer. The timer excludes input loading and
    // equality checks, but includes output allocation and the actual decompression/decoding.
    let raw_frame = compressed.len() >= payload.len();
    let stored_bytes = if raw_frame {
        payload.len()
    } else {
        compressed.len()
    };
    let t = Instant::now();
    let inflated = if raw_frame {
        None
    } else {
        Some(decompressor.decompress(&compressed, payload.len()).unwrap())
    };
    let decompress_seconds = t.elapsed().as_secs_f64();
    let wire = inflated.as_deref().unwrap_or(&payload);
    let t = Instant::now();
    let restored = if experimental {
        float_codec_trials::decode(wire).unwrap()
    } else {
        adaptive_float::decode(wire, bits.len()).unwrap()
    };
    let decode_seconds = t.elapsed().as_secs_f64();
    assert_eq!(wire, payload, "Zstd payload round trip");
    assert_eq!(restored.as_slice(), bits, "every IEEE bit must round trip");
    json!({"encoded_bytes":payload.len(), "zstd_bytes":compressed.len(),
        "stored_bytes":stored_bytes,"raw_frame":raw_frame,
        "compress_seconds":compress_seconds,"decompress_seconds":decompress_seconds,
        "decode_seconds":decode_seconds,"read_seconds":decompress_seconds+decode_seconds})
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    let n = values.len();
    if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() < 3 {
        return Err(
            "usage: bench_can_float_codecs MANIFEST OUTDIR [FORMAT] [REPEATS] [MAX_COLUMNS]".into(),
        );
    }
    let manifest: Value = serde_json::from_slice(&fs::read(&args[1])?)?;
    let out = Path::new(&args[2]);
    fs::create_dir_all(out)?;
    let output_file = out.join("columns.jsonl");
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output_file)?;
    let format = args.get(3).map(String::as_str).unwrap_or("all");
    let repeats: usize = args.get(4).map(|s| s.parse()).transpose()?.unwrap_or(3);
    if repeats == 0 {
        return Err("repeats must be positive".into());
    }
    let max_columns: usize = args
        .get(5)
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(usize::MAX);
    let start = Instant::now();
    let mut columns = 0;
    let mut rows = 0u64;
    let mut compressor = zstd::bulk::Compressor::new(3)?;
    let mut decompressor = zstd::bulk::Decompressor::new()?;
    for column in manifest["columns"]
        .as_array()
        .ok_or("columns missing")?
        .iter()
        .filter(|c| format == "all" || c["format"] == format)
        .take(max_columns)
    {
        let bytes = fs::read(column["path"].as_str().ok_or("column path")?)?;
        let hash = format!("{:x}", Sha256::digest(&bytes));
        if let Some(expected) = column["sha256"].as_str() {
            assert_eq!(hash, expected);
        }
        let bits: Vec<_> = bytes
            .chunks_exact(8)
            .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
            .collect();
        assert_eq!(bits.len() * 8, bytes.len());
        assert_eq!(bits.len() as u64, column["rows"].as_u64().unwrap());
        drop(bytes);
        let original = fs::read(column["encoded_path"].as_str().ok_or("encoded_path")?)?;
        assert_eq!(adaptive_float::decode(&original, bits.len())?, bits);
        let stored = compressor.compress(&original)?.len().min(original.len());
        if !column["shared_frame"].as_bool().unwrap_or(true) {
            assert_eq!(
                stored as u64,
                column["current_compressed_bytes"].as_u64().unwrap(),
                "original frame must recompress to the same size: {}",
                column["id"]
            );
        }
        let mut trials: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        let mut writer_order = Vec::new();
        let mut stats_times = Vec::new();
        let mut decimal_tail_times = Vec::new();
        for repeat in 0..repeats {
            // Alternate baseline-first/experiment-first to reduce a systematic order advantage.
            for phase in 0..2 {
                if (phase + repeat) % 2 == 0 {
                    let t = Instant::now();
                    let mut candidates = adaptive_float::candidates(&bits, true);
                    stats_times.push(t.elapsed().as_secs_f64());
                    let mut dictionaries = 0;
                    loop {
                        let t = Instant::now();
                        let payload = candidates.next();
                        let encode_seconds = t.elapsed().as_secs_f64();
                        let Some(payload) = payload else {
                            decimal_tail_times.push(encode_seconds);
                            break;
                        };
                        let name = baseline_name(&payload, &mut dictionaries);
                        let family = if adaptive_float::is_decimal(&payload) {
                            "existing_decimal"
                        } else {
                            "baseline"
                        };
                        if repeat == 0 {
                            writer_order.push(name.clone());
                        }
                        let matches_current_payload = payload == original;
                        let mut result =
                            measure(&bits, payload, false, &mut compressor, &mut decompressor);
                        result["matches_current_payload"] = json!(matches_current_payload);
                        result["encode_seconds"] = json!(encode_seconds);
                        result["family"] = json!(family);
                        result["repeat"] = json!(repeat);
                        trials.entry(name).or_default().push(result);
                    }
                } else {
                    let kinds = float_codec_trials::Kind::all();
                    for index in 0..kinds.len() {
                        let kind = kinds[(index + repeat) % kinds.len()];
                        let t = Instant::now();
                        let payload = float_codec_trials::encode(&bits, kind);
                        let encode_seconds = t.elapsed().as_secs_f64();
                        let diagnostic = float_codec_trials::diagnostics(&payload)?;
                        let mut result =
                            measure(&bits, payload, true, &mut compressor, &mut decompressor);
                        result["diagnostics"] = json!({"rows":diagnostic.rows,
                            "blocks":diagnostic.blocks,"exceptions":diagnostic.exceptions,
                            "scale_blocks":diagnostic.scale_blocks});
                        result["encode_seconds"] = json!(encode_seconds);
                        result["family"] = json!("experimental");
                        result["repeat"] = json!(repeat);
                        trials
                            .entry(kind.name().to_owned())
                            .or_default()
                            .push(result);
                    }
                }
            }
        }
        let mut medians = BTreeMap::new();
        for (name, samples) in &trials {
            assert_eq!(samples.len(), repeats);
            for sample in samples {
                assert_eq!(sample["stored_bytes"], samples[0]["stored_bytes"]);
                assert_eq!(sample["encoded_bytes"], samples[0]["encoded_bytes"]);
            }
            let mut result = samples[0].clone();
            result.as_object_mut().unwrap().remove("repeat");
            for key in [
                "encode_seconds",
                "compress_seconds",
                "decompress_seconds",
                "decode_seconds",
                "read_seconds",
            ] {
                result[key] = json!(median(
                    samples.iter().map(|s| s[key].as_f64().unwrap()).collect()
                ));
            }
            result["encode_compress_seconds"] = json!(median(
                samples
                    .iter()
                    .map(|s| s["encode_seconds"].as_f64().unwrap()
                        + s["compress_seconds"].as_f64().unwrap())
                    .collect()
            ));
            medians.insert(name.clone(), result);
        }
        let baseline_name = writer_order
            .iter()
            .filter(|name| medians[*name]["family"] == "baseline")
            .min_by_key(|name| medians[*name]["stored_bytes"].as_u64().unwrap())
            .unwrap();
        let baseline = &medians[baseline_name];
        assert_eq!(
            baseline["stored_bytes"].as_u64().unwrap(),
            stored as u64,
            "candidate menu disagrees with actual archive frame: {}",
            column["id"]
        );
        assert_eq!(
            baseline["matches_current_payload"], true,
            "baseline decode timing must use the exact current payload"
        );
        let best = medians
            .iter()
            .min_by_key(|(_, v)| v["stored_bytes"].as_u64().unwrap())
            .unwrap();
        let result = json!({"column":column,"sha256":hash,"rows":bits.len(),
            "baseline_name":baseline_name,"baseline_bytes":stored,"best_name":best.0,
            "best_bytes":best.1["stored_bytes"],"trials":medians,"samples":trials,
            "baseline_stats_seconds":median(stats_times),"decimal_tail_seconds":median(decimal_tail_times)});
        writeln!(output, "{}", serde_json::to_string(&result)?)?;
        output.flush()?;
        columns += 1;
        rows += bits.len() as u64;
        eprintln!(
            "{columns}: {} ({} rows): {} -> {} bytes ({})",
            column["id"],
            bits.len(),
            stored,
            best.1["stored_bytes"],
            best.0
        );
    }
    let summary = json!({"manifest":args[1],"format":format,"columns":columns,"rows":rows,
        "repeats":repeats,"elapsed_seconds":start.elapsed().as_secs_f64(),
        "zstd_level":3,"zstd_version":zstd::zstd_safe::version_string(),"raw_frame_fallback":true,
        "restart_rows":delta_blocks::ROWS,"bit_exact_round_trips":true,
        "notes":"All physical input values are checked for every candidate and repeat. Timing excludes file IO, hashing and validation, includes allocations; sums of per-column medians are stage estimates, not end-to-end query or compression timings. Existing decimal candidate generation is lazy and may be charged to the first decimal candidate. New payloads are experimental custom transforms, not production ALP/Gorilla implementations."});
    fs::write(
        out.join("summary.json"),
        serde_json::to_string_pretty(&summary)? + "\n",
    )?;
    Ok(())
}
