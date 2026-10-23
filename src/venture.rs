//! A venture: a directory with a mandate, and optionally `run` and `report`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::gate::Adopted;
use crate::index::Adoption;
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
    /// Set when no mandate can be had for the venture: a 1.0 adoption whose
    /// file no longer reads or has changed, or text that does not match its
    /// hash. The gate then permits nothing for this venture until
    /// `avsm add` runs again.
    pub problem: Option<String>,
    /// Set when the file on disk no longer matches the adopted text, or
    /// cannot be read. The adopted text keeps running; it takes
    /// `avsm add` to adopt what is on disk.
    pub note: Option<String>,
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

/// The relative programs a mandate runs that are not an executable file in
/// its directory, one line each. Only a `run:` whose command starts with a
/// relative path (`./reset`) is checked: a bare word or a shell line is
/// the shell's to find. Also the venture's `report` file, when it exists
/// and cannot be executed.
pub fn missing_programs(dir: &Path, m: &Mandate) -> Vec<String> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |p: &Path| {
        std::fs::metadata(p).is_ok_and(|md| md.is_file() && md.permissions().mode() & 0o111 != 0)
    };
    let mut runs: Vec<(String, &str)> = Vec::new();
    for p in &m.permitted {
        if let Some(r) = &p.run {
            runs.push((format!("lever {}", p.name), r));
        }
    }
    for g in &m.guards {
        if let Some(r) = &g.run {
            runs.push((format!("guard {}", g.name), r));
        }
    }
    for a in &m.audits {
        if let Some(r) = &a.run {
            runs.push((format!("audit {}", a.name), r));
        }
    }
    for r in &m.readings {
        runs.push((format!("reading {}", r.name), &r.run));
    }
    let mut out = Vec::new();
    for (what, run) in runs {
        let Some(word) = run.split_whitespace().next() else {
            continue;
        };
        if word.contains('/') && !word.starts_with('/') && !executable(&dir.join(word)) {
            out.push(format!(
                "{what} runs {word}, which is not an executable file in {}",
                dir.display()
            ));
        }
    }
    let report = dir.join("report");
    if report.is_file() && !executable(&report) {
        out.push(format!(
            "report is not executable in {}; chmod +x {}",
            dir.display(),
            report.display()
        ));
    }
    out
}

/// Read and hash a venture directory's mandate: the mandate, the hash of
/// its text and the text.
pub fn read_mandate(dir: &Path) -> Result<(Mandate, String, String), String> {
    let path = dir.join(MANDATE_FILE);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mandate = Mandate::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let hash = Mandate::hash(&text);
    Ok((mandate, hash, text))
}

/// Every venture the ledger says was adopted, from the latest declared
/// Adopt record of each. The mandate in force is the text that record
/// holds; a 1.0 record has none, and its mandate is read from disk now,
/// and a change there leaves the venture listed with a problem.
pub fn load_all(entries: &[Entry]) -> BTreeMap<String, Venture> {
    let mut latest: BTreeMap<String, Adoption> = BTreeMap::new();
    for e in entries {
        if let Record::Decision {
            claim: Claim::Declared,
            venture,
            verdict:
                Verdict::Adopt {
                    mandate,
                    path,
                    text,
                },
            ..
        } = &e.record
        {
            latest.insert(
                venture.clone(),
                Adoption {
                    seq: e.seq,
                    hash: mandate.clone(),
                    path: path.clone(),
                    text: text.clone(),
                },
            );
        }
    }
    from_adoptions(&latest)
}

/// Every venture from the latest declared adoption of each: name, then
/// the adopting record, the mandate's hash, its directory and its text.
pub fn from_adoptions(latest: &BTreeMap<String, Adoption>) -> BTreeMap<String, Venture> {
    let mut ventures = BTreeMap::new();
    for (name, a) in latest {
        let (name, seq, hash, path) = (name.clone(), a.seq, a.hash.clone(), &a.path);
        if name == crate::gate::SELF {
            ventures.insert(name, crate::selfventure::venture(seq, &hash));
            continue;
        }
        let dir = PathBuf::from(path);
        let changed = "mandate changed on disk; run avsm add again".to_string();
        let on_disk = read_mandate(&dir);
        let (mandate, problem, note) = match (&a.text, on_disk) {
            // 2.0: the adopted text runs; the disk is only compared.
            (Some(text), disk) => {
                let note = match disk {
                    Ok((_, h, _)) if h == hash => None,
                    Ok(_) => Some(changed),
                    Err(e) => Some(e),
                };
                match Mandate::parse(text) {
                    Ok(m) if Mandate::hash(text) == hash => (m, None, note),
                    Ok(_) => (
                        Mandate::empty(),
                        Some("the adopted text does not match its hash".to_string()),
                        note,
                    ),
                    Err(e) => (
                        Mandate::empty(),
                        Some(format!("the adopted text does not parse: {e}")),
                        note,
                    ),
                }
            }
            // 1.0: no text in the ledger; the disk is all there is.
            (None, Ok((m, h, _))) if h == hash => (m, None, None),
            (None, Ok((m, _, _))) => (m, Some(changed), None),
            (None, Err(e)) => (Mandate::empty(), Some(e), None),
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
                note,
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
