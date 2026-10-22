//! Origin-bound passkey ceremonies. Persist states only in a short-lived, single-use,
//! server-side store bound to the initiating user. Never put state in cookies.

use serde::{Deserialize, Serialize};
use webauthn_rs::prelude::{PasskeyAuthentication, PasskeyRegistration, Webauthn, WebauthnBuilder};

pub use webauthn_rs::prelude::{
    CreationChallengeResponse, CredentialID, Passkey, PublicKeyCredential,
    RegisterPublicKeyCredential, RequestChallengeResponse, Url, Uuid,
};

use crate::AuthError;

/// Trusted registration state, consumed by completion. Serializable only for server storage.
#[derive(Serialize, Deserialize)]
pub struct RegistrationState {
    inner: PasskeyRegistration,
    excluded: Vec<CredentialID>,
    require_user_verification: bool,
}

/// Trusted authentication state, consumed by completion. Never accept it from the client.
#[derive(Serialize, Deserialize)]
pub struct AuthenticationState {
    inner: PasskeyAuthentication,
    require_user_verification: bool,
}

/// Verified assertion metadata. Persist the counter and updated passkey atomically.
#[derive(Debug)]
pub struct AuthenticationResult {
    /// Identifier of the credential whose signature was verified.
    pub credential_id: CredentialID,
    /// Latest authenticator counter; zero is valid for counterless synced passkeys.
    pub new_sign_count: u32,
    /// Whether the signed authenticator flags report user verification.
    pub user_verified: bool,
    inner: webauthn_rs::prelude::AuthenticationResult,
}

impl AuthenticationResult {
    /// Applies counter and backup-state changes to the matching stored passkey.
    ///
    /// # Errors
    /// Rejects a passkey whose identifier differs from the verified credential.
    pub fn update_passkey(&self, passkey: &mut Passkey) -> Result<(), AuthError> {
        passkey
            .update_credential(&self.inner)
            .ok_or(AuthError::Passkey)?;
        Ok(())
    }
}

/// A fixed relying party with exact-origin validation and no attestation requirement.
pub struct Passkeys {
    inner: Webauthn,
}

impl Passkeys {
    /// Validates an HTTPS relying-party origin (HTTP is allowed only for localhost).
    /// Origins must not contain credentials, query strings, fragments, or a path.
    ///
    /// # Errors
    /// Rejects invalid RP configuration without exposing dependency error text.
    pub fn new(rp_id: &str, rp_origin: &str) -> Result<Self, AuthError> {
        let origin = Url::parse(rp_origin).map_err(|_| AuthError::Passkey)?;
        if !(origin.scheme() == "https" || (origin.scheme() == "http" && rp_id == "localhost"))
            || !origin.username().is_empty()
            || origin.password().is_some()
            || origin.query().is_some()
            || origin.fragment().is_some()
            || origin.path() != "/"
        {
            return Err(AuthError::Passkey);
        }
        let inner = WebauthnBuilder::new(rp_id, &origin)
            .map_err(|_| AuthError::Passkey)?
            .allow_subdomains(false)
            .allow_any_port(false)
            .build()
            .map_err(|_| AuthError::Passkey)?;
        Ok(Self { inner })
    }

    /// Starts registration with fresh challenge entropy and excludes existing credentials.
    ///
    /// Set `require_user_verification` for admin flows. The upstream passkey API also
    /// sets `UserVerificationPolicy::Required` for ordinary passkeys, so neither
    /// flow permits a downgrade to user presence alone. Attestation is not required.
    ///
    /// # Errors
    /// Rejects empty/oversized usernames, nil user IDs, or challenge-generation failures.
    pub fn start_registration(
        &self,
        user_id: Uuid,
        username: &str,
        existing_credential_ids: &[CredentialID],
        require_user_verification: bool,
    ) -> Result<(CreationChallengeResponse, RegistrationState), AuthError> {
        if user_id.is_nil() || username.is_empty() || username.len() > 256 {
            return Err(AuthError::Passkey);
        }
        let excluded = existing_credential_ids.to_vec();
        let (challenge, inner) = self
            .inner
            .start_passkey_registration(user_id, username, username, Some(excluded.clone()))
            .map_err(|_| AuthError::Passkey)?;
        Ok((
            challenge,
            RegistrationState {
                inner,
                excluded,
                require_user_verification,
            },
        ))
    }

    /// Finishes a challenge-bound registration and returns a serde-serializable passkey.
    /// Callers must additionally enforce global credential-ID uniqueness in storage.
    ///
    /// # Errors
    /// Rejects invalid signatures, challenge/origin mismatches, missing UV, or duplicates.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "ceremony states are consumed to prevent accidental reuse"
    )]
    pub fn finish_registration(
        &self,
        state: RegistrationState,
        response: &RegisterPublicKeyCredential,
    ) -> Result<Passkey, AuthError> {
        let passkey = self
            .inner
            .finish_passkey_registration(response, &state.inner)
            .map_err(|_| AuthError::Passkey)?;
        // Upstream's registration state enforces Required, including for admin.
        // Keep the admin flag bound to trusted state for the same API as authentication.
        let _ = state.require_user_verification;
        if state.excluded.contains(passkey.cred_id()) {
            return Err(AuthError::Passkey);
        }
        Ok(passkey)
    }

    /// Starts authentication against stored passkeys with a fresh challenge.
    /// The admin flag is stored server-side and enforced again on completion.
    /// Upstream requires user verification in both flows.
    ///
    /// # Errors
    /// Rejects an empty credential set or challenge-generation failure.
    pub fn start_authentication(
        &self,
        passkeys: &[Passkey],
        require_user_verification: bool,
    ) -> Result<(RequestChallengeResponse, AuthenticationState), AuthError> {
        if passkeys.is_empty() {
            return Err(AuthError::Passkey);
        }
        let (challenge, inner) = self
            .inner
            .start_passkey_authentication(passkeys)
            .map_err(|_| AuthError::Passkey)?;
        Ok((
            challenge,
            AuthenticationState {
                inner,
                require_user_verification,
            },
        ))
    }

    /// Verifies the signature, RP/origin, challenge, UV, and monotonic nonzero counter.
    /// Consumes state; the HTTP layer must atomically consume its stored copy as well.
    ///
    /// # Errors
    /// Every ceremony failure returns the same credential-free error.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "ceremony states are consumed to prevent accidental reuse"
    )]
    pub fn finish_authentication(
        &self,
        state: AuthenticationState,
        response: &PublicKeyCredential,
    ) -> Result<AuthenticationResult, AuthError> {
        let result = self
            .inner
            .finish_passkey_authentication(response, &state.inner)
            .map_err(|_| AuthError::Passkey)?;
        if state.require_user_verification && !result.user_verified() {
            return Err(AuthError::Passkey);
        }
        Ok(AuthenticationResult {
            credential_id: result.cred_id().clone(),
            new_sign_count: result.counter(),
            user_verified: result.user_verified(),
            inner: result,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use serde::de::DeserializeOwned;
    use webauthn_authenticator_rs::{WebauthnAuthenticator, softtoken::SoftToken};

    use super::*;

    fn stored<T: Serialize + DeserializeOwned>(value: &T) -> T {
        serde_json::from_slice(&serde_json::to_vec(value).unwrap()).unwrap()
    }

    #[test]
    fn software_authenticator_registration_and_authentication_roundtrip() {
        let server = Passkeys::new("localhost", "http://localhost:8080").unwrap();
        let origin = Url::parse("http://localhost:8080").unwrap();
        // Test-only software authenticator simulates successful PIN/biometric UV.
        let (soft, _) = SoftToken::new(true).unwrap();
        let mut client = WebauthnAuthenticator::new(soft);
        let (challenge, state) = server
            .start_registration(Uuid::new_v4(), "test user", &[], true)
            .unwrap();
        let json = serde_json::to_value(&challenge).unwrap();
        assert_eq!(
            json["publicKey"]["authenticatorSelection"]["userVerification"],
            "required"
        );
        assert_eq!(json["publicKey"]["attestation"], "none");
        let response = client.do_registration(origin.clone(), challenge).unwrap();
        let mut passkey = server
            .finish_registration(stored(&state), &response)
            .unwrap();
        passkey = stored(&passkey);

        let mut previous_count = 0;
        for admin in [true, false] {
            let (challenge, state) = server
                .start_authentication(std::slice::from_ref(&passkey), admin)
                .unwrap();
            let json = serde_json::to_value(&challenge).unwrap();
            assert_eq!(json["publicKey"]["userVerification"], "required");
            let response = client.do_authentication(origin.clone(), challenge).unwrap();
            let result = server
                .finish_authentication(stored(&state), &response)
                .unwrap();
            assert_eq!(&result.credential_id, passkey.cred_id());
            assert!(result.user_verified);
            assert!(result.new_sign_count > previous_count);
            previous_count = result.new_sign_count;
            result.update_passkey(&mut passkey).unwrap();
            let (_, fresh_state) = server
                .start_authentication(std::slice::from_ref(&passkey), admin)
                .unwrap();
            assert!(
                server
                    .finish_authentication(fresh_state, &response)
                    .is_err()
            );
        }
        let (challenge, _) = server
            .start_registration(
                Uuid::new_v4(),
                "another user",
                &[passkey.cred_id().clone()],
                true,
            )
            .unwrap();
        assert_eq!(challenge.public_key.exclude_credentials.unwrap().len(), 1);
    }

    #[test]
    fn challenge_origin_uv_and_counter_failures_are_rejected() {
        let server = Passkeys::new("localhost", "http://localhost:8080").unwrap();
        let origin = Url::parse("http://localhost:8080").unwrap();
        let (soft, _) = SoftToken::new(true).unwrap();
        let mut client = WebauthnAuthenticator::new(soft);
        let user = Uuid::new_v4();
        let (challenge, state) = server.start_registration(user, "test", &[], true).unwrap();
        let response = client.do_registration(origin.clone(), challenge).unwrap();
        let (_, other_state) = server.start_registration(user, "test", &[], true).unwrap();
        assert!(server.finish_registration(other_state, &response).is_err());
        let passkey = server.finish_registration(state, &response).unwrap();

        let (challenge, state) = server
            .start_authentication(std::slice::from_ref(&passkey), true)
            .unwrap();
        // The client may operate at another port; the RP must reject that origin.
        let bad_origin = Url::parse("http://localhost:8081").unwrap();
        let response = client.do_authentication(bad_origin, challenge).unwrap();
        assert!(server.finish_authentication(state, &response).is_err());

        let (challenge, state) = server
            .start_authentication(std::slice::from_ref(&passkey), true)
            .unwrap();
        let response = client.do_authentication(origin.clone(), challenge).unwrap();
        let result = server
            .finish_authentication(stored(&state), &response)
            .unwrap();
        let mut updated = passkey.clone();
        result.update_passkey(&mut updated).unwrap();
        // Retain the original challenge while installing the just-observed counter.
        // A signed assertion with that same counter must now fail, even for that challenge.
        let mut replay_state = serde_json::to_value(&state).unwrap();
        let updated = serde_json::to_value(&updated).unwrap();
        replay_state["inner"]["ast"]["credentials"] = serde_json::json!([updated["cred"].clone()]);
        let replay_state: AuthenticationState = serde_json::from_value(replay_state).unwrap();
        assert!(
            server
                .finish_authentication(replay_state, &response)
                .is_err()
        );

        let (challenge, state) = server
            .start_authentication(std::slice::from_ref(&passkey), true)
            .unwrap();
        let mut value = serde_json::to_value(&challenge).unwrap();
        value["publicKey"]["userVerification"] = "discouraged".into();
        let weakened = serde_json::from_value(value).unwrap();
        let response = client.do_authentication(origin.clone(), weakened).unwrap();
        assert!(server.finish_authentication(state, &response).is_err());

        let (soft, _) = SoftToken::new(false).unwrap();
        let mut no_uv_client = WebauthnAuthenticator::new(soft);
        let (challenge, state) = server.start_registration(user, "test", &[], true).unwrap();
        // Simulate a client ignoring Required so the server must validate signed UV.
        let mut value = serde_json::to_value(&challenge).unwrap();
        value["publicKey"]["authenticatorSelection"]["userVerification"] = "discouraged".into();
        let weakened = serde_json::from_value(value).unwrap();
        let response = no_uv_client.do_registration(origin, weakened).unwrap();
        assert!(server.finish_registration(state, &response).is_err());
    }

    #[test]
    fn invalid_configuration_and_empty_inputs() {
        for (rp, origin) in [
            ("example.com", "not a URL"),
            ("example.com", "http://example.com"),
            ("example.com", "https://other.example"),
            ("example.com", "https://a:b@example.com"),
            ("example.com", "https://example.com/path"),
            ("example.com", "https://example.com?q=1"),
            ("example.com", "https://example.com#fragment"),
        ] {
            assert!(Passkeys::new(rp, origin).is_err());
        }
        let server = Passkeys::new("example.com", "https://example.com").unwrap();
        assert!(
            server
                .start_registration(Uuid::nil(), "user", &[], true)
                .is_err()
        );
        assert!(
            server
                .start_registration(Uuid::new_v4(), "", &[], true)
                .is_err()
        );
        assert!(server.start_authentication(&[], true).is_err());
    }
}
