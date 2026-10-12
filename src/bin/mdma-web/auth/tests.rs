use super::*;

const TOKEN: &str = "00ff";

type Case<'a> = (Method, &'a str, &'a [(&'static str, &'a str)], Verdict);

fn check(method: &Method, uri: &str, headers: &[(&'static str, &str)]) -> Verdict {
    let capability = Capability::new(TOKEN.to_owned(), 6364);
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        map.append(*name, HeaderValue::from_str(value).unwrap());
    }
    capability.check(method, &uri.parse().unwrap(), &map)
}

#[test]
fn token_is_random_hex() {
    let (a, b) = (generate_token().unwrap(), generate_token().unwrap());
    assert_eq!(a.len(), 64);
    assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(a, b);
}

#[test]
fn verdicts() {
    let session = "mdma_web_6364=00ff";
    let cases: &[Case] = &[
        (Method::GET, "/?token=00ff", &[("host", "127.0.0.1:6364")], Verdict::Login("/".into())),
        (Method::GET, "/playground?x=1&token=00ff", &[("host", "localhost:6364")], Verdict::Login("/playground".into())),
        (Method::GET, "/?token=00fe", &[("host", "127.0.0.1:6364")], Verdict::Deny("invalid token")),
        (Method::GET, "/?token=00ff", &[("host", "evil.example:6364")], Verdict::Deny("unexpected Host header")),
        (Method::GET, "/", &[("host", "127.0.0.1:50081"), ("cookie", session)], Verdict::Deny("unexpected Host header")),
        (Method::GET, "/", &[], Verdict::Deny("unexpected Host header")),
        (Method::GET, "/", &[("host", "[::1]:6364"), ("cookie", session)], Verdict::Pass),
        (Method::GET, "/", &[("host", "127.0.0.1:6364"), ("cookie", "a=b; mdma_web_6364=00ff")], Verdict::Pass),
        (Method::GET, "/", &[("host", "127.0.0.1:6364"), ("cookie", "mdma_web_50081=00ff")],
            Verdict::Deny("missing session; open the URL mdma-web printed at startup")),
        (Method::GET, "/", &[("host", "127.0.0.1:6364"), ("cookie", "mdma_web_6364=00f")],
            Verdict::Deny("missing session; open the URL mdma-web printed at startup")),
        (Method::POST, "/playground/run", &[("host", "127.0.0.1:6364"), ("cookie", session), ("origin", "http://127.0.0.1:6364")], Verdict::Pass),
        (Method::POST, "/playground/run", &[("host", "127.0.0.1:6364"), ("cookie", session)], Verdict::Deny("cross-origin request")),
        (Method::POST, "/playground/run", &[("host", "127.0.0.1:6364"), ("cookie", session), ("origin", "http://localhost:6364")],
            Verdict::Deny("cross-origin request")),
        (Method::POST, "/playground/run", &[("host", "127.0.0.1:6364"), ("cookie", session), ("origin", "null")],
            Verdict::Deny("cross-origin request")),
    ];
    for (method, uri, headers, expected) in cases {
        assert_eq!(&check(method, uri, headers), expected, "{method} {uri} {headers:?}");
    }
}

#[tokio::test]
async fn guard_sets_cookie_and_hardens_every_response() {
    #[derive(Clone)]
    struct Ok200; // MOCKED: stands in for the router.
    impl Service<Request> for Ok200 {
        type Output = Response;
        type Error = Infallible;
        async fn serve(&self, _: Request) -> Result<Response, Infallible> { Ok(Response::new(Body::from("ok"))) }
    }
    let guard = Guard::new(Ok200, Capability::new(TOKEN.to_owned(), 6364));
    let request = |uri: &str, cookie: Option<&str>| {
        let mut builder = Request::builder().uri(uri).header("host", "127.0.0.1:6364");
        if let Some(cookie) = cookie { builder = builder.header("cookie", cookie); }
        builder.body(Body::empty()).unwrap()
    };

    let login = guard.serve(request("/federation?token=00ff", None)).await.unwrap();
    assert_eq!(login.status(), StatusCode::SEE_OTHER);
    assert_eq!(login.headers()[header::LOCATION], "/federation");
    assert_eq!(login.headers()[header::SET_COOKIE], "mdma_web_6364=00ff; Path=/; HttpOnly; SameSite=Strict");

    let denied = guard.serve(request("/", None)).await.unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let passed = guard.serve(request("/", Some("mdma_web_6364=00ff"))).await.unwrap();
    assert_eq!(passed.status(), StatusCode::OK);
    for response in [&login, &denied, &passed] {
        assert!(response.headers()[header::CONTENT_SECURITY_POLICY].to_str().unwrap().contains("frame-ancestors 'none'"));
        assert_eq!(response.headers()[header::REFERRER_POLICY], "no-referrer");
    }
}
