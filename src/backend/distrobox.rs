use std::path::Path;
use std::sync::Arc;

use crate::args::BackendArg;
use crate::backend::VersionInfo;
use crate::backend::backend_type::BackendType;
use crate::backend::options::BackendOptions;
use crate::cmd::CmdLineRunner;
use crate::config::{Settings, SettingsExt};
use crate::hash::hash_sha256_to_str;
use crate::install_context::InstallContext;
use crate::toolset::{ToolVersion, ToolVersionOptions};
use crate::ui::progress_report::SingleReport;
use crate::{backend::Backend, config::Config, file, timeout};
use async_trait::async_trait;
use eyre::{Result, bail};

pub(crate) const EXPERIMENTAL: bool = true;

/// Records which container an install exported its commands from, so
/// uninstall removes that container even if the tool options changed since.
const CONTAINER_MARKER: &str = ".mise-distrobox-container";

/// Exports commands from a [distrobox](https://distrobox.it) container built
/// from an OCI image. `distrobox:IMAGE:bin1,bin2@TAG` creates a container from
/// `IMAGE:TAG` and writes a wrapper for each bin to `bin/`.
#[derive(Debug)]
pub(crate) struct DistroboxBackend {
    ba: Arc<BackendArg>,
}

#[derive(Debug, Clone, Copy)]
struct DistroboxOptions<'a> {
    values: BackendOptions<'a>,
}

impl<'a> DistroboxOptions<'a> {
    fn new(raw: &'a ToolVersionOptions) -> Self {
        Self {
            values: BackendOptions::new(raw),
        }
    }

    /// Space-separated packages distrobox installs while creating the
    /// container, for commands the image does not ship.
    fn packages(&self) -> Option<&'a str> {
        self.values.str("packages").filter(|p| !p.trim().is_empty())
    }

    fn distrobox(&self) -> &'a str {
        self.values.str("distrobox").unwrap_or("distrobox")
    }
}

/// The parsed `IMAGE:bin1,bin2` tool name.
#[derive(Debug, PartialEq, Eq)]
struct DistroboxTool {
    image: String,
    bins: Vec<String>,
}

#[async_trait]
impl Backend for DistroboxBackend {
    fn get_type(&self) -> BackendType {
        BackendType::Distrobox
    }

    fn ba(&self) -> &Arc<BackendArg> {
        &self.ba
    }

    fn supports_lockfile_url(&self) -> bool {
        false
    }

    async fn _list_remote_versions(&self, _config: &Arc<Config>) -> Result<Vec<VersionInfo>> {
        let tool = parse_tool(&self.tool_name())?;
        let image = tool.image;
        timeout::run_with_timeout_async(
            async || {
                let tags = mise_oci::registry::list_tags(&image).await?;
                Ok(tags
                    .into_iter()
                    .filter(|tag| validate_tag(tag).is_ok() && !is_artifact_tag(tag))
                    .map(|version| VersionInfo {
                        version,
                        ..Default::default()
                    })
                    .collect())
            },
            Settings::get().fetch_remote_versions_timeout(),
        )
        .await
    }

    /// Registry tags are listed in lexical order, so the last one is rarely the
    /// newest. Follow the container convention instead: `latest` means the
    /// `latest` tag whenever the image publishes one.
    async fn latest_stable_version(&self, config: &Arc<Config>) -> Result<Option<String>> {
        let versions = self.list_remote_versions(config).await?;
        Ok(versions
            .iter()
            .any(|v| v == "latest")
            .then(|| "latest".to_string()))
    }

    // Runs before `--force` removes an existing install, so a rejected
    // reinstall leaves the working one in place.
    async fn prepare_install_version(
        &self,
        _ctx: &InstallContext,
        tv: ToolVersion,
    ) -> Result<ToolVersion> {
        Settings::get().ensure_experimental("the distrobox backend")?;
        parse_tool(&self.tool_name())?;
        validate_tag(&tv.version)?;
        if !cfg!(target_os = "linux") {
            bail!("the distrobox backend only supports Linux");
        }
        Ok(tv)
    }

    async fn install_version_(&self, ctx: &InstallContext, tv: ToolVersion) -> Result<ToolVersion> {
        let tool = parse_tool(&self.tool_name())?;
        validate_tag(&tv.version)?;
        let request_opts = tv.request.options();
        let opts = DistroboxOptions::new(&request_opts);
        let distrobox = opts.distrobox();
        if !distrobox_available(distrobox) {
            bail!(
                "the distrobox backend needs `{distrobox}` on PATH.\n\
                 Install it from https://distrobox.it or your distribution's packages."
            );
        }
        let pr = ctx.pr.as_ref();
        let image = format!("{}:{}", tool.image, tv.version);
        let name = container_name(&tool, &tv.version, opts.packages());

        // A reinstall (or a previous failed attempt) may have left the
        // container behind; recreate it so a moved tag is pulled again.
        if container_exists(distrobox, &name).await? {
            remove_container(distrobox, &name, pr)?;
        }
        let mut create = CmdLineRunner::new(distrobox)
            .args(["create", "--yes", "--name", &name, "--image", &image])
            .with_pr(pr);
        if let Some(packages) = opts.packages() {
            create = create.args(["--additional-packages", packages]);
        }
        create.execute()?;

        let bin_dir = tv.install_path().join("bin");
        if let Err(err) = export_bins(distrobox, &name, &tool.bins, &bin_dir, pr).await {
            // mise removes the half-written install dir; take the container
            // with it so a failed install leaves nothing behind.
            if let Err(rm_err) = remove_container(distrobox, &name, pr) {
                warn!("failed to remove distrobox container {name}: {rm_err:#}");
            }
            return Err(err);
        }
        file::write(tv.install_path().join(CONTAINER_MARKER), &name)?;
        Ok(tv)
    }

    async fn uninstall_version_impl(
        &self,
        _config: &Arc<Config>,
        pr: &dyn SingleReport,
        tv: &ToolVersion,
    ) -> Result<()> {
        let marker = tv.install_path().join(CONTAINER_MARKER);
        let Ok(name) = file::read_to_string(&marker) else {
            return Ok(());
        };
        let name = name.trim();
        if validate_container_name(name).is_err() {
            warn!("not removing distrobox container with unexpected name {name:?}");
            return Ok(());
        }
        let request_opts = tv.request.options();
        let distrobox = DistroboxOptions::new(&request_opts).distrobox().to_string();
        if !distrobox_available(&distrobox) {
            warn!("{distrobox} not found; leaving distrobox container {name} in place");
            return Ok(());
        }
        if container_exists(&distrobox, name).await? {
            remove_container(&distrobox, name, pr)?;
        }
        Ok(())
    }
}

impl DistroboxBackend {
    pub(crate) fn from_arg(ba: BackendArg) -> Self {
        Self { ba: Arc::new(ba) }
    }
}

/// Options that only matter while installing, so a cached lockfile entry never
/// pins them.
pub(crate) fn install_time_option_keys() -> Vec<String> {
    vec!["packages".into(), "distrobox".into()]
}

fn parse_tool(tool: &str) -> Result<DistroboxTool> {
    let usage = || {
        eyre::eyre!(
            "expected distrobox:IMAGE:bin1,bin2 (e.g. distrobox:registry.fedoraproject.org/fedora-toolbox:htop,jq), got distrobox:{tool}"
        )
    };
    // The last `:` separates the bins, so a registry port stays in the image.
    let (image, bins) = tool.rsplit_once(':').ok_or_else(usage)?;
    let valid_image = !image.is_empty()
        && !image.starts_with(['-', '/'])
        && !image.contains("..")
        && !image.contains("//")
        && image
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | ':'));
    if !valid_image {
        return Err(usage());
    }
    let bins: Vec<String> = bins
        .split(',')
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(String::from)
        .collect();
    if bins.is_empty() {
        return Err(usage());
    }
    for bin in &bins {
        validate_bin(bin)?;
    }
    Ok(DistroboxTool {
        image: image.to_string(),
        bins,
    })
}

/// A bin is a command name looked up on the container's PATH, or an absolute
/// path inside the container.
fn validate_bin(bin: &str) -> Result<()> {
    let name = bin.strip_prefix('/').unwrap_or(bin);
    let valid = !name.is_empty()
        && !name.starts_with('-')
        && (bin.starts_with('/') || !name.contains('/'))
        && name.split('/').all(|part| !matches!(part, "" | "." | ".."))
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '+' | '/'));
    if !valid {
        bail!("distrobox bin must be a command name or an absolute path, got {bin:?}");
    }
    Ok(())
}

/// OCI tag grammar: `[A-Za-z0-9_][A-Za-z0-9._-]{0,127}`.
fn validate_tag(tag: &str) -> Result<()> {
    let valid = !tag.is_empty()
        && tag.len() <= 128
        && !tag.starts_with(['.', '-'])
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    if !valid {
        bail!("{tag:?} is not a valid OCI image tag");
    }
    Ok(())
}

/// Tags such as `sha256-<digest>.sig` that cosign and similar tools push next
/// to an image to hold its signatures, attestations and SBOMs.
fn is_artifact_tag(tag: &str) -> bool {
    tag.strip_prefix("sha256-").is_some_and(|rest| {
        rest.split_once('.').is_some_and(|(digest, _)| {
            digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit())
        })
    })
}

fn validate_container_name(name: &str) -> Result<()> {
    let valid = name.starts_with("mise-")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    if !valid {
        bail!("invalid distrobox container name {name:?}");
    }
    Ok(())
}

/// One container per tool and tag. The hash keeps two tools exporting
/// different bins from the same image from sharing (and removing) one
/// container.
fn container_name(tool: &DistroboxTool, tag: &str, packages: Option<&str>) -> String {
    let readable: String = tool
        .image
        .rsplit('/')
        .next()
        .unwrap_or(&tool.image)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(32)
        .collect();
    let key = format!(
        "{}:{tag}:{}:{}",
        tool.image,
        tool.bins.join(","),
        packages.unwrap_or("")
    );
    let hash = hash_sha256_to_str(&key);
    let tag: String = tag
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(32)
        .collect();
    format!("mise-{readable}-{tag}-{}", &hash[..8])
}

fn distrobox_available(distrobox: &str) -> bool {
    if Path::new(distrobox).is_absolute() {
        Path::new(distrobox).is_file()
    } else {
        file::which(distrobox).is_some()
    }
}

async fn export_bins(
    distrobox: &str,
    name: &str,
    bins: &[String],
    bin_dir: &Path,
    pr: &dyn SingleReport,
) -> Result<()> {
    file::create_dir_all(bin_dir)?;
    for bin in bins {
        let path = resolve_bin(distrobox, name, bin).await?;
        pr.set_message(format!("export {path}"));
        CmdLineRunner::new(distrobox)
            .args(["enter", "--name", name, "--no-tty", "--"])
            .args(["distrobox-export", "--bin", &path, "--export-path"])
            .arg(bin_dir)
            .with_pr(pr)
            .execute()?;
        let exported = bin_dir.join(file_name(&path));
        if !exported.is_file() {
            bail!("distrobox-export did not write {}", exported.display());
        }
    }
    Ok(())
}

async fn container_exists(distrobox: &str, name: &str) -> Result<bool> {
    let output = CmdLineRunner::new(distrobox)
        .args(["list", "--no-color"])
        .read()
        .await?;
    Ok(listed_names(&output).any(|n| n == name))
}

/// Names from the `ID | NAME | STATUS | IMAGE` table `distrobox list` prints.
fn listed_names(output: &str) -> impl Iterator<Item = &str> {
    output
        .lines()
        .skip(1)
        .filter_map(|line| line.split('|').nth(1))
        .map(str::trim)
}

fn remove_container(distrobox: &str, name: &str, pr: &dyn SingleReport) -> Result<()> {
    CmdLineRunner::new(distrobox)
        .args(["rm", "--force", name])
        .with_pr(pr)
        .execute()
}

/// The absolute path of `bin` inside the container, which `distrobox-export`
/// requires.
async fn resolve_bin(distrobox: &str, name: &str, bin: &str) -> Result<String> {
    let output = CmdLineRunner::new(distrobox)
        .args(["enter", "--name", name, "--no-tty", "--"])
        .args(["sh", "-c", r#"command -v -- "$1""#, "sh", bin])
        .read()
        .await;
    let path = output.ok().and_then(|out| {
        out.lines()
            .map(str::trim)
            .rfind(|l| l.starts_with('/'))
            .map(String::from)
    });
    match path {
        Some(path) => Ok(path),
        None => bail!(
            "`{bin}` was not found in the distrobox container {name}; \
             install it with the `packages` tool option"
        ),
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_image_and_bins() {
        assert_eq!(
            parse_tool("fedora/fedora-toolbox:htop,jq").unwrap(),
            DistroboxTool {
                image: "fedora/fedora-toolbox".into(),
                bins: vec!["htop".into(), "jq".into()],
            }
        );
        assert_eq!(
            parse_tool("localhost:5000/tools/box:/opt/bin/x").unwrap(),
            DistroboxTool {
                image: "localhost:5000/tools/box".into(),
                bins: vec!["/opt/bin/x".into()],
            }
        );
        for bad in [
            "alpine",
            "alpine:",
            ":jq",
            "alpine:a/b",
            "alpine:../x",
            "alpine:-x",
            "-alpine:jq",
            "a lpine:jq",
            "alpine:/usr/../bin/x",
        ] {
            assert!(parse_tool(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn validates_tags() {
        for good in ["latest", "41", "3.20.1", "bookworm-slim", "v1_2"] {
            assert!(validate_tag(good).is_ok(), "{good}");
        }
        for bad in ["", "-x", ".x", "a/b", "a:b", &"a".repeat(129)] {
            assert!(validate_tag(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn skips_signature_tags() {
        assert!(is_artifact_tag(&format!("sha256-{}.sig", "a".repeat(64))));
        assert!(is_artifact_tag(&format!("sha256-{}.att", "0".repeat(64))));
        assert!(!is_artifact_tag("sha256-short.sig"));
        assert!(!is_artifact_tag("latest"));
    }

    #[test]
    fn container_names_are_stable_and_distinct() {
        let jq = parse_tool("docker.io/library/alpine:jq").unwrap();
        let curl = parse_tool("docker.io/library/alpine:curl").unwrap();
        let name = container_name(&jq, "3.20", None);
        assert!(name.starts_with("mise-alpine-3-20-"), "{name}");
        assert!(validate_container_name(&name).is_ok());
        assert_eq!(name, container_name(&jq, "3.20", None));
        assert_ne!(name, container_name(&curl, "3.20", None));
        assert_ne!(name, container_name(&jq, "3.20", Some("bash")));
    }

    #[test]
    fn reads_names_from_distrobox_list() {
        let output = "ID           | NAME                 | STATUS        | IMAGE\n\
                      0123abcd     | mise-alpine-x        | Up 2 minutes  | alpine:3.20\n\
                      4567ef01     | fedora               | Exited        | fedora:41\n";
        let names: Vec<_> = listed_names(output).collect();
        assert_eq!(names, ["mise-alpine-x", "fedora"]);
    }
}
