//! Distinct `UUIDv7` identifiers prevent mixing entity types.

use std::{fmt, str::FromStr};
use uuid::Uuid;

// Always use the string representation, including non-human-readable formats.
mod uuid_string {
    use serde::{Deserialize, Deserializer, Serializer};
    use uuid::Uuid;

    pub fn serialize<S: Serializer>(value: &Uuid, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&value.hyphenated())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Uuid, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

macro_rules! ids {
    ($($name:ident),+ $(,)?) => {$ (
        #[doc = concat!("A typed, time-ordered identifier for `", stringify!($name), "` rows.")]
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        #[cfg_attr(feature = "sqlx", derive(sqlx::Type))]
        #[cfg_attr(feature = "sqlx", sqlx(transparent))]
        #[repr(transparent)]
        pub struct $name(#[serde(with = "uuid_string")] pub Uuid);

        impl $name {
            /// Generates a fresh `UUIDv7` identifier.
            #[must_use]
            pub fn new() -> Self { Self(Uuid::now_v7()) }
        }
        impl Default for $name {
            fn default() -> Self { Self::new() }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.hyphenated().fmt(f)
            }
        }
        impl FromStr for $name {
            type Err = uuid::Error;
            fn from_str(s: &str) -> Result<Self, Self::Err> { s.parse().map(Self) }
        }
    )+};
}
ids!(
    PhotoId,
    AlbumId,
    UserId,
    GroupId,
    ShareLinkId,
    TagId,
    JobId,
    SessionId
);

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn ids_are_v7_and_round_trip_as_hyphenated_strings() {
        macro_rules! check { ($($ty:ident),+) => {$ (
            let id = $ty::new();
            assert_eq!(id.0.get_version_num(), 7);
            assert_eq!(id.to_string().len(), 36);
            assert_eq!(id.to_string().parse::<$ty>().unwrap(), id);
            let json = serde_json::to_string(&id).unwrap();
            assert_eq!(json, format!("\"{id}\""));
            assert_eq!(serde_json::from_str::<$ty>(&json).unwrap(), id);
            assert!("invalid".parse::<$ty>().is_err());
        )+}; }
        check!(
            PhotoId,
            AlbumId,
            UserId,
            GroupId,
            ShareLinkId,
            TagId,
            JobId,
            SessionId
        );
    }

    #[cfg(feature = "sqlx")]
    #[test]
    fn ids_have_the_uuid_sql_type() {
        use sqlx::{Postgres, Type};
        assert_eq!(
            <PhotoId as Type<Postgres>>::type_info(),
            <Uuid as Type<Postgres>>::type_info()
        );
    }
}
