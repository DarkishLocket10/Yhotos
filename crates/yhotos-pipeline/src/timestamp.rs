//! EXIF dates retain their wall clock; offsets override the configured IANA zone.
use crate::{JobError, protocol::Metadata};
use jiff::tz::TimeZone;
use time::{OffsetDateTime, PrimitiveDateTime, UtcOffset, macros::format_description};

pub(crate) fn capture_time(
    metadata: &Metadata,
    timezone: &TimeZone,
) -> Result<(Option<OffsetDateTime>, Option<PrimitiveDateTime>), JobError> {
    let bad = || JobError::Fatal("invalid capture timestamp".into());
    let Some(text) = metadata.taken_at.as_deref() else {
        return Ok((None, None));
    };
    // Retain compatibility with hosts that already report a complete instant.
    if let Ok(instant) = OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
    {
        return Ok((
            Some(instant),
            Some(PrimitiveDateTime::new(instant.date(), instant.time())),
        ));
    }
    let local = PrimitiveDateTime::parse(
        text,
        format_description!("[year]:[month]:[day] [hour]:[minute]:[second]"),
    )
    .map_err(|_| bad())?;
    let instant = if let Some(offset) = metadata.offset_time_original.as_deref() {
        let offset = UtcOffset::parse(
            offset,
            format_description!("[offset_hour sign:mandatory]:[offset_minute]"),
        )
        .map_err(|_| bad())?;
        if offset.whole_hours().abs() > 23 {
            return Err(bad());
        }
        local.assume_offset(offset)
    } else {
        let civil =
            jiff::civil::DateTime::strptime("%Y:%m:%d %H:%M:%S", text).map_err(|_| bad())?;
        // Compatible disambiguation: earlier instant for a fall-back fold;
        // advance by the gap for a spring-forward nonexistent wall clock.
        let timestamp = timezone
            .to_ambiguous_timestamp(civil)
            .compatible()
            .map_err(|_| bad())?;
        OffsetDateTime::from_unix_timestamp(timestamp.as_second()).map_err(|_| bad())?
    };
    Ok((Some(instant.to_offset(UtcOffset::UTC)), Some(local)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    fn metadata(date: &str, offset: Option<&str>) -> Metadata {
        serde_json::from_value(serde_json::json!({"taken_at":date,"offset_time_original":offset}))
            .unwrap()
    }
    fn utc(date: &str) -> OffsetDateTime {
        OffsetDateTime::parse(date, &time::format_description::well_known::Rfc3339).unwrap()
    }
    #[test]
    fn explicit_offsets_override_zone_and_preserve_wall_clock() {
        for (offset, expected) in [
            ("+05:30", "2026-01-01T21:34:05Z"),
            ("-04:00", "2026-01-02T07:04:05Z"),
        ] {
            let (instant, local) = capture_time(
                &metadata("2026:01:02 03:04:05", Some(offset)),
                &TimeZone::UTC,
            )
            .unwrap();
            assert_eq!(instant, Some(utc(expected)));
            assert_eq!(local.unwrap().to_string(), "2026-01-02 3:04:05.0");
        }
    }
    #[test]
    fn missing_offset_uses_zone_and_dst_policy() {
        let zone = TimeZone::get("America/Toronto").unwrap();
        for (wall, expected) in [
            ("2026:01:02 03:04:05", "2026-01-02T08:04:05Z"),
            ("2026:07:02 03:04:05", "2026-07-02T07:04:05Z"),
            ("2026:11:01 01:30:00", "2026-11-01T05:30:00Z"),
            ("2026:03:08 02:30:00", "2026-03-08T07:30:00Z"),
        ] {
            assert_eq!(
                capture_time(&metadata(wall, None), &zone).unwrap().0,
                Some(utc(expected))
            );
        }
        for (date, offset) in [
            ("2026:02:30 00:00:00", None),
            ("2026:01:02 03:04:05", Some("+25:00")),
            ("bad", None),
        ] {
            assert!(capture_time(&metadata(date, offset), &zone).is_err());
        }
    }
}
