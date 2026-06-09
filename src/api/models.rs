use std::fs;
use std::path::Path;
use std::sync::{OnceLock, RwLock};

use axum::Json;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct ModelCatalog {
    entries: Vec<ModelListEntry>,
}

#[derive(Debug, Clone)]
struct ModelListEntry {
    base: String,
    suffixes: Vec<String>,
    owned_by: String,
    fast_variants: bool,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CatalogFile {
    Wrapped { models: Vec<ModelEntryConfig> },
    List(Vec<ModelEntryConfig>),
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct ModelEntryConfig {
    base: String,
    id: String,
    suffixes: Vec<String>,
    #[serde(rename = "owned-by")]
    owned_by: String,
    #[serde(rename = "fast-variants")]
    fast_variants: bool,
}

impl Default for ModelEntryConfig {
    fn default() -> Self {
        Self {
            base: String::new(),
            id: String::new(),
            suffixes: Vec::new(),
            owned_by: "openai".to_string(),
            fast_variants: true,
        }
    }
}

impl Default for ModelCatalog {
    fn default() -> Self {
        Self {
            entries: DEFAULT_MODEL_LIST
                .iter()
                .map(|entry| ModelListEntry {
                    base: entry.base.to_string(),
                    suffixes: entry.suffixes.iter().map(|s| (*s).to_string()).collect(),
                    owned_by: "openai".to_string(),
                    fast_variants: true,
                })
                .collect(),
        }
    }
}

impl ModelCatalog {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        let data = fs::read_to_string(path)
            .map_err(|e| format!("读取 models catalog 失败 {}: {e}", path.display()))?;
        Self::from_str(&data)
    }

    pub fn from_str(data: &str) -> Result<Self, String> {
        let file: CatalogFile =
            serde_yaml::from_str(data).map_err(|e| format!("解析 models catalog 失败: {e}"))?;
        let configs = match file {
            CatalogFile::Wrapped { models } => models,
            CatalogFile::List(models) => models,
        };
        let mut entries = Vec::with_capacity(configs.len());
        for config in configs {
            let base = if config.base.trim().is_empty() {
                config.id
            } else {
                config.base
            };
            let base = base.trim().to_string();
            if base.is_empty() {
                continue;
            }
            let suffixes = config
                .suffixes
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            let owned_by = config.owned_by.trim();
            entries.push(ModelListEntry {
                base,
                suffixes,
                owned_by: if owned_by.is_empty() {
                    "openai".to_string()
                } else {
                    owned_by.to_string()
                },
                fast_variants: config.fast_variants,
            });
        }
        if entries.is_empty() {
            return Err("models catalog 中没有有效模型".to_string());
        }
        Ok(Self { entries })
    }

    fn response(&self) -> ModelsResponse {
        let mut data = Vec::new();
        for entry in &self.entries {
            push_model_variants(&mut data, entry);
        }
        ModelsResponse {
            object: "list",
            data,
        }
    }
}

#[derive(Debug, Serialize)]
struct ModelItem {
    id: String,
    object: &'static str,
    owned_by: String,
}

#[derive(Debug, Serialize)]
pub struct ModelsResponse {
    object: &'static str,
    data: Vec<ModelItem>,
}

struct DefaultModelListEntry {
    base: &'static str,
    suffixes: &'static [&'static str],
}

const DEFAULT_MODEL_LIST: &[DefaultModelListEntry] = &[
    DefaultModelListEntry {
        base: "gpt-5",
        suffixes: &["low", "medium", "high", "auto"],
    },
    DefaultModelListEntry {
        base: "gpt-5-codex",
        suffixes: &["low", "medium", "high", "auto"],
    },
    DefaultModelListEntry {
        base: "gpt-5-codex-mini",
        suffixes: &["low", "medium", "high", "auto"],
    },
    DefaultModelListEntry {
        base: "gpt-5.1",
        suffixes: &["low", "medium", "high", "none", "auto"],
    },
    DefaultModelListEntry {
        base: "gpt-5.1-codex",
        suffixes: &["low", "medium", "high", "max", "auto"],
    },
    DefaultModelListEntry {
        base: "gpt-5.1-codex-mini",
        suffixes: &["low", "medium", "high", "auto"],
    },
    DefaultModelListEntry {
        base: "gpt-5.1-codex-max",
        suffixes: &["low", "medium", "high", "xhigh", "auto"],
    },
    DefaultModelListEntry {
        base: "gpt-5.2",
        suffixes: &["low", "medium", "high", "xhigh", "none", "auto"],
    },
    DefaultModelListEntry {
        base: "gpt-5.2-codex",
        suffixes: &["low", "medium", "high", "xhigh", "auto"],
    },
    DefaultModelListEntry {
        base: "gpt-5.3-codex",
        suffixes: &["low", "medium", "high", "xhigh", "none", "auto"],
    },
    DefaultModelListEntry {
        base: "gpt-5.4",
        suffixes: &["low", "medium", "high", "xhigh", "none", "auto"],
    },
    DefaultModelListEntry {
        base: "gpt-5.4-mini",
        suffixes: &["low", "medium", "high", "xhigh", "none", "auto"],
    },
    DefaultModelListEntry {
        base: "gpt-5.5",
        suffixes: &["low", "medium", "high", "xhigh", "none", "auto"],
    },
    DefaultModelListEntry {
        base: "codex-auto-review",
        suffixes: &["low", "medium", "high", "xhigh"],
    },
];

static MODEL_CATALOG: OnceLock<RwLock<ModelCatalog>> = OnceLock::new();

pub fn set_model_catalog(catalog: ModelCatalog) {
    let mut guard = global_catalog()
        .write()
        .expect("model catalog lock poisoned");
    *guard = catalog;
}

pub async fn v1_models() -> Json<ModelsResponse> {
    let guard = global_catalog()
        .read()
        .expect("model catalog lock poisoned");
    Json(guard.response())
}

fn global_catalog() -> &'static RwLock<ModelCatalog> {
    MODEL_CATALOG.get_or_init(|| RwLock::new(ModelCatalog::default()))
}

fn push_model_variants(data: &mut Vec<ModelItem>, entry: &ModelListEntry) {
    data.push(ModelItem {
        id: entry.base.clone(),
        object: "model",
        owned_by: entry.owned_by.clone(),
    });
    if entry.fast_variants {
        data.push(ModelItem {
            id: format!("{}-fast", entry.base),
            object: "model",
            owned_by: entry.owned_by.clone(),
        });
    }
    for suffix in &entry.suffixes {
        data.push(ModelItem {
            id: format!("{}-{suffix}", entry.base),
            object: "model",
            owned_by: entry.owned_by.clone(),
        });
        if entry.fast_variants {
            data.push(ModelItem {
                id: format!("{}-{suffix}-fast", entry.base),
                object: "model",
                owned_by: entry.owned_by.clone(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_catalog_loads_wrapped_yaml() {
        let catalog = ModelCatalog::from_str(
            r#"
models:
  - base: custom-model
    suffixes: [low, high]
    owned-by: custom
    fast-variants: false
"#,
        )
        .expect("catalog");

        let ids: Vec<String> = catalog.response().data.into_iter().map(|m| m.id).collect();
        assert_eq!(
            ids,
            ["custom-model", "custom-model-low", "custom-model-high"]
        );
    }
}
