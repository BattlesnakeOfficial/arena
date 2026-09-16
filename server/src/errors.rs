use std::fmt::{Debug, Display};

use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect};

#[derive(Debug)]
pub struct ServerError<R: IntoResponse>(pub(crate) cja::color_eyre::Report, pub(crate) R);

pub type ServerResult<S, F> = Result<S, ServerError<F>>;

impl<R: IntoResponse> Display for ServerError<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl<R: IntoResponse + Debug> IntoResponse for ServerError<R> {
    fn into_response(self) -> axum::response::Response {
        let response = self.1.into_response();
        if response.status() == StatusCode::NOT_FOUND {
            tracing::debug!(error = ?self.0, "Resource not found");
        } else {
            tracing::error!(error = ?self.0, status = %response.status(), "Request Error");
        }
        response
    }
}

impl<E> From<E> for ServerError<StatusCode>
where
    E: Into<cja::color_eyre::Report>,
{
    fn from(err: E) -> Self {
        ServerError(err.into(), StatusCode::INTERNAL_SERVER_ERROR)
    }
}

pub(crate) trait WithStatus<T> {
    fn with_status(self, status: StatusCode) -> Result<T, ServerError<StatusCode>>;
}

impl<T> WithStatus<T> for Result<T, cja::color_eyre::Report> {
    fn with_status(self, status: StatusCode) -> Result<T, ServerError<StatusCode>> {
        match self {
            Ok(val) => Ok(val),
            Err(err) => Err(ServerError(err, status)),
        }
    }
}

// Implementation of WithStatus for Result<T, String>
impl<T> WithStatus<T> for Result<T, String> {
    fn with_status(self, status: StatusCode) -> Result<T, ServerError<StatusCode>> {
        match self {
            Ok(val) => Ok(val),
            Err(err) => Err(ServerError(cja::color_eyre::eyre::eyre!(err), status)),
        }
    }
}

pub(crate) trait WithRedirect<T> {
    fn with_redirect(self, redirect: Redirect) -> Result<T, ServerError<Redirect>>;
}

impl<T> WithRedirect<T> for Result<T, cja::color_eyre::Report> {
    fn with_redirect(self, redirect: Redirect) -> Result<T, ServerError<Redirect>> {
        match self {
            Ok(val) => Ok(val),
            Err(err) => Err(ServerError(err, redirect)),
        }
    }
}
