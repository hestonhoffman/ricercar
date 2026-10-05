//! Plugins from outside the hub: a link to a binary, a GitHub repository
//! (the binary for this computer in its latest release), a file on this
//! computer, or another index in the hub's format. Nothing is run before
//! the user agreed: a link is only read, then `install` starts the binary
//! once to learn its id, and copies it next to the hub's installs.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::PluginInfo;
use super::catalog::{self, CatalogError, Entry, MAX_BINARY, sha256_hex, url_host};
use crate::config::PluginConfig;

/// Largest checksum file read next to a binary.
const MAX_SUMS: u64 = 64 * 1024;
/// Largest GitHub API answer read.
const MAX_API: u64 = 4 * 1024 * 1024;

/// Where an installed plugin came from, as stored in its `source`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Hub,
    /// Another index, by URL.
    Index(String),
    /// `owner/repo` on github.com.
    Github(String),
    /// A link to the binary.
    Url(String),
    File(PathBuf),
}

impl Origin {
    pub fn parse(s: &str) -> Option<Origin> {
        if s == "hub" {
            return Some(Origin::Hub);
        }
        let (kind, rest) = s.split_once(':')?;
        Some(match kind {
            "index" => Origin::Index(rest.to_string()),
            "github" => Origin::Github(rest.to_string()),
            "url" => Origin::Url(rest.to_string()),
            "file" => Origin::File(PathBuf::from(rest)),
            _ => return None,
        })
    }

    /// The origin of an installed plugin (`None`: declared by hand).
    pub fn of(p: &PluginConfig) -> Option<Origin> {
        catalog::installed_origin(p).and_then(|s| Origin::parse(&s))
    }

    /// Whether a plugin installed from `self` may be replaced by one from
    /// `other` without removing it first: the same source, or two local
    /// files (a plugin built again somewhere else).
    pub fn replaceable_by(&self, other: &Origin) -> bool {
        self == other || matches!((self, other), (Origin::File(_), Origin::File(_)))
    }
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Origin::Hub => write!(f, "hub"),
            Origin::Index(u) => write!(f, "index:{u}"),
            Origin::Github(r) => write!(f, "github:{r}"),
            Origin::Url(u) => write!(f, "url:{u}"),
            Origin::File(p) => write!(f, "file:{}", p.display()),
        }
    }
}

/// A binary in hand, not run yet.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub origin: Origin,
    /// Host it was downloaded from.
    pub host: Option<String>,
    /// The SHA-256 matched one its author published next to it.
    pub verified: bool,
    /// Version to record instead of the one the plugin gives (a release
    /// tag, compared with the next ones).
    pub version: Option<String>,
}

/// The latest release of a GitHub repository, and its binary for this
/// computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// `owner/repo`.
    pub repo: String,
    pub tag: String,
    pub asset_name: String,
    pub asset_url: String,
    /// SHA-256 published in the release (`<asset>.sha256` or a sums file).
    pub sha256: Option<String>,
}

/// What a link points at, read before asking the user anything.
#[derive(Debug, Clone)]
pub enum Link {
    Index { url: String, entries: Vec<Entry> },
    Github(Release),
    Binary(Candidate),
}

/// `owner/repo` when `link` is a GitHub repository page (or its releases
/// page), not a file in it.
pub fn github_repo(link: &str) -> Option<String> {
    let rest = link
        .trim()
        .strip_prefix("https://github.com/")
        .or_else(|| link.trim().strip_prefix("https://www.github.com/"))?;
    let rest = rest.split(['?', '#']).next()?.trim_end_matches('/');
    let parts: Vec<&str> = rest.split('/').collect();
    let ok = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    };
    let repo = match parts.as_slice() {
        [o, r] | [o, r, "releases"] | [o, r, "releases", "latest"] => (*o, *r),
        _ => return None,
    };
    let r = repo.1.strip_suffix(".git").unwrap_or(repo.1);
    (ok(repo.0) && ok(r)).then(|| format!("{}/{r}", repo.0))
}

/// Looks like a program: an ELF binary or a script.
fn executable(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x7fELF") || bytes.starts_with(b"#!")
}

/// The SHA-256 given for `name` in a checksum file: `<hash>` alone, or
/// `<hash>  <name>` lines (`sha256sum` output).
pub fn sum_for(text: &str, name: &str) -> Option<String> {
    let hex64 = |s: &str| {
        (s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())).then(|| s.to_ascii_lowercase())
    };
    let lone: Vec<&str> = text.split_whitespace().collect();
    if lone.len() == 1 {
        return hex64(lone[0]);
    }
    text.lines().find_map(|l| {
        let mut w = l.split_whitespace();
        let (h, n) = (w.next()?, w.next()?);
        let n = n.trim_start_matches('*');
        (n == name || n.rsplit('/').next() == Some(name))
            .then(|| hex64(h))
            .flatten()
    })
}

/// Read what a link points at. A path to a file on this computer is read
/// as a binary too.
pub fn read_link(link: &str) -> Result<Link, CatalogError> {
    let link = link.trim();
    if let Some(p) = link.strip_prefix("file://") {
        return read_file(Path::new(&crate::meta::percent_decode(p))).map(Link::Binary);
    }
    if link.starts_with('/') || link.starts_with("~/") {
        let p = match link.strip_prefix("~/") {
            Some(rest) => std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join(rest))
                .unwrap_or_else(|| PathBuf::from(link)),
            None => PathBuf::from(link),
        };
        return read_file(&p).map(Link::Binary);
    }
    if let Some(repo) = github_repo(link) {
        return latest_release(&repo).map(Link::Github);
    }
    if !link.starts_with("https://") {
        return Err(CatalogError::Invalid(
            "the link must start with https://".into(),
        ));
    }
    let bytes = catalog::fetch(link, MAX_BINARY, false)?;
    if bytes.first() == Some(&b'{') {
        let text = String::from_utf8_lossy(&bytes);
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
            && v.get("plugins").is_some_and(|p| p.is_array())
        {
            let entries = catalog::parse_index(&text, false)?;
            return Ok(Link::Index {
                url: link.to_string(),
                entries,
            });
        }
    }
    if !executable(&bytes) {
        return Err(CatalogError::Invalid(
            "this link is neither a plugin program nor a plugin index".into(),
        ));
    }
    let sha256 = sha256_hex(&bytes);
    // A `<file>.sha256` published next to it is checked when there is one.
    let name = link.rsplit('/').next().unwrap_or("");
    let published = catalog::fetch(&format!("{link}.sha256"), MAX_SUMS, false)
        .ok()
        .and_then(|b| sum_for(&String::from_utf8_lossy(&b), name));
    if let Some(expected) = &published
        && *expected != sha256
    {
        return Err(CatalogError::Checksum {
            expected: expected.clone(),
            got: sha256,
        });
    }
    Ok(Link::Binary(Candidate {
        bytes,
        sha256,
        origin: Origin::Url(link.to_string()),
        host: url_host(link),
        verified: published.is_some(),
        version: None,
    }))
}

/// A plugin binary on this computer (copied when installed).
pub fn read_file(path: &Path) -> Result<Candidate, CatalogError> {
    let path = std::fs::canonicalize(path).map_err(|e| CatalogError::Io(e.to_string()))?;
    let f = std::fs::File::open(&path).map_err(|e| CatalogError::Io(e.to_string()))?;
    if !f.metadata().is_ok_and(|m| m.is_file()) {
        return Err(CatalogError::Invalid("not a file".into()));
    }
    let bytes = catalog::read_limited(f, MAX_BINARY)?;
    if !executable(&bytes) {
        return Err(CatalogError::Invalid("not a program".into()));
    }
    Ok(Candidate {
        sha256: sha256_hex(&bytes),
        bytes,
        origin: Origin::File(path),
        host: None,
        verified: false,
        version: None,
    })
}

/// Words that name a CPU architecture in release file names.
fn arch_words(arch: &str) -> &'static [&'static str] {
    match arch {
        "x86_64" => &["x86_64", "x86-64", "amd64", "x64"],
        "aarch64" => &["aarch64", "arm64"],
        _ => &[],
    }
}

/// The release file to install on `arch`, among `names`: a binary (not an
/// archive, package or checksum) whose name gives this architecture, for
/// Linux when several do.
pub fn pick_asset(names: &[String], arch: &str) -> Option<usize> {
    const SKIP: [&str; 20] = [
        ".sha256",
        ".sha512",
        ".sig",
        ".minisig",
        ".asc",
        ".txt",
        ".md",
        ".json",
        ".tar.gz",
        ".tgz",
        ".tar.xz",
        ".tar.zst",
        ".tar.bz2",
        ".zip",
        ".deb",
        ".rpm",
        ".exe",
        ".dmg",
        ".msi",
        ".appimage",
    ];
    const OTHER_OS: [&str; 5] = ["windows", "darwin", "macos", "apple", "freebsd"];
    let words = arch_words(arch);
    let mut found: Vec<(usize, bool)> = names
        .iter()
        .enumerate()
        .filter_map(|(i, n)| {
            let n = n.to_ascii_lowercase();
            let wanted = !SKIP.iter().any(|s| n.ends_with(s))
                && !OTHER_OS.iter().any(|s| n.contains(s))
                && words.iter().any(|w| n.contains(w));
            wanted.then(|| (i, n.contains("linux")))
        })
        .collect();
    found.sort_by_key(|(_, linux)| !linux);
    found.first().map(|(i, _)| *i)
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
}

/// The latest release of `owner/repo` on GitHub and its binary for this
/// computer, with the checksum the author published for it, if any.
pub fn latest_release(repo: &str) -> Result<Release, CatalogError> {
    let bytes = catalog::fetch(
        &format!("https://api.github.com/repos/{repo}/releases/latest"),
        MAX_API,
        false,
    )?;
    let rel: ApiRelease =
        serde_json::from_slice(&bytes).map_err(|e| CatalogError::Invalid(e.to_string()))?;
    let names: Vec<String> = rel.assets.iter().map(|a| a.name.clone()).collect();
    let i = pick_asset(&names, catalog::current_arch()).ok_or(CatalogError::NoAsset)?;
    let asset = &rel.assets[i];
    if !asset.browser_download_url.starts_with("https://") {
        return Err(CatalogError::Invalid("not an https URL".into()));
    }
    let sums = [
        format!("{}.sha256", asset.name),
        "SHA256SUMS".to_string(),
        "SHA256SUMS.txt".to_string(),
        "sha256sums.txt".to_string(),
        "checksums.txt".to_string(),
    ];
    let sha256 = sums.iter().find_map(|s| {
        let a = rel.assets.iter().find(|a| a.name.eq_ignore_ascii_case(s))?;
        let b = catalog::fetch(&a.browser_download_url, MAX_SUMS, false).ok()?;
        sum_for(&String::from_utf8_lossy(&b), &asset.name)
    });
    Ok(Release {
        repo: repo.to_string(),
        tag: rel.tag_name,
        asset_name: asset.name.clone(),
        asset_url: asset.browser_download_url.clone(),
        sha256,
    })
}

/// Download a release's binary, checked against its published checksum.
pub fn download_release(r: &Release) -> Result<Candidate, CatalogError> {
    let bytes = catalog::fetch(&r.asset_url, MAX_BINARY, false)?;
    let sha256 = sha256_hex(&bytes);
    if let Some(expected) = &r.sha256
        && *expected != sha256
    {
        return Err(CatalogError::Checksum {
            expected: expected.clone(),
            got: sha256,
        });
    }
    Ok(Candidate {
        bytes,
        sha256,
        origin: Origin::Github(r.repo.clone()),
        host: url_host(&r.asset_url),
        verified: r.sha256.is_some(),
        version: Some(r.tag.trim_start_matches('v').to_string()),
    })
}

/// Whether the latest release is newer than the installed version.
pub fn release_is_newer(installed: &PluginConfig, r: &Release) -> bool {
    installed.version.as_deref().is_some_and(|v| {
        crate::update::compare(&r.tag, v) == std::cmp::Ordering::Greater
            && r.tag
                .trim_start_matches('v')
                .starts_with(|c: char| c.is_ascii_digit())
    })
}

/// Run the binary once to learn its id (see `host::probe`), then install
/// it as `<root>/<id>/<version>/<id>` and return the declaration to add.
/// A plugin already declared with the same id is replaced only when it was
/// installed from the same place (`Origin::replaceable_by`); else the user
/// has to remove it first.
pub fn install(
    c: &Candidate,
    root: &Path,
    host_version: &str,
    existing: &[PluginConfig],
) -> Result<(PluginConfig, PluginInfo), CatalogError> {
    let io = |e: std::io::Error| CatalogError::Io(e.to_string());
    std::fs::create_dir_all(root).map_err(io)?;
    let scratch = root.join(format!(
        ".probe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    std::fs::create_dir_all(&scratch).map_err(io)?;
    let info = (|| {
        let bin = scratch.join("plugin");
        std::fs::write(&bin, &c.bytes).map_err(io)?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).map_err(io)?;
        }
        super::host::probe(&bin, host_version, &scratch).map_err(CatalogError::Invalid)
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    let info = info?;
    if let Some(p) = existing.iter().find(|p| p.id == info.id) {
        let same = Origin::of(p).is_some_and(|o| o.replaceable_by(&c.origin));
        if !same {
            return Err(CatalogError::Taken(info.id));
        }
    }
    let version = c.version.clone().unwrap_or_else(|| info.version.clone());
    let path = catalog::place(&c.bytes, &info.id, &version, root)?;
    Ok((
        PluginConfig {
            id: info.id.clone(),
            command: path,
            args: Vec::new(),
            enabled: true,
            version: Some(if version.is_empty() {
                "0".into()
            } else {
                version
            }),
            host: c.host.clone(),
            source: Some(c.origin.to_string()),
            settings: Default::default(),
        },
        info,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_round_trip() {
        for o in [
            Origin::Hub,
            Origin::Index("https://x.org/index.json".into()),
            Origin::Github("a/b".into()),
            Origin::Url("https://x.org/p".into()),
            Origin::File("/opt/p".into()),
        ] {
            assert_eq!(Origin::parse(&o.to_string()), Some(o));
        }
        assert_eq!(Origin::parse("nope:x"), None);
        assert!(Origin::File("/a".into()).replaceable_by(&Origin::File("/b".into())));
        assert!(!Origin::Hub.replaceable_by(&Origin::File("/b".into())));
        assert!(!Origin::Github("a/b".into()).replaceable_by(&Origin::Github("a/c".into())));
    }

    #[test]
    fn github_links() {
        let r = |s: &str| github_repo(s);
        assert_eq!(
            r("https://github.com/someone/plug").as_deref(),
            Some("someone/plug")
        );
        assert_eq!(
            r("https://github.com/someone/plug/").as_deref(),
            Some("someone/plug")
        );
        assert_eq!(
            r("https://github.com/someone/plug.git").as_deref(),
            Some("someone/plug")
        );
        assert_eq!(
            r("https://github.com/someone/plug/releases/latest").as_deref(),
            Some("someone/plug")
        );
        assert_eq!(r("https://github.com/someone"), None);
        assert_eq!(
            r("https://github.com/someone/plug/releases/download/v1/p-x86_64"),
            None
        );
        assert_eq!(r("https://github.com/../x"), None);
        assert_eq!(r("http://github.com/a/b"), None);
    }

    #[test]
    fn asset_choice() {
        let n = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let names = n(&[
            "plug-x86_64-linux.sha256",
            "plug-aarch64-linux",
            "plug-x86_64-windows.exe",
            "plug-x86_64-apple-darwin",
            "plug-x86_64.tar.gz",
            "plug-x86_64-unknown-linux-musl",
            "SHA256SUMS",
        ]);
        assert_eq!(pick_asset(&names, "x86_64"), Some(5));
        assert_eq!(pick_asset(&names, "aarch64"), Some(1));
        assert_eq!(pick_asset(&names, "riscv64"), None);
        assert_eq!(
            pick_asset(&n(&["plug-amd64", "plug-arm64"]), "x86_64"),
            Some(0)
        );
        assert_eq!(pick_asset(&n(&["plug"]), "x86_64"), None);
    }

    #[test]
    fn checksum_files() {
        let h = "ab".repeat(32);
        assert_eq!(sum_for(&format!("{h}\n"), "x"), Some(h.clone()));
        let sums = format!("{}  other\n{h} *plug-x86_64\n", "cd".repeat(32));
        assert_eq!(sum_for(&sums, "plug-x86_64"), Some(h.clone()));
        assert_eq!(sum_for(&sums, "missing"), None);
        assert_eq!(sum_for("not a hash", "x"), None);
        assert_eq!(sum_for(&format!("{}\n", h.to_uppercase()), "x"), Some(h));
    }

    #[test]
    fn files_must_be_programs() {
        let dir = tempfile::tempdir().unwrap();
        let text = dir.path().join("notes.txt");
        std::fs::write(&text, "hello").unwrap();
        assert!(matches!(read_file(&text), Err(CatalogError::Invalid(_))));
        let script = dir.path().join("p");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        let c = read_file(&script).unwrap();
        assert!(matches!(c.origin, Origin::File(_)) && !c.verified);
        assert!(read_file(dir.path()).is_err());
        assert!(matches!(
            read_link("http://example.org/p"),
            Err(CatalogError::Invalid(_))
        ));
    }

    #[test]
    fn newer_release() {
        let p = PluginConfig {
            id: "p".into(),
            command: "/x".into(),
            args: vec![],
            enabled: true,
            version: Some("0.2.0".into()),
            host: None,
            source: Some("github:a/b".into()),
            settings: Default::default(),
        };
        let rel = |t: &str| Release {
            repo: "a/b".into(),
            tag: t.into(),
            asset_name: String::new(),
            asset_url: String::new(),
            sha256: None,
        };
        assert!(release_is_newer(&p, &rel("v0.3.0")));
        assert!(!release_is_newer(&p, &rel("v0.2.0")));
        assert!(!release_is_newer(&p, &rel("nightly")));
    }
}
