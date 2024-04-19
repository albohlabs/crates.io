use crate::builders::{CrateBuilder, PublishBuilder};
use crate::util::{RequestHelper, TestApp};
use crates_io::schema::versions_published_by;
use diesel::{QueryDsl, RunQueryDsl};
use googletest::prelude::*;
use http::StatusCode;
use insta::assert_json_snapshot;

#[tokio::test(flavor = "multi_thread")]
async fn new_krate() {
    let (app, _, user) = TestApp::full().with_user();

    let crate_to_publish = PublishBuilder::new("foo_new", "1.0.0");
    let response = user.async_publish_crate(crate_to_publish).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_json_snapshot!(response.json(), {
        ".crate.created_at" => "[datetime]",
        ".crate.updated_at" => "[datetime]",
    });

    let crates = app.crates_from_index_head("foo_new");
    assert_json_snapshot!(crates);

    let expected_files = vec!["crates/foo_new/foo_new-1.0.0.crate", "index/fo/o_/foo_new"];
    assert_eq!(app.async_stored_files().await, expected_files);

    app.db(|conn| {
        let email: String = versions_published_by::table
            .select(versions_published_by::email)
            .first(conn)
            .unwrap();
        assert_eq!(email, "something@example.com");
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn new_krate_with_token() {
    let (app, _, _, token) = TestApp::full().with_token();

    let crate_to_publish = PublishBuilder::new("foo_new", "1.0.0");
    let response = token.async_publish_crate(crate_to_publish).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_json_snapshot!(response.json(), {
        ".crate.created_at" => "[datetime]",
        ".crate.updated_at" => "[datetime]",
    });

    let expected_files = vec!["crates/foo_new/foo_new-1.0.0.crate", "index/fo/o_/foo_new"];
    assert_eq!(app.async_stored_files().await, expected_files);
}

#[tokio::test(flavor = "multi_thread")]
async fn new_krate_weird_version() {
    let (app, _, _, token) = TestApp::full().with_token();

    let crate_to_publish = PublishBuilder::new("foo_weird", "0.0.0-pre");
    let response = token.async_publish_crate(crate_to_publish).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_json_snapshot!(response.json(), {
        ".crate.created_at" => "[datetime]",
        ".crate.updated_at" => "[datetime]",
    });

    let expected_files = vec![
        "crates/foo_weird/foo_weird-0.0.0-pre.crate",
        "index/fo/o_/foo_weird",
    ];
    assert_eq!(app.async_stored_files().await, expected_files);
}

#[tokio::test(flavor = "multi_thread")]
async fn new_krate_twice() {
    let (app, _, _, token) = TestApp::full().with_token();

    let crate_to_publish = PublishBuilder::new("foo_twice", "0.99.0");
    token.async_publish_crate(crate_to_publish).await.good();

    let crate_to_publish =
        PublishBuilder::new("foo_twice", "2.0.0").description("2.0.0 description");
    let response = token.async_publish_crate(crate_to_publish).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_json_snapshot!(response.json(), {
        ".crate.created_at" => "[datetime]",
        ".crate.updated_at" => "[datetime]",
    });

    let crates = app.crates_from_index_head("foo_twice");
    assert_json_snapshot!(crates);

    let expected_files = vec![
        "crates/foo_twice/foo_twice-0.99.0.crate",
        "crates/foo_twice/foo_twice-2.0.0.crate",
        "index/fo/o_/foo_twice",
    ];
    assert_eq!(app.async_stored_files().await, expected_files);
}

#[tokio::test(flavor = "multi_thread")]
async fn new_krate_duplicate_version() {
    let (app, _, user, token) = TestApp::full().with_token();

    app.db(|conn| {
        // Insert a crate directly into the database and then we'll try to publish the same version
        CrateBuilder::new("foo_dupe", user.as_model().id)
            .version("1.0.0")
            .expect_build(conn);
    });

    let crate_to_publish = PublishBuilder::new("foo_dupe", "1.0.0");
    let response = token.async_publish_crate(crate_to_publish).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_json_snapshot!(response.json());

    assert_that!(app.async_stored_files().await, empty());
}
