#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::too_many_lines)]
//! Database role and migration boundaries, tested against isolated Postgres databases.
use secrecy::SecretString;
use sqlx::{ConnectOptions, PgPool};
use uuid::Uuid;
use yhotos_db::{DatabaseTls, Db, DbError, MIGRATOR, Role, users};

#[sqlx::test(migrations = false)]
async fn migrations_from_empty_are_idempotent(pool: PgPool) {
    MIGRATOR.run(&pool).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let row = sqlx::query!(r#"SELECT count(*) AS "count!" FROM _sqlx_migrations WHERE success"#)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(usize::try_from(row.count).unwrap(), MIGRATOR.iter().count());
    let extensions=sqlx::query!(r#"SELECT count(*) AS "count!" FROM pg_extension WHERE extname IN ('pgcrypto','citext','vector')"#).fetch_one(&pool).await.unwrap();
    assert_eq!(extensions.count, 3);
}

#[sqlx::test]
async fn connection_settings_and_admin_only_migration(pool: PgPool) {
    let url: SecretString = pool.connect_options().to_url_lossy().to_string().into();
    for (role, timeout, name) in [
        (Role::Public, "5s", "yhotos_public"),
        (Role::Admin, "30s", "yhotos_admin"),
        (Role::Worker, "1min", "yhotos_worker"),
    ] {
        let db = Db::connect(&url, role, 2, DatabaseTls::Disable)
            .await
            .unwrap();
        // Acquire two simultaneously so after_connect is tested on more than one connection.
        let mut one = db.pool().acquire().await.unwrap();
        let mut two = db.pool().acquire().await.unwrap();
        for conn in [&mut one, &mut two] {
            let row=sqlx::query!(r#"SELECT current_setting('statement_timeout') AS "timeout!",current_setting('application_name') AS "app!",current_setting('idle_in_transaction_session_timeout') AS "idle!",current_user::text AS "role!""#).fetch_one(&mut **conn).await.unwrap();
            assert_eq!(row.timeout, timeout);
            assert_eq!(row.app, name);
            assert_eq!(row.idle, "30s");
            if role != Role::Admin {
                assert_eq!(row.role, name);
            }
        }
        drop(one);
        drop(two);
        if role == Role::Admin {
            db.migrate().await.unwrap();
        } else {
            assert!(matches!(db.migrate().await, Err(DbError::AdminRequired)));
        }
        db.pool().close().await;
    }
    assert!(
        Db::connect(&url, Role::Admin, 0, DatabaseTls::Disable)
            .await
            .is_err()
    );
    let invalid: SecretString = Uuid::now_v7().to_string().into();
    let Err(err) = Db::connect(&invalid, Role::Public, 1, DatabaseTls::default()).await else {
        unreachable!()
    };
    assert!(
        !err.to_string()
            .contains(secrecy::ExposeSecret::expose_secret(&invalid))
    );
}

#[tokio::test]
async fn disabling_tls_for_remote_hosts_is_rejected_before_dialing() {
    for host in ["192.0.2.1", "[2001:db8::1]", "db.example.invalid"] {
        let url: SecretString = format!("postgres://{host}/gallery?sslmode=disable").into();
        assert!(matches!(
            Db::connect(&url, Role::Admin, 1, DatabaseTls::Disable).await,
            Err(DbError::InsecureTransport)
        ));
    }
}

#[sqlx::test]
async fn required_tls_rejects_plain_postgres_and_loopback_disable_succeeds(pool: PgPool) {
    // The dev/CI service intentionally has no TLS, so Require must fail rather than fall back.
    let ssl = sqlx::query_scalar!("SHOW ssl")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(ssl.as_deref(), Some("off"));
    let url: SecretString = pool.connect_options().to_url_lossy().to_string().into();
    assert!(matches!(
        Db::connect(&url, Role::Admin, 1, DatabaseTls::default()).await,
        Err(DbError::Sqlx(sqlx::Error::Tls(_)))
    ));
    let local = Db::connect(&url, Role::Admin, 1, DatabaseTls::Disable)
        .await
        .unwrap();
    let mut conn = local.pool().acquire().await.unwrap();
    assert_eq!(
        sqlx::query_scalar!("SELECT 1 AS \"value!\"")
            .fetch_one(&mut *conn)
            .await
            .unwrap(),
        1
    );
    drop(conn);
    local.pool().close().await;
}

#[sqlx::test]
async fn grant_matrix_catalog_and_forbidden_statements(pool: PgPool) {
    let rows = sqlx::query!(
        r#"SELECT r.rolname::text AS "role!",c.relname::text AS "table!",
        has_table_privilege(r.oid,c.oid,'SELECT') AS "select!",
        has_table_privilege(r.oid,c.oid,'INSERT') AS "insert!",
        has_table_privilege(r.oid,c.oid,'UPDATE') AS "update!",
        has_table_privilege(r.oid,c.oid,'DELETE') AS "delete!"
        FROM pg_roles r CROSS JOIN pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
        WHERE r.rolname IN ('yhotos_public','yhotos_admin','yhotos_worker')
          AND n.nspname='public' AND c.relkind='r' AND c.relname<>'_sqlx_migrations'"#
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    // Includes both share-session memberships and the offline gazetteer.
    assert_eq!(rows.len(), 33 * 3);
    for row in rows {
        let actual = (row.select, row.insert, row.update, row.delete);
        let expected = match row.role.as_str() {
            "yhotos_admin" => (true, true, true, true),
            "yhotos_worker" if row.table == "gazetteer" => (true, false, false, false),
            "yhotos_worker" => {
                let allowed = ![
                    "users",
                    "user_mfa",
                    "settings",
                    "audit_log",
                    "pending_login",
                    "pending_webauthn",
                    "auth_lockout",
                    "share_session_links",
                ]
                .contains(&row.table.as_str());
                (allowed, allowed, allowed, allowed)
            }
            "yhotos_public"
                if [
                    "pending_login",
                    "pending_webauthn",
                    "auth_lockout",
                    "share_session_links",
                ]
                .contains(&row.table.as_str()) =>
            {
                (true, true, true, true)
            }
            "yhotos_public" => (
                !["user_mfa", "audit_log", "gazetteer"].contains(&row.table.as_str()),
                [
                    "events",
                    "likes",
                    "flags",
                    "share_link_opens",
                    "sessions",
                    "audit_log",
                ]
                .contains(&row.table.as_str()),
                false,
                ["sessions", "likes"].contains(&row.table.as_str()),
            ),
            _ => unreachable!(),
        };
        assert_eq!(actual, expected, "{} {}", row.role, row.table);
    }
    let columns=sqlx::query!(r#"SELECT a.attname::text AS "column!",has_column_privilege('yhotos_public','sessions',a.attname,'UPDATE') AS "allowed!" FROM pg_attribute a WHERE a.attrelid='sessions'::regclass AND a.attnum>0 AND NOT a.attisdropped"#).fetch_all(&pool).await.unwrap();
    for column in columns {
        assert_eq!(
            column.allowed,
            ["last_seen_at", "reauth_at"].contains(&column.column.as_str())
        );
    }
    let roles=sqlx::query!("SELECT rolcanlogin FROM pg_roles WHERE rolname IN ('yhotos_public','yhotos_admin','yhotos_worker')").fetch_all(&pool).await.unwrap();
    assert!(roles.iter().all(|r| r.rolcanlogin == Some(false)));
    let mut conn = pool.acquire().await.unwrap();
    let hash: SecretString = Uuid::now_v7().to_string().into();
    let user = users::create(&mut conn, "viewer", &hash, true)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query!("SET LOCAL ROLE yhotos_public")
        .execute(&mut *tx)
        .await
        .unwrap();
    {
        use sqlx::Acquire;
        let mut savepoint = tx.begin().await.unwrap();
        let err = sqlx::query!(
            "INSERT INTO users(id,username,password_hash) VALUES ($1,'forbidden',$2)",
            Uuid::now_v7(),
            secrecy::ExposeSecret::expose_secret(&hash)
        )
        .execute(&mut *savepoint)
        .await
        .unwrap_err();
        assert_eq!(
            err.as_database_error().unwrap().code().as_deref(),
            Some("42501")
        );
        savepoint.rollback().await.unwrap();
    }
    {
        use sqlx::Acquire;
        let mut savepoint = tx.begin().await.unwrap();
        let err = sqlx::query!("UPDATE users SET is_admin=true WHERE false")
            .execute(&mut *savepoint)
            .await
            .unwrap_err();
        assert_eq!(
            err.as_database_error().unwrap().code().as_deref(),
            Some("42501")
        );
        savepoint.rollback().await.unwrap();
    }
    {
        use sqlx::Acquire;
        let mut savepoint = tx.begin().await.unwrap();
        let err = sqlx::query!("DELETE FROM users WHERE false")
            .execute(&mut *savepoint)
            .await
            .unwrap_err();
        assert_eq!(
            err.as_database_error().unwrap().code().as_deref(),
            Some("42501")
        );
        savepoint.rollback().await.unwrap();
    }
    {
        use sqlx::Acquire;
        let mut savepoint = tx.begin().await.unwrap();
        let err = sqlx::query!("SELECT secret_enc FROM user_mfa WHERE false")
            .fetch_all(&mut *savepoint)
            .await
            .unwrap_err();
        assert_eq!(
            err.as_database_error().unwrap().code().as_deref(),
            Some("42501")
        );
        savepoint.rollback().await.unwrap();
    }
    {
        use sqlx::Acquire;
        let mut savepoint = tx.begin().await.unwrap();
        let err = sqlx::query!("SELECT detail FROM audit_log WHERE false")
            .fetch_all(&mut *savepoint)
            .await
            .unwrap_err();
        assert_eq!(
            err.as_database_error().unwrap().code().as_deref(),
            Some("42501")
        );
        savepoint.rollback().await.unwrap();
    }
    {
        use sqlx::Acquire;
        let mut savepoint = tx.begin().await.unwrap();
        let err = sqlx::query!("UPDATE settings SET value='{}' WHERE false")
            .execute(&mut *savepoint)
            .await
            .unwrap_err();
        assert_eq!(
            err.as_database_error().unwrap().code().as_deref(),
            Some("42501")
        );
        savepoint.rollback().await.unwrap();
    }
    {
        use sqlx::Acquire;
        let mut savepoint = tx.begin().await.unwrap();
        let err = sqlx::query!(
            "INSERT INTO jobs(id,kind,payload) VALUES ($1,'forbidden','{}')",
            Uuid::now_v7()
        )
        .execute(&mut *savepoint)
        .await
        .unwrap_err();
        assert_eq!(
            err.as_database_error().unwrap().code().as_deref(),
            Some("42501")
        );
        savepoint.rollback().await.unwrap();
    }
    {
        use sqlx::Acquire;
        let mut savepoint = tx.begin().await.unwrap();
        let err = sqlx::query!("UPDATE sessions SET user_id=$1 WHERE false", Uuid::now_v7())
            .execute(&mut *savepoint)
            .await
            .unwrap_err();
        assert_eq!(
            err.as_database_error().unwrap().code().as_deref(),
            Some("42501")
        );
        savepoint.rollback().await.unwrap();
    }
    {
        use sqlx::Acquire;
        let mut savepoint = tx.begin().await.unwrap();
        let err = sqlx::query!(
            "UPDATE sessions SET token_hash=$1::bytea WHERE false",
            secrecy::ExposeSecret::expose_secret(&hash).as_bytes()
        )
        .execute(&mut *savepoint)
        .await
        .unwrap_err();
        assert_eq!(
            err.as_database_error().unwrap().code().as_deref(),
            Some("42501")
        );
        savepoint.rollback().await.unwrap();
    }
    {
        use sqlx::Acquire;
        let mut savepoint = tx.begin().await.unwrap();
        let err =
            sqlx::query!("UPDATE flags SET resolution='dismissed',resolved_at=now() WHERE false")
                .execute(&mut *savepoint)
                .await
                .unwrap_err();
        assert_eq!(
            err.as_database_error().unwrap().code().as_deref(),
            Some("42501")
        );
        savepoint.rollback().await.unwrap();
    }
    {
        use sqlx::Acquire;
        let mut savepoint = tx.begin().await.unwrap();
        let err = sqlx::query!("DELETE FROM photos WHERE false")
            .execute(&mut *savepoint)
            .await
            .unwrap_err();
        assert_eq!(
            err.as_database_error().unwrap().code().as_deref(),
            Some("42501")
        );
        savepoint.rollback().await.unwrap();
    }
    {
        use sqlx::Acquire;
        let mut savepoint = tx.begin().await.unwrap();
        let err = sqlx::query!("CREATE TABLE public.forbidden_table(id uuid)")
            .execute(&mut *savepoint)
            .await
            .unwrap_err();
        assert_eq!(
            err.as_database_error().unwrap().code().as_deref(),
            Some("42501")
        );
        savepoint.rollback().await.unwrap();
    }
    users::set_password(&mut tx, user.id, &hash).await.unwrap();
    users::touch_login(&mut tx, user.id).await.unwrap();
    sqlx::query!("UPDATE sessions SET last_seen_at=now(),reauth_at=now() WHERE false")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query!("DELETE FROM sessions WHERE false")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query!("DELETE FROM likes WHERE false")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query!("RESET ROLE").execute(&mut *tx).await.unwrap();
    sqlx::query!("SET LOCAL ROLE yhotos_worker")
        .execute(&mut *tx)
        .await
        .unwrap();
    {
        use sqlx::Acquire;
        let mut savepoint = tx.begin().await.unwrap();
        let err = sqlx::query!("SELECT password_hash FROM users WHERE false")
            .fetch_all(&mut *savepoint)
            .await
            .unwrap_err();
        assert_eq!(
            err.as_database_error().unwrap().code().as_deref(),
            Some("42501")
        );
        savepoint.rollback().await.unwrap();
    }
    {
        use sqlx::Acquire;
        let mut savepoint = tx.begin().await.unwrap();
        let err = sqlx::query!("SELECT secret_enc FROM user_mfa WHERE false")
            .fetch_all(&mut *savepoint)
            .await
            .unwrap_err();
        assert_eq!(
            err.as_database_error().unwrap().code().as_deref(),
            Some("42501")
        );
        savepoint.rollback().await.unwrap();
    }
    {
        use sqlx::Acquire;
        let mut savepoint = tx.begin().await.unwrap();
        let err = sqlx::query!("SELECT value FROM settings WHERE false")
            .fetch_all(&mut *savepoint)
            .await
            .unwrap_err();
        assert_eq!(
            err.as_database_error().unwrap().code().as_deref(),
            Some("42501")
        );
        savepoint.rollback().await.unwrap();
    }
    {
        use sqlx::Acquire;
        let mut savepoint = tx.begin().await.unwrap();
        let err = sqlx::query!("SELECT detail FROM audit_log WHERE false")
            .fetch_all(&mut *savepoint)
            .await
            .unwrap_err();
        assert_eq!(
            err.as_database_error().unwrap().code().as_deref(),
            Some("42501")
        );
        savepoint.rollback().await.unwrap();
    }
    {
        use sqlx::Acquire;
        let mut savepoint = tx.begin().await.unwrap();
        let err = sqlx::query!("UPDATE users SET is_admin=true WHERE false")
            .execute(&mut *savepoint)
            .await
            .unwrap_err();
        assert_eq!(
            err.as_database_error().unwrap().code().as_deref(),
            Some("42501")
        );
        savepoint.rollback().await.unwrap();
    }
    {
        use sqlx::Acquire;
        let mut savepoint = tx.begin().await.unwrap();
        let err = sqlx::query!("DELETE FROM user_mfa WHERE false")
            .execute(&mut *savepoint)
            .await
            .unwrap_err();
        assert_eq!(
            err.as_database_error().unwrap().code().as_deref(),
            Some("42501")
        );
        savepoint.rollback().await.unwrap();
    }
    {
        use sqlx::Acquire;
        let mut savepoint = tx.begin().await.unwrap();
        let err = sqlx::query!("UPDATE settings SET value='{}' WHERE false")
            .execute(&mut *savepoint)
            .await
            .unwrap_err();
        assert_eq!(
            err.as_database_error().unwrap().code().as_deref(),
            Some("42501")
        );
        savepoint.rollback().await.unwrap();
    }
    {
        use sqlx::Acquire;
        let mut savepoint = tx.begin().await.unwrap();
        let err = sqlx::query!("DELETE FROM audit_log WHERE false")
            .execute(&mut *savepoint)
            .await
            .unwrap_err();
        assert_eq!(
            err.as_database_error().unwrap().code().as_deref(),
            Some("42501")
        );
        savepoint.rollback().await.unwrap();
    }
    {
        use sqlx::Acquire;
        let mut savepoint = tx.begin().await.unwrap();
        let err = sqlx::query!(
            "SELECT user_set_own_password($1,$2)",
            Uuid::now_v7(),
            secrecy::ExposeSecret::expose_secret(&hash)
        )
        .execute(&mut *savepoint)
        .await
        .unwrap_err();
        assert_eq!(
            err.as_database_error().unwrap().code().as_deref(),
            Some("42501")
        );
        savepoint.rollback().await.unwrap();
    }
    {
        use sqlx::Acquire;
        let mut savepoint = tx.begin().await.unwrap();
        let err = sqlx::query!("SELECT user_touch_login($1)", Uuid::now_v7())
            .execute(&mut *savepoint)
            .await
            .unwrap_err();
        assert_eq!(
            err.as_database_error().unwrap().code().as_deref(),
            Some("42501")
        );
        savepoint.rollback().await.unwrap();
    }
    {
        use sqlx::Acquire;
        let mut savepoint = tx.begin().await.unwrap();
        let err = sqlx::query!("CREATE TABLE public.forbidden_table(id uuid)")
            .execute(&mut *savepoint)
            .await
            .unwrap_err();
        assert_eq!(
            err.as_database_error().unwrap().code().as_deref(),
            Some("42501")
        );
        savepoint.rollback().await.unwrap();
    }
    sqlx::query!("UPDATE jobs SET priority=1 WHERE false")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query!("UPDATE photos SET title='changed' WHERE false")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query!("RESET ROLE").execute(&mut *tx).await.unwrap();
    sqlx::query!("SET LOCAL ROLE yhotos_admin")
        .execute(&mut *tx)
        .await
        .unwrap();
    {
        use sqlx::Acquire;
        let mut savepoint = tx.begin().await.unwrap();
        let err = sqlx::query!("CREATE TABLE public.forbidden_table(id uuid)")
            .execute(&mut *savepoint)
            .await
            .unwrap_err();
        assert_eq!(
            err.as_database_error().unwrap().code().as_deref(),
            Some("42501")
        );
        savepoint.rollback().await.unwrap();
    }
    sqlx::query!("UPDATE users SET is_admin=true WHERE false")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query!("UPDATE user_mfa SET label='changed' WHERE false")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query!("UPDATE settings SET value='{}' WHERE false")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query!("SELECT detail FROM audit_log WHERE false")
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    sqlx::query!("RESET ROLE").execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert!(
        !users::get_by_id(&mut conn, user.id)
            .await
            .unwrap()
            .unwrap()
            .must_change_password
    );
}

#[sqlx::test]
async fn core_sql_types_validate_database_text(pool: PgPool) {
    use yhotos_core::{
        AlbumId, GroupId, JobId, PhotoId, SessionId, ShareLinkId, ShortId, Slug, TagId, UserId,
        access::Visibility,
    };
    let mut conn = pool.acquire().await.unwrap();
    let invalid = format!("invalid/{}", Uuid::now_v7());
    assert!(
        sqlx::query_scalar!(r#"SELECT $1::text AS "slug!: Slug""#, &invalid)
            .fetch_one(&mut *conn)
            .await
            .is_err()
    );
    assert!(
        sqlx::query_scalar!(r#"SELECT $1::text AS "short!: ShortId""#, &invalid)
            .fetch_one(&mut *conn)
            .await
            .is_err()
    );
    let slug = Slug::new("a-valid-slug").unwrap();
    assert_eq!(
        sqlx::query_scalar!(r#"SELECT $1::citext AS "slug!: Slug""#, &slug as _)
            .fetch_one(&mut *conn)
            .await
            .unwrap(),
        slug
    );
    let photo = PhotoId::new();
    let short = ShortId::from_photo_id(photo);
    assert_eq!(
        sqlx::query_scalar!(r#"SELECT $1::text AS "short!: ShortId""#, short as _)
            .fetch_one(&mut *conn)
            .await
            .unwrap(),
        short
    );
    for value in [
        Visibility::Inherit,
        Visibility::Public,
        Visibility::Restricted,
    ] {
        assert_eq!(
            sqlx::query_scalar!(
                r#"SELECT $1::visibility AS "visibility!: Visibility""#,
                value as _
            )
            .fetch_one(&mut *conn)
            .await
            .unwrap(),
            value
        );
    }
    let id = Uuid::now_v7();
    let row = sqlx::query!(r#"SELECT $1::uuid AS "album!: AlbumId", $1::uuid AS "photo!: PhotoId", $1::uuid AS "user!: UserId", $1::uuid AS "group!: GroupId", $1::uuid AS "tag!: TagId", $1::uuid AS "job!: JobId", $1::uuid AS "session!: SessionId", $1::uuid AS "link!: ShareLinkId""#, id)
        .fetch_one(&mut *conn).await.unwrap();
    assert_eq!(
        [
            row.album.0,
            row.photo.0,
            row.user.0,
            row.group.0,
            row.tag.0,
            row.job.0,
            row.session.0,
            row.link.0
        ],
        [id; 8]
    );
}
