use serde::Serialize;
use serde_json::Value;
use std::borrow::Cow;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    MixValidationError,
    MixRequestInvalid,
    MixNotFound,
    MixConflict,
    MixConfigInvalid,
    MixLocalFailure,
    MixInternalError,
    MixAuthRequired,
    MixOriginDenied,
    MixSwitchRecoveryRequired,
    MixSwitchRolledBack,
    MixSwitchRecoveryFailed,
    MixSwitchVerificationFailed,
    MixActiveAccountUnmanaged,
    MixAccountRefreshFailed,
    MixAccountReauthRequired,
    MixAccountLocalRepairUnavailable,
    MixCodexFileAuthRequired,
    MixCredentialUnavailable,
    MixCredentialNotFound,
    MixSensitiveDataRejected,
    MixCredentialTooLarge,
    MixSessionUnavailable,
    MixProcessStillRunning,
    MixUnsupported,
}

impl ErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MixValidationError => "MIX_VALIDATION_ERROR",
            Self::MixRequestInvalid => "MIX_REQUEST_INVALID",
            Self::MixNotFound => "MIX_NOT_FOUND",
            Self::MixConflict => "MIX_CONFLICT",
            Self::MixConfigInvalid => "MIX_CONFIG_INVALID",
            Self::MixLocalFailure => "MIX_LOCAL_FAILURE",
            Self::MixInternalError => "MIX_INTERNAL_ERROR",
            Self::MixAuthRequired => "MIX_AUTH_REQUIRED",
            Self::MixOriginDenied => "MIX_ORIGIN_DENIED",
            Self::MixSwitchRecoveryRequired => "MIX_SWITCH_RECOVERY_REQUIRED",
            Self::MixSwitchRolledBack => "MIX_SWITCH_ROLLED_BACK",
            Self::MixSwitchRecoveryFailed => "MIX_SWITCH_RECOVERY_FAILED",
            Self::MixSwitchVerificationFailed => "MIX_SWITCH_VERIFICATION_FAILED",
            Self::MixActiveAccountUnmanaged => "MIX_ACTIVE_ACCOUNT_UNMANAGED",
            Self::MixAccountRefreshFailed => "MIX_ACCOUNT_REFRESH_FAILED",
            Self::MixAccountReauthRequired => "MIX_ACCOUNT_REAUTH_REQUIRED",
            Self::MixAccountLocalRepairUnavailable => "MIX_ACCOUNT_LOCAL_REPAIR_UNAVAILABLE",
            Self::MixCodexFileAuthRequired => "MIX_CODEX_FILE_AUTH_REQUIRED",
            Self::MixCredentialUnavailable => "MIX_CREDENTIAL_STORE_UNAVAILABLE",
            Self::MixCredentialNotFound => "MIX_CREDENTIAL_NOT_FOUND",
            Self::MixSensitiveDataRejected => "MIX_SENSITIVE_DATA_REJECTED",
            Self::MixCredentialTooLarge => "MIX_CREDENTIAL_TOO_LARGE",
            Self::MixSessionUnavailable => "MIX_SESSION_UNAVAILABLE",
            Self::MixProcessStillRunning => "MIX_PROCESS_STILL_RUNNING",
            Self::MixUnsupported => "MIX_UNSUPPORTED",
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct Error {
    pub code: ErrorCode,
    pub message: Cow<'static, str>,
    pub details: Value,
}

impl Error {
    pub fn new(code: ErrorCode, message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            code,
            message: message.into(),
            details: Value::Object(Default::default()),
        }
    }

    pub fn details(mut self, details: impl Serialize) -> Self {
        self.details = serde_json::to_value(details).unwrap_or(Value::Null);
        self
    }

    pub fn invalid(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(ErrorCode::MixValidationError, message)
    }

    pub fn not_found(kind: &str, id: &str) -> Self {
        Self::new(ErrorCode::MixNotFound, format!("unknown {kind}: {id}"))
    }

    pub fn io(context: &'static str, error: std::io::Error) -> Self {
        Self::new(ErrorCode::MixLocalFailure, format!("{context}: {error}"))
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::new(
            ErrorCode::MixConfigInvalid,
            format!("invalid Mix JSON: {error}"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{Error, ErrorCode};

    #[test]
    fn credential_size_error_uses_the_public_error_code() {
        let error = Error::new(ErrorCode::MixCredentialTooLarge, "credential is too large");
        assert_eq!(error.code.as_str(), "MIX_CREDENTIAL_TOO_LARGE");
    }
}
