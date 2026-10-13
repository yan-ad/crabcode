use anyhow::{bail, Context, Result};
use semver::Version;
use serde::Deserialize;
use std::process::{Command, Stdio};
use std::time::Duration;

mod progress;

pub(crate) const GITHUB_REPO: &str = "yan-ad/crabcode";

/// Effective build version, including timestamp versions in preview builds.
pub(crate) fn current_version() -> String {
    crate::version::CURRENT.to_string()
}

/// Upgrade without streaming installer output into the active TUI.
pub(crate) fn upgrade_noninteractive(target: Option<&str>) -> Result<String> {
    let release_tag = match target {
        Some(target) if target != "latest" => target_release_tag(target)?,
        _ => latest_release_tag_captured()?,
    };
    run_installer(&release_tag)?;
    Ok(release_tag)
}

const PREVIEW_RELEASES_URL: &str =
    "https://api.github.com/repos/yan-ad/crabcode/releases?per_page=100";

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
}

fn latest_preview_release_tag(releases: Vec<Release>) -> Result<String> {
    releases
        .into_iter()
        .filter(|release| target_release_tag(&release.tag_name).is_ok())
        .max_by(|left, right| {
            left.published_at
                .as_deref()
                .or(left.created_at.as_deref())
                .cmp(
                    &right
                        .published_at
                        .as_deref()
                        .or(right.created_at.as_deref()),
                )
        })
        .map(|release| release.tag_name)
        .context("no crabcode preview release found")
}

#[derive(Debug, PartialEq, Eq)]
enum VersionCheck {
    UpToDate { current: Version, latest: Version },
    UpdateAvailable { current: Version, latest: Version },
}

fn check_version(current: &str, release_tag: &str) -> Result<VersionCheck> {
    let current = Version::parse(current).context("invalid current version")?;
    let latest = Version::parse(release_tag.trim_start_matches('v'))
        .with_context(|| format!("invalid release tag {release_tag:?}"))?;

    if latest > current {
        Ok(VersionCheck::UpdateAvailable { current, latest })
    } else {
        Ok(VersionCheck::UpToDate { current, latest })
    }
}

fn target_release_tag(target: &str) -> Result<String> {
    if target.strip_prefix("gondescode-").is_some_and(|commit| {
        ((7..=40).contains(&commit.len()) || commit.len() == 64)
            && commit.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        return Ok(target.to_owned());
    }

    bail!("invalid target release {target:?}; expected a gondescode-<commit> preview tag or latest")
}

fn run_upgrade<F>(check: VersionCheck, install: F) -> Result<VersionCheck>
where
    F: FnOnce() -> Result<()>,
{
    if matches!(check, VersionCheck::UpdateAvailable { .. }) {
        install()?;
    }

    Ok(check)
}

async fn latest_release_tag() -> Result<String> {
    let releases = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?
        .get(PREVIEW_RELEASES_URL)
        .header(reqwest::header::USER_AGENT, "crabcode-upgrade")
        .send()
        .await
        .context("failed to check for the latest crabcode release")?
        .error_for_status()
        .context("failed to check for the latest crabcode release")?
        .json::<Vec<Release>>()
        .await
        .context("failed to read the latest crabcode release")?;

    latest_preview_release_tag(releases)
}

fn latest_release_tag_captured() -> Result<String> {
    let output = Command::new("curl")
        .args([
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "-fsSL",
            "--max-time",
            "10",
            "-H",
            "User-Agent: crabcode-upgrade",
            PREVIEW_RELEASES_URL,
        ])
        .stdin(Stdio::null())
        .output()
        .context("failed to check the latest preview release")?;
    if !output.status.success() {
        bail!(
            "preview release lookup failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    latest_preview_release_tag(serde_json::from_slice(&output.stdout)?)
}

#[cfg(not(target_os = "windows"))]
fn run_installer(release_tag: &str) -> Result<()> {
    use std::io::Write;
    let mut script = tempfile::NamedTempFile::with_suffix(".sh")?;
    script.write_all(include_bytes!("../install.sh"))?;
    script.flush()?;
    let output = Command::new("sh")
        .arg(script.path())
        .arg(release_tag)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("failed to start the crabcode installer")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail = stderr
            .chars()
            .rev()
            .take(800)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>();
        bail!("crabcode installer exited with {}\n{tail}", output.status);
    }

    Ok(())
}

#[cfg(target_os = "windows")]
fn run_installer(_release_tag: &str) -> Result<()> {
    bail!("automatic upgrades are not supported on Windows; reinstall with npm, cargo, or the latest GitHub release")
}

pub async fn upgrade(target: Option<&str>) -> Result<()> {
    eprintln!("Installed version: {}", current_version());
    let release_tag = match target {
        Some(target) if target != "latest" => target_release_tag(target)?,
        _ => latest_release_tag().await?,
    };
    if release_tag.starts_with("gondescode-") {
        eprintln!("\nUpgrading crabcode version: {release_tag}");
        progress::run(|| run_installer(&release_tag))?;
        eprintln!("Upgraded crabcode to preview {release_tag}. Restart crabcode to apply.");
        return Ok(());
    }

    let check = check_version(crate::version::CURRENT, &release_tag)?;
    let check = run_upgrade(check, || run_installer(&release_tag))?;

    match check {
        VersionCheck::UpToDate { current, .. } => {
            eprintln!("crabcode v{current} is already up to date.")
        }
        VersionCheck::UpdateAvailable { current, latest } => {
            eprintln!("Upgraded crabcode from v{current} to v{latest}.")
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_a_newer_release_tag() {
        assert_eq!(
            check_version("0.0.9", "v0.0.10").unwrap(),
            VersionCheck::UpdateAvailable {
                current: Version::parse("0.0.9").unwrap(),
                latest: Version::parse("0.0.10").unwrap(),
            }
        );
    }

    #[test]
    fn detects_an_up_to_date_release_tag() {
        assert_eq!(
            check_version("0.1.0", "v0.1.0").unwrap(),
            VersionCheck::UpToDate {
                current: Version::parse("0.1.0").unwrap(),
                latest: Version::parse("0.1.0").unwrap(),
            }
        );
    }

    #[test]
    fn accepts_an_explicit_preview_target() {
        assert_eq!(
            target_release_tag("gondescode-0123456789abcdef").unwrap(),
            "gondescode-0123456789abcdef"
        );
    }

    #[test]
    fn rejects_an_invalid_explicit_target() {
        let error = target_release_tag("nightly").unwrap_err();

        assert_eq!(
            error.to_string(),
            "invalid target release \"nightly\"; expected a gondescode-<commit> preview tag or latest"
        );
    }

    #[test]
    fn rejects_tags_that_could_escape_the_release_url() {
        for target in [
            "gondescode-",
            "gondescode-abc",
            "gondescode-abcd123;echo",
            "gondescode-../../main",
            "gondescode-abcd123\n",
        ] {
            assert!(target_release_tag(target).is_err());
        }
    }

    #[test]
    fn selects_latest_published_preview_instead_of_api_order() {
        let releases = vec![
            Release {
                tag_name: "gondescode-0000000".to_string(),
                published_at: Some("2026-10-01T04:24:03Z".to_string()),
                created_at: Some("2026-10-01T04:13:03Z".to_string()),
            },
            Release {
                tag_name: "gondescode-1111111".to_string(),
                published_at: Some("2026-10-01T04:47:59Z".to_string()),
                created_at: Some("2026-10-01T04:39:53Z".to_string()),
            },
        ];

        assert_eq!(
            latest_preview_release_tag(releases).unwrap(),
            "gondescode-1111111"
        );
    }

    #[test]
    fn latest_selection_ignores_invalid_tags_and_uses_creation_date() {
        let releases = vec![
            Release {
                tag_name: "gondescode-abcd123;echo".into(),
                published_at: Some("2026-10-14T00:00:00Z".into()),
                created_at: None,
            },
            Release {
                tag_name: "v0.0.14".into(),
                published_at: Some("2026-10-14T00:00:00Z".into()),
                created_at: None,
            },
            Release {
                tag_name: "gondescode-abcdef0".into(),
                published_at: None,
                created_at: Some("2026-10-13T00:00:00Z".into()),
            },
        ];
        assert_eq!(
            latest_preview_release_tag(releases).unwrap(),
            "gondescode-abcdef0"
        );
    }

    #[test]
    fn latest_selection_fails_without_a_valid_preview() {
        assert!(latest_preview_release_tag(Vec::new()).is_err());
    }

    #[test]
    fn current_version_uses_effective_preview_build_version() {
        assert_eq!(current_version(), crate::version::CURRENT);
    }

    #[test]
    fn runs_the_installer_only_when_an_upgrade_is_available() {
        let check = check_version("0.0.9", "v0.1.0").unwrap();
        let mut installed = false;

        let result = run_upgrade(check, || {
            installed = true;
            Ok(())
        })
        .unwrap();

        assert!(installed);
        assert!(matches!(result, VersionCheck::UpdateAvailable { .. }));
    }

    #[test]
    fn reports_installer_failures() {
        let check = check_version("0.0.9", "v0.1.0").unwrap();

        let error = run_upgrade(check, || bail!("installer failed")).unwrap_err();

        assert_eq!(error.to_string(), "installer failed");
    }
}
