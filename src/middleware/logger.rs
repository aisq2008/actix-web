//! For middleware documentation, see [`Logger`].

use std::{
    collections::HashSet,
    convert::TryFrom,
    env,
    fmt::{self, Display as _},
    future::Future,
    marker::PhantomData,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

use actix_service::{Service, Transform};
use actix_utils::future::{ok, Ready};
use bytes::Bytes;
use futures_core::ready;
use log::{debug, warn};
use regex::{Regex, RegexSet};
use time::OffsetDateTime;

use crate::{
    dev::{BodySize, MessageBody, ResponseBody},
    http::{HeaderName, StatusCode},
    service::{ServiceRequest, ServiceResponse},
    Error, HttpResponse, Result,
};

/// Middleware for logging request and response summaries to the terminal.
///
/// This middleware uses the `log` crate to output information. Enable `log`'s output for the
/// "actix_web" scope using [`env_logger`](https://docs.rs/env_logger) or similar crate.
///
/// # Default Format
/// The [`default`](Logger::default) Logger uses the following format:
///
/// ```plain
/// %a "%r" %s %b "%{Referer}i" "%{User-Agent}i" %T
///
/// Example Output:
/// 127.0.0.1:54278 "GET /test HTTP/1.1" 404 20 "-" "HTTPie/2.2.0" 0.001074
/// ```
///
/// # Examples
/// ```
/// use actix_web::{middleware::Logger, App};
///
/// // access logs are printed with the INFO level so ensure it is enabled by default
/// env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));
///
/// let app = App::new()
///     // .wrap(Logger::default())
///     .wrap(Logger::new("%a %{User-Agent}i"));
/// ```
///
/// # Format
/// Variable | Description
/// -------- | -----------
/// `%%` | The percent sign
/// `%a` | Peer IP address (or IP address of reverse proxy if used)
/// `%t` | Time when the request started processing (in RFC 3339 format)
/// `%r` | First line of request (Example: `GET /test HTTP/1.1`)
/// `%s` | Response status code
/// `%b` | Size of response in bytes, including HTTP headers
/// `%T` | Time taken to serve the request, in seconds to 6 decimal places
/// `%D` | Time taken to serve the request, in milliseconds
/// `%U` | Request URL
/// `%{r}a` | "Real IP" remote address **\***
/// `%{FOO}i` |  `request.headers["FOO"]`
/// `%{FOO}o` | `response.headers["FOO"]`
/// `%{FOO}e` | `env_var["FOO"]`
/// `%{FOO}xi` | [Custom request replacement](Logger::custom_request_replace) labelled "FOO"
///
/// # Security
/// **\*** "Real IP" remote address is calculated using
/// [`ConnectionInfo::realip_remote_addr()`](crate::dev::ConnectionInfo::realip_remote_addr())
///
/// If you use this value, ensure that all requests come from trusted hosts. Otherwise, it is
/// trivial for the remote client to falsify their source IP address.
#[derive(Debug)]
pub struct Logger(Rc<Inner>);

#[derive(Debug, Clone)]
struct Inner {
    format: Format,
    exclude: HashSet<String>,
    exclude_regex: RegexSet,
}

impl Logger {
    /// Create `Logger` middleware with the specified `format`.
    pub fn new(format: &str) -> Logger {
        Logger(Rc::new(Inner {
            format: Format::new(format),
            exclude: HashSet::new(),
            exclude_regex: RegexSet::empty(),
        }))
    }

    /// Ignore and do not log access info for specified path.
    pub fn exclude<T: Into<String>>(mut self, path: T) -> Self {
        Rc::get_mut(&mut self.0)
            .unwrap()
            .exclude
            .insert(path.into());
        self
    }

    /// Ignore and do not log access info for paths that match regex.
    pub fn exclude_regex<T: Into<String>>(mut self, path: T) -> Self {
        let inner = Rc::get_mut(&mut self.0).unwrap();
        let mut patterns = inner.exclude_regex.patterns().to_vec();
        patterns.push(path.into());
        let regex_set = RegexSet::new(patterns).unwrap();
        inner.exclude_regex = regex_set;
        self
    }

    /// Register a function that receives a ServiceRequest and returns a String for use in the
    /// log line. The label passed as the first argument should match a replacement substring in
    /// the logger format like `%{label}xi`.
    ///
    /// It is convention to print "-" to indicate no output instead of an empty string.
    ///
    /// # Example
    /// ```
    /// # use actix_web::{http::HeaderValue, middleware::Logger};
    /// # fn parse_jwt_id (_req: Option<&HeaderValue>) -> String { "jwt_uid".to_owned() }
    /// Logger::new("example %{JWT_ID}xi")
    ///     .custom_request_replace("JWT_ID", |req| parse_jwt_id(req.headers().get("Authorization")));
    /// ```
    pub fn custom_request_replace(
        mut self,
        label: &str,
        f: impl Fn(&ServiceRequest) -> String + 'static,
    ) -> Self {
        let inner = Rc::get_mut(&mut self.0).unwrap();

        let ft = inner.format.0.iter_mut().find(
            |ft| matches!(ft, FormatText::CustomRequest(unit_label, _) if label == unit_label),
        );

        if let Some(FormatText::CustomRequest(_, request_fn)) = ft {
            // replace into None or previously registered fn using same label
            request_fn.replace(CustomRequestFn {
                inner_fn: Rc::new(f),
            });
        } else {
            // non-printed request replacement function diagnostic
            debug!(
                "Attempted to register custom request logging function for nonexistent label: {}",
                label
            );
        }

        self
    }
}

impl Default for Logger {
    /// Create `Logger` middleware with format:
    ///
    /// ```plain
    /// %a "%r" %s %b "%{Referer}i" "%{User-Agent}i" %T
    /// ```
    fn default() -> Logger {
        Logger(Rc::new(Inner {
            format: Format::default(),
            exclude: HashSet::new(),
            exclude_regex: RegexSet::empty(),
        }))
    }
}

impl<S, B> Transform<S, ServiceRequest> for Logger
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    B: MessageBody,
{
    type Response = ServiceResponse<StreamLog<B>>;
    type Error = Error;
    type InitError = ();
    type Transform = LoggerMiddleware<S>;
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        for unit in &self.0.format.0 {
            // missing request replacement function diagnostic
            if let FormatText::CustomRequest(label, None) = unit {
                warn!(
                    "No custom request replacement function was registered for label \"{}\".",
                    label
                );
            }
        }

        ok(LoggerMiddleware {
            service,
            inner: self.0.clone(),
        })
    }
}

/// Logger middleware service.
pub struct LoggerMiddleware<S> {
    inner: Rc<Inner>,
    service: S,
}

impl<S, B> Service<ServiceRequest> for LoggerMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    B: MessageBody,
{
    type Response = ServiceResponse<StreamLog<B>>;
    type Error = Error;
    type Future = LoggerResponse<S, B>;

    actix_service::forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        if self.inner.exclude.contains(req.path())
            || self.inner.exclude_regex.is_match(req.path())
        {
            LoggerResponse {
                fut: self.service.call(req),
                format: None,
                time: OffsetDateTime::now_utc(),
                _phantom: PhantomData,
            }
        } else {
            let now = OffsetDateTime::now_utc();
            let mut format = self.inner.format.clone();

            for unit in &mut format.0 {
                unit.render_request(now, &req);
            }
            LoggerResponse {
                fut: self.service.call(req),
                format: Some(format),
                time: now,
                _phantom: PhantomData,
            }
        }
    }
}

#[pin_project::pin_project]
pub struct LoggerResponse<S, B>
where
    B: MessageBody,
    S: Service<ServiceRequest>,
{
    #[pin]
    fut: S::Future,
    time: OffsetDateTime,
    format: Option<Format>,
    _phantom: PhantomData<B>,
}

impl<S, B> Future for LoggerResponse<S, B>
where
    B: MessageBody,
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
{
    type Output = Result<ServiceResponse<StreamLog<B>>, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        let res = match ready!(this.fut.poll(cx)) {
            Ok(res) => res,
            Err(e) => return Poll::Ready(Err(e)),
        };

        if let Some(error) = res.response().error() {
            if res.response().head().status != StatusCode::INTERNAL_SERVER_ERROR {
                debug!("Error in response: {:?}", error);
            }
        }

        if let Some(ref mut format) = this.format {
            for unit in &mut format.0 {
                unit.render_response(res.response());
            }
        }

        let time = *this.time;
        let format = this.format.take();

        Poll::Ready(Ok(res.map_body(move |_, body| {
            ResponseBody::Body(StreamLog {
                body,
                time,
                format,
                size: 0,
            })
        })))
    }
}

use pin_project::{pin_project, pinned_drop};

#[pin_project(PinnedDrop)]
pub struct StreamLog<B> {
    #[pin]
    body: ResponseBody<B>,
    format: Option<Format>,
    size: usize,
    time: OffsetDateTime,
}

#[pinned_drop]
impl<B> PinnedDrop for StreamLog<B> {
    fn drop(self: Pin<&mut Self>) {
        if let Some(ref format) = self.format {
            let render = |fmt: &mut fmt::Formatter<'_>| {
                for unit in &format.0 {
                    unit.render(fmt, self.size, self.time)?;
                }
                Ok(())
            };
            log::info!("{}", FormatDisplay(&render));
        }
    }
}

impl<B> MessageBody for StreamLog<B>
where
    B: MessageBody,
    B::Error: Into<Error>,
{
    type Error = Error;

    fn size(&self) -> BodySize {
        self.body.size()
    }

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Self::Error>>> {
        let this = self.project();
        match this.body.poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                *this.size += chunk.len();
                Poll::Ready(Some(Ok(chunk)))
            }
            val => val,
        }
    }
}

/// A formatting style for the `Logger` consisting of multiple concatenated `FormatText` items.
#[derive(Debug, Clone)]
struct Format(Vec<FormatText>);

impl Default for Format {
    /// Return the default formatting style for the `Logger`:
    fn default() -> Format {
        Format::new(r#"%a "%r" %s %b "%{Referer}i" "%{User-Agent}i" %T"#)
    }
}

impl Format {
    /// Create a `Format` from a format string.
    ///
    /// Returns `None` if the format string syntax is incorrect.
    pub fn new(s: &str) -> Format {
        log::trace!("Access log format: {}", s);
        let fmt = Regex::new(r"%(\{([A-Za-z0-9\-_]+)\}([aioe]|xi)|[%atPrUsbTD]?)").unwrap();

        let mut idx = 0;
        let mut results = Vec::new();
        for cap in fmt.captures_iter(s) {
            let m = cap.get(0).unwrap();
            let pos = m.start();
            if idx != pos {
                results.push(FormatText::Str(s[idx..pos].to_owned()));
            }
            idx = m.end();

            if let Some(key) = cap.get(2) {
                results.push(match cap.get(3).unwrap().as_str() {
                    "a" => {
                        if key.as_str() == "r" {
                            FormatText::RealIpRemoteAddr
                        } else {
                            unreachable!()
                        }
                    }
                    "i" => {
                        FormatText::RequestHeader(HeaderName::try_from(key.as_str()).unwrap())
                    }
                    "o" => {
                        FormatText::ResponseHeader(HeaderName::try_from(key.as_str()).unwrap())
                    }
                    "e" => FormatText::EnvironHeader(key.as_str().to_owned()),
                    "xi" => FormatText::CustomRequest(key.as_str().to_owned(), None),
                    _ => unreachable!(),
                })
            } else {
                let m = cap.get(1).unwrap();
                results.push(match m.as_str() {
                    "%" => FormatText::Percent,
                    "a" => FormatText::RemoteAddr,
                    "t" => FormatText::RequestTime,
                    "r" => FormatText::RequestLine,
                    "s" => FormatText::ResponseStatus,
                    "b" => FormatText::ResponseSize,
                    "U" => FormatText::UrlPath,
                    "T" => FormatText::Time,
                    "D" => FormatText::TimeMillis,
                    _ => FormatText::Str(m.as_str().to_owned()),
                });
            }
        }
        if idx != s.len() {
            results.push(FormatText::Str(s[idx..].to_owned()));
        }

        Format(results)
    }
}

/// A string of text to be logged.
///
/// This is either one of the data fields supported by the `Logger`, or a custom `String`.
#[non_exhaustive]
#[derive(Debug, Clone)]
enum FormatText {
    Str(String),
    Percent,
    RequestLine,
    RequestTime,
    ResponseStatus,
    ResponseSize,
    Time,
    TimeMillis,
    RemoteAddr,
    RealIpRemoteAddr,
    UrlPath,
    RequestHeader(HeaderName),
    ResponseHeader(HeaderName),
    EnvironHeader(String),
    CustomRequest(String, Option<CustomRequestFn>),
}

#[derive(Clone)]
struct CustomRequestFn {
    inner_fn: Rc<dyn Fn(&ServiceRequest) -> String>,
}

impl CustomRequestFn {
    fn call(&self, req: &ServiceRequest) -> String {
        (self.inner_fn)(req)
    }
}

impl fmt::Debug for CustomRequestFn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("custom_request_fn")
    }
}

impl FormatText {
    fn render(
        &self,
        fmt: &mut fmt::Formatter<'_>,
        size: usize,
        entry_time: OffsetDateTime,
    ) -> Result<(), fmt::Error> {
        match self {
            FormatText::Str(ref string) => fmt.write_str(string),
            FormatText::Percent => "%".fmt(fmt),
            FormatText::ResponseSize => size.fmt(fmt),
            FormatText::Time => {
                let rt = OffsetDateTime::now_utc() - entry_time;
                let rt = rt.as_seconds_f64();
                fmt.write_fmt(format_args!("{:.6}", rt))
            }
            FormatText::TimeMillis => {
                let rt = OffsetDateTime::now_utc() - entry_time;
                let rt = (rt.whole_nanoseconds() as f64) / 1_000_000.0;
                fmt.write_fmt(format_args!("{:.6}", rt))
            }
            FormatText::EnvironHeader(ref name) => {
                if let Ok(val) = env::var(name) {
                    fmt.write_fmt(format_args!("{}", val))
                } else {
                    "-".fmt(fmt)
                }
            }
            _ => Ok(()),
        }
    }

    fn render_response<B>(&mut self, res: &HttpResponse<B>) {
        match self {
            FormatText::ResponseStatus => {
                *self = FormatText::Str(format!("{}", res.status().as_u16()))
            }
            FormatText::ResponseHeader(ref name) => {
                let s = if let Some(val) = res.headers().get(name) {
                    if let Ok(s) = val.to_str() {
                        s
                    } else {
                        "-"
                    }
                } else {
                    "-"
                };
                *self = FormatText::Str(s.to_string())
            }
            _ => {}
        }
    }

    fn render_request(&mut self, now: OffsetDateTime, req: &ServiceRequest) {
        match self {
            FormatText::RequestLine => {
                *self = if req.query_string().is_empty() {
                    FormatText::Str(format!(
                        "{} {} {:?}",
                        req.method(),
                        req.path(),
                        req.version()
                    ))
                } else {
                    FormatText::Str(format!(
                        "{} {}?{} {:?}",
                        req.method(),
                        req.path(),
                        req.query_string(),
                        req.version()
                    ))
                };
            }
            FormatText::UrlPath => *self = FormatText::Str(req.path().to_string()),
            FormatText::RequestTime => *self = FormatText::Str(now.format("%Y-%m-%dT%H:%M:%S")),
            FormatText::RequestHeader(ref name) => {
                let s = if let Some(val) = req.headers().get(name) {
                    if let Ok(s) = val.to_str() {
                        s
                    } else {
                        "-"
                    }
                } else {
                    "-"
                };
                *self = FormatText::Str(s.to_string());
            }
            FormatText::RemoteAddr => {
                let s = if let Some(ref peer) = req.connection_info().remote_addr() {
                    FormatText::Str((*peer).to_string())
                } else {
                    FormatText::Str("-".to_string())
                };
                *self = s;
            }
            FormatText::RealIpRemoteAddr => {
                let s = if let Some(remote) = req.connection_info().realip_remote_addr() {
                    FormatText::Str(remote.to_string())
                } else {
                    FormatText::Str("-".to_string())
                };
                *self = s;
            }
            FormatText::CustomRequest(_, request_fn) => {
                let s = match request_fn {
                    Some(f) => FormatText::Str(f.call(req)),
                    None => FormatText::Str("-".to_owned()),
                };

                *self = s;
            }
            _ => {}
        }
    }
}

/// Converter to get a String from something that writes to a Formatter.
pub(crate) struct FormatDisplay<'a>(
    &'a dyn Fn(&mut fmt::Formatter<'_>) -> Result<(), fmt::Error>,
);

impl<'a> fmt::Display for FormatDisplay<'a> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        (self.0)(fmt)
    }
}

#[cfg(test)]
mod tests {
    use actix_service::{IntoService, Service, Transform};
    use actix_utils::future::ok;

    use super::*;
    use crate::http::{header, StatusCode};
    use crate::test::{self, TestRequest};

    #[actix_rt::test]
    async fn test_logger() {
        let srv = |req: ServiceRequest| {
            ok(req.into_response(
                HttpResponse::build(StatusCode::OK)
                    .insert_header(("X-Test", "ttt"))
                    .finish(),
            ))
        };
        let logger = Logger::new("%% %{User-Agent}i %{X-Test}o %{HOME}e %D test");

        let srv = logger.new_transform(srv.into_service()).await.unwrap();

        let req = TestRequest::default()
            .insert_header((
                header::USER_AGENT,
                header::HeaderValue::from_static("ACTIX-WEB"),
            ))
            .to_srv_request();
        let _res = srv.call(req).await;
    }

    #[actix_rt::test]
    async fn test_logger_exclude_regex() {
        let srv = |req: ServiceRequest| {
            ok(req.into_response(
                HttpResponse::build(StatusCode::OK)
                    .insert_header(("X-Test", "ttt"))
                    .finish(),
            ))
        };
        let logger =
            Logger::new("%% %{User-Agent}i %{X-Test}o %{HOME}e %D test").exclude_regex("\\w");

        let srv = logger.new_transform(srv.into_service()).await.unwrap();

        let req = TestRequest::default()
            .insert_header((
                header::USER_AGENT,
                header::HeaderValue::from_static("ACTIX-WEB"),
            ))
            .to_srv_request();
        let _res = srv.call(req).await.unwrap();
    }

    #[actix_rt::test]
    async fn test_escape_percent() {
        let mut format = Format::new("%%{r}a");

        let req = TestRequest::default()
            .insert_header((
                header::FORWARDED,
                header::HeaderValue::from_static("for=192.0.2.60;proto=http;by=203.0.113.43"),
            ))
            .to_srv_request();

        let now = OffsetDateTime::now_utc();
        for unit in &mut format.0 {
            unit.render_request(now, &req);
        }

        let resp = HttpResponse::build(StatusCode::OK).force_close().finish();
        for unit in &mut format.0 {
            unit.render_response(&resp);
        }

        let entry_time = OffsetDateTime::now_utc();
        let render = |fmt: &mut fmt::Formatter<'_>| {
            for unit in &format.0 {
                unit.render(fmt, 1024, entry_time)?;
            }
            Ok(())
        };
        let s = format!("{}", FormatDisplay(&render));
        assert_eq!(s, "%{r}a");
    }

    #[actix_rt::test]
    async fn test_url_path() {
        let mut format = Format::new("%T %U");
        let req = TestRequest::default()
            .insert_header((
                header::USER_AGENT,
                header::HeaderValue::from_static("ACTIX-WEB"),
            ))
            .uri("/test/route/yeah")
            .to_srv_request();

        let now = OffsetDateTime::now_utc();
        for unit in &mut format.0 {
            unit.render_request(now, &req);
        }

        let resp = HttpResponse::build(StatusCode::OK).force_close().finish();
        for unit in &mut format.0 {
            unit.render_response(&resp);
        }

        let render = |fmt: &mut fmt::Formatter<'_>| {
            for unit in &format.0 {
                unit.render(fmt, 1024, now)?;
            }
            Ok(())
        };
        let s = format!("{}", FormatDisplay(&render));
        println!("{}", s);
        assert!(s.contains("/test/route/yeah"));
    }

    #[actix_rt::test]
    async fn test_default_format() {
        let mut format = Format::default();

        let req = TestRequest::default()
            .insert_header((
                header::USER_AGENT,
                header::HeaderValue::from_static("ACTIX-WEB"),
            ))
            .peer_addr("127.0.0.1:8081".parse().unwrap())
            .to_srv_request();

        let now = OffsetDateTime::now_utc();
        for unit in &mut format.0 {
            unit.render_request(now, &req);
        }

        let resp = HttpResponse::build(StatusCode::OK).force_close().finish();
        for unit in &mut format.0 {
            unit.render_response(&resp);
        }

        let entry_time = OffsetDateTime::now_utc();
        let render = |fmt: &mut fmt::Formatter<'_>| {
            for unit in &format.0 {
                unit.render(fmt, 1024, entry_time)?;
            }
            Ok(())
        };
        let s = format!("{}", FormatDisplay(&render));
        assert!(s.contains("GET / HTTP/1.1"));
        assert!(s.contains("127.0.0.1"));
        assert!(s.contains("200 1024"));
        assert!(s.contains("ACTIX-WEB"));
    }

    #[actix_rt::test]
    async fn test_request_time_format() {
        let mut format = Format::new("%t");
        let req = TestRequest::default().to_srv_request();

        let now = OffsetDateTime::now_utc();
        for unit in &mut format.0 {
            unit.render_request(now, &req);
        }

        let resp = HttpResponse::build(StatusCode::OK).force_close().finish();
        for unit in &mut format.0 {
            unit.render_response(&resp);
        }

        let render = |fmt: &mut fmt::Formatter<'_>| {
            for unit in &format.0 {
                unit.render(fmt, 1024, now)?;
            }
            Ok(())
        };
        let s = format!("{}", FormatDisplay(&render));
        assert!(s.contains(&now.format("%Y-%m-%dT%H:%M:%S")));
    }

    #[actix_rt::test]
    async fn test_remote_addr_format() {
        let mut format = Format::new("%{r}a");

        let req = TestRequest::default()
            .insert_header((
                header::FORWARDED,
                header::HeaderValue::from_static("for=192.0.2.60;proto=http;by=203.0.113.43"),
            ))
            .to_srv_request();

        let now = OffsetDateTime::now_utc();
        for unit in &mut format.0 {
            unit.render_request(now, &req);
        }

        let resp = HttpResponse::build(StatusCode::OK).force_close().finish();
        for unit in &mut format.0 {
            unit.render_response(&resp);
        }

        let entry_time = OffsetDateTime::now_utc();
        let render = |fmt: &mut fmt::Formatter<'_>| {
            for unit in &format.0 {
                unit.render(fmt, 1024, entry_time)?;
            }
            Ok(())
        };
        let s = format!("{}", FormatDisplay(&render));
        println!("{}", s);
        assert!(s.contains("192.0.2.60"));
    }

    #[actix_rt::test]
    async fn test_custom_closure_log() {
        let mut logger = Logger::new("test %{CUSTOM}xi")
            .custom_request_replace("CUSTOM", |_req: &ServiceRequest| -> String {
                String::from("custom_log")
            });
        let mut unit = Rc::get_mut(&mut logger.0).unwrap().format.0[1].clone();

        let label = match &unit {
            FormatText::CustomRequest(label, _) => label,
            ft => panic!("expected CustomRequest, found {:?}", ft),
        };

        assert_eq!(label, "CUSTOM");

        let req = TestRequest::default().to_srv_request();
        let now = OffsetDateTime::now_utc();

        unit.render_request(now, &req);

        let render = |fmt: &mut fmt::Formatter<'_>| unit.render(fmt, 1024, now);

        let log_output = FormatDisplay(&render).to_string();
        assert_eq!(log_output, "custom_log");
    }

    #[actix_rt::test]
    async fn test_closure_logger_in_middleware() {
        let captured = "custom log replacement";

        let logger = Logger::new("%{CUSTOM}xi")
            .custom_request_replace("CUSTOM", move |_req: &ServiceRequest| -> String {
                captured.to_owned()
            });

        let srv = logger.new_transform(test::ok_service()).await.unwrap();

        let req = TestRequest::default().to_srv_request();
        srv.call(req).await.unwrap();
    }
}
