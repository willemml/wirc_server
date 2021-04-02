use std::{collections::HashMap, sync::Arc};

use base64::URL_SAFE_NO_PAD;
use futures::lock::Mutex;
use parse_display::{Display, FromStr};
use reqwest::header::{AUTHORIZATION, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha3::{Digest, Sha3_256};
use tokio::sync::RwLock;

use crate::{
    config::AuthConfigs, error::AuthError, get_system_millis, user::User, Result, ID,
    USER_AGENT_STRING,
};

use oauth2::{basic::BasicClient, reqwest::http_client, AuthorizationCode};
use oauth2::{AuthUrl, ClientId, ClientSecret, CsrfToken, Scope, TokenResponse, TokenUrl};

type SessionMap = Arc<RwLock<HashMap<String, HashMap<String, u128>>>>; // HashMap<Hashed User ID, HashMap<Hashed Token, Token Expiry Date>>
type LoginSession = (u128, BasicClient); // (Login Start Time, Client)
type LoginSessionMap = Arc<Mutex<HashMap<String, LoginSession>>>; // HashMap<Login Secret, <LoginSession>>

/// Relative path to the file where sessions (user ID, auth token and expiry time triples) are stored.
pub const SESSION_FILE: &str = "data/sessions.json";

/// Represents supported OAuth services.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Display, FromStr)]
pub enum Service {
    GitHub,
}

/// Parameters for authentication finish queries.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AuthQuery {
    /// A random string generated by WICRS to be used in the OAuth flow.
    pub state: String,
    /// Code given by the OAuth service after starting the OAuth flow.
    pub code: String,
    /// Optional expiry date in milliseconds from Unix Epoch for the token returned after the authentication is complete.
    pub expires: Option<u128>,
}

/// Combination of a user ID and an authentication token.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct IDToken {
    /// ID of a user.
    pub id: ID,
    /// Authentication token.
    pub token: String,
}

/// Authentication handler.
pub struct Auth {
    /// GitHub specific OAuth handlers.
    github: Arc<Mutex<GitHub>>,
    /// List of authenticated session tokens and their corresponding user IDs, all values are hashed.
    sessions: SessionMap,
}

impl Auth {
    /// Sets up an authentication manager based on a configuration object and preloads previous authenticated token sessions from disk.
    pub fn from_config(config: &AuthConfigs) -> Self {
        std::fs::create_dir_all("data/users")
            .expect("Failed to create the ./data/users directory.");
        let github_conf = config.github.as_ref().expect(
            "GitHub is currently the only supported oauth service provider, it must be configured.",
        );
        Self {
            github: Arc::new(Mutex::new(GitHub::new(
                github_conf.client_id.clone(),
                github_conf.client_secret.clone(),
            ))),
            sessions: Arc::new(RwLock::new(Auth::load_tokens())),
        }
    }

    /// Creates an authentication manager with hardcoded user data for testing purposes only.
    pub async fn for_testing() -> (Self, ID, String) {
        let auth = Self {
            github: Arc::new(Mutex::new(GitHub::new(
                "testing".to_string(),
                "testing".to_string(),
            ))),
            sessions: Arc::new(RwLock::new(HashMap::new())),
        };
        let account = User {
            id: ID::from_u128(0),
            username: "testuser".to_string(),
            email: "test@example.com".to_string(),
            in_hubs: Vec::new(),
            created: 0,
            service: Service::GitHub,
        };
        account.save().await.expect("Failed to save test account.");
        let token = "testtoken".to_string();
        let hashed = hash_auth(account.id.clone(), token.clone());
        let mut map = HashMap::new();
        map.insert(hashed.1, u128::MAX);
        auth.sessions.write().await.insert(hashed.0, map);
        (auth, account.id, token)
    }

    /// Saves current authenticated token sessions to disk.
    ///
    /// # Errors
    ///
    /// Returns an error if the data could not be written to the disk.
    fn save_tokens(sessions: &HashMap<String, HashMap<String, u128>>) -> Result<()> {
        std::fs::write(
            SESSION_FILE,
            serde_json::to_string(sessions).unwrap_or("{}".to_string()),
        )
        .map_err(|e| e.into())
    }

    /// Loads authentication tokens from disk and remover any that have expired.
    fn load_tokens() -> HashMap<String, HashMap<String, u128>> {
        if let Ok(read) = std::fs::read_to_string("data/sessions.json") {
            if let Ok(mut map) =
                serde_json::from_str::<HashMap<String, HashMap<String, u128>>>(&read)
            {
                let now = get_system_millis();
                map.iter_mut()
                    .for_each(|v| v.1.retain(|_, v| v > &mut now.clone()));
                let _save = Auth::save_tokens(&map);
                return map;
            }
        }
        return HashMap::new();
    }

    /// Checks if a given token and user ID match and are authenticated.
    pub async fn is_authenticated(manager: Arc<RwLock<Self>>, id: ID, token_str: String) -> bool {
        let sessions_arc;
        let lock = manager.read().await;
        sessions_arc = lock.sessions.clone();
        let sessions_lock = sessions_arc.read().await;
        let hashed = hash_auth(id, token_str.clone());
        if let Some(map) = sessions_lock.get(&hashed.0) {
            if let Some(expires) = map.get(&hashed.1) {
                if expires > &get_system_millis() {
                    return true;
                }
            }
        }
        false
    }

    /// Invalidates any tokens that are for the given user ID.
    pub async fn invalidate_tokens(manager: Arc<RwLock<Self>>, id: ID) {
        let sessions_arc;
        let mut sessions_lock;
        {
            let lock = manager.write().await;
            sessions_arc = lock.sessions.clone();
            sessions_lock = sessions_arc.write().await;
        }
        sessions_lock.remove(hash_auth(id, String::new()).0.as_str());
        let _save = Auth::save_tokens(&sessions_lock);
    }

    /// Start the OAuth login process. Returns a redirect to the given OAuth service's page with the correct parameters.
    pub async fn start_login(manager: Arc<RwLock<Self>>, service: Service) -> String {
        match service {
            Service::GitHub => {
                let service_arc;
                let service_lock;
                {
                    let lock = manager.write().await;
                    service_arc = lock.github.clone();
                    service_lock = service_arc.lock().await;
                }
                service_lock.start_login().await
            }
        }
    }

    /// Handles the OAuth follow-up request.
    /// Possible errors ase usually caused by external services failing or behaving in unexpected ways.
    pub async fn handle_oauth(
        manager: Arc<RwLock<Self>>,
        service: Service,
        query: AuthQuery,
    ) -> Result<IDToken> {
        let expires = query.expires.unwrap_or(get_system_millis() + 604800000);
        match service {
            Service::GitHub => {
                let service_arc;
                let service_lock;
                {
                    let lock = manager.write().await;
                    service_arc = lock.github.clone();
                    service_lock = service_arc.lock().await;
                }
                service_lock
                    .handle_oauth(manager, query.state, query.code, expires)
                    .await
            }
        }
    }

    /// Finalizes login by adding the user ID + token and expiry time to the session map.
    /// This function will return an error if a new user's data fails to save for any of the reasons outlined in [`User::save`].
    async fn finalize_login(
        manager: Arc<RwLock<Self>>,
        service: Service,
        id: &str,
        expires: u128,
        email: String,
    ) -> Result<IDToken> {
        let user;
        if let Ok(loaded_account) = User::load_get_id(id, &service).await {
            user = loaded_account;
        } else {
            let new_account = User::new(id.to_string(), email, service);
            new_account.save().await?;
            user = new_account;
        }
        let id = user.id;
        let mut vec: Vec<u8> = Vec::with_capacity(64);
        for _ in 0..vec.capacity() {
            vec.push(rand::random());
        }
        let token = base64::encode_config(vec, URL_SAFE_NO_PAD);
        {
            let sessions_arc;
            let mut sessions_lock;
            {
                let lock = manager.write().await;
                sessions_arc = lock.sessions.clone();
                sessions_lock = sessions_arc.write().await;
            }
            let hashed = hash_auth(id.clone(), token.clone());
            if let Some(tokens) = sessions_lock.get_mut(&hashed.0) {
                tokens.insert(hashed.1, expires);
            } else {
                let mut map = HashMap::new();
                map.insert(hashed.1, expires);
                sessions_lock.insert(hashed.0, map);
            }
            let now = get_system_millis();
            sessions_lock
                .iter_mut()
                .for_each(|v| v.1.retain(|_, v| v > &mut now.clone()));
            let _write = Auth::save_tokens(&sessions_lock);
        }
        Ok(IDToken {
            id: id,
            token: token,
        })
    }
}

struct GitHub {
    client: reqwest::Client,
    client_id: ClientId,
    client_secret: ClientSecret,
    auth_url: AuthUrl,
    token_url: TokenUrl,
    sessions: LoginSessionMap,
}

impl GitHub {
    fn new(client_id: String, client_secret: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            client_id: ClientId::new(client_id),
            client_secret: ClientSecret::new(client_secret),
            auth_url: AuthUrl::new("https://github.com/login/oauth/authorize".to_string())
                .expect("Invalid GitHub authorization endpoint URL"),
            token_url: TokenUrl::new("https://github.com/login/oauth/access_token".to_string())
                .expect("Invalid GitHub token endpoint URL"),
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn get_id(&self, token: &String) -> Result<String, AuthError> {
        let user_request = self
            .client
            .get("https://api.github.com/user")
            .header(USER_AGENT, USER_AGENT_STRING)
            .header(AUTHORIZATION, "token ".to_owned() + token)
            .send()
            .await;
        if let Ok(response) = user_request {
            if let Ok(json) = response.json::<Value>().await {
                Ok(json["id"].to_string())
            } else {
                Err(AuthError::BadJson)
            }
        } else {
            Err(AuthError::NoResponse)
        }
    }

    async fn get_email(&self, token: &String) -> Result<String, AuthError> {
        let email_request = self
            .client
            .get("https://api.github.com/user/emails")
            .header(USER_AGENT, USER_AGENT_STRING)
            .header(AUTHORIZATION, "token ".to_owned() + token)
            .send()
            .await;
        if let Ok(response) = email_request {
            if let Ok(json) = response.json::<Value>().await {
                if let Some(email_array) = json.as_array() {
                    for email_entry in email_array {
                        if let Some(is_primary) = email_entry["primary"].as_bool() {
                            if is_primary {
                                return Ok(email_entry["email"].to_string());
                            }
                        }
                    }
                }
            }
            Err(AuthError::BadJson)
        } else {
            Err(AuthError::NoResponse)
        }
    }

    async fn get_session(&self, state: &String) -> Option<LoginSession> {
        let arc = self.sessions.clone();
        let mut lock = arc.lock().await;
        lock.remove(state)
    }

    async fn start_login(&self) -> String {
        let client = BasicClient::new(
            self.client_id.clone(),
            Some(self.client_secret.clone()),
            self.auth_url.clone(),
            Some(self.token_url.clone()),
        );
        let (authorize_url, csrf_state) = client
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new("read:user".to_string()))
            .add_scope(Scope::new("user:email".to_string()))
            .url();
        {
            let arc = self.sessions.clone();
            let mut lock = arc.lock().await;
            lock.insert(csrf_state.secret().clone(), (get_system_millis(), client));
        }
        authorize_url.to_string()
    }

    async fn handle_oauth(
        &self,
        manager: Arc<RwLock<Auth>>,
        state: String,
        code: String,
        expires: u128,
    ) -> Result<IDToken> {
        if let Some(client) = self.get_session(&state).await {
            let code = AuthorizationCode::new(code.clone());
            match client.1.exchange_code(code).request(http_client) {
                Ok(token) => {
                    let token = token.access_token().secret();
                    let id = self.get_id(&token).await?;
                    let email = self.get_email(&token).await?;
                    Auth::finalize_login(manager, Service::GitHub, &id, expires, email).await
                }
                Err(_) => Err(AuthError::OAuthExchangeFailed.into()),
            }
        } else {
            Err(AuthError::InvalidSession.into())
        }
    }
}

fn hash_auth(id: ID, token: String) -> (String, String) {
    // (Hashed ID, Hashed Token)
    let mut hasher = Sha3_256::new();
    hasher.update(id.as_bytes());
    let id_hash = format!("{:x}", hasher.finalize_reset());
    hasher.update(token.as_bytes());
    (id_hash, format!("{:x}", hasher.finalize_reset()))
}
