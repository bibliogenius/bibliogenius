use sea_orm::{entity::prelude::*, ActiveValue::Set};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstallationProfile {
    Individual,
    Professional,
}

impl From<String> for InstallationProfile {
    fn from(s: String) -> Self {
        match s.as_str() {
            "professional" => InstallationProfile::Professional,
            _ => InstallationProfile::Individual,
        }
    }
}

impl std::fmt::Display for InstallationProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            InstallationProfile::Individual => write!(f, "individual"),
            InstallationProfile::Professional => write!(f, "professional"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "installation_profiles")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub profile_type: String,
    pub enabled_modules: String, // JSON array
    pub theme: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileConfig {
    pub profile: InstallationProfile,
    pub enabled_modules: Vec<String>,
    pub theme: String,
}

impl ProfileConfig {
    pub async fn load(db: &DatabaseConnection) -> Result<Self, String> {
        let profile_model = Entity::find()
            .one(db)
            .await
            .map_err(|e| format!("Failed to load profile: {}", e))?
            .ok_or_else(|| "No profile found".to_string())?;

        let enabled_modules: Vec<String> =
            serde_json::from_str(&profile_model.enabled_modules).unwrap_or_default();

        Ok(ProfileConfig {
            profile: InstallationProfile::from(profile_model.profile_type),
            enabled_modules,
            theme: profile_model.theme.unwrap_or_else(|| "default".to_string()),
        })
    }

    pub async fn save(&self, db: &DatabaseConnection) -> Result<(), String> {
        let now = chrono::Utc::now().to_rfc3339();
        let modules_json = serde_json::to_string(&self.enabled_modules)
            .map_err(|e| format!("Failed to serialize modules: {}", e))?;

        // Update existing profile (assume ID 1 for now)
        let profile = ActiveModel {
            id: Set(1),
            profile_type: Set(self.profile.to_string()),
            enabled_modules: Set(modules_json),
            theme: Set(Some(self.theme.clone())),
            updated_at: Set(now),
            ..Default::default()
        };

        profile
            .update(db)
            .await
            .map_err(|e| format!("Failed to save profile: {}", e))?;

        Ok(())
    }

    pub fn is_module_enabled(&self, module_name: &str) -> bool {
        self.enabled_modules.contains(&module_name.to_string())
    }

    pub fn is_professional(&self) -> bool {
        matches!(self.profile, InstallationProfile::Professional)
    }
}
