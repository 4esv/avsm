//! A venture: a directory with a mandate, and optionally `run` and `report`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::gate::Adopted;
use crate::mandate::Mandate;
use crate::record::{Claim, Entry, Record, Seq, Verdict};

/// The file a venture directory must hold.
pub const MANDATE_FILE: &str = "mandate.yaml";

/// A venture the kernel supervises.
#[derive(Clone, Debug)]
pub struct Venture {
    pub name: String,
    /// The directory. None for the built-in self venture.
    pub path: Option<PathBuf>,
    pub mandate: Mandate,
    /// Hash of the mandate text as adopted.
    pub hash: String,
    /// The declared record that adopted the mandate.
    pub adopted: Seq,
    /// Set when the mandate on disk no longer matches the adopted one, or
    /// cannot be read; the text says what is wrong. The gate then permits
    /// nothing for this venture until `avsm add` runs again.
    pub problem: Option<String>,
}

impl Venture {
    /// The venture's executable `report`, if it has one.
    pub fn report(&self) -> Option<PathBuf> {
        self.executable("report")
    }

    /// The venture's executable `run`, if it has one.
    pub fn run(&self) -> Option<PathBuf> {
        self.executable("run")
    }

    fn executable(&self, name: &str) -> Option<PathBuf> {
        let p = self.path.as_ref()?.join(name);
        p.is_file().then_some(p)
    }
}

/// Read and hash a venture directory's mandate.
pub fn read_mandate(dir: &Path) -> Result<(Mandate, String), String> {
    let path = dir.join(MANDATE_FILE);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mandate = Mandate::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((mandate, Mandate::hash(&text)))
}

/// Every venture the ledger says was adopted, from the latest declared
/// Adopt record of each, with its mandate read from disk now. A mandate
/// that changed or cannot be read leaves the venture listed with a
/// problem, and the gate permits it nothing.
pub fn load_all(entries: &[Entry]) -> BTreeMap<String, Venture> {
    let mut latest: BTreeMap<String, (Seq, String, String)> = BTreeMap::new();
    for e in entries {
        if let Record::Decision {
            claim: Claim::Declared,
            venture,
            verdict: Verdict::Adopt { mandate, path },
            ..
        } = &e.record
        {
            latest.insert(venture.clone(), (e.seq, mandate.clone(), path.clone()));
        }
    }
    let mut ventures = BTreeMap::new();
    for (name, (seq, hash, path)) in latest {
        if name == crate::gate::SELF {
            ventures.insert(name, crate::selfventure::venture(seq, &hash));
            continue;
        }
        let dir = PathBuf::from(&path);
        let (mandate, problem) = match read_mandate(&dir) {
            Ok((m, h)) if h == hash => (m, None),
            Ok((m, _)) => (
                m,
                Some("mandate changed on disk; run avsm add again".to_string()),
            ),
            Err(e) => (Mandate::empty(), Some(e)),
        };
        ventures.insert(
            name.clone(),
            Venture {
                name,
                path: Some(dir),
                mandate,
                hash,
                adopted: seq,
                problem,
            },
        );
    }
    ventures
}

/// The mandates the gate may use: adopted and unchanged.
pub fn adopted(ventures: &BTreeMap<String, Venture>) -> Vec<Adopted> {
    ventures
        .values()
        .filter(|v| v.problem.is_none())
        .map(|v| Adopted {
            seq: v.adopted,
            venture: v.name.clone(),
            hash: v.hash.clone(),
            mandate: v.mandate.clone(),
        })
        .collect()
}

/// The name of the venture in a directory: the directory's own name, which
/// must be one word and not `self`.
pub fn name_of(dir: &Path) -> Result<String, String> {
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("{} has no usable directory name", dir.display()))?;
    let one_word = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if !one_word {
        return Err(format!(
            "a venture directory name is one word (letters, digits, _ - .), not {name:?}"
        ));
    }
    if name == crate::gate::SELF {
        return Err("self is the built-in venture; name yours something else".to_string());
    }
    Ok(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_come_from_the_directory() {
        assert_eq!(name_of(Path::new("/x/counter")).unwrap(), "counter");
        assert!(name_of(Path::new("/x/self")).is_err());
        assert!(name_of(Path::new("/x/two words")).is_err());
    }
}
