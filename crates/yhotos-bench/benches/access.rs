#![forbid(unsafe_code)]
//! Benchmarks the allocation-free access decision at shallow and nested albums.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use yhotos_core::{
    AlbumId, GroupId, PhotoId, UserId,
    access::{AlbumAccess, Grant, PhotoAccess, Principal, Visibility, can_view_photo},
};

fn access(c: &mut Criterion) {
    let group = GroupId::new();
    let groups = [group];
    let grants = [Grant::Group(group)];
    let user = UserId::new();
    for depth in [1, 5] {
        let ancestors: Vec<_> = (1..depth)
            .map(|_| AlbumAccess {
                id: AlbumId::new(),
                visibility: Visibility::Public,
                grants: &[],
                ancestors: &[],
            })
            .collect();
        let album = AlbumAccess {
            id: AlbumId::new(),
            visibility: Visibility::Public,
            grants: &[],
            ancestors: &ancestors,
        };
        let photo = PhotoAccess {
            id: PhotoId::new(),
            album: &album,
            visibility: Visibility::Restricted,
            grants: &grants,
        };
        for (name, principal) in [
            ("anonymous", Principal::Anonymous),
            (
                "group",
                Principal::User {
                    id: user,
                    groups: &groups,
                    is_admin: false,
                },
            ),
            (
                "admin",
                Principal::User {
                    id: user,
                    groups: &[],
                    is_admin: true,
                },
            ),
        ] {
            c.bench_function(&format!("access/{name}/depth_{depth}"), |b| {
                b.iter(|| can_view_photo(black_box(&principal), black_box(&photo)));
            });
        }
    }
}
criterion_group!(benches, access);
criterion_main!(benches);
