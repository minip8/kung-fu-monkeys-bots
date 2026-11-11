//! The one error type of the API (`docs/ladder-api.md` §2): a code from a
//! fixed table, its HTTP status and a fixed message. Internals (SQL, provider
//! replies, I/O) are logged, never sent.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

macro_rules! error_codes {
    ($($variant:ident = ($code:literal, $status:ident, $message:literal),)*) => {
        /// Every error code the API sends. Append only.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum ErrorCode {
            $($variant,)*
        }

        impl ErrorCode {
            pub const ALL: &'static [ErrorCode] = &[$(ErrorCode::$variant,)*];

            pub fn as_str(self) -> &'static str {
                match self {
                    $(ErrorCode::$variant => $code,)*
                }
            }

            pub fn status(self) -> StatusCode {
                match self {
                    $(ErrorCode::$variant => StatusCode::$status,)*
                }
            }

            pub fn message(self) -> &'static str {
                match self {
                    $(ErrorCode::$variant => $message,)*
                }
            }

            pub fn parse(code: &str) -> Option<ErrorCode> {
                match code {
                    $($code => Some(ErrorCode::$variant),)*
                    _ => None,
                }
            }
        }
    };
}

error_codes! {
    BadRequest = ("bad_request", BAD_REQUEST, "The request is malformed."),
    InvalidName = ("invalid_name", BAD_REQUEST, "The name does not meet the naming rules."),
    InvalidScope = ("invalid_scope", BAD_REQUEST, "The scope must be \"submit\" or \"read\"."),
    Unauthenticated = ("unauthenticated", UNAUTHORIZED, "Sign in or send a bearer token."),
    InvalidToken = ("invalid_token", UNAUTHORIZED, "The bearer token is unknown or revoked."),
    SessionRequired = ("session_required", FORBIDDEN, "This route needs a signed-in portal session, not a token."),
    InsufficientScope = ("insufficient_scope", FORBIDDEN, "The token's scope does not allow this."),
    BadCsrf = ("bad_csrf", FORBIDDEN, "The CSRF token is missing or wrong; reload the page."),
    Forbidden = ("forbidden", FORBIDDEN, "You are not allowed to do this."),
    NotFound = ("not_found", NOT_FOUND, "Not found."),
    UnknownUser = ("unknown_user", NOT_FOUND, "No user has that id or name."),
    MethodNotAllowed = ("method_not_allowed", METHOD_NOT_ALLOWED, "The method is not allowed on this route."),
    NameTaken = ("name_taken", CONFLICT, "That team name is taken."),
    AlreadyMember = ("already_member", CONFLICT, "That user is already a member of the team."),
    LastOwner = ("last_owner", CONFLICT, "The last owner of a team cannot leave it."),
    AmbiguousUser = ("ambiguous_user", CONFLICT, "Several users have that name; use the user id."),
    AlreadyLinked = ("already_linked", CONFLICT, "Your account already has a different identity of this provider."),
    LimitReached = ("limit_reached", CONFLICT, "A limit was reached (tokens per user or members per team)."),
    OAuthState = ("oauth_state", BAD_REQUEST, "The sign-in expired or did not start here; try again."),
    OAuthDenied = ("oauth_denied", FORBIDDEN, "The provider did not grant the sign-in."),
    OAuthFailed = ("oauth_failed", BAD_GATEWAY, "The provider's reply could not be verified; try again."),
    Internal = ("internal", INTERNAL_SERVER_ERROR, "Something went wrong on our side."),
    BadManifest = ("bad_manifest", BAD_REQUEST, "The manifest is refused."),
    BadSource = ("bad_source", BAD_REQUEST, "The source archive is refused."),
    TooLarge = ("too_large", PAYLOAD_TOO_LARGE, "The upload is over its size cap (4 MiB of source)."),
    RateLimited = ("rate_limited", TOO_MANY_REQUESTS, "A team submits at most once a minute; try again shortly."),
    NotReady = ("not_ready", CONFLICT, "Only a ready submission can be made active."),
    InvalidCode = ("invalid_code", BAD_REQUEST, "The login code is unknown, used or expired; run kfm login again."),
    AlreadyInTeam = ("already_in_team", CONFLICT, "An account is in at most one team, and this one already has a team."),
}

/// An API error: `{"error":"<code>","message":"<message>"}` with the code's status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppError(pub ErrorCode);

pub type AppResult<T> = Result<T, AppError>;

impl AppError {
    pub fn code(self) -> ErrorCode {
        self.0
    }

    /// An internal failure: logs `detail` and answers `internal`.
    pub fn internal(detail: impl std::fmt::Display) -> AppError {
        tracing::error!(error = %detail, "internal error");
        AppError(ErrorCode::Internal)
    }
}

impl From<ErrorCode> for AppError {
    fn from(code: ErrorCode) -> Self {
        AppError(code)
    }
}

impl From<sqlx::Error> for AppError {
    fn from(err: sqlx::Error) -> Self {
        AppError::internal(format_args!("database: {err}"))
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let code = self.0;
        let body = serde_json::json!({ "error": code.as_str(), "message": code.message() });
        (code.status(), Json(body)).into_response()
    }
}

/// An error whose message names exactly what was refused (`bad_manifest`,
/// `bad_source`: the first violation found): the code's message with the
/// detail appended. Still the two keys of every error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub code: ErrorCode,
    pub detail: Option<String>,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn with_detail(code: ErrorCode, detail: impl Into<String>) -> ApiError {
        ApiError {
            code,
            detail: Some(detail.into()),
        }
    }

    /// The message sent: the code's, with the detail after a colon.
    pub fn message(&self) -> String {
        match &self.detail {
            Some(detail) => {
                format!(
                    "{}: {}",
                    self.code.message().trim_end_matches('.'),
                    detail.trim_end_matches('.')
                ) + "."
            }
            None => self.code.message().to_owned(),
        }
    }
}

impl From<AppError> for ApiError {
    fn from(err: AppError) -> Self {
        ApiError {
            code: err.0,
            detail: None,
        }
    }
}

impl From<ErrorCode> for ApiError {
    fn from(code: ErrorCode) -> Self {
        ApiError { code, detail: None }
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(err: sqlx::Error) -> Self {
        AppError::from(err).into()
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({ "error": self.code.as_str(), "message": self.message() });
        (self.code.status(), Json(body)).into_response()
    }
}

/// Whether `err` is a unique violation of the constraint or index `name`.
pub fn is_unique_violation(err: &sqlx::Error, name: &str) -> bool {
    match err {
        sqlx::Error::Database(db) => {
            db.code().as_deref() == Some("23505") && db.constraint() == Some(name)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_unique_and_round_trip() {
        let mut seen = std::collections::HashSet::new();
        for &code in ErrorCode::ALL {
            assert!(seen.insert(code.as_str()), "{} twice", code.as_str());
            assert_eq!(ErrorCode::parse(code.as_str()), Some(code));
            assert!(code.status().is_client_error() || code.status().is_server_error());
        }
        assert_eq!(ErrorCode::parse("nope"), None);
    }

    #[test]
    fn a_detail_follows_the_message() {
        let err = ApiError::with_detail(ErrorCode::BadSource, "\"../x.c\" contains a .. component");
        assert_eq!(
            err.message(),
            "The source archive is refused: \"../x.c\" contains a .. component."
        );
        assert_eq!(
            ApiError::from(ErrorCode::NotReady).message(),
            ErrorCode::NotReady.message()
        );
    }
}
