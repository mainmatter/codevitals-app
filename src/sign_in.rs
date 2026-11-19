//! Signing people in with GitHub after they install the app.

use std::{fmt::Display, sync::Arc};

use axum::{
    extract::{Query, State},
    http::{
        HeaderMap, StatusCode,
        header::{COOKIE, SET_COOKIE},
    },
    response::{IntoResponse, Redirect},
};
use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
use octocrab::{auth::ExchangeWebFlowCodeBuilder, models::Installation, secrecy::ExposeSecret};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::{App, limits, list};

/// `__Host-`: set by this origin only, over HTTPS only.
const VERIFIER_COOKIE: &str = "__Host-pkce";

/// `GET /setup`: GitHub sends people here after they install the app. The `installation_id` it
/// adds can be forged, so it is ignored, and the person is sent to sign in instead.
pub async fn setup(State(app): State<Arc<App>>) -> impl IntoResponse {
    // PKCE ties the sign-in to this browser: the cookie keeps a random verifier, and GitHub,
    // which sees only its hash, hands out a token only alongside the verifier. The hash
    // doubles as `state`, so that mismatched callbacks fail without asking GitHub.
    let mut verifier = [0; 32];
    getrandom::fill(&mut verifier).expect("the OS to provide randomness");
    let verifier = BASE64_URL_SAFE_NO_PAD.encode(verifier);
    let challenge = challenge(&verifier);

    // `SameSite=Lax`, not `Strict`, so the cookie survives GitHub's redirect back.
    let cookie = format!(
        "{VERIFIER_COOKIE}={verifier}; Max-Age=600; Path=/; Secure; HttpOnly; SameSite=Lax"
    );
    let client_id = app.config.client_id.expose_secret();
    let authorize = format!(
        "https://github.com/login/oauth/authorize?client_id={client_id}\
         &state={challenge}&code_challenge={challenge}&code_challenge_method=S256"
    );
    ([(SET_COOKIE, cookie)], Redirect::to(&authorize))
}

fn challenge(verifier: &str) -> String {
    BASE64_URL_SAFE_NO_PAD.encode(Sha256::digest(verifier))
}

#[derive(Deserialize)]
pub struct Callback {
    code: String,
    state: String,
}

/// `GET /callback`: GitHub sends people back here once they have signed in. Shows the
/// installations they can reach.
pub async fn callback(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(Callback { code, state }): Query<Callback>,
) -> Result<String, StatusCode> {
    let verifier = headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .flat_map(|header| header.split(';'))
        .find_map(|cookie| {
            cookie
                .trim()
                .strip_prefix(VERIFIER_COOKIE)?
                .strip_prefix('=')
        })
        .filter(|verifier| challenge(verifier) == state)
        .ok_or_else(|| {
            warn!("sign-in did not start in this browser");
            StatusCode::BAD_REQUEST
        })?;

    let config = &app.config;
    let token = ExchangeWebFlowCodeBuilder::new(
        &app.oauth,
        &config.client_id,
        &config.client_secret,
        &code,
    )
    .code_verifier(verifier)
    .send()
    .await
    .map_err(failed)?;

    // Acts as the person who signed in. Store `token.access_token` to keep acting for them.
    let user = app
        .github
        .user_access_token(token.access_token)
        .map_err(failed)?;
    let installations: Vec<Installation> =
        list(&user, "/user/installations", limits::INSTALLATIONS)
            .await
            .map_err(failed)?;

    let accounts: Vec<_> = installations.into_iter().map(|i| i.account.login).collect();
    info!(?accounts, "signed in");
    Ok(format!(
        "Signed in. Installations you can reach: {accounts:?}"
    ))
}

fn failed(error: impl Display) -> StatusCode {
    warn!(%error, "sign-in failed");
    StatusCode::BAD_GATEWAY
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    use super::*;
    use crate::fixtures;

    /// GitHub never answers. Callbacks that pass the check ask it only so many at once, and all
    /// give up, those still waiting for a turn included.
    #[tokio::test(start_paused = true)]
    async fn callbacks_are_bounded() {
        let asked = Arc::new(AtomicUsize::new(0));
        let counted = asked.clone();
        let github = fixtures::github(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            std::future::pending()
        });
        let router = crate::router(fixtures::app(github));
        let uri = format!("/callback?code=code&state={}", challenge("verifier"));
        let callbacks = Vec::from_iter((0..2 * limits::SIGN_INS_AT_ONCE).map(|_| {
            let request = Request::get(&uri)
                .header(COOKIE, format!("{VERIFIER_COOKIE}=verifier"))
                .body(Body::empty())
                .unwrap();
            tokio::spawn(router.clone().oneshot(request))
        }));

        // The paused clock moves once every task waits.
        tokio::time::sleep(limits::SIGN_IN_TIME / 2).await;
        assert_eq!(asked.load(Ordering::SeqCst), limits::SIGN_INS_AT_ONCE);
        for callback in callbacks {
            let response = callback.await.unwrap().unwrap();
            assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
        }
    }
}
