//! config.yaml: when the nightly cycle runs and which backends roles may
//! use. The keys file beside it is read by the warden alone.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use serde::{Deserialize, Serialize};

pub const CONFIG_FILE: &str = "config.yaml";
pub const KEYS_FILE: &str = "keys";

/// The daemon's settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Local time of the nightly cycle, as HH:MM.
    #[serde(default = "default_nightly")]
    pub nightly: String,
    /// The backend a mandate gets when it names none.
    #[serde(default)]
    pub default: Option<String>,
    /// Backends by name.
    #[serde(default)]
    pub backends: BTreeMap<String, Backend>,
}

fn default_nightly() -> String {
    "03:00".to_string()
}

impl Default for Config {
    fn default() -> Config {
        Config {
            nightly: default_nightly(),
            default: None,
            backends: BTreeMap::new(),
        }
    }
}

/// Something that answers a prompt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum Backend {
    /// The Anthropic Messages API, or anything that speaks it.
    Anthropic {
        #[serde(default = "anthropic_url")]
        url: String,
        model: String,
        /// The name of the key in the keys file.
        #[serde(default = "anthropic_key")]
        key: String,
        /// Reasoning effort, when the model takes one: low, medium, high.
        #[serde(default)]
        effort: Option<String>,
    },
    /// The OpenAI Chat Completions API: OpenAI, DeepSeek, Ollama, vLLM,
    /// llama.cpp, LM Studio and the like.
    Openai {
        url: String,
        model: String,
        /// The name of the key in the keys file, if the server wants one.
        #[serde(default)]
        key: Option<String>,
    },
    /// A local command: the prompt as JSON on stdin, the reply on stdout.
    /// Any classifier, fine-tune or local model fits behind one.
    Command { run: String },
}

fn anthropic_url() -> String {
    "https://api.anthropic.com/v1/messages".to_string()
}

fn anthropic_key() -> String {
    "anthropic".to_string()
}

impl Config {
    /// Read `config.yaml` in a home directory; a missing file means defaults.
    pub fn load(home: &Path) -> Result<Config, String> {
        let path = home.join(CONFIG_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => Config::parse(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    /// Parse and check config text.
    pub fn parse(text: &str) -> Result<Config, String> {
        let config: Config = serde_yaml::from_str(text).map_err(|e| e.to_string())?;
        config.nightly_minutes()?;
        if let Some(d) = &config.default {
            if !config.backends.contains_key(d) {
                return Err(format!(
                    "default names a backend that is not listed: {d}; list it under backends"
                ));
            }
        }
        for (name, b) in &config.backends {
            let one_word = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
            if !one_word {
                return Err(format!("a backend name is one word, not {name:?}"));
            }
            if let Backend::Anthropic {
                effort: Some(e), ..
            } = b
            {
                if !matches!(e.as_str(), "low" | "medium" | "high" | "xhigh" | "max") {
                    return Err(format!(
                        "backend {name}: effort is low, medium, high, xhigh or max, not {e:?}"
                    ));
                }
            }
        }
        Ok(config)
    }

    /// The nightly time as minutes after local midnight.
    pub fn nightly_minutes(&self) -> Result<u32, String> {
        let bad = || format!("nightly is a time like 03:00, not {:?}", self.nightly);
        let (h, m) = self.nightly.split_once(':').ok_or_else(bad)?;
        let h: u32 = h.parse().map_err(|_| bad())?;
        let m: u32 = m.parse().map_err(|_| bad())?;
        if h > 23 || m > 59 {
            return Err(bad());
        }
        Ok(h * 60 + m)
    }

    /// The backend a mandate asks for, or the default.
    pub fn backend(&self, name: Option<&str>) -> Result<(String, &Backend), String> {
        let name = match name.or(self.default.as_deref()) {
            Some(n) => n,
            None => {
                return Err(
                    "no backend configured; add one under backends in config.yaml and name it as default"
                        .to_string(),
                )
            }
        };
        self.backends
            .get(name)
            .map(|b| (name.to_string(), b))
            .ok_or_else(|| format!("no backend named {name} in config.yaml"))
    }
}

/// The keys file: one `name=value` per line. Only the warden calls this.
/// A missing file means no keys; a file others can read is refused.
pub fn load_keys(home: &Path) -> Result<BTreeMap<String, String>, String> {
    let path = home.join(KEYS_FILE);
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(format!(
            "{} must be readable by you alone; run: chmod 600 {}",
            path.display(),
            path.display()
        ));
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut keys = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, value) = line.split_once('=').ok_or_else(|| {
            format!(
                "{} line {}: a key line looks like name=value",
                path.display(),
                i + 1
            )
        })?;
        keys.insert(name.trim().to_string(), value.trim().to_string());
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_backends_of_every_kind() {
        let c = Config::parse(
            "nightly: \"02:30\"\ndefault: a\nbackends:\n  a: {kind: anthropic, model: claude-opus-5-5}\n  d: {kind: openai, url: http://x/v1/chat/completions, model: m, key: deepseek}\n  k: {kind: command, run: ./classify}\n",
        )
        .unwrap();
        assert_eq!(c.nightly_minutes().unwrap(), 150);
        assert_eq!(c.backend(None).unwrap().0, "a");
        assert_eq!(c.backend(Some("k")).unwrap().0, "k");
        assert!(
            matches!(c.backend(Some("a")).unwrap().1, Backend::Anthropic { url, key, .. } if url.contains("api.anthropic.com") && key == "anthropic")
        );
        assert!(c
            .backend(Some("zz"))
            .unwrap_err()
            .contains("no backend named zz"));
    }

    #[test]
    fn refuses_bad_values() {
        assert!(Config::parse("nightly: late\n")
            .unwrap_err()
            .contains("03:00"));
        assert!(Config::parse("default: x\n")
            .unwrap_err()
            .contains("not listed"));
        assert!(
            Config::parse("backends:\n  a: {kind: anthropic, model: m, effort: wild}\n")
                .unwrap_err()
                .contains("effort")
        );
        assert!(Config::parse("backends:\n  a: {kind: magic}\n").is_err());
        assert!(Config::default()
            .backend(None)
            .unwrap_err()
            .contains("no backend configured"));
    }

    #[test]
    fn keys_must_be_private() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load_keys(tmp.path()).unwrap().is_empty());
        let path = tmp.path().join(KEYS_FILE);
        std::fs::write(&path, "anthropic = sk-1\n# comment\n\ndeepseek=k2\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_keys(tmp.path()).unwrap_err().contains("chmod 600"));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let keys = load_keys(tmp.path()).unwrap();
        assert_eq!(keys["anthropic"], "sk-1");
        assert_eq!(keys["deepseek"], "k2");
    }
}
