use tranquil_db_traits::InviteCodeError;

use crate::api::error::ApiError;
use crate::state::AppState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InviteRegistration {
    Bootstrap,
    Standard(Option<String>),
}

impl InviteRegistration {
    pub fn into_invite_code(self) -> Option<String> {
        match self {
            InviteRegistration::Bootstrap => None,
            InviteRegistration::Standard(code) => code,
        }
    }
}

pub async fn check_registration_invite(
    state: &AppState,
    invite_code: Option<&str>,
) -> Result<InviteRegistration, ApiError> {
    let is_bootstrap = state.bootstrap_invite_code.is_some()
        && state.repos.user.count_users().await.unwrap_or(1) == 0;

    if is_bootstrap {
        return match invite_code {
            Some(code) if Some(code) == state.bootstrap_invite_code.as_deref() => {
                Ok(InviteRegistration::Bootstrap)
            }
            _ => Err(ApiError::InvalidInviteCode),
        };
    }

    match invite_code.map(str::trim).filter(|code| !code.is_empty()) {
        Some(code) => match state.repos.infra.validate_invite_code(code).await {
            Ok(_) => Ok(InviteRegistration::Standard(Some(code.to_owned()))),
            Err(InviteCodeError::DatabaseError(e)) => {
                tracing::error!("failed to validate invite code: {e:?}");
                Err(ApiError::InternalError(None))
            }
            Err(_) => Err(ApiError::InvalidInviteCode),
        },
        None => match tranquil_config::get().server.invite_code_required {
            true => Err(ApiError::InviteCodeRequired),
            false => Ok(InviteRegistration::Standard(None)),
        },
    }
}
