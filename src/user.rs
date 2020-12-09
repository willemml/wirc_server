use std::{collections::HashMap, sync::Arc};

use crate::{
    auth::{Auth, TokenQuery},
    get_system_millis,
    guild::Guild,
    new_id, ApiActionError, JsonLoadError, JsonSaveError, Name, ID, NAME_ALLOWED_CHARS,
};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use warp::{filters::BoxedFilter, Filter, Rejection, Reply};

static ACCOUNT_FOLDER: &str = "data/accounts/";

#[derive(Serialize, Deserialize, Clone)]
pub struct User {
    pub id: ID,
    pub username: String,
    pub created: u128,
    pub parent_id: String,
    pub in_guilds: Vec<ID>,
}

impl User {
    pub fn new(username: String, parent_id: String) -> Result<Self, ()> {
        if username.chars().all(|c| NAME_ALLOWED_CHARS.contains(c)) {
            Ok(Self {
                id: new_id(),
                username,
                parent_id,
                created: get_system_millis(),
                in_guilds: Vec::new(),
            })
        } else {
            Err(())
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Account {
    pub id: String,
    pub email: String,
    pub created: u128,
    pub service: String,
    pub users: HashMap<ID, User>,
}

impl Account {
    pub fn new(id: String, email: String, service: String) -> Self {
        Self {
            id: get_id(&id, &service),
            email,
            service,
            users: HashMap::new(),
            created: get_system_millis(),
        }
    }

    pub async fn create_new_user(&mut self, username: String) -> Result<User, ApiActionError> {
        if let Ok(user) = User::new(username, self.id.clone()) {
            self.users.insert(new_id(), user.clone());
            if let Ok(_) = self.save().await {
                Ok(user)
            } else {
                Err(ApiActionError::WriteFileError)
            }
        } else {
            Err(ApiActionError::BadNameCharacters)
        }
    }

    pub async fn send_guild_message(
        &self,
        user: ID,
        guild: ID,
        channel: ID,
        message: String,
    ) -> Result<(), ApiActionError> {
        if let Some(user) = self.users.get(&user) {
            if user.in_guilds.contains(&guild) {
                if let Ok(mut guild) = Guild::load(&guild.to_string()).await {
                    guild.send_message(user.id, channel, message).await
                } else {
                    Err(ApiActionError::GuildNotFound)
                }
            } else {
                Err(ApiActionError::NotInGuild)
            }
        } else {
            Err(ApiActionError::UserNotFound)
        }
    }

    pub async fn create_guild(&mut self, name: String, user: ID) -> Result<ID, ApiActionError> {
        if !name.chars().all(|c| NAME_ALLOWED_CHARS.contains(c)) {
            return Err(ApiActionError::BadNameCharacters);
        }
        if let Some(user) = self.users.get_mut(&user) {
            let new_guild = Guild::new(name, new_id(), user);
            if let Ok(_) = new_guild.save().await {
                user.in_guilds.push(new_guild.id.clone());
                Ok(new_guild.id)
            } else {
                Err(ApiActionError::WriteFileError)
            }
        } else {
            Err(ApiActionError::UserNotFound)
        }
    }

    pub async fn save(&self) -> Result<(), JsonSaveError> {
        if let Err(_) = tokio::fs::create_dir_all(ACCOUNT_FOLDER).await {
            return Err(JsonSaveError::Directory);
        }
        if let Ok(json) = serde_json::to_string(self) {
            if let Ok(result) =
                std::fs::write(ACCOUNT_FOLDER.to_owned() + &self.id.to_string(), json)
            {
                Ok(result)
            } else {
                Err(JsonSaveError::WriteFile)
            }
        } else {
            Err(JsonSaveError::Serialize)
        }
    }

    pub async fn load(id: &str) -> Result<Self, JsonLoadError> {
        if let Ok(json) = tokio::fs::read_to_string(ACCOUNT_FOLDER.to_owned() + id).await {
            if let Ok(result) = serde_json::from_str(&json) {
                Ok(result)
            } else {
                Err(JsonLoadError::Deserialize)
            }
        } else {
            Err(JsonLoadError::ReadFile)
        }
    }

    pub async fn load_get_id(id: &str, service: &str) -> Result<Self, JsonLoadError> {
        Self::load(&get_id(id, service)).await
    }
}

pub fn get_id(id: &str, service: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(id);
    hasher.update(service);
    format!("{:x}", hasher.finalize())
}

fn api_v1_accountinfo(auth_manager: Arc<Mutex<Auth>>) -> BoxedFilter<(impl Reply,)> {
    warp::get()
        .and(warp::path!("accounts" / String))
        .and(warp::query::<TokenQuery>())
        .and_then(move |id: String, token: TokenQuery| {
            let tmp_auth = auth_manager.clone();
            async move {
                Ok::<_, warp::Rejection>(
                    if Auth::is_authenticated(tmp_auth, id.clone(), token.token).await {
                        if let Ok(account) = Account::load(&id).await {
                            warp::reply::json(&account).into_response()
                        } else {
                            warp::reply::with_status(
                                "That account does not exist.",
                                StatusCode::NOT_FOUND,
                            )
                            .into_response()
                        }
                    } else {
                        warp::reply::with_status(
                            "Invalid authentication details.",
                            StatusCode::FORBIDDEN,
                        )
                        .into_response()
                    },
                )
            }
        })
        .boxed()
}

fn api_v1_adduser(auth_manager: Arc<Mutex<Auth>>) -> BoxedFilter<(impl Reply,)> {
    warp::get()
        .and(warp::path!("account" / "adduser" / String))
        .and(warp::query::<TokenQuery>())
        .and(warp::body::json::<Name>())
        .and_then(move |id: String, token: TokenQuery, name: Name| {
            let tmp_auth = auth_manager.clone();
            async move { Ok::<_, Rejection>(
                if Auth::is_authenticated(tmp_auth, id.clone(), token.token).await {
                    if let Ok(mut account) = Account::load(&id).await {
                        let create = account.create_new_user(name.name).await;
                        if let Ok(user) = create {
                            warp::reply::json(&user).into_response()
                        } else if let Err(err) = create {
                            match err {
                                ApiActionError::WriteFileError => warp::reply::with_status("Server could not write user data to disk.", StatusCode::INTERNAL_SERVER_ERROR).into_response(),
                                ApiActionError::BadNameCharacters => warp::reply::with_status(format!("Username string can only contain the following characters: \"{}\"", NAME_ALLOWED_CHARS), StatusCode::BAD_REQUEST).into_response(),
                                _ => warp::reply::with_status("The server is doing things that it shouldn't.", StatusCode::INTERNAL_SERVER_ERROR).into_response()
                            }
                        } else {
                            warp::reply::with_status("The server is doing things that it shouldn't.", StatusCode::INTERNAL_SERVER_ERROR).into_response()
                        }
                    } else {
                        warp::reply::with_status("That account does not exist.", StatusCode::NOT_FOUND).into_response()
                    }
                } else {
                    warp::reply::with_status("Invalid authentication details.", StatusCode::FORBIDDEN).into_response()
                }
            )}
        }).boxed()
}

pub fn api_v1(auth_manager: Arc<Mutex<Auth>>) -> BoxedFilter<(impl Reply,)> {
    api_v1_accountinfo(auth_manager.clone())
        .or(api_v1_adduser(auth_manager.clone()))
        .boxed()
}
