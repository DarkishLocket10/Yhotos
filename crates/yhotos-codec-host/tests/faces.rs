#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used)]
//! Real sandboxed host to verified storage, using only the medium derivative.
#[tokio::test]
async fn medium_to_private_cached_face_round_trip() {
    use yhotos_storage::{Area, DerivativeKey, Store};
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let work = store.tmp_dir().unwrap();
    let source = work.path().join("medium.jpeg");
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/images/photo.jpg");
    let bytes = std::fs::read(fixture).unwrap();
    let hash = blake3::hash(&bytes).to_hex().as_str().parse().unwrap();
    std::fs::write(&source, &bytes).unwrap();
    store
        .put_derivative(
            &DerivativeKey {
                hash: &hash,
                variant: "medium",
                format: "jpeg",
                hdr: false,
            },
            &source,
        )
        .await
        .unwrap();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://127.0.0.1/unused")
        .unwrap();
    let runner = yhotos_pipeline::Ingest::new(
        store.clone(),
        pool,
        env!("CARGO_BIN_EXE_yhotos-codec-host").into(),
    );
    let first = runner.crop_face(&hash, [0.1, 0.1, 0.5, 0.6]).await.unwrap();
    let second = runner.crop_face(&hash, [0.1, 0.1, 0.5, 0.6]).await.unwrap();
    assert_eq!(first.hash, second.hash);
    let key = DerivativeKey {
        hash: &first.hash,
        variant: "face",
        format: "jpeg",
        hdr: false,
    };
    assert!(
        store
            .exists(Area::Derivatives, &first.hash, Some(&key))
            .await
    );
    let path = store.path_derivative(&key).unwrap();
    let id = yhotos_codec::identify(&path, &yhotos_codec::Limits::default()).unwrap();
    assert_eq!((id.width, id.height), (Some(256), Some(256)));
    assert_eq!(first.bytes, std::fs::metadata(path).unwrap().len());
    assert!(
        runner
            .crop_face(&hash, [f32::NAN, 0.0, 0.5, 0.5])
            .await
            .is_err()
    );
    assert!(
        runner
            .crop_face(&hash, [0.1, 0.1, 0.105, 0.5])
            .await
            .is_err()
    );
}
