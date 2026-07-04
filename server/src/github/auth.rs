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

// GitHub API response for token exchange
#[derive(Debug, Deserialize)]
pub struct GitHubTokenResponse {
    pub access_token: String,
    // These fields are required for proper deserialization of GitHub's API response
    // but are not used in our code
    #[allow(dead_code)]
    pub token_type: String,
    #[allow(dead_code)]
    pub scope: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
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
