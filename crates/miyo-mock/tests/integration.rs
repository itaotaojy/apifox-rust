//! Mock Server 集成测试：真实启动服务并通过 HTTP 请求验证。

use std::collections::HashMap;

use miyo_core::model::{FieldDoc, MockMatchItem, ResponseExample};
use miyo_mock::server::{self, MockDefinition, MockStore};

fn users_def() -> MockDefinition {
    let mut d = MockDefinition::from_endpoint("GET", "/users/{id}", None).unwrap();
    d.body_template = "{\"id\":\"{{params.id}}\",\"email\":\"{{mock.email}}\"}".into();
    d
}

fn rule_def() -> MockDefinition {
    MockDefinition {
        method: "POST".into(),
        path: "/echo".into(),
        match_query: vec![MockMatchItem {
            key: "env".into(),
            value: "prod".into(),
        }],
        match_headers: vec![MockMatchItem {
            key: "x-api-key".into(),
            value: "k1".into(),
        }],
        status: 201,
        headers: HashMap::from([("x-mock".into(), "yes".into())]),
        body_template: "{\"message\":\"rule hit\"}".into(),
        delay_ms: 0,
        fault_rate_pct: 0,
        fault_status: 500,
        priority: 5,
        source: server::MockSource::Rule,
        rule_id: None,
        source_name: "echo 规则".into(),
    }
}

#[tokio::test]
async fn mock_serves_templated_path_response() {
    let store = MockStore::new();
    store.set_definitions(vec![users_def()]);
    let server = server::start(store).await.expect("start mock server");

    let res = reqwest::get(format!("{}/users/42?x=1", server.address()))
        .await
        .expect("http get");
    assert_eq!(res.status(), 200);
    let body = res.text().await.unwrap();
    assert!(body.contains("\"id\":\"42\""), "body: {body}");
    assert!(body.contains("example.com"), "email template: {body}");
    server.stop().await;
}

#[tokio::test]
async fn mock_array_count_renders_independent_uuids_after_expansion() {
    let example = ResponseExample {
        id: uuid::Uuid::new_v4(),
        endpoint_id: uuid::Uuid::new_v4(),
        name: "200".into(),
        status: 200,
        headers: HashMap::new(),
        body: r#"{
            "data": {
                "list": [
                    {"id":"{{mock.uuid}}","name":"stable","enabled":true},
                    {"id":"discard","name":"second"}
                ],
                "total":42,
                "other":[1,2]
            },
            "message":"unchanged"
        }"#
        .into(),
        content_type: "application/json".into(),
        docs: HashMap::from([(
            "/data/list".into(),
            FieldDoc {
                mock_count: Some(3),
                ..FieldDoc::default()
            },
        )]),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let original = example.clone();
    let def = MockDefinition::from_endpoint("GET", "/array", Some(&example)).unwrap();
    assert_eq!(example, original);
    let store = MockStore::new();
    store.set_definitions(vec![def]);
    let server = server::start(store).await.expect("start mock server");
    let mut ids = std::collections::HashSet::new();
    for _ in 0..2 {
        let res = reqwest::get(format!("{}/array", server.address()))
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let body: serde_json::Value = serde_json::from_str(&res.text().await.unwrap()).unwrap();
        let list = body["data"]["list"].as_array().unwrap();
        assert_eq!(list.len(), 3);
        for item in list {
            let id = item["id"].as_str().unwrap();
            assert!(uuid::Uuid::parse_str(id).is_ok());
            assert!(ids.insert(id.to_string()), "duplicate UUID: {id}");
            assert_eq!(item["name"], "stable");
            assert_eq!(item["enabled"], true);
        }
        assert_eq!(body["data"]["total"], 42);
        assert_eq!(body["data"]["other"], serde_json::json!([1, 2]));
        assert_eq!(body["message"], "unchanged");
    }
    assert_eq!(ids.len(), 6);
    assert_eq!(example, original);
    server.stop().await;
}

#[tokio::test]
async fn mock_rule_with_query_and_header_match() {
    let store = MockStore::new();
    store.set_definitions(vec![rule_def(), users_def()]);
    let server = server::start(store).await.expect("start mock server");

    let client = reqwest::Client::new();
    // 条件不满足 → 可能命中默认 def？默认 def 无 POST /echo → 404。
    let miss = client
        .post(format!("{}/echo", server.address()))
        .send()
        .await
        .unwrap();
    assert_eq!(miss.status(), 404);

    // 条件满足 → 规则命中。
    let hit = client
        .post(format!("{}/echo?env=prod", server.address()))
        .header("x-api-key", "k1")
        .send()
        .await
        .unwrap();
    assert_eq!(hit.status(), 201);
    assert_eq!(
        hit.headers().get("x-mock").and_then(|v| v.to_str().ok()),
        Some("yes")
    );
    assert!(hit.text().await.unwrap().contains("rule hit"));
    server.stop().await;
}

#[tokio::test]
async fn mock_delays_response() {
    let mut d = users_def();
    d.delay_ms = 150;
    let store = MockStore::new();
    store.set_definitions(vec![d]);
    let server = server::start(store).await.expect("start mock server");
    let started = std::time::Instant::now();
    let res = reqwest::get(format!("{}/users/1", server.address()))
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert!(started.elapsed().as_millis() >= 120);
    server.stop().await;
}

#[tokio::test]
async fn mock_fault_injection_returns_fault_status() {
    let mut d = users_def();
    d.fault_rate_pct = 100;
    d.fault_status = 503;
    let store = MockStore::new();
    store.set_definitions(vec![d]);
    let server = server::start(store).await.expect("start mock server");
    let res = reqwest::get(format!("{}/users/1", server.address()))
        .await
        .unwrap();
    assert_eq!(res.status(), 503);
    assert!(res.text().await.unwrap().contains("故障注入"));
    server.stop().await;
}

#[tokio::test]
async fn default_json_for_endpoint_without_example() {
    let store = MockStore::new();
    store.set_definitions(vec![users_def()]);
    let server = server::start(store).await.expect("start mock server");
    // 未匹配路径 → 404 JSON。
    let res = reqwest::get(format!("{}/nope", server.address()))
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
    server.stop().await;
}
