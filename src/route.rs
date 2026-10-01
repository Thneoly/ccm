use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    pub primary: String,
    #[serde(default)]
    pub fallback: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedRoute {
    pub target: String,
    pub primary: String,
    pub fallback: Vec<String>,
}

impl ResolvedRoute {
    pub fn candidates(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.primary.as_str()).chain(self.fallback.iter().map(String::as_str))
    }
}
