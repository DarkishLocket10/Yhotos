//! One-round-trip calendar summaries and paginated photos, in UTC.
use crate::{DbError, PgConnection, photos::PagePhoto};
use std::fmt::Write;
use time::{Date, Month, OffsetDateTime};
use yhotos_core::access::Principal;

/// Valid calendar selection. Constructors reject impossible dates before SQL.
#[derive(Clone, Copy, Debug)]
pub struct Selection {
    year: Option<i32>,
    month: Option<u8>,
    day: Option<u8>,
}
impl Selection {
    /// Validates the route hierarchy and Gregorian date, supporting years 1..=9999.
    #[must_use]
    pub fn new(year: Option<i32>, month: Option<u8>, day: Option<u8>) -> Option<Self> {
        if year.is_none() && (month.is_some() || day.is_some()) || month.is_none() && day.is_some()
        {
            return None;
        }
        if let Some(year) = year {
            if !(1..=9999).contains(&year) {
                return None;
            }
            Date::from_calendar_date(
                year,
                Month::try_from(month.unwrap_or(1)).ok()?,
                day.unwrap_or(1),
            )
            .ok()?;
        }
        Some(Self { year, month, day })
    }
    /// Canonical same-origin path for pagination and breadcrumbs.
    #[must_use]
    pub fn path(self) -> String {
        let mut path = String::from("/calendar");
        if let Some(year) = self.year {
            let _ = write!(path, "/{year}");
        }
        if let Some(month) = self.month {
            let _ = write!(path, "/{month}");
        }
        if let Some(day) = self.day {
            let _ = write!(path, "/{day}");
        }
        path
    }
    /// Whether this selection already represents a single day.
    #[must_use]
    pub fn is_day(self) -> bool {
        self.day.is_some()
    }
    /// Human-readable title from validated numbers only.
    #[must_use]
    pub fn title(self) -> String {
        match (self.year, self.month, self.day) {
            (Some(y), Some(m), Some(d)) => format!("Photos from {y:04}-{m:02}-{d:02}"),
            (Some(y), Some(m), _) => format!("Photos from {y:04}-{m:02}"),
            (Some(y), _, _) => format!("Photos from {y:04}"),
            _ => "Photo calendar".into(),
        }
    }
    fn range(self) -> Result<(Option<OffsetDateTime>, Option<OffsetDateTime>), DbError> {
        let Some(y) = self.year else {
            return Ok((None, None));
        };
        let date = Date::from_calendar_date(
            y,
            Month::try_from(self.month.unwrap_or(1)).map_err(|_| DbError::InvalidInput)?,
            self.day.unwrap_or(1),
        )
        .map_err(|_| DbError::InvalidInput)?;
        let end = if self.day.is_some() {
            date.next_day()
        } else if let Some(m) = self.month {
            Date::from_calendar_date(
                y + i32::from(m == 12),
                Month::try_from(if m == 12 { 1 } else { m + 1 })
                    .map_err(|_| DbError::InvalidInput)?,
                1,
            )
            .ok()
        } else {
            Date::from_calendar_date(y + 1, Month::January, 1).ok()
        };
        Ok((
            Some(date.midnight().assume_utc()),
            end.map(|d| d.midnight().assume_utc()),
        ))
    }
}
/// A nonempty year, month or day, and its visible cover.
#[derive(Debug, serde::Deserialize)]
pub struct Bucket {
    /// Numeric year, month or day for this level.
    pub number: i32,
    /// Visible count, computed before pagination.
    pub count: u64,
    /// Number whose capture date is missing.
    pub posted_count: u64,
    /// Deterministic visible cover, with derivatives.
    pub cover: PagePhoto,
}
/// A page photo explicitly marks the posted-date fallback.
#[derive(Debug, serde::Deserialize)]
pub struct CalendarPhoto {
    /// Visible photo and responsive derivatives.
    pub photo: PagePhoto,
    /// True only when `taken_at` is absent.
    pub posted: bool,
}
/// All calendar content is obtained by one statement, including empty-page counts.
#[derive(Debug, serde::Deserialize)]
pub struct CalendarPage {
    /// Visible time buckets, descending by date.
    pub buckets: Vec<Bucket>,
    /// Month/day photo grid, at most 60 rows.
    pub photos: Vec<CalendarPhoto>,
    /// Full visible count within the selection.
    pub total: u64,
}
/// Reads a summary and optional month/day grid in exactly one SQL round trip.
///
/// # Errors
/// Rejects page zero, unavailable pages, invalid stored metadata or DB failures.
pub async fn page(
    conn: &mut PgConnection,
    principal: &Principal<'_>,
    selection: Selection,
    number: u32,
) -> Result<Option<CalendarPage>, DbError> {
    if number == 0 || selection.month.is_none() && number != 1 {
        return Ok(None);
    }
    let (start, end) = selection.range()?;
    let part = if selection.year.is_none() {
        "year"
    } else if selection.month.is_none() {
        "month"
    } else {
        "day"
    };
    let grid = selection.month.is_some();
    let offset = i64::from(number - 1) * 60;
    let p = crate::access_sql::visible_photos(principal);
    let row=crate::visible_page_query!(r#",
      selected AS MATERIALIZED (
        SELECT p.*,coalesce(taken_at,posted_at) AS date,
          extract(year FROM coalesce(taken_at,posted_at) AT TIME ZONE 'UTC')::int AS year,
          extract(month FROM coalesce(taken_at,posted_at) AT TIME ZONE 'UTC')::int AS month,
          extract(day FROM coalesce(taken_at,posted_at) AT TIME ZONE 'UTC')::int AS day
        FROM visible_photos p WHERE ($6::timestamptz IS NULL OR coalesce(taken_at,posted_at)>=$6)
          AND ($7::timestamptz IS NULL OR coalesce(taken_at,posted_at)<$7)
      ), grouped AS (
        SELECT CASE $8 WHEN 'year' THEN year WHEN 'month' THEN month ELSE day END AS number,
          count(*) AS count,count(*) FILTER(WHERE taken_at IS NULL) AS posted_count,
          (array_agg(id ORDER BY date DESC,id))[1] AS cover_id
        FROM selected GROUP BY 1
      ), wanted AS (
        SELECT id FROM grouped g JOIN selected s ON s.id=g.cover_id
        UNION SELECT id FROM (SELECT id FROM selected WHERE $9::bool ORDER BY date DESC,id LIMIT 60 OFFSET $10) t
      ), decorated AS (
        SELECT s.id,jsonb_build_object('photo',to_jsonb(s)||jsonb_build_object('thumbhash',encode(s.placeholder,'base64'),
          'derivatives',coalesce((SELECT jsonb_agg(jsonb_build_object('variant',d.variant,'format',d.format,'hdr',d.hdr,'hash',d.hash,'w',d.width,'h',d.height) ORDER BY d.width,d.format,d.hdr)
            FROM derivatives d WHERE d.photo_id=s.id),'[]'::jsonb)), 'posted',s.taken_at IS NULL) AS value
        FROM selected s JOIN wanted w ON w.id=s.id
      ) SELECT jsonb_build_object(
        'buckets',coalesce((SELECT jsonb_agg(jsonb_build_object('number',g.number,'count',g.count,'posted_count',g.posted_count,'cover',d.value->'photo') ORDER BY g.number DESC) FROM grouped g JOIN decorated d ON d.id=g.cover_id),'[]'::jsonb),
        'photos',coalesce((SELECT jsonb_agg(d.value ORDER BY t.date DESC,t.id) FROM (SELECT id,date FROM selected WHERE $9::bool ORDER BY date DESC,id LIMIT 60 OFFSET $10) t JOIN decorated d ON d.id=t.id),'[]'::jsonb),
        'total',(SELECT count(*) FROM selected)) AS "payload!"
    "#,p,start,end,part,grid,offset).fetch_one(conn).await?;
    let page: CalendarPage =
        serde_json::from_value(row.payload).map_err(|_| DbError::InvalidInput)?;
    if number > 1 && offset >= i64::try_from(page.total).map_err(|_| DbError::InvalidInput)?
        || selection.year.is_some() && page.total == 0
    {
        return Ok(None);
    }
    Ok(Some(page))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_gregorian_dates_and_hierarchy() {
        assert!(Selection::new(Some(2024), Some(2), Some(29)).is_some());
        for (y, m, d) in [
            (Some(2025), Some(2), Some(29)),
            (Some(0), None, None),
            (Some(2025), Some(13), None),
            (None, Some(1), None),
            (Some(2025), None, Some(1)),
        ] {
            assert!(Selection::new(y, m, d).is_none());
        }
    }
}
