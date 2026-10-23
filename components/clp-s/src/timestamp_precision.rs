//! Explicit archive timestamp ticks and deliberate precision reduction.
//! Source timestamp patterns/units are independent of this physical storage unit.

/// Duration of one native timestamp tick, expressed in positive integer nanoseconds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimestampPrecision(i64);

/// Ingestion policy; only a resolved, concrete precision is written to an archive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimestampPrecisionSetting {
    Auto,
    Explicit(TimestampPrecision),
}

impl std::str::FromStr for TimestampPrecisionSetting {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value == "auto" { Ok(Self::Auto) } else { value.parse().map(Self::Explicit) }
    }
}

impl TimestampPrecisionSetting {
    pub fn storage_default(self) -> TimestampPrecision {
        match self { Self::Auto => TimestampPrecision::Nanoseconds, Self::Explicit(p) => p }
    }
}

impl Default for TimestampPrecision {
    fn default() -> Self {
        Self::Nanoseconds
    }
}

#[allow(non_upper_case_globals)]
impl TimestampPrecision {
    pub const Nanoseconds: Self = Self(1);
    pub const Microseconds: Self = Self(1_000);
    pub const Milliseconds: Self = Self(1_000_000);
    pub const Seconds: Self = Self(1_000_000_000);

    /// Nanoseconds represented by one stored tick.
    pub const fn nanoseconds_per_tick(self) -> i64 {
        self.0
    }

    /// Validate a tick duration read from timestamp field metadata.
    pub const fn from_nanoseconds_per_tick(value: u64) -> Option<Self> {
        if value == 0 || value > i64::MAX as u64 {
            None
        } else {
            Some(Self(value as i64))
        }
    }

    /// Quantize an epoch to stored ticks, rejecting a result that cannot be normalized to i64 ns.
    /// Truncation floors to the unit boundary, including for pre-epoch timestamps. Nearest rounds
    /// halfway cases away from the Unix epoch. i128 arithmetic avoids intermediate overflow.
    pub fn quantize(self, epoch_ns: i64, rounding: TimestampRounding) -> Option<i64> {
        let value = i128::from(epoch_ns);
        let scale = i128::from(self.nanoseconds_per_tick());
        let ticks = match rounding {
            TimestampRounding::Truncate => value.div_euclid(scale),
            TimestampRounding::Nearest => value.signum() * ((value.abs() + scale / 2) / scale),
        };
        i64::try_from(ticks * scale).ok()?;
        i64::try_from(ticks).ok()
    }
}

impl std::str::FromStr for TimestampPrecision {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let split = value
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(value.len());
        let (count, unit) = value.split_at(split);
        let scale = match unit {
            "ns" => 1_u64,
            "us" => 1_000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            _ => {
                return Err(format!(
                    "invalid timestamp precision `{value}`; use ns, us, ms, s or a positive integer multiple such as 100us"
                ));
            }
        };
        let count = if count.is_empty() {
            1
        } else {
            count
                .parse::<u64>()
                .map_err(|_| format!("timestamp precision `{value}` overflows"))?
        };
        count
            .checked_mul(scale)
            .and_then(Self::from_nanoseconds_per_tick)
            .ok_or_else(|| {
                format!(
                    "timestamp precision `{value}` must be 1..{} nanoseconds per tick",
                    i64::MAX
                )
            })
    }
}

impl std::fmt::Display for TimestampPrecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (scale, unit) in [
            (1_000_000_000, "s"),
            (1_000_000, "ms"),
            (1_000, "us"),
            (1, "ns"),
        ] {
            if self.0 % scale == 0 {
                return write!(f, "{}{unit}", self.0 / scale);
            }
        }
        unreachable!("one nanosecond divides every tick duration")
    }
}

/// How a timestamp with finer precision is reduced to stored ticks.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
#[repr(u64)]
pub enum TimestampRounding {
    /// Floor to the selected unit boundary (discard fractional ticks).
    #[default]
    Truncate = 0,
    /// Nearest tick, with halfway cases away from the Unix epoch.
    #[cfg_attr(feature = "cli", value(alias = "round"))]
    Nearest = 1,
}

impl TimestampRounding {
    /// Validate the recorded quantization mode.
    pub const fn from_wire(value: u64) -> Option<Self> {
        match value {
            0 => Some(Self::Truncate),
            1 => Some(Self::Nearest),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[allow(non_snake_case)]
    fn quantization_handles_negative_epochs_ties_and_overflow() {
        let (Nanoseconds, Milliseconds, Seconds) = (
            TimestampPrecision::Nanoseconds,
            TimestampPrecision::Milliseconds,
            TimestampPrecision::Seconds,
        );
        use TimestampRounding::*;
        assert_eq!(Some(1234), Milliseconds.quantize(1_234_567_890, Truncate));
        assert_eq!(Some(1235), Milliseconds.quantize(1_234_567_890, Nearest));
        assert_eq!(Some(-1235), Milliseconds.quantize(-1_234_567_890, Truncate));
        assert_eq!(Some(-1235), Milliseconds.quantize(-1_234_567_890, Nearest));
        assert_eq!(Some(1), Seconds.quantize(500_000_000, Nearest));
        assert_eq!(Some(-1), Seconds.quantize(-500_000_000, Nearest));
        assert_eq!(Some(-1), Seconds.quantize(-1, Truncate));
        for value in [i64::MIN, -1, 0, 1, i64::MAX] {
            assert_eq!(Some(value), Nanoseconds.quantize(value, Truncate));
            assert_eq!(Some(value), Nanoseconds.quantize(value, Nearest));
        }
        assert_eq!(None, Seconds.quantize(i64::MAX, Nearest));
        assert_eq!(None, Seconds.quantize(i64::MIN, Truncate));
    }
}

/// Per-field archive precision and quantization policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimestampStorage {
    pub precision: TimestampPrecision,
    pub rounding: TimestampRounding,
}
impl Default for TimestampStorage {
    fn default() -> Self {
        Self {
            precision: TimestampPrecision::Nanoseconds,
            rounding: TimestampRounding::Truncate,
        }
    }
}

/// Parsed CLI declaration: one unit for numeric input, storage ticks, and numeric output.
#[derive(Clone, Debug)]
pub struct TimestampFieldSpec {
    pub path: crate::ingest::KvIrTimestampResolver,
    pub storage: TimestampStorage,
    pub auto_precision: bool,
}
impl TimestampFieldSpec {
    /// Parse `field:precision=ms:rounding=nearest`.
    /// A literal colon in a field name is written `\u003a`. Duplicate options are errors.
    pub fn parse(spec: &str, defaults: TimestampStorage) -> Result<Self, String> {
        Self::parse_with_auto_default(spec, defaults, false)
    }

    pub fn parse_with_auto_default(spec: &str, defaults: TimestampStorage, auto_precision: bool) -> Result<Self, String> {
        let mut parts = spec.split(':');
        let descriptor = parts.next().unwrap_or_default();
        let path =
            crate::ingest::KvIrTimestampResolver::parse(descriptor).map_err(|e| e.to_string())?;
        if path.namespace().is_none() {
            return Err("timestamp field uses a reserved namespace".into());
        }
        let mut result = Self {
            path,
            storage: defaults,
            auto_precision,
        };
        let mut seen = std::collections::HashSet::new();
        for option in parts {
            let (name, value) = option
                .split_once('=')
                .ok_or_else(|| format!("timestamp option `{option}` must be name=value"))?;
            if !seen.insert(name) {
                return Err(format!("duplicate timestamp option `{name}`"));
            }
            match name {
                "precision" => {
                    let setting: TimestampPrecisionSetting = value.parse()?;
                    result.auto_precision = setting == TimestampPrecisionSetting::Auto;
                    result.storage.precision = setting.storage_default();
                }
                "rounding" => {
                    result.storage.rounding = match value {
                        "truncate" => TimestampRounding::Truncate,
                        "nearest" | "round" => TimestampRounding::Nearest,
                        _ => return Err(format!("invalid timestamp rounding `{value}`")),
                    }
                }
                _ => return Err(format!("unknown timestamp option `{name}`")),
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod field_spec_tests {
    use super::*;
    #[test]
    fn field_options_are_order_independent() {
        let spec = TimestampFieldSpec::parse(
            r"event\u003atime:precision=ms:rounding=nearest",
            TimestampStorage::default(),
        )
        .unwrap();
        assert_eq!(spec.path.path().components(), &["event:time"]);
        assert_eq!(spec.storage.precision, TimestampPrecision::Milliseconds);
        assert_eq!(spec.storage.rounding, TimestampRounding::Nearest);
        assert_eq!(
            TimestampFieldSpec::parse("ts", TimestampStorage::default())
                .unwrap()
                .storage,
            TimestampStorage::default()
        );
    }
    #[test]
    fn invalid_options_fail_instead_of_silently_changing_semantics() {
        for bad in [
            "",
            "ts:foo=ms",
            "ts:precision=ms:precision=ns",
            "ts:precision=ps",
            "ts:increment=100ms",
            "ts:factor=100",
            "ts:rounding=ceil",
            "ts:input-unit=ms",
            "ts:unit=ms",
            "ts:precision",
            "ts:",
        ] {
            assert!(
                TimestampFieldSpec::parse(bad, TimestampStorage::default()).is_err(),
                "{bad}"
            );
        }
    }
}

#[cfg(test)]
mod tick_duration_tests {
    use super::*;
    #[test]
    fn aliases_and_positive_integer_tick_durations_are_exact() {
        for (unit, ns) in [
            ("ns", 1),
            ("us", 1_000),
            ("ms", 1_000_000),
            ("s", 1_000_000_000),
        ] {
            let base: TimestampPrecision = unit.parse().unwrap();
            assert_eq!(base, format!("1{unit}").parse().unwrap());
            for count in [1, 10, 100, 250] {
                let tick: TimestampPrecision = format!("{count}{unit}").parse().unwrap();
                assert_eq!(tick.nanoseconds_per_tick(), count * ns);
                assert_eq!(
                    tick.to_string().parse::<TimestampPrecision>().unwrap(),
                    tick
                );
                let spec = TimestampFieldSpec::parse(
                    &format!("ts:precision={count}{unit}"),
                    TimestampStorage::default(),
                )
                .unwrap();
                assert_eq!(spec.storage.precision, tick);
            }
        }
        assert_eq!(
            "1000us".parse::<TimestampPrecision>().unwrap(),
            TimestampPrecision::Milliseconds
        );
        assert_eq!(
            "9223372036854775807ns"
                .parse::<TimestampPrecision>()
                .unwrap()
                .nanoseconds_per_tick(),
            i64::MAX
        );
    }
    #[test]
    fn invalid_or_overflowing_ticks_are_rejected() {
        for bad in [
            "",
            "0ns",
            "0us",
            "0ms",
            "0s",
            "-1us",
            "+1us",
            "1.5ms",
            "1",
            "10ps",
            "1m",
            "1US",
            " 1us",
            "1us ",
            "1ms1us",
            "9223372036854775808ns",
            "9223372036854776us",
            "18446744073709551616ns",
            "18446744073709551615s",
        ] {
            assert!(bad.parse::<TimestampPrecision>().is_err(), "{bad}");
        }
        assert!(TimestampPrecision::from_nanoseconds_per_tick(0).is_none());
        assert!(TimestampPrecision::from_nanoseconds_per_tick(i64::MAX as u64 + 1).is_none());
    }
    #[test]
    fn hundred_microsecond_ticks_floor_or_round_at_epoch_boundaries() {
        let tick: TimestampPrecision = "100us".parse().unwrap();
        for (ns, floor, nearest) in [
            (123_456, 1, 1),
            (150_000, 1, 2),
            (-1, -1, 0),
            (-150_000, -2, -2),
            (-123_456, -2, -1),
        ] {
            assert_eq!(tick.quantize(ns, TimestampRounding::Truncate), Some(floor));
            assert_eq!(tick.quantize(ns, TimestampRounding::Nearest), Some(nearest));
        }
    }
}
