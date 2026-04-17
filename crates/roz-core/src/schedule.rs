use std::str::FromStr;

use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;
use cron::Schedule;
use regex::Regex;

pub const DEFAULT_PREVIEW_COUNT: usize = 5;
pub const MAX_CATCH_UP_WINDOW: Duration = Duration::days(7);

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatchUpPolicy {
    SkipMissed,
    RunLatest,
    RunAll,
}

impl CatchUpPolicy {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SkipMissed => "skip_missed",
            Self::RunLatest => "run_latest",
            Self::RunAll => "run_all",
        }
    }
}

impl std::fmt::Display for CatchUpPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for CatchUpPolicy {
    type Err = ScheduleError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "skip_missed" | "SkipMissed" => Ok(Self::SkipMissed),
            "run_latest" | "RunLatest" => Ok(Self::RunLatest),
            "run_all" | "RunAll" => Ok(Self::RunAll),
            _ => Err(ScheduleError::InvalidCatchUpPolicy(value.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledOccurrence {
    pub fire_at_utc: DateTime<Utc>,
    pub fire_at_local: DateTime<Tz>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatchUpResolution {
    pub due_runs: Vec<ScheduledOccurrence>,
    pub next_fire_at_utc: Option<DateTime<Utc>>,
    pub window_truncated: bool,
}

#[derive(Debug, Clone)]
pub struct ScheduleDefinition {
    canonical_cron: String,
    timezone: Tz,
    schedule: Schedule,
}

impl ScheduleDefinition {
    pub fn parse(cron_expression: &str, timezone_name: &str) -> Result<Self, ScheduleError> {
        let canonical_cron = canonicalize_cron(cron_expression)?;
        let timezone = timezone_name
            .parse::<Tz>()
            .map_err(|_| ScheduleError::InvalidTimezone(timezone_name.to_string()))?;
        let schedule = Schedule::from_str(&canonical_cron).map_err(|source| ScheduleError::InvalidCron {
            cron: canonical_cron.clone(),
            source,
        })?;

        Ok(Self {
            canonical_cron,
            timezone,
            schedule,
        })
    }

    #[must_use]
    pub fn canonical_cron(&self) -> &str {
        &self.canonical_cron
    }

    #[must_use]
    pub fn timezone(&self) -> Tz {
        self.timezone
    }

    #[must_use]
    pub fn timezone_name(&self) -> String {
        self.timezone.to_string()
    }

    pub fn preview_next_runs(
        &self,
        now_utc: DateTime<Utc>,
        count: usize,
    ) -> Result<Vec<ScheduledOccurrence>, ScheduleError> {
        if count == 0 {
            return Ok(Vec::new());
        }

        let start_local = now_utc.with_timezone(&self.timezone);
        self.schedule
            .after(&start_local)
            .take(count)
            .map(|fire_at_local| {
                Ok(ScheduledOccurrence {
                    fire_at_utc: fire_at_local.with_timezone(&Utc),
                    fire_at_local,
                })
            })
            .collect()
    }

    pub fn next_fire_after(&self, after_utc: DateTime<Utc>) -> Result<Option<ScheduledOccurrence>, ScheduleError> {
        let after_local = after_utc.with_timezone(&self.timezone);
        Ok(self
            .schedule
            .after(&after_local)
            .next()
            .map(|fire_at_local| ScheduledOccurrence {
                fire_at_utc: fire_at_local.with_timezone(&Utc),
                fire_at_local,
            }))
    }

    pub fn resolve_catch_up(
        &self,
        next_fire_at_utc: Option<DateTime<Utc>>,
        now_utc: DateTime<Utc>,
        policy: CatchUpPolicy,
    ) -> Result<CatchUpResolution, ScheduleError> {
        let Some(next_fire_at_utc) = next_fire_at_utc else {
            return Ok(CatchUpResolution {
                due_runs: Vec::new(),
                next_fire_at_utc: self.next_fire_after(now_utc)?.map(|fire| fire.fire_at_utc),
                window_truncated: false,
            });
        };

        if next_fire_at_utc > now_utc {
            return Ok(CatchUpResolution {
                due_runs: Vec::new(),
                next_fire_at_utc: Some(next_fire_at_utc),
                window_truncated: false,
            });
        }

        let window_start = now_utc - MAX_CATCH_UP_WINDOW;
        let due_start = if next_fire_at_utc < window_start {
            window_start
        } else {
            next_fire_at_utc
        };
        let window_truncated = next_fire_at_utc < window_start;
        let due_runs = self.occurrences_between(due_start, now_utc);

        let due_runs = match policy {
            CatchUpPolicy::SkipMissed => Vec::new(),
            CatchUpPolicy::RunLatest => due_runs.into_iter().rev().take(1).collect(),
            CatchUpPolicy::RunAll => due_runs,
        };

        let next_cursor = due_runs.last().map(|fire| fire.fire_at_utc).unwrap_or(now_utc);
        let next_fire_at_utc = self.next_fire_after(next_cursor)?.map(|fire| fire.fire_at_utc);

        Ok(CatchUpResolution {
            due_runs,
            next_fire_at_utc,
            window_truncated,
        })
    }

    fn occurrences_between(
        &self,
        start_inclusive_utc: DateTime<Utc>,
        end_inclusive_utc: DateTime<Utc>,
    ) -> Vec<ScheduledOccurrence> {
        if start_inclusive_utc > end_inclusive_utc {
            return Vec::new();
        }

        let cursor_local = (start_inclusive_utc - Duration::seconds(1)).with_timezone(&self.timezone);
        self.schedule
            .after(&cursor_local)
            .take_while(|fire_at_local| fire_at_local.with_timezone(&Utc) <= end_inclusive_utc)
            .map(|fire_at_local| ScheduledOccurrence {
                fire_at_utc: fire_at_local.with_timezone(&Utc),
                fire_at_local,
            })
            .collect()
    }
}

pub fn canonicalize_cron(cron_expression: &str) -> Result<String, ScheduleError> {
    let fields: Vec<_> = cron_expression.split_whitespace().collect();
    if fields.len() != 6 {
        return Err(ScheduleError::ExpectedSixFields {
            got: fields.len(),
            cron: cron_expression.to_string(),
        });
    }

    let canonical_cron = fields.join(" ");
    Schedule::from_str(&canonical_cron).map_err(|source| ScheduleError::InvalidCron {
        cron: canonical_cron.clone(),
        source,
    })?;
    Ok(canonical_cron)
}

pub fn parse_natural_language_schedule(nl_schedule: &str) -> Result<String, ScheduleError> {
    let normalized = nl_schedule
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_ascii_lowercase();
    if normalized.is_empty() {
        return Err(ScheduleError::EmptyNaturalLanguageSchedule);
    }

    if normalized == "every minute" || normalized == "each minute" {
        return Ok("0 * * * * *".to_string());
    }
    if normalized == "every hour" || normalized == "each hour" {
        return Ok("0 0 * * * *".to_string());
    }

    let every_minutes = Regex::new(r"^(?:every|each) (\d{1,2}) minutes?$").expect("valid minutes regex");
    if let Some(captures) = every_minutes.captures(&normalized) {
        let step = captures[1]
            .parse::<u32>()
            .map_err(|_| ScheduleError::InvalidNaturalLanguageSchedule(nl_schedule.to_string()))?;
        if !(1..=59).contains(&step) {
            return Err(ScheduleError::InvalidNaturalLanguageSchedule(nl_schedule.to_string()));
        }
        return Ok(format!("0 */{step} * * * *"));
    }

    let every_hours = Regex::new(r"^(?:every|each) (\d{1,2}) hours?$").expect("valid hours regex");
    if let Some(captures) = every_hours.captures(&normalized) {
        let step = captures[1]
            .parse::<u32>()
            .map_err(|_| ScheduleError::InvalidNaturalLanguageSchedule(nl_schedule.to_string()))?;
        if !(1..=23).contains(&step) {
            return Err(ScheduleError::InvalidNaturalLanguageSchedule(nl_schedule.to_string()));
        }
        return Ok(format!("0 0 */{step} * * *"));
    }

    let Some((day_expr, time_expr)) = normalized.split_once(" at ") else {
        return Err(ScheduleError::InvalidNaturalLanguageSchedule(nl_schedule.to_string()));
    };
    let day_of_week = parse_day_expression(day_expr)?;
    let (hour, minute) = parse_time_expression(time_expr, nl_schedule)?;
    Ok(format!("0 {minute} {hour} * * {day_of_week}"))
}

fn parse_day_expression(day_expr: &str) -> Result<&'static str, ScheduleError> {
    match day_expr.trim() {
        "every day" | "daily" | "each day" => Ok("*"),
        "every weekday" | "weekdays" | "each weekday" => Ok("Mon-Fri"),
        "every weekend" | "weekends" | "each weekend" => Ok("Sat,Sun"),
        "every monday" | "monday" | "every mon" | "mon" => Ok("Mon"),
        "every tuesday" | "tuesday" | "every tue" | "tue" | "every tues" | "tues" => Ok("Tue"),
        "every wednesday" | "wednesday" | "every wed" | "wed" => Ok("Wed"),
        "every thursday" | "thursday" | "every thu" | "thu" | "every thur" | "thur" | "every thurs" | "thurs" => {
            Ok("Thu")
        }
        "every friday" | "friday" | "every fri" | "fri" => Ok("Fri"),
        "every saturday" | "saturday" | "every sat" | "sat" => Ok("Sat"),
        "every sunday" | "sunday" | "every sun" | "sun" => Ok("Sun"),
        _ => Err(ScheduleError::InvalidNaturalLanguageSchedule(day_expr.to_string())),
    }
}

fn parse_time_expression(time_expr: &str, original: &str) -> Result<(u32, u32), ScheduleError> {
    let mut tokens = time_expr.split_whitespace();
    let first = tokens
        .next()
        .ok_or_else(|| ScheduleError::InvalidNaturalLanguageSchedule(original.to_string()))?;
    let second = tokens.next();
    let combined = match second {
        Some(suffix @ ("am" | "pm")) => format!("{first}{suffix}"),
        _ => first.to_string(),
    };

    let twelve_hour =
        Regex::new(r"^(?P<hour>\d{1,2})(?::(?P<minute>\d{2}))?(?P<suffix>am|pm)$").expect("valid twelve hour regex");
    if let Some(captures) = twelve_hour.captures(&combined) {
        let hour = captures["hour"]
            .parse::<u32>()
            .map_err(|_| ScheduleError::InvalidNaturalLanguageSchedule(original.to_string()))?;
        if !(1..=12).contains(&hour) {
            return Err(ScheduleError::InvalidNaturalLanguageSchedule(original.to_string()));
        }
        let minute = captures
            .name("minute")
            .map(|value| value.as_str().parse::<u32>())
            .transpose()
            .map_err(|_| ScheduleError::InvalidNaturalLanguageSchedule(original.to_string()))?
            .unwrap_or(0);
        if minute > 59 {
            return Err(ScheduleError::InvalidNaturalLanguageSchedule(original.to_string()));
        }
        let suffix = &captures["suffix"];
        let hour = match (hour, suffix) {
            (12, "am") => 0,
            (12, "pm") => 12,
            (hour, "am") => hour,
            (hour, "pm") => hour + 12,
            _ => unreachable!("regex constrains suffix"),
        };
        return Ok((hour, minute));
    }

    let twenty_four = Regex::new(r"^(?P<hour>\d{1,2}):(?P<minute>\d{2})$").expect("valid twenty four hour regex");
    if let Some(captures) = twenty_four.captures(&combined) {
        let hour = captures["hour"]
            .parse::<u32>()
            .map_err(|_| ScheduleError::InvalidNaturalLanguageSchedule(original.to_string()))?;
        let minute = captures["minute"]
            .parse::<u32>()
            .map_err(|_| ScheduleError::InvalidNaturalLanguageSchedule(original.to_string()))?;
        if hour > 23 || minute > 59 {
            return Err(ScheduleError::InvalidNaturalLanguageSchedule(original.to_string()));
        }
        return Ok((hour, minute));
    }

    Err(ScheduleError::InvalidNaturalLanguageSchedule(original.to_string()))
}

#[derive(Debug, thiserror::Error)]
pub enum ScheduleError {
    #[error("cron expression must use Roz's six-field syntax, got {got} fields: {cron}")]
    ExpectedSixFields { got: usize, cron: String },
    #[error("invalid cron expression `{cron}`: {source}")]
    InvalidCron {
        cron: String,
        #[source]
        source: cron::error::Error,
    },
    #[error("invalid timezone `{0}`")]
    InvalidTimezone(String),
    #[error("invalid catch-up policy `{0}`")]
    InvalidCatchUpPolicy(String),
    #[error("natural-language schedule cannot be empty")]
    EmptyNaturalLanguageSchedule,
    #[error("invalid natural-language schedule `{0}`")]
    InvalidNaturalLanguageSchedule(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn schedule_preview_returns_next_five_local_runs() {
        let schedule = ScheduleDefinition::parse("0 0 9 * * Mon-Fri", "America/New_York").unwrap();
        let now = Utc.with_ymd_and_hms(2026, 4, 6, 12, 0, 0).unwrap();

        let preview = schedule.preview_next_runs(now, DEFAULT_PREVIEW_COUNT).unwrap();

        assert_eq!(preview.len(), 5);
        assert_eq!(
            preview
                .iter()
                .map(|run| run.fire_at_local.to_rfc3339())
                .collect::<Vec<_>>(),
            vec![
                "2026-04-06T09:00:00-04:00",
                "2026-04-07T09:00:00-04:00",
                "2026-04-08T09:00:00-04:00",
                "2026-04-09T09:00:00-04:00",
                "2026-04-10T09:00:00-04:00",
            ]
        );
    }

    #[test]
    fn schedule_preview_skips_nonexistent_spring_forward_time() {
        let schedule = ScheduleDefinition::parse("0 30 2 * * *", "America/New_York").unwrap();
        let now = Utc.with_ymd_and_hms(2026, 3, 7, 6, 0, 0).unwrap();

        let preview = schedule.preview_next_runs(now, 4).unwrap();

        assert_eq!(
            preview
                .iter()
                .map(|run| run.fire_at_local.to_rfc3339())
                .collect::<Vec<_>>(),
            vec![
                "2026-03-07T02:30:00-05:00",
                "2026-03-09T02:30:00-04:00",
                "2026-03-10T02:30:00-04:00",
                "2026-03-11T02:30:00-04:00",
            ]
        );
    }

    #[test]
    fn schedule_preview_keeps_both_fall_back_occurrences() {
        let schedule = ScheduleDefinition::parse("0 30 1 * * *", "America/New_York").unwrap();
        let now = Utc.with_ymd_and_hms(2026, 10, 31, 5, 0, 0).unwrap();

        let preview = schedule.preview_next_runs(now, 4).unwrap();

        assert_eq!(
            preview
                .iter()
                .map(|run| run.fire_at_local.to_rfc3339())
                .collect::<Vec<_>>(),
            vec![
                "2026-10-31T01:30:00-04:00",
                "2026-11-01T01:30:00-04:00",
                "2026-11-01T01:30:00-05:00",
                "2026-11-02T01:30:00-05:00",
            ]
        );
    }

    fn local_rfc3339(occurrences: &[ScheduledOccurrence]) -> Vec<String> {
        occurrences.iter().map(|run| run.fire_at_local.to_rfc3339()).collect()
    }

    #[test]
    fn schedule_catch_up_skip_missed_advances_without_replay() {
        let schedule = ScheduleDefinition::parse("0 0 * * * *", "UTC").unwrap();
        let now = Utc.with_ymd_and_hms(2026, 4, 16, 10, 15, 0).unwrap();
        let next_fire = Utc.with_ymd_and_hms(2026, 4, 16, 10, 0, 0).unwrap();

        let resolution = schedule
            .resolve_catch_up(Some(next_fire), now, CatchUpPolicy::SkipMissed)
            .unwrap();

        assert!(resolution.due_runs.is_empty());
        assert_eq!(
            resolution.next_fire_at_utc,
            Some(Utc.with_ymd_and_hms(2026, 4, 16, 11, 0, 0).unwrap())
        );
        assert!(!resolution.window_truncated);
    }

    #[test]
    fn schedule_catch_up_run_all_skips_nonexistent_spring_forward_slot() {
        let schedule = ScheduleDefinition::parse("0 30 2 * * *", "America/New_York").unwrap();
        let now = Utc.with_ymd_and_hms(2026, 3, 9, 8, 0, 0).unwrap();
        let next_fire = Utc.with_ymd_and_hms(2026, 3, 7, 7, 30, 0).unwrap();

        let resolution = schedule
            .resolve_catch_up(Some(next_fire), now, CatchUpPolicy::RunAll)
            .unwrap();

        assert_eq!(
            local_rfc3339(&resolution.due_runs),
            vec!["2026-03-07T02:30:00-05:00", "2026-03-09T02:30:00-04:00"]
        );
        assert_eq!(
            resolution.next_fire_at_utc,
            Some(Utc.with_ymd_and_hms(2026, 3, 10, 6, 30, 0).unwrap())
        );
        assert!(!resolution.window_truncated);
    }

    #[test]
    fn schedule_catch_up_policies_respect_fall_back_overlap() {
        let schedule = ScheduleDefinition::parse("0 30 1 * * *", "America/New_York").unwrap();
        let now = Utc.with_ymd_and_hms(2026, 11, 1, 7, 0, 0).unwrap();
        let next_fire = Utc.with_ymd_and_hms(2026, 11, 1, 5, 30, 0).unwrap();
        let expected_next_fire = Some(Utc.with_ymd_and_hms(2026, 11, 2, 6, 30, 0).unwrap());

        let run_all = schedule
            .resolve_catch_up(Some(next_fire), now, CatchUpPolicy::RunAll)
            .unwrap();
        assert_eq!(
            local_rfc3339(&run_all.due_runs),
            vec!["2026-11-01T01:30:00-04:00", "2026-11-01T01:30:00-05:00"]
        );
        assert_eq!(run_all.next_fire_at_utc, expected_next_fire);
        assert!(!run_all.window_truncated);

        let run_latest = schedule
            .resolve_catch_up(Some(next_fire), now, CatchUpPolicy::RunLatest)
            .unwrap();
        assert_eq!(local_rfc3339(&run_latest.due_runs), vec!["2026-11-01T01:30:00-05:00"]);
        assert_eq!(run_latest.next_fire_at_utc, expected_next_fire);
        assert!(!run_latest.window_truncated);

        let skip_missed = schedule
            .resolve_catch_up(Some(next_fire), now, CatchUpPolicy::SkipMissed)
            .unwrap();
        assert!(skip_missed.due_runs.is_empty());
        assert_eq!(skip_missed.next_fire_at_utc, expected_next_fire);
        assert!(!skip_missed.window_truncated);
    }

    #[test]
    fn schedule_catch_up_run_latest_only_replays_latest_due_slot() {
        let schedule = ScheduleDefinition::parse("0 */15 * * * *", "UTC").unwrap();
        let now = Utc.with_ymd_and_hms(2026, 4, 16, 10, 0, 0).unwrap();
        let next_fire = Utc.with_ymd_and_hms(2026, 4, 16, 9, 0, 0).unwrap();

        let resolution = schedule
            .resolve_catch_up(Some(next_fire), now, CatchUpPolicy::RunLatest)
            .unwrap();

        assert_eq!(
            resolution
                .due_runs
                .iter()
                .map(|run| run.fire_at_utc)
                .collect::<Vec<_>>(),
            vec![Utc.with_ymd_and_hms(2026, 4, 16, 10, 0, 0).unwrap()]
        );
        assert_eq!(
            resolution.next_fire_at_utc,
            Some(Utc.with_ymd_and_hms(2026, 4, 16, 10, 15, 0).unwrap())
        );
    }

    #[test]
    fn schedule_catch_up_run_all_is_bounded_to_seven_days() {
        let schedule = ScheduleDefinition::parse("0 0 9 * * *", "America/New_York").unwrap();
        let now = Utc.with_ymd_and_hms(2026, 4, 16, 14, 0, 0).unwrap();
        let next_fire = Utc.with_ymd_and_hms(2026, 4, 1, 13, 0, 0).unwrap();

        let resolution = schedule
            .resolve_catch_up(Some(next_fire), now, CatchUpPolicy::RunAll)
            .unwrap();

        assert_eq!(
            resolution
                .due_runs
                .iter()
                .map(|run| run.fire_at_local.to_rfc3339())
                .collect::<Vec<_>>(),
            vec![
                "2026-04-10T09:00:00-04:00",
                "2026-04-11T09:00:00-04:00",
                "2026-04-12T09:00:00-04:00",
                "2026-04-13T09:00:00-04:00",
                "2026-04-14T09:00:00-04:00",
                "2026-04-15T09:00:00-04:00",
                "2026-04-16T09:00:00-04:00",
            ]
        );
        assert_eq!(
            resolution.next_fire_at_utc,
            Some(Utc.with_ymd_and_hms(2026, 4, 17, 13, 0, 0).unwrap())
        );
        assert!(resolution.window_truncated);
    }

    #[test]
    fn schedule_rejects_non_six_field_cron() {
        let err = ScheduleDefinition::parse("0 9 * * *", "UTC").unwrap_err();

        assert!(matches!(err, ScheduleError::ExpectedSixFields { got: 5, .. }));
    }

    #[test]
    fn natural_language_schedule_parses_weekdays_with_timezone_suffix() {
        let cron = parse_natural_language_schedule("every weekday at 9am Eastern").unwrap();

        assert_eq!(cron, "0 0 9 * * Mon-Fri");
    }

    #[test]
    fn natural_language_schedule_parses_every_fifteen_minutes() {
        let cron = parse_natural_language_schedule("every 15 minutes").unwrap();

        assert_eq!(cron, "0 */15 * * * *");
    }

    #[test]
    fn natural_language_schedule_rejects_unknown_form() {
        let err = parse_natural_language_schedule("on blue moons").unwrap_err();

        assert!(matches!(err, ScheduleError::InvalidNaturalLanguageSchedule(_)));
    }
}
