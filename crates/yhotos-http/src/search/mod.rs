//! Public, principal-private search with bounded concurrent candidate lists and RRF.
mod client;
#[cfg(feature = "admin")]
pub(crate) mod people;
pub mod query;
use crate::{
    AppState,
    error::HttpError,
    middleware::{security_headers::Nonce, session::SessionIdentity},
    pages,
    share::Links,
};
use axum::{
    Extension,
    extract::{Query, State},
    http::header,
    response::{Html, IntoResponse, Response},
};
pub use client::Client;
use std::collections::{HashMap, HashSet};
use yhotos_core::{PhotoId, ShortId};
use yhotos_db::{
    access_sql,
    search::{self, Candidate, List},
};

/// A stable ranking entry; ties break by time-ordered photo identifier.
pub struct Ranked {
    /// Already-visible metadata and untrusted OCR text.
    pub candidate: Candidate,
    /// Sum of reciprocal one-based ranks with k=60.
    pub score: f64,
}
/// Fuses up to three bounded lists without crediting duplicates within a list.
#[must_use]
pub fn fuse(lists: impl IntoIterator<Item = Vec<Candidate>>) -> Vec<Ranked> {
    let mut scores: HashMap<PhotoId, Ranked> = HashMap::new();
    for list in lists.into_iter().take(3) {
        let mut seen = HashSet::new();
        for (rank, candidate) in list.into_iter().take(200).enumerate() {
            if !seen.insert(candidate.photo.id) {
                continue;
            }
            let score = 1.0 / (60.0 + f64::from(u32::try_from(rank + 1).unwrap_or(201)));
            scores
                .entry(candidate.photo.id)
                .and_modify(|v| v.score += score)
                .or_insert(Ranked { candidate, score });
        }
    }
    let mut rows: Vec<_> = scores.into_values().collect();
    rows.sort_unstable_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(a.candidate.photo.id.cmp(&b.candidate.photo.id))
    });
    rows
}
#[derive(Default, serde::Deserialize)]
pub(crate) struct Options {
    #[serde(default)]
    q: String,
    like: Option<String>,
    page: Option<u32>,
}
pub(crate) struct Results {
    pub(crate) rows: Vec<Ranked>,
    pub(crate) total: usize,
    pub(crate) page: u32,
    pub(crate) parsed: query::Query,
}
pub(crate) async fn execute(
    state: &AppState,
    identity: &SessionIdentity,
    links: &Links,
    options: &Options,
) -> Result<Results, HttpError> {
    let _permit = state.search.permit().ok_or(HttpError::RateLimited)?;
    let parsed = query::parse(&options.q).map_err(|_| HttpError::BadRequest)?;
    let page = options.page.unwrap_or(1);
    if page == 0 || page > 10 {
        return Err(HttpError::NotFound);
    }
    let like = options
        .like
        .as_deref()
        .map(str::parse::<ShortId>)
        .transpose()
        .map_err(|_| HttpError::BadRequest)?;
    let like = like.map(|v| v.to_string());
    let principal = identity.principal();
    let bindings = access_sql::visible_photos(&principal)
        .with_links(links.0.iter().filter(|l| l.caps.view).map(|l| l.target));
    let vector = if cfg!(feature = "ml-client") && !parsed.text.is_empty() && like.is_none() {
        state.search.embed(&parsed.text).await
    } else {
        None
    };
    let vector = vector.as_ref().map(|v| {
        format!(
            "[{}]",
            v.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        )
    });
    let filters: Vec<_> = parsed
        .filters
        .iter()
        .map(|f| search::Filter {
            key: f.key.clone(),
            value: f.value.clone(),
        })
        .collect();
    let load = |list| {
        search::candidates(
            &state.pool,
            &bindings,
            &filters,
            &parsed.text,
            vector.as_deref(),
            like.as_deref(),
            list,
        )
    };
    let semantic = async {
        if vector.is_some() || like.is_some() {
            load(List::Semantic).await
        } else {
            Ok(Vec::new())
        }
    };
    let text = async {
        if parsed.text.is_empty() {
            Ok(Vec::new())
        } else {
            load(List::Text).await
        }
    };
    let structured = async {
        if like.is_none() {
            load(List::Structured).await
        } else {
            Ok(Vec::new())
        }
    };
    let (semantic, text, structured) =
        tokio::try_join!(semantic, text, structured).map_err(|e| HttpError::Internal(e.into()))?;
    let rows = fuse([semantic, text, structured]);
    let total = rows.len();
    let offset = usize::try_from(page - 1).unwrap_or(10) * 60;
    if offset >= total && page > 1 {
        return Err(HttpError::NotFound);
    }
    // A scene/object vocabulary prevents raw names, addresses or other PII from
    // entering analytics. Unknown words are redacted; retrieval still uses them.
    let pool = state.pool.clone();
    let normalized = analytics_query(&parsed.text);
    tokio::spawn(async move {
        if search::event(&pool, &normalized).await.is_err() {
            tracing::warn!("search event unavailable");
        }
    });
    Ok(Results {
        rows: rows.into_iter().skip(offset).take(60).collect(),
        total,
        page,
        parsed,
    })
}
pub(crate) async fn page(
    State(state): State<AppState>,
    Extension(identity): Extension<SessionIdentity>,
    Extension(links): Extension<Links>,
    Extension(nonce): Extension<Nonce>,
    Query(options): Query<Options>,
) -> Result<Response, HttpError> {
    let results = execute(&state, &identity, &links, &options).await?;
    let nonce = nonce.to_string();
    let html = pages::render_search(
        &results,
        &options.q,
        options.like.as_deref(),
        &pages::context(&nonce, &state)?,
        "/search",
    )?;
    Ok(([(header::CACHE_CONTROL, "private, no-store")], Html(html)).into_response())
}

#[cfg(test)]
mod tests;

fn analytics_query(text: &str) -> String {
    static WORDS: std::sync::LazyLock<std::collections::HashSet<&'static str>> =
        std::sync::LazyLock::new(|| {
            include_str!("../../../../ml/labels.txt")
                .split_whitespace()
                .collect()
        });
    text.split_whitespace()
        .map(|word| {
            if WORDS.contains(word) {
                word
            } else {
                "[redacted]"
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
