//! Typed repository operations; authorization belongs to the caller.

use crate::DbError;
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;
use yhotos_core::{GroupId, UserId};

/// A group identity and name, independent of caller authorization.
#[derive(Clone, Debug)]
pub struct GroupRow {
    /// Stored id.
    pub id: GroupId,
    /// Stored created at.
    pub created_at: OffsetDateTime,
    /// Stored updated at.
    pub updated_at: OffsetDateTime,
    /// Stored name.
    pub name: String,
}

/// Creates a uniquely named group with an application-generated `UUIDv7`.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn create(conn: &mut PgConnection, name: &str) -> Result<GroupRow, DbError> {
    Ok(sqlx::query_as!(GroupRow,r#"INSERT INTO groups(id,name) VALUES ($1,$2) RETURNING id as "id: _", created_at, updated_at, name"#, GroupId::new() as _, name).fetch_one(conn).await?)
}

/// Renames a group without changing memberships or grants.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn rename(conn: &mut PgConnection, id: GroupId, name: &str) -> Result<u64, DbError> {
    Ok(
        sqlx::query!(r#"UPDATE groups SET name=$2 WHERE id=$1"#, id as _, name)
            .execute(conn)
            .await?
            .rows_affected(),
    )
}

/// Adds a verified user to a group idempotently.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn add_member(
    conn: &mut PgConnection,
    group: GroupId,
    user: UserId,
) -> Result<u64, DbError> {
    Ok(sqlx::query!(r#"INSERT INTO user_groups(id,group_id,user_id) VALUES ($1,$2,$3) ON CONFLICT (user_id,group_id) DO NOTHING"#, Uuid::now_v7(), group as _, user as _).execute(conn).await?.rows_affected())
}

/// Removes a user membership without deleting the user or group.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn remove_member(
    conn: &mut PgConnection,
    group: GroupId,
    user: UserId,
) -> Result<u64, DbError> {
    Ok(sqlx::query!(
        r#"DELETE FROM user_groups WHERE group_id=$1 AND user_id=$2"#,
        group as _,
        user as _
    )
    .execute(conn)
    .await?
    .rows_affected())
}

/// Returns verified member identifiers for this group.
///
/// # Errors
/// Returns database failures.
pub async fn members(conn: &mut PgConnection, group: GroupId) -> Result<Vec<UserId>, DbError> {
    Ok(sqlx::query_scalar!(r#"SELECT user_id AS "user_id: UserId" FROM user_groups WHERE group_id=$1 ORDER BY user_id"#, group as _).fetch_all(conn).await?)
}

/// Fetches all groups of the verified user in stable identifier order.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn groups_of(conn: &mut PgConnection, user: UserId) -> Result<Vec<GroupRow>, DbError> {
    Ok(sqlx::query_as!(GroupRow,r#"SELECT g.id as "id: _", g.created_at, g.updated_at, g.name FROM groups g JOIN user_groups ug ON ug.group_id=g.id WHERE ug.user_id=$1 ORDER BY g.id"#, user as _).fetch_all(conn).await?)
}
