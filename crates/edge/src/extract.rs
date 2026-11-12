//! Extractors that refuse in the API's own envelope.
//!
//! axum's own `Query`, `Path` and `Json` answer a bad request with a plain-text body at 400, 415
//! or 422. Every other error here is `{error, message}`, and a client that calls `.json()` on the
//! answer should not have to know which kind of failure it hit.

use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use kittiwake_types::ApiError;
use serde::de::DeserializeOwned;

use crate::ErrorResponse;

/// A request the extractor could not read is the caller's to fix, except a route declared with
/// parameters its path does not carry, which is ours.
fn refusal(status: axum::http::StatusCode, text: String) -> ErrorResponse {
    if status.is_server_error() {
        ApiError::Internal(text).into()
    } else {
        ApiError::BadRequest(text).into()
    }
}

pub struct Query<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for Query<T> {
    type Rejection = ErrorResponse;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ErrorResponse> {
        axum::extract::Query::<T>::from_request_parts(parts, state)
            .await
            .map(|q| Query(q.0))
            .map_err(|r| refusal(r.status(), r.body_text()))
    }
}

pub struct Path<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned + Send> FromRequestParts<S> for Path<T> {
    type Rejection = ErrorResponse;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ErrorResponse> {
        axum::extract::Path::<T>::from_request_parts(parts, state)
            .await
            .map(|p| Path(p.0))
            .map_err(|r| refusal(r.status(), r.body_text()))
    }
}

pub struct Json<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for Json<T> {
    type Rejection = ErrorResponse;

    async fn from_request(req: Request, state: &S) -> Result<Self, ErrorResponse> {
        axum::Json::<T>::from_request(req, state)
            .await
            .map(|j| Json(j.0))
            .map_err(|r| refusal(r.status(), r.body_text()))
    }
}
