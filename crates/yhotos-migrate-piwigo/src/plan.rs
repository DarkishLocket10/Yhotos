use crate::{
    Problem, Report, Result,
    source::{Album, Image, Snapshot},
};
use std::collections::{BTreeMap, BTreeSet};
use yhotos_core::Slug;

pub(crate) struct PlannedAlbum {
    pub source: Album,
    pub title: String,
    pub description: String,
    pub slug: Slug,
    pub order: i32,
    pub rank: Vec<u32>,
}
pub(crate) struct Plan {
    pub albums: Vec<PlannedAlbum>,
    pub images: Vec<Image>,
    pub members: BTreeMap<i64, Vec<(i64, i32)>>,
    pub tags: BTreeMap<i64, (Slug, String)>,
    pub tag_links: BTreeMap<i64, Vec<i64>>,
    pub aliases: BTreeMap<String, i64>,
}

pub(crate) fn problem(id: Option<i64>, reason: &str) -> Problem {
    Problem {
        id,
        reason: reason.into(),
    }
}

// Names and attribution are text, not HTML descriptions. Preserve decoded
// angle brackets; the viewer escapes text when rendering it.
pub(crate) fn text(input: &str) -> String {
    html_escape::decode_html_entities(input)
        .chars()
        .filter(|ch| !ch.is_control() || ch.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn plain(input: &str) -> String {
    let decoded = html_escape::decode_html_entities(input);
    let mut text = String::new();
    let mut in_tag = false;
    for ch in decoded.chars() {
        match ch {
            '<' => {
                in_tag = true;
                text.push(' ');
            }
            '>' if in_tag => in_tag = false,
            _ if !in_tag && (!ch.is_control() || ch == '\n' || ch == '\t') => text.push(ch),
            _ => {}
        }
    }
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn unique_slug(title: &str, used: &mut BTreeSet<String>) -> (Slug, bool) {
    let base = Slug::from_title(title);
    let mut candidate = base.clone();
    let mut suffix = 2;
    while !used.insert(candidate.to_string()) {
        let tail = format!("-{suffix}");
        let stem = base
            .as_str()
            .chars()
            .take(80 - tail.len())
            .collect::<String>();
        candidate = Slug::from_title(&format!("{}{tail}", stem.trim_end_matches('-')));
        suffix += 1;
    }
    let collided = candidate != base;
    (candidate, collided)
}

fn alias(value: &str) -> Option<String> {
    // Legacy HTTP parser accepts a single ASCII alphanumeric/_/- segment.
    (!value.is_empty()
        && value.len() <= 200
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-')))
    .then(|| value.to_owned())
}

#[allow(clippy::too_many_lines)]
pub(crate) fn build(snapshot: Snapshot, report: &mut Report) -> Result<Plan> {
    let mut albums = Vec::new();
    let mut remaining = snapshot.albums;
    let mut known = BTreeSet::new();
    let mut used = BTreeMap::<Option<i64>, BTreeSet<String>>::new();
    while !remaining.is_empty() {
        let before = remaining.len();
        let mut deferred = Vec::new();
        for source in remaining {
            if source.id <= 0 || source.parent.is_some_and(|p| !known.contains(&p)) {
                deferred.push(source);
                continue;
            }
            if !matches!(source.status.as_str(), "public" | "private") {
                report.unsupported.push(problem(
                    Some(source.id),
                    "unknown album status; treated as private",
                ));
            }
            let title = text(&source.name);
            let description = plain(source.description.as_deref().unwrap_or(""));
            let (slug, collision) = unique_slug(&title, used.entry(source.parent).or_default());
            if collision {
                report
                    .collisions
                    .push(problem(Some(source.id), "sibling album slug suffixed"));
            }
            let rank = source
                .global_rank
                .as_deref()
                .unwrap_or("")
                .split('.')
                .map(str::parse::<u32>)
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap_or_default();
            if rank.is_empty() {
                report.unsupported.push(problem(
                    Some(source.id),
                    "invalid global_rank; ordered by id",
                ));
            }
            known.insert(source.id);
            albums.push(PlannedAlbum {
                source,
                title,
                description,
                slug,
                order: 0,
                rank,
            });
        }
        if deferred.len() == before {
            for source in deferred {
                report.unsupported.push(problem(
                    Some(source.id),
                    "album has missing parent or cycle; skipped",
                ));
            }
            break;
        }
        remaining = deferred;
    }
    // Rank comparisons are numeric, so 2 sorts before 10. Parents were already
    // topologically ordered above; assign sibling order without rearranging them.
    let mut sibling_order = BTreeMap::<Option<i64>, Vec<(Vec<u32>, i64)>>::new();
    for a in &albums {
        sibling_order
            .entry(a.source.parent)
            .or_default()
            .push((a.rank.clone(), a.source.id));
    }
    for siblings in sibling_order.values_mut() {
        siblings.sort();
    }
    for a in &mut albums {
        a.order = sibling_order
            .get(&a.source.parent)
            .and_then(|s| s.iter().position(|(_, id)| *id == a.source.id))
            .and_then(|i| i32::try_from(i).ok())
            .ok_or(crate::Error::SourceData)?;
    }
    let rank: BTreeMap<_, _> = albums
        .iter()
        .map(|a| (a.source.id, a.rank.clone()))
        .collect();
    let mut members = BTreeMap::<i64, Vec<(i64, i32)>>::new();
    let image_ids: BTreeSet<_> = snapshot.images.iter().map(|i| i.id).collect();
    for link in snapshot.memberships {
        if !known.contains(&link.category) || !image_ids.contains(&link.image) {
            report.unsupported.push(problem(
                Some(link.image),
                "membership references missing image or album",
            ));
            continue;
        }
        let order = link
            .photo_order
            .and_then(|n| i32::try_from(n).ok())
            .unwrap_or(0);
        members
            .entry(link.image)
            .or_default()
            .push((link.category, order));
    }
    for rows in members.values_mut() {
        rows.sort_by_key(|(category, _)| (rank.get(category), *category));
        rows.dedup_by_key(|(category, _)| *category);
    }
    let mut tag_slugs = BTreeSet::new();
    let mut tags = BTreeMap::new();
    for tag in snapshot.tags {
        let name = text(&tag.name);
        let (slug, collision) = unique_slug(&name, &mut tag_slugs);
        if collision {
            report
                .collisions
                .push(problem(Some(tag.id), "tag slug suffixed"));
        }
        tags.insert(tag.id, (slug, name));
    }
    let mut tag_links = BTreeMap::<i64, Vec<i64>>::new();
    for link in snapshot.tag_links {
        if tags.contains_key(&link.tag) && image_ids.contains(&link.image) {
            tag_links.entry(link.image).or_default().push(link.tag);
        } else {
            report.unsupported.push(problem(
                Some(link.image),
                "tag link references missing image or tag",
            ));
        }
    }
    let mut aliases = BTreeMap::new();
    let mut explicit = BTreeMap::new();
    let mut explicit_ambiguous = BTreeSet::new();
    let mut ambiguous = BTreeSet::new();
    for a in &albums {
        let mut names =
            BTreeSet::from([a.slug.to_string(), Slug::from_title(&a.title).to_string()]);
        names.insert(Slug::from_title(&a.title).to_string().replace('-', "_"));
        // Keep name aliases with spaces or underscores, punctuation and case.
        // Encode whole segments, including '/'.
        for name in [&a.title, &a.title.replace(' ', "_")] {
            for variant in [name.clone(), name.to_lowercase()] {
                let encoded = url::form_urlencoded::byte_serialize(variant.as_bytes())
                    .collect::<String>()
                    .replace('+', "%20");
                if !encoded.is_empty() {
                    names.insert(encoded);
                }
            }
        }
        if let Some(name) = alias(&a.title) {
            names.insert(name);
        }
        if let Some(value) = a
            .source
            .url_name
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            if let Some(name) = alias(value) {
                if let Some(old) = explicit.insert(name.clone(), a.source.id)
                    && old != a.source.id
                {
                    explicit_ambiguous.insert(name.clone());
                }
                names.insert(name);
            } else {
                report.unsupported.push(problem(
                    Some(a.source.id),
                    "permalink needs manual legacy redirect",
                ));
            }
        }
        for name in names {
            if let Some(old) = aliases.insert(name.clone(), a.source.id)
                && old != a.source.id
            {
                ambiguous.insert(name);
            }
        }
    }
    for name in ambiguous {
        let resolution = if explicit.contains_key(&name) && !explicit_ambiguous.contains(&name) {
            "source permalink retained"
        } else {
            aliases.remove(&name);
            "alias omitted; numeric links retained"
        };
        report.collisions.push(problem(
            None,
            &format!("album legacy alias {name} collides; {resolution}"),
        ));
    }
    // A genuine Piwigo permalink outranks a coincidentally generated name.
    for (name, id) in explicit {
        if !explicit_ambiguous.contains(&name) {
            aliases.insert(name, id);
        }
    }
    report.unsupported.push(problem(
        None,
        "users, comments, ratings and Piwigo derivatives are not migrated",
    ));
    Ok(Plan {
        albums,
        images: snapshot.images,
        members,
        tags,
        tag_links,
        aliases,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strips_html_entities_controls_and_bounds_slug_suffixes() {
        assert_eq!(plain("Trip &amp; &lt;b&gt;Snow&lt;/b&gt;\0"), "Trip & Snow");
        assert_eq!(text("Sky &lt;3 &amp; sun\0"), "Sky <3 & sun");
        let mut used = BTreeSet::new();
        let title = "a".repeat(80);
        assert!(!unique_slug(&title, &mut used).1);
        let (slug, collision) = unique_slug(&title, &mut used);
        assert!(collision);
        assert_eq!(slug.as_str().len(), 80);
        assert!(slug.as_str().ends_with("-2"));
    }
}
