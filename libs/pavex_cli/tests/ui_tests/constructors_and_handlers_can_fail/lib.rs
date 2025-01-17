use std::path::PathBuf;

use pavex_builder::{f, AppBlueprint, Lifecycle};
use pavex_runtime::{http::Request, hyper::body::Body, response::Response};

pub struct Logger;

pub fn extract_path(_inner: Request<Body>) -> Result<PathBuf, ExtractPathError<String>> {
    todo!()
}

#[derive(Debug)]
pub struct ExtractPathError<T>(T);

pub fn handle_extract_path_error(
    _e: &ExtractPathError<String>,
    _logger: Logger,
) -> pavex_runtime::response::Response {
    todo!()
}

pub fn logger() -> Result<Logger, LoggerError> {
    todo!()
}

#[derive(Debug)]
pub struct LoggerError;

pub fn handle_logger_error(_e: &LoggerError) -> Response {
    todo!()
}

pub fn request_handler(
    _inner: PathBuf,
    _logger: Logger,
    _http_client: HttpClient,
) -> Result<Response, HandlerError> {
    todo!()
}

#[derive(Debug)]
pub struct HandlerError;

pub fn handle_handler_error(_e: &HandlerError) -> Response {
    todo!()
}

#[derive(Clone)]
pub struct Config;

pub fn config() -> Config {
    todo!()
}

#[derive(Clone)]
pub struct HttpClient;

#[derive(Debug)]
pub struct HttpClientError;

pub fn http_client(_config: Config) -> Result<HttpClient, HttpClientError> {
    todo!()
}

pub fn blueprint() -> AppBlueprint {
    let mut bp = AppBlueprint::new();
    bp.constructor(f!(crate::http_client), Lifecycle::Singleton);
    bp.constructor(f!(crate::extract_path), Lifecycle::RequestScoped)
        .error_handler(f!(crate::handle_extract_path_error));
    bp.constructor(f!(crate::logger), Lifecycle::Transient)
        .error_handler(f!(crate::handle_logger_error));
    bp.route(f!(crate::request_handler), "/home")
        .error_handler(f!(crate::handle_handler_error));
    bp
}
