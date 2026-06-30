#[test]
fn checked_in_fixture_contract_is_satisfied() {
    for relative_path in restream::test_fixtures::REQUIRED_CHECKED_IN_FIXTURES {
        let path = restream::test_fixtures::checked_in_fixture(relative_path)
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(
            path.is_file(),
            "fixture contract path must exist: {}",
            path.display()
        );
    }
}

#[test]
fn canonical_transport_fixtures_resolve() {
    let h264 =
        restream::test_fixtures::canonical_h264_ts_fixture().unwrap_or_else(|e| panic!("{e}"));
    let h265 =
        restream::test_fixtures::canonical_h265_ts_fixture().unwrap_or_else(|e| panic!("{e}"));
    let sparse =
        restream::test_fixtures::sparse_gop_mp4_fixture().unwrap_or_else(|e| panic!("{e}"));

    assert!(h264.ends_with("test/fixtures/correctness-h264.ts"));
    assert!(h265.ends_with("test/fixtures/correctness-h265.ts"));
    assert!(sparse.ends_with("test/fixtures/sparse-gop-5s.mp4"));
}
