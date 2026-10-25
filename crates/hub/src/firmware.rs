use std::path::Path;

use serde_json::Value as Json;

/// Board name in firmware file names → ESP Web Tools chip family.
pub const BOARDS: &[(&str, &str)] = &[("tdisplay-s3", "ESP32-S3")];

const SUFFIX: &str = ".factory.bin";

#[derive(Debug, Clone, PartialEq)]
pub struct Firmware {
    pub board: &'static str,
    pub chip_family: &'static str,
    pub version: String,
    pub file: String,
}

/// Parse `<board>-<version>.factory.bin`; `None` for anything else.
pub fn parse_name(file: &str) -> Option<Firmware> {
    let stem = file.strip_suffix(SUFFIX)?;
    let mut boards: Vec<&(&str, &str)> = BOARDS.iter().collect();
    boards.sort_by_key(|(b, _)| std::cmp::Reverse(b.len()));
    let (board, chip_family, version) =
        boards.into_iter().find_map(|(b, c)| stem.strip_prefix(*b)?.strip_prefix('-').map(|v| (*b, *c, v)))?;
    let valid = version.starts_with(|c: char| c.is_ascii_digit())
        && version.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'));
    valid.then(|| Firmware { board, chip_family, version: version.to_owned(), file: file.to_owned() })
}

/// The newest firmware per known board in `dir`, sorted by board.
pub fn scan(dir: &Path) -> std::io::Result<Vec<Firmware>> {
    let mut newest: Vec<Firmware> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let Some(fw) = entry.file_name().to_str().and_then(parse_name) else { continue };
        match newest.iter_mut().find(|f| f.board == fw.board) {
            Some(current) if version_key(&fw.version) > version_key(&current.version) => *current = fw,
            Some(_) => {}
            None => newest.push(fw),
        }
    }
    newest.sort_by(|a, b| a.board.cmp(b.board));
    Ok(newest)
}

/// ESP Web Tools manifest for `builds`; `None` when there is nothing to flash.
pub fn manifest(builds: &[Firmware]) -> Option<Json> {
    let version = builds.iter().map(|f| &f.version).max_by_key(|v| version_key(v))?;
    let builds: Vec<Json> = builds
        .iter()
        .map(|f| {
            serde_json::json!({
                "chipFamily": f.chip_family,
                "parts": [{"path": format!("/firmware/{}", f.file), "offset": 0}]
            })
        })
        .collect();
    Some(serde_json::json!({
        "name": "Peephole",
        "version": version,
        "new_install_prompt_erase": true,
        "builds": builds
    }))
}

/// Numeric release parts first; a release sorts above its pre-releases.
fn version_key(version: &str) -> (Vec<u64>, bool, String) {
    let (release, pre) = match version.split_once('-') {
        Some((r, p)) => (r, Some(p)),
        None => (version, None),
    };
    let release = release.split('+').next().unwrap_or(release);
    let parts = release.split('.').map(|p| p.parse().unwrap_or(0)).collect();
    (parts, pre.is_none(), pre.unwrap_or_default().to_owned())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_board_and_version_including_prereleases() {
        let fw = parse_name("tdisplay-s3-0.1.0.factory.bin").unwrap();
        assert_eq!((fw.board, fw.chip_family, fw.version.as_str()), ("tdisplay-s3", "ESP32-S3", "0.1.0"));
        assert_eq!(fw.file, "tdisplay-s3-0.1.0.factory.bin");
        assert_eq!(parse_name("tdisplay-s3-0.2.0-rc1.factory.bin").unwrap().version, "0.2.0-rc1");
    }

    #[test]
    fn rejects_unknown_boards_and_other_files() {
        for name in [
            "esp32c3-0.1.0.factory.bin",
            "tdisplay-s3-0.1.0.bin",
            "tdisplay-s3-.factory.bin",
            "tdisplay-s3-latest.factory.bin",
            "../tdisplay-s3-0.1.0.factory.bin",
            "README.md",
        ] {
            assert_eq!(parse_name(name), None, "{name}");
        }
    }

    #[test]
    fn scan_keeps_the_newest_version_by_number_not_text() {
        let dir = tempfile::tempdir().unwrap();
        for f in ["tdisplay-s3-0.9.0.factory.bin", "tdisplay-s3-0.10.0.factory.bin", "x.txt"] {
            std::fs::write(dir.path().join(f), b"").unwrap();
        }
        let found = scan(dir.path()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].version, "0.10.0");
    }

    #[test]
    fn manifest_has_one_build_per_board() {
        let fw = parse_name("tdisplay-s3-0.1.0.factory.bin").unwrap();
        assert_eq!(
            manifest(&[fw]).unwrap(),
            json!({
                "name": "Peephole",
                "version": "0.1.0",
                "new_install_prompt_erase": true,
                "builds": [{"chipFamily": "ESP32-S3", "parts": [{"path": "/firmware/tdisplay-s3-0.1.0.factory.bin", "offset": 0}]}]
            })
        );
        assert_eq!(manifest(&[]), None);
    }

    #[test]
    fn a_release_is_newer_than_its_prerelease() {
        assert!(version_key("0.2.0") > version_key("0.2.0-rc1"));
        assert!(version_key("0.2.0-rc1") > version_key("0.1.9"));
    }
}
