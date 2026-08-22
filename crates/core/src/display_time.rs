//! Stable calendar and minute-precision display formatting.

use std::time::SystemTime;

use jiff::{Timestamp, fmt::strtime, tz::TimeZone};
use thiserror::Error;

/// Failure to represent or format a wall-clock time.
#[derive(Debug, Error)]
pub enum DisplayTimeError {
	/// The timestamp or static display format could not be represented.
	#[error(transparent)]
	Jiff(#[from] jiff::Error),
}

/// Formats `time` as `YYYY-MM-DD` in the host's local timezone.
pub fn local_calendar_date(time: SystemTime) -> Result<String, DisplayTimeError> {
	format_in_zone(time, TimeZone::system(), "%Y-%m-%d")
}

/// Formats `time` as `YYYY-MM-DD HH:MM ±HH:MM` in the host's local timezone.
pub fn local_minute_with_offset(time: SystemTime) -> Result<String, DisplayTimeError> {
	format_in_zone(time, TimeZone::system(), "%Y-%m-%d %H:%M %:z")
}

/// Formats `time` as deterministic UTC `YYYY-MM-DD HH:MM`.
pub fn utc_minute(time: SystemTime) -> Result<String, DisplayTimeError> {
	format_in_zone(time, TimeZone::UTC, "%Y-%m-%d %H:%M")
}

fn format_in_zone(
	time: SystemTime,
	zone: TimeZone,
	format: &'static str,
) -> Result<String, DisplayTimeError> {
	let timestamp = Timestamp::try_from(time)?;
	Ok(strtime::format(format, &timestamp.to_zoned(zone))?)
}

#[cfg(test)]
mod tests {
	use std::time::{Duration, UNIX_EPOCH};

	use super::*;

	#[test]
	fn utc_minute_is_stable_and_minute_precision() {
		let time = UNIX_EPOCH + Duration::from_secs(1_735_689_599);
		assert_eq!(utc_minute(time).unwrap(), "2024-12-31 23:59");
	}

	#[test]
	fn local_formats_have_calendar_and_numeric_offset_shapes() {
		let time = UNIX_EPOCH + Duration::from_secs(1_735_689_599);
		let date = local_calendar_date(time).unwrap();
		let minute = local_minute_with_offset(time).unwrap();
		assert_eq!(date.len(), 10);
		assert_eq!(&minute[..10], date);
		assert_eq!(minute.len(), 23);
		assert!(matches!(minute.as_bytes()[17], b'+' | b'-'));
		assert_eq!(minute.as_bytes()[20], b':');
	}
}
