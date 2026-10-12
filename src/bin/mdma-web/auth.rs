// Every request needs the startup token (then a cookie), a loopback Host, and
// a same-origin Origin for writes.
use rama::Service;
use rama::error::BoxError;
use rama::http::header::{self, HeaderMap, HeaderValue};
use rama::http::{Body, Method, Request, Response, StatusCode};
use rama::net::uri::Uri;
use std::borrow::Cow;
use std::convert::Infallible;
use std::sync::Arc;

pub(super) fn generate_token() -> Result<String, BoxError> {
    let mut token = [0_u8; 32];
    aws_lc_rs::rand::fill(&mut token).map_err(|_| "system random number generator failed")?;
    Ok(hex::encode(token))
}

#[derive(Debug, Clone)]
pub(super) struct Capability {
    token: Arc<str>,
    cookie: Arc<str>,
    hosts: Arc<[String]>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    Pass,
    Login(String),
    Deny(&'static str),
}

impl Capability {
    pub fn new(token: String, port: u16) -> Self {
        let hosts = [format!("127.0.0.1:{port}"), format!("localhost:{port}"), format!("[::1]:{port}")];
        Self { token: token.into(), cookie: format!("mdma_web_{port}").into(), hosts: hosts.into() }
    }

    pub fn check(&self, method: &Method, uri: &Uri, headers: &HeaderMap) -> Verdict {
        let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).map(Cow::Borrowed)
            .or_else(|| uri.authority().map(|a| Cow::Owned(a.to_string())));
        let Some(host) = host.filter(|h| self.hosts.iter().any(|allowed| allowed.eq_ignore_ascii_case(h))) else {
            return Verdict::Deny("unexpected Host header");
        };
        if !matches!(*method, Method::GET | Method::HEAD) {
            let origin = headers.get(header::ORIGIN).and_then(|h| h.to_str().ok());
            if origin.is_none_or(|origin| !origin.strip_prefix("http://").is_some_and(|o| o.eq_ignore_ascii_case(&host))) {
                return Verdict::Deny("cross-origin request");
            }
        }
        let query = uri.query_or_empty();
        if let Some(offered) = query.split('&').find_map(|kv| kv.strip_prefix("token=")) {
            return if same(offered, &self.token) { Verdict::Login(uri.path_or_root().into_owned()) } else { Verdict::Deny("invalid token") };
        }
        let presented = headers.get_all(header::COOKIE).iter()
            .filter_map(|h| h.to_str().ok())
            .flat_map(|h| h.split(';'))
            .filter_map(|pair| pair.trim().split_once('='))
            .any(|(name, value)| name == &*self.cookie && same(value, &self.token));
        if presented { Verdict::Pass } else { Verdict::Deny("missing session; open the URL mdma-web printed at startup") }
    }

    fn login(&self, path: &str) -> Response {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::SEE_OTHER;
        let cookie = format!("{}={}; Path=/; HttpOnly; SameSite=Strict", self.cookie, self.token);
        let headers = response.headers_mut();
        headers.insert(header::LOCATION, HeaderValue::from_str(path).unwrap_or(HeaderValue::from_static("/")));
        headers.insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).expect("hex token and port form a valid header"));
        response
    }
}

/// Constant-time comparison.
fn same(offered: &str, token: &str) -> bool {
    offered.len() == token.len() && offered.bytes().zip(token.bytes()).fold(0, |acc, (a, b)| acc | (a ^ b)) == 0
}

fn forbidden(reason: &'static str) -> Response {
    let mut response = Response::new(Body::from(format!("mdma-web: {reason}\n")));
    *response.status_mut() = StatusCode::FORBIDDEN;
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    response
}

const CSP: &str = "default-src 'self'; script-src 'self' 'unsafe-eval'; style-src 'self' 'unsafe-inline'; \
    img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

fn harden(headers: &mut HeaderMap) {
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
}

#[derive(Debug, Clone)]
pub(super) struct Guard<S> {
    inner: S,
    capability: Capability,
}

impl<S> Guard<S> {
    pub const fn new(inner: S, capability: Capability) -> Self {
        Self { inner, capability }
    }
}

impl<S> Service<Request> for Guard<S>
where
    S: Service<Request, Output = Response, Error = Infallible>,
{
    type Output = Response;
    type Error = Infallible;

    async fn serve(&self, request: Request) -> Result<Response, Infallible> {
        let mut response = match self.capability.check(request.method(), request.uri(), request.headers()) {
            Verdict::Pass => self.inner.serve(request).await?,
            Verdict::Login(path) => self.capability.login(&path),
            Verdict::Deny(reason) => forbidden(reason),
        };
        harden(response.headers_mut());
        Ok(response)
    }
}

#[cfg(test)]
#[path = "auth/tests.rs"]
mod tests;
