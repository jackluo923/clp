//! Bakes the binary's expiry day and a token that binds it, into the build.
//!
//! By default a build expires 15 days after it is compiled. `CLP_S_EXPIRY_DAY` is stamped with that
//! day (days since the Unix epoch) and `CLP_S_EXPIRY_TOKEN` with [`mix`] of a hard-coded seed and
//! that day; `main` recomputes the token and refuses to run when it no longer matches or the day
//! has passed, so editing just the stamped day does not extend the build. Building with
//! `CLP_S_NO_EXPIRE` set (to anything but empty or `0`) emits the `clp_s_no_expire` cfg instead,
//! which compiles the check out entirely. `CLP_S_EXPIRE_DAYS` overrides the window. The day is
//! captured whenever this script runs, so a clean build counts the window from that compilation.
//!
//! This only raises the bar against a casual edit: the seed is in the binary, and a local binary
//! can always be patched. Real enforcement would check against a server.

use std::time::SystemTime;
use std::time::UNIX_EPOCH;

const DEFAULT_EXPIRE_DAYS: u64 = 15;
const SECONDS_PER_DAY: u64 = 86_400;
const EXPIRY_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// A 64-bit avalanche mix (`SplitMix64` finalizer); kept identical to the binary's copy.
const fn mix(value: u64) -> u64 {
    let mut z = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn main() {
    println!("cargo:rerun-if-env-changed=CLP_S_NO_EXPIRE");
    println!("cargo:rerun-if-env-changed=CLP_S_EXPIRE_DAYS");
    println!("cargo:rustc-check-cfg=cfg(clp_s_no_expire)");

    let disabled =
        std::env::var_os("CLP_S_NO_EXPIRE").is_some_and(|value| !value.is_empty() && value != "0");
    if disabled {
        println!("cargo:rustc-cfg=clp_s_no_expire");
        return;
    }

    let days = std::env::var("CLP_S_EXPIRE_DAYS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_EXPIRE_DAYS);
    let now_day = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
        / SECONDS_PER_DAY;
    let expiry_day = now_day.saturating_add(days);
    println!("cargo:rustc-env=CLP_S_EXPIRY_DAY={expiry_day}");
    println!(
        "cargo:rustc-env=CLP_S_EXPIRY_TOKEN={}",
        mix(EXPIRY_SEED ^ expiry_day)
    );
}
