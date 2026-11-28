//! The build half of a deploy: runs the project's own tools (its framework's build, pip) and leaves
//! everything the deployment is made of in one folder, with no AWS involved. Publishing (publish.rs)
//! takes the folder from there, on this machine or another.
//!
//! The folder:
//! - `build.json`: what was built and the settings it asks for (`Build`)
//! - `package.zip`: the Lambda package
//! - `static/`: the static files by content and their manifest, laid out as in S3
//! - `source.zip`: the project's own files, for the dashboard's Source view (source.rs)
//! - `next.json`, `next/prerender/` and `next-handlers.json`: a Next.js build's manifest, its
//!   prerendered files, and from Next 16.2 its entrypoints with their traced files (next.rs)
//!
//! The project's commands get its env vars for the target, and Vercel's system variables
//! (`VERCEL_ENV`, `VERCEL_URL`, `VERCEL_GIT_*`, `CI`), and none of this machine's AWS credentials.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use forkrail_common::routes::valid_id;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::buildlog::{self, BuildLog};
use crate::deploy::check_project;
use crate::framework;
use crate::git;
use crate::native;
use crate::nodeapp;
use crate::package::{self, Package};
use crate::source::{self, Archive};
use crate::state::{GitCommit, Target, now_rfc3339};
use crate::statics::{self, Prepared};
use crate::system_env;
use crate::vercel::{self, Language};

pub const BUILD_FILE: &str = "build.json";
pub const PACKAGE_FILE: &str = "package.zip";
/// The version of `build.json`'s layout. Publishing refuses one it doesn't know. Fields added
/// with a default keep the version; one that changes what an old reader would do changes it.
pub const FORMAT: u32 = 1;
/// The importer that reads a project the way Vercel does (`vercel.json`, `vercel.ts`/`vercel.toml`,
/// framework presets) into forkrail's build. Other importers (Next.js's adapter) get their own name.
pub const IMPORTER: &str = "vercel";
/// The importer that takes a framework's Vercel Build Output (`.vercel/output`) as it is.
pub const BUILD_OUTPUT_IMPORTER: &str = "build-output";

/// What to build, for which deployment. The CLI makes one from the install's state; a builder
/// elsewhere gets one as a file (`forkrail build --request`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub project: String,
    /// What the project is called now, if it was renamed (`state::Project::display_name`): the
    /// name its app and build are told. Absent from requests made before renames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub target: Target,
    /// The install's base domain, for the deployment's URLs.
    pub base_domain: String,
    /// The project's env vars for the target.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// The commit being built when the request knows it (a push); otherwise git says, if the folder
    /// is a checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitCommit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployer: Option<String>,
    /// A push to the project's repository asked for it: its `ignoreCommand` decides whether it's
    /// built at all. A deploy or a redeploy someone asked for by hand is always built.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pushed: bool,
    /// For a push: the commit the branch was at before it, by its whole hash, which builds get as
    /// VERCEL_GIT_PREVIOUS_SHA. None for a branch's first push.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_sha: Option<String>,
    /// The project's framework preset, from its settings (a Vercel slug such as `flask`):
    /// `vercel.json`'s `framework` wins over it. None: found from the project's files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framework: Option<String>,
    /// The project has skew protection on (skew.rs): the build and the app get
    /// `VERCEL_SKEW_PROTECTION_ENABLED=1` (a new project gets it by the default, `skew::at_build`).
    /// With `skew_if_framework`, only if the project turns out to be one of `skew::FRAMEWORKS`:
    /// what requests from before every new project got it say. Absent from requests made before
    /// either: off.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skew: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skew_if_framework: bool,
}

impl Request {
    /// Whether the build gets skew protection, for a project of `framework` (as a build records it).
    pub fn skew_for(&self, framework: Option<&str>) -> bool {
        self.skew || (self.skew_if_framework && framework.is_some_and(crate::skew::supports))
    }
}

/// Raised by a build that its project's `ignoreCommand` called off: nothing failed, and there is
/// nothing to publish.
#[derive(Debug)]
pub struct Ignored;

/// What a deployment that `ignoreCommand` called off says of itself.
pub const IGNORED: &str = "Skipped: the project's ignoreCommand exited 0, which says this commit needs no build";

impl std::fmt::Display for Ignored {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(IGNORED)
    }
}

impl std::error::Error for Ignored {}

/// Raised by a build whose command was killed from outside (SIGKILL), which is nearly always one
/// that ran out of memory: the builder says so in its outcome, and the control function sends it
/// once to the large builder (remote.rs `finished`).
#[derive(Debug)]
pub struct OutOfMemory(pub String);

impl std::fmt::Display for OutOfMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for OutOfMemory {}

/// Why `what`'s build command ended as it did, if it was killed from outside: what running out of
/// memory looks like, on Lambda (whose sandbox kills its biggest process) as on any machine.
pub fn out_of_memory(status: std::process::ExitStatus, what: &str) -> Option<OutOfMemory> {
    #[cfg(unix)]
    let killed = std::os::unix::process::ExitStatusExt::signal(&status) == Some(9) || status.code() == Some(137);
    #[cfg(not(unix))]
    let killed = status.code() == Some(137);
    if !killed {
        return None;
    }
    let here = match std::env::var("AWS_LAMBDA_FUNCTION_MEMORY_SIZE") {
        Ok(mb) => format!("this builder has {mb} MB"),
        Err(_) => "this machine had no more to give it; build it on one with more memory".to_owned(),
    };
    Some(OutOfMemory(format!("the {what} build was killed (SIGKILL), which is what running out of memory looks like: {here}")))
}

/// `build.json`: what a build made, and what the deployment asks for. This is forkrail's own format:
/// an importer reads a project (today `IMPORTER`: `vercel.json` and framework presets) and writes
/// it; publishing, the router and the dashboard read only this and what it names (the package, the
/// static files' manifest, the routing program), never the project's own config files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Build {
    pub format: u32,
    pub id: String,
    pub project: String,
    pub target: Target,
    /// When the build finished.
    pub built: String,
    pub language: Language,
    /// The Lambda runtime its function runs on (`python3.12`, `nodejs22.x`, `provided.al2023`),
    /// from the version the project asks for. None in builds from before it was read, which ran on
    /// their language's (`Language::runtime`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    /// The file the runtime starts, relative to the package's `app/`.
    pub entry: String,
    /// What the project's own files fixed (`vercel.json`'s `functions` and `fluid`); publishing's
    /// own flags win over them, and they win over the project's settings. The deployment's record
    /// keeps them (`DeploymentRecord::pinned`), so a redeploy never reads `vercel.json` again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration_s: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrent: Option<bool>,
    /// Per-file functions: the least memory and maxDuration their shared function needs, when
    /// `functions` sets them for some and not others (`state::Floor`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor: Option<crate::state::Floor>,
    /// `vercel.json`'s `regions`, by AWS's names: where its function runs, over the project's
    /// setting (`regions::function_regions`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regions: Option<Vec<String>>,
    /// The `vercel.json` itself, kept in the deployment's record for people to read.
    #[serde(default)]
    pub config: Option<Value>,
    /// The importer that wrote this build (`IMPORTER`). None in builds from before it was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub importer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framework: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitCommit>,
    /// The hash of the files packaged as the app, and how many there were.
    pub source_sha256: String,
    pub source_files: usize,
    /// The Lambda package; none for a static site, which has no function.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<Package>,
    #[serde(default, rename = "static", skip_serializing_if = "Option::is_none")]
    pub static_files: Option<Prepared>,
    /// The routing program's id (`static/rules/<id>.json` in the folder), if `vercel.json` has
    /// routing rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<String>,
    /// Its Image Optimization settings' id (`static/images/<id>.json` in the folder), if
    /// `vercel.json` has `images`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<String>,
    /// The project's own files, kept in `source.zip` for the dashboard; none if there were too many.
    #[serde(default, rename = "source", skip_serializing_if = "Option::is_none")]
    pub kept_source: Option<Archive>,
    /// `crons` and `schedules`, checked. Kept in the deployment's record, so a redeploy, promote or
    /// rollback brings back the deployment's own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schedules: Vec<forkrail_common::schedules::Schedule>,
    /// Its functions in `api/`, by file, when that's what it runs (Vercel's zero-config functions):
    /// for people to read, in the deployment's record.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub functions: Vec<String>,
    /// Built with skew protection on: its app gets `VERCEL_SKEW_PROTECTION_ENABLED=1` too, and a
    /// project that had no setting records it as on (`skew::default_after_build`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skew_protection: bool,
    /// The name its framework gave the deployment (a Build Output's `deploymentId`) when it isn't
    /// its id: skew protection takes requests naming it as this deployment's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skew_id: Option<String>,
    /// What it runs, for people to read: its functions, prerenders, middleware and image sizes
    /// (`state::Outline`). Kept in the deployment's record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outline: Option<crate::state::Outline>,
    /// A Next.js build's manifest (`next.json` in the folder, next.rs): what the router, the
    /// shared cache's seed and the runtime read of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<NextBuild>,
    /// A Next.js build's entry in the routing table (`next_router.rs`): its shape and the id of
    /// the router's config, `static/next/<id>.json` in the folder. Kept in the deployment's
    /// record, so a redeploy, promote or rollback keeps the router's Next rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_router: Option<forkrail_common::routes::NextRef>,
}

/// What `build.json` says of a Next.js build's manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NextBuild {
    /// The Next version that built it (`16.4.6`).
    pub version: String,
    /// The manifest's file in the build folder, and its hash.
    pub manifest: String,
    pub sha256: String,
    /// Next's own id for the build (`BUILD_ID`).
    pub build_id: String,
    /// How many paths it prerendered, and whether their files are in the package (`package`) or
    /// only in the folder's `next/prerender/` (`seed`).
    pub prerenders: usize,
    pub prerender_files: crate::next::Kept,
    /// Next's caches are forkrail's shared cache: the function gets `FORKRAIL_NEXT_CACHE=1`.
    #[serde(default)]
    pub shared_cache: bool,
    /// The shared cache's seed, in the folder's `next-seed/` (`next::SEED_DIR`), put under
    /// `static/next-seed/<id>/` in each region when the deployment is published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<NextSeed>,
    /// The server the package runs: Next's own (`next`, also every record from before this
    /// field) or forkrail's of Next's entrypoints (`direct`, next_handlers.rs).
    #[serde(default, skip_serializing_if = "crate::next_handlers::Server::is_next")]
    pub server: crate::next_handlers::Server,
}

/// What the seed of a Next build holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NextSeed {
    pub entries: usize,
    pub bytes: u64,
}

impl Build {
    /// The build in folder `out`, checked: its layout's version, its names, and that the package
    /// is the one it describes.
    pub fn read(out: &Path) -> Result<Self> {
        let path = out.join(BUILD_FILE);
        let text = std::fs::read(&path).with_context(|| format!("reading {}; is {} a folder `forkrail build` made?", path.display(), out.display()))?;
        let build: Build = serde_json::from_slice(&text).with_context(|| format!("{} isn't a build description", path.display()))?;
        if build.format != FORMAT {
            bail!("{} is format {}; this forkrail reads format {FORMAT}", path.display(), build.format);
        }
        if !valid_id(&build.id) {
            bail!("{}: {:?} isn't a deployment id", path.display(), build.id);
        }
        check_project(&build.project)?;
        if let Some(described) = &build.package {
            let package = out.join(PACKAGE_FILE);
            let sha256 = forkrail_common::hex(&package::file_sha256(&package)?);
            if sha256 != described.sha256 {
                bail!("{} isn't the package {} describes", package.display(), path.display());
            }
        } else if build.static_files.is_none() {
            bail!("{} describes neither a package nor static files", path.display());
        }
        // Named by their content, which they must hold (`statics::check_content`): another
        // project's deployment may name the same.
        if let Some(rules) = &build.rules {
            let key = forkrail_common::statics::rules_key(rules);
            if !crate::statics::content_id(rules) || !out.join(&key).is_file() {
                bail!("{} names routing rules {rules:?} it doesn't have", path.display());
            }
            crate::statics::check_content(&key, &out.join(&key), None)?;
        }
        if let Some(images) = &build.images {
            let key = forkrail_common::statics::images_key(images);
            if !crate::statics::content_id(images) || !out.join(&key).is_file() {
                bail!("{} names image settings {images:?} it doesn't have", path.display());
            }
            crate::statics::check_content(&key, &out.join(&key), None)?;
        }
        if let Some(next) = &build.next {
            let manifest = out.join(&next.manifest);
            if next.manifest.contains(['/', '\\']) || forkrail_common::hex(&package::file_sha256(&manifest)?) != next.sha256 {
                bail!("{} isn't the Next.js manifest {} describes", manifest.display(), path.display());
            }
        }
        if let Some(kept) = &build.kept_source {
            let archive = out.join(source::ARCHIVE_FILE);
            if forkrail_common::hex(&package::file_sha256(&archive)?) != kept.sha256 {
                bail!("{} isn't the source {} describes", archive.display(), path.display());
            }
        }
        Ok(build)
    }

    pub fn package_path(out: &Path) -> PathBuf {
        out.join(PACKAGE_FILE)
    }

    /// The Lambda runtime its function runs on.
    pub fn runtime(&self) -> &str {
        self.runtime.as_deref().unwrap_or_else(|| self.language.runtime())
    }
}

pub struct Options {
    /// For the package: the runtime, Python, and whether to reinstall cached requirements.
    pub package: package::Options,
    /// For a framework: take its existing output instead of building.
    pub prebuilt: bool,
    /// For a framework: whether its install command runs when there's a `node_modules` already.
    pub install: framework::Install,
}

/// Build the project in `dir` into the folder `out`, which must be empty or not exist yet.
pub fn build(dir: &Path, request: &Request, opts: &Options, out: &Path, log: &dyn BuildLog) -> Result<Build> {
    if !valid_id(&request.id) {
        bail!("{:?} isn't a deployment id", request.id);
    }
    check_project(&request.project)?;
    if out.exists() && std::fs::read_dir(out).with_context(|| format!("reading {}", out.display()))?.next().is_some() {
        bail!("{} isn't empty; a build goes into an empty folder", out.display());
    }
    std::fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;

    log.step(buildlog::PREPARING);
    let mut spec = vercel::read_as(dir, request.framework.as_deref())?;
    for warning in &spec.warnings {
        log.info(&format!("warning: {warning}"));
    }
    // Checked before anything is built: a region forkrail doesn't know fails the build at once.
    let regions = vercel::regions(spec.config.as_ref())?;
    if let Some(regions) = &regions {
        log.info(&format!("functions run in {} (vercel.json's `regions`)", crate::regions::describe(regions)));
    }
    if let Some(backend) = &spec.backend {
        match backend.files {
            Some(_) => log.info(&format!("{}: {}", backend.language().name(), backend.found)),
            None => log.info(&format!("{} ({}): {}", backend.preset.name, backend.preset.language.name(), backend.found)),
        }
    }
    if let Some(version) = &spec.version {
        log.info(&format!("{}, on Lambda's {}", version.describe(spec.language.name()), version.runtime));
    }
    // For the record only: the framework an API is written with (a site's is the one that builds
    // it), who is deploying, and the commit the folder is at.
    let framework_name = spec.framework.as_ref().map(|f| f.name).or_else(|| framework::api(dir, &spec));
    let git = request.git.clone().or_else(|| git::commit(dir));
    let deployer = request.deployer.clone().or_else(|| git::deployer(dir));
    let skew = request.skew_for(framework_name);
    if skew {
        log.info("skew protection: on (VERCEL_SKEW_PROTECTION_ENABLED=1)");
    }
    let env = build_env(request, git.as_ref(), skew);
    // A push's build first asks the project whether to build at all, as Vercel's Ignored Build
    // Step does: before anything is installed.
    if request.pushed
        && let Some(command) = &spec.ignore_command
    {
        ignore_step(dir, command, &env, log)?;
    }
    // The folder as it is before anything builds in it. A forkrail release's dashboard is built
    // without it (`FORKRAIL_NO_SOURCE=1`, deploy/aws/bundle.sh): that build is published for anyone
    // to download, and the source isn't.
    let kept_source = if std::env::var_os("FORKRAIL_NO_SOURCE").is_some_and(|v| v == "1") { None } else { source::keep(dir, out, log)? };

    // The Node major the project asks for, when this machine has more than one (the builder's
    // image says where each is with FORKRAIL_NODE_<major>): its own `node` and `npm` build it.
    let mut env = env;
    if spec.language == Language::Node
        && let Some(version) = &spec.version
        && let Some(bin) = std::env::var_os(format!("FORKRAIL_NODE_{}", version.version)).filter(|b| Path::new(b).is_dir())
    {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let joined = std::env::join_paths(std::iter::once(std::path::PathBuf::from(&bin)).chain(std::env::split_paths(&path)))?;
        env.insert("PATH".to_owned(), joined.to_string_lossy().into_owned());
        log.info(&format!("building with Node {} ({})", version.version, Path::new(&bin).display()));
    }
    // A backend preset may build first: a Node one installs and compiles, a Go or Rust one makes
    // its program. What they make goes into the package beside the project's files.
    let made = package::TempDir::new("made")?;
    let mut extra = package::Extra::default();
    if let Some(backend) = spec.backend.clone() {
        if let Some(files) = &backend.files {
            let cache = package::native_cache()?;
            let manifest = match files {
                crate::backends::FileFunctions::Go(files) => {
                    let program = crate::functions::build_go(dir, files, made.path(), &cache, &env, log)?;
                    extra.files.push((".forkrail/functions".to_owned(), program));
                    Some(crate::functions::go_manifest(files))
                }
                crate::backends::FileFunctions::Rust(functions) => {
                    for (f, program) in crate::functions::build_rust(dir, functions, &cache, &env, log)? {
                        extra.files.push((format!(".forkrail/fn/{}", f.bin), program));
                    }
                    Some(crate::functions::rust_manifest(functions))
                }
                // Node's are compiled after the site's build, which may install their packages.
                crate::backends::FileFunctions::Node(_) => None,
                crate::backends::FileFunctions::Python(functions) => {
                    let path = made.path().join("functions.py");
                    std::fs::write(&path, crate::pyfns::entry_source(functions))?;
                    extra.files.push((crate::pyfns::ENTRY.to_owned(), path));
                    None
                }
            };
            if let Some(manifest) = manifest {
                let path = made.path().join("functions.json");
                std::fs::write(&path, serde_json::to_vec_pretty(&manifest)?)?;
                extra.files.push((crate::functions::MANIFEST.to_owned(), path));
            }
        } else if let Some(server) = &backend.server {
            let program = native::build(dir, server, &package::native_cache()?, &env, log)?;
            extra.files.push((server.entry(), program));
        } else if backend.language() == Language::Node {
            let prepared = nodeapp::build(dir, &backend, spec.config.as_ref(), made.path(), opts.install, &env, log)?;
            extra.files.extend(prepared.files);
            spec.entry = prepared.entry;
        } else if backend.django_settings.is_some() {
            extra.collect_static = Some(made.path().join("static"));
        }
    }
    // A framework builds the app: its server is the function (if it makes one), its output's
    // public folder (a static site's whole output) the static files.
    // A static preset's build that wrote a Build Output (Qwik City's `vercel-edge` adapter, on a
    // project Vercel calls `vite`): that is what's deployed, as Vercel's static-build takes it.
    let mut wrote_output = false;
    if let Some(framework) = &spec.framework {
        if opts.prebuilt {
            framework.check(dir)?;
        } else {
            if framework.build.is_some() {
                log.info(&format!("building with {}", framework.name));
            }
            let started = std::time::SystemTime::now();
            framework.build(dir, opts.install, &env, log)?;
            wrote_output = !framework.build_output() && !framework.server() && framework.build.is_some() && crate::build_output::written_since(dir, started);
        }
        // Nitro built for its `node-server` preset, not Vercel's: that server is the deployment.
        if let Some((server, why)) = framework.nitro_server(dir, &env) {
            log.info(&format!(
                "warning: Nitro built with its node-server preset ({why}), not the `vercel` preset Vercel builds it with: forkrail runs it as one server, so its route rules, `isr` and scheduled tasks run inside it, not as on Vercel"
            ));
            if framework::Framework::nuxt_image(dir) {
                log.info("warning: Nuxt Image still picks its Vercel provider (VERCEL=1), so pages ask /_vercel/image for their images, which a node-server build has no image settings for: they won't load. Build with Nitro's `vercel` preset, or set `image.provider` (such as \"ipx\")");
            }
            crate::vercel::use_server(&mut spec, server)?;
        }
    }
    // A Next.js static export (`output: 'export'`) has no server: its exported folder is the
    // deployment, a static site with the routes Vercel's Next builder puts around one
    // (next_export.rs).
    let export = match &spec.framework {
        Some(framework) if framework.next() => crate::next_export::find(dir, &framework.output)?,
        _ => None,
    };
    if let (Some(export), Some(framework)) = (&export, spec.framework.clone()) {
        let folder = export.dir.strip_prefix(dir).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_else(|_| export.dir.to_string_lossy().into_owned());
        let shape = match (export.base_path.as_str(), export.trailing_slash) {
            ("", false) => String::new(),
            ("", true) => ", trailingSlash".to_owned(),
            (base, slash) => format!(", under {base}{}", if slash { ", trailingSlash" } else { "" }),
        };
        log.info(&format!("{} {} static export (`output: 'export'`): {folder}/ is deployed as a static site{shape}", framework.name, export.version));
        if export.own_routes > 0 {
            log.info(&format!(
                "next.config's redirects, rewrites and headers ({}) apply to the export, as on Vercel: Next's warning that they won't is for hosts that don't apply them",
                export.own_routes
            ));
        }
        let site = framework::Framework { output: folder, entry: "", public: "", find: framework::Output::Fixed, hashed: &[], routes: "", ..framework };
        crate::vercel::use_static(&mut spec, site, &export.routes)?;
    }
    // Next.js: its standalone server is the package, its manifest and the shared cache's seed go
    // in the folder (next.rs, nextseed.rs). The seed is made here, before anything is packaged,
    // from the standalone folder as the build left it.
    let mut next = match &spec.framework {
        Some(framework) if framework.next() => {
            log.step(buildlog::PACKAGING);
            let next = crate::next::collect(dir, spec.config.as_ref(), out, &request.id, env.get("PATH").map(String::as_str), log)?;
            spec.entry = next.manifest.entry.clone();
            Some(next)
        }
        _ => None,
    };
    // On Next 16.2 and later the folder now also has `next-handlers.json` (`next::Handlers`): every
    // entrypoint with its traced files, still where the build left them. On a Next release
    // forkrail's own server is verified on, the package becomes that server's (next_handlers.rs).
    let next_server = match next.as_mut() {
        Some(built) => crate::next_handlers::choose(out, built, log)?,
        None => crate::next_handlers::Server::Next,
    };
    if let Some(floor) = next.as_ref().and_then(|n| n.floor) {
        spec.floor = Some(match spec.floor {
            Some(own) => crate::state::Floor { memory_mb: own.memory_mb.max(floor.memory_mb), max_duration_s: own.max_duration_s.max(floor.max_duration_s) },
            None => floor,
        });
    }
    let (app_dir, public) = match &spec.framework {
        Some(framework) => {
            // Per-file functions beside a site are the project's own, not its build's.
            let app = if spec.file_functions().is_some() { dir.to_path_buf() } else { framework.output_dir(dir) };
            (app, Some(framework.public_dir(dir)))
        }
        None => (dir.to_path_buf(), spec.public.as_ref().map(|p| dir.join(p))),
    };
    // A framework whose Vercel adapter writes a Build Output: its routes, functions and static
    // files are what's deployed (build_output.rs).
    let imported = match &spec.framework {
        Some(framework) if framework.build_output() || wrote_output => {
            if wrote_output {
                log.info(&format!("the build wrote a Build Output ({}): deploying that", crate::build_output::DIR));
                spec.entry = crate::build_output::DISPATCHER.to_owned();
            }
            let output = crate::build_output::read(&dir.join(crate::build_output::DIR))?;
            let empty = serde_json::Map::new();
            let top = spec.config.as_ref().and_then(Value::as_object).unwrap_or(&empty);
            let names = output.functions.len();
            let imported = crate::build_output::import(dir, output, top, framework.name)?;
            for warning in &imported.warnings {
                log.info(&format!("warning: {warning}"));
            }
            log.info(&format!(
                "{}'s Build Output: {} functions ({names} names), {} routes",
                framework.name,
                imported.functions.len(),
                imported.program.routes.len()
            ));
            Some(imported)
        }
        _ => None,
    };
    // Schedules a framework declares that won't run as the user may expect, now that what the
    // build made is known (Nitro 3's tasks are its Build Output's crons; Nitro 2's are nothing).
    let built = match (&imported, &spec.framework) {
        (Some(_), _) => framework::Built::BuildOutput,
        (None, Some(f)) if f.nitro() => framework::Built::NitroServer,
        _ => framework::Built::Other,
    };
    for warning in framework::declared_schedules(dir, built) {
        log.info(&format!("warning: {warning}"));
    }
    let public = match (&imported, &next) {
        (Some(imported), _) => Some(imported.static_dir.clone()),
        // `.next/static` and `public/`, laid out by the paths they're served at.
        (None, Some(next)) => Some(next.static_dir.clone()),
        // An export's folder, under the base path if it has one.
        (None, None) => export.as_ref().map(|e| e.static_dir.clone()).or(public),
    };
    // The project's own Routing Middleware (middleware.rs), bundled now that the site's build has
    // installed what it imports.
    let middleware = crate::middleware::find(dir, spec.config.as_ref(), spec.framework.as_ref().map(|f| f.name))?;
    let mut middleware_route = None;
    if let Some(found) = &middleware {
        let node_functions = matches!(spec.file_functions(), Some(crate::backends::FileFunctions::Node(_)));
        if !(spec.static_only() || imported.is_some() || node_functions) {
            bail!(
                "{}: Routing Middleware beside {} isn't supported yet (forkrail runs it beside a static site, Node functions in api/, or a framework's Build Output)",
                found.entry,
                spec.describe()
            );
        }
        let bundle = crate::middleware::build(dir, found, made.path(), &env, log)?;
        extra.files.push((found.bundle(), bundle));
        middleware_route = Some(found.route()?);
        let paths = found.matcher.as_ref().map_or("every path".to_owned(), |m| m.join(", "));
        log.info(&format!("Routing Middleware: {} on {paths}", found.entry));
        // A static site's only function: its dispatcher serves the middleware alone.
        if spec.static_only() && imported.is_none() {
            let path = made.path().join("functions.mjs");
            std::fs::write(&path, crate::middleware::dispatcher_source(found))?;
            extra.files.push((crate::build_output::DISPATCHER.to_owned(), path));
            spec.entry = crate::build_output::DISPATCHER.to_owned();
        }
    }
    if let Some(imported) = &imported {
        if let Some(floor) = imported.floor {
            spec.floor = Some(match spec.floor {
                Some(own) => crate::state::Floor { memory_mb: own.memory_mb.max(floor.memory_mb), max_duration_s: own.max_duration_s.max(floor.max_duration_s) },
                None => floor,
            });
        }
        // The framework's cron jobs and schedules, after `vercel.json`'s, as `vercel build` merges
        // them (`mergeCrons`, `mergeSchedules`).
        let framework_schedules = merge_schedules(&mut spec.schedules, imported)?;
        if framework_schedules > 0 {
            log.info(&format!("schedules from the Build Output: {framework_schedules}"));
        }
        let path = made.path().join("functions.mjs");
        let dispatcher = match &middleware {
            Some(found) => crate::build_output::with_function(&imported.dispatcher, &found.dispatcher_entry()),
            None => imported.dispatcher.clone(),
        };
        std::fs::write(&path, dispatcher)?;
        extra.files.push((crate::build_output::DISPATCHER.to_owned(), path));
    }
    let static_only = match &imported {
        Some(imported) => imported.functions.is_empty(),
        None => spec.static_only(),
    } && middleware.is_none();
    // Schedules call a function: a deployment without one (its middleware isn't one) can't take them.
    let functionless = match &imported {
        Some(imported) => imported.functions.is_empty(),
        None => spec.static_only(),
    };
    if functionless && !spec.schedules.is_empty() {
        bail!("`crons`: a static site has no function for a schedule to call");
    }
    // Packages that work only on Vercel (vercel_services.rs), in what runs Node here: a
    // framework's functions, Node functions or server, or middleware. Names of env vars only.
    if !static_only && (spec.language == Language::Node || imported.is_some() || middleware.is_some()) {
        for warning in crate::vercel_services::warnings(dir, request.target.as_str(), request.env.keys()) {
            log.info(&format!("warning: {warning}"));
        }
    }
    // Node functions, compiled now that the site's build has run: what they import from npm is
    // what their package keeps of `node_modules`.
    let mut node_packages = None;
    if let Some(crate::backends::FileFunctions::Node(files)) = spec.file_functions() {
        let installed = spec.framework.as_ref().is_some_and(|f| f.build.is_some() && !opts.prebuilt);
        let prepared = crate::nodefns::build(dir, files, spec.config.as_ref(), made.path(), opts.install, installed, middleware.as_ref(), &env, log)?;
        extra.files.extend(prepared.files);
        node_packages = Some(crate::nodefns::package_dirs(dir, &prepared.packages));
    }
    // Gatsby's own plugin may write the site's routes; Vercel takes them in place of the preset's,
    // and the file isn't served.
    let mut routing = match &imported {
        Some(imported) => Some(imported.program.clone()),
        None => spec.routing.clone(),
    };
    if let (Some(framework), Some(public)) = (&spec.framework, &public)
        && framework.name == "Gatsby"
        && let Ok(text) = std::fs::read_to_string(public.join(GATSBY_ROUTES_FILE))
    {
        std::fs::remove_file(public.join(GATSBY_ROUTES_FILE)).with_context(|| format!("removing {GATSBY_ROUTES_FILE}"))?;
        let routes: serde_json::Value = serde_json::from_str(&text).with_context(|| format!("{GATSBY_ROUTES_FILE} isn't JSON"))?;
        let empty = serde_json::Map::new();
        let top = spec.config.as_ref().and_then(serde_json::Value::as_object).unwrap_or(&empty);
        routing = forkrail_common::rules::Program::from_vercel(top, &forkrail_common::rules::Function::default(), Some(("Gatsby", &routes))).map_err(anyhow::Error::msg)?;
        log.info(&format!("routes from Gatsby's {GATSBY_ROUTES_FILE}"));
    }
    // The middleware's route, where Vercel's `mergeRoutes` puts a project's middleware.
    if let Some(route) = middleware_route {
        let program = routing.get_or_insert_with(forkrail_common::rules::Program::default);
        program.insert_middleware(route);
        forkrail_common::rules::Engine::new(program).map_err(|e| anyhow::anyhow!("the middleware's route: {e}"))?;
    }
    // A Django app's static files are collected with its requirements, while it's packaged: its
    // package comes first, then the static files, its own beside `public/`'s.
    let django = extra.collect_static.clone();
    let mut overrides = statics::Overrides::default();
    for (file, o) in imported.iter().flat_map(|i| &i.overrides) {
        if let Some(path) = &o.path {
            overrides.paths.insert(file.clone(), path.clone());
        }
        if let Some(kind) = &o.content_type {
            overrides.types.insert(file.clone(), kind.clone());
        }
    }
    for (key, file) in imported.iter().flat_map(|i| &i.fallbacks) {
        overrides.extra.insert(key.clone(), file.clone());
    }
    if let Some(next) = &next {
        overrides.revalidated = next.revalidated.clone();
    }
    if let Some(export) = &export {
        overrides.revalidated = export.revalidated.clone();
    }
    let prepare_statics = |collected: Option<&Path>| -> Result<Option<statics::Prepared>> {
        let bulk = spec.bulk_redirects.as_ref().map(|p| dir.join(p));
        let hashed: Vec<&str> = match (&next, &export) {
            (Some(next), _) => next.hashed.iter().map(String::as_str).collect(),
            (None, Some(export)) => export.hashed.iter().map(String::as_str).collect(),
            (None, None) => spec.framework.as_ref().map_or(&[][..], |f| f.hashed).to_vec(),
        };
        let merged;
        let public = match (&public, collected.filter(|c| c.is_dir())) {
            (public, Some(collected)) => {
                merged = package::TempDir::new("public")?;
                if let Some(public) = public.as_deref().filter(|p| p.is_dir()) {
                    copy_tree(public, merged.path())?;
                }
                copy_tree(collected, merged.path())?;
                Some(merged.path().to_path_buf())
            }
            (Some(public), None) => Some(public.clone()),
            (None, None) => None,
        };
        let prepared = match &public {
            Some(p) => statics::prepare_with(p, out, p.as_path() == dir, &hashed, bulk.as_deref(), &overrides)?,
            None => None,
        };
        if let Some(prepared) = &prepared {
            log.info(&format!("static files: {} ({:.1} MB), manifest {}", prepared.files, prepared.bytes as f64 / 1e6, &prepared.id[..12]));
        }
        Ok(prepared)
    };
    let mut static_files = if django.is_none() { prepare_statics(None)? } else { None };
    let (package, source_sha256, source_files) = if static_only {
        let Some(prepared) = &static_files else {
            bail!("there's nothing to serve: {} has no files", public.as_deref().unwrap_or(dir).display());
        };
        (None, prepared.id.clone(), prepared.files)
    } else if let Some(next) = &next {
        // Next's files stay where its build left them, links as links, each with its own time.
        let package = package::build_entries(&next.entries, &opts.package, &out.join(PACKAGE_FILE))?;
        log.info(&format!(
            "package: {}, {} entries, {:.1} MB zipped ({:.0} MB unzipped)",
            next_server.describe(),
            package.entries,
            package.zipped as f64 / 1e6,
            package.unzipped as f64 / 1e6
        ));
        (Some(package), next.source_sha256.clone(), next.source_files)
    } else {
        let files = match &imported {
            Some(imported) => imported.files.clone(),
            // A static site's middleware is its package alone (in `extra`).
            None if spec.static_only() && middleware.is_some() => Vec::new(),
            None => package::collect(&app_dir, spec.language)?,
        };
        let source_sha256 = package::source_hash(&files)?;
        let kept;
        let packaged: &[package::SourceFile] = match spec.file_functions() {
            None => &files,
            Some(functions) => {
                kept = function_files(&spec, functions, &files, node_packages.as_ref(), log);
                &kept
            }
        };
        let package = package::build_with(&app_dir, &spec, packaged, &extra, &opts.package, &out.join(PACKAGE_FILE), &env, log)?;
        if django.is_some() {
            static_files = prepare_statics(django.as_deref())?;
        }
        log.info(&format!(
            "package: {} files from the project, {} entries, {:.1} MB zipped ({:.0} MB unzipped){}",
            files.len(),
            package.entries,
            package.zipped as f64 / 1e6,
            package.unzipped as f64 / 1e6,
            match package.deps_cached {
                Some(true) => ", Python requirements cached",
                Some(false) => ", Python requirements installed",
                None => "",
            }
        ));
        (Some(package), source_sha256, files.len())
    };
    // Bulk redirects, read now that the build has run (it may write them): a table the program
    // names, so a deployment with nothing else to route gets a program for them.
    if let Some(path) = &spec.bulk_redirects {
        let redirects = read_bulk(dir, path)?;
        let table = forkrail_common::rules::bulk::build(&redirects).map_err(|e| anyhow::anyhow!("bulkRedirectsPath: {e}"))?;
        let id = statics::write_redirects(&table, out)?;
        log.info(&format!("bulk redirects: {} from {path} ({:.1} MB table), {}", redirects.len(), table.len() as f64 / 1e6, &id[..12]));
        let program = routing.get_or_insert_with(|| forkrail_common::rules::Program { fallback: !spec.static_only(), ..Default::default() });
        program.bulk = Some(id);
    }
    let rules = match &routing {
        Some(program) => {
            let id = statics::write_rules(program, out)?;
            log.info(&format!("routing rules: {} ({}), {}", program.rule_count(), describe_rules(program), &id[..12]));
            Some(id)
        }
        None => None,
    };
    // A framework's Build Output names its own image settings (its routes and pages make the
    // `/_vercel/image` URLs to match them); `vercel.json`'s are for every other project.
    // N3: what the router needs of a Next build, and Next's own image settings for the router's
    // image function (`next_router.rs`).
    let next_router = match &next {
        Some(next) => Some(crate::next_router::prepare(&next.manifest, out, next_server)?),
        None => None,
    };
    let own_images = match imported.as_ref().and_then(|i| i.images.as_ref()) {
        Some(value) => {
            let config = forkrail_common::images::parse(value).map_err(|e| anyhow::anyhow!("the Build Output's images: {e}"))?;
            if spec.config.as_ref().and_then(|c| c.get("images")).is_some_and(|v| forkrail_common::images::parse(v).ok().as_ref() != Some(&config)) {
                log.info("warning: vercel.json's `images` differ from the framework's; the framework's are used");
            }
            Some(config)
        }
        None => match next_router.as_ref().and_then(|n| n.images.clone()) {
            Some(config) => {
                if spec.config.as_ref().and_then(|c| c.get("images")).is_some() {
                    log.info("warning: vercel.json's `images` are left out: Next.js's own image settings are used");
                }
                Some(config)
            }
            None => images(spec.config.as_ref())?,
        },
    };
    let image_sizes = own_images.as_ref().map(|c| crate::state::ImagesOutline { sizes: c.sizes.clone(), formats: c.formats.clone() });
    let images = match own_images {
        Some(config) => {
            let id = statics::write_images(&config, out)?;
            log.info(&format!(
                "image optimization: widths {}, {}, {}",
                config.sizes.iter().map(u32::to_string).collect::<Vec<_>>().join(", "),
                config.formats.join(", "),
                &id[..12]
            ));
            Some(id)
        }
        None => None,
    };

    let build = Build {
        format: FORMAT,
        id: request.id.clone(),
        project: request.project.clone(),
        target: request.target,
        built: now_rfc3339(),
        language: spec.language,
        runtime: (!static_only).then(|| spec.runtime()),
        entry: spec.entry.clone(),
        memory_mb: spec.memory_mb,
        max_duration_s: spec.max_duration_s,
        concurrent: spec.concurrent,
        floor: spec.floor,
        regions,
        config: spec.config.clone(),
        importer: Some(
            match (&imported, &next) {
                (Some(_), _) => BUILD_OUTPUT_IMPORTER,
                (None, Some(_)) => crate::next::IMPORTER,
                (None, None) => IMPORTER,
            }
            .to_owned(),
        ),
        framework: framework_name.map(str::to_owned),
        deployer,
        git,
        source_sha256,
        source_files,
        package,
        static_files,
        rules,
        images,
        kept_source,
        schedules: spec.schedules.clone(),
        functions: match &imported {
            Some(imported) => imported.functions.clone(),
            None => spec.file_functions().map(|f| f.files()).unwrap_or_default(),
        },
        skew_protection: skew,
        // A Build Output's `deploymentId`, or the one a Next build had (its config's own, when the
        // project names one): what its pages put on their requests.
        skew_id: imported
            .as_ref()
            .and_then(|i| i.deployment_id.clone())
            .or_else(|| next.as_ref().and_then(|n| n.manifest.deployment_id.clone()).filter(|name| forkrail_common::routes::valid_skew_id(name)))
            .or_else(|| export.as_ref().and_then(|e| e.deployment_id.clone()).filter(|name| forkrail_common::routes::valid_skew_id(name)))
            .filter(|name| *name != request.id),
        outline: {
            let mut outline = imported.as_ref().map(|i| i.outline.clone()).or_else(|| next.as_ref().map(|n| n.outline.clone())).unwrap_or_default();
            if let Some(found) = &middleware {
                outline.middleware.push(crate::state::MiddlewareOutline { function: found.function(), entry: Some(found.entry.clone()), matcher: found.matcher.clone(), edge: found.edge });
            }
            outline.images = image_sizes;
            (!outline.is_empty()).then_some(outline)
        },
        next: match &next {
            Some(next) => Some(NextBuild {
                version: next.manifest.next_version.clone(),
                manifest: crate::next::MANIFEST_FILE.to_owned(),
                sha256: forkrail_common::hex(&package::file_sha256(&out.join(crate::next::MANIFEST_FILE))?),
                build_id: next.manifest.build_id.clone(),
                prerenders: next.manifest.prerenders.len(),
                prerender_files: next.manifest.prerender_files,
                shared_cache: next.manifest.shared_cache,
                seed: next.seed.as_ref().map(|s| NextSeed { entries: s.entries, bytes: s.bytes }),
                server: next_server,
            }),
            None => None,
        },
        next_router: next_router.map(|n| n.entry),
    };
    if let Some(name) = &build.skew_id {
        log.info(&format!("the framework names this deployment {name} (its Build Output's deploymentId): skew protection takes that name for it"));
    }
    if !build.schedules.is_empty() {
        let names: Vec<&str> = build.schedules.iter().map(|s| s.label()).collect();
        log.info(&format!("schedules: {}", names.join(", ")));
    }
    let mut json = serde_json::to_vec_pretty(&build)?;
    json.push(b'\n');
    std::fs::write(out.join(BUILD_FILE), json).with_context(|| format!("writing {}", out.join(BUILD_FILE).display()))?;
    Ok(build)
}

/// The project's files the package of its per-file functions holds, as Vercel's functions get them.
/// Node and Python functions get the project's files (Node's `node_modules` only as far as the
/// functions import it, `node_packages`), Go and Rust ones their programs alone; then each
/// function's `excludeFiles` takes out what it matches and its `includeFiles` adds what it matches.
/// They share one package, so it holds what any of them would.
fn function_files(spec: &vercel::Spec, functions: &crate::backends::FileFunctions, files: &[package::SourceFile], node_packages: Option<&std::collections::BTreeSet<String>>, log: &dyn BuildLog) -> Vec<package::SourceFile> {
    use crate::functions::glob_match;
    let whole = matches!(functions, crate::backends::FileFunctions::Node(_) | crate::backends::FileFunctions::Python(_));
    let settings: Vec<vercel::FunctionSettings> = if spec.functions.is_empty() { functions.files().into_iter().map(|file| vercel::FunctionSettings { file, ..Default::default() }).collect() } else { spec.functions.clone() };
    let kept: Vec<package::SourceFile> = files
        .iter()
        .filter(|f| {
            let traced = whole && node_packages.is_none_or(|dirs| crate::nodefns::keeps(&f.rel, dirs));
            settings.iter().any(|s| {
                (traced && !s.exclude_files.as_deref().is_some_and(|g| glob_match(g, &f.rel))) || s.include_files.as_deref().is_some_and(|g| glob_match(g, &f.rel))
            })
        })
        .cloned()
        .collect();
    if let Some(dirs) = node_packages {
        let modules = kept.iter().filter(|f| f.rel.starts_with("node_modules/")).count();
        log.info(&format!("node_modules: {} packages the functions use ({modules} files)", dirs.len()));
    }
    kept
}

/// A Build Output's `crons` and `schedules` after `vercel.json`'s (`schedules`), as `vercel build`
/// merges them: crons appended (`mergeCrons`; a path on an expression already listed is the same
/// job here, since a schedule is keyed by both), schedules by name, a name given by both refused
/// (its `SCHEDULE_NAME_CONFLICT`). How many were added.
fn merge_schedules(schedules: &mut Vec<forkrail_common::schedules::Schedule>, imported: &crate::build_output::Imported) -> Result<usize> {
    let mut top = serde_json::Map::new();
    if !imported.crons.is_empty() {
        top.insert("crons".to_owned(), Value::Array(imported.crons.iter().map(|c| serde_json::json!({ "path": c.path, "schedule": c.schedule })).collect()));
    }
    if let Some(named) = imported.schedules.as_ref().filter(|s| !s.is_empty()) {
        top.insert("schedules".to_owned(), Value::Object(named.clone()));
    }
    if top.is_empty() {
        return Ok(0);
    }
    let mut added = 0;
    for schedule in vercel::schedules(&top).context("the Build Output's crons and schedules")? {
        if let Some(name) = &schedule.name
            && schedules.iter().any(|s| s.name.as_deref() == Some(name))
        {
            bail!("schedule {name:?} is defined by both vercel.json and the framework's Build Output: schedule names must be unique within a deployment");
        }
        if !schedules.iter().any(|s| s.key == schedule.key) {
            schedules.push(schedule);
            added += 1;
        }
    }
    Ok(added)
}

/// Copy every file under `from` into `to`, keeping their paths.
pub(crate) fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    for entry in ignore::WalkBuilder::new(from).standard_filters(false).follow_links(false).build() {
        let entry = entry?;
        if !entry.path().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(from).expect("walked from it");
        let dest = to.join(rel);
        std::fs::create_dir_all(dest.parent().expect("has a folder"))?;
        std::fs::copy(entry.path(), &dest).with_context(|| format!("copying {}", rel.display()))?;
    }
    Ok(())
}

/// Where Gatsby's Vercel plugin writes the site's routes.
const GATSBY_ROUTES_FILE: &str = "__now_routes_g4t5bY.json";

/// The redirects in `bulkRedirectsPath` (relative to `dir`): one file, or every `.csv`, `.json`
/// and `.jsonl` in a folder (at most `MAX_FILES`), in name order.
/// `vercel.json`'s `images`, checked as Vercel's schema has it (`forkrail_common::images::parse`).
pub fn images(config: Option<&serde_json::Value>) -> Result<Option<forkrail_common::images::ImagesConfig>> {
    match config.and_then(|c| c.get("images")) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => forkrail_common::images::parse(value).map(Some).map_err(|e| anyhow::anyhow!("vercel.json: {e}")),
    }
}

fn read_bulk(dir: &Path, path: &str) -> Result<Vec<forkrail_common::rules::bulk::BulkRedirect>> {
    use forkrail_common::rules::bulk;
    let full = dir.join(path);
    let files: Vec<std::path::PathBuf> = if full.is_dir() {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(&full).with_context(|| format!("bulkRedirectsPath: reading {path}"))? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !entry.file_type()?.is_file() {
                continue;
            }
            if !(name.ends_with(".csv") || name.ends_with(".json") || name.ends_with(".jsonl")) {
                bail!("bulkRedirectsPath: {path}/{name} isn't a .csv, .json or .jsonl file");
            }
            files.push(entry.path());
        }
        files.sort();
        if files.len() > bulk::MAX_FILES {
            bail!("bulkRedirectsPath: {path} has {} files; the limit is {}", files.len(), bulk::MAX_FILES);
        }
        files
    } else if full.is_file() {
        vec![full]
    } else {
        bail!("bulkRedirectsPath: {path} doesn't exist (after the build)");
    };
    let mut out = Vec::new();
    for file in files {
        let name = file.strip_prefix(dir).unwrap_or(&file).display().to_string();
        let text = std::fs::read_to_string(&file).with_context(|| format!("bulkRedirectsPath: reading {name}"))?;
        out.extend(bulk::parse(&name, &text).map_err(|e| anyhow::anyhow!("bulkRedirectsPath: {e}"))?);
        if out.len() > bulk::MAX_REDIRECTS {
            bail!("bulkRedirectsPath: more than {} redirects; that's the limit a project", bulk::MAX_REDIRECTS);
        }
    }
    Ok(out)
}

/// Which of `vercel.json`'s routing keys a program came from, for the build log.
fn describe_rules(program: &forkrail_common::rules::Program) -> String {
    let mut kinds: Vec<String> = Vec::new();
    for route in &program.routes {
        if let forkrail_common::rules::Route::Source(r) = route
            && let Some(from) = &r.from
        {
            let kind = from.split('[').next().unwrap_or(from).to_owned();
            if !kinds.contains(&kind) {
                kinds.push(kind);
            }
        }
    }
    if program.bulk.is_some() {
        kinds.push("bulkRedirectsPath".to_owned());
    }
    if kinds.is_empty() { "none".to_owned() } else { kinds.join(", ") }
}

/// The first thing a push's build does, for a builder that has more to fetch before building (the
/// project's build cache): ask the project's `ignoreCommand`, if it has one, whether to build at
/// all. `Ignored` if not. `build` asks too, unless told the request isn't a push's any more.
pub fn ask_first(dir: &Path, request: &Request, log: &dyn BuildLog) -> Result<()> {
    if !request.pushed {
        return Ok(());
    }
    let spec = vercel::read_as(dir, request.framework.as_deref())?;
    let Some(command) = &spec.ignore_command else { return Ok(()) };
    let git = request.git.clone().or_else(|| git::commit(dir));
    let skew = request.skew_for(spec.framework.as_ref().map(|f| f.name));
    ignore_step(dir, command, &build_env(request, git.as_ref(), skew), log)
}

/// Run the project's `ignoreCommand` in its folder, with the build's env vars: exiting 0 calls
/// the build off (`Ignored`), anything else builds, as on Vercel.
fn ignore_step(dir: &Path, command: &str, env: &BTreeMap<String, String>, log: &dyn BuildLog) -> Result<()> {
    log.info(&format!("ignoreCommand: `{command}`"));
    // The commit's files are here as GitHub archives them, without the repository's history.
    if command.split(|c: char| !c.is_ascii_alphanumeric()).any(|word| word == "git") && !dir.join(".git").exists() {
        log.info("note: a build has the commit's files, not the repository's history, so git commands fail here (and the build goes on); VERCEL_GIT_COMMIT_REF, VERCEL_GIT_COMMIT_SHA and VERCEL_GIT_PREVIOUS_SHA say what was pushed");
    }
    let status = buildlog::run_to_end(std::process::Command::new("sh").arg("-c").arg(command).current_dir(dir), "ignoreCommand", env, log)?;
    if status.success() {
        log.info("ignoreCommand exited 0: this commit needs no build, so the deployment is skipped");
        return Err(Ignored.into());
    }
    log.info(&format!("ignoreCommand ended with {status}: building"));
    Ok(())
}

/// What the project's commands see besides this machine's environment (none of whose `AWS_*`
/// they get: `buildlog::run`): the system variables (system_env.rs), then the project's own env
/// vars, which win. `skew`: skew protection is on for this build (`Request::skew_for`).
pub fn build_env(request: &Request, git: Option<&GitCommit>, skew: bool) -> BTreeMap<String, String> {
    let branch_url = system_env::build_branch_url(&request.project, &request.base_domain, request.target, request.pushed, git);
    let deployment = system_env::Deployment {
        project: &request.project,
        name: request.name.as_deref().unwrap_or(&request.project),
        id: &request.id,
        target: request.target,
        base_domain: &request.base_domain,
        git,
        branch_url: branch_url.as_deref(),
        user: &request.env,
        skew,
    };
    let mut env = system_env::build(&deployment, request.previous_sha.as_deref());
    env.extend(request.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_killed_build_says_memory() {
        let killed = std::process::Command::new("sh").args(["-c", "kill -9 $$"]).status().unwrap();
        let why = out_of_memory(killed, "Next.js").unwrap().to_string();
        assert!(why.starts_with("the Next.js build was killed (SIGKILL)") && why.contains("out of memory"), "{why}");
        // Raised as itself, so the builder tells the control function through every context.
        let e = anyhow::Error::new(out_of_memory(killed, "Vite").unwrap()).context("building");
        assert!(e.chain().any(|c| c.is::<OutOfMemory>()));
        let failed = std::process::Command::new("sh").args(["-c", "exit 1"]).status().unwrap();
        assert!(out_of_memory(failed, "Next.js").is_none(), "an ordinary failure is the build's own to explain");
    }

    fn request() -> Request {
        Request {
            id: "a1b2c3d4".into(),
            project: "shop".into(),
            name: None,
            target: Target::Preview,
            base_domain: "example.dev".into(),
            env: [("API_KEY".to_owned(), "k".to_owned()), ("AWS_REGION".to_owned(), "x".to_owned()), ("CI".to_owned(), "0".to_owned())].into(),
            git: None,
            deployer: None,
            pushed: false,
            previous_sha: None,
            framework: None,
            skew: false,
            skew_if_framework: false,
        }
    }

    /// A fake runtime: the ELF header of a Linux arm64 executable is all the check reads.
    fn options(dir: &Path) -> Options {
        let bootstrap = dir.join("bootstrap");
        let mut elf = b"\x7fELF\x02\x01\x01".to_vec();
        elf.resize(18, 0);
        elf.extend(183u16.to_le_bytes());
        std::fs::write(&bootstrap, elf).unwrap();
        Options { package: package::Options { bootstrap, python: "python3.13".into(), no_cache: false }, prebuilt: false, install: framework::Install::IfMissing }
    }

    #[test]
    fn a_pushs_build_asks_the_ignore_command_first() {
        let dir = package::TempDir::new("ignore-test").unwrap();
        let project = dir.path().join("shop");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("server.mjs"), "export default 1\n").unwrap();
        // Previews are skipped, production is built: Vercel's own example of the setting.
        std::fs::write(project.join("vercel.json"), r#"{"ignoreCommand": "if [ \"$VERCEL_ENV\" = production ]; then exit 1; else exit 0; fi"}"#).unwrap();
        let opts = options(dir.path());
        let log = crate::buildlog::Terminal;
        let pushed = |target| Request { pushed: true, target, previous_sha: Some("0".repeat(39) + "1"), ..request() };

        let skipped = build(&project, &pushed(Target::Preview), &opts, &dir.path().join("skipped"), &log).unwrap_err();
        assert!(skipped.downcast_ref::<Ignored>().is_some(), "{skipped:#}");
        assert!(!dir.path().join("skipped").join(BUILD_FILE).exists());
        assert_eq!(build(&project, &pushed(Target::Production), &opts, &dir.path().join("built"), &log).unwrap().entry, "server.mjs");
        // A deploy someone asked for by hand is built whatever the command would say.
        assert!(build(&project, &request(), &opts, &dir.path().join("by-hand"), &log).is_ok());
        // A command that fails (git, with no history here) doesn't stop the build.
        std::fs::write(project.join("vercel.json"), r#"{"ignoreCommand": "git diff HEAD^ HEAD --quiet -- ."}"#).unwrap();
        assert!(build(&project, &pushed(Target::Preview), &opts, &dir.path().join("git"), &log).is_ok());
        assert_eq!(build_env(&pushed(Target::Preview), None, false).get("VERCEL_GIT_PREVIOUS_SHA"), None, "only with a commit");
    }

    #[test]
    fn vercel_variables_then_the_projects() {
        let git = GitCommit {
            repo: "ada/shop".into(),
            branch: "feature/cart".into(),
            sha: "1a2b3c4".into(),
            message: "Add the cart".into(),
            full_sha: Some("1a2b3c4".repeat(5) + "12345"),
            author: Some("Ada Lovelace".into()),
        };
        let env = build_env(&request(), Some(&git), false);
        assert_eq!(env["VERCEL"], "1");
        assert_eq!(env["VERCEL_ENV"], "preview");
        assert_eq!(env["VERCEL_URL"], "shop-a1b2c3d4.example.dev");
        assert_eq!(env["VERCEL_PROJECT_PRODUCTION_URL"], "shop.example.dev");
        assert_eq!(env["VERCEL_DEPLOYMENT_ID"], "a1b2c3d4");
        assert_eq!((env["VERCEL_GIT_REPO_OWNER"].as_str(), env["VERCEL_GIT_REPO_SLUG"].as_str()), ("ada", "shop"));
        assert_eq!(env["VERCEL_GIT_COMMIT_REF"], "feature/cart");
        assert_eq!(env["VERCEL_GIT_COMMIT_SHA"].len(), 40);
        assert_eq!(env["VERCEL_GIT_COMMIT_AUTHOR_NAME"], "Ada Lovelace");
        // The project's own values win, its own AWS_* among them (the machine's never get in:
        // buildlog::run).
        assert_eq!(env["API_KEY"], "k");
        assert_eq!(env["CI"], "0");
        assert!(env.contains_key("AWS_REGION"));
        assert_eq!(env["FORKRAIL_ENV"], env["VERCEL_ENV"]);
        // Without a commit, no VERCEL_GIT_*.
        assert!(!build_env(&request(), None, false).keys().any(|k| k.starts_with("VERCEL_GIT_")));
    }

    #[test]
    fn skew_protection_reaches_the_build() {
        assert!(Request { skew: true, ..request() }.skew_for(None));
        // A new project's default: only for a framework that supports it.
        let new = Request { skew_if_framework: true, ..request() };
        assert!(new.skew_for(Some("SvelteKit")) && !new.skew_for(Some("Vite")) && !new.skew_for(None));
        assert!(!request().skew_for(Some("SvelteKit")));
        assert_eq!(build_env(&request(), None, true).get("VERCEL_SKEW_PROTECTION_ENABLED").map(String::as_str), Some("1"));
        assert!(!build_env(&request(), None, false).contains_key("VERCEL_SKEW_PROTECTION_ENABLED"));
        // Requests from before it read as off.
        let old: Request = serde_json::from_value(serde_json::json!({"id": "a1b2c3d4", "project": "shop", "target": "preview", "base_domain": "example.dev"})).unwrap();
        assert!(!old.skew && !old.skew_if_framework);
    }

    #[test]
    fn a_build_folder_round_trips() {
        let dir = package::TempDir::new("build-test").unwrap();
        let project = dir.path().join("shop");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("server.mjs"), "export default 1\n").unwrap();
        let out = dir.path().join("out");
        let opts = options(dir.path());
        let built = build(&project, &request(), &opts, &out, &crate::buildlog::Terminal).unwrap();
        assert_eq!(built.language, Language::Node);
        assert_eq!(built.entry, "server.mjs");
        assert_eq!(built.importer.as_deref(), Some(IMPORTER));
        assert_eq!(Build::read(&out).unwrap(), built);
        // A build.json from before `importer` was kept still reads.
        let mut old: Value = serde_json::from_slice(&std::fs::read(out.join(BUILD_FILE)).unwrap()).unwrap();
        old.as_object_mut().unwrap().remove("importer");
        std::fs::write(out.join(BUILD_FILE), serde_json::to_vec(&old).unwrap()).unwrap();
        assert_eq!(Build::read(&out).unwrap(), Build { importer: None, ..built.clone() });
        // A second build won't mix into the first's folder.
        assert!(build(&project, &request(), &opts, &out, &crate::buildlog::Terminal).unwrap_err().to_string().contains("isn't empty"));
        // A package that isn't the one described is refused.
        std::fs::write(out.join(PACKAGE_FILE), b"other").unwrap();
        assert!(Build::read(&out).unwrap_err().to_string().contains("isn't the package"));
    }

    /// A Next.js static export (`output: 'export'`) deploys as a static site, where forkrail used to
    /// refuse it: a visitor gets what Vercel serves for one, the pages by their names without
    /// `.html`, Next's redirects and the project's own routing keys around them, and the export's
    /// 404 page for what's not there; with a base path, all of it under that path.
    #[test]
    fn a_next_static_export_deploys_as_a_static_site() {
        use forkrail_common::rules::{Engine, Outcome, Program, Request as Visit};
        let dir = package::TempDir::new("next-export").unwrap();
        let opts = Options { prebuilt: true, ..options(dir.path()) };
        let site = |name: &str, config: &str, redirects: &str, files: &[&str], vercel_json: Option<&str>| -> PathBuf {
            let project = dir.path().join(name);
            let out = project.join("out");
            let mut write = vec![
                ("package.json".to_owned(), r#"{"scripts": {"build": "next build"}, "dependencies": {"next": "16.4.6"}}"#.to_owned()),
                ("node_modules/next/package.json".to_owned(), r#"{"version": "16.4.6"}"#.to_owned()),
                (".next/BUILD_ID".to_owned(), "b1".to_owned()),
                (".next/required-server-files.json".to_owned(), format!(r#"{{"version": 1, "config": {config}}}"#)),
                (".next/routes-manifest.json".to_owned(), format!(r#"{{"version": 3, "redirects": {redirects}, "headers": [], "rewrites": {{"beforeFiles": [], "afterFiles": [], "fallback": []}}}}"#)),
                (".next/export-detail.json".to_owned(), format!(r#"{{"version": 1, "outDirectory": {:?}, "success": true}}"#, out.to_string_lossy())),
                ("out/_next/static/chunks/a.js".to_owned(), "js".to_owned()),
            ];
            write.extend(files.iter().map(|f| (format!("out/{f}"), format!("<p>{f}</p>"))));
            write.extend(vercel_json.map(|v| ("vercel.json".to_owned(), v.to_owned())));
            for (rel, text) in write {
                let path = project.join(rel);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, text).unwrap();
            }
            project
        };
        let deploy = |project: &Path| {
            let out = project.with_extension("out");
            let built = build(project, &request(), &opts, &out, &crate::buildlog::Terminal).unwrap();
            let read = |key: String| std::fs::read(out.join(key)).unwrap();
            let program: Program = serde_json::from_slice(&read(forkrail_common::statics::rules_key(built.rules.as_deref().unwrap()))).unwrap();
            let manifest: forkrail_common::statics::Manifest = serde_json::from_slice(&read(forkrail_common::statics::manifest_key(&built.static_files.as_ref().unwrap().id))).unwrap();
            (built, program, manifest)
        };
        let visit = |program: &Program, manifest: &forkrail_common::statics::Manifest, path: &str| {
            let headers = hyper::HeaderMap::new();
            let d = Engine::new(program).unwrap().route(&Visit { method: "GET", path, query: None, host: "shop.example.dev", headers: &headers }, manifest);
            (d.outcome, d.status)
        };
        let page = |file: &str| Outcome::Static { file: file.to_owned() };
        let moved = |to: &str| Outcome::Redirect { location: to.to_owned() };

        // Without a trailing slash, the internal redirect Next writes takes it off.
        let plain = site(
            "plain",
            r#"{"output": "export"}"#,
            r#"[{"source": "/:path+/", "destination": "/:path+", "internal": true, "statusCode": 308, "regex": "^(?:/((?:[^/]+?)(?:/(?:[^/]+?))*))/$"}, {"source": "/old", "destination": "/about", "statusCode": 307, "regex": "^(?!/_next)/old(?:/)?$"}]"#,
            &["index.html", "about.html", "about.txt", "blog/first.html", "404.html"],
            Some(r#"{"redirects": [{"source": "/from-vercel-json", "destination": "/about"}]}"#),
        );
        let (built, program, manifest) = deploy(&plain);
        assert_eq!((built.package.as_ref(), built.runtime.as_ref(), built.next.as_ref()), (None, None, None), "no function, no server");
        assert_eq!(built.framework.as_deref(), Some("Next.js"));
        assert_eq!(built.entry, "");
        for (path, outcome, status) in [
            ("/", page("/index.html"), None),
            ("/about", page("/about.html"), None),
            ("/blog/first", page("/blog/first.html"), None),
            ("/about.txt", page("/about.txt"), None),
            // Vercel renames the pages: their `.html` names aren't found.
            ("/about.html", page("/404.html"), Some(404)),
            ("/about/", moved("/about"), Some(308)),
            ("/old", moved("/about"), Some(307)),
            ("/from-vercel-json", moved("/about"), Some(308)),
            ("/404", page("/404.html"), Some(404)),
            ("/nope", page("/404.html"), Some(404)),
            ("/_next/data/b1/x.json", page("/404.html"), Some(404)),
        ] {
            assert_eq!(visit(&program, &manifest, path), (outcome, status), "{path}");
        }
        assert_eq!(manifest.files["/_next/static/chunks/a.js"].cache_control, forkrail_common::statics::IMMUTABLE, "Next's hashed files are cached for good");

        // With a trailing slash and a base path, as Next writes them both.
        let based = site(
            "based",
            r#"{"output": "export", "basePath": "/docs", "trailingSlash": true}"#,
            r#"[{"source": "/docs", "destination": "/docs/", "basePath": false, "internal": true, "statusCode": 308, "regex": "^/docs$"},
                {"source": "/:file((?!\\.well-known(?:/.*)?)(?:[^/]+/)*[^/]+\\.\\w+)/", "destination": "/:file", "internal": true, "statusCode": 308, "regex": "^(?:/((?!\\.well-known(?:/.*)?)(?:[^/]+/)*[^/]+\\.\\w+))/$"},
                {"source": "/:notfile((?!\\.well-known(?:/.*)?)(?:[^/]+/)*[^/\\.]+)", "destination": "/:notfile/", "internal": true, "statusCode": 308, "regex": "^(?:/((?!\\.well-known(?:/.*)?)(?:[^/]+/)*[^/\\.]+))$"}]"#,
            &["index.html", "about/index.html", "404.html", "404/index.html"],
            None,
        );
        let (_, program, manifest) = deploy(&based);
        for (path, outcome, status) in [
            ("/docs", moved("/docs/"), Some(308)),
            ("/docs/", page("/docs/index.html"), None),
            ("/docs/about", moved("/docs/about/"), Some(308)),
            ("/docs/about/", page("/docs/about/index.html"), None),
            ("/docs/_next/static/chunks/a.js", page("/docs/_next/static/chunks/a.js"), None),
            // With `trailingSlash` Next writes the 404 page twice, `404.html` and `404/index.html`.
            ("/docs/nope/", page("/docs/404/index.html"), Some(404)),
            ("/docs/404/", page("/docs/404/index.html"), Some(404)),
            // Outside the base path there's nothing of the site's.
            ("/about/", Outcome::NotFound, Some(404)),
        ] {
            assert_eq!(visit(&program, &manifest, path), (outcome, status), "{path}");
        }
    }

    #[test]
    fn warns_of_packages_that_work_only_on_vercel() {
        struct Lines(std::sync::Mutex<Vec<String>>);
        impl BuildLog for Lines {
            fn line(&self, _: buildlog::Stream, text: &str) {
                self.0.lock().unwrap().push(text.to_owned());
            }
        }
        let dir = package::TempDir::new("vercel-only").unwrap();
        let opts = options(dir.path());
        let deps = r#""dependencies": {"@vercel/blob": "^2.9.0", "workflow": "^5.5.0"}"#;
        // A Node server: both warned of, in the build log, naming the variables and never a value.
        let server = dir.path().join("server");
        std::fs::create_dir_all(server.join("node_modules")).unwrap();
        std::fs::write(server.join("server.mjs"), "export default 1\n").unwrap();
        std::fs::write(server.join("package.json"), format!(r#"{{"type": "module", {deps}}}"#)).unwrap();
        let log = Lines(Default::default());
        let with_env = Request { env: [("BLOB_STORE_ID".to_owned(), "store-value-7f3a".to_owned())].into(), ..request() };
        build(&server, &with_env, &opts, &dir.path().join("server-out"), &log).unwrap();
        let lines = log.0.into_inner().unwrap();
        let warnings: Vec<&String> = lines.iter().filter(|l| l.starts_with("warning: ")).collect();
        assert_eq!(warnings.len(), 2, "{lines:#?}");
        assert!(warnings[0].starts_with("warning: @vercel/blob: BLOB_STORE_ID without BLOB_READ_WRITE_TOKEN"), "{}", warnings[0]);
        assert!(warnings[1].starts_with("warning: workflow: ") && warnings[1].contains("WORKFLOW_TARGET_WORLD isn't set for preview"), "{}", warnings[1]);
        assert!(!lines.iter().any(|l| l.contains("store-value-7f3a")), "a value was logged");
        // A static site runs no code of its own here: nothing to say.
        let site = dir.path().join("site");
        std::fs::create_dir_all(&site).unwrap();
        std::fs::write(site.join("index.html"), "<p>hi</p>").unwrap();
        std::fs::write(site.join("package.json"), format!("{{{deps}}}")).unwrap();
        let log = Lines(Default::default());
        build(&site, &request(), &opts, &dir.path().join("site-out"), &log).unwrap();
        let lines = log.0.into_inner().unwrap();
        assert!(!lines.iter().any(|l| l.starts_with("warning: ")), "{lines:#?}");
    }
}
