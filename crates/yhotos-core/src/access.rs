//! Pure authorization. Ancestors must contain the complete chain, nearest parent
//! first, without the album itself. A restriction at any level narrows access.

use crate::{AlbumId, GroupId, PhotoId, UserId};

/// Visibility can be narrowed by restrictions on any ancestor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Visibility {
    /// Visible without a grant, subject to ancestor restrictions.
    Public,
    /// Requires an explicit user or group grant.
    Restricted,
    /// Uses the parent visibility; a root album defaults to public.
    Inherit,
}
/// The verified identity and grants used for one access decision.
#[derive(Clone, Copy, Debug)]
pub enum Principal<'a> {
    /// No authenticated identity.
    Anonymous,
    /// Group membership and admin status must come from trusted storage.
    User {
        /// Authenticated user identifier.
        id: UserId,
        /// Verified group membership.
        groups: &'a [GroupId],
        /// Administrators bypass all visibility restrictions.
        is_admin: bool,
    },
    /// A validated link authorizes only its exact target subtree or photo.
    ShareLink {
        /// The target obtained from the validated link record.
        target: ShareTarget,
    },
}
/// The scope of a validated share link, never an untrusted URL parameter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShareTarget {
    /// An album and its subtree, subject to further narrowing.
    Album(AlbumId),
    /// Exactly one photo, subject to album restrictions.
    Photo(PhotoId),
}
/// An album and its complete, nearest-parent-first ancestor chain.
#[derive(Clone, Debug)]
pub struct AlbumAccess<'a> {
    /// Album being checked.
    pub id: AlbumId,
    /// Visibility at this level.
    pub visibility: Visibility,
    /// Explicit grants at this level only.
    pub grants: &'a [Grant],
    /// Complete chain from parent to root; nested `ancestors` are not traversed.
    pub ancestors: &'a [AlbumAccess<'a>],
}
/// A photo and its album's complete access context.
#[derive(Clone, Debug)]
pub struct PhotoAccess<'a> {
    /// Photo being checked.
    pub id: PhotoId,
    /// Containing album and ancestor restrictions.
    pub album: &'a AlbumAccess<'a>,
    /// Visibility at the photo level.
    pub visibility: Visibility,
    /// Explicit photo grants.
    pub grants: &'a [Grant],
}
/// A grant authorizes a single user or all members of a group.
#[derive(Clone, Copy, Debug)]
pub enum Grant {
    /// Explicit user grant.
    User(UserId),
    /// Explicit group grant.
    Group(GroupId),
}

fn admin(p: &Principal<'_>) -> bool {
    matches!(p, Principal::User { is_admin: true, .. })
}
fn granted(p: &Principal<'_>, grants: &[Grant]) -> bool {
    let Principal::User { id, groups, .. } = p else {
        return false;
    };
    grants.iter().any(|g| match g {
        Grant::User(user) => user == id,
        Grant::Group(group) => groups.contains(group),
    })
}
fn level(p: &Principal<'_>, a: &AlbumAccess<'_>, shared_album: Option<AlbumId>) -> bool {
    a.visibility != Visibility::Restricted || Some(a.id) == shared_album || granted(p, a.grants)
}

/// Denies all descendants of any ungranted restricted ancestor, except admins.
#[must_use]
pub fn can_view_album(p: &Principal<'_>, a: &AlbumAccess<'_>) -> bool {
    if admin(p) {
        return true;
    }
    let shared = match p {
        Principal::ShareLink {
            target: ShareTarget::Album(id),
        } => {
            if a.id != *id && !a.ancestors.iter().any(|ancestor| ancestor.id == *id) {
                return false;
            }
            Some(*id)
        }
        Principal::ShareLink { .. } => return false,
        _ => None,
    };
    // Once every restricted level has passed, inheritance resolves to either a
    // granted restriction or public, including the implicit public root.
    level(p, a, shared)
        && a.ancestors
            .iter()
            .all(|ancestor| level(p, ancestor, shared))
}

/// Applies album narrowing before photo visibility or a photo-specific link.
#[must_use]
pub fn can_view_photo(p: &Principal<'_>, ph: &PhotoAccess<'_>) -> bool {
    if admin(p) {
        return true;
    }
    if let Principal::ShareLink {
        target: ShareTarget::Photo(id),
    } = p
    {
        return *id == ph.id
            && level(p, ph.album, None)
            && ph.album.ancestors.iter().all(|a| level(p, a, None));
    }
    can_view_album(p, ph.album)
        && (ph.visibility != Visibility::Restricted || granted(p, ph.grants))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn full_principal_visibility_grant_ancestor_matrix() {
        let user = UserId::new();
        let group = GroupId::new();
        let groups = [group];
        let album_id = AlbumId::new();
        let photo_id = PhotoId::new();
        let principals = [
            Principal::Anonymous,
            Principal::User {
                id: user,
                groups: &[],
                is_admin: false,
            },
            Principal::User {
                id: UserId::new(),
                groups: &groups,
                is_admin: false,
            },
            Principal::User {
                id: UserId::new(),
                groups: &[],
                is_admin: true,
            },
            Principal::ShareLink {
                target: ShareTarget::Album(album_id),
            },
            Principal::ShareLink {
                target: ShareTarget::Photo(photo_id),
            },
            Principal::ShareLink {
                target: ShareTarget::Album(AlbumId::new()),
            },
            Principal::ShareLink {
                target: ShareTarget::Photo(PhotoId::new()),
            },
        ];
        let grants = [Grant::User(user), Grant::Group(group)];
        for (kind, principal) in principals.iter().enumerate() {
            for visibility in [
                Visibility::Public,
                Visibility::Restricted,
                Visibility::Inherit,
            ] {
                for has_grant in [false, true] {
                    for restricted in [false, true] {
                        let current_grants = if has_grant { &grants[..] } else { &[] };
                        let ancestors = [AlbumAccess {
                            id: AlbumId::new(),
                            visibility: if restricted {
                                Visibility::Restricted
                            } else {
                                Visibility::Public
                            },
                            grants: &[],
                            ancestors: &[],
                        }];
                        let mut album = AlbumAccess {
                            id: album_id,
                            visibility,
                            grants: current_grants,
                            ancestors: &ancestors,
                        };
                        let user_granted = (kind == 1 || kind == 2) && has_grant;
                        let expected_album = kind == 3
                            || (!restricted
                                && match kind {
                                    0..=2 => visibility != Visibility::Restricted || user_granted,
                                    4 => true,
                                    _ => false,
                                });
                        assert_eq!(
                            can_view_album(principal, &album),
                            expected_album,
                            "album kind={kind} visibility={visibility:?} grant={has_grant} restricted={restricted}"
                        );
                        // Isolate each photo visibility from its containing album.
                        album.visibility = Visibility::Public;
                        let photo = PhotoAccess {
                            id: photo_id,
                            album: &album,
                            visibility,
                            grants: current_grants,
                        };
                        let expected_photo = kind == 3
                            || (!restricted
                                && match kind {
                                    0..=2 => visibility != Visibility::Restricted || user_granted,
                                    4 => visibility != Visibility::Restricted,
                                    5 => true,
                                    _ => false,
                                });
                        assert_eq!(
                            can_view_photo(principal, &photo),
                            expected_photo,
                            "photo kind={kind} visibility={visibility:?} grant={has_grant} restricted={restricted}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn inheritance_and_grants_narrow_at_every_level() {
        let user_id = UserId::new();
        let user = Principal::User {
            id: user_id,
            groups: &[],
            is_admin: false,
        };
        let grants = [Grant::User(user_id)];
        let mut ancestors = [AlbumAccess {
            id: AlbumId::new(),
            visibility: Visibility::Restricted,
            grants: &grants,
            ancestors: &[],
        }];
        let album_id = AlbumId::new();
        for granted_ancestor in [true, false] {
            ancestors[0].grants = if granted_ancestor { &grants } else { &[] };
            let album = AlbumAccess {
                id: album_id,
                visibility: Visibility::Inherit,
                grants: &[],
                ancestors: &ancestors,
            };
            let photo = PhotoAccess {
                id: PhotoId::new(),
                album: &album,
                visibility: Visibility::Inherit,
                grants: &[],
            };
            assert_eq!(can_view_album(&user, &album), granted_ancestor);
            assert_eq!(can_view_photo(&user, &photo), granted_ancestor);
            assert!(!can_view_photo(&Principal::Anonymous, &photo));
        }
        let root = AlbumAccess {
            id: album_id,
            visibility: Visibility::Inherit,
            grants: &[],
            ancestors: &[],
        };
        assert!(can_view_album(&Principal::Anonymous, &root));
        let photo = PhotoAccess {
            id: PhotoId::new(),
            album: &root,
            visibility: Visibility::Inherit,
            grants: &[],
        };
        assert!(can_view_photo(&Principal::Anonymous, &photo));
    }

    #[test]
    fn every_restricted_ancestor_needs_its_own_user_or_group_grant() {
        let user_id = UserId::new();
        let group_id = GroupId::new();
        let groups = [group_id];
        for grant in [Grant::User(user_id), Grant::Group(group_id)] {
            let grants = [grant];
            let user = Principal::User {
                id: user_id,
                groups: &groups,
                is_admin: false,
            };
            let chain: Vec<_> = (0..4)
                .map(|_| AlbumAccess {
                    id: AlbumId::new(),
                    visibility: Visibility::Restricted,
                    grants: &grants,
                    ancestors: &[],
                })
                .collect();
            for missing in 0..=chain.len() {
                let mut ancestors = chain.clone();
                if let Some(ancestor) = ancestors.get_mut(missing) {
                    ancestor.grants = &[];
                }
                for visibility in [
                    Visibility::Public,
                    Visibility::Restricted,
                    Visibility::Inherit,
                ] {
                    let album = AlbumAccess {
                        id: AlbumId::new(),
                        visibility,
                        grants: &grants,
                        ancestors: &ancestors,
                    };
                    let photo = PhotoAccess {
                        id: PhotoId::new(),
                        album: &album,
                        visibility,
                        grants: &grants,
                    };
                    let expected = missing == chain.len();
                    assert_eq!(can_view_album(&user, &album), expected);
                    assert_eq!(can_view_photo(&user, &photo), expected);
                }
            }
        }
    }

    #[test]
    fn share_links_cannot_escape_target_or_bypass_other_restrictions() {
        let target = AlbumAccess {
            id: AlbumId::new(),
            visibility: Visibility::Restricted,
            grants: &[],
            ancestors: &[],
        };
        let album_link = Principal::ShareLink {
            target: ShareTarget::Album(target.id),
        };
        assert!(can_view_album(&album_link, &target));
        let ancestors = [target];
        let mut child = AlbumAccess {
            id: AlbumId::new(),
            visibility: Visibility::Inherit,
            grants: &[],
            ancestors: &ancestors,
        };
        assert!(can_view_album(&album_link, &child));
        let mut photo = PhotoAccess {
            id: PhotoId::new(),
            album: &child,
            visibility: Visibility::Inherit,
            grants: &[],
        };
        assert!(can_view_photo(&album_link, &photo));
        photo.visibility = Visibility::Restricted;
        assert!(!can_view_photo(&album_link, &photo));
        child.visibility = Visibility::Restricted;
        assert!(!can_view_album(&album_link, &child));
        let photo_link = Principal::ShareLink {
            target: ShareTarget::Photo(PhotoId::new()),
        };
        let restricted_photo = PhotoAccess {
            id: match photo_link {
                Principal::ShareLink {
                    target: ShareTarget::Photo(id),
                } => id,
                _ => PhotoId::new(),
            },
            album: &child,
            visibility: Visibility::Public,
            grants: &[],
        };
        assert!(!can_view_photo(&photo_link, &restricted_photo));
        let blocked_target = AlbumAccess {
            id: child.id,
            visibility: Visibility::Restricted,
            grants: &[],
            ancestors: &ancestors,
        };
        assert!(!can_view_album(
            &Principal::ShareLink {
                target: ShareTarget::Album(child.id)
            },
            &blocked_target
        ));
    }

    fn visibility(n: u8) -> Visibility {
        match n % 3 {
            0 => Visibility::Public,
            1 => Visibility::Restricted,
            _ => Visibility::Inherit,
        }
    }
    proptest! {
        #[test]
        fn restricting_an_ancestor_never_widens_access(
            levels in prop::collection::vec(0_u8..3, 1..12), selected in 0_usize..12,
            album_visibility in 0_u8..3, photo_visibility in 0_u8..3, kind in 0_u8..6, grant in any::<bool>()
        ) {
            let user = UserId::new();
            let group = GroupId::new();
            let groups = [group];
            let grants = [Grant::User(user), Grant::Group(group)];
            let ancestors: Vec<_> = levels.iter().map(|v| AlbumAccess { id: AlbumId::new(), visibility: visibility(*v), grants: if grant { &grants } else { &[] }, ancestors: &[] }).collect();
            let album_id = AlbumId::new();
            let photo_id = PhotoId::new();
            let principal = match kind {
                0 => Principal::Anonymous,
                1 => Principal::User { id: user, groups: &[], is_admin: false },
                2 => Principal::User { id: UserId::new(), groups: &groups, is_admin: false },
                3 => Principal::User { id: user, groups: &[], is_admin: true },
                4 => Principal::ShareLink { target: ShareTarget::Album(album_id) },
                _ => Principal::ShareLink { target: ShareTarget::Photo(photo_id) },
            };
            let check = |chain| {
                let album = AlbumAccess { id: album_id, visibility: visibility(album_visibility), grants: &grants, ancestors: chain };
                let photo = PhotoAccess { id: photo_id, album: &album, visibility: visibility(photo_visibility), grants: &grants };
                (can_view_album(&principal, &album), can_view_photo(&principal, &photo))
            };
            let before = check(&ancestors);
            let index = selected % ancestors.len();
            let mut narrowed = ancestors.clone();
            if let Some(ancestor) = narrowed.get_mut(index) { ancestor.visibility = Visibility::Restricted; }
            let after = check(&narrowed);
            prop_assert!(!after.0 || before.0);
            prop_assert!(!after.1 || before.1);
        }
        #[test]
        fn admin_always_true(levels in prop::collection::vec(0_u8..3, 0..16), av in 0_u8..3, pv in 0_u8..3) {
            let ancestors: Vec<_> = levels.iter().map(|v| AlbumAccess { id: AlbumId::new(), visibility: visibility(*v), grants: &[], ancestors: &[] }).collect();
            let album = AlbumAccess { id: AlbumId::new(), visibility: visibility(av), grants: &[], ancestors: &ancestors };
            let photo = PhotoAccess { id: PhotoId::new(), album: &album, visibility: visibility(pv), grants: &[] };
            let admin = Principal::User { id: UserId::new(), groups: &[], is_admin: true };
            prop_assert!(can_view_album(&admin, &album));
            prop_assert!(can_view_photo(&admin, &photo));
        }
    }
}
