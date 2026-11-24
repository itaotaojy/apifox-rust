//! Mock Server（M8）：基于 axum 的本地 Mock 服务。
//!
//! 匹配优先级：自定义 MockRule > Endpoint ResponseExample > 默认 JSON（SPEC §16.2）。
//! 支持路径参数 `{id}`、query/header 匹配、模板变量 `{{params.x}}` 等、延迟返回。

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, Response as AxumResponse, StatusCode};
use axum::response::IntoResponse;
use axum::Router;
use miyo_core::model::{FieldDoc, MockMatchItem, MockRequestLog, MockRule, ResponseExample};
use miyo_core::AppError;
use uuid::Uuid;

/// 默认 Mock 端口。
pub const DEFAULT_MOCK_PORT: u16 = 4010;
/// 端口被占用时最多尝试的次数（4010~4029）。
pub const MAX_PORT_TRIES: u16 = 20;

/// 定义来源（决定匹配优先级）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MockSource {
    Default,
    Example,
    Rule,
}

/// 一条可匹配的 Mock 定义。
#[derive(Debug, Clone)]
pub struct MockDefinition {
    pub method: String,
    pub path: String,
    pub match_query: Vec<MockMatchItem>,
    pub match_headers: Vec<MockMatchItem>,
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body_template: String,
    pub delay_ms: u64,
    /// 故障注入：百分之多少的命中请求返回 fault_status（0 = 关闭）。
    pub fault_rate_pct: u8,
    pub fault_status: u16,
    pub priority: i64,
    pub source: MockSource,
    /// 命中的规则 id（示例 / 默认响应来源为 None）；请求日志归属用。
    pub rule_id: Option<uuid::Uuid>,
    /// 命中来源名：规则名 / 接口的 method+path（示例命中时展示用）。
    pub source_name: String,
}

impl MockDefinition {
    pub fn from_rule(rule: &MockRule) -> Self {
        MockDefinition {
            method: rule.method.as_str().to_string(),
            path: rule.path.clone(),
            match_query: rule.match_query.clone(),
            match_headers: rule.match_headers.clone(),
            status: rule.response_status,
            headers: rule.response_headers.clone(),
            body_template: rule.response_body_template.clone(),
            delay_ms: rule.delay_ms,
            fault_rate_pct: rule.fault_rate_pct,
            fault_status: rule.fault_status,
            priority: rule.priority,
            source: MockSource::Rule,
            rule_id: Some(rule.id),
            source_name: rule.name.clone(),
        }
    }

    /// 基于接口的响应示例自动 Mock；无示例时返回默认 200 JSON。
    pub fn from_endpoint(
        method: &str,
        path: &str,
        example: Option<&ResponseExample>,
    ) -> miyo_core::Result<Self> {
        Ok(if let Some(ex) = example {
            MockDefinition {
                method: method.to_string(),
                path: path.to_string(),
                match_query: Vec::new(),
                match_headers: Vec::new(),
                status: ex.status,
                headers: {
                    let mut h = ex.headers.clone();
                    h.entry("Content-Type".into())
                        .or_insert_with(|| ex.content_type.clone());
                    h
                },
                body_template: expand_mock_arrays(&ex.body, &ex.docs)?,
                delay_ms: 0,
                fault_rate_pct: 0,
                fault_status: 500,
                priority: 0,
                source: MockSource::Example,
                rule_id: None,
                source_name: format!("{method} {path}"),
            }
        } else {
            MockDefinition {
                method: method.to_string(),
                path: path.to_string(),
                match_query: Vec::new(),
                match_headers: Vec::new(),
                status: 200,
                headers: HashMap::from([("content-type".into(), "application/json".into())]),
                body_template: "{\"message\":\"Mock 默认响应\"}".into(),
                delay_ms: 0,
                fault_rate_pct: 0,
                fault_status: 500,
                priority: -10,
                source: MockSource::Default,
                rule_id: None,
                source_name: format!("{method} {path}"),
            }
        })
    }
}

const MAX_MOCK_ARRAY_COUNT: u32 = 1000;
const MAX_MOCK_OUTPUT_NODES: usize = 10_000;

pub fn expand_mock_arrays(
    body: &str,
    docs: &HashMap<String, FieldDoc>,
) -> miyo_core::Result<String> {
    if !docs.values().any(|doc| doc.mock_count.is_some()) {
        return Ok(body.to_string());
    }
    for (path, doc) in docs {
        if let Some(count) = doc.mock_count {
            if count > MAX_MOCK_ARRAY_COUNT {
                return Err(AppError::Validation(format!(
                    "Mock count（{path}）必须为 0..={MAX_MOCK_ARRAY_COUNT} 的整数，当前为 {count}"
                )));
            }
        }
    }
    let value: serde_json::Value = serde_json::from_str(body).map_err(|err| {
        AppError::Validation(format!("配置 Mock count 的响应示例必须是有效 JSON：{err}"))
    })?;
    let mut visited = 0;
    let expanded = expand_mock_value(&value, "", docs, &mut visited)?;
    Ok(serde_json::to_string(&expanded)?)
}

fn expand_mock_value(
    value: &serde_json::Value,
    path: &str,
    docs: &HashMap<String, FieldDoc>,
    visited: &mut usize,
) -> miyo_core::Result<serde_json::Value> {
    if *visited >= MAX_MOCK_OUTPUT_NODES {
        return Err(AppError::Validation(format!(
            "Mock count 展开后的 JSON 节点数不能超过 {MAX_MOCK_OUTPUT_NODES}"
        )));
    }
    *visited += 1;
    match value {
        serde_json::Value::Object(fields) => {
            let mut output = serde_json::Map::new();
            for (key, child) in fields {
                let escaped = key.replace('~', "~0").replace('/', "~1");
                let child_path = format!("{path}/{escaped}");
                let expanded = expand_mock_value(child, &child_path, docs, visited)?;
                output.insert(key.clone(), expanded);
            }
            Ok(serde_json::Value::Object(output))
        }
        serde_json::Value::Array(items) => {
            let mut output = Vec::new();
            let sample_path = format!("{path}/0");
            if let Some(count) = docs.get(path).and_then(|doc| doc.mock_count) {
                if let Some(sample) = items.first() {
                    for _ in 0..count {
                        output.push(expand_mock_value(sample, &sample_path, docs, visited)?);
                    }
                }
            } else {
                for item in items {
                    output.push(expand_mock_value(item, &sample_path, docs, visited)?);
                }
            }
            Ok(serde_json::Value::Array(output))
        }
        _ => Ok(value.clone()),
    }
}

/// 预编译路径段：字面量精确匹配，`{name}` 捕获路径参数。
#[derive(Debug, Clone)]
enum PathSeg {
    Literal(String),
    Param(String),
}

fn compile_path(template: &str) -> Vec<PathSeg> {
    template
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| {
            if let Some(name) = s.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                PathSeg::Param(name.to_string())
            } else {
                PathSeg::Literal(s.to_string())
            }
        })
        .collect()
}

/// 预编译后的路由：方法大写归一 + 路径段预切分，请求时零分配复用。
#[derive(Debug, Clone)]
struct CompiledRoute {
    def: MockDefinition,
    method_upper: String,
    segments: Vec<PathSeg>,
}

#[derive(Default)]
struct StoreInner {
    routes: Vec<CompiledRoute>,
    /// (METHOD, 段数) → 路由下标（保持插入顺序，优先级语义不变）。
    index: HashMap<(String, usize), Vec<usize>>,
}

/// 索引匹配命中：（定义，路径参数，预解析 query map）。
pub type MatchHit = (
    MockDefinition,
    HashMap<String, String>,
    HashMap<String, String>,
);

/// Mock 定义存储（可随时整体替换）。
///
/// 读多写少：`set_definitions` 时预编译 + 建索引；请求路径走读锁，
/// 按 (method, 段数) 只匹配候选分组，避免 N 条全扫与逐条切分 path。
#[derive(Clone, Default)]
pub struct MockStore {
    inner: Arc<RwLock<StoreInner>>,
    /// 最近请求环形缓冲（命中可见化 + 未匹配诊断；只在服务运行期驻内存）。
    logs: Arc<Mutex<VecDeque<MockRequestLog>>>,
}

/// 请求日志容量：只在内存里，重启即清；够覆盖一轮排查。
pub const MOCK_LOG_CAP: usize = 100;

impl MockStore {
    pub fn new() -> Self {
        MockStore {
            inner: Arc::new(RwLock::new(StoreInner::default())),
            logs: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// 记录一条请求日志（超容量淘汰最旧）。
    pub fn record_request(&self, log: MockRequestLog) {
        let mut logs = self.logs.lock().unwrap_or_else(|p| p.into_inner());
        if logs.len() >= MOCK_LOG_CAP {
            logs.pop_front();
        }
        logs.push_back(log);
    }

    /// 最近请求日志（新→旧）。
    pub fn recent_logs(&self) -> Vec<MockRequestLog> {
        self.logs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .rev()
            .cloned()
            .collect()
    }

    /// 清空请求日志。
    pub fn clear_logs(&self) {
        self.logs.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }

    pub fn set_definitions(&self, defs: Vec<MockDefinition>) {
        let mut inner = StoreInner {
            routes: Vec::with_capacity(defs.len()),
            index: HashMap::new(),
        };
        for def in defs {
            let route = CompiledRoute {
                method_upper: def.method.to_ascii_uppercase(),
                segments: compile_path(&def.path),
                def,
            };
            let key = (route.method_upper.clone(), route.segments.len());
            inner.index.entry(key).or_default().push(inner.routes.len());
            inner.routes.push(route);
        }
        *self.inner.write().unwrap_or_else(|p| p.into_inner()) = inner;
    }

    /// 索引化匹配：每请求只切分一次 path、解析一次 query；
    /// 连同解析后的 query map 一并返回，渲染阶段直接复用。
    pub fn match_request(
        &self,
        method: &str,
        path: &str,
        query: &str,
        headers: &HeaderMap,
    ) -> Option<MatchHit> {
        let guard = self.inner.read().unwrap_or_else(|p| p.into_inner());
        let req_segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let query_map = parse_query(query);
        let candidates = guard
            .index
            .get(&(method.to_ascii_uppercase(), req_segs.len()))?;
        let mut best: Option<(usize, HashMap<String, String>)> = None;
        for &idx in candidates {
            let route = &guard.routes[idx];
            let Some(vars) = match_compiled(&route.segments, &req_segs) else {
                continue;
            };
            if !query_match_map(&route.def.match_query, &query_map) {
                continue;
            }
            if !headers_match(&route.def.match_headers, headers) {
                continue;
            }
            match &best {
                None => best = Some((idx, vars)),
                Some((b, _)) => {
                    if picks(&route.def, &guard.routes[*b].def) {
                        best = Some((idx, vars));
                    }
                }
            }
        }
        // 读锁内只做下标选择；命中体 clone 一次（读锁不互斥，无写锁竞争）。
        best.map(|(idx, vars)| (guard.routes[idx].def.clone(), vars, query_map))
    }
}

/// 请求 query 解析一次，多处复用（条件匹配 + 模板渲染）。
fn parse_query(query: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for kv in query.split('&').filter(|s| !s.is_empty()) {
        let mut it = kv.splitn(2, '=');
        let k = it.next().unwrap_or("");
        let v = it.next().unwrap_or("");
        map.entry(k.to_string()).or_insert_with(|| v.to_string());
    }
    map
}

fn match_compiled(template: &[PathSeg], path: &[&str]) -> Option<HashMap<String, String>> {
    if template.len() != path.len() {
        return None;
    }
    let mut vars = HashMap::new();
    for (t, p) in template.iter().zip(path.iter()) {
        match t {
            PathSeg::Param(name) => {
                vars.insert(name.clone(), (*p).to_string());
            }
            PathSeg::Literal(lit) => {
                if lit != p {
                    return None;
                }
            }
        }
    }
    Some(vars)
}

/// 基于预解析 query map 的条件匹配（`query_match` 的零重复解析版）。
fn query_match_map(items: &[MockMatchItem], pairs: &HashMap<String, String>) -> bool {
    items.iter().all(|item| match pairs.get(&item.key) {
        Some(v) => item.value.is_empty() || *v == item.value,
        None => false,
    })
}

/// 路径模板匹配：`/users/{id}` vs `/users/1`，返回捕获的参数。
pub fn match_path(template: &str, path: &str) -> Option<HashMap<String, String>> {
    let t: Vec<&str> = template.split('/').filter(|s| !s.is_empty()).collect();
    let p: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if t.len() != p.len() {
        return None;
    }
    let mut vars = HashMap::new();
    for (tt, pp) in t.iter().zip(p.iter()) {
        if let Some(name) = tt.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            vars.insert(name.to_string(), (*pp).to_string());
        } else if tt != pp {
            return None;
        }
    }
    Some(vars)
}

/// 检查 query 条件（key=value，value 为空表示仅要求存在 key）。
pub fn query_match(items: &[MockMatchItem], query: &str) -> bool {
    query_match_map(items, &parse_query(query))
}

/// 检查 header 条件（大小写不敏感）。
pub fn headers_match(items: &[MockMatchItem], headers: &HeaderMap) -> bool {
    items.iter().all(|item| {
        headers
            .get(item.key.as_str())
            .and_then(|v| v.to_str().ok())
            .map(|v| item.value.is_empty() || v == item.value)
            .unwrap_or(false)
    })
}

/// 选择最佳匹配定义：先按来源（规则 > 示例 > 默认），再按 priority 降序。
pub fn resolve(
    defs: &[MockDefinition],
    method: &str,
    path: &str,
    query: &str,
    headers: &HeaderMap,
) -> Option<(MockDefinition, HashMap<String, String>)> {
    let mut best: Option<(MockDefinition, HashMap<String, String>)> = None;
    for def in defs {
        if !def.method.eq_ignore_ascii_case(method) {
            continue;
        }
        let Some(vars) = match_path(&def.path, path) else {
            continue;
        };
        if !query_match(&def.match_query, query) {
            continue;
        }
        if !headers_match(&def.match_headers, headers) {
            continue;
        }
        match &best {
            None => best = Some((def.clone(), vars)),
            Some((b, _)) => {
                if picks(def, b) {
                    best = Some((def.clone(), vars));
                }
            }
        }
    }
    best
}

fn picks(a: &MockDefinition, b: &MockDefinition) -> bool {
    if a.source != b.source {
        return a.source > b.source;
    }
    a.priority > b.priority
}

/// 模板渲染：支持 `{{params.id}}` `{{query.name}}` `{{headers.X-Token}}`
/// `{{mock.uuid}}` `{{mock.email}}` `{{mock.name}}` `{{mock.word}}` `{{mock.timestamp}}` `{{mock.int}}`。
pub fn render_template(
    template: &str,
    params: &HashMap<String, String>,
    query: &str,
    headers: &HeaderMap,
) -> String {
    // query 预解析一次：模板含 V 个 {{query.x}} 时原来是 O(V×Q) 重复切分。
    let query_map = parse_query(query);
    render_template_with_query(template, params, &query_map, headers)
}

/// 预解析 query map 版渲染（请求路径与 `render_template` 同语义，零重复解析）。
pub fn render_template_with_query(
    template: &str,
    params: &HashMap<String, String>,
    query: &HashMap<String, String>,
    headers: &HeaderMap,
) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let key = &after[..end];
        let value = lookup(key, params, query, headers);
        out.push_str(&value);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

fn lookup(
    key: &str,
    params: &HashMap<String, String>,
    query: &HashMap<String, String>,
    headers: &HeaderMap,
) -> String {
    if let Some(name) = key.strip_prefix("params.") {
        return params.get(name).cloned().unwrap_or_default();
    }
    if let Some(name) = key.strip_prefix("query.") {
        return query.get(name).cloned().unwrap_or_default();
    }
    if let Some(name) = key.strip_prefix("headers.") {
        return headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
    }
    if let Some(kind) = key.strip_prefix("mock.") {
        return mock_value(kind);
    }
    String::new()
}

fn mock_value(kind: &str) -> String {
    match kind {
        "uuid" => Uuid::new_v4().to_string(),
        "email" => format!("{}@example.com", simple_word().to_lowercase()),
        "name" => simple_word(),
        "word" => simple_word(),
        "timestamp" => chrono::Utc::now().timestamp().to_string(),
        "int" => rand::random_range(0..1000).to_string(),
        _ => String::new(),
    }
}

fn simple_word() -> String {
    let words = [
        "alpha", "beta", "gamma", "delta", "nova", "miyo", "mock", "demo",
    ];
    let i = rand::random_range(0..words.len());
    words[i].to_string()
}

async fn mock_handler(
    State(store): State<MockStore>,
    req: Request<Body>,
) -> axum::response::Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let headers = req.headers().clone();
    let started = Instant::now();

    let hit = store.match_request(method.as_str(), &path, &query, &headers);

    let Some((def, params, query_map)) = hit else {
        tracing::info!("[mock] {} {} → 404 未匹配", method, path);
        store.record_request(MockRequestLog {
            at: chrono::Utc::now(),
            method: method.to_string(),
            path: path.clone(),
            status: 404,
            matched: false,
            source: "unmatched".into(),
            rule_id: None,
            source_name: String::new(),
        });
        return (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "application/json")],
            "{\"message\":\"未匹配到 Mock 规则\"}",
        )
            .into_response();
    };

    if def.delay_ms > 0 {
        tracing::info!(
            "[mock] {} {} → {} (延迟 {}ms)",
            method,
            path,
            def.status,
            def.delay_ms
        );
        tokio::time::sleep(Duration::from_millis(def.delay_ms)).await;
    }

    // 故障注入：按命中比例返回故障状态码（延迟之后判定，模拟"慢且失败"）。
    if def.fault_rate_pct > 0 && rand::random_range(0..100) < def.fault_rate_pct as u32 {
        tracing::info!(
            "[mock] {} {} → 故障注入 {}（{}% 比例）",
            method,
            path,
            def.fault_status,
            def.fault_rate_pct
        );
        store.record_request(MockRequestLog {
            at: chrono::Utc::now(),
            method: method.to_string(),
            path: path.clone(),
            status: def.fault_status,
            matched: true,
            source: mock_source_key(&def.source),
            rule_id: def.rule_id,
            source_name: def.source_name.clone(),
        });
        return (
            StatusCode::from_u16(def.fault_status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            [(header::CONTENT_TYPE, "application/json")],
            format!(
                "{{\"message\":\"Mock 故障注入（{}% 比例）\"}}",
                def.fault_rate_pct
            ),
        )
            .into_response();
    }

    let body = render_template_with_query(&def.body_template, &params, &query_map, &headers);
    let mut builder = AxumResponse::builder().status(def.status);
    for (k, v) in &def.headers {
        let Ok(kn) = header::HeaderName::from_bytes(k.as_bytes()) else {
            continue;
        };
        let Ok(vv) = v.parse::<header::HeaderValue>() else {
            continue;
        };
        builder = builder.header(kn, vv);
    }
    // 已显式配置 Content-Type（示例 content_type / 用户配置的响应头）或 HTML
    // 响应时不追加默认 JSON 头，避免响应出现两条 Content-Type。
    let has_content_type = def
        .headers
        .keys()
        .any(|k| k.eq_ignore_ascii_case("content-type"))
        || def
            .headers
            .values()
            .any(|v| v.to_lowercase().contains("html"));
    if !has_content_type {
        builder = builder.header(header::CONTENT_TYPE, "application/json; charset=utf-8");
    }

    tracing::info!(
        "[mock] {} {} → {}（{} ms）",
        method,
        path,
        def.status,
        started.elapsed().as_millis()
    );
    store.record_request(MockRequestLog {
        at: chrono::Utc::now(),
        method: method.to_string(),
        path: path.clone(),
        status: def.status,
        matched: true,
        source: mock_source_key(&def.source),
        rule_id: def.rule_id,
        source_name: def.source_name.clone(),
    });
    builder
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// 命中来源 → 稳定键（前端按语言映射展示文案）。
fn mock_source_key(source: &MockSource) -> String {
    match source {
        MockSource::Rule => "rule",
        MockSource::Example => "example",
        MockSource::Default => "default",
    }
    .to_string()
}

/// 正在运行的 Mock 服务句柄。
pub struct MockServer {
    pub port: u16,
    store: MockStore,
    shutdown: tokio::sync::oneshot::Sender<()>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl MockServer {
    pub fn address(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// 运行中的定义存储：热重载时整体替换定义（读锁内 swap，在途请求不受影响）。
    pub fn store(&self) -> &MockStore {
        &self.store
    }

    /// 停止 Mock 服务（幂等）。
    pub async fn stop(mut self) {
        let _ = self.shutdown.send(());
        if let Some(h) = self.handle.take() {
            let _ = h.await;
        }
    }
}

/// 启动 Mock 服务。端口从 4010 起依次尝试（最多 `MAX_PORT_TRIES` 次）。
pub async fn start(store: MockStore) -> Result<MockServer, AppError> {
    for port in DEFAULT_MOCK_PORT..DEFAULT_MOCK_PORT + MAX_PORT_TRIES {
        let addr: SocketAddr = ([0, 0, 0, 0], port).into();
        let router = Router::new()
            .fallback(mock_handler)
            .with_state(store.clone());
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(_) => continue,
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await;
        });
        tracing::info!("[mock] Mock 服务已启动：监听 0.0.0.0:{port}（本机访问 http://127.0.0.1:{port}，局域网内用本机 IP 访问）");
        return Ok(MockServer {
            port,
            store,
            shutdown: tx,
            handle: Some(handle),
        });
    }
    Err(AppError::Mock(format!(
        "端口 {}~{} 均被占用，无法启动 Mock 服务",
        DEFAULT_MOCK_PORT,
        DEFAULT_MOCK_PORT + MAX_PORT_TRIES - 1
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(method: &str, path: &str, source: MockSource) -> MockDefinition {
        MockDefinition {
            method: method.into(),
            path: path.into(),
            match_query: vec![],
            match_headers: vec![],
            status: 200,
            headers: HashMap::new(),
            body_template: "body".into(),
            delay_ms: 0,
            fault_rate_pct: 0,
            fault_status: 500,
            priority: 0,
            source,
            rule_id: None,
            source_name: String::new(),
        }
    }

    #[test]
    fn path_params_captured() {
        let vars = match_path("/users/{id}/posts/{pid}", "/users/10/posts/99").unwrap();
        assert_eq!(vars.get("id").map(String::as_str), Some("10"));
        assert_eq!(vars.get("pid").map(String::as_str), Some("99"));
        assert!(match_path("/users/{id}", "/users/1/2").is_none());
        assert!(match_path("/users", "/users/1").is_none());
        assert_eq!(match_path("/users/{id}", "/users/7").unwrap().len(), 1);
    }

    #[test]
    fn rule_beats_example_beats_default() {
        let defs = vec![
            def("GET", "/users", MockSource::Default),
            def("GET", "/users", MockSource::Example),
            def("GET", "/users", MockSource::Rule),
        ];
        let hit = resolve(&defs, "GET", "/users", "", &HeaderMap::new()).unwrap();
        assert_eq!(hit.0.source, MockSource::Rule);
    }

    #[test]
    fn priority_desc_within_same_source() {
        let mut low = def("GET", "/users", MockSource::Example);
        low.priority = 1;
        let mut high = def("GET", "/users", MockSource::Example);
        high.priority = 9;
        let hit = resolve(&[low, high], "GET", "/users", "", &HeaderMap::new()).unwrap();
        assert_eq!(hit.0.priority, 9);
    }

    #[test]
    fn query_and_header_matching() {
        let mut d = def("GET", "/users", MockSource::Rule);
        d.match_query = vec![MockMatchItem {
            key: "a".into(),
            value: "1".into(),
        }];
        d.match_headers = vec![MockMatchItem {
            key: "x-token".into(),
            value: "abc".into(),
        }];
        assert!(resolve(&[d.clone()], "GET", "/users", "a=1", &HeaderMap::new()).is_none());
        let mut headers = HeaderMap::new();
        headers.insert("x-token", "abc".parse().unwrap());
        assert!(resolve(&[d.clone()], "GET", "/users", "a=1", &headers).is_some());
        assert!(resolve(&[d], "GET", "/users", "a=1&b=2", &headers).is_some());
        assert!(resolve(
            &[def("GET", "/users", MockSource::Rule)],
            "GET",
            "/users",
            "nope=1",
            &HeaderMap::new()
        )
        .is_some());
    }

    #[test]
    fn render_all_variable_kinds() {
        let mut headers = HeaderMap::new();
        headers.insert("x-token", "t1".parse().unwrap());
        let mut params = HashMap::new();
        params.insert("id".into(), "42".into());
        let out = render_template(
            "id={{params.id}} q={{query.name}} h={{headers.x-token}} heading={{mock.uuid}} pin={{mock.int}}",
            &params,
            "name=zhang",
            &headers,
        );
        assert!(out.contains("id=42"));
        assert!(out.contains("q=zhang"));
        assert!(out.contains("h=t1"));
        assert!(out.contains("heading="));
        assert!(out.contains("pin="));
        let no_uuid = render_template("{{mock.uuid}}", &HashMap::new(), "", &HeaderMap::new());
        assert!(uuid::Uuid::parse_str(&no_uuid).is_ok());
    }

    #[test]
    fn unknown_vars_rendered_empty() {
        let out = render_template("a {{nope.x}} b", &HashMap::new(), "", &HeaderMap::new());
        assert_eq!(out, "a  b");
    }

    fn count_docs(counts: &[(&str, u32)]) -> HashMap<String, FieldDoc> {
        counts
            .iter()
            .map(|(path, count)| {
                (
                    (*path).to_string(),
                    FieldDoc {
                        mock_count: Some(*count),
                        ..FieldDoc::default()
                    },
                )
            })
            .collect()
    }

    fn expanded(body: &str, counts: &[(&str, u32)]) -> serde_json::Value {
        serde_json::from_str(&expand_mock_arrays(body, &count_docs(counts)).unwrap()).unwrap()
    }

    #[test]
    fn no_mock_counts_preserve_body_exactly_without_parsing() {
        let docs = HashMap::from([("/list".into(), FieldDoc::default())]);
        for body in ["", "not JSON {{mock.uuid}}", " {\n  \"list\": [1, 2]\n} "] {
            assert_eq!(expand_mock_arrays(body, &HashMap::new()).unwrap(), body);
            assert_eq!(expand_mock_arrays(body, &docs).unwrap(), body);
        }
    }

    #[test]
    fn mock_arrays_duplicate_first_sample_only_and_leave_other_fields_stable() {
        let body = r#"{"data":{"list":[{"id":"{{mock.uuid}}","name":"sample"},{"name":"discard"}],"other":[1,2]},"ok":true}"#;
        let actual = expanded(body, &[("/data/list", 3)]);
        let sample = serde_json::json!({"id": "{{mock.uuid}}", "name": "sample"});
        assert_eq!(
            actual["data"]["list"],
            serde_json::json!([sample, sample, sample])
        );
        assert_eq!(actual["data"]["other"], serde_json::json!([1, 2]));
        assert_eq!(actual["ok"], true);
        assert_eq!(expanded("[1,2]", &[("", 3)]), serde_json::json!([1, 1, 1]));
    }

    #[test]
    fn nested_mock_arrays_use_sample_paths_for_every_repeated_parent() {
        let body = r#"{"groups":[{"name":"first","rows":[{"tags":["a","b"]},{"tags":["discard"]}]},{"name":"discard","rows":[] }]}"#;
        let actual = expanded(
            body,
            &[
                ("/groups", 2),
                ("/groups/0/rows", 3),
                ("/groups/0/rows/0/tags", 2),
            ],
        );
        let row = serde_json::json!({"tags": ["a", "a"]});
        let group = serde_json::json!({"name": "first", "rows": [row, row, row]});
        assert_eq!(actual, serde_json::json!({"groups": [group, group]}));
    }

    #[test]
    fn unconfigured_arrays_keep_all_items_with_nested_sample_metadata() {
        let body = r#"{"groups":[{"name":"first","rows":[{"tags":[1,9]},{"tags":[2]}]},{"name":"second","rows":[{"tags":[3,8]}]}]}"#;
        let actual = expanded(body, &[("/groups/0/rows/0/tags", 2)]);
        assert_eq!(
            actual,
            serde_json::json!({"groups": [
                {"name": "first", "rows": [{"tags": [1,1]}, {"tags": [2,2]}]},
                {"name": "second", "rows": [{"tags": [3,3]}]}
            ]})
        );
    }

    #[test]
    fn mock_array_paths_escape_slashes_tildes_and_empty_keys() {
        let body = r#"{"a/b":{"~list":[{"~1":[5,9],"":[true]}]}}"#;
        let actual = expanded(
            body,
            &[
                ("/a~1b/~0list", 2),
                ("/a~1b/~0list/0/~01", 3),
                ("/a~1b/~0list/0/", 0),
            ],
        );
        let item = serde_json::json!({"~1": [5,5,5], "": []});
        assert_eq!(actual, serde_json::json!({"a/b": {"~list": [item, item]}}));
    }

    #[test]
    fn zero_counts_clear_arrays_and_empty_arrays_have_no_sample() {
        let body = r#"{"list":[1,2],"empty":[],"stable":[3,4]}"#;
        let actual = expanded(body, &[("/list", 0), ("/empty", 1000)]);
        assert_eq!(
            actual,
            serde_json::json!({"list": [], "empty": [], "stable": [3,4]})
        );
    }

    #[test]
    fn stale_and_nonarray_mock_paths_are_ignored() {
        let body = r#"{"count":42,"list":[{"nested":[1,2]}, {"nested":[3]}]}"#;
        let actual = expanded(
            body,
            &[("/missing", 3), ("/count", 2), ("/list/1/nested", 4)],
        );
        assert_eq!(
            actual,
            serde_json::from_str::<serde_json::Value>(body).unwrap()
        );
    }

    #[test]
    fn mock_counts_reject_invalid_json_and_counts_over_limit() {
        for count in [1001, u32::MAX] {
            let err = expand_mock_arrays("[1]", &count_docs(&[("", count)])).unwrap_err();
            assert!(matches!(err, AppError::Validation(ref message) if message.contains("1000")));
        }
        for body in ["", "text", "{", r#"{"list":[{{mock.int}}]}"#] {
            let err = expand_mock_arrays(body, &count_docs(&[("/list", 0)])).unwrap_err();
            assert!(matches!(err, AppError::Validation(ref message) if message.contains("JSON")));
        }
        assert_eq!(
            expanded("[7]", &[("", 1000)]).as_array().unwrap().len(),
            1000
        );
    }

    #[test]
    fn mock_output_node_limit_counts_containers_and_unchanged_items() {
        let docs = count_docs(&[("/stale", 1)]);
        let at_limit = serde_json::to_string(&vec![serde_json::Value::Null; 9999]).unwrap();
        assert!(expand_mock_arrays(&at_limit, &docs).is_ok());
        let over_limit = serde_json::to_string(&vec![serde_json::Value::Null; 10_000]).unwrap();
        let err = expand_mock_arrays(&over_limit, &docs).unwrap_err();
        assert!(matches!(err, AppError::Validation(ref message) if message.contains("10000")));
        assert_eq!(
            expand_mock_arrays(&over_limit, &HashMap::new()).unwrap(),
            over_limit
        );
        assert_eq!(expanded(&over_limit, &[("", 0)]), serde_json::json!([]));
    }

    #[test]
    fn nested_amplification_stops_at_output_node_limit() {
        let docs = count_docs(&[("", 1000), ("/0", 1000)]);
        let value = serde_json::json!([[null]]);
        let mut visited = 0;
        let err = expand_mock_value(&value, "", &docs, &mut visited).unwrap_err();
        assert_eq!(visited, MAX_MOCK_OUTPUT_NODES);
        assert!(matches!(err, AppError::Validation(ref message) if message.contains("10000")));
        assert!(expand_mock_arrays("[[null]]", &docs).is_err());
    }

    #[test]
    fn from_endpoint_expands_without_mutating_example_and_propagates_errors() {
        let mut ex = ResponseExample {
            id: Uuid::new_v4(),
            endpoint_id: Uuid::new_v4(),
            name: "200".into(),
            status: 200,
            headers: HashMap::new(),
            body: " { \"list\": [1, 2] } ".into(),
            content_type: "application/json".into(),
            docs: count_docs(&[("/list", 3)]),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let original = ex.clone();
        let def = MockDefinition::from_endpoint("GET", "/list", Some(&ex)).unwrap();
        assert_eq!(ex, original);
        assert_eq!(def.body_template, r#"{"list":[1,1,1]}"#);
        ex.docs = count_docs(&[("/list", 1001)]);
        assert!(matches!(
            MockDefinition::from_endpoint("GET", "/list", Some(&ex)),
            Err(AppError::Validation(_))
        ));
        ex.body = "[[null]]".into();
        ex.docs = count_docs(&[("", 1000), ("/0", 1000)]);
        assert!(matches!(
            MockDefinition::from_endpoint("GET", "/list", Some(&ex)),
            Err(AppError::Validation(_))
        ));
    }

    #[test]
    fn from_endpoint_default_and_example() {
        let def = MockDefinition::from_endpoint("GET", "/ping", None).unwrap();
        assert_eq!(def.source, MockSource::Default);
        assert_eq!(def.status, 200);
        assert!(def.body_template.contains("Mock 默认响应"));
        let ex = ResponseExample {
            id: Uuid::new_v4(),
            endpoint_id: Uuid::new_v4(),
            name: "200".into(),
            status: 201,
            headers: HashMap::new(),
            body: "{\"ok\":true}".into(),
            content_type: "application/json".into(),
            docs: HashMap::new(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let d = MockDefinition::from_endpoint("POST", "/echo", Some(&ex)).unwrap();
        assert_eq!(d.status, 201);
        assert_eq!(d.source, MockSource::Example);
        assert!(d.headers.contains_key("content-type") || d.headers.contains_key("Content-Type"));
    }
}

#[cfg(test)]
mod log_tests {
    use super::*;

    fn log(method: &str, status: u16, matched: bool) -> MockRequestLog {
        MockRequestLog {
            at: chrono::Utc::now(),
            method: method.into(),
            path: "/p".into(),
            status,
            matched,
            source: if matched { "rule" } else { "unmatched" }.into(),
            rule_id: None,
            source_name: String::new(),
        }
    }

    #[test]
    fn ring_buffer_records_newest_first_and_evicts_oldest() {
        let store = MockStore::new();
        for i in 0..(MOCK_LOG_CAP + 10) {
            store.record_request(log("GET", 200 + (i % 3) as u16, true));
        }
        let logs = store.recent_logs();
        assert_eq!(logs.len(), MOCK_LOG_CAP);
        // 新→旧：最旧 10 条（i 0..10）被淘汰
        assert_eq!(logs[0].status, 200 + ((MOCK_LOG_CAP + 9) % 3) as u16);
        assert_eq!(logs.last().unwrap().status, 200 + (10 % 3) as u16);
    }

    #[test]
    fn clear_logs_empties_buffer() {
        let store = MockStore::new();
        store.record_request(log("POST", 404, false));
        assert_eq!(store.recent_logs().len(), 1);
        store.clear_logs();
        assert!(store.recent_logs().is_empty());
    }

    #[test]
    fn mock_source_keys_are_stable() {
        assert_eq!(mock_source_key(&MockSource::Rule), "rule");
        assert_eq!(mock_source_key(&MockSource::Example), "example");
        assert_eq!(mock_source_key(&MockSource::Default), "default");
    }
}
