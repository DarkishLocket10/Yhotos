//! Compile-time administrative views; every stored string is escaped at the template boundary.
#![allow(clippy::too_many_lines, clippy::too_many_arguments)]
// serde_json object-key indexing returns Null for absent keys.
#![allow(clippy::indexing_slicing)]
use crate::{error::HttpError, middleware::csrf::CsrfToken, static_assets};
use askama::Template;
use serde_json::Value;

#[derive(Default)]
struct Choice {
    value: String,
    label: String,
    selected: bool,
}
#[derive(Default)]
struct Field {
    name: String,
    label: String,
    kind: String,
    value: String,
    choices: Vec<Choice>,
    checked: bool,
}
#[derive(Default)]
struct Form {
    title: String,
    op: String,
    endpoint: String,
    method: String,
    fields: Vec<Field>,
}
#[derive(Default)]
struct Card {
    id: String,
    parent: String,
    title: String,
    href: String,
    preview: String,
    badges: String,
    description: String,
    details: Vec<(String, String)>,
    forms: Vec<Form>,
    selectable: bool,
    download: String,
}
#[derive(Template)]
#[template(path = "default/admin/library.html")]
struct Page<'a> {
    title: String,
    name: &'a str,
    csrf: &'a str,
    nonce: &'a str,
    stylesheet: &'static str,
    script: &'static str,
    cards: Vec<Card>,
    forms: Vec<Form>,
    counts: Vec<(String, String)>,
    filter: &'a str,
    next: u32,
    manual: bool,
    chunk: u32,
    album: String,
}
fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_owned()
}
fn rows(v: &Value) -> &[Value] {
    v.as_array().map_or(&[], Vec::as_slice)
}
fn text(name: &str, label: &str, value: impl Into<String>) -> Field {
    Field {
        name: name.into(),
        label: label.into(),
        kind: "text".into(),
        value: value.into(),
        ..Default::default()
    }
}
fn hidden(name: &str, value: impl Into<String>) -> Field {
    Field {
        kind: "hidden".into(),
        ..text(name, "", value)
    }
}
fn optional(name: &str, label: &str, value: impl Into<String>) -> Field {
    Field {
        kind: "optional".into(),
        ..text(name, label, value)
    }
}
fn check(name: &str, label: &str, checked: bool) -> Field {
    Field {
        kind: "boolean".into(),
        checked,
        ..text(name, label, "")
    }
}
fn area(name: &str, label: &str, value: impl Into<String>) -> Field {
    Field {
        kind: "textarea".into(),
        ..text(name, label, value)
    }
}
fn select(name: &str, label: &str, options: &[(&str, &str)], selected: &str) -> Field {
    Field {
        kind: "select".into(),
        choices: options
            .iter()
            .map(|(v, l)| Choice {
                value: (*v).into(),
                label: (*l).into(),
                selected: *v == selected,
            })
            .collect(),
        ..text(name, label, "")
    }
}
fn override_mode(name: &str, label: &str) -> Field {
    select(
        name,
        label,
        &[
            ("keep", "Keep current value and source"),
            ("automatic", "Use automatic metadata"),
            ("manual", "Override with the entered value"),
        ],
        "keep",
    )
}
fn choices(
    name: &str,
    label: &str,
    data: &Value,
    key: &str,
    selected: &[String],
    multi: bool,
    blank: bool,
) -> Field {
    let mut items = Vec::new();
    if blank {
        items.push(Choice {
            label: "None".into(),
            selected: selected.is_empty(),
            ..Default::default()
        });
    }
    items.extend(rows(data).iter().map(|r| {
        let id = s(r, "id");
        Choice {
            label: s(r, key),
            selected: selected.contains(&id),
            value: id,
        }
    }));
    Field {
        kind: if multi {
            "ids-select"
        } else {
            "optional-select"
        }
        .into(),
        choices: items,
        ..text(name, label, "")
    }
}
fn grant_field(data: &Value, current: &Value) -> Field {
    let mut items = Vec::new();
    for (kind, key, label) in [("user", "users", "username"), ("group", "groups", "name")] {
        for row in rows(&data[key]) {
            let id = s(row, "id");
            items.push(Choice {
                value: format!("{kind}:{id}"),
                label: format!("{kind}: {}", s(row, label)),
                selected: rows(current)
                    .iter()
                    .any(|g| s(g, "kind") == kind && s(g, "id") == id),
            });
        }
    }
    Field {
        kind: "grants".into(),
        choices: items,
        ..text("grants", "Users and groups allowed to view", "")
    }
}
fn form(title: &str, op: &str, fields: Vec<Field>) -> Form {
    Form {
        title: title.into(),
        op: op.into(),
        endpoint: "/admin/api/actions".into(),
        method: "POST".into(),
        fields,
    }
}
fn link_form(kind: &str, id: &str) -> Form {
    Form {
        title: "Create share link (URL shown once)".into(),
        op: String::new(),
        endpoint: "/admin/api/share-links".into(),
        method: "POST".into(),
        fields: vec![
            hidden("target_kind", kind),
            hidden("target_id", id),
            optional("expires_at", "Expires at (RFC 3339, empty means never)", ""),
            Field {
                kind: "password".into(),
                ..optional("passphrase", "Passphrase (optional)", "")
            },
            check("allow_view", "Allow viewing", true),
            check("allow_download", "Allow downloads", false),
            check("allow_like", "Allow likes", false),
            check("allow_flag", "Allow flags", false),
        ],
    }
}
fn vis(photo: bool, current: &str) -> Field {
    let opts = if photo {
        vec![
            ("inherit", "Inherit album access"),
            ("public", "Public"),
            ("restricted", "Restricted"),
        ]
    } else {
        vec![("public", "Public"), ("restricted", "Restricted")]
    };
    select("visibility", "Visibility", &opts, current)
}
fn id_values(v: &Value) -> Vec<String> {
    rows(v)
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}
fn pairs(v: &Value) -> Vec<(String, String)> {
    v.as_object().map_or_else(Vec::new, |o| {
        o.iter()
            .map(|(k, v)| {
                (
                    k.replace('_', " "),
                    v.as_str().map_or_else(|| v.to_string(), str::to_owned),
                )
            })
            .collect()
    })
}
fn trash_forms(kind: &str, id: &str) -> Vec<Form> {
    vec![
        form(
            "Restore",
            "restore",
            vec![hidden("kind", kind), hidden("id", id)],
        ),
        form(
            "Permanently purge",
            "purge",
            vec![
                hidden("kind", kind),
                hidden("id", id),
                text("confirmation", "Type PURGE to permanently delete", ""),
            ],
        ),
    ]
}
fn photo_card(r: &Value, selectable: bool) -> Card {
    Card {
        id: s(r, "id"),
        title: s(r, "title"),
        href: format!("/admin/photos/{}", s(r, "short_id")),
        preview: s(r, "preview"),
        selectable,
        badges: format!(
            "{} · {}{} · {}{}",
            s(r, "visibility"),
            s(r, "kind"),
            if r["is_hdr"] == true { " · HDR" } else { "" },
            s(r, "status"),
            if r["deleted_at"].is_null() {
                ""
            } else {
                " · trash"
            }
        ),
        ..Default::default()
    }
}
/// Renders a page without runtime templates. The only unescaped markup is the sanitized
/// Markdown preview, constructed here rather than accepted from database HTML.
pub(super) fn render(
    name: &str,
    data: &Value,
    options: &Value,
    csrf: &CsrfToken,
    nonce: &str,
    filter: &str,
    page: u32,
    chunk: u32,
) -> Result<String, HttpError> {
    let title = match name {
        "dashboard" => "Library dashboard",
        "album" => "Album photos",
        "photo" => "Photo editor",
        "albums" => "Album tree",
        "trash" => "Trash",
        "flags" => "Open flags",
        "likes" => "Likes overview",
        "users" => "Users and sessions",
        "groups" => "Groups",
        "links" => "Share links",
        "jobs" => "Job queue",
        "audit" => "Audit log",
        _ => return Err(HttpError::NotFound),
    };
    let mut p = Page {
        title: title.into(),
        name,
        csrf: csrf.form_value(),
        nonce,
        stylesheet: static_assets::stylesheet_url()?,
        script: static_assets::admin_url()?,
        cards: Vec::new(),
        forms: Vec::new(),
        counts: Vec::new(),
        filter,
        next: page.saturating_add(1),
        manual: false,
        chunk,
        album: String::new(),
    };
    match name {
        "dashboard" => {
            p.counts = pairs(data)
                .into_iter()
                .filter(|(k, _)| k != "activity")
                .collect();
            for r in rows(&data["activity"]) {
                p.cards.push(Card {
                    title: s(r, "action"),
                    details: pairs(r),
                    ..Default::default()
                });
            }
        }
        "albums" => {
            p.forms.push(form(
                "Create album",
                "album_create",
                vec![
                    text("title", "Title", ""),
                    choices(
                        "parent_id",
                        "Parent album",
                        &options["albums"],
                        "path",
                        &[],
                        false,
                        true,
                    ),
                ],
            ));
            for r in rows(data) {
                let id = s(r, "id");
                let mut card = Card {
                    id: id.clone(),
                    parent: s(r, "parent_id"),
                    title: s(r, "path"),
                    href: format!("/admin/albums/{id}"),
                    ..Default::default()
                };
                card.badges = s(r, "visibility");
                let mut ancestor = s(r, "parent_id");
                let mut seen = std::collections::HashSet::new();
                while !ancestor.is_empty() && seen.insert(ancestor.clone()) {
                    if let Some(parent) = rows(data).iter().find(|a| s(a, "id") == ancestor) {
                        if s(parent, "visibility") == "restricted" {
                            card.badges.push_str(", hidden by restricted parent ");
                            card.badges.push_str(&s(parent, "title"));
                        }
                        ancestor = s(parent, "parent_id");
                    } else {
                        break;
                    }
                }
                if !r["deleted_at"].is_null() {
                    card.badges.push_str(" · trash");
                }
                card.description = crate::markdown::render(&s(r, "description"));
                card.forms.push(form(
                    "Edit album",
                    "album_edit",
                    vec![
                        hidden("id", &id),
                        text("title", "Title", s(r, "title")),
                        area("description", "Description (Markdown)", s(r, "description")),
                        vis(false, &s(r, "visibility")),
                        check(
                            "manual_order",
                            "Use manual photo order",
                            r["manual_order"] == true,
                        ),
                        choices(
                            "cover_photo_id",
                            "Cover photo",
                            &Value::Array(
                                rows(&options["photos"])
                                    .iter()
                                    .filter(|p| s(p, "album_id") == id)
                                    .cloned()
                                    .collect(),
                            ),
                            "title",
                            &[s(r, "cover_photo_id")],
                            false,
                            true,
                        ),
                        grant_field(options, &r["grants"]),
                    ],
                ));
                for (direction, label) in [
                    ("up", "Move up"),
                    ("down", "Move down"),
                    ("indent", "Indent under previous album"),
                    ("outdent", "Outdent"),
                ] {
                    card.forms.push(form(
                        label,
                        "album_move",
                        vec![
                            hidden("id", &id),
                            hidden("direction", direction),
                            optional("parent_id", "", ""),
                            optional("before_id", "", ""),
                        ],
                    ));
                }
                card.forms.push(form(
                    "Move to parent",
                    "album_move",
                    vec![
                        hidden("id", &id),
                        hidden("direction", "move"),
                        choices(
                            "parent_id",
                            "Parent album",
                            &options["albums"],
                            "path",
                            &[],
                            false,
                            true,
                        ),
                        choices(
                            "before_id",
                            "Place before album (optional)",
                            &options["albums"],
                            "path",
                            &[],
                            false,
                            true,
                        ),
                    ],
                ));
                card.forms.push(form(
                    "Move album to trash",
                    "trash",
                    vec![hidden("id", &id), hidden("kind", "album")],
                ));
                card.forms.push(link_form("album", &id));
                p.cards.push(card);
            }
        }
        "album" => {
            let a = &data["album"];
            p.title = s(a, "title");
            p.album = s(a, "id");
            p.manual = a["manual_order"] == true;
            p.forms.push(link_form("album", &p.album));
            for r in rows(&data["photos"]) {
                p.cards.push(photo_card(r, r["deleted_at"].is_null()));
            }
            let base = || {
                vec![Field {
                    kind: "selection".into(),
                    ..hidden("ids", "")
                }]
            };
            for (kind, label) in [
                ("move", "Move selected photos"),
                ("copy", "Copy selected photos"),
            ] {
                let mut fields = base();
                fields.push(hidden("action.kind", kind));
                fields.push(choices(
                    "action.album_id",
                    "Destination album",
                    &options["albums"],
                    "path",
                    &[],
                    false,
                    false,
                ));
                p.forms.push(form(label, "bulk", fields));
            }
            let mut fields = base();
            fields.push(hidden("action.kind", "visibility"));
            let mut v = vis(true, "inherit");
            v.name = "action.visibility".into();
            fields.push(v);
            let mut g = grant_field(options, &Value::Null);
            g.name = "action.grants".into();
            fields.push(g);
            p.forms.push(form(
                "Set selected photo visibility and grants",
                "bulk",
                fields,
            ));
            let mut fields = base();
            fields.extend([
                hidden("action.kind", "tags"),
                Field {
                    kind: "tags".into(),
                    ..text("action.add", "Add tags (comma separated)", "")
                },
                Field {
                    kind: "tags".into(),
                    ..text("action.remove", "Remove tags (comma separated)", "")
                },
            ]);
            p.forms
                .push(form("Edit selected photo tags", "bulk", fields));
            let mut fields = base();
            fields.extend([
                hidden("action.kind", "pattern"),
                text(
                    "action.title",
                    "Title pattern ({album}, {n})",
                    "{album} {n}",
                ),
                area("action.description", "Description pattern", ""),
            ]);
            p.forms
                .push(form("Apply title and description patterns", "bulk", fields));
            let mut fields = base();
            fields.push(hidden("action.kind", "delete"));
            p.forms
                .push(form("Move selected photos to trash", "bulk", fields));
        }
        "photo" => {
            let id = s(data, "id");
            p.title = s(data, "title");
            let mut fields = vec![
                hidden("id", &id),
                text("title", "Title", s(data, "title")),
                area(
                    "description",
                    "Description (Markdown)",
                    s(data, "description"),
                ),
                override_mode("taken_at_mode", "Capture date source"),
                override_mode("gps_mode", "Coordinates source"),
                override_mode("place_mode", "Place source"),
                optional(
                    "taken_at",
                    "Taken at override (RFC 3339)",
                    s(data, "taken_at"),
                ),
                choices(
                    "place_id",
                    "Place override",
                    &options["places"],
                    "name",
                    &[s(data, "place_id")],
                    false,
                    true,
                ),
            ];
            for (key, label) in [("gps_lat", "Latitude"), ("gps_lon", "Longitude")] {
                fields.push(Field {
                    kind: "optional-number".into(),
                    ..text(
                        key,
                        label,
                        data[key]
                            .as_f64()
                            .map_or_else(String::new, |v| v.to_string()),
                    )
                });
            }
            fields.extend([
                vis(true, &s(data, "visibility")),
                Field {
                    kind: "tags".into(),
                    ..text(
                        "tags",
                        "Tags (comma separated)",
                        id_values(&data["tags"]).join(", "),
                    )
                },
                grant_field(options, &data["grants"]),
            ]);
            p.forms
                .push(form("Save photo metadata", "photo_edit", fields));
            p.forms.push(form(
                "Retry ingest",
                "ingest_retry",
                vec![hidden("id", &id)],
            ));
            p.forms.push(link_form("photo", &id));
            p.cards.push(Card {
                id,
                title: "Ingest and original".into(),
                description: crate::markdown::render(&s(data, "description")),
                details: [
                    "status",
                    "failure_reason",
                    "original_name",
                    "original_bytes",
                    "original_hash",
                    "width",
                    "height",
                ]
                .iter()
                .map(|k| {
                    (
                        k.replace('_', " "),
                        data[*k]
                            .as_str()
                            .map_or_else(|| data[*k].to_string(), str::to_owned),
                    )
                })
                .collect(),
                download: format!("/admin/api/photos/{}/original", s(data, "short_id")),
                ..Default::default()
            });
            for d in rows(&data["derivatives"]) {
                p.cards.push(Card {
                    title: format!("{} {}", s(d, "variant"), s(d, "format")),
                    details: pairs(d),
                    ..Default::default()
                });
            }
            p.album = s(data, "album_id");
        }
        "trash" => {
            for kind in ["album", "photo"] {
                for r in rows(&data[format!("{kind}s")]) {
                    let id = s(r, "id");
                    p.cards.push(Card {
                        title: s(r, "title"),
                        badges: format!("Deleted {}", s(r, "deleted_at")),
                        forms: trash_forms(kind, &id),
                        ..Default::default()
                    });
                }
            }
            for r in rows(&data["originals"]) {
                p.cards.push(Card {
                    title: s(r, "original_name"),
                    badges: "Retained replacement original".into(),
                    details: pairs(r),
                    ..Default::default()
                });
            }
        }
        "flags" => {
            for r in rows(data) {
                let id = s(r, "id");
                let mut card = photo_card(r, false);
                card.download = format!("/admin/api/photos/{}/original", s(r, "short_id"));
                for f in rows(&r["flags"]) {
                    card.details.push((s(f, "flagger"), s(f, "at")));
                }
                for (resolution, label) in [
                    ("dismissed", "Dismiss flags"),
                    ("reedited", "Mark re-edited"),
                    ("removed", "Remove photo to trash"),
                    ("visibility", "Change visibility and resolve flags"),
                ] {
                    let mut fields = vec![hidden("id", &id), hidden("resolution", resolution)];
                    if resolution == "visibility" {
                        fields.push(vis(true, "restricted"));
                    } else {
                        fields.push(optional("visibility", "", ""));
                    }
                    card.forms.push(form(label, "flag_resolve", fields));
                }
                p.cards.push(card);
            }
        }
        "likes" => {
            for r in rows(&data["photos"]) {
                let mut card = photo_card(r, false);
                card.details = pairs(r);
                p.cards.push(card);
            }
            for r in rows(&data["users"]) {
                p.cards.push(Card {
                    title: s(r, "username"),
                    details: pairs(r),
                    ..Default::default()
                });
            }
        }
        "users" => {
            p.forms.push(Form {
                title: "Create user (temporary password shown once)".into(),
                endpoint: "/admin/api/users/temporary-password".into(),
                method: "POST".into(),
                fields: vec![text("username", "Username", "")],
                ..Default::default()
            });
            for r in rows(data) {
                let id = s(r, "id");
                let mut card = Card {
                    title: s(r, "username"),
                    badges: if r["disabled_at"].is_null() {
                        "Enabled"
                    } else {
                        "Disabled"
                    }
                    .into(),
                    details: vec![
                        ("MFA status".into(), r["mfa"].to_string()),
                        (
                            "Password change required".into(),
                            r["must_change_password"].to_string(),
                        ),
                    ],
                    ..Default::default()
                };
                if r["is_admin"] != true {
                    card.forms.push(Form {
                        title: "Reset temporary password (shown once)".into(),
                        endpoint: "/admin/api/users/temporary-password".into(),
                        method: "POST".into(),
                        fields: vec![hidden("id", &id)],
                        ..Default::default()
                    });
                    card.forms.push(form(
                        "Save account status",
                        "user_state",
                        vec![
                            hidden("id", &id),
                            check("disabled", "Disable account", !r["disabled_at"].is_null()),
                            check("delete", "Soft delete account", false),
                            optional("confirmation", "Type DELETE when deleting", ""),
                        ],
                    ));
                }
                card.forms.push(form(
                    "Save group membership",
                    "user_groups",
                    vec![
                        hidden("id", &id),
                        choices(
                            "groups",
                            "Groups",
                            &options["groups"],
                            "name",
                            &id_values(&r["groups"]),
                            true,
                            false,
                        ),
                    ],
                ));
                for session in rows(&r["sessions"]) {
                    card.details.push(("Session".into(), session.to_string()));
                    card.forms.push(form(
                        "Revoke session",
                        "session_revoke",
                        vec![hidden("id", s(session, "id"))],
                    ));
                }
                p.cards.push(card);
            }
        }
        "groups" => {
            p.forms.push(form(
                "Create group",
                "group_save",
                vec![
                    optional("id", "", ""),
                    text("name", "Group name", ""),
                    choices(
                        "members",
                        "Members",
                        &options["users"],
                        "username",
                        &[],
                        true,
                        false,
                    ),
                ],
            ));
            for r in rows(data) {
                p.cards.push(Card {
                    title: s(r, "name"),
                    forms: vec![form(
                        "Save group",
                        "group_save",
                        vec![
                            hidden("id", s(r, "id")),
                            text("name", "Group name", s(r, "name")),
                            choices(
                                "members",
                                "Members",
                                &options["users"],
                                "username",
                                &id_values(&r["members"]),
                                true,
                                false,
                            ),
                        ],
                    )],
                    ..Default::default()
                });
            }
        }
        "links" => {
            for (kind, key, label) in [("album", "albums", "path"), ("photo", "photos", "title")] {
                let mut create = link_form(kind, "");
                create.title = format!("Create {kind} share link (URL shown once)");
                create.fields.retain(|f| f.name != "target_id");
                create.fields.insert(
                    1,
                    choices(
                        "target_id",
                        "Target",
                        &options[key],
                        label,
                        &[],
                        false,
                        false,
                    ),
                );
                p.forms.push(create);
            }
            for r in rows(data) {
                let id = s(r, "id");
                let mut card = Card {
                    title: format!("{} {}", s(r, "target_kind"), s(r, "target_id")),
                    details: pairs(r),
                    ..Default::default()
                };
                let mut fields = vec![optional(
                    "expires_at",
                    "Expires at (RFC 3339, empty means never)",
                    s(r, "expires_at"),
                )];
                for (key, label) in [
                    ("allow_view", "Allow viewing"),
                    ("allow_download", "Allow downloads"),
                    ("allow_like", "Allow likes"),
                    ("allow_flag", "Allow flags"),
                ] {
                    fields.push(check(key, label, r[key] == true));
                }
                card.forms.push(Form {
                    title: "Save link settings".into(),
                    endpoint: format!("/admin/api/share-links/{id}"),
                    method: "PATCH".into(),
                    fields,
                    ..Default::default()
                });
                card.forms.push(Form {
                    title: "Revoke link".into(),
                    endpoint: format!("/admin/api/share-links/{id}/revoke"),
                    method: "POST".into(),
                    ..Default::default()
                });
                p.cards.push(card);
            }
        }
        "jobs" => {
            for r in rows(&data["depth"]) {
                p.counts.push((s(r, "kind"), r["count"].to_string()));
            }
            for r in rows(&data["jobs"]) {
                let mut card = Card {
                    title: s(r, "kind"),
                    details: pairs(r),
                    ..Default::default()
                };
                if r["locked_at"].is_null() {
                    for (retry, label) in [(true, "Retry job"), (false, "Delete job")] {
                        card.forms.push(form(
                            label,
                            "job",
                            vec![hidden("id", s(r, "id")), check("retry", "Retry", retry)],
                        ));
                    }
                }
                p.cards.push(card);
            }
        }
        "audit" => {
            for r in rows(data) {
                p.cards.push(Card {
                    title: s(r, "action"),
                    details: pairs(r),
                    ..Default::default()
                });
            }
        }
        _ => {}
    }
    p.render().map_err(|e| HttpError::Internal(e.into()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn markdown_subset_removes_executable_content() {
        let html = crate::markdown::render(
            "**Hello** <script>alert(1)</script> [bad](javascript:alert(1)) <img src=x onerror=alert(1)>",
        );
        assert!(html.contains("<strong>Hello</strong>"));
        assert!(!html.contains("<script"));
        assert!(!html.contains("javascript:"));
        assert!(!html.contains("<img"));
    }
}
