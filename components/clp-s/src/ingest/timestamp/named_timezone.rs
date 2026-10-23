//! Resolve names using bundled IANA data, never the host's timezone configuration.
use std::collections::BTreeMap;
use std::sync::OnceLock;

use super::{FractionPrecision, JsonTimestampError, iso_date_time_pattern};
use tz::TimeZoneRef;
use tz::timezone::{LocalTimeType, TransitionRule};

struct Candidate {
    zones: Vec<TimeZoneRef<'static>>,
    patterns: [OnceLock<String>; 42],
}

impl Default for Candidate {
    fn default() -> Self {
        Self { zones: Vec::new(), patterns: std::array::from_fn(|_| OnceLock::new()) }
    }
}

type Candidates = BTreeMap<i32, Candidate>;

#[derive(Default)]
struct Registry {
    names: BTreeMap<&'static str, Candidates>,
    abbreviations: BTreeMap<&'static str, Candidates>,
}

static REGISTRY: OnceLock<Registry> = OnceLock::new();

fn registry() -> &'static Registry {
    REGISTRY.get_or_init(|| {
        let mut result = Registry::default();
        for &name in tzdb::TZ_NAMES {
            let zone = tzdb::tz_by_name(name).expect("bundled timezone name exists");
            let mut add = |local: &'static LocalTimeType| {
                let offset = local.ut_offset();
                result.names.entry(name).or_default().entry(offset).or_default();
                let abbreviation = local.time_zone_designation();
                if !abbreviation.is_empty() && abbreviation.bytes().all(|c| c.is_ascii_alphabetic()) {
                    let candidate = result.abbreviations.entry(abbreviation).or_default()
                        .entry(offset).or_default();
                    if !candidate.zones.contains(&zone) {
                        candidate.zones.push(zone);
                    }
                }
            };
            for local in zone.local_time_types() {
                add(local);
            }
            match zone.extra_rule() {
                Some(TransitionRule::Fixed(local)) => add(local),
                Some(TransitionRule::Alternate(rule)) => {
                    add(rule.std());
                    add(rule.dst());
                }
                None => {}
            }
        }
        result
    })
}

pub(super) struct Resolved {
    name: &'static str,
    pub(super) offset_seconds: i32,
    candidate: &'static Candidate,
}

pub(super) fn resolve(name: &str, local_seconds: i64) -> Result<Resolved, JsonTimestampError> {
    let registry = registry();
    let (name, candidates, abbreviation) =
        if let Some((&name, candidates)) = registry.abbreviations.get_key_value(name) {
            (name, candidates, true)
        } else if let Some((&name, candidates)) = registry.names.get_key_value(name) {
            (name, candidates, false)
        } else {
            return Err(JsonTimestampError::UnknownTimezone);
        };
    let mut selected = None;
    for (&offset, candidate) in candidates {
        let utc = local_seconds.checked_sub(i64::from(offset))
            .ok_or(JsonTimestampError::EpochNanosecondsOutOfRange)?;
        // Unambiguous abbreviations denote fixed offsets, including EST in summer.
        // For names with multiple historical meanings, check the supplied date.
        let matches = if abbreviation && candidates.len() == 1 {
            true
        } else if abbreviation {
            candidate.zones.iter().any(|zone| zone.find_local_time_type(utc).is_ok_and(|local| {
                local.ut_offset() == offset && local.time_zone_designation() == name
            }))
        } else {
            tzdb::tz_by_name(name).is_some_and(|zone| {
                zone.find_local_time_type(utc).is_ok_and(|local| local.ut_offset() == offset)
            })
        };
        if matches {
            if selected.is_some() {
                return Err(JsonTimestampError::AmbiguousTimezone);
            }
            selected = Some(Resolved { name, offset_seconds: offset, candidate });
        }
    }
    selected.ok_or(JsonTimestampError::NonexistentLocalTime)
}

impl Resolved {
    pub(super) fn pattern(
        &self,
        separator: u8,
        fraction: Option<(u8, &str, FractionPrecision)>,
    ) -> Result<&'static str, JsonTimestampError> {
        let base = iso_date_time_pattern(separator, fraction, false)?;
        let index = usize::from(separator == b' ') * 21 + super::fraction_format_index(fraction);
        // Finite database-derived slots; repeated events allocate nothing. The pattern
        // stores its resolved offset so extraction never depends on future tzdb updates.
        Ok(self.candidate.patterns[index].get_or_init(|| {
            format!("{} \\z{{{}|{}}}\"", &base[..base.len() - 1], self.offset_seconds, self.name)
        }))
    }
}
