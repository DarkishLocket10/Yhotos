//! Bounded public search grammar. Unknown filters remain ordinary searchable text.
/// Canonical structured restriction; only the parser constructs public query filters.
#[derive(Clone, Debug)]
pub struct Filter {
    /// Recognized filter key.
    pub key: String,
    /// Validated canonical value.
    pub value: String,
}

/// A validated query contains at most sixteen terms and eight structured filters.
#[derive(Clone, Debug, Default)]
pub struct Query {
    /// Normalized free text, suitable for embedding and bound full-text queries.
    pub text: String,
    /// AND-combined structured restrictions.
    pub filters: Vec<Filter>,
}

/// Invalid syntax or a bound exceeded; supplied text is never included in errors.
#[derive(Debug)]
pub struct InvalidQuery;

/// Parses quotes and recognized filters without interpreting SQL or person identities.
///
/// # Errors
/// Rejects controls, unclosed quotes, invalid filter values, and oversized queries.
pub fn parse(input: &str) -> Result<Query, InvalidQuery> {
    if input.chars().count() > 256 || input.chars().any(char::is_control) {
        return Err(InvalidQuery);
    }
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quoted = false;
    for c in input.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !token.is_empty() {
                    tokens.push(std::mem::take(&mut token));
                }
            }
            _ => token.push(c),
        }
    }
    if quoted {
        return Err(InvalidQuery);
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    if tokens.len() > 16 {
        return Err(InvalidQuery);
    }
    let mut query = Query::default();
    let mut terms = Vec::new();
    for token in tokens {
        if let Some((key, value)) = token.split_once(':') {
            let value = value.trim().to_lowercase();
            if matches!(
                key,
                "camera"
                    | "lens"
                    | "tag"
                    | "place"
                    | "year"
                    | "month"
                    | "color"
                    | "is"
                    | "before"
                    | "after"
            ) {
                if value.is_empty() || query.filters.len() == 8 {
                    return Err(InvalidQuery);
                }
                let valid = match key {
                    "year" => value.parse::<u16>().is_ok_and(|v| (1..=9999).contains(&v)),
                    "month" => value.parse::<u8>().is_ok_and(|v| (1..=12).contains(&v)),
                    "is" => matches!(value.as_str(), "raw" | "hdr" | "video"),
                    "before" | "after" => valid_date(&value),
                    "color" => color(&value).is_some(),
                    _ => true,
                };
                if !valid {
                    return Err(InvalidQuery);
                }
                let value = if key == "color" {
                    color(&value).ok_or(InvalidQuery)?.to_string()
                } else {
                    value
                };
                query.filters.push(Filter {
                    key: key.into(),
                    value,
                });
                continue;
            }
        }
        if terms.len() == 16 {
            return Err(InvalidQuery);
        }
        terms.push(token.to_lowercase());
    }
    query.text = terms.join(" ");
    if query.text.chars().count() > 256 {
        return Err(InvalidQuery);
    }
    Ok(query)
}
fn valid_date(value: &str) -> bool {
    let parts: Vec<_> = value.split('-').collect();
    let [year, month, day] = parts.as_slice() else {
        return false;
    };
    if ![year, month, day]
        .iter()
        .all(|v| v.bytes().all(|b| b.is_ascii_digit()))
        || year.len() != 4
        || month.len() != 2
        || day.len() != 2
    {
        return false;
    }
    let (Ok(year), Ok(month), Ok(day)) =
        (year.parse::<i32>(), month.parse::<u8>(), day.parse::<u8>())
    else {
        return false;
    };
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    };
    (1..=9999).contains(&year) && day > 0 && day <= days
}
fn color(value: &str) -> Option<u32> {
    Some(match value {
        "red" => 0x00ff_0000,
        "green" => 0x0000_8000,
        "blue" => 0x0000_00ff,
        "black" => 0,
        "white" => 0x00ff_ffff,
        "gray" | "grey" => 0x0080_8080,
        "yellow" => 0x00ff_ff00,
        "orange" => 0x00ff_a500,
        "purple" => 0x0080_0080,
        "pink" => 0x00ff_c0cb,
        "brown" => 0x00a5_2a2a,
        _ => {
            let hex = value.strip_prefix('#').unwrap_or(value);
            if hex.len() != 6 || !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            u32::from_str_radix(hex, 16).ok()?
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn grammar_and_public_person_boundary() {
        for (input, text, filters) in [
            ("Sunset beach", "sunset beach", 0),
            (
                "\"golden hour\" camera:\"Sony A7\" year:2025",
                "golden hour",
                2,
            ),
            (
                "tag:beach month:2 is:raw before:2026-01-01 color:#FF0088",
                "",
                5,
            ),
            ("person:Yash unknown:value", "person:yash unknown:value", 0),
            ("", "", 0),
        ] {
            let q = parse(input).unwrap();
            assert_eq!(q.text, text);
            assert_eq!(q.filters.len(), filters);
        }
        for invalid in [
            "year:no",
            "month:13",
            "before:2025-02-29",
            "is:photo",
            "color:zzzzzz",
            "tag:",
            "\"unclosed",
            "bad\0text",
        ] {
            assert!(parse(invalid).is_err(), "{invalid}");
        }
        assert!(parse(&"a".repeat(257)).is_err());
        assert!(parse(&"x ".repeat(17)).is_err());
        assert!(parse(&"tag:x ".repeat(9)).is_err());
        assert!(parse(&"é".repeat(256)).is_ok());
    }
}
