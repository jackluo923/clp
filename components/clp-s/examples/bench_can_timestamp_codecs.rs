//! Lossless, restartable timestamp transform experiment; not an archive format change.
//! cargo run --release -p clp-s --example bench_can_timestamp_codecs -- MANIFEST OUTDIR
//! cargo run --release -p clp-s --example bench_can_timestamp_codecs -- --pruning MANIFEST OUTDIR all 3
//! Every candidate includes its tag/anchors/directory and is measured after Zstd level 3.
#![allow(dead_code)]
#[path = "../src/encoding_effort.rs"]
mod encoding_effort;
#[path = "../src/adaptive_int.rs"]
mod adaptive_int;
#[path = "../src/delta_blocks.rs"]
mod delta_blocks;
// The imported production modules' own test modules reference these companions.
#[cfg(test)]
#[path = "../src/adaptive_float.rs"]
mod adaptive_float;
#[cfg(test)]
#[path = "../src/encoded_numeric.rs"]
mod encoded_numeric;
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, io::Write, path::Path, time::Instant};
const ROWS: usize = delta_blocks::ROWS;

/// A paired selection benchmark uses the existing experimental wire formats below. Its timer
/// includes the statistics pass, payload generation, MixedInteger prefix, and every Zstd call.
/// Reading input and checking round trips happen outside the timed region.
#[derive(Clone, Copy, Debug)]
enum SelectionMode {
    Exhaustive,
    Pruned,
}

struct SelectionTrial {
    elapsed_seconds: f64,
    generation_seconds: f64,
    compress_seconds: f64,
    best_bytes: usize,
    winner: String,
    trials: Vec<Value>,
    zero_run: bool,
    center_patched: bool,
}

impl SelectionTrial {
    fn as_json(&self) -> Value {
        json!({
            "elapsed_seconds": self.elapsed_seconds,
            "generation_seconds": self.generation_seconds,
            "compress_seconds": self.compress_seconds,
            "best_bytes": self.best_bytes,
            "winner": self.winner,
            "candidate_count": self.trials.len(),
            "zero_run_selected": self.zero_run,
            "center_patched_selected": self.center_patched,
            "trials": self.trials,
        })
    }
}

fn selection_trial(values: &[i64], mixed_integer: bool, mode: SelectionMode) -> SelectionTrial {
    let start = Instant::now();
    match mode {
        SelectionMode::Exhaustive => measure_selection(
            start,
            values,
            mixed_integer,
            adaptive_int::candidates(values),
            true,
            true,
        ),
        SelectionMode::Pruned => {
            let analysis = adaptive_int::analyze_candidates(values);
            let selection = analysis.delta_candidates();
            measure_selection(
                start,
                values,
                mixed_integer,
                analysis.candidates(),
                selection.zero_run,
                selection.center_patched,
            )
        }
    }
}

fn measure_selection(
    start: Instant,
    values: &[i64],
    mixed_integer: bool,
    mut baseline: impl Iterator<Item = Vec<u8>>,
    zero_run: bool,
    center_patched: bool,
) -> SelectionTrial {
    let mut generation_seconds = start.elapsed().as_secs_f64();
    let mut best_bytes = usize::MAX;
    let mut winner = String::new();
    let mut compress_seconds = 0.0;
    let mut trials = Vec::new();
    let mut measure_payload = |name: String, payload: Vec<u8>| {
        let wire = if mixed_integer {
            let mut wire = Vec::with_capacity(payload.len() + 1);
            wire.push(0);
            wire.extend_from_slice(&payload);
            wire
        } else {
            payload
        };
        let compress_start = Instant::now();
        let compressed = zstd::bulk::compress(&wire, 3).unwrap();
        compress_seconds += compress_start.elapsed().as_secs_f64();
        let stored_bytes = wire.len().min(compressed.len());
        if stored_bytes < best_bytes {
            best_bytes = stored_bytes;
            winner.clone_from(&name);
        }
        trials.push(json!({
            "name": name,
            "encoded_bytes": wire.len(),
            "zstd_bytes": compressed.len(),
            "stored_bytes": stored_bytes,
        }));
    };
    loop {
        let encode_start = Instant::now();
        let next = baseline.next();
        generation_seconds += encode_start.elapsed().as_secs_f64();
        let Some(payload) = next else { break };
        measure_payload(baseline_name(&payload), payload);
    }
    for (selected, kind) in [
        (zero_run, Kind::ZeroRunDelta),
        (center_patched, Kind::CenterPatched4),
    ] {
        if selected {
            let encode_start = Instant::now();
            let payload = encode(values, kind);
            generation_seconds += encode_start.elapsed().as_secs_f64();
            measure_payload(format!("{kind:?}"), payload);
        }
    }
    SelectionTrial {
        elapsed_seconds: start.elapsed().as_secs_f64(),
        generation_seconds,
        compress_seconds,
        best_bytes,
        winner,
        trials,
        zero_run,
        center_patched,
    }
}

#[derive(Default)]
struct SelectionSummary {
    columns: usize,
    rows: u64,
    exhaustive_bytes: u64,
    pruned_bytes: u64,
    exhaustive_calls: usize,
    pruned_calls: usize,
    regretted_columns: usize,
    zero_run_selected: usize,
    center_patched_selected: usize,
    exhaustive_seconds: Vec<f64>,
    pruned_seconds: Vec<f64>,
}

fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    }
}

impl SelectionSummary {
    fn as_json(&self) -> Value {
        json!({
            "columns": self.columns,
            "rows": self.rows,
            "exhaustive_bytes": self.exhaustive_bytes,
            "pruned_bytes": self.pruned_bytes,
            "regret_bytes": self.pruned_bytes - self.exhaustive_bytes,
            "regretted_columns": self.regretted_columns,
            "exhaustive_calls_per_repeat": self.exhaustive_calls,
            "pruned_calls_per_repeat": self.pruned_calls,
            "avoided_calls_per_repeat": self.exhaustive_calls - self.pruned_calls,
            "zero_run_selected_columns": self.zero_run_selected,
            "center_patched_selected_columns": self.center_patched_selected,
            "exhaustive_seconds_by_repeat": self.exhaustive_seconds,
            "pruned_seconds_by_repeat": self.pruned_seconds,
            "exhaustive_median_seconds": median(&self.exhaustive_seconds),
            "pruned_median_seconds": median(&self.pruned_seconds),
            "observed_speedup": median(&self.exhaustive_seconds) / median(&self.pruned_seconds),
        })
    }
}

fn benchmark_pruning(args: &[String]) {
    assert!(
        args.len() >= 2,
        "--pruning MANIFEST OUTDIR [FORMAT|all] [REPEATS]"
    );
    let manifest: Value = serde_json::from_slice(&fs::read(&args[0]).unwrap()).unwrap();
    let outdir = Path::new(&args[1]);
    let repeats: usize = args.get(3).map_or(3, |value| value.parse().unwrap());
    assert!(repeats > 0);
    fs::create_dir_all(outdir).unwrap();
    let mut columns = fs::File::create(outdir.join("columns.jsonl")).unwrap();
    let mut summaries: BTreeMap<String, SelectionSummary> = BTreeMap::new();
    let mut column_index = 0;
    validate_edges();
    validate_pruning_estimates();
    for column in manifest["columns"].as_array().unwrap() {
        let format = column["format"].as_str().unwrap();
        if args
            .get(2)
            .is_some_and(|wanted| wanted != "all" && wanted != format)
        {
            continue;
        }
        let bytes = fs::read(column["path"].as_str().unwrap()).unwrap();
        assert_eq!(bytes.len() % 8, 0);
        let values: Vec<i64> = bytes
            .chunks_exact(8)
            .map(|bytes| i64::from_le_bytes(bytes.try_into().unwrap()))
            .collect();
        assert_eq!(Some(values.len() as u64), column["rows"].as_u64());
        let mixed_integer = column["mixed_integer"].as_bool().unwrap_or(false);
        let diagnostics = adaptive_int::analyze_candidates(&values).delta_candidates();
        // Verify both optional transforms independently of the timed selection runs.
        for kind in [Kind::ZeroRunDelta, Kind::CenterPatched4] {
            let encoded = encode(&values, kind);
            if matches!(kind, Kind::ZeroRunDelta) {
                assert_eq!(diagnostics.zero_run_bytes, encoded.len());
            }
            let compressed = zstd::bulk::compress(&encoded, 3).unwrap();
            let restored = zstd::bulk::decompress(&compressed, encoded.len()).unwrap();
            assert_eq!(values, decode(&restored, values.len(), kind));
        }
        let summary = summaries
            .entry(format.into())
            .or_insert_with(|| SelectionSummary {
                exhaustive_seconds: vec![0.0; repeats],
                pruned_seconds: vec![0.0; repeats],
                ..Default::default()
            });
        let mut paired_trials = Vec::new();
        let mut first_result = None;
        for repeat in 0..repeats {
            // Alternate AB/BA within each column and also across neighboring columns.
            let exhaustive_first = (column_index + repeat) % 2 == 0;
            let (exhaustive, pruned) = if exhaustive_first {
                let exhaustive = selection_trial(&values, mixed_integer, SelectionMode::Exhaustive);
                let pruned = selection_trial(&values, mixed_integer, SelectionMode::Pruned);
                (exhaustive, pruned)
            } else {
                let pruned = selection_trial(&values, mixed_integer, SelectionMode::Pruned);
                let exhaustive = selection_trial(&values, mixed_integer, SelectionMode::Exhaustive);
                (exhaustive, pruned)
            };
            assert!(pruned.best_bytes >= exhaustive.best_bytes);
            for retained in &pruned.trials {
                assert!(exhaustive.trials.contains(retained));
            }
            let sizes = (exhaustive.best_bytes, pruned.best_bytes);
            if let Some(previous) = first_result {
                assert_eq!(previous, sizes, "nondeterministic candidate selection");
            } else {
                first_result = Some(sizes);
                summary.columns += 1;
                summary.rows += values.len() as u64;
                summary.exhaustive_bytes += exhaustive.best_bytes as u64;
                summary.pruned_bytes += pruned.best_bytes as u64;
                summary.exhaustive_calls += exhaustive.trials.len();
                summary.pruned_calls += pruned.trials.len();
                summary.regretted_columns += usize::from(pruned.best_bytes > exhaustive.best_bytes);
                summary.zero_run_selected += usize::from(pruned.zero_run);
                summary.center_patched_selected += usize::from(pruned.center_patched);
            }
            summary.exhaustive_seconds[repeat] += exhaustive.elapsed_seconds;
            summary.pruned_seconds[repeat] += pruned.elapsed_seconds;
            paired_trials.push(json!({
                "repeat": repeat,
                "exhaustive_first": exhaustive_first,
                "exhaustive": exhaustive.as_json(),
                "pruned": pruned.as_json(),
            }));
        }
        writeln!(
            columns,
            "{}",
            json!({
                "column":column,"round_trip_verified":true,"pairs":paired_trials,
                "statistics": {
                    "zero_run_bytes":diagnostics.zero_run_bytes,
                    "center_patched_min_bytes":diagnostics.center_patched_min_bytes,
                    "plain_bytes":diagnostics.plain_bytes,
                    "zero_delta_count":diagnostics.zero_delta_count,
                    "delta_count":diagnostics.delta_count,
                },
            })
        )
        .unwrap();
        columns.flush().unwrap();
        column_index += 1;
        let summary_json = summaries
            .iter()
            .map(|(format, summary)| (format.clone(), summary.as_json()))
            .collect::<serde_json::Map<_, _>>();
        fs::write(outdir.join("summary.json"), serde_json::to_vec_pretty(&json!({
            "manifest":args[0], "repeats":repeats, "zstd_level":3, "restart_rows":ROWS,
            "candidate_scope":"existing adaptive candidates plus ZeroRunDelta and CenterPatched4",
            "timing":"alternating paired runs per column; includes statistics, candidate generation and Zstd; excludes input loading and round-trip validation",
            "baseline_stats":"exhaustive uses existing scan; pruned uses extended single scan shared with baseline candidate generation",
            "raw_frame_fallback":true, "mixed_integer_flag_included":true,
            "aggregate":summary_json,
        })).unwrap()).unwrap();
        eprintln!("paired selection: {column_index} columns complete");
    }
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    PlainBss,
    DeltaBss,
    DodFixed,
    DodBss,
    DodPacked,
    DeltaVarint,
    DodVarint,
    ZeroRunDelta,
    RunDelta,
    GcdDeltaBss,
    LinearBss,
    Patched8,
    Patched12,
    Patched16,
    RationalBss,
    RationalPacked,
    CenterPatched4,
    CenterPatched6,
    CenterPatched8,
    MiniPacked32,
    MiniPacked128,
    MiniPacked1024,
}
const KINDS: &[Kind] = &[
    Kind::PlainBss,
    Kind::DeltaBss,
    Kind::DodFixed,
    Kind::DodBss,
    Kind::DodPacked,
    Kind::DeltaVarint,
    Kind::DodVarint,
    Kind::ZeroRunDelta,
    Kind::RunDelta,
    Kind::GcdDeltaBss,
    Kind::LinearBss,
    Kind::Patched8,
    Kind::Patched12,
    Kind::Patched16,
    Kind::RationalBss,
    Kind::RationalPacked,
    Kind::CenterPatched4,
    Kind::CenterPatched6,
    Kind::CenterPatched8,
    Kind::MiniPacked32,
    Kind::MiniPacked128,
    Kind::MiniPacked1024,
];
fn zig(v: i64) -> u64 {
    ((v as u64) << 1) ^ ((v >> 63) as u64)
}
fn unzig(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}
fn var(mut v: u64, out: &mut Vec<u8>) {
    while v >= 128 {
        out.push((v as u8) | 128);
        v >>= 7;
    }
    out.push(v as u8);
}
fn readvar(b: &[u8], p: &mut usize) -> u64 {
    let mut v = 0;
    let mut shift = 0;
    loop {
        let x = b[*p];
        *p += 1;
        v |= ((x & 127) as u64) << shift;
        if x < 128 {
            return v;
        }
        shift += 7;
        assert!(shift < 70);
    }
}
fn word(v: i64, out: &mut Vec<u8>) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn readword(b: &[u8], p: &mut usize) -> i64 {
    let v = i64::from_le_bytes(b[*p..*p + 8].try_into().unwrap());
    *p += 8;
    v
}
fn bss(v: &[i64], out: &mut Vec<u8>) {
    for byte in 0..8 {
        for x in v {
            out.push(((*x as u64) >> (byte * 8)) as u8);
        }
    }
}
fn unbss(b: &[u8], n: usize) -> Vec<i64> {
    let mut v = vec![0u64; n];
    for byte in 0..8 {
        for i in 0..n {
            v[i] |= (b[byte * n + i] as u64) << (byte * 8)
        }
    }
    v.into_iter().map(|x| x as i64).collect()
}
fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let x = a % b;
        a = b;
        b = x;
    }
    a
}
fn deltas(v: &[i64]) -> Vec<i64> {
    std::iter::once(v[0])
        .chain(v.windows(2).map(|w| w[1].wrapping_sub(w[0])))
        .collect()
}
fn integrate(v: &mut [i64]) {
    for i in 1..v.len() {
        v[i] = v[i].wrapping_add(v[i - 1]);
    }
}
fn width(k: Kind) -> u8 {
    match k {
        Kind::Patched8 => 8,
        Kind::Patched12 => 12,
        Kind::Patched16 => 16,
        Kind::CenterPatched4 => 4,
        Kind::CenterPatched6 => 6,
        Kind::CenterPatched8 => 8,
        _ => unreachable!(),
    }
}
fn variable(k: Kind) -> bool {
    matches!(
        k,
        Kind::DodPacked
            | Kind::DeltaVarint
            | Kind::DodVarint
            | Kind::ZeroRunDelta
            | Kind::RunDelta
            | Kind::Patched8
            | Kind::Patched12
            | Kind::Patched16
            | Kind::RationalPacked
            | Kind::CenterPatched4
            | Kind::CenterPatched6
            | Kind::CenterPatched8
            | Kind::MiniPacked32
            | Kind::MiniPacked128
            | Kind::MiniPacked1024
    )
}
fn encode_block(v: &[i64], k: Kind, out: &mut Vec<u8>) {
    if matches!(k, Kind::PlainBss) {
        bss(v, out);
        return;
    }
    let mut d = deltas(v);
    match k {
        Kind::DeltaBss => bss(&d, out),
        Kind::DodFixed | Kind::DodBss | Kind::DodPacked | Kind::DodVarint => {
            for i in (2..d.len()).rev() {
                d[i] = d[i].wrapping_sub(d[i - 1]);
            }
            match k {
                Kind::DodFixed => {
                    for x in d {
                        word(x, out)
                    }
                }
                Kind::DodBss => bss(&d, out),
                Kind::DodVarint => {
                    word(d[0], out);
                    if d.len() > 1 {
                        word(d[1], out)
                    }
                    for x in &d[2.min(d.len())..] {
                        var(zig(*x), out)
                    }
                }
                Kind::DodPacked => {
                    word(d[0], out);
                    if d.len() > 1 {
                        word(d[1], out)
                    }
                    let tail = &d[2.min(d.len())..];
                    let min = tail.iter().min().copied().unwrap_or(0);
                    let max = tail.iter().max().copied().unwrap_or(0);
                    let w = adaptive_int::bit_width_for((max as u64).wrapping_sub(min as u64));
                    word(min, out);
                    out.push(w);
                    let offsets: Vec<u64> = tail
                        .iter()
                        .map(|x| (*x as u64).wrapping_sub(min as u64))
                        .collect();
                    adaptive_int::pack_bits(&offsets, w, out);
                }
                _ => unreachable!(),
            }
        }
        Kind::DeltaVarint => {
            word(d[0], out);
            for x in &d[1..] {
                var(zig(*x), out)
            }
        }
        Kind::ZeroRunDelta => {
            word(d[0], out);
            let mut i = 1;
            while i < d.len() {
                if d[i] == 0 {
                    let start = i;
                    while i < d.len() && d[i] == 0 {
                        i += 1
                    }
                    var(0, out);
                    var((i - start) as u64, out)
                } else {
                    var(zig(d[i]), out);
                    i += 1
                }
            }
        }
        Kind::RunDelta => {
            word(d[0], out);
            let mut i = 1;
            while i < d.len() {
                let start = i;
                let x = d[i];
                while i < d.len() && d[i] == x {
                    i += 1
                }
                var(zig(x), out);
                var((i - start) as u64, out);
            }
        }
        Kind::GcdDeltaBss => {
            let g = d[1..]
                .iter()
                .fold(0, |a, b| gcd(a, b.unsigned_abs()))
                .max(1);
            word(g as i64, out);
            for x in &mut d[1..] {
                *x = ((*x as i128) / (g as i128)) as i64
            }
            bss(&d, out)
        }
        Kind::LinearBss => {
            let slope = if v.len() > 1 {
                ((v[v.len() - 1] as i128 - v[0] as i128) / (v.len() - 1) as i128) as i64
            } else {
                0
            };
            word(v[0], out);
            word(slope, out);
            for (i, x) in d.iter_mut().enumerate() {
                *x = v[i].wrapping_sub(v[0].wrapping_add(slope.wrapping_mul(i as i64)))
            }
            bss(&d, out)
        }
        Kind::MiniPacked32 | Kind::MiniPacked128 | Kind::MiniPacked1024 => {
            word(d[0], out);
            let size = match k {
                Kind::MiniPacked32 => 32,
                Kind::MiniPacked128 => 128,
                _ => 1024,
            };
            for mini in d[1..].chunks(size) {
                let min = *mini.iter().min().unwrap();
                let max = *mini.iter().max().unwrap();
                word(min, out);
                let w = adaptive_int::bit_width_for((max as u64).wrapping_sub(min as u64));
                out.push(w);
                let offsets: Vec<u64> = mini
                    .iter()
                    .map(|x| (*x as u64).wrapping_sub(min as u64))
                    .collect();
                adaptive_int::pack_bits(&offsets, w, out);
            }
        }
        Kind::RationalBss | Kind::RationalPacked => {
            let diff = v[v.len() - 1] as i128 - v[0] as i128;
            word(v[0], out);
            out.extend_from_slice(&diff.to_le_bytes());
            for (i, x) in d.iter_mut().enumerate() {
                let pred =
                    v[0].wrapping_add((diff * i as i128 / (v.len() - 1).max(1) as i128) as i64);
                *x = v[i].wrapping_sub(pred);
            }
            if matches!(k, Kind::RationalBss) {
                bss(&d, out)
            } else {
                let min = *d.iter().min().unwrap();
                let max = *d.iter().max().unwrap();
                word(min, out);
                let w = adaptive_int::bit_width_for((max as u64).wrapping_sub(min as u64));
                out.push(w);
                let offsets: Vec<u64> = d
                    .iter()
                    .map(|x| (*x as u64).wrapping_sub(min as u64))
                    .collect();
                adaptive_int::pack_bits(&offsets, w, out);
            }
        }
        Kind::Patched8
        | Kind::Patched12
        | Kind::Patched16
        | Kind::CenterPatched4
        | Kind::CenterPatched6
        | Kind::CenterPatched8 => {
            let w = width(k);
            let mask = (1u64 << w) - 1;
            word(d[0], out);
            let centered = matches!(
                k,
                Kind::CenterPatched4 | Kind::CenterPatched6 | Kind::CenterPatched8
            );
            let center = if centered {
                let mut tail = d[1..].to_vec();
                let len = tail.len();
                let center = if len == 0 {
                    0
                } else {
                    *tail.select_nth_unstable(len / 2).1
                };
                word(center, out);
                center
            } else {
                0
            };
            let z: Vec<u64> = d[1..].iter().map(|x| zig(x.wrapping_sub(center))).collect();
            let lows: Vec<u64> = z.iter().map(|x| x & mask).collect();
            adaptive_int::pack_bits(&lows, w, out);
            let mut exceptions = Vec::new();
            let mut prev = 0;
            let mut count = 0;
            for (i, x) in z.iter().copied().enumerate() {
                if x > mask {
                    var((i - prev) as u64, &mut exceptions);
                    var(x >> w, &mut exceptions);
                    prev = i;
                    count += 1;
                }
            }
            var(count, out);
            out.extend_from_slice(&exceptions);
        }
        Kind::PlainBss => unreachable!(),
    }
}
fn decode_block(b: &[u8], n: usize, k: Kind) -> Vec<i64> {
    if matches!(k, Kind::PlainBss) {
        assert_eq!(b.len(), n * 8);
        return unbss(b, n);
    }
    let mut p = 0;
    let mut d = match k {
        Kind::DeltaBss | Kind::DodBss => {
            p = n * 8;
            unbss(b, n)
        }
        Kind::DodFixed => (0..n).map(|_| readword(b, &mut p)).collect(),
        Kind::DodPacked | Kind::DodVarint => {
            let mut d = vec![readword(b, &mut p)];
            if n > 1 {
                d.push(readword(b, &mut p));
            }
            if matches!(k, Kind::DodPacked) {
                let min = readword(b, &mut p);
                let w = b[p];
                p += 1;
                let size = adaptive_int::packed_len(n.saturating_sub(2), w);
                d.extend(
                    adaptive_int::unpack_bits(&b[p..], w, n.saturating_sub(2))
                        .unwrap()
                        .into_iter()
                        .map(|x| x.wrapping_add(min as u64) as i64),
                );
                p += size;
            } else {
                while d.len() < n {
                    d.push(unzig(readvar(b, &mut p)))
                }
            }
            d
        }
        Kind::DeltaVarint | Kind::ZeroRunDelta | Kind::RunDelta => {
            let mut d = vec![readword(b, &mut p)];
            while d.len() < n {
                let z = readvar(b, &mut p);
                if matches!(k, Kind::RunDelta) {
                    let count = readvar(b, &mut p) as usize;
                    d.resize(d.len() + count, unzig(z));
                } else if matches!(k, Kind::ZeroRunDelta) && z == 0 {
                    let count = readvar(b, &mut p) as usize;
                    d.resize(d.len() + count, 0);
                } else {
                    d.push(unzig(z));
                }
            }
            d
        }
        Kind::GcdDeltaBss => {
            let g = readword(b, &mut p) as u64;
            let mut d = unbss(&b[p..], n);
            p += n * 8;
            for x in &mut d[1..] {
                *x = (*x as u64).wrapping_mul(g) as i64;
            }
            d
        }
        Kind::LinearBss => {
            let base = readword(b, &mut p);
            let slope = readword(b, &mut p);
            let mut d = unbss(&b[p..], n);
            assert_eq!(b.len(), p + n * 8);
            for (i, x) in d.iter_mut().enumerate() {
                *x = x.wrapping_add(base.wrapping_add(slope.wrapping_mul(i as i64)))
            }
            return d;
        }
        Kind::MiniPacked32 | Kind::MiniPacked128 | Kind::MiniPacked1024 => {
            let mut d = vec![readword(b, &mut p)];
            let size = match k {
                Kind::MiniPacked32 => 32,
                Kind::MiniPacked128 => 128,
                _ => 1024,
            };
            while d.len() < n {
                let count = (n - d.len()).min(size);
                let min = readword(b, &mut p);
                let w = b[p];
                p += 1;
                d.extend(
                    adaptive_int::unpack_bits(&b[p..], w, count)
                        .unwrap()
                        .into_iter()
                        .map(|x| x.wrapping_add(min as u64) as i64),
                );
                p += adaptive_int::packed_len(count, w);
            }
            d
        }
        Kind::RationalBss | Kind::RationalPacked => {
            let base = readword(b, &mut p);
            let diff = i128::from_le_bytes(b[p..p + 16].try_into().unwrap());
            p += 16;
            let mut d = if matches!(k, Kind::RationalBss) {
                let d = unbss(&b[p..], n);
                p += n * 8;
                d
            } else {
                let min = readword(b, &mut p);
                let w = b[p];
                p += 1;
                let d = adaptive_int::unpack_bits(&b[p..], w, n)
                    .unwrap()
                    .into_iter()
                    .map(|x| x.wrapping_add(min as u64) as i64)
                    .collect::<Vec<_>>();
                p += adaptive_int::packed_len(n, w);
                d
            };
            assert_eq!(b.len(), p);
            for (i, x) in d.iter_mut().enumerate() {
                let pred = base.wrapping_add((diff * i as i128 / (n - 1).max(1) as i128) as i64);
                *x = x.wrapping_add(pred)
            }
            return d;
        }
        Kind::Patched8
        | Kind::Patched12
        | Kind::Patched16
        | Kind::CenterPatched4
        | Kind::CenterPatched6
        | Kind::CenterPatched8 => {
            let w = width(k);
            let first = readword(b, &mut p);
            let center = if matches!(
                k,
                Kind::CenterPatched4 | Kind::CenterPatched6 | Kind::CenterPatched8
            ) {
                readword(b, &mut p)
            } else {
                0
            };
            let mut z = adaptive_int::unpack_bits(&b[p..], w, n - 1).unwrap();
            p += adaptive_int::packed_len(n - 1, w);
            let count = readvar(b, &mut p);
            let mut pos = 0;
            for _ in 0..count {
                pos += readvar(b, &mut p) as usize;
                z[pos] |= readvar(b, &mut p) << w;
            }
            std::iter::once(first)
                .chain(z.into_iter().map(|x| unzig(x).wrapping_add(center)))
                .collect()
        }
        Kind::PlainBss => unreachable!(),
    };
    assert_eq!(d.len(), n);
    assert_eq!(p, b.len());
    if matches!(
        k,
        Kind::DodFixed | Kind::DodBss | Kind::DodPacked | Kind::DodVarint
    ) {
        for i in 2..d.len() {
            d[i] = d[i].wrapping_add(d[i - 1]);
        }
    }
    integrate(&mut d);
    d
}
fn encode(v: &[i64], k: Kind) -> Vec<u8> {
    let blocks = v.len().div_ceil(ROWS);
    let mut out = vec![128 + k as u8];
    // Variable payloads have an offset directory; fixed-size blocks can be sought arithmetically.
    if variable(k) {
        out.resize(1 + 8 * (blocks + 1), 0);
    }
    for (i, b) in v.chunks(ROWS).enumerate() {
        if variable(k) {
            let pos = out.len() as u64;
            out[1 + 8 * i..9 + 8 * i].copy_from_slice(&pos.to_le_bytes());
        }
        encode_block(b, k, &mut out);
    }
    if variable(k) {
        let pos = out.len() as u64;
        out[1 + 8 * blocks..9 + 8 * blocks].copy_from_slice(&pos.to_le_bytes());
    }
    out
}
fn decode(b: &[u8], n: usize, k: Kind) -> Vec<i64> {
    let blocks = n.div_ceil(ROWS);
    let mut out = Vec::with_capacity(n);
    let mut p = 1;
    for i in 0..blocks {
        let rows = (n - i * ROWS).min(ROWS);
        let (start, end) = if variable(k) {
            (
                u64::from_le_bytes(b[1 + 8 * i..9 + 8 * i].try_into().unwrap()) as usize,
                u64::from_le_bytes(b[9 + 8 * i..17 + 8 * i].try_into().unwrap()) as usize,
            )
        } else {
            let extra = match k {
                Kind::GcdDeltaBss => 8,
                Kind::LinearBss => 16,
                Kind::RationalBss => 24,
                _ => 0,
            };
            (p, p + rows * 8 + extra)
        };
        out.extend(decode_block(&b[start..end], rows, k));
        p = end;
    }
    assert_eq!(p, b.len());
    out
}
fn baseline_name(b: &[u8]) -> String {
    match b[0] {
        0 => "plain",
        1 => "for",
        2 => "legacy_delta",
        3 => "dictionary",
        4 => "rle",
        5 => "constant",
        6 => match b[9] {
            255 => "local_packed_delta",
            64 if b[1..9] == [0; 8] => "fixed_delta",
            _ => "shared_packed_delta",
        },
        _ => "unknown",
    }
    .into()
}
fn validate_edges() {
    for n in [1, 2, 3, ROWS - 1, ROWS, ROWS + 1, 2 * ROWS + 3] {
        let v: Vec<i64> = (0..n)
            .map(|i| match i % 13 {
                0 => i64::MIN,
                1 => i64::MAX,
                2 => 0,
                3 => -1,
                _ => ((i as u64).wrapping_mul(0x9e3779b97f4a7c15)) as i64,
            })
            .collect();
        for &k in KINDS {
            assert_eq!(v, decode(&encode(&v, k), v.len(), k), "edge {:?} {n}", k);
        }
    }
}

fn validate_pruning_estimates() {
    for count in [0, 1, 2, 127, 128, ROWS - 1, ROWS, ROWS + 1, 2 * ROWS + 3] {
        for pattern in 0..4 {
            let values: Vec<i64> = (0..count)
                .map(|index| match pattern {
                    0 => 42,
                    1 => index as i64,
                    2 => (index / 137) as i64,
                    _ => match index % 7 {
                        0 => i64::MIN,
                        1 => i64::MAX,
                        2 => 0,
                        _ => ((index as u64).wrapping_mul(0x9e3779b97f4a7c15)) as i64,
                    },
                })
                .collect();
            let statistics = adaptive_int::analyze_candidates(&values).delta_candidates();
            assert_eq!(
                statistics.zero_run_bytes,
                encode(&values, Kind::ZeroRunDelta).len(),
                "zero-run byte estimate: {count} rows, pattern {pattern}",
            );
        }
    }
    // A small pre-Zstd representation alone cannot prove that patched packing loses.
    // Fixed-length repetitions are a regression case where both must still be measured.
    for step in 1..=3 {
        let values: Vec<i64> = (0..2 * ROWS)
            .map(|index| (index as i64 / 16) * step)
            .collect();
        let selection = adaptive_int::analyze_candidates(&values).delta_candidates();
        assert!(selection.zero_run && selection.center_patched);
    }
}

#[test]
fn experimental_codecs_preserve_wraparound_and_restart_boundaries() {
    validate_edges();
}

#[test]
fn pruning_estimates_match_actual_zero_run_encoding() {
    validate_pruning_estimates();
}
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|argument| argument == "--pruning") {
        benchmark_pruning(&args[2..]);
        return;
    }
    assert!(args.len() >= 3, "MANIFEST OUTDIR [FORMAT]");
    let m: Value = serde_json::from_slice(&fs::read(&args[1]).unwrap()).unwrap();
    let outdir = Path::new(&args[2]);
    fs::create_dir_all(outdir).unwrap();
    validate_edges();
    let mut per = fs::File::create(outdir.join("columns.jsonl")).unwrap();
    let mut aggregate: BTreeMap<String, BTreeMap<String, (u64, u64, f64, f64, u64)>> =
        BTreeMap::new();
    let start = Instant::now();
    let mut count = 0;
    let mut rows = 0u64;
    for c in m["columns"].as_array().unwrap() {
        let fmt = c["format"].as_str().unwrap_or("unknown");
        if args.get(3).is_some_and(|x| x != fmt) {
            continue;
        }
        let bytes = fs::read(c["path"].as_str().unwrap()).unwrap();
        assert_eq!(bytes.len() % 8, 0);
        let v: Vec<i64> = bytes
            .chunks_exact(8)
            .map(|b| i64::from_le_bytes(b.try_into().unwrap()))
            .collect();
        if v.is_empty() {
            continue;
        }
        if let Some(n) = c["row_count"].as_u64().or_else(|| c["rows"].as_u64()) {
            assert_eq!(v.len() as u64, n)
        }
        let mut trials = Vec::new();
        let baseline_start = Instant::now();
        let mut baseline_best = usize::MAX;
        let mut baseline_winner = String::new();
        let prefix = usize::from(c["mixed_integer"].as_bool().unwrap_or(false));
        for payload in adaptive_int::candidates(&v) {
            let name = baseline_name(&payload);
            assert_eq!(adaptive_int::decode(&payload, v.len()).unwrap(), v);
            let wire = if prefix == 1 {
                let mut w = vec![0];
                w.extend_from_slice(&payload);
                w
            } else {
                payload
            };
            let t = Instant::now();
            let z = zstd::bulk::compress(&wire, 3).unwrap();
            let ct = t.elapsed().as_secs_f64();
            let stored = z.len().min(wire.len());
            let t = Instant::now();
            let raw = zstd::bulk::decompress(&z, wire.len()).unwrap();
            assert_eq!(raw, wire);
            assert_eq!(adaptive_int::decode(&raw[prefix..], v.len()).unwrap(), v);
            let dt = t.elapsed().as_secs_f64();
            if stored < baseline_best {
                baseline_best = stored;
                baseline_winner = name.clone();
            }
            trials.push(json!({"name":name,"family":"baseline","encoded_bytes":wire.len(),"zstd_bytes":z.len(),"stored_bytes":stored,"compress_seconds":ct,"decode_seconds":dt}));
        }
        let baseline_seconds = baseline_start.elapsed().as_secs_f64();
        let mut best = baseline_best;
        let mut winner = baseline_winner.clone();
        for &k in KINDS {
            let name = format!("{:?}", k);
            let t = Instant::now();
            let payload = encode(&v, k);
            let wire = if prefix == 1 {
                let mut w = vec![0];
                w.extend_from_slice(&payload);
                w
            } else {
                payload
            };
            let enc = t.elapsed().as_secs_f64();
            let t = Instant::now();
            let z = zstd::bulk::compress(&wire, 3).unwrap();
            let ct = t.elapsed().as_secs_f64();
            let stored = z.len().min(wire.len());
            let t = Instant::now();
            let raw = zstd::bulk::decompress(&z, wire.len()).unwrap();
            assert_eq!(
                decode(&raw[prefix..], v.len(), k),
                v,
                "{} {}",
                c["id"],
                name
            );
            let dt = t.elapsed().as_secs_f64();
            if stored < best {
                best = stored;
                winner = name.clone()
            }
            let entry = aggregate
                .entry(fmt.into())
                .or_default()
                .entry(name.clone())
                .or_default();
            entry.0 += stored as u64;
            entry.1 += baseline_best.min(stored) as u64;
            entry.2 += enc + ct;
            entry.3 += dt;
            if stored < baseline_best {
                entry.4 += 1;
            }
            trials.push(json!({"name":name,"family":"experimental","encoded_bytes":wire.len(),"zstd_bytes":z.len(),"stored_bytes":stored,"encode_seconds":enc,"compress_seconds":ct,"decode_seconds":dt}));
        }
        let a = aggregate.entry(fmt.into()).or_default();
        a.entry("BASELINE".into()).or_default().0 += baseline_best as u64;
        a.entry("ADAPTIVE_PLUS_ALL".into()).or_default().0 += best as u64;
        writeln!(per,"{}",json!({"column":c,"rows":v.len(),"baseline_bytes":baseline_best,"baseline_winner":baseline_winner,"baseline_seconds":baseline_seconds,"actual_frame_bytes":c["current_compressed_bytes"],"baseline_matches_actual":c["current_compressed_bytes"].as_u64().map(|n|n==baseline_best as u64),"best_bytes":best,"winner":winner,"trials":trials})).unwrap();
        per.flush().unwrap();
        count += 1;
        rows += v.len() as u64;
        if count % 25 == 0 {
            eprintln!(
                "{} columns, {} rows, {:.1}s",
                count,
                rows,
                start.elapsed().as_secs_f64()
            );
        }
        let summary = json!({"manifest":args[1],"zstd_level":3,"restart_rows":ROWS,"raw_frame_fallback":true,"mixed_integer_flag_included":true,"columns":count,"rows":rows,"elapsed_seconds":start.elapsed().as_secs_f64(),"aggregate_tuple_fields":["standalone_stored_bytes","baseline_plus_candidate_bytes","encode_and_compress_seconds","decompress_and_decode_seconds","winning_columns"],"aggregate":aggregate});
        fs::write(
            outdir.join("summary.json"),
            serde_json::to_vec_pretty(&summary).unwrap(),
        )
        .unwrap();
    }
    eprintln!(
        "DONE {} columns, {} rows, {:.1}s",
        count,
        rows,
        start.elapsed().as_secs_f64()
    );
}
