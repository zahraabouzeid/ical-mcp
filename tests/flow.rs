use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use ical_mcp::app;
use ical_mcp::auth::oauth::challenge_of;
use ical_mcp::config::Settings;
use ical_mcp::feed::Addresses;
use reqwest::Url;
use serde_json::{Value, json};
use tower::ServiceExt;

const CALENDAR: &str = include_str!("fixtures/calendar.ics");
const REDIRECT: &str = "https://claude.ai/api/mcp/auth_callback";
const VERIFIER: &str = "a-verifier-that-is-long-enough-for-pkce-0123456789";
const HOST: &str = "testserver";

struct Server {
    router: Router,
    calendar_link: String,
    folder: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.folder);
    }
}

async fn start(name: &str) -> Server {
    let calendars = Router::new()
        .route("/calendar.ics", get(|| async { CALENDAR }))
        .route("/page.html", get(|| async { "<html></html>" }))
        .fallback(|| async { StatusCode::NOT_FOUND.into_response() });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("an address");
    tokio::spawn(async move { axum::serve(listener, calendars).await });

    let folder = std::env::temp_dir().join(format!("ical-flow-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&folder);
    let settings = Settings::from_map(&HashMap::from([
        ("PUBLIC_HOST".to_owned(), HOST.to_owned()),
        ("DATA_DIR".to_owned(), folder.display().to_string()),
    ]))
    .expect("valid settings");
    Server {
        router: app(&settings, Addresses::Any).expect("an app"),
        calendar_link: format!("http://{address}/calendar.ics"),
        folder,
    }
}

fn get_request(path: &str) -> Request<Body> {
    Request::get(path)
        .header(header::HOST, HOST)
        .body(Body::empty())
        .expect("a request")
}

fn encoded(value: &str) -> String {
    Url::parse_with_params("https://x", [("v", value)])
        .expect("url")
        .query()
        .expect("query")["v=".len()..]
        .to_owned()
}

fn form(path: &str, fields: &[(&str, &str)]) -> Request<Body> {
    let body = fields
        .iter()
        .map(|(key, value)| format!("{key}={}", encoded(value)))
        .collect::<Vec<_>>()
        .join("&");
    Request::post(path)
        .header(header::HOST, HOST)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header("sec-fetch-site", "same-origin")
        .body(Body::from(body))
        .expect("a request")
}

impl Server {
    async fn call(&self, request: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("a response");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("a body");
        (
            status,
            headers,
            String::from_utf8_lossy(&bytes).into_owned(),
        )
    }

    async fn register(&self, name: &str) -> String {
        let body = json!({"client_name": name, "redirect_uris": [REDIRECT]});
        let request = Request::post("/oauth/register")
            .header(header::HOST, HOST)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .expect("a request");
        let (status, _, text) = self.call(request).await;
        assert_eq!(status, StatusCode::CREATED, "{text}");
        let answer: Value = serde_json::from_str(&text).expect("json");
        answer["client_id"].as_str().expect("an id").to_owned()
    }

    async fn answer(
        &self,
        app: &str,
        link: &str,
        action: &str,
    ) -> (StatusCode, axum::http::HeaderMap, String) {
        let challenge = challenge_of(VERIFIER);
        self.call(form(
            "/oauth/authorize",
            &[
                ("client_id", app),
                ("redirect_uri", REDIRECT),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
                ("state", "s1"),
                ("link", link),
                ("action", action),
            ],
        ))
        .await
    }

    async fn connect(&self) -> (String, Value) {
        let app = self.register("Claude").await;
        let link = self.calendar_link.clone();
        let (status, headers, _) = self.answer(&app, &link, "allow").await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let back = Url::parse(headers[header::LOCATION].to_str().expect("text")).expect("url");
        let pairs: HashMap<_, _> = back.query_pairs().into_owned().collect();
        assert!(back.as_str().starts_with(REDIRECT));
        assert_eq!(pairs["state"], "s1");
        assert_eq!(pairs["iss"], format!("https://{HOST}"));
        let (status, _, text) = self
            .call(form(
                "/oauth/token",
                &[
                    ("grant_type", "authorization_code"),
                    ("client_id", &app),
                    ("code", &pairs["code"]),
                    ("redirect_uri", REDIRECT),
                    ("code_verifier", VERIFIER),
                ],
            ))
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        (app, serde_json::from_str(&text).expect("json"))
    }

    async fn mcp(
        &self,
        token: Option<&str>,
        method: &str,
        params: Value,
    ) -> (StatusCode, axum::http::HeaderMap, Value) {
        let mut request = Request::post("/mcp")
            .header(header::HOST, HOST)
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header(header::CONTENT_TYPE, "application/json")
            .header("mcp-protocol-version", "2025-06-18");
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let (status, headers, text) = self
            .call(
                request
                    .body(Body::from(body.to_string()))
                    .expect("a request"),
            )
            .await;
        (
            status,
            headers,
            serde_json::from_str(&text).unwrap_or(Value::Null),
        )
    }
}

fn token(tokens: &Value) -> &str {
    tokens["access_token"].as_str().expect("a token")
}

#[tokio::test]
async fn an_app_finds_the_sign_in_from_the_first_refused_request() {
    let server = start("discovery").await;
    let (status, headers, _) = server.mcp(None, "tools/list", json!({})).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let challenge = headers[header::WWW_AUTHENTICATE].to_str().expect("text");
    assert!(
        challenge.contains("/.well-known/oauth-protected-resource/mcp"),
        "{challenge}"
    );

    let (_, _, text) = server
        .call(get_request("/.well-known/oauth-protected-resource/mcp"))
        .await;
    let resource: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(resource["resource"], format!("https://{HOST}/mcp"));

    let (_, _, text) = server
        .call(get_request("/.well-known/oauth-authorization-server"))
        .await;
    let issuer: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(issuer["code_challenge_methods_supported"], json!(["S256"]));
    assert_eq!(
        issuer["authorization_endpoint"],
        format!("https://{HOST}/oauth/authorize")
    );
}

#[tokio::test]
async fn the_sign_in_page_works_on_a_phone_and_names_the_app_safely() {
    let server = start("page").await;
    let app = server.register("<b>Evil</b>").await;
    let path = format!(
        "/oauth/authorize?response_type=code&client_id={app}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&state=s1",
        encoded(REDIRECT),
        challenge_of(VERIFIER)
    );
    let (status, headers, page) = server.call(get_request(&path)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("width=device-width"));
    assert!(page.contains("&#60;b&#62;Evil&#60;/b&#62;") && !page.contains("<b>Evil"));
    assert!(
        page.contains("claude.ai"),
        "the page names where the answer goes"
    );
    assert_eq!(headers["x-frame-options"], "DENY");

    let other = format!(
        "/oauth/authorize?client_id=unknown&redirect_uri={}",
        encoded(REDIRECT)
    );
    assert_eq!(
        server.call(get_request(&other)).await.0,
        StatusCode::BAD_REQUEST
    );

    let attacker = format!(
        "/oauth/authorize?client_id={app}&redirect_uri={}&code_challenge=x&code_challenge_method=S256",
        encoded("https://attacker.example/cb")
    );
    assert_eq!(
        server.call(get_request(&attacker)).await.0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn a_wrong_link_stays_on_the_page_and_never_comes_back_in_it() {
    let server = start("wrong").await;
    let app = server.register("Claude").await;
    let not_calendar = server.calendar_link.replace("calendar.ics", "page.html");
    let (status, _, page) = server.answer(&app, &not_calendar, "allow").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(page.contains("does not lead to a calendar"), "{page}");
    assert!(!page.contains("page.html"));

    let missing = server.calendar_link.replace("calendar.ics", "gone.ics");
    assert_eq!(
        server.answer(&app, &missing, "allow").await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        server.answer(&app, "", "allow").await.0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn cancelling_goes_back_to_the_app_without_a_code() {
    let server = start("cancel").await;
    let app = server.register("Claude").await;
    let (status, headers, _) = server.answer(&app, "", "deny").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let back = headers[header::LOCATION].to_str().expect("text");
    assert!(
        back.contains("error=access_denied") && !back.contains("code="),
        "{back}"
    );
}

#[tokio::test]
async fn a_form_from_another_site_is_refused() {
    let server = start("other-site").await;
    let app = server.register("Claude").await;
    let mut request = form("/oauth/authorize", &[("client_id", &app)]);
    request
        .headers_mut()
        .insert("sec-fetch-site", "cross-site".parse().expect("a header"));
    assert_eq!(server.call(request).await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_connected_app_reads_the_calendar_with_a_valid_output() {
    let server = start("read").await;
    let (_, tokens) = server.connect().await;
    let (status, _, listed) = server
        .mcp(Some(token(&tokens)), "tools/list", json!({}))
        .await;
    assert_eq!(status, StatusCode::OK);
    let tools = listed["result"]["tools"].as_array().expect("tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["annotations"]["readOnlyHint"], true);
    let schema = tools[0]["outputSchema"].clone();

    let (status, _, called) = server
        .mcp(
            Some(token(&tokens)),
            "tools/call",
            json!({"name": "list_events", "arguments": {"from": "2026-10-19", "to": "2026-10-26"}}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let result = &called["result"];
    assert_eq!(result["isError"], false, "{result}");
    let content = &result["structuredContent"];
    jsonschema::validator_for(&schema)
        .expect("a valid schema")
        .validate(content)
        .expect("an answer inside the schema");

    let events = content["events"].as_array().expect("events");
    let titles: Vec<&str> = events
        .iter()
        .map(|e| e["title"].as_str().expect("a title"))
        .collect();
    assert_eq!(titles, ["Seminar", "41.09000 Messtechnik VO", "Seminar"]);
    assert_eq!(events[1]["start"], "2026-10-21T12:30:00+02:00");
    assert_eq!(events[1]["weekday"], "Wednesday");
    assert_eq!(events[1]["location"], "H09");
    assert_eq!(events[2]["start"], "2026-10-26T10:00:00+01:00");
    assert_eq!(content["total"], 3);
}

#[tokio::test]
async fn searching_all_day_events_and_bad_days_are_handled() {
    let server = start("tool").await;
    let (_, tokens) = server.connect().await;
    let call = |arguments: Value| {
        let token = token(&tokens).to_owned();
        let server = &server;
        async move {
            let (_, _, called) = server
                .mcp(
                    Some(&token),
                    "tools/call",
                    json!({"name": "list_events", "arguments": arguments}),
                )
                .await;
            called["result"].clone()
        }
    };
    let found =
        call(json!({"from": "2026-10-19", "to": "2026-10-26", "search": "messtechnik"})).await;
    assert_eq!(found["structuredContent"]["total"], 1);

    let holiday =
        call(json!({"from": "2026-12-22", "to": "2026-12-22", "search": "holiday"})).await;
    let event = &holiday["structuredContent"]["events"][0];
    assert_eq!(event["start"], "2026-12-21");
    assert_eq!(event["end"], "2026-12-23");
    assert_eq!(event["all_day"], true);

    for arguments in [
        json!({"from": "tomorrow"}),
        json!({"from": "2026-10-20", "to": "2026-10-19"}),
        json!({"from": "2026-01-01", "to": "2028-01-01"}),
    ] {
        assert_eq!(call(arguments).await["isError"], true);
    }
}

#[tokio::test]
async fn a_bad_token_reads_nothing_and_refreshing_replaces_the_old_one() {
    let server = start("tokens").await;
    let (app, tokens) = server.connect().await;
    assert_eq!(
        server.mcp(Some("made up"), "tools/list", json!({})).await.0,
        StatusCode::UNAUTHORIZED
    );
    let refresh = tokens["refresh_token"].as_str().expect("a refresh token");
    let refreshed = form(
        "/oauth/token",
        &[
            ("grant_type", "refresh_token"),
            ("client_id", &app),
            ("refresh_token", refresh),
        ],
    );
    let (status, _, text) = server.call(refreshed).await;
    assert_eq!(status, StatusCode::OK);
    let fresh: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(
        server
            .mcp(Some(token(&tokens)), "tools/list", json!({}))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        server
            .mcp(Some(token(&fresh)), "tools/list", json!({}))
            .await
            .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn the_link_is_not_stored_readable() {
    let server = start("stored").await;
    server.connect().await;
    let mut stored = Vec::new();
    for file in fs::read_dir(&server.folder).expect("a folder") {
        stored.extend(fs::read(file.expect("a file").path()).expect("content"));
    }
    assert!(!String::from_utf8_lossy(&stored).contains("calendar.ics"));
}

#[tokio::test]
async fn another_host_is_refused_and_the_health_check_answers() {
    let server = start("host").await;
    let (_, tokens) = server.connect().await;
    let request = Request::post("/mcp")
        .header(header::HOST, "evil.example")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {}", token(&tokens)))
        .header("mcp-protocol-version", "2025-06-18")
        .body(Body::from(
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string(),
        ))
        .expect("a request");
    assert_eq!(server.call(request).await.0, StatusCode::FORBIDDEN);
    assert_eq!(server.call(get_request("/health")).await.0, StatusCode::OK);
}
