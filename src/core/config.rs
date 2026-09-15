//! Strict per-backend defaults, resolved before any daemon or protocol work.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::backend::{Backend, ModelSettings};
use crate::cli::RunArgs;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    codex: Defaults,
    dsh: ProviderDefaults,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Defaults {
    model: Option<String>,
    effort: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ProviderDefaults {
    provider: Option<String>,
    model: Option<String>,
    effort: Option<String>,
}

pub struct Config {
    sections: HashMap<&'static str, ModelSettings>,
}

/// An override names the file, whereas the data-home setting names its base.
fn config_path() -> PathBuf {
    if let Some(path) = std::env::var_os("AGENT_BRIDGE_CONFIG").filter(|s| !s.is_empty()) {
        return PathBuf::from(path);
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| super::daemon::home_dir().map(|home| home.join(".local").join("share")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("agent-bridge").join("config.toml")
}

pub fn load() -> Result<Config> {
    let path = config_path();
    let file: FileConfig = match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text)
            .with_context(|| format!("parsing config file {}", path.display()))?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => FileConfig::default(),
        Err(err) => {
            return Err(err).with_context(|| format!("reading config file {}", path.display()));
        }
    };
    Ok(Config {
        sections: HashMap::from([
            (
                "codex",
                ModelSettings {
                    model: file.codex.model,
                    effort: file.codex.effort,
                    provider: None,
                },
            ),
            (
                "dsh",
                ModelSettings {
                    model: file.dsh.model,
                    effort: file.dsh.effort,
                    provider: file.dsh.provider,
                },
            ),
        ]),
    })
}

impl Config {
    pub fn resolve(&self, backend: &impl Backend, args: &RunArgs) -> ModelSettings {
        // Resuming preserves the existing thread's settings, just as the CLI's
        // conflicts forbid creation flags alongside --thread.
        if args.thread.is_some() {
            return ModelSettings::default();
        }
        // state_name is a table key, not a branch selecting backend behavior.
        let defaults = self.sections.get(backend.state_name());
        ModelSettings {
            model: args
                .model
                .clone()
                .or_else(|| defaults.and_then(|d| d.model.clone())),
            effort: args
                .effort
                .clone()
                .or_else(|| defaults.and_then(|d| d.effort.clone())),
            provider: defaults.and_then(|d| d.provider.clone()),
        }
    }
}
