//! axum's extractors with their rejections mapped to [`AppError`], so every
//! failure has the API's error shape.

use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use serde::de::DeserializeOwned;

use crate::error::{AppError, ErrorCode};

/// A JSON body; a malformed one is `bad_request`.
pub struct JsonBody<T>(pub T);

impl<S, T> FromRequest<S> for JsonBody<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(value)) => Ok(JsonBody(value)),
            Err(_) => Err(AppError(ErrorCode::BadRequest)),
        }
    }
}

/// An urlencoded form body (the portal); a malformed one is `bad_request`.
pub struct FormBody<T>(pub T);

impl<S, T> FromRequest<S> for FormBody<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Form::<T>::from_request(req, state).await {
            Ok(axum::Form(value)) => Ok(FormBody(value)),
            Err(_) => Err(AppError(ErrorCode::BadRequest)),
        }
    }
}

/// Path parameters; one that does not parse (an id that is not a UUID) names
/// nothing, so it is `not_found`.
pub struct ApiPath<T>(pub T);

impl<S, T> FromRequestParts<S> for ApiPath<T>
where
    S: Send + Sync,
    T: DeserializeOwned + Send,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match axum::extract::Path::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Path(value)) => Ok(ApiPath(value)),
            Err(_) => Err(AppError(ErrorCode::NotFound)),
        }
    }
}
