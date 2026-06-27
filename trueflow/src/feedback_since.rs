use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedFeedbackSince {
    All,
    Timestamp(i64),
    Last,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FeedbackSinceExpr(String);

impl FeedbackSinceExpr {
    pub fn new(value: impl AsRef<str>) -> Result<Self> {
        let value = value.as_ref().trim();
        if value.is_empty() {
            return Err(anyhow!("feedback since value cannot be empty"));
        }
        validate_feedback_since_expr(value)?;
        Ok(Self(value.to_string()))
    }

    pub fn all() -> Self {
        Self("all".to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn resolve(&self) -> Result<ResolvedFeedbackSince> {
        self.resolve_at(Utc::now())
    }

    pub fn resolve_at(&self, now: DateTime<Utc>) -> Result<ResolvedFeedbackSince> {
        resolve_feedback_since(self.as_str(), now)
    }
}

impl Default for FeedbackSinceExpr {
    fn default() -> Self {
        Self::all()
    }
}

impl fmt::Display for FeedbackSinceExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for FeedbackSinceExpr {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for FeedbackSinceExpr {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

fn validate_feedback_since_expr(raw: &str) -> Result<()> {
    let _ = resolve_feedback_since(raw, Utc::now())?;
    Ok(())
}

fn resolve_feedback_since(raw: &str, now: DateTime<Utc>) -> Result<ResolvedFeedbackSince> {
    let raw = raw.trim();
    if raw.eq_ignore_ascii_case("all") {
        return Ok(ResolvedFeedbackSince::All);
    }
    if raw.eq_ignore_ascii_case("last") {
        return Ok(ResolvedFeedbackSince::Last);
    }
    if let Ok(timestamp) = raw.parse::<i64>() {
        return Ok(ResolvedFeedbackSince::Timestamp(timestamp));
    }
    if let Some(timestamp) = parse_relative_since_timestamp(raw, now)? {
        return Ok(ResolvedFeedbackSince::Timestamp(timestamp));
    }

    let parsed = DateTime::parse_from_rfc3339(raw)
        .map(|value| value.with_timezone(&Utc).timestamp())
        .map_err(|error| {
            anyhow!(
                "Invalid feedback since value '{raw}'. Use 'all', 'last', relative durations like '1h', unix timestamp, or RFC3339 ({error})"
            )
        })?;
    Ok(ResolvedFeedbackSince::Timestamp(parsed))
}

fn parse_relative_since_timestamp(raw: &str, now: DateTime<Utc>) -> Result<Option<i64>> {
    let trimmed = raw.trim();
    let trimmed = trimmed
        .strip_suffix("ago")
        .map(str::trim_end)
        .unwrap_or(trimmed);
    let compact = trimmed
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .collect::<String>();
    if compact.is_empty() {
        return Ok(None);
    }

    let split_at = compact
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(compact.len());
    if split_at == 0 || split_at == compact.len() {
        return Ok(None);
    }

    let amount = compact[..split_at]
        .parse::<i64>()
        .map_err(|error| anyhow!("invalid relative duration amount in '{raw}': {error}"))?;
    let unit = compact[split_at..].to_ascii_lowercase();
    let scale = match unit.as_str() {
        "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 60 * 60,
        "d" | "day" | "days" => 60 * 60 * 24,
        "w" | "wk" | "wks" | "week" | "weeks" => 60 * 60 * 24 * 7,
        _ => return Ok(None),
    };

    let seconds = amount
        .checked_mul(scale)
        .ok_or_else(|| anyhow!("relative duration is too large in '{raw}'"))?;
    let timestamp = now
        .timestamp()
        .checked_sub(seconds)
        .ok_or_else(|| anyhow!("relative duration is too large in '{raw}'"))?;
    Ok(Some(timestamp))
}

#[cfg(test)]
mod tests {
    use super::{FeedbackSinceExpr, ResolvedFeedbackSince};
    use chrono::{TimeZone, Utc};

    #[test]
    fn feedback_since_expr_accepts_relative_hours() {
        let now = Utc.with_ymd_and_hms(2026, 4, 6, 12, 0, 0).unwrap();
        let parsed = FeedbackSinceExpr::new("1h")
            .unwrap_or_else(|error| panic!("relative duration should parse: {error}"))
            .resolve_at(now)
            .unwrap_or_else(|error| panic!("relative duration should resolve: {error}"));
        assert_eq!(
            parsed,
            ResolvedFeedbackSince::Timestamp(now.timestamp() - 60 * 60)
        );
    }

    #[test]
    fn feedback_since_expr_accepts_relative_days_with_ago_suffix() {
        let now = Utc.with_ymd_and_hms(2026, 4, 6, 12, 0, 0).unwrap();
        let parsed = FeedbackSinceExpr::new("2d ago")
            .unwrap_or_else(|error| panic!("relative duration should parse: {error}"))
            .resolve_at(now)
            .unwrap_or_else(|error| panic!("relative duration should resolve: {error}"));
        assert_eq!(
            parsed,
            ResolvedFeedbackSince::Timestamp(now.timestamp() - 2 * 60 * 60 * 24)
        );
    }

    #[test]
    fn feedback_since_expr_rejects_relative_duration_timestamp_underflow() {
        let now = Utc.timestamp_opt(-100, 0).unwrap();
        let duration = format!("{}s", i64::MAX);
        let error = FeedbackSinceExpr::new(&duration)
            .unwrap_or_else(|error| panic!("relative duration should parse: {error}"))
            .resolve_at(now)
            .unwrap_err();
        assert!(error.to_string().contains("relative duration is too large"));
    }

    #[test]
    fn feedback_since_expr_rejects_unknown_values() {
        let error = FeedbackSinceExpr::new("someday").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Invalid feedback since value 'someday'")
        );
    }

    #[test]
    fn feedback_since_expr_defaults_to_all() {
        assert_eq!(FeedbackSinceExpr::default().as_str(), "all");
    }
}
