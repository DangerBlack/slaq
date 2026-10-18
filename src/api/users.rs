use serde::Deserialize;

/// Extended user profile information.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct UserProfileExtended {
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub real_name: String,
    #[serde(default)]
    pub image_72: String,
    #[serde(default)]
    pub image_192: String,
    #[serde(default)]
    pub image_512: String,
    #[serde(default)]
    pub status_text: String,
    #[serde(default)]
    pub status_emoji: String,
    #[serde(default)]
    pub status_expiration: i64,
    #[serde(default)]
    pub phone: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub skype: String,
}

/// Response from users.info.
#[derive(Debug, Clone, Deserialize)]
pub struct UsersInfoResponse {
    pub ok: bool,
    pub user: Option<UserDetail>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserDetail {
    pub id: String,
    pub team_id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub profile: UserProfileExtended,
    #[serde(default)]
    pub updated: i64,
    #[serde(default)]
    pub is_bot: bool,
    #[serde(default)]
    pub is_app_user: bool,
}
