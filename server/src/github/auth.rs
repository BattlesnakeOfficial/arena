use serde::Deserialize;

// Config structure for GitHub OAuth. Built from env in `crate::config`
// (the single env-reading boundary), not here.
#[derive(Clone, Debug)]
pub struct GitHubOAuthConfig {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
    // Configurable URLs for testing with mock OAuth server
    pub oauth_url: String,
    pub token_url: String,
    pub api_url: String,
}

// GitHub OAuth parameters
#[derive(Debug, Deserialize)]
pub struct GitHubAuthParams {
    pub code: String,
    pub state: String,
}

// GitHub API response for token exchange. Only the access token is used
// (the callback's Bearer header); serde ignores the rest of GitHub's JSON
// (no deny_unknown_fields), so unused fields are deleted rather than
// #[allow(dead_code)]'d.
#[derive(Debug, Deserialize)]
pub struct GitHubTokenResponse {
    pub access_token: String,
}

// GitHub API response for user data
#[derive(Debug, Deserialize)]
pub struct GitHubUser {
    pub id: i64,
    pub login: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    pub avatar_url: String,
}
