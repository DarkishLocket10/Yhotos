//! Canonical ASCII URL slugs, with bounded title transliteration.

use std::fmt;

/// A nonempty ASCII slug of at most eighty bytes with single interior hyphens.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Slug(String);

/// Explains why a string cannot be used as a canonical slug.
#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum SlugError {
    /// Slugs must have one through eighty characters.
    #[error("slug must contain one to eighty characters")]
    Length,
    /// Only lowercase ASCII letters, digits, and hyphens are accepted.
    #[error("slug contains an invalid character")]
    Character,
    /// Hyphens may neither repeat nor appear at either end.
    #[error("slug has a leading, trailing, or repeated hyphen")]
    Hyphen,
}
impl Slug {
    /// Validates an already canonical spelling without silently normalizing it.
    ///
    /// # Errors
    /// Returns the first invalid length, character, or hyphen rule.
    pub fn new(value: &str) -> Result<Self, SlugError> {
        if value.is_empty() || value.len() > 80 {
            return Err(SlugError::Length);
        }
        if !value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(SlugError::Character);
        }
        if value.starts_with('-') || value.ends_with('-') || value.contains("--") {
            return Err(SlugError::Hyphen);
        }
        Ok(Self(value.to_owned()))
    }
    /// Transliterates a title and keeps whole words within eighty characters.
    /// A single oversized word is clipped; an empty result becomes `untitled`.
    #[must_use]
    pub fn from_title(title: &str) -> Self {
        let ascii = deunicode::deunicode(title).to_ascii_lowercase();
        let mut result = String::new();
        for word in ascii
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| !w.is_empty())
        {
            if result.is_empty() {
                result.extend(word.chars().take(80));
            } else if result.len() + 1 + word.len() <= 80 {
                result.push('-');
                result.push_str(word);
            } else {
                break;
            }
        }
        if result.is_empty() {
            result.push_str("untitled");
        }
        Self(result)
    }
    /// Borrows the canonical ASCII spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for Slug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(feature = "sqlx")]
mod postgres {
    use super::Slug;
    use sqlx::{
        Decode, Encode, Postgres, Type,
        encode::IsNull,
        error::BoxDynError,
        postgres::{PgArgumentBuffer, PgTypeInfo, PgValueRef},
    };

    impl Type<Postgres> for Slug {
        fn type_info() -> PgTypeInfo {
            PgTypeInfo::with_name("citext")
        }
        fn compatible(ty: &PgTypeInfo) -> bool {
            <String as Type<Postgres>>::compatible(ty) || *ty == Self::type_info()
        }
    }
    impl Encode<'_, Postgres> for Slug {
        fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
            <&str as Encode<Postgres>>::encode(self.as_str(), buf)
        }
    }
    impl Decode<'_, Postgres> for Slug {
        fn decode(value: PgValueRef<'_>) -> Result<Self, BoxDynError> {
            let text = <&str as Decode<Postgres>>::decode(value)?;
            Ok(Self::new(text)?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejection_classes() {
        for value in [String::new(), "a".repeat(81)] {
            assert_eq!(Slug::new(&value), Err(SlugError::Length));
        }
        for value in ["A", "café", "a_b", "a b", "a/b", "a.b", "a\nb"] {
            assert_eq!(Slug::new(value), Err(SlugError::Character));
        }
        for value in ["-a", "a-", "a--b", "-"] {
            assert_eq!(Slug::new(value), Err(SlugError::Hyphen));
        }
        for value in ["a", "0", "a-b2", &"a".repeat(80)] {
            assert!(Slug::new(value).is_ok());
        }
    }
    #[test]
    fn title_normalization_and_word_boundaries() {
        assert_eq!(
            Slug::from_title("  Café / déjà... vu! 2026 ").as_str(),
            "cafe-deja-vu-2026"
        );
        assert_eq!(Slug::from_title("").as_str(), "untitled");
        assert_eq!(Slug::from_title("/// ---").as_str(), "untitled");
        assert_eq!(
            Slug::from_title(&format!("{} word", "a".repeat(77))).as_str(),
            "a".repeat(77)
        );
        assert_eq!(Slug::from_title(&"a".repeat(100)).as_str(), "a".repeat(80));
    }
    proptest::proptest! {
        #[test]
        fn every_title_produces_a_valid_slug(title in ".*") {
            proptest::prop_assert!(Slug::new(Slug::from_title(&title).as_str()).is_ok());
        }
    }
}
