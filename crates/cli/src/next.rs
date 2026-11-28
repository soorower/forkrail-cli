//! Next.js 13.5 to 16, App and Pages Router, run as Next's own server.
//!
//! forkrail runs the project's build and takes Next's **standalone output** (`.next/standalone`:
//! `server.js` with the files Next traced for it), which runs as one function under forkrail's
//! runtime like any Node server. Next routes, runs middleware, renders and reads its caches itself.
//! The project is never changed:
//!
//! - **The build** is the project's own (`buildCommand`, its `build` script, else `next build`),
//!   with `NEXT_PRIVATE_STANDALONE=1` (every version takes it as the default `output`) and, on
//!   Next 15.5 and 16.2 and later (not 16.0, 16.1: `MINIMAL_WITH_ADAPTER`; not 16.3.0 to 16.3.4:
//!   `ADAPTER_CRASHES`), forkrail's adapter
//!   (next_adapter.cjs, written into a scratch folder and
//!   named by `NEXT_ADAPTER_PATH`), whose `modifyConfig` sets `output: 'standalone'` over whatever
//!   the config says and is where the shared cache's handlers go in (`cache_seam`). On 16.2 and
//!   later the adapter's `onBuildComplete` also writes what Next hands it; before that the same
//!   is read from `.next`'s manifests.
//! - **The manifest** (`next.json` in the build folder, `Manifest`): the build id, base path,
//!   middleware matchers, prerenders with their files and cache tags, each route's settings, the
//!   image config and the static files. The router, the cache's seed and the runtime read it.
//! - **The package** is the standalone folder as it is, links kept as links (pnpm's and
//!   Turbopack's `node_modules`), every file with its own time (Next takes a prerendered page's
//!   file time for when it was made), with the build machine's `sharp` replaced by Linux arm64's.
//! - **Static files** (`.next/static` under `/_next/static/`, cached for good, and `public/`) go to
//!   the router, which answers them before the function. A file the app's middleware may match
//!   stays with Next's server instead, so the middleware still runs for it.
//! - **A static export** (`output: 'export'`) has no server: its exported folder is deployed as a
//!   static site, as Vercel deploys one (next_export.rs).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::buildlog::BuildLog;
use crate::framework::{Framework, Output};
use crate::package::{self, Entry, Source, TempDir};

/// For the user, and in a deployment's record.
pub const NAME: &str = "Next.js";
/// Vercel's slug for it.
pub const SLUG: &str = "nextjs";
/// The importer a Next build is recorded with (`build::Build::importer`).
pub const IMPORTER: &str = "next";
/// What the runtime starts: forkrail's entry beside Next's `server.js` (so `apps/web/` before it in
/// a workspace). It loads `HELPER` and runs `server.js` as `SERVER_ENTRY_JS` says.
pub const ENTRY: &str = "forkrail-server.mjs";
/// forkrail's part of running Next's server (the visitor's origin, priming at start, the router's
/// files), beside `server.js`: the runtime's `next-standalone.cjs`.
pub const HELPER: &str = "forkrail-next.cjs";
const HELPER_JS: &str = include_str!("../../runtime/src/next-standalone.cjs");
/// Beside `server.js`: the static files the router serves and the package doesn't hold, one path
/// per line (with the base path), so Next's server under forkrail knows them as its own when a
/// rewrite lands on one, and tells the router to answer with it (`next-standalone.cjs`, 3).
pub const ROUTER_FILES: &str = "forkrail-router-files.txt";
/// The entry, as `next-standalone.cjs` gives it. The order is its contract: `prime()` and
/// `routerFiles()`, then `server.js`, then `install()`, with nothing awaited between.
const SERVER_ENTRY_JS: &str = "import { createRequire } from 'node:module'
import { fileURLToPath } from 'node:url'
const forkrail = createRequire(import.meta.url)('./forkrail-next.cjs')
forkrail.prime()
forkrail.routerFiles()
await import('./server.js')
forkrail.install(fileURLToPath(new URL('./server.js', import.meta.url)))
";
/// The manifest, in the build folder beside `build.json`.
pub const MANIFEST_FILE: &str = "next.json";
/// The seed of the shared cache (nextseed.rs), in the build folder: what's put under
/// `static/next-seed/<deployment>/` in each region.
pub const SEED_DIR: &str = "next-seed";
/// What a server that calls Next's entrypoints directly needs of the build (Next 16.2 and later),
/// in the build folder: `Handlers`.
pub const HANDLERS_FILE: &str = "next-handlers.json";
/// The version of the manifest's layout. Fields added with a default keep it.
pub const FORMAT: u32 = 1;
/// What the adapter writes in the dist folder (next_adapter.cjs).
const ADAPTER_OUTPUT: &str = "forkrail-next.json";
const ADAPTER: &str = include_str!("next_adapter.cjs");
/// The build variable the adapter reads the shared cache's handlers from (`cache_seam`).
pub const CACHE_ENV: &str = "FORKRAIL_NEXT_CACHE_HANDLERS";
/// Whether the prerendered files leave the package once the shared cache's seed has them (`seed`,
/// the default) or stay in it as well (`package`: for running the package without forkrail's
/// router, where Next reads them itself).
pub const PRERENDERS_ENV: &str = "FORKRAIL_NEXT_PRERENDERS";
/// The function's variable that has the runtime hand Next forkrail's cache handlers (N2).
pub const CACHE_SWITCH: &str = "FORKRAIL_NEXT_CACHE";
/// Next's `deploymentId`, from the build's environment.
pub const DEPLOYMENT_ID_ENV: &str = "NEXT_DEPLOYMENT_ID";
/// The first Next that takes `NEXT_DEPLOYMENT_ID` with nothing in its config (Vercel's docs: earlier
/// ones from 13.4.7 need `experimental.useDeploymentId`).
const SKEW_FROM: Version = Version(14, 1, 4);

/// The oldest Next that builds here: standalone output with its own `server.js` as it is today.
pub const OLDEST: Version = Version(13, 5, 0);
/// The first with `NEXT_ADAPTER_PATH` (experimental until 16.2).
const ADAPTER_FROM: Version = Version(15, 5, 0);
/// 16.0 and 16.1 build the whole app in minimal mode when an adapter is named
/// (`process.env.MINIMAL_MODE` from `experimental.adapterPath`, `build/define-env`): its pages then
/// leave routing to the platform, which for S is Next's own server (a closed route rendered a path
/// it doesn't list). There the adapter has nothing else to do below 16.2 (`output` is
/// `NEXT_PRIVATE_STANDALONE`'s default), so they build without it, as 15.4 and older do.
const MINIMAL_WITH_ADAPTER: (Version, Version) = (Version(16, 0, 0), Version(16, 2, 0));
/// 16.3.0 to 16.3.4 fail `next build` when any adapter is named and the output is standalone
/// (`ENOENT .next/next-server.js.nft.json` after the pages are made; vercel/next.js#96646, fixed in
/// 16.3.5; seen with the adapter's hook and without it). They build without one, as 16.0 and 16.1
/// do: they run Next's own server (direct handlers are for verified versions only), so what's
/// missed is the outputs only H reads, and the manifest is read from `.next` instead.
const ADAPTER_CRASHES: (Version, Version) = (Version(16, 3, 0), Version(16, 3, 5));
/// The first whose adapter hook gets the build's outputs (the adapter's `onBuildComplete` is
/// defined only from here: `OUTPUTS_ENV`).
const OUTPUTS_FROM: Version = Version(16, 2, 0);
/// Tells the adapter to define `onBuildComplete`.
const OUTPUTS_ENV: &str = "FORKRAIL_NEXT_OUTPUTS";
/// The first that finds cache handlers at run time (`globalThis[Symbol.for('@next/cache-handlers')]`)
/// when its config names none. Before it, the config must name one (`next_preload.cjs`).
const HANDLERS_FOUND_FROM: Version = Version(15, 0, 0);
/// For 13.5 to 14.2: what the build's Node processes preload, and the handler it names, a file of
/// forkrail's in the project's `node_modules/.forkrail/` (Next traces it into the standalone folder).
const PRELOAD: &str = include_str!("next_preload.cjs");
const STUB_DIR: &str = "node_modules/.forkrail";
const STUB_FILE: &str = "forkrail-cache-handler.cjs";
/// The handler: forkrail's in a worker whose runtime has the cache on, Next's own file cache
/// anywhere else (the build itself). The same as `STUB` in the runtime's `next-cache.mjs`.
const STUB: &str = "const forkrail = globalThis[Symbol.for('forkrail.next-cache')];
module.exports = forkrail ? forkrail.IncrementalCache : require('next/dist/server/lib/incremental-cache/file-system-cache').default;
";

/// Variables of the machine a Next build never sees.
///
/// `NOW_BUILDER` tells Next it builds on Vercel's own infrastructure: it then leaves its image
/// optimizer and sharp out of the server (Vercel answers `/_next/image` itself), turns off its
/// compression and writes its server's config for Vercel's proxy. forkrail runs Next's server
/// whole, so the build must not have it (`before_build` takes forkrail's own off too).
///
/// The rest make Next's CLI take the build for an AI agent's (its `@vercel/detect-agent`): Next
/// 16.4.3 and later then end a finished build with exit 1 to show an upgrade notice. A
/// deployment's build is no agent's session, wherever the CLI was started from.
pub const UNSET: [&str; 19] = [
    "NOW_BUILDER",
    "AI_AGENT",
    "ANTIGRAVITY_AGENT",
    "AUGMENT_AGENT",
    "CLAUDECODE",
    "CLAUDE_CODE",
    "CLAUDE_CODE_IS_COWORK",
    "CODEX_CI",
    "CODEX_SANDBOX",
    "CODEX_THREAD_ID",
    "COPILOT_ALLOW_ALL",
    "COPILOT_GITHUB_TOKEN",
    "COPILOT_MODEL",
    "CURSOR_AGENT",
    "CURSOR_EXTENSION_HOST_ROLE",
    "CURSOR_TRACE_ID",
    "GEMINI_CLI",
    "OPENCODE_CLIENT",
    "REPL_ID",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    /// `16.4.6`, `16.4.0-canary.48`, `15.5.29`.
    pub fn parse(text: &str) -> Option<Version> {
        let core = text.trim().trim_start_matches('v').split(['-', '+']).next()?;
        let mut parts = core.split('.').map(|p| p.parse::<u32>().ok());
        Some(Version(parts.next()??, parts.next().flatten().unwrap_or(0), parts.next().flatten().unwrap_or(0)))
    }
}

/// The Next the project has installed: `node_modules/next` in its folder or one above it (a
/// workspace hoists it), as Node finds it.
pub fn installed(dir: &Path) -> Result<(String, Version)> {
    for folder in dir.ancestors() {
        let path = folder.join("node_modules/next/package.json");
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let package: Value = serde_json::from_str(&text).with_context(|| format!("{} isn't valid JSON", path.display()))?;
        let text = package.get("version").and_then(Value::as_str).with_context(|| format!("{} names no version", path.display()))?;
        let version = Version::parse(text).with_context(|| format!("{}: {text:?} isn't a version", path.display()))?;
        return Ok((text.to_owned(), version));
    }
    // Yarn's Plug'n'Play keeps no node_modules, which Next's standalone output is made of.
    if dir.ancestors().any(|folder| folder.join(".pnp.cjs").is_file() || folder.join(".pnp.loader.mjs").is_file()) {
        bail!("{NAME} with Yarn Plug'n'Play isn't supported: Next's standalone server is traced from node_modules. Set `nodeLinker: node-modules` in .yarnrc.yml");
    }
    bail!("Next.js isn't installed: no node_modules/next in {} or above it (did the install command run?)", dir.display())
}

/// The framework for a Next project. `overrides` is `vercel.json`, whose `buildCommand` and
/// `installCommand` win. Its `outputDirectory` names where Next's output folder is, as Vercel's
/// Next builder takes it (it doesn't read next.config): a project whose `distDir` is elsewhere, or
/// an Nx or workspace build that writes it deep down (`dist/apps/web/.next`, further than the
/// search finds), says so there. It's the first place looked at (`find_dist`).
pub fn framework(dir: &Path, package: Option<&Value>, overrides: Option<&Value>) -> Result<Framework> {
    if package.is_none() {
        bail!("{NAME} needs a package.json");
    }
    let script = |key: &str| overrides.and_then(|o| o.get(key)).and_then(Value::as_str).map(str::to_owned);
    let output = match script("outputDirectory") {
        Some(named) => crate::framework::clean_output(&named)?,
        None => DIST.to_owned(),
    };
    let manager = crate::framework::package_manager_name(dir);
    let scripts = package.and_then(|p| p.get("scripts"));
    let has = |name: &str| scripts.and_then(|s| s.get(name)).is_some_and(Value::is_string);
    // As Vercel's builder: `vercel-build`, then `now-build`, then `build`, else `next build`.
    let build = match script("buildCommand") {
        Some(command) => command,
        None => match ["vercel-build", "now-build", "build"].into_iter().find(|s| has(s)) {
            Some(run) => format!("{manager} run {run}"),
            None => "next build".to_owned(),
        },
    };
    Ok(Framework {
        name: NAME,
        install: script("installCommand").unwrap_or_else(|| crate::framework::install_command(dir)),
        build: Some(build),
        output,
        entry: ENTRY,
        public: "",
        build_env: &[],
        install_env: &[],
        // Vercel's preset names `NEXT_PUBLIC_`.
        env_prefix: "NEXT_PUBLIC_",
        // Vercel's `cachePattern` for Next: its compiler's cache, beside `node_modules`. The
        // builder keeps it in an archive of its own, after every build (cache.rs `split_next`).
        cache: &[CACHE_DIR],
        find: Output::Next,
        hashed: &[],
        routes: "",
        unset: &UNSET,
    })
}

/// Next's default `distDir`.
pub const DIST: &str = ".next";
/// Next's own cache in the default `distDir`: its compilers' caches, and the keys it keeps
/// between builds there (`.rscinfo`, `.previewinfo`).
pub const CACHE_DIR: &str = ".next/cache";
/// The file in `.next/cache` with the key Next encrypts Server Actions' bound arguments with,
/// and when it renews it (14 days on), from Next 15 on (`encryption-utils-server.js`). Next
/// writes none where it thinks it runs in Docker (`/.dockerenv`, or `docker` in the cgroup).
pub const ACTIONS_KEY_FILE: &str = ".rscinfo";
/// Where Next 16.4 writes service workers, inside its static folder (`build/index.ts`).
pub(crate) const SERVICE_WORKERS: &str = "service-worker/";

/// Cache handlers to put in this build's config (`cacheHandler`, `cacheHandlers`,
/// `cacheMaxMemorySize`), as the adapter takes them in `FORKRAIL_NEXT_CACHE_HANDLERS`. None:
/// without them Next keeps its caches in each worker's memory and its package's files, as
/// `next start` does on one machine; and the shared cache, as it is being built, needs none in
/// the config from Next 15 on (its handlers are found at run time, by the name Next looks them
/// up under), so this stays the place for a build that does need one.
///
/// A handler file must be inside the project (Turbopack refuses a path outside its root), so
/// what supplies one writes it there first. Next 13.5 to 14.2 get theirs through the config
/// loader instead (`next_preload.cjs`); 15.0 to 15.4 need none.
pub fn cache_seam(_dir: &Path, _version: Version) -> Option<Value> {
    None
}

/// What a build of Next is given that mustn't outlast it: the adapter's file, and on 13.5 to 14.2
/// the cache handler forkrail puts in the project's `node_modules`. Gone when this is dropped,
/// whichever way the build ended.
pub struct Adapter {
    _scratch: TempDir,
    /// The handler's file, and the folders made for it (deepest first) to take away if empty.
    stub: Option<(PathBuf, Vec<PathBuf>)>,
}

impl Drop for Adapter {
    fn drop(&mut self) {
        if let Some((file, made)) = self.stub.take() {
            let _ = std::fs::remove_file(&file);
            for dir in made {
                // Only if empty: what else is there stays.
                let _ = std::fs::remove_dir(&dir);
            }
        }
    }
}

/// Write the cache handler into `dir/node_modules/.forkrail/`: its path, and the folders that
/// weren't there before (deepest first).
fn write_stub(dir: &Path) -> Result<(PathBuf, Vec<PathBuf>)> {
    let folder = dir.join(STUB_DIR);
    let mut made = Vec::new();
    let mut at = folder.as_path();
    while !at.exists() {
        made.push(at.to_path_buf());
        match at.parent() {
            Some(parent) if parent.starts_with(dir) => at = parent,
            _ => break,
        }
    }
    std::fs::create_dir_all(&folder).with_context(|| format!("creating {}", folder.display()))?;
    let file = folder.join(STUB_FILE);
    std::fs::write(&file, STUB).with_context(|| format!("writing {}", file.display()))?;
    Ok((file, made))
}

/// `NODE_OPTIONS` with `--require <file>` after what it already had.
fn with_require(options: Option<&str>, file: &Path) -> String {
    let path = file.to_string_lossy();
    let arg = if path.contains([' ', '"', '\\']) { format!("--require \"{}\"", path.replace('\\', "\\\\").replace('"', "\\\"")) } else { format!("--require {path}") };
    match options.map(str::trim).filter(|o| !o.is_empty()) {
        Some(own) => format!("{own} {arg}"),
        None => arg,
    }
}

/// Between the install and the build: checks the Next that was installed and gives the build its
/// variables. The returned value must live until the build has ended.
pub fn before_build(dir: &Path, env: &mut BTreeMap<String, String>, log: &dyn BuildLog) -> Result<Adapter> {
    let (text, version) = installed(dir)?;
    if version < OLDEST {
        bail!(
            "{NAME} {text} isn't supported: forkrail runs Next's standalone server, from Next {}.{} on (this project has {text}); upgrade Next to deploy it",
            OLDEST.0,
            OLDEST.1
        );
    }
    let scratch = TempDir::new("next")?;
    // Other frameworks' builds are told they build as on Vercel; Next's must not be (`UNSET`).
    env.remove("NOW_BUILDER");
    // Skew protection on (`skew.rs`): the deployment's id is Next's `deploymentId`, as Vercel's
    // build sets it.
    let skew = env.get(crate::skew::ENV).is_some_and(|v| v == "1").then(|| env.get("VERCEL_DEPLOYMENT_ID").cloned()).flatten();
    let mut set = |key: &str, value: String| {
        // The project's own value wins, as with every build variable.
        env.entry(key.to_owned()).or_insert(value);
    };
    set("NEXT_TELEMETRY_DISABLED", "1".to_owned());
    // The default `output` on every version; the adapter's modifyConfig also sets it where there
    // is one, over a config that names another.
    set("NEXT_PRIVATE_STANDALONE", "1".to_owned());
    // Next then puts it on its assets (`?dpl=`) and on its navigations' and Server Actions'
    // requests (`x-deployment-id`), which the routers send to the deployment that served the page.
    // A value of the project's own wins.
    if let Some(id) = skew {
        set(DEPLOYMENT_ID_ENV, id);
        if version < SKEW_FROM {
            log.info(&format!(
                "note: {NAME} {text} puts the deployment's id on its requests (skew protection) only with `experimental.useDeploymentId: true` in its config (and `useDeploymentIdServerActions` for Server Actions); {}.{}.{} and later always do",
                SKEW_FROM.0, SKEW_FROM.1, SKEW_FROM.2
            ));
        }
    }
    let within = |(from, to): (Version, Version)| version >= from && version < to;
    let minimal = within(MINIMAL_WITH_ADAPTER);
    let crashes = within(ADAPTER_CRASHES);
    if version >= ADAPTER_FROM && !minimal && !crashes {
        let adapter = scratch.path().join("forkrail-next-adapter.cjs");
        std::fs::write(&adapter, ADAPTER).with_context(|| format!("writing {}", adapter.display()))?;
        set("NEXT_ADAPTER_PATH", adapter.to_string_lossy().into_owned());
        if version >= OUTPUTS_FROM {
            set(OUTPUTS_ENV, "1".to_owned());
        }
        if let Some(seam) = cache_seam(dir, version) {
            set(CACHE_ENV, seam.to_string());
        }
        log.info(&format!("{NAME} {text}: building its standalone server, through forkrail's adapter"));
    } else if crashes {
        log.info(&format!(
            "{NAME} {text}: building its standalone server (NEXT_PRIVATE_STANDALONE=1), without forkrail's adapter: {text}'s build fails with any adapter and a standalone server (fixed in {}.{}.{}). It runs Next's own server either way",
            ADAPTER_CRASHES.1.0, ADAPTER_CRASHES.1.1, ADAPTER_CRASHES.1.2
        ));
    } else {
        log.info(&format!("{NAME} {text}: building its standalone server (NEXT_PRIVATE_STANDALONE=1)"));
    }
    // 13.5 to 14.2 take a cache handler only from the config: the build's config loader names
    // forkrail's (next_preload.cjs), unless the project names its own. Nothing of the project's
    // changes; the handler's file goes when the build ends (`Adapter`'s drop).
    let mut stub = None;
    if version < HANDLERS_FOUND_FROM {
        let preload = scratch.path().join("forkrail-next-preload.cjs");
        std::fs::write(&preload, PRELOAD).with_context(|| format!("writing {}", preload.display()))?;
        let (file, made) = write_stub(dir)?;
        let options = env.get("NODE_OPTIONS").cloned().or_else(|| std::env::var("NODE_OPTIONS").ok());
        env.insert("NODE_OPTIONS".to_owned(), with_require(options.as_deref(), &preload));
        env.insert("FORKRAIL_NEXT_CACHE_STUB".to_owned(), file.to_string_lossy().into_owned());
        stub = Some((file, made));
    }
    Ok(Adapter { _scratch: scratch, stub })
}

// ---- The manifest ----

/// `next.json`: what a Next build is, for the parts of forkrail that serve it. Paths of files are
/// relative to the package's `app/` (and to `next/prerender/` in the build folder).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub format: u32,
    pub next_version: String,
    /// Where it was read from: Next 16.2's adapter outputs, or `.next`'s manifests.
    pub source: From,
    pub build_id: String,
    /// `deploymentId` as the build had it (`NEXT_DEPLOYMENT_ID`), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<String>,
    /// `basePath` (`/docs`), or empty.
    pub base_path: String,
    /// `assetPrefix` as configured, or empty.
    pub asset_prefix: String,
    pub trailing_slash: bool,
    /// `i18n` as configured (`locales`, `defaultLocale`, `domains`, `localeDetection`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub i18n: Option<Value>,
    /// The folder of Next's `server.js` in the package: empty at its top, `apps/web` in a workspace.
    pub app_dir: String,
    /// Next's dist folder, relative to `app_dir` (`.next`).
    pub dist_dir: String,
    /// What the runtime starts.
    pub entry: String,
    pub preview: Preview,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub middleware: Option<Middleware>,
    /// `routes-manifest.json`'s routing: `redirects`, `headers`, `rewrites`, `dynamicRoutes`,
    /// `staticRoutes`, `dataRoutes`, `rsc`, `caseSensitive`, `pages404`, as Next wrote them.
    pub routes: Value,
    /// Next 16.2 and later: the routing lists as Next hands them to an adapter
    /// (`beforeMiddleware`, `beforeFiles`, `afterFiles`, `dynamicRoutes`, `onMatch`, `fallback`,
    /// `shouldNormalizeNextData`, `rsc`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter_routing: Option<Value>,
    /// Cache Components (`cacheComponents`) and Partial Prerendering are on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cache_components: bool,
    /// The project names a cache handler of its own (`cacheHandler`, or 13.5's
    /// `experimental.incrementalCacheHandlerPath`): Next keeps its pages through that, not through
    /// forkrail's shared cache.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cache_handler: bool,
    /// Next's caches are forkrail's shared cache (N2): its pages, route answers, fetches and
    /// `'use cache'` entries are the router's, one store for the deployment, seeded from the build;
    /// the function's environment has `FORKRAIL_NEXT_CACHE=1` (a name projects can't set). Off when
    /// the project names its own handler, or (13.5 to 14.2) when the build's config didn't take
    /// forkrail's.
    #[serde(default)]
    pub shared_cache: bool,
    /// The routes that run code, each once.
    pub functions: Vec<Function>,
    pub prerenders: Vec<Prerender>,
    /// Whether the package holds the prerendered files too (`package`), or only the build
    /// folder's `next/prerender/` does (`seed`).
    pub prerender_files: Kept,
    /// Next 16.2 and later: the file in the build folder (`next-handlers.json`) that lists every
    /// entrypoint with its traced files, for a package that calls them directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handlers: Option<String>,
    /// `images-manifest.json`'s `images`, as Next wrote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Value>,
    pub static_files: Vec<StaticFile>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum From {
    Adapter,
    Manifests,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kept {
    Package,
    Seed,
}

/// Preview mode's id: what a request's `x-prerender-revalidate` header must be for Next to
/// regenerate a page on demand, and what Draft Mode's bypass cookie is checked against. A secret
/// of the build (it is in the package's `prerender-manifest.json` too).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preview {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Middleware {
    /// `edge` (`middleware.ts`) or `nodejs` (`proxy.ts`, Node middleware).
    pub runtime: String,
    pub matchers: Vec<Matcher>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Matcher {
    /// As written (`/dashboard/:path*`), when Next kept it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The pattern Next matches a request's path with (with the base path, the locale and the
    /// `/_next/data` prefix it allows for).
    pub regexp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Function {
    /// The path it answers (`/api/hello`, `/blog/[slug]`), without the base path.
    pub pathname: String,
    /// Its page in the project (`/api/hello/route`, `/blog/[slug]/page`, `/api/hello`).
    pub page: String,
    pub kind: FunctionKind,
    /// `nodejs` or `edge`.
    pub runtime: String,
    /// `maxDuration` from the route's own config, else from `vercel.json`'s `functions`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration: Option<u64>,
    /// `memory` from `vercel.json`'s `functions`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<u32>,
    /// `preferredRegion` from the route's config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regions: Option<Value>,
    /// Next 16.2 and later: the output's id, as `next-handlers.json` and a prerender's `parent`
    /// name it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// For an Edge entry: what registers and calls it (`modulePath`, `entryKey`, `handlerExport`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge_runtime: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FunctionKind {
    /// A Pages Router page.
    Page,
    /// A Pages Router API route.
    Api,
    AppPage,
    AppRoute,
}

/// One prerendered path with everything Next made for it at build.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Prerender {
    /// The path (`/blog/first`), as Next lists it: without the base path, with its locale.
    pub pathname: String,
    /// The route it belongs to (`/blog/[slug]`); the path itself for a route without parameters.
    pub route: String,
    /// The route's page in the project (`/(site)/blog/[slug]/page`; a Pages Router page's is its
    /// route): what Next names the route's cache entries after, from 15.5.27 and 16.3.8 on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<String>,
    pub router: Router,
    pub kind: PrerenderKind,
    /// Next 16.2 and later: the id of the output that renders it (`parentOutputId`), and the
    /// headers that chain a PPR shell's resume (`pprChain`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ppr_chain: Option<Value>,
    /// Prerenders of one group are revalidated together (Next 16.2 and later).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<u64>,
    /// Seconds until it's stale; None never (only on demand).
    pub revalidate: Option<u64>,
    /// Seconds until it may no longer be served at all, when Next says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expire: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// The headers it is served with.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Its cache tags, from `x-next-cache-tags`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Partial Prerendering's postponed state, for a shell that resumes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postponed: Option<String>,
    /// The query parameters that make pages apart: empty none, None when Next didn't say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_query: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_header: Option<Vec<String>>,
    /// Requests that must not be answered from a cache (server actions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bypass_for: Option<Value>,
    /// `PARTIALLY_STATIC` for a PPR page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rendering_mode: Option<String>,
    /// Its files: the page, its RSC payload, data and segments, each with the path it answers.
    pub files: Vec<PrerenderFile>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Router {
    App,
    Pages,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrerenderKind {
    /// A whole page.
    Page,
    /// A route handler's answer.
    Route,
    /// A PPR shell: the static part of a page, the rest rendered on request.
    Shell,
    /// A page served for paths of a dynamic route not made yet.
    Fallback,
    /// Files with no page of their own in the build's list.
    Data,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrerenderFile {
    /// The path it answers, without the base path: `/blog/first`, `/blog/first.rsc`,
    /// `/_next/data/<id>/blog/first.json`, `/blog/first.segments/_tree.segment.rsc`; the home
    /// page's are `/`, `/index.rsc`, `/index.segments/…`.
    pub pathname: String,
    pub kind: FileKind,
    /// The file, relative to `app/` in the package and to `next/prerender/` in the build folder.
    pub file: String,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    Html,
    Rsc,
    /// A Pages Router page's props (`.json`).
    Data,
    /// A route handler's body.
    Body,
    /// An App Router segment (`.segment.rsc`), what the client's prefetches ask for.
    Segment,
    /// A PPR page's prefetch payload (`.prefetch.rsc`).
    Prefetch,
    /// Next's own record of the entry (`.meta`): status, headers, postponed state.
    Meta,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StaticFile {
    /// The path it's served at, with the base path.
    pub pathname: String,
    pub size: u64,
    /// Under `/_next/static/`: its name changes with its contents, so it's cached for good.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub immutable: bool,
    /// The router serves it, before the function.
    pub router: bool,
    /// The package holds it too, for Next's server: a file the middleware may match (the router
    /// then leaves it to Next, so the middleware runs), or an image Next's optimizer reads.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub server: bool,
}

// ---- Reading a build ----

/// A Next build's dist folder and what every reader of it needs.
struct Dist {
    /// The dist folder (`<project>/.next`).
    dir: PathBuf,
    /// Its path in the project (`.next`).
    name: String,
    /// `required-server-files.json`'s `config`.
    config: Value,
    /// The project's folder under the standalone folder (`apps/web`), or empty.
    app_dir: String,
    build_id: String,
}

impl Dist {
    fn json(&self, rel: &str) -> Result<Option<Value>> {
        let path = self.dir.join(rel);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).with_context(|| format!("{} isn't valid JSON", path.display()))?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// A path of the project (`.next/server/app/isr.html`) as the package has it.
    fn packaged(&self, project_rel: &str) -> String {
        join(&self.app_dir, project_rel)
    }
}

fn join(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_owned(),
        (_, true) => a.to_owned(),
        _ => format!("{}/{}", a.trim_end_matches('/'), b.trim_start_matches('/')),
    }
}

/// The dist folder of the build in `dir`: `.next`, or the folder `distDir` names, found by what a
/// build leaves in it.
pub fn find_dist(dir: &Path, named: &str) -> Option<PathBuf> {
    let is_dist = |p: &Path| p.join("required-server-files.json").is_file() && p.join("BUILD_ID").is_file();
    // The folder vercel.json's `outputDirectory` names (`.next` without one), then `.next`.
    for first in [named, DIST] {
        let at = dir.join(first);
        if is_dist(&at) {
            return Some(at);
        }
    }
    // `distDir: 'build'`, `distDir: 'dist/next'`: two levels are looked at.
    let mut level: Vec<PathBuf> = vec![dir.to_path_buf()];
    for _ in 0..2 {
        let mut next = Vec::new();
        for folder in &level {
            let Ok(entries) = std::fs::read_dir(folder) else { continue };
            let mut dirs: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .filter(|e| !matches!(e.file_name().to_str(), Some("node_modules" | ".git" | "standalone" | "public")))
                .map(|e| e.path())
                .collect();
            dirs.sort();
            if let Some(found) = dirs.iter().find(|d| is_dist(d)) {
                return Some(found.clone());
            }
            next.extend(dirs);
        }
        level = next;
    }
    None
}

/// The build's output is there: a standalone server, or a finished static export (deployed as a
/// static site, next_export.rs).
pub fn check(dir: &Path, named: &str) -> Result<()> {
    if let Some(dist) = find_dist(dir, named)
        && crate::next_export::config_of(&dist)?.is_some()
    {
        return crate::next_export::folder(dir, &dist).map(|_| ());
    }
    read_dist(dir, named).map(|_| ())
}

fn read_dist(dir: &Path, named: &str) -> Result<Dist> {
    let Some(dist) = find_dist(dir, named) else {
        bail!("the build left no {NAME} output in {} (no .next/BUILD_ID): the build command must run `next build`", dir.display());
    };
    let name = dist.strip_prefix(dir).unwrap_or(&dist).to_string_lossy().replace('\\', "/");
    let path = dist.join("required-server-files.json");
    let required: Value = serde_json::from_slice(&std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?).with_context(|| format!("{} isn't valid JSON", path.display()))?;
    let config = required.get("config").cloned().unwrap_or(Value::Null);
    // An export has no server to package: the build deploys its folder as a static site first.
    if config.get("output").and_then(Value::as_str) == Some("export") {
        bail!("this {NAME} app is a static export (`output: 'export'`): its exported folder is deployed as a static site, there's no server to package");
    }
    let standalone = dist.join("standalone");
    if !standalone.is_dir() {
        bail!(
            "the build left no {}: Next writes its standalone server there with `output: 'standalone'`, which forkrail asks for through NEXT_PRIVATE_STANDALONE and its adapter. A next.config that sets `output` to something else on Next before 15.5, or a build made without forkrail (`--prebuilt`), needs `output: 'standalone'`",
            standalone.display()
        );
    }
    // Next nests the project under its path from the tracing root (a workspace's top).
    let app_dir = match required.get("relativeAppDir").and_then(Value::as_str) {
        Some(rel) => rel.trim_matches('/').replace('\\', "/"),
        None => find_server(&standalone, 8).and_then(|d| d.strip_prefix(&standalone).ok().map(|p| p.to_string_lossy().replace('\\', "/"))).unwrap_or_default(),
    };
    if !standalone.join(&app_dir).join("server.js").is_file() {
        bail!("{} has no server.js: Next's standalone build didn't finish", standalone.join(&app_dir).display());
    }
    let build_id = std::fs::read_to_string(dist.join("BUILD_ID")).context("reading BUILD_ID")?.trim().to_owned();
    Ok(Dist { dir: dist, name, config, app_dir, build_id })
}

/// The shallowest folder with a `server.js` outside `node_modules`.
fn find_server(dir: &Path, depth: usize) -> Option<PathBuf> {
    if dir.join("server.js").is_file() {
        return Some(dir.to_path_buf());
    }
    if depth == 0 {
        return None;
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir).ok()?.filter_map(Result::ok).filter(|e| e.file_type().is_ok_and(|t| t.is_dir()) && e.file_name() != "node_modules").map(|e| e.path()).collect();
    dirs.sort();
    dirs.iter().find_map(|d| find_server(d, depth - 1))
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn strings(value: Option<&Value>) -> Option<Vec<String>> {
    Some(value?.as_array()?.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
}

/// Next's headers for an entry as one value each (a list joined as HTTP joins them).
fn headers_of(value: Option<&Value>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (key, value) in value.and_then(Value::as_object).into_iter().flatten() {
        let joined = match value {
            Value::String(s) => s.clone(),
            Value::Array(list) => list.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "),
            Value::Number(n) => n.to_string(),
            _ => continue,
        };
        out.insert(key.to_ascii_lowercase(), joined);
    }
    out
}

fn tags_of(headers: &BTreeMap<String, String>) -> Vec<String> {
    let mut tags: Vec<String> = headers.get("x-next-cache-tags").into_iter().flat_map(|v| v.split(',')).map(|t| t.trim().to_owned()).filter(|t| !t.is_empty()).collect();
    tags.sort();
    tags.dedup();
    tags
}

/// `initialRevalidate`: `false` never, a number of seconds.
fn seconds(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64)
}

/// The manifest of the build in `dist`, without its static files (`statics` adds them).
#[cfg(test)]
fn assemble(project: &Path, dist: &Dist, version: &str, vercel_json: Option<&Value>) -> Result<Manifest> {
    assemble_with(project, dist, version, vercel_json).map(|(manifest, _)| manifest)
}

/// `assemble`, and what the adapter wrote for direct handler calls (`Handlers`), if it did.
fn assemble_with(project: &Path, dist: &Dist, version: &str, vercel_json: Option<&Value>) -> Result<(Manifest, Option<Value>)> {
    let config = &dist.config;
    let adapter = match dist.json(ADAPTER_OUTPUT)? {
        // An earlier build's, left in a dist folder Next didn't clean, isn't this build's.
        Some(dump) if dump.get("buildId").and_then(Value::as_str) == Some(&dist.build_id) && dump.get("format").and_then(Value::as_u64) == Some(1) => Some(dump),
        _ => None,
    };
    let prerender_manifest = dist.json("prerender-manifest.json")?.unwrap_or(Value::Null);
    let preview = prerender_manifest.pointer("/preview/previewModeId").and_then(Value::as_str).unwrap_or_default().to_owned();
    let routes_manifest = dist.json("routes-manifest.json")?.unwrap_or(Value::Null);
    let mut routes = serde_json::Map::new();
    for key in ["redirects", "headers", "rewrites", "dynamicRoutes", "staticRoutes", "dataRoutes", "rsc", "caseSensitive", "pages404", "skipMiddlewareUrlNormalize", "skipProxyUrlNormalize"] {
        if let Some(value) = routes_manifest.get(key) {
            routes.insert(key.to_owned(), value.clone());
        }
    }
    let middleware_manifest = dist.json("server/middleware-manifest.json")?.unwrap_or(Value::Null);
    let functions_config = dist.json("server/functions-config-manifest.json")?.unwrap_or(Value::Null);
    let (middleware, mut functions, prerenders) = match &adapter {
        Some(dump) => {
            let base = text(config, "basePath").unwrap_or_default();
            (adapter_middleware(dump), adapter_functions(dump, &base), adapter_prerenders(dist, dump, &base))
        }
        None => (
            manifest_middleware(&middleware_manifest, &functions_config),
            manifest_functions(dist, &middleware_manifest, &functions_config)?,
            manifest_prerenders(dist, &prerender_manifest, &routes_manifest)?,
        ),
    };
    project_functions(project, vercel_json, &mut functions);
    // Each prerender's page: its route's, among the functions.
    let pages: BTreeMap<&str, &str> = functions.iter().map(|f| (f.pathname.as_str(), f.page.as_str())).collect();
    let mut prerenders = prerenders;
    for prerender in &mut prerenders {
        prerender.page = pages.get(prerender.route.as_str()).filter(|page| !page.is_empty()).map(|page| (*page).to_owned());
    }
    let images = dist.json("images-manifest.json")?.and_then(|m| m.get("images").cloned()).or_else(|| config.get("images").cloned());
    let experimental = config.get("experimental");
    let cache_components = config.get("cacheComponents").and_then(Value::as_bool).unwrap_or(false)
        || experimental.is_some_and(|e| e.get("cacheComponents").and_then(Value::as_bool).unwrap_or(false) || e.get("ppr").is_some_and(|p| p.as_bool() == Some(true) || p.as_str() == Some("incremental")));
    let handlers = adapter.as_ref().and_then(|dump| {
        let mut h = dump.get("handlers").filter(|h| h.get("outputs").is_some_and(Value::is_array))?.clone();
        let object = h.as_object_mut()?;
        // With it, all a second package needs is in one file.
        object.insert("format".to_owned(), Value::from(FORMAT));
        object.insert("nextVersion".to_owned(), Value::from(version));
        object.insert("buildId".to_owned(), Value::from(dist.build_id.clone()));
        // From the project's folder, as H's package and server take it: the adapter's own, which
        // differs from `dist.name` (from the build's folder) when the build runs above the project
        // (`next build apps/web`, Next's app-dir/upward-distdir test).
        let project_dist = dump.get("distDir").and_then(Value::as_str).filter(|d| !d.is_empty()).map(str::to_owned).unwrap_or_else(|| dist.name.clone());
        object.insert("distDir".to_owned(), Value::from(project_dist));
        for key in ["basePath", "trailingSlash", "i18n"] {
            object.insert(key.to_owned(), config.get(key).cloned().unwrap_or(Value::Null));
        }
        for key in ["routing", "prerenders"] {
            object.insert(key.to_owned(), dump.get(key).cloned().unwrap_or(Value::Null));
        }
        Some(h)
    });
    // The handler the config names, and whether it's forkrail's (13.5 to 14.2, `before_build`).
    let named = |v: Option<&Value>| v.and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
    let handler = named(config.get("cacheHandler")).or_else(|| named(experimental.and_then(|e| e.get("incrementalCacheHandlerPath"))));
    let ours = handler.as_deref().is_some_and(|h| h.ends_with(STUB_FILE));
    let own_handler = handler.is_some() && !ours;
    let shared_cache = !own_handler && (ours || Version::parse(version).is_some_and(|v| v >= HANDLERS_FOUND_FROM));
    let manifest = Manifest {
        format: FORMAT,
        next_version: version.to_owned(),
        source: if adapter.is_some() { From::Adapter } else { From::Manifests },
        build_id: dist.build_id.clone(),
        // 13.5 keeps it under `experimental`.
        deployment_id: text(config, "deploymentId").or_else(|| experimental.and_then(|e| e.get("deploymentId")).and_then(Value::as_str).map(str::to_owned)).filter(|d| !d.is_empty()),
        base_path: text(config, "basePath").unwrap_or_default(),
        asset_prefix: text(config, "assetPrefix").unwrap_or_default(),
        trailing_slash: config.get("trailingSlash").and_then(Value::as_bool).unwrap_or(false),
        i18n: config.get("i18n").filter(|v| v.is_object()).cloned(),
        app_dir: dist.app_dir.clone(),
        dist_dir: dist.name.clone(),
        entry: join(&dist.app_dir, ENTRY),
        preview: Preview { id: preview },
        middleware,
        routes: Value::Object(routes),
        adapter_routing: adapter.as_ref().and_then(|d| d.get("routing").cloned()),
        cache_components,
        cache_handler: own_handler,
        shared_cache,
        functions,
        prerenders,
        prerender_files: Kept::Package,
        handlers: None,
        images,
        static_files: Vec::new(),
    };
    Ok((manifest, handlers))
}

/// `next-handlers.json`: for Next 16.2 and later, everything a package that calls Next's
/// entrypoints directly (no Next server) is made from, taken from the same build as the
/// standalone server. forkrail's fields: `format`, `nextVersion`, `buildId`, `distDir`,
/// `basePath`, `trailingSlash`, `i18n`. Next's, as its adapter API hands them: `routing` whole;
/// `prerenders` (each with `parentOutputId`, `groupId`, `renderingMode`, `pprChain` and its
/// `fallback` with `postponedState`, the file's path from the project); `outputs`, one for every
/// entry in `pages`, `pagesApi`, `appPages` and `appRoutes`, and `middleware`: `kind`, `id`,
/// `pathname`, `sourcePage`, `runtime`, `file` (the entry, from the repository's root), `config`
/// (with an Edge entry's `env`, which holds the build's keys), `edgeRuntime`, `wasmAssets` and
/// `assets`, its traced files as places in `files`. `files` is each traced file once: its path
/// from the repository's root (`repoRoot`, as it was at build; `projectDir` is the project's path
/// under it), or `[path, source]` when Next places it under a name other than where it is.
pub struct Handlers(pub Value);

impl Handlers {
    /// The handlers file of the build folder `out`, if the build made one.
    pub fn read(out: &Path) -> Result<Option<Handlers>> {
        match std::fs::read(out.join(HANDLERS_FILE)) {
            Ok(bytes) => Ok(Some(Handlers(serde_json::from_slice(&bytes).with_context(|| format!("{HANDLERS_FILE} isn't valid JSON"))?))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {HANDLERS_FILE}")),
        }
    }

    /// Every file the outputs need, entries and traced files, each once: its path in a package
    /// laid out from the repository's root, and where it is under `repo_root` now.
    pub fn files(&self, repo_root: &Path) -> BTreeMap<String, PathBuf> {
        let mut out = BTreeMap::new();
        for file in self.0.get("files").and_then(Value::as_array).into_iter().flatten() {
            let (path, from) = match file {
                Value::String(path) => (path.as_str(), path.as_str()),
                Value::Array(pair) => match (pair.first().and_then(Value::as_str), pair.get(1).and_then(Value::as_str)) {
                    (Some(path), Some(from)) => (path, from),
                    _ => continue,
                },
                _ => continue,
            };
            out.insert(path.to_owned(), repo_root.join(from));
        }
        let outputs = self.0.get("outputs").and_then(Value::as_array).into_iter().flatten().chain(self.0.get("middleware").filter(|m| m.is_object()));
        for output in outputs {
            if let Some(file) = output.get("file").and_then(Value::as_str) {
                out.entry(file.to_owned()).or_insert_with(|| repo_root.join(file));
            }
            for file in output.get("wasmAssets").and_then(Value::as_object).into_iter().flat_map(|w| w.values()).filter_map(Value::as_str) {
                out.entry(file.to_owned()).or_insert_with(|| repo_root.join(file));
            }
        }
        out
    }
}

// -- from the adapter's outputs (Next 16.2 and later) --

fn adapter_middleware(dump: &Value) -> Option<Middleware> {
    let middleware = dump.get("middleware").filter(|m| m.is_object())?;
    let matchers = middleware
        .get("matchers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            Some(Matcher { source: text(m, "source"), regexp: text(m, "sourceRegex")?, has: m.get("has").filter(|v| !v.is_null()).cloned(), missing: m.get("missing").filter(|v| !v.is_null()).cloned() })
        })
        .collect();
    Some(Middleware { runtime: text(middleware, "runtime").unwrap_or_else(|| "nodejs".to_owned()), matchers })
}

/// A path Next hands an adapter, without the base path it puts in front (`/docs/blog` → `/blog`,
/// `/docs` → `/`): the manifest names routes as Next's own manifests do.
fn unbased(pathname: &str, base: &str) -> String {
    if base.is_empty() {
        return pathname.to_owned();
    }
    match pathname.strip_prefix(base) {
        Some("") => "/".to_owned(),
        Some(rest) if rest.starts_with('/') => rest.to_owned(),
        _ => pathname.to_owned(),
    }
}

fn adapter_functions(dump: &Value, base: &str) -> Vec<Function> {
    let mut out: Vec<Function> = Vec::new();
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for f in dump.get("functions").and_then(Value::as_array).into_iter().flatten() {
        let (Some(pathname), Some(kind)) = (text(f, "pathname").map(|p| unbased(&p, base)), f.get("kind").and_then(Value::as_str)) else { continue };
        let kind = match kind {
            "pages" => FunctionKind::Page,
            "pagesApi" => FunctionKind::Api,
            "appPages" => FunctionKind::AppPage,
            "appRoutes" => FunctionKind::AppRoute,
            _ => continue,
        };
        // An App Router entry's `.rsc` twin is the same entrypoint under a second name.
        let pathname = match kind {
            FunctionKind::AppPage | FunctionKind::AppRoute => rsc_base(&pathname),
            _ => pathname,
        };
        let function = Function {
            pathname: pathname.clone(),
            page: text(f, "sourcePage").unwrap_or_default(),
            kind,
            runtime: text(f, "runtime").unwrap_or_else(|| "nodejs".to_owned()),
            max_duration: f.get("maxDuration").and_then(Value::as_u64),
            memory: None,
            regions: f.get("preferredRegion").filter(|v| !v.is_null()).cloned(),
            id: text(f, "id"),
            edge_runtime: f.get("edgeRuntime").filter(|v| v.is_object()).cloned(),
        };
        match seen.get(&pathname) {
            // The twin says the same; what one of them leaves out is taken from the other.
            Some(&at) => {
                let first: &mut Function = &mut out[at];
                first.max_duration = first.max_duration.or(function.max_duration);
                first.regions = first.regions.take().or(function.regions);
                first.edge_runtime = first.edge_runtime.take().or(function.edge_runtime);
                // The output's own id, not its twin's.
                if first.id.as_deref().is_none_or(|id| id.ends_with(".rsc")) && function.id.is_some() {
                    first.id = function.id;
                }
            }
            None => {
                seen.insert(pathname, out.len());
                out.push(function);
            }
        }
    }
    out.sort_by(|a, b| a.pathname.cmp(&b.pathname));
    out
}

/// `/blog/first.rsc` → `/blog/first`, `/index.rsc` → `/`.
fn rsc_base(pathname: &str) -> String {
    match pathname.strip_suffix(".rsc") {
        Some("/index") => "/".to_owned(),
        Some(base) => base.to_owned(),
        None => pathname.to_owned(),
    }
}

/// The page a prerender output belongs to, by its path: `/blog/first.rsc`,
/// `/blog/first.segments/_tree.segment.rsc` and `/_next/data/<id>/blog/first.json` are
/// `/blog/first`'s; `/index.rsc` is `/`'s.
fn page_of(pathname: &str, build_id: &str) -> String {
    let data = format!("/_next/data/{build_id}/");
    let base = if let Some(at) = pathname.find(&data)
        && let Some(rest) = pathname[at + data.len()..].strip_suffix(".json")
    {
        format!("{}/{rest}", &pathname[..at])
    } else if let Some(at) = pathname.find(".segments/") {
        pathname[..at].to_owned()
    } else if let Some(base) = pathname.strip_suffix(".prefetch.rsc") {
        base.to_owned()
    } else if let Some(base) = pathname.strip_suffix(".rsc") {
        base.to_owned()
    } else {
        pathname.to_owned()
    };
    match base.strip_suffix("/index") {
        Some("") => "/".to_owned(),
        Some(parent) if pathname != base => parent.to_owned(),
        _ => base,
    }
}

fn file_kind(pathname: &str, file: &str) -> FileKind {
    if file.ends_with(".meta") {
        FileKind::Meta
    } else if pathname.contains(".segments/") || file.ends_with(".segment.rsc") {
        FileKind::Segment
    } else if file.ends_with(".prefetch.rsc") {
        FileKind::Prefetch
    } else if file.ends_with(".rsc") {
        FileKind::Rsc
    } else if file.ends_with(".json") {
        FileKind::Data
    } else if file.ends_with(".body") {
        FileKind::Body
    } else {
        FileKind::Html
    }
}

fn adapter_prerenders(dist: &Dist, dump: &Value, base: &str) -> Vec<Prerender> {
    let project = dist.dir.parent().map(Path::to_path_buf).unwrap_or_default();
    // The project's folder: the dist folder's path without its name in the project.
    let project = match dist.name.split('/').count() {
        0 | 1 => project,
        n => dist.dir.ancestors().nth(n).map(Path::to_path_buf).unwrap_or(project),
    };
    let list: Vec<&Value> = dump.get("prerenders").and_then(Value::as_array).into_iter().flatten().collect();
    let path_of = |p: &Value| text(p, "pathname").map(|path| unbased(&path, base));
    let file_of = |p: &Value| -> Option<PrerenderFile> {
        let pathname = path_of(p)?;
        let rel = p.pointer("/fallback/file").and_then(Value::as_str)?;
        let size = std::fs::metadata(project.join(rel)).map(|m| m.len()).unwrap_or(0);
        Some(PrerenderFile { kind: file_kind(&pathname, rel), pathname, file: dist.packaged(&rel.replace('\\', "/")), size })
    };
    let mut out: Vec<Prerender> = Vec::new();
    let mut by_path: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_group: BTreeMap<u64, usize> = BTreeMap::new();
    for p in list.iter().filter(|p| p.get("routeType").is_some_and(Value::is_string)) {
        let Some(pathname) = path_of(p) else { continue };
        let fallback = p.get("fallback").unwrap_or(&Value::Null);
        let headers = headers_of(fallback.get("initialHeaders"));
        let kind = match p.get("routeType").and_then(Value::as_str) {
            Some("route") => PrerenderKind::Route,
            Some("shell") => PrerenderKind::Shell,
            Some("fallback") => PrerenderKind::Fallback,
            _ => PrerenderKind::Page,
        };
        let group = p.get("groupId").and_then(Value::as_u64);
        let router = if p.pointer("/fallback/file").and_then(Value::as_str).is_some_and(|f| f.contains("/pages/") || f.contains("/PAGES/")) { Router::Pages } else { Router::App };
        by_path.entry(pathname.clone()).or_insert(out.len());
        if let Some(group) = group {
            by_group.entry(group).or_insert(out.len());
        }
        out.push(Prerender {
            route: text(p, "route").unwrap_or_else(|| pathname.clone()),
            pathname,
            page: None,
            router,
            kind,
            parent: text(p, "parentOutputId"),
            ppr_chain: p.get("pprChain").filter(|v| v.is_object()).cloned(),
            group,
            revalidate: seconds(fallback.get("initialRevalidate")),
            expire: seconds(fallback.get("initialExpiration")),
            status: fallback.get("initialStatus").and_then(Value::as_u64).and_then(|s| u16::try_from(s).ok()),
            tags: tags_of(&headers),
            headers,
            postponed: text(fallback, "postponedState"),
            allow_query: strings(p.get("allowQuery")),
            allow_header: strings(p.get("allowHeader")),
            bypass_for: p.get("bypassFor").filter(|v| !v.is_null()).cloned(),
            rendering_mode: text(p, "renderingMode"),
            files: {
                // Next's own record of the entry sits beside its page, though the adapter doesn't list it.
                let page = file_of(p);
                let meta = page.as_ref().and_then(|page| {
                    let rel = p.pointer("/fallback/file").and_then(Value::as_str)?;
                    let meta = format!("{}.meta", rel.rsplit_once('.')?.0);
                    let size = std::fs::metadata(project.join(&meta)).ok().filter(|m| m.is_file())?.len();
                    let name = if page.pathname == "/" { "/index" } else { page.pathname.as_str() };
                    Some(PrerenderFile { pathname: format!("{name}.meta"), kind: FileKind::Meta, file: dist.packaged(&meta.replace('\\', "/")), size })
                });
                page.into_iter().chain(meta).collect()
            },
        });
    }
    for p in list.iter().filter(|p| !p.get("routeType").is_some_and(Value::is_string)) {
        let (Some(pathname), Some(file)) = (path_of(p), file_of(p)) else { continue };
        let page = page_of(&pathname, &dist.build_id);
        let group = p.get("groupId").and_then(Value::as_u64);
        let at = by_path.get(&page).copied().or_else(|| group.and_then(|g| by_group.get(&g).copied()));
        match at {
            Some(at) => out[at].files.push(file),
            None => {
                // Files whose page the build didn't list: kept as their own entry.
                let fallback = p.get("fallback").unwrap_or(&Value::Null);
                let headers = headers_of(fallback.get("initialHeaders"));
                by_path.insert(page.clone(), out.len());
                out.push(Prerender {
                    route: text(p, "route").unwrap_or_else(|| page.clone()),
                    pathname: page,
                    page: None,
                    router: if file.kind == FileKind::Data { Router::Pages } else { Router::App },
                    kind: PrerenderKind::Data,
                    parent: text(p, "parentOutputId"),
                    ppr_chain: p.get("pprChain").filter(|v| v.is_object()).cloned(),
                    group,
                    revalidate: seconds(fallback.get("initialRevalidate")),
                    expire: seconds(fallback.get("initialExpiration")),
                    status: None,
                    tags: tags_of(&headers),
                    headers,
                    postponed: None,
                    allow_query: strings(p.get("allowQuery")),
                    allow_header: strings(p.get("allowHeader")),
                    bypass_for: None,
                    rendering_mode: text(p, "renderingMode"),
                    files: vec![file],
                });
            }
        }
    }
    for p in &mut out {
        p.files.sort_by(|a, b| a.pathname.cmp(&b.pathname));
    }
    out.sort_by(|a, b| a.pathname.cmp(&b.pathname));
    out
}

// -- from `.next`'s manifests (every version) --

fn matchers_of(list: Option<&Value>) -> Vec<Matcher> {
    list.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            Some(Matcher {
                source: text(m, "originalSource").or_else(|| text(m, "source")),
                regexp: text(m, "regexp").or_else(|| text(m, "sourceRegex"))?,
                has: m.get("has").filter(|v| !v.is_null()).cloned(),
                missing: m.get("missing").filter(|v| !v.is_null()).cloned(),
            })
        })
        .collect()
}

/// The middleware: an Edge one is in `middleware-manifest.json`, a Node one (`proxy.ts`, Node
/// middleware) in `functions-config-manifest.json` under `/_middleware`.
fn manifest_middleware(middleware_manifest: &Value, functions_config: &Value) -> Option<Middleware> {
    if let Some(edge) = middleware_manifest.get("middleware").and_then(Value::as_object).and_then(|m| m.get("/").or_else(|| m.values().next())) {
        return Some(Middleware { runtime: "edge".to_owned(), matchers: matchers_of(edge.get("matchers")) });
    }
    let node = functions_config.pointer("/functions/~1_middleware").filter(|v| v.is_object())?;
    Some(Middleware { runtime: text(node, "runtime").unwrap_or_else(|| "nodejs".to_owned()), matchers: matchers_of(node.get("matchers")) })
}

/// An App Router page's path: `/(shop)/blog/[slug]/page` → `/blog/[slug]`, `/api/hello/route` →
/// `/api/hello`, `/@modal/x/page` isn't a path of its own (None).
fn app_path(page: &str) -> Option<String> {
    let inner = page.strip_suffix("/page").or_else(|| page.strip_suffix("/route"))?;
    let mut out = String::new();
    for segment in inner.split('/').filter(|s| !s.is_empty()) {
        if segment.starts_with('@') {
            return None;
        }
        if segment.starts_with('(') && segment.ends_with(')') && !segment.starts_with("(.") {
            continue;
        }
        out.push('/');
        out.push_str(segment);
    }
    Some(if out.is_empty() { "/".to_owned() } else { out })
}

fn manifest_functions(dist: &Dist, middleware_manifest: &Value, functions_config: &Value) -> Result<Vec<Function>> {
    let edge: BTreeSet<String> = middleware_manifest.get("functions").and_then(Value::as_object).into_iter().flatten().filter_map(|(_, f)| text(f, "page")).collect();
    let settings = |pathname: &str, page: &str| functions_config.get("functions").and_then(|f| f.get(pathname).or_else(|| f.get(page))).cloned().unwrap_or(Value::Null);
    let mut out = Vec::new();
    for (page, file) in dist.json("server/pages-manifest.json")?.as_ref().and_then(Value::as_object).into_iter().flatten() {
        // Pages Next rendered to a file at build have no function; nor have its own shells.
        if matches!(page.as_str(), "/_app" | "/_document" | "/_error" | "/404" | "/500") || file.as_str().is_some_and(|f| f.ends_with(".html")) {
            continue;
        }
        let kind = if page == "/api" || page.starts_with("/api/") { FunctionKind::Api } else { FunctionKind::Page };
        let config = settings(page, page);
        out.push(Function {
            pathname: page.clone(),
            page: page.clone(),
            kind,
            runtime: if edge.contains(page) { "edge".to_owned() } else { text(&config, "runtime").unwrap_or_else(|| "nodejs".to_owned()) },
            max_duration: config.get("maxDuration").and_then(Value::as_u64),
            memory: None,
            regions: config.get("regions").or_else(|| config.get("preferredRegion")).filter(|v| !v.is_null()).cloned(),
            id: None,
            edge_runtime: None,
        });
    }
    for (page, _) in dist.json("server/app-paths-manifest.json")?.as_ref().and_then(Value::as_object).into_iter().flatten() {
        let Some(pathname) = app_path(page) else { continue };
        let kind = if page.ends_with("/route") { FunctionKind::AppRoute } else { FunctionKind::AppPage };
        let config = settings(&pathname, page);
        out.push(Function {
            pathname,
            page: page.clone(),
            kind,
            runtime: if edge.contains(page) { "edge".to_owned() } else { text(&config, "runtime").unwrap_or_else(|| "nodejs".to_owned()) },
            max_duration: config.get("maxDuration").and_then(Value::as_u64),
            memory: None,
            regions: config.get("regions").or_else(|| config.get("preferredRegion")).filter(|v| !v.is_null()).cloned(),
            id: None,
            edge_runtime: None,
        });
    }
    out.sort_by(|a, b| a.pathname.cmp(&b.pathname).then(a.page.cmp(&b.page)));
    out.dedup_by(|a, b| a.pathname == b.pathname);
    Ok(out)
}

/// A route's parameters by their plain names, from `routes-manifest.json`'s `routeKeys`
/// (`nxtPslug` → `slug`).
fn route_params(routes_manifest: &Value, route: &str) -> Option<Vec<String>> {
    let found = routes_manifest.get("dynamicRoutes").and_then(Value::as_array)?.iter().find(|r| r.get("page").and_then(Value::as_str) == Some(route))?;
    let mut names: Vec<String> = found.get("routeKeys").and_then(Value::as_object)?.values().filter_map(Value::as_str).map(|k| k.strip_prefix("nxtP").or_else(|| k.strip_prefix("nxtI")).unwrap_or(k).to_owned()).collect();
    names.sort();
    Some(names)
}

/// Where a build keeps its prerendered files. Without an adapter they sit beside the pages
/// (`server/app/<path>.html`, `server/pages/<path>.json`); with one (Next 15.5 and later) each
/// route has a folder of its own, `server/route-cache/<KIND>/<hash>/$/<path>.html`.
struct Layout {
    /// `<KIND>/<path with its extension>` → the file, relative to the dist folder.
    scoped: BTreeMap<String, String>,
}

impl Layout {
    fn read(dist: &Dist) -> Result<Layout> {
        let mut scoped = BTreeMap::new();
        for (rel, _) in scan(&dist.dir.join("server/route-cache"))? {
            // `PAGES/<hash>/$/posts/1.html`
            let Some((owner, path)) = rel.split_once("/$/") else { continue };
            let Some((kind, _hash)) = owner.split_once('/') else { continue };
            scoped.entry(format!("{kind}/{path}")).or_insert(format!("server/route-cache/{rel}"));
        }
        Ok(Layout { scoped })
    }

    /// The file of `name` (`/posts/1.html`, `/index.rsc`) of a page of `router`, relative to the
    /// dist folder, if the build left one.
    fn find(&self, dist: &Dist, router: Router, name: &str) -> Option<String> {
        let (folder, kinds): (&str, &[&str]) = match router {
            Router::Pages => ("server/pages", &["PAGES"]),
            Router::App => ("server/app", &["APP_PAGE", "APP_ROUTE"]),
        };
        let classic = format!("{folder}{name}");
        if dist.dir.join(&classic).is_file() {
            return Some(classic);
        }
        kinds.iter().find_map(|kind| self.scoped.get(&format!("{kind}{name}")).cloned())
    }

    /// The files of a page's `.segments` folder, as (path under it, file relative to the dist folder).
    fn segments(&self, dist: &Dist, name: &str) -> Result<Vec<(String, String)>> {
        let classic = format!("server/app{name}.segments");
        if dist.dir.join(&classic).is_dir() {
            return Ok(scan(&dist.dir.join(&classic))?.into_iter().map(|(rel, _)| (rel.clone(), format!("{classic}/{rel}"))).collect());
        }
        let prefix = format!("APP_PAGE{name}.segments/");
        Ok(self.scoped.range(prefix.clone()..).take_while(|(key, _)| key.starts_with(&prefix)).map(|(key, file)| (key[prefix.len()..].to_owned(), file.clone())).collect())
    }
}

/// The prerenders as `prerender-manifest.json` lists them, with the files Next left for each
/// (`<path>.html|.rsc|.meta|.body|.prefetch.rsc|.segments/` of an App Router page,
/// `<path>.html|.json` of a Pages Router one), wherever the build keeps them (`Layout`).
fn manifest_prerenders(dist: &Dist, prerender_manifest: &Value, routes_manifest: &Value) -> Result<Vec<Prerender>> {
    let mut out = Vec::new();
    let layout = Layout::read(dist)?;
    let found = |rel: &str, pathname: &str, kind: FileKind| -> Option<PrerenderFile> {
        let size = std::fs::metadata(dist.dir.join(rel)).ok().filter(|m| m.is_file())?.len();
        Some(PrerenderFile { pathname: pathname.to_owned(), kind, file: dist.packaged(&join(&dist.name, rel)), size })
    };
    let file = |router: Router, name: &str, pathname: &str, kind: FileKind| -> Option<PrerenderFile> { found(&layout.find(dist, router, name)?, pathname, kind) };
    for (path, info) in prerender_manifest.get("routes").and_then(Value::as_object).into_iter().flatten() {
        let name = if path == "/" { "/index".to_owned() } else { path.clone() };
        let data_route = info.get("dataRoute").and_then(Value::as_str);
        let router = if data_route.is_some_and(|d| d.ends_with(".json")) { Router::Pages } else { Router::App };
        let mut files = Vec::new();
        let mut headers = headers_of(info.get("initialHeaders"));
        let mut status = info.get("initialStatus").and_then(Value::as_u64).and_then(|s| u16::try_from(s).ok());
        let mut postponed = None;
        let mut kind = PrerenderKind::Page;
        match router {
            Router::Pages => {
                files.extend(file(router, &format!("{name}.html"), path, FileKind::Html));
                if let Some(data) = data_route {
                    files.extend(file(router, &format!("{name}.json"), data, FileKind::Data));
                }
            }
            Router::App => {
                files.extend(file(router, &format!("{name}.html"), path, FileKind::Html));
                if let Some(body) = file(router, &format!("{name}.body"), path, FileKind::Body) {
                    kind = PrerenderKind::Route;
                    files.push(body);
                }
                if let Some(data) = data_route {
                    files.extend(file(router, &format!("{name}.rsc"), data, FileKind::Rsc));
                }
                if let Some(prefetch) = info.get("prefetchDataRoute").and_then(Value::as_str) {
                    files.extend(file(router, &format!("{name}.prefetch.rsc"), prefetch, FileKind::Prefetch));
                }
                for (rel, at) in layout.segments(dist, &name)? {
                    files.extend(found(&at, &format!("{name}.segments/{rel}"), FileKind::Segment));
                }
            }
        }
        // Next's own record of the entry says what the manifest doesn't (cache tags, before 15).
        if let Some(at) = layout.find(dist, router, &format!("{name}.meta")) {
            files.extend(found(&at, &format!("{name}.meta"), FileKind::Meta));
            if let Ok(Some(meta)) = dist.json(&at) {
                for (key, value) in headers_of(meta.get("headers")) {
                    headers.entry(key).or_insert(value);
                }
                status = status.or_else(|| meta.get("status").and_then(Value::as_u64).and_then(|s| u16::try_from(s).ok()));
                postponed = text(&meta, "postponed");
            }
        }
        let route = text(info, "srcRoute").unwrap_or_else(|| path.clone());
        let rendering_mode = text(info, "renderingMode").or_else(|| info.get("experimentalPPR").and_then(Value::as_bool).filter(|p| *p).map(|_| "PARTIALLY_STATIC".to_owned()));
        if rendering_mode.as_deref() == Some("PARTIALLY_STATIC") && postponed.is_some() {
            kind = PrerenderKind::Shell;
        }
        out.push(Prerender {
            pathname: path.clone(),
            allow_query: Some(if route == *path { Vec::new() } else { route_params(routes_manifest, &route).unwrap_or_default() }),
            route,
            page: None,
            router,
            kind,
            parent: None,
            ppr_chain: None,
            group: None,
            revalidate: seconds(info.get("initialRevalidateSeconds")),
            expire: seconds(info.get("initialExpireSeconds")),
            status,
            tags: tags_of(&headers),
            headers,
            postponed,
            allow_header: strings(info.get("allowHeader")),
            bypass_for: info.get("experimentalBypassFor").filter(|v| !v.is_null()).cloned(),
            rendering_mode,
            files,
        });
    }
    // A dynamic route's fallback page (`fallback: true` in getStaticPaths, a PPR fallback shell).
    for (route, info) in prerender_manifest.get("dynamicRoutes").and_then(Value::as_object).into_iter().flatten() {
        let Some(fallback) = info.get("fallback").and_then(Value::as_str) else { continue };
        let router = if info.get("dataRoute").and_then(Value::as_str).is_some_and(|d| d.ends_with(".json")) { Router::Pages } else { Router::App };
        let files: Vec<PrerenderFile> = file(router, fallback, route, FileKind::Html).into_iter().collect();
        if files.is_empty() {
            continue;
        }
        let headers = headers_of(info.get("fallbackHeaders"));
        out.push(Prerender {
            pathname: route.clone(),
            route: route.clone(),
            page: None,
            router,
            kind: PrerenderKind::Fallback,
            parent: None,
            ppr_chain: None,
            group: None,
            revalidate: seconds(info.get("fallbackRevalidate")),
            expire: seconds(info.get("fallbackExpire")),
            status: info.get("fallbackStatus").and_then(Value::as_u64).and_then(|s| u16::try_from(s).ok()),
            tags: tags_of(&headers),
            headers,
            postponed: None,
            allow_query: route_params(routes_manifest, route),
            allow_header: strings(info.get("allowHeader")),
            bypass_for: info.get("experimentalBypassFor").filter(|v| !v.is_null()).cloned(),
            rendering_mode: text(info, "renderingMode"),
            files,
        });
    }
    out.sort_by(|a, b| a.pathname.cmp(&b.pathname).then(a.kind.cmp(&b.kind)));
    Ok(out)
}

/// `vercel.json`'s `functions` for a Next project: each pattern is a glob over the project's
/// source files (`app/api/**/*`, `pages/api/slow.ts`), the first that matches a route's file
/// giving its `memory` and, where the route set none itself, its `maxDuration`.
fn project_functions(project: &Path, vercel_json: Option<&Value>, functions: &mut [Function]) {
    let Some(patterns) = vercel_json.and_then(|c| c.get("functions")).and_then(Value::as_object) else { return };
    const EXTENSIONS: [&str; 6] = ["ts", "tsx", "js", "jsx", "mjs", "mts"];
    for function in functions {
        let folder = match function.kind {
            FunctionKind::Page | FunctionKind::Api => "pages",
            FunctionKind::AppPage | FunctionKind::AppRoute => "app",
        };
        let page = if function.page == "/" && folder == "pages" { "/index" } else { function.page.as_str() };
        let candidates: Vec<String> = ["", "src/"]
            .iter()
            .flat_map(|src| EXTENSIONS.iter().flat_map(move |ext| [format!("{src}{folder}{page}.{ext}"), format!("{src}{folder}{page}/index.{ext}")]))
            .filter(|rel| project.join(rel).is_file())
            .collect();
        let Some((_, settings)) = patterns.iter().find(|(pattern, _)| candidates.iter().any(|file| crate::functions::glob_match(pattern, file))) else { continue };
        function.memory = settings.get("memory").and_then(Value::as_u64).and_then(|m| u32::try_from(m).ok());
        if function.max_duration.is_none() {
            function.max_duration = settings.get("maxDuration").and_then(Value::as_u64);
        }
    }
}

// ---- Files ----

/// Every file under `dir`, through links, as (path relative to it, the file), sorted.
pub(crate) fn scan(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    for entry in ignore::WalkBuilder::new(dir).standard_filters(false).follow_links(true).sort_by_file_name(|a, b| a.cmp(b)).build() {
        let entry = entry.with_context(|| format!("listing {}", dir.display()))?;
        if !entry.path().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(dir).expect("walked from dir").to_string_lossy().replace('\\', "/");
        if rel.split('/').any(|part| part == ".DS_Store") {
            continue;
        }
        out.push((rel, entry.path().to_path_buf()));
    }
    Ok(out)
}

/// A path without its `.` and `..` parts, as the file system would resolve it were no part of it
/// a link.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// The relative path from folder `from` to `to` (both absolute, normalized).
fn relative(from: &Path, to: &Path) -> String {
    let (a, b): (Vec<Component>, Vec<Component>) = (from.components().collect(), to.components().collect());
    let shared = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut parts: Vec<String> = std::iter::repeat_n("..".to_owned(), a.len() - shared).collect();
    parts.extend(b[shared..].iter().map(|c| c.as_os_str().to_string_lossy().into_owned()));
    if parts.is_empty() { ".".to_owned() } else { parts.join("/") }
}

/// The standalone folder `root` as a package's entries: every real file once with its own time,
/// and every link as a link when what it points at is inside the folder (pnpm's `node_modules`,
/// Turbopack's externals), so each real folder is packaged once and Node resolves packages as it
/// did in the build. A link that leaves the folder is replaced by what it points at.
fn walk(root: &Path) -> Result<BTreeMap<String, Entry>> {
    let root = root.canonicalize().with_context(|| format!("reading {}", root.display()))?;
    let mut out = BTreeMap::new();
    let mut chain = vec![root.clone()];
    visit(&root, &root, "", &mut chain, &mut out)?;
    Ok(out)
}

fn visit(root: &Path, dir: &Path, prefix: &str, chain: &mut Vec<PathBuf>, out: &mut BTreeMap<String, Entry>) -> Result<()> {
    let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?.collect::<std::io::Result<_>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".DS_Store" {
            continue;
        }
        let rel = join(prefix, &name);
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path).with_context(|| format!("reading {}", path.display()))?;
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&path).with_context(|| format!("reading the link {}", path.display()))?;
            // A link written with a whole path is followed to where it really is (the folder may
            // itself be reached through a link, as macOS's temporary folders are).
            let resolved = if target.is_absolute() { std::fs::canonicalize(&target).unwrap_or_else(|_| normalize(&target)) } else { normalize(&dir.join(&target)) };
            if resolved.starts_with(root) {
                // Where the link is in the package: not `dir`, inside a folder that was itself
                // brought in from outside.
                let here = root.join(prefix);
                let target = if target.is_absolute() || here != dir { relative(&here, &resolved) } else { target.to_string_lossy().replace('\\', "/") };
                out.insert(rel, Entry { source: Source::Link(target), modified: None });
                continue;
            }
            // It leaves the folder: what it points at goes in its place.
            match std::fs::metadata(&path) {
                Ok(m) if m.is_file() => {
                    out.insert(rel, Entry { source: Source::File(path.clone()), modified: m.modified().ok() });
                }
                Ok(m) if m.is_dir() => {
                    let real = path.canonicalize().with_context(|| format!("reading {}", path.display()))?;
                    if !chain.contains(&real) {
                        chain.push(real.clone());
                        visit(root, &real, &rel, chain, out)?;
                        chain.pop();
                    }
                }
                _ => {}
            }
        } else if meta.is_dir() {
            visit(root, &path, &rel, chain, out)?;
        } else if meta.is_file() {
            out.insert(rel, Entry { source: Source::File(path), modified: meta.modified().ok() });
        }
    }
    Ok(())
}

/// The middleware's matchers, compiled: whether a path may be one the middleware runs for.
struct Matchers(Vec<Option<fancy_regex::Regex>>);

impl Matchers {
    fn of(middleware: Option<&Middleware>) -> Self {
        // A pattern this engine can't read is taken to match everything.
        Matchers(middleware.map(|m| m.matchers.iter().map(|m| fancy_regex::Regex::new(&m.regexp).ok()).collect()).unwrap_or_default())
    }

    /// Whether the middleware may run for `path`. `has` and `missing` conditions depend on the
    /// request, so a matcher with them counts by its path alone.
    fn may_match(&self, path: &str) -> bool {
        self.0.iter().any(|m| match m {
            Some(regex) => regex.is_match(path).unwrap_or(true),
            None => true,
        })
    }
}

const IMAGE_EXTENSIONS: [&str; 11] = ["png", "jpg", "jpeg", "gif", "webp", "avif", "svg", "ico", "bmp", "tif", "tiff"];

fn is_image(rel: &str) -> bool {
    rel.rsplit_once('.').is_some_and(|(_, ext)| IMAGE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// Next's standalone `server.js` with the adapter's path taken out of the config it carries
/// (`const nextConfig = {...}`, the one its server runs with), or None when there's none to take.
/// With an adapter named (16.0 and 16.1 `experimental.adapterPath`, from 16.2 `adapterPath`), a
/// page whose params are closed (`dynamicParams: false`, `fallback: false`) answers 404 for a path
/// it doesn't list, leaving a less specific route to the platform's routing (H's); without one it
/// tells Next's own router to try the next route, as under `next start` and on Vercel. S runs
/// Next's own router. The path is the build machine's and means nothing at run time.
fn server_config(server_js: &str) -> Option<String> {
    const AT: &str = "\nconst nextConfig = ";
    let start = server_js.find(AT)? + AT.len();
    let end = start + server_js[start..].find('\n')?;
    let mut config: Value = serde_json::from_str(&server_js[start..end]).ok()?;
    let mut taken = config.as_object_mut()?.remove("adapterPath").is_some();
    if let Some(experimental) = config.get_mut("experimental").and_then(Value::as_object_mut) {
        taken |= experimental.remove("adapterPath").is_some();
    }
    taken.then(|| format!("{}{}{}", &server_js[..start], config, &server_js[end..]))
}

/// Whether Next's own optimizer answers `/_next/image` (the default loader, not `unoptimized`):
/// it reads a local image by asking its own server for it, so the package must hold the images.
fn optimizes_images(images: Option<&Value>) -> bool {
    let Some(images) = images else { return true };
    images.get("loader").and_then(Value::as_str).unwrap_or("default") == "default" && images.get("unoptimized").and_then(Value::as_bool) != Some(true)
}

/// Put `from` at `to`, as a link to it where the file system allows, unless something is there
/// already (the first file at a path stays). Never a copy onto a link of the file itself, which
/// would empty it: several prerendered paths may share one file (a 404 page).
pub(crate) fn link_or_copy(from: &Path, to: &Path) -> Result<()> {
    if to.symlink_metadata().is_ok() {
        return Ok(());
    }
    std::fs::create_dir_all(to.parent().expect("files have a folder"))?;
    if std::fs::hard_link(from, to).is_err() {
        std::fs::copy(from, to).with_context(|| format!("copying {}", from.display()))?;
    }
    Ok(())
}

/// What a Next build gives the deployment.
pub struct Built {
    pub manifest: Manifest,
    /// The package's entries, by their path in `app/`.
    pub entries: BTreeMap<String, Entry>,
    /// The folder the router's static files are read from, laid out by the paths they're served at.
    pub static_dir: PathBuf,
    /// The path prefixes whose files are cached for good.
    pub hashed: Vec<String>,
    /// Prefixes inside those whose files are revalidated instead (service workers).
    pub revalidated: Vec<String>,
    /// The most memory and the longest maxDuration a route asks for.
    pub floor: Option<crate::state::Floor>,
    pub outline: crate::state::Outline,
    /// The hash of what the package holds, and how many files.
    pub source_sha256: String,
    pub source_files: usize,
    /// The shared cache's seed, in the build folder's `next-seed/`, when one was made.
    pub seed: Option<crate::nextseed::Seeded>,
    _scratch: TempDir,
}

/// Take the Next build in `dir`: write its manifest and the shared cache's seed (for deployment
/// `deployment`) into the build folder `out`, and lay out its package and static files.
/// `vercel_json` is the project's configuration; `path` the build's `PATH`, where its `node` is.
pub fn collect(dir: &Path, vercel_json: Option<&Value>, out: &Path, deployment: &str, path: Option<&str>, log: &dyn BuildLog) -> Result<Built> {
    let package = match std::env::var(PRERENDERS_ENV).as_deref() {
        Ok("package") => true,
        Ok("seed") | Ok("") | Err(_) => false,
        Ok(other) => bail!("{PRERENDERS_ENV} must be `package` or `seed`, not {other:?}"),
    };
    let node = find_node(path);
    let seeder = |app: &Path, dist: &str, into: &Path| {
        let tools = TempDir::new("next-seed")?;
        crate::nextseed::make(&node, tools.path(), app, dist, into, deployment)
    };
    collect_with(dir, vercel_json, out, package, &fetch_sharp, &seeder, log)
}

/// The path pages ask for the hashed files under, from `assetPrefix`: the prefix itself when it's
/// a path (`/cdn`), the URL's path when it's a URL (`https://cdn.example.com/x` asks for
/// `/x/_next/static/…`, which Next's own server answers on the deployment's host too, taking the
/// path off), none when there's no path. The files are published there besides `/_next/static/`.
fn asset_path(prefix: &str) -> Option<String> {
    let prefix = prefix.trim();
    let path = match prefix.split_once("//") {
        Some((scheme, rest)) if scheme.is_empty() || scheme.ends_with(':') => rest.find('/').map_or("", |at| &rest[at..]),
        _ => prefix,
    };
    let path = path.split(['?', '#']).next().unwrap_or_default().trim_end_matches('/');
    (path.starts_with('/') && path.len() > 1).then(|| path.to_owned())
}

/// `node` in `path` (the build's `PATH`), else whatever `node` the CLI's own finds.
fn find_node(path: Option<&str>) -> PathBuf {
    let path = path.map(str::to_owned).or_else(|| std::env::var("PATH").ok()).unwrap_or_default();
    std::env::split_paths(&path).map(|dir| dir.join("node")).find(|node| node.is_file()).unwrap_or_else(|| PathBuf::from("node"))
}

/// What fetches one of sharp's packages for Linux arm64: its name and version to its folder.
type FetchSharp<'a> = &'a dyn Fn(&str, &str, &dyn BuildLog) -> Result<PathBuf>;
/// What makes the shared cache's seed: from the folder of Next's `server.js` and its dist folder's
/// name, into a folder (`nextseed::make`).
type Seeder<'a> = &'a dyn Fn(&Path, &str, &Path) -> Result<crate::nextseed::Seeded>;

/// `collect`, told whether the prerendered files stay in the package once seeded, how to fetch
/// sharp and how to seed.
fn collect_with(dir: &Path, vercel_json: Option<&Value>, out: &Path, keep_in_package: bool, fetch: FetchSharp, seeder: Seeder, log: &dyn BuildLog) -> Result<Built> {
    let named = vercel_json.and_then(|v| v.get("outputDirectory")).and_then(Value::as_str).map(crate::framework::clean_output).transpose()?;
    let dist = read_dist(dir, named.as_deref().unwrap_or(DIST))?;
    let version = installed(dir).map(|(text, _)| text).unwrap_or_else(|_| "unknown".to_owned());
    let (mut manifest, handlers) = assemble_with(dir, &dist, &version, vercel_json)?;
    let scratch = TempDir::new("next-out")?;
    let standalone = dist.dir.join("standalone");
    let mut entries = walk(&standalone)?;
    // Next's client-reference manifests, each repeated chunk list written once (rsc_manifests.rs):
    // the rewrites are packaged in place of the build's own files, which stay as they are (the
    // seed below reads the standalone folder).
    let compacted = crate::rsc_manifests::compact_into(&standalone, &scratch.path().join("manifests"))?;
    if let Some(line) = compacted.describe() {
        log.info(&line);
    }
    for entry in entries.values_mut() {
        if let Source::File(path) = &mut entry.source
            && let Some(rewrite) = compacted.replaced.get(path)
        {
            *path = rewrite.clone();
        }
    }

    // Next's own server, as S runs it, without the adapter's path in its config (`server_config`).
    let server_js = join(&dist.app_dir, "server.js");
    if let Some(Entry { source: Source::File(path), .. }) = entries.get(&server_js)
        && let Some(text) = server_config(&std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?)
    {
        let rewritten = scratch.path().join("server.js");
        std::fs::write(&rewritten, text)?;
        entries.insert(server_js, Entry::file(&rewritten));
    }

    // Env files Next copied for its server: forkrail never packages them (docs/deploying.md).
    let left_out: Vec<String> = entries.keys().filter(|name| name.strip_prefix(&join(&dist.app_dir, ".env")).is_some_and(|rest| !rest.contains('/'))).cloned().collect();
    for name in &left_out {
        entries.remove(name);
    }
    if !left_out.is_empty() {
        let names: Vec<&str> = left_out.iter().map(|n| n.rsplit('/').next().unwrap_or(n)).collect();
        log.info(&format!(
            "warning: {} isn't packaged (forkrail never packages .env files): the build read it, but the running app won't; set what the app reads at run time with `forkrail env add`",
            names.join(", ")
        ));
    }

    // Static files: `.next/static` and `public/`, to the router unless the middleware may match.
    let base = manifest.base_path.trim_end_matches('/').to_owned();
    let prefix = asset_path(&manifest.asset_prefix).filter(|p| *p != base);
    let matchers = Matchers::of(manifest.middleware.as_ref());
    let optimizer = optimizes_images(manifest.images.as_ref());
    let static_dir = scratch.path().join("static");
    std::fs::create_dir_all(&static_dir)?;
    let mut hashed = vec![format!("{base}/_next/static/")];
    hashed.extend(prefix.iter().map(|p| format!("{p}/_next/static/")));
    // A service worker keeps its name across builds (`/_next/static/service-worker/sw.js`, so the
    // browser finds the new one): revalidated on every use, as Next's own server serves it.
    let revalidated: Vec<String> = hashed.iter().map(|h| format!("{h}{SERVICE_WORKERS}")).collect();
    let mut kept_by_server = 0u64;
    let mut router_files = String::new();
    let groups = [(scan(&dist.dir.join("static"))?, true), (scan(&dir.join("public"))?, false)];
    for (files, under_static) in groups {
        for (rel, abs) in files {
            let (served, packaged) = if under_static { (format!("/_next/static/{rel}"), join(&dist.name, &format!("static/{rel}"))) } else { (format!("/{rel}"), format!("public/{rel}")) };
            let immutable = under_static && !rel.starts_with(SERVICE_WORKERS);
            let pathname = format!("{base}{served}");
            let meta = std::fs::metadata(&abs)?;
            let middleware = matchers.may_match(&pathname) || (!base.is_empty() && matchers.may_match(&served));
            let server = middleware || (optimizer && is_image(&rel));
            if server {
                kept_by_server += meta.len();
                entries.insert(dist.packaged(&packaged), Entry { source: Source::File(abs.clone()), modified: meta.modified().ok() });
            }
            if !middleware {
                link_or_copy(&abs, &static_dir.join(pathname.trim_start_matches('/')))?;
                if under_static && let Some(prefix) = &prefix {
                    link_or_copy(&abs, &static_dir.join(format!("{prefix}{served}").trim_start_matches('/')))?;
                }
            }
            if !server {
                router_files.push_str(&pathname);
                router_files.push('\n');
            }
            manifest.static_files.push(StaticFile { pathname, size: meta.len(), immutable, router: !middleware, server });
        }
    }
    // The router answers `/_next/image` itself (its image function, as Vercel's platform does),
    // unless the app's middleware runs on it: then Next's own optimizer does, which before Next 15
    // needs the project's own sharp in a standalone server (15 and later bring it themselves).
    let image_path = format!("{base}/_next/image");
    if optimizer
        && matchers.may_match(&image_path)
        && Version::parse(&manifest.next_version).is_some_and(|v| v < Version(15, 0, 0))
        && manifest.static_files.iter().any(|f| is_image(&f.pathname))
        && !entries.keys().any(|name| name.ends_with("node_modules/sharp/package.json"))
    {
        log.info(&format!(
            "warning: the app's middleware runs on {image_path}, so Next's own optimizer answers it, which in {NAME} {}'s standalone server works only when the project installs `sharp` (nextjs.org/docs/messages/sharp-missing-in-production); this one doesn't, so the images are answered as they are. Add `sharp` to the project's dependencies, or leave {image_path} out of the middleware's matcher",
            manifest.next_version
        ));
    }
    let to_server = manifest.static_files.iter().filter(|f| !f.router).count();
    if to_server > 0 {
        log.info(&format!(
            "{to_server} of {} static files are left to Next's server, since the app's middleware matches their paths (it runs for them as under `next start`); a matcher that leaves out _next/static and files lets the router answer them without a worker",
            manifest.static_files.len()
        ));
    }

    // Prerendered files: the shared cache's seed is made from them, read through Next's own cache
    // in the standalone folder as the build left it (file times included: they're the pages'
    // ages); then they leave the package, which Next's handlers never read them from.
    let mut prerender_bytes = 0u64;
    // Each file once: paths that aren't found (`notFound: true`) all name the 404 page's.
    let mut prerendered: BTreeSet<String> = BTreeSet::new();
    let mut leaving: BTreeSet<String> = BTreeSet::new();
    let mut staying: BTreeSet<String> = BTreeSet::new();
    for prerender in &manifest.prerenders {
        for file in &prerender.files {
            if !matches!(entries.get(&file.file), Some(Entry { source: Source::File(_), .. })) {
                continue;
            }
            if prerendered.insert(file.file.clone()) {
                prerender_bytes += file.size;
            }
            // A fallback or a shell is read from the package's files, not from the cache.
            if matches!(prerender.kind, PrerenderKind::Page | PrerenderKind::Route | PrerenderKind::Data) {
                leaving.insert(file.file.clone());
            } else {
                staying.insert(file.file.clone());
            }
        }
    }
    // A page Next rendered to a file at build (`pages/404.html` of a 404 page without
    // getStaticProps) is read by Next's server itself whenever it renders that page: it stays,
    // though the paths that aren't found (`notFound: true`) name it as their file too.
    for (_, file) in dist.json("server/pages-manifest.json")?.as_ref().and_then(Value::as_object).into_iter().flatten() {
        if let Some(file) = file.as_str().filter(|f| f.ends_with(".html")) {
            staying.insert(dist.packaged(&join(&dist.name, &format!("server/{file}"))));
        }
    }
    let seed = if !manifest.shared_cache {
        if manifest.cache_handler {
            log.info(&format!("the project names its own cache handler: {NAME} keeps its pages and data through it, not through forkrail's shared cache"));
        } else {
            log.info(&format!(
                "warning: the build's config didn't take forkrail's cache handler, so {NAME}'s caches stay in each worker, as under `next start` on one machine (no shared cache)"
            ));
        }
        None
    } else {
        let into = out.join(SEED_DIR);
        let started = std::time::Instant::now();
        match seeder(&standalone.join(&dist.app_dir), &dist.name, &into) {
            Ok(seeded) => {
                log.info(&format!(
                    "shared cache: seeded with {} pages ({:.1} MB){} in {:.1} s",
                    seeded.entries,
                    seeded.bytes as f64 / 1e6,
                    if seeded.skipped > 0 { format!(", {} left to their first request", seeded.skipped) } else { String::new() },
                    started.elapsed().as_secs_f64()
                ));
                Some(seeded)
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&into);
                log.info(&format!("warning: the shared cache couldn't be seeded ({e:#}): the prerendered pages stay in the package, and each worker reads them there until the cache has them"));
                None
            }
        }
    };
    let leave = seed.is_some() && !keep_in_package;
    if leave {
        // A file a fallback or a shell also names stays.
        for file in leaving.difference(&staying) {
            entries.remove(file);
        }
    }
    let prerender_files = prerendered.len();
    manifest.prerender_files = if leave { Kept::Seed } else { Kept::Package };

    fix_sharp(&mut entries, fetch, log);

    // The entry, beside Next's own `server.js` (which listens on PORT): forkrail's part of running
    // it (the runtime's next-standalone.cjs), then the server as the build wrote it.
    let entry = scratch.path().join(ENTRY);
    std::fs::write(&entry, SERVER_ENTRY_JS)?;
    let helper = scratch.path().join(HELPER);
    std::fs::write(&helper, HELPER_JS)?;
    entries.insert(manifest.entry.clone(), Entry::file(&entry));
    entries.insert(join(&dist.app_dir, HELPER), Entry::file(&helper));
    if !router_files.is_empty() {
        let listed = scratch.path().join(ROUTER_FILES);
        std::fs::write(&listed, &router_files)?;
        entries.insert(join(&dist.app_dir, ROUTER_FILES), Entry::file(&listed));
    }

    // Nothing is written for a package Lambda wouldn't take.
    check_size(&entries, &dist, prerender_bytes, leave)?;

    let functions = manifest.functions.len();
    log.info(&format!(
        "{NAME} {}: {functions} routes, {} prerendered paths ({prerender_files} files, {:.1} MB{}), middleware {}, {}",
        manifest.next_version,
        manifest.prerenders.len(),
        prerender_bytes as f64 / 1e6,
        if leave { ", left out of the package" } else { "" },
        match &manifest.middleware {
            Some(m) => format!("on {} ({} matchers)", m.runtime, m.matchers.len()),
            None => "none".to_owned(),
        },
        match manifest.source {
            From::Adapter => "read from the adapter's outputs",
            From::Manifests => "read from .next's manifests",
        }
    ));
    let in_package = manifest.static_files.iter().filter(|f| f.server).count();
    if in_package > 0 {
        log.info(&format!(
            "{in_package} static files ({:.1} MB) are in the package too: images, which Next's optimizer reads from its own server, and what the middleware matches",
            kept_by_server as f64 / 1e6
        ));
    }

    // What a second package, of direct handler calls, is made from (Next 16.2 and later). Its
    // files are where the build left them: the project's `.next` and `node_modules`, which nothing
    // here moves or removes, so that package is made from `Handlers::files` while the build's
    // folder is still there (build.rs, right after this returns).
    if let Some(handlers) = &handlers {
        std::fs::write(out.join(HANDLERS_FILE), serde_json::to_vec(handlers)?).with_context(|| format!("writing {HANDLERS_FILE}"))?;
        manifest.handlers = Some(HANDLERS_FILE.to_owned());
    }
    let mut json = serde_json::to_vec(&manifest)?;
    json.push(b'\n');
    std::fs::write(out.join(MANIFEST_FILE), &json).with_context(|| format!("writing {}", out.join(MANIFEST_FILE).display()))?;

    let floor = {
        let memory_mb = manifest.functions.iter().filter_map(|f| f.memory).max();
        let max_duration_s = manifest.functions.iter().filter_map(|f| f.max_duration).max();
        (memory_mb.is_some() || max_duration_s.is_some()).then_some(crate::state::Floor { memory_mb, max_duration_s })
    };
    let (source_sha256, source_files) = hash_entries(&entries)?;
    let outline = outline(&manifest);
    Ok(Built { manifest, entries, static_dir, hashed, revalidated, floor, outline, source_sha256, source_files, seed, _scratch: scratch })
}

/// How many functions and prerenders a deployment's record lists: enough to read, and small
/// enough for the record whatever the app's size. The manifest has them all.
const OUTLINE_MAX: usize = 200;

fn outline(manifest: &Manifest) -> crate::state::Outline {
    use crate::state::{FunctionOutline, MiddlewareOutline, Outline, PrerenderOutline};
    Outline {
        functions: manifest
            .functions
            .iter()
            .take(OUTLINE_MAX)
            .map(|f| FunctionOutline { path: f.pathname.clone(), routes: Vec::new(), runtime: f.runtime.clone(), memory_mb: f.memory, max_duration_s: f.max_duration, streams: true })
            .collect(),
        prerenders: manifest
            .prerenders
            .iter()
            .take(OUTLINE_MAX)
            .map(|p| PrerenderOutline { path: p.pathname.clone(), expiration_s: p.revalidate, group: p.group, fallback: !p.files.is_empty(), allow_query: p.allow_query.clone() })
            .collect(),
        middleware: manifest
            .middleware
            .iter()
            .map(|m| MiddlewareOutline {
                function: "/_middleware".to_owned(),
                entry: None,
                matcher: Some(m.matchers.iter().map(|x| x.source.clone().unwrap_or_else(|| x.regexp.clone())).collect()),
                edge: m.runtime == "edge",
            })
            .collect(),
        images: None,
    }
}

fn hash_entries(entries: &BTreeMap<String, Entry>) -> Result<(String, usize)> {
    let mut hasher = Sha256::new();
    let mut files = 0;
    for (name, entry) in entries {
        hasher.update(name.as_bytes());
        hasher.update([0]);
        match &entry.source {
            Source::File(path) => {
                hasher.update(package::file_sha256(path)?);
                files += 1;
            }
            Source::Link(target) => {
                hasher.update([1]);
                hasher.update(target.as_bytes());
            }
        }
    }
    Ok((forkrail_common::hex(&hasher.finalize()), files))
}

/// Refuse a package Lambda wouldn't take, before it's zipped, saying what fills it.
fn check_size(entries: &BTreeMap<String, Entry>, dist: &Dist, prerender_bytes: u64, seeded: bool) -> Result<()> {
    let mut total = 0u64;
    let mut modules = 0u64;
    let mut server = 0u64;
    let mut largest: Vec<(u64, &str)> = Vec::new();
    let dist_server = dist.packaged(&join(&dist.name, "server/"));
    for (name, entry) in entries {
        let size = match &entry.source {
            Source::File(path) => std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
            Source::Link(target) => target.len() as u64,
        };
        total += size;
        if name.contains("node_modules/") {
            modules += size;
        } else if name.starts_with(&dist_server) {
            server += size;
        }
        largest.push((size, name));
    }
    if total <= package::MAX_UNZIPPED {
        return Ok(());
    }
    largest.sort_by_key(|(size, _)| std::cmp::Reverse(*size));
    let mb = |bytes: u64| bytes as f64 / 1_048_576.0;
    let top: Vec<String> = largest.iter().take(3).map(|(size, name)| format!("{name} ({:.0} MB)", mb(*size))).collect();
    bail!(
        "this {NAME} app's server is {:.0} MB unzipped, over the 250 MB a Lambda function's package may hold: node_modules {:.0} MB, {}/server {:.0} MB{}, the rest {:.0} MB; its largest files are {}. forkrail runs Next's server as one function for now: an app this large needs functions packaged as container images, which aren't supported yet",
        mb(total),
        mb(modules),
        dist.name,
        mb(server),
        if seeded || prerender_bytes == 0 { String::new() } else { format!(" (prerendered pages {:.0} MB of it)", mb(prerender_bytes)) },
        mb(total.saturating_sub(modules + server)),
        top.join(", ")
    )
}

// ---- sharp ----

/// What Lambda runs: sharp's own name for it.
const LAMBDA_PLATFORM: &str = "linux-arm64";

/// A path's `@img/sharp-<platform>` or `@img/sharp-libvips-<platform>` folder, as (the path up to
/// and with `@img/`, whether it's libvips, the platform, what follows the folder).
fn sharp_binary(name: &str) -> Option<(&str, bool, &str, &str)> {
    let at = name.find("@img/sharp-")?;
    let after = &name[at + "@img/sharp-".len()..];
    let (folder, rest) = after.split_once('/').unwrap_or((after, ""));
    let (libvips, platform) = match folder.strip_prefix("libvips-") {
        Some(platform) => (true, platform),
        None => (false, folder),
    };
    // `@img/sharp-wasm32` runs anywhere.
    if platform.is_empty() || platform == "wasm32" {
        return None;
    }
    Some((&name[..at + "@img/".len()], libvips, platform, rest))
}

/// Put Linux arm64's sharp where the build machine's is, so Next's image optimizer works on
/// Lambda. In forkrail's builder the install is Lambda's already and nothing is done; elsewhere
/// (`--local` on a Mac) the two packages are fetched from npm at the versions the project has.
/// A failure is only warned of: everything but `/_next/image` works without.
fn fix_sharp(entries: &mut BTreeMap<String, Entry>, fetch: FetchSharp, log: &dyn BuildLog) {
    let foreign: Vec<String> = entries.keys().filter(|n| sharp_binary(n).is_some_and(|(_, _, platform, _)| platform != LAMBDA_PLATFORM)).cloned().collect();
    if foreign.is_empty() {
        return;
    }
    match swap_sharp(entries, &foreign, fetch, log) {
        Ok(()) => {}
        Err(e) => log.info(&format!(
            "warning: sharp for Linux arm64 couldn't be put in the package ({e:#}): the app's own image optimizer (/_next/image) won't work on Lambda with this machine's sharp; build in the builder (deploy without --local) to have it"
        )),
    }
}

fn swap_sharp(entries: &mut BTreeMap<String, Entry>, foreign: &[String], fetch: FetchSharp, log: &dyn BuildLog) -> Result<()> {
    let package_of = |libvips: bool| format!("sharp-{}{LAMBDA_PLATFORM}", if libvips { "libvips-" } else { "" });
    // Every place the build machine's packages are: a real folder gets Linux's files at the
    // version it had (a project may hold two), a link is pointed at the Linux folder beside what
    // it pointed at.
    let mut folders: BTreeMap<(String, bool), Option<String>> = BTreeMap::new();
    for name in foreign {
        let (before, libvips, platform, rest) = sharp_binary(name).expect("foreign entries are sharp's");
        let entry = entries.remove(name).expect("listed from the entries");
        match entry.source {
            Source::Link(target) if rest.is_empty() => {
                let theirs = format!("sharp-{}{platform}", if libvips { "libvips-" } else { "" });
                let target = match target.strip_suffix(&theirs) {
                    Some(stem) => format!("{stem}{}", package_of(libvips)),
                    None => target,
                };
                entries.insert(format!("{before}{}", package_of(libvips)), Entry { source: Source::Link(target), modified: None });
            }
            Source::File(path) => {
                let version = folders.entry((before.to_owned(), libvips)).or_default();
                if rest == "package.json"
                    && let Ok(json) = std::fs::read_to_string(&path)
                {
                    *version = serde_json::from_str::<Value>(&json).ok().and_then(|p| text(&p, "version"));
                }
            }
            Source::Link(_) => {
                folders.entry((before.to_owned(), libvips)).or_default();
            }
        }
    }
    for ((before, libvips), version) in folders {
        let Some(version) = version else { bail!("{before}{} names no version", package_of(libvips)) };
        let from = fetch(&package_of(libvips), &version, log)?;
        for (rel, abs) in scan(&from)? {
            entries.insert(format!("{before}{}/{rel}", package_of(libvips)), Entry::file(abs));
        }
    }
    Ok(())
}

/// `@img/<package>` (sharp's Linux arm64 binary, or its libvips) at `version`, from npm, kept in
/// forkrail's cache folder for the next build: the package's own folder.
fn fetch_sharp(package: &str, version: &str, log: &dyn BuildLog) -> Result<PathBuf> {
    if !version.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+')) {
        bail!("sharp's version {version:?} isn't one to fetch");
    }
    let cache = package::native_cache()?.join("sharp").join(format!("{package}@{version}"));
    let folder = cache.join("node_modules/@img").join(package);
    if cache.join(".complete").is_file() && folder.is_dir() {
        return Ok(folder);
    }
    log.info(&format!("fetching @img/{package} {version} (this machine's sharp can't run on Lambda)"));
    let building = cache.with_extension(format!("tmp-{}", forkrail_common::random_hex(4)));
    std::fs::create_dir_all(&building)?;
    let mut npm = Command::new("npm");
    // The package alone: its own optional dependency (libvips) is fetched at the project's version.
    npm.args(["install", "--force", "--no-save", "--ignore-scripts", "--no-audit", "--no-fund", "--omit=optional", "--os=linux", "--cpu=arm64", "--libc=glibc", "--prefix"]).arg(&building);
    npm.arg(format!("@img/{package}@{version}"));
    let output = npm.stdin(std::process::Stdio::null()).output().context("running npm")?;
    if !output.status.success() || !building.join("node_modules/@img").join(package).is_dir() {
        let _ = std::fs::remove_dir_all(&building);
        bail!("npm install @img/{package}@{version} failed: {}", String::from_utf8_lossy(&output.stderr).lines().last().unwrap_or("").trim());
    }
    std::fs::write(building.join(".complete"), b"")?;
    let _ = std::fs::remove_dir_all(&cache);
    std::fs::create_dir_all(cache.parent().expect("has a folder"))?;
    std::fs::rename(&building, &cache).context("keeping the fetched sharp")?;
    Ok(folder)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Lines(std::sync::Mutex<Vec<String>>);

    impl BuildLog for Lines {
        fn line(&self, _: crate::buildlog::Stream, text: &str) {
            self.0.lock().unwrap().push(text.to_owned());
        }
    }

    fn lines() -> Lines {
        Lines(Default::default())
    }

    fn write(dir: &Path, files: &[(&str, &str)]) {
        for (name, text) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    const PREVIEW: &str = r#""preview": {"previewModeId": "p1d", "previewModeSigningKey": "s", "previewModeEncryptionKey": "e"}"#;

    /// A build as Next leaves one: the dist folder's manifests and a standalone folder, for a
    /// project at `app_dir` of its workspace ("" for a project of its own).
    fn built(dir: &Path, app_dir: &str, config: &str, files: &[(&str, &str)]) {
        let standalone = dir.join(".next/standalone").join(app_dir);
        write(
            dir,
            &[
                (".next/BUILD_ID", "b1\n"),
                (".next/required-server-files.json", &format!(r#"{{"version": 1, "relativeAppDir": "{app_dir}", "config": {config}}}"#)),
                (".next/routes-manifest.json", r#"{"version": 3, "basePath": "", "redirects": [{"source": "/old", "destination": "/new", "statusCode": 308, "regex": "^/old$"}], "headers": [], "rewrites": {"beforeFiles": [], "afterFiles": [], "fallback": []}, "dynamicRoutes": [{"page": "/blog/[slug]", "regex": "^/blog/([^/]+?)(?:/)?$", "routeKeys": {"nxtPslug": "nxtPslug"}}, {"page": "/posts/[id]", "regex": "^/posts/([^/]+?)(?:/)?$", "routeKeys": {"nxtPid": "nxtPid"}}], "staticRoutes": [], "dataRoutes": [], "rsc": {"header": "rsc"}}"#),
                (".next/images-manifest.json", r#"{"version": 1, "images": {"loader": "default", "unoptimized": false, "sizes": [64, 640], "formats": ["image/webp"], "minimumCacheTTL": 60}}"#),
                (".next/static/chunks/main-abc.js", "console.log(1)"),
                (".next/static/service-worker/sw.js", "self.addEventListener('install', () => {})"),
                (".next/static/media/hero.123.png", "png"),
                ("public/logo.svg", "<svg/>"),
                ("public/docs/secret.pdf", "pdf"),
                ("public/robots.txt", "User-agent: *"),
            ],
        );
        write(&standalone, &[("server.js", "require('next')"), ("package.json", "{}"), (".next/BUILD_ID", "b1\n"), ("node_modules/next/package.json", r#"{"version": "15.5.29"}"#), (".env", "SECRET=1"), (".env.production", "A=1")]);
        write(dir, &[("node_modules/next/package.json", r#"{"version": "15.5.29"}"#)]);
        write(dir, files);
        // What Next copies of the dist folder into the standalone one: the server's files.
        for (name, text) in files {
            if let Some(rest) = name.strip_prefix(".next/server/") {
                write(&standalone, &[(&format!(".next/server/{rest}"), text)]);
            }
        }
    }

    fn project(name: &str) -> TempDir {
        TempDir::new(name).unwrap()
    }

    fn first_page<'a>(m: &'a Manifest, pathname: &str) -> Option<&'a str> {
        m.prerenders.iter().find(|p| p.pathname == pathname).and_then(|p| p.page.as_deref())
    }

    #[test]
    fn versions_and_paths() {
        assert_eq!(Version::parse("16.4.0-canary.48"), Some(Version(16, 4, 0)));
        assert_eq!(Version::parse("v15.5.29"), Some(Version(15, 5, 29)));
        assert!(Version::parse("13.4.19").unwrap() < OLDEST && Version::parse("13.5.0").unwrap() >= OLDEST);
        assert_eq!(Version::parse("latest"), None);
        // The page each prerender output belongs to.
        for (output, page) in [
            ("/blog/first.rsc", "/blog/first"),
            ("/index.rsc", "/"),
            ("/index.segments/_tree.segment.rsc", "/"),
            ("/blog/first.segments/blog/$d$slug/__PAGE__.segment.rsc", "/blog/first"),
            ("/_next/data/b1/posts/1.json", "/posts/1"),
            ("/_next/data/b1/index.json", "/"),
            ("/docs/_next/data/b1/guide.json", "/docs/guide"),
            ("/blog/first.prefetch.rsc", "/blog/first"),
            ("/blog/index", "/blog/index"),
        ] {
            assert_eq!(page_of(output, "b1"), page, "{output}");
        }
        // Next hands an adapter its paths with the base path in front; the manifest's are without.
        assert_eq!((unbased("/docs", "/docs"), unbased("/docs/index.rsc", "/docs"), unbased("/docsy/x", "/docs"), unbased("/x", "")), ("/".to_owned(), "/index.rsc".to_owned(), "/docsy/x".to_owned(), "/x".to_owned()));
        // An App Router page's path: groups and the file's own name aren't part of it.
        assert_eq!(app_path("/(shop)/blog/[slug]/page").as_deref(), Some("/blog/[slug]"));
        assert_eq!(app_path("/page").as_deref(), Some("/"));
        assert_eq!(app_path("/api/hello/route").as_deref(), Some("/api/hello"));
        assert_eq!(app_path("/@modal/(.)photo/page"), None);
        assert_eq!(app_path("/feed/(.)photo/[id]/page").as_deref(), Some("/feed/(.)photo/[id]"));
    }

    #[test]
    fn installed_next_is_found_as_node_finds_it() {
        let dir = project("next-installed");
        // Hoisted to the workspace's top.
        write(dir.path(), &[("node_modules/next/package.json", r#"{"version": "14.2.35"}"#), ("apps/web/package.json", "{}")]);
        assert_eq!(installed(&dir.path().join("apps/web")).unwrap(), ("14.2.35".to_owned(), Version(14, 2, 35)));
        // Too old a Next is refused by name, with the version that works, before anything builds.
        write(dir.path(), &[("old/node_modules/next/package.json", r#"{"version": "12.3.4"}"#)]);
        let mut env = BTreeMap::new();
        let error = before_build(&dir.path().join("old"), &mut env, &lines()).err().unwrap().to_string();
        assert!(error.contains("12.3.4") && error.contains("13.5"), "{error}");
        // No node_modules at all under Yarn's Plug'n'Play: said by name.
        let pnp = project("next-pnp");
        write(pnp.path(), &[(".pnp.cjs", ""), ("apps/web/package.json", "{}")]);
        assert!(installed(&pnp.path().join("apps/web")).unwrap_err().to_string().contains("nodeLinker: node-modules"));
    }

    #[test]
    fn the_build_gets_its_variables_and_the_adapter() {
        let dir = project("next-env");
        write(dir.path(), &[("node_modules/next/package.json", r#"{"version": "16.4.6"}"#)]);
        // What every build is given, and a value of the project's own.
        let mut env: BTreeMap<String, String> = [("NOW_BUILDER", "1"), ("VERCEL", "1"), ("NEXT_TELEMETRY_DISABLED", "0")].iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        let log = lines();
        let adapter = before_build(dir.path(), &mut env, &log).unwrap();
        assert!(!env.contains_key("NOW_BUILDER"), "Next must not take the build for Vercel's own");
        assert_eq!((env["VERCEL"].as_str(), env["NEXT_PRIVATE_STANDALONE"].as_str(), env["NEXT_TELEMETRY_DISABLED"].as_str()), ("1", "1", "0"));
        let path = PathBuf::from(&env["NEXT_ADAPTER_PATH"]);
        assert!(path.is_absolute() && !path.starts_with(dir.path()), "the adapter is never written into the project");
        assert!(std::fs::read_to_string(&path).unwrap().contains("modifyConfig"));
        assert!(!env.contains_key(CACHE_ENV), "no cache handlers until the shared cache supplies them");
        assert_eq!(env[OUTPUTS_ENV], "1", "16.2 and later hand the adapter their outputs");
        drop(adapter);
        assert!(!path.exists(), "gone with the build");
        assert!(!env.contains_key("NODE_OPTIONS") && !dir.path().join(STUB_DIR).exists(), "15.0 on find the handlers at run time");
        // 15.5: the adapter, without the hook Next would run its own pass for.
        write(dir.path(), &[("node_modules/next/package.json", r#"{"version": "15.5.29"}"#)]);
        let mut env = BTreeMap::new();
        let _adapter = before_build(dir.path(), &mut env, &log).unwrap();
        assert!(env.contains_key("NEXT_ADAPTER_PATH") && !env.contains_key(OUTPUTS_ENV));
        // 16.0 and 16.1 would build their pages in minimal mode with one: none, the variable alone.
        for version in ["16.0.11", "16.1.7"] {
            write(dir.path(), &[("node_modules/next/package.json", &format!(r#"{{"version": "{version}"}}"#))]);
            let mut env = BTreeMap::new();
            let _adapter = before_build(dir.path(), &mut env, &log).unwrap();
            assert!(!env.contains_key("NEXT_ADAPTER_PATH") && env["NEXT_PRIVATE_STANDALONE"] == "1", "{version}");
        }
        // 16.3.0 to 16.3.4 crash with any adapter and a standalone server (canaries too): none, the
        // variable alone. 16.3.5 has the fix, and the adapter again.
        for (version, adapter) in [("16.3.0", false), ("16.3.1-canary.1", false), ("16.3.4", false), ("16.3.5", true), ("16.3.8", true), ("16.2.11", true)] {
            write(dir.path(), &[("node_modules/next/package.json", &format!(r#"{{"version": "{version}"}}"#))]);
            let mut env = BTreeMap::new();
            let _adapter = before_build(dir.path(), &mut env, &log).unwrap();
            assert_eq!(env.contains_key("NEXT_ADAPTER_PATH"), adapter, "{version}");
            assert_eq!(env.contains_key(OUTPUTS_ENV), adapter, "{version}");
            assert_eq!(env["NEXT_PRIVATE_STANDALONE"], "1", "{version}");
        }
        // Before 15.5 there's no adapter: the variable alone asks for the standalone server.
        write(dir.path(), &[("node_modules/next/package.json", r#"{"version": "15.2.0"}"#)]);
        let mut env = BTreeMap::new();
        let _adapter = before_build(dir.path(), &mut env, &log).unwrap();
        assert!(!env.contains_key("NEXT_ADAPTER_PATH") && env["NEXT_PRIVATE_STANDALONE"] == "1" && env["NEXT_TELEMETRY_DISABLED"] == "1");
        assert!(!env.contains_key("NODE_OPTIONS"));
        // 13.5 to 14.2: the config loader names forkrail's handler, a file put in the project's
        // node_modules for the build only. The project's own NODE_OPTIONS stay first.
        write(dir.path(), &[("node_modules/next/package.json", r#"{"version": "14.2.35"}"#)]);
        let mut env: BTreeMap<String, String> = [("NODE_OPTIONS".to_owned(), "--max-old-space-size=4096".to_owned())].into();
        let adapter = before_build(dir.path(), &mut env, &log).unwrap();
        let stub = dir.path().join(STUB_DIR).join(STUB_FILE);
        assert_eq!(env["FORKRAIL_NEXT_CACHE_STUB"], stub.to_string_lossy());
        assert_eq!(std::fs::read_to_string(&stub).unwrap(), STUB);
        let options = env["NODE_OPTIONS"].clone();
        assert!(options.starts_with("--max-old-space-size=4096 --require "), "{options}");
        let preload = PathBuf::from(options.rsplit(' ').next().unwrap());
        assert!(preload.is_file() && !preload.starts_with(dir.path()));
        drop(adapter);
        assert!(!stub.exists() && !dir.path().join(STUB_DIR).exists() && dir.path().join("node_modules/next/package.json").is_file(), "the build leaves the project as it was");
        assert!(!preload.exists());
        assert!(!env.contains_key(DEPLOYMENT_ID_ENV), "skew protection is off");
        // Skew protection on: the deployment's id is Next's deploymentId, unless the project sets
        // its own; Next before 14.1.4 takes it only with an experimental setting, and is told so.
        let skewed = |version: &str, own: Option<&str>| {
            write(dir.path(), &[("node_modules/next/package.json", &format!(r#"{{"version": "{version}"}}"#))]);
            let mut env: BTreeMap<String, String> = [("VERCEL_SKEW_PROTECTION_ENABLED", "1"), ("VERCEL_DEPLOYMENT_ID", "a1b2c3d4")].iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
            if let Some(own) = own {
                env.insert(DEPLOYMENT_ID_ENV.to_owned(), own.to_owned());
            }
            let log = lines();
            let _adapter = before_build(dir.path(), &mut env, &log).unwrap();
            let noted = log.0.lock().unwrap().iter().any(|l| l.contains("experimental.useDeploymentId"));
            (env[DEPLOYMENT_ID_ENV].clone(), noted)
        };
        assert_eq!(skewed("16.4.6", None), ("a1b2c3d4".to_owned(), false));
        assert_eq!(skewed("14.2.35", Some("mine")), ("mine".to_owned(), false));
        assert_eq!(skewed("13.5.11", None), ("a1b2c3d4".to_owned(), true));
        // A path with a space is quoted for Node.
        assert_eq!(with_require(None, Path::new("/tmp/a b/p.cjs")), r#"--require "/tmp/a b/p.cjs""#);
        assert_eq!(with_require(Some(" "), Path::new("/tmp/p.cjs")), "--require /tmp/p.cjs");
    }

    #[test]
    fn the_handler_is_the_runtimes_and_the_config_names_it_on_13_and_14() {
        // The stub is the one the runtime's handlers are written for.
        assert!(include_str!("../../runtime/src/next-cache.mjs").contains(&format!("export const STUB = `{STUB}`")));
        // The entry's helper is the runtime's, and says so in the order it's used.
        assert!(HELPER_JS.contains("forkrail.prime()") && SERVER_ENTRY_JS.contains("createRequire(import.meta.url)('./forkrail-next.cjs')"));
        if Command::new("node").arg("--version").output().is_err() {
            return;
        }
        let dir = project("next-preload-js");
        let preload = dir.path().join("preload.cjs");
        std::fs::write(&preload, PRELOAD).unwrap();
        let script = r#"
          process.env.FORKRAIL_NEXT_CACHE_STUB = '/p/node_modules/.forkrail/forkrail-cache-handler.cjs';
          const { patch } = require(process.argv[1]);
          console.log(JSON.stringify({
            v13: patch({ experimental: { a: 1 } }, '13.5.11'),
            v140: patch({}, '14.0.4'),
            v141: patch({ experimental: { a: 1 } }, '14.1.4'),
            v142: patch({}, '14.2.35'),
            own: patch({ cacheHandler: '/mine.js' }, '14.2.35'),
            own13: patch({ experimental: { incrementalCacheHandlerPath: '/mine.js' } }, '13.5.11'),
            v15: patch({}, '15.2.0'),
          }));
        "#;
        let output = Command::new("node").arg("-e").arg(script).arg(&preload).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let out: Value = serde_json::from_slice(&output.stdout).unwrap();
        let stub = "/p/node_modules/.forkrail/forkrail-cache-handler.cjs";
        assert_eq!(out["v13"], serde_json::json!({"experimental": {"a": 1, "incrementalCacheHandlerPath": stub, "isrMemoryCacheSize": 0}}));
        assert_eq!(out["v140"], serde_json::json!({"experimental": {"incrementalCacheHandlerPath": stub, "isrMemoryCacheSize": 0}}));
        assert_eq!(out["v141"], serde_json::json!({"experimental": {"a": 1}, "cacheHandler": stub, "cacheMaxMemorySize": 0}));
        assert_eq!(out["v142"], serde_json::json!({"cacheHandler": stub, "cacheMaxMemorySize": 0}));
        assert_eq!(out["own"], serde_json::json!({"cacheHandler": "/mine.js"}));
        assert_eq!(out["own13"], serde_json::json!({"experimental": {"incrementalCacheHandlerPath": "/mine.js"}}));
        assert_eq!(out["v15"], serde_json::json!({}));
    }

    #[test]
    fn whose_cache_handler_the_build_has() {
        let shared = |version: &str, config: &str| {
            let dir = project("next-handler");
            built(dir.path(), "", config, &[]);
            let m = assemble(dir.path(), &read_dist(dir.path(), DIST).unwrap(), version, None).unwrap();
            (m.shared_cache, m.cache_handler)
        };
        // 15.0 on: forkrail's handlers are found at run time unless the config names one.
        assert_eq!(shared("15.5.29", r#"{"distDir": ".next"}"#), (true, false));
        assert_eq!(shared("16.4.6", r#"{"distDir": ".next", "cacheHandler": "/app/cache.js"}"#), (false, true));
        // 13.5 to 14.2: only when the build's config took forkrail's.
        assert_eq!(shared("14.2.35", r#"{"distDir": ".next", "cacheHandler": "../node_modules/.forkrail/forkrail-cache-handler.cjs"}"#), (true, false));
        assert_eq!(shared("13.5.11", r#"{"distDir": ".next", "experimental": {"incrementalCacheHandlerPath": "../node_modules/.forkrail/forkrail-cache-handler.cjs"}}"#), (true, false));
        assert_eq!(shared("14.2.35", r#"{"distDir": ".next"}"#), (false, false));
        assert_eq!(shared("13.5.11", r#"{"distDir": ".next", "experimental": {"incrementalCacheHandlerPath": "/app/redis.js"}}"#), (false, true));
        // The deployment id Next built with: 13.5 keeps it under `experimental`.
        let id = |version: &str, config: &str| {
            let dir = project("next-dpl");
            built(dir.path(), "", config, &[]);
            assemble(dir.path(), &read_dist(dir.path(), DIST).unwrap(), version, None).unwrap().deployment_id
        };
        assert_eq!(id("16.4.6", r#"{"distDir": ".next", "deploymentId": "a1b2c3d4"}"#).as_deref(), Some("a1b2c3d4"));
        assert_eq!(id("13.5.11", r#"{"distDir": ".next", "experimental": {"useDeploymentId": true, "deploymentId": "a1b2c3d4"}}"#).as_deref(), Some("a1b2c3d4"));
        assert_eq!(id("14.2.35", r#"{"distDir": ".next", "deploymentId": ""}"#), None);
    }

    #[test]
    fn a_manifest_from_nexts_own_manifests() {
        let dir = project("next-manifests");
        built(
            dir.path(),
            "",
            r#"{"distDir": ".next", "basePath": "", "assetPrefix": "", "trailingSlash": true, "i18n": {"locales": ["en", "fr"], "defaultLocale": "en"}, "deploymentId": "dpl_1", "experimental": {}}"#,
            &[
                (
                    ".next/prerender-manifest.json",
                    &format!(
                        r#"{{"version": 4, "routes": {{
                          "/isr": {{"initialRevalidateSeconds": 30, "initialExpireSeconds": 31536000, "srcRoute": null, "dataRoute": "/_next/data/b1/isr.json"}},
                          "/posts/1": {{"initialRevalidateSeconds": false, "srcRoute": "/posts/[id]", "dataRoute": "/_next/data/b1/posts/1.json"}},
                          "/": {{"initialRevalidateSeconds": false, "srcRoute": "/", "dataRoute": "/index.rsc", "experimentalBypassFor": [{{"type": "header", "key": "Next-Action"}}]}},
                          "/blog/first": {{"initialRevalidateSeconds": 60, "srcRoute": "/blog/[slug]", "dataRoute": "/blog/first.rsc", "prefetchDataRoute": "/blog/first.prefetch.rsc", "renderingMode": "PARTIALLY_STATIC"}},
                          "/feed.xml": {{"initialRevalidateSeconds": 3600, "srcRoute": "/feed.xml", "dataRoute": null, "initialHeaders": {{"content-type": "application/xml", "x-next-cache-tags": "_N_T_/feed.xml,feed"}}}}
                        }}, "dynamicRoutes": {{
                          "/posts/[id]": {{"routeRegex": "^/posts/([^/]+?)(?:/)?$", "dataRoute": "/_next/data/b1/posts/[id].json", "fallback": "/posts/[id].html"}},
                          "/blog/[slug]": {{"routeRegex": "^/blog/([^/]+?)(?:/)?$", "dataRoute": "/blog/[slug].rsc", "fallback": null}}
                        }}, "notFoundRoutes": [], {PREVIEW}}}"#
                    ),
                ),
                (".next/server/middleware-manifest.json", r#"{"version": 3, "middleware": {"/": {"name": "middleware", "page": "/", "matchers": [{"regexp": "^(?:\\/(_next\\/data\\/[^/]{1,}))?\\/docs(?:\\/.*)?[\\/#\\?]?$", "originalSource": "/docs/:path*"}], "env": {"__NEXT_PREVIEW_MODE_SIGNING_KEY": "never-copied"}}}, "functions": {"/api/edge/route": {"name": "x", "page": "/api/edge/route"}}, "sortedMiddleware": ["/"]}"#),
                (".next/server/functions-config-manifest.json", r#"{"version": 1, "functions": {"/api/slow": {"maxDuration": 120}, "/blog/[slug]": {}}}"#),
                (".next/server/pages-manifest.json", r#"{"/_app": "pages/_app.js", "/_error": "pages/_error.js", "/404": "pages/404.html", "/about": "pages/about.html", "/isr": "pages/isr.js", "/posts/[id]": "pages/posts/[id].js", "/api/slow": "pages/api/slow.js"}"#),
                (".next/server/app-paths-manifest.json", r#"{"/page": "app/page.js", "/(site)/blog/[slug]/page": "app/(site)/blog/[slug]/page.js", "/api/edge/route": "app/api/edge/route.js", "/feed.xml/route": "app/feed.xml/route.js", "/_not-found/page": "app/_not-found/page.js"}"#),
                // Pages Router files beside the pages; an App Router page's with its record.
                (".next/server/pages/isr.html", "<html>isr</html>"),
                (".next/server/pages/isr.json", "{}"),
                (".next/server/pages/posts/1.html", "<html>1</html>"),
                (".next/server/pages/posts/1.json", "{}"),
                (".next/server/pages/posts/[id].html", "<html>fallback</html>"),
                (".next/server/app/index.html", "<html>home</html>"),
                (".next/server/app/index.rsc", "rsc"),
                (".next/server/app/index.meta", r#"{"headers": {"x-next-cache-tags": "_N_T_/layout,_N_T_/"}}"#),
                (".next/server/app/blog/first.html", "<html>first</html>"),
                (".next/server/app/blog/first.rsc", "rsc"),
                (".next/server/app/blog/first.prefetch.rsc", "prefetch"),
                (".next/server/app/blog/first.meta", r#"{"status": 200, "headers": {"x-next-cache-tags": "post:first,_N_T_/blog/[slug]/page"}, "postponed": "{\"state\":1}"}"#),
                (".next/server/app/blog/first.segments/_tree.segment.rsc", "tree"),
                (".next/server/app/feed.xml.body", "<rss/>"),
                (".next/server/app/feed.xml.meta", r#"{"status": 200, "headers": {"content-type": "application/xml"}}"#),
                ("pages/api/slow.ts", "export default 1"),
                ("app/api/edge/route.ts", "export const runtime = 'edge'"),
            ],
        );
        let dist = read_dist(dir.path(), DIST).unwrap();
        let vercel_json = serde_json::json!({"functions": {"pages/api/**/*": {"memory": 2048, "maxDuration": 30}, "app/api/**/*.ts": {"maxDuration": 15}}});
        let m = assemble(dir.path(), &dist, "15.4.1", Some(&vercel_json)).unwrap();
        assert_eq!((m.format, m.source, m.build_id.as_str(), m.next_version.as_str()), (FORMAT, From::Manifests, "b1", "15.4.1"));
        assert_eq!((m.app_dir.as_str(), m.dist_dir.as_str(), m.entry.as_str(), m.preview.id.as_str()), ("", ".next", ENTRY, "p1d"));
        assert_eq!((m.trailing_slash, m.deployment_id.as_deref(), m.i18n.as_ref().unwrap()["defaultLocale"].as_str()), (true, Some("dpl_1"), Some("en")));
        assert_eq!(m.routes["redirects"][0]["source"], "/old");
        assert_eq!(m.images.as_ref().unwrap()["sizes"], serde_json::json!([64, 640]));
        // The middleware: its runtime and matchers, never the keys Next puts in its environment.
        let middleware = m.middleware.as_ref().unwrap();
        assert_eq!((middleware.runtime.as_str(), middleware.matchers.len(), middleware.matchers[0].source.as_deref()), ("edge", 1, Some("/docs/:path*")));
        assert!(!serde_json::to_string(&m).unwrap().contains("never-copied"));
        // Functions: pages rendered to a file at build have none; each route's own settings, then
        // vercel.json's for its source file.
        let by_path: BTreeMap<&str, &Function> = m.functions.iter().map(|f| (f.pathname.as_str(), f)).collect();
        assert_eq!(by_path.keys().copied().collect::<Vec<_>>(), ["/", "/_not-found", "/api/edge", "/api/slow", "/blog/[slug]", "/feed.xml", "/isr", "/posts/[id]"]);
        let slow = by_path["/api/slow"];
        assert_eq!((slow.kind, slow.max_duration, slow.memory, slow.runtime.as_str()), (FunctionKind::Api, Some(120), Some(2048), "nodejs"));
        let edge = by_path["/api/edge"];
        assert_eq!((edge.kind, edge.runtime.as_str(), edge.max_duration, edge.memory), (FunctionKind::AppRoute, "edge", Some(15), None));
        assert_eq!((by_path["/blog/[slug]"].kind, by_path["/blog/[slug]"].page.as_str()), (FunctionKind::AppPage, "/(site)/blog/[slug]/page"));
        // Prerenders: each path with its files, where the package has them.
        let by_path: BTreeMap<(&str, PrerenderKind), &Prerender> = m.prerenders.iter().map(|p| ((p.pathname.as_str(), p.kind), p)).collect();
        let files = |p: &Prerender| p.files.iter().map(|f| (f.pathname.clone(), f.kind, f.file.clone())).collect::<Vec<_>>();
        let isr = by_path[&("/isr", PrerenderKind::Page)];
        assert_eq!((isr.router, isr.revalidate, isr.expire, isr.route.as_str(), isr.allow_query.clone()), (Router::Pages, Some(30), Some(31536000), "/isr", Some(vec![])));
        assert_eq!(
            files(isr),
            [("/isr".to_owned(), FileKind::Html, ".next/server/pages/isr.html".to_owned()), ("/_next/data/b1/isr.json".to_owned(), FileKind::Data, ".next/server/pages/isr.json".to_owned())]
        );
        let post = by_path[&("/posts/1", PrerenderKind::Page)];
        assert_eq!((post.revalidate, post.route.as_str(), post.allow_query.clone()), (None, "/posts/[id]", Some(vec!["id".to_owned()])));
        // The page each belongs to: a Pages Router page's is its route, an App Router one's its module.
        assert_eq!((post.page.as_deref(), first_page(&m, "/blog/first"), first_page(&m, "/")), (Some("/posts/[id]"), Some("/(site)/blog/[slug]/page"), Some("/page")));
        let home = by_path[&("/", PrerenderKind::Page)];
        assert_eq!((home.router, home.tags.clone(), home.files.len()), (Router::App, vec!["_N_T_/".to_owned(), "_N_T_/layout".to_owned()], 3));
        assert!(home.bypass_for.is_some());
        // A PPR page: its shell, postponed state, prefetch payload and segments.
        let first = by_path[&("/blog/first", PrerenderKind::Shell)];
        assert_eq!((first.revalidate, first.status, first.postponed.as_deref(), first.rendering_mode.as_deref()), (Some(60), Some(200), Some("{\"state\":1}"), Some("PARTIALLY_STATIC")));
        assert_eq!(first.tags, ["_N_T_/blog/[slug]/page", "post:first"]);
        assert_eq!(
            first.files.iter().map(|f| (f.pathname.as_str(), f.kind)).collect::<Vec<_>>(),
            [
                ("/blog/first", FileKind::Html),
                ("/blog/first.rsc", FileKind::Rsc),
                ("/blog/first.prefetch.rsc", FileKind::Prefetch),
                ("/blog/first.segments/_tree.segment.rsc", FileKind::Segment),
                ("/blog/first.meta", FileKind::Meta)
            ]
        );
        // A route handler's answer, with the headers and tags the manifest gives.
        let feed = by_path[&("/feed.xml", PrerenderKind::Route)];
        assert_eq!((feed.revalidate, feed.headers["content-type"].as_str(), feed.tags.clone()), (Some(3600), "application/xml", vec!["_N_T_/feed.xml".to_owned(), "feed".to_owned()]));
        assert_eq!(feed.files[0].kind, FileKind::Body);
        // A dynamic route's fallback page.
        let fallback = by_path[&("/posts/[id]", PrerenderKind::Fallback)];
        assert_eq!((fallback.router, fallback.files[0].file.as_str(), fallback.allow_query.clone()), (Router::Pages, ".next/server/pages/posts/[id].html", Some(vec!["id".to_owned()])));
        // It reads back as it was written.
        let again: Manifest = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(again, m);
    }

    #[test]
    fn prerenders_are_found_where_an_adapter_build_keeps_them() {
        // Next 15.5 to 16.1 with an adapter: no outputs handed over, and each route's files in a
        // folder of its own.
        let dir = project("next-scoped");
        built(
            dir.path(),
            "",
            r#"{"distDir": ".next"}"#,
            &[
                (".next/prerender-manifest.json", &format!(r#"{{"version": 4, "routes": {{"/posts/1": {{"initialRevalidateSeconds": 60, "srcRoute": "/posts/[id]", "dataRoute": "/_next/data/b1/posts/1.json"}}, "/": {{"initialRevalidateSeconds": false, "srcRoute": "/", "dataRoute": "/index.rsc"}}}}, "dynamicRoutes": {{}}, {PREVIEW}}}"#)),
                (".next/server/route-cache/PAGES/c5d6/$/posts/1.html", "<html>1</html>"),
                (".next/server/route-cache/PAGES/c5d6/$/posts/1.json", "{}"),
                (".next/server/route-cache/PAGES/c5d6/$/posts/1.meta", r#"{"headers": {}}"#),
                (".next/server/route-cache/APP_PAGE/aa11/$/index.html", "<html/>"),
                (".next/server/route-cache/APP_PAGE/aa11/$/index.rsc", "rsc"),
                (".next/server/route-cache/APP_PAGE/aa11/$/index.segments/__PAGE__.segment.rsc", "seg"),
                // The Node middleware (`proxy.ts`) is a function's config, not an Edge manifest's.
                (".next/server/functions-config-manifest.json", r#"{"version": 1, "functions": {"/_middleware": {"runtime": "nodejs", "matchers": [{"regexp": "^/admin.*$", "originalSource": "/admin/:path*"}]}}}"#),
            ],
        );
        let dist = read_dist(dir.path(), DIST).unwrap();
        let m = assemble(dir.path(), &dist, "15.5.29", None).unwrap();
        assert_eq!(m.source, From::Manifests);
        assert_eq!(m.middleware.as_ref().map(|x| (x.runtime.as_str(), x.matchers[0].regexp.as_str())), Some(("nodejs", "^/admin.*$")));
        let post = m.prerenders.iter().find(|p| p.pathname == "/posts/1").unwrap();
        assert_eq!(
            post.files.iter().map(|f| (f.kind, f.file.as_str())).collect::<Vec<_>>(),
            [
                (FileKind::Html, ".next/server/route-cache/PAGES/c5d6/$/posts/1.html"),
                (FileKind::Data, ".next/server/route-cache/PAGES/c5d6/$/posts/1.json"),
                (FileKind::Meta, ".next/server/route-cache/PAGES/c5d6/$/posts/1.meta")
            ]
        );
        let home = m.prerenders.iter().find(|p| p.pathname == "/").unwrap();
        assert_eq!(home.files.iter().map(|f| f.pathname.as_str()).collect::<Vec<_>>(), ["/", "/index.rsc", "/index.segments/__PAGE__.segment.rsc"]);
    }

    /// What the adapter writes for a build with one ISR page, a PPR route's shell, a Pages Router
    /// page and a route handler.
    const ADAPTER_DUMP: &str = r#"{
      "format": 1, "nextVersion": "16.4.6", "buildId": "b1", "distDir": ".next",
      "config": {"basePath": "/docs", "cacheComponents": true},
      "routing": {"beforeMiddleware": [], "beforeFiles": [], "afterFiles": [], "dynamicRoutes": [], "onMatch": [], "fallback": [], "shouldNormalizeNextData": false, "rsc": {}},
      "functions": [
        {"kind": "appPages", "id": "/isr", "pathname": "/docs/isr", "sourcePage": "/isr/page", "runtime": "nodejs"},
        {"kind": "appPages", "id": "/isr.rsc", "pathname": "/docs/isr.rsc", "sourcePage": "/isr/page", "runtime": "nodejs"},
        {"kind": "appPages", "id": "/index.rsc", "pathname": "/docs/index.rsc", "sourcePage": "/page", "runtime": "nodejs"},
        {"kind": "appPages", "id": "/", "pathname": "/docs", "sourcePage": "/page", "runtime": "nodejs"},
        {"kind": "appRoutes", "id": "/api/slow.rsc", "pathname": "/docs/api/slow.rsc", "sourcePage": "/api/slow/route", "runtime": "edge"},
        {"kind": "appRoutes", "id": "/api/slow", "pathname": "/docs/api/slow", "sourcePage": "/api/slow/route", "runtime": "edge", "maxDuration": 42, "preferredRegion": ["sfo1"], "edgeRuntime": {"modulePath": "m", "entryKey": "e", "handlerExport": "handler"}},
        {"kind": "pages", "id": "/legacy", "pathname": "/docs/legacy", "sourcePage": "/legacy", "runtime": "nodejs"}
      ],
      "middleware": {"kind": "middleware", "pathname": "/docs/_middleware", "runtime": "nodejs", "matchers": [{"source": "/isr", "sourceRegex": "^\\/isr[\\/#\\?]?$", "missing": [{"type": "header", "key": "x-prerender-revalidate", "value": "p1d"}]}]},
      "handlers": {
        "repoRoot": "/build/repo", "projectDir": "apps/web",
        "files": ["node_modules/next/dist/server.js", ["apps/web/.next/node_modules/pg-1a2b/index.js", "node_modules/pg/index.js"], "apps/web/.next/server/chunks/1.js"],
        "outputs": [
          {"kind": "appPages", "id": "/isr", "pathname": "/docs/isr", "sourcePage": "/isr/page", "runtime": "nodejs", "file": "apps/web/.next/server/app/isr/page.js", "config": {}, "assets": [0, 2]},
          {"kind": "appRoutes", "id": "/api/slow", "pathname": "/docs/api/slow", "sourcePage": "/api/slow/route", "runtime": "edge", "file": "apps/web/.next/server/app/api/slow/route.js", "config": {"maxDuration": 42, "env": {"KEY": "k"}}, "edgeRuntime": {"modulePath": "m", "entryKey": "e", "handlerExport": "handler"}, "assets": [1], "wasmAssets": {"wasm_1": "apps/web/.next/server/chunks/x.wasm"}}
        ],
        "middleware": {"kind": "middleware", "id": "/_middleware", "pathname": "/_middleware", "runtime": "nodejs", "file": "apps/web/.next/server/middleware.js", "config": {}, "assets": [0]}
      },
      "prerenders": [
        {"id": "/isr", "pathname": "/docs/isr", "parentOutputId": "/isr", "groupId": 3, "route": "/isr", "routeType": "page", "response": "complete", "compute": "static",
         "fallback": {"file": ".next/server/route-cache/APP_PAGE/h1/$/isr.html", "initialRevalidate": 30, "initialExpiration": 31536000, "initialHeaders": {"x-next-cache-tags": "_N_T_/isr,posts", "vary": ["rsc", "next-router-state-tree"]}},
         "allowQuery": [], "allowHeader": ["host"], "bypassFor": [{"type": "header", "key": "next-action"}]},
        {"id": "/isr.rsc", "pathname": "/docs/isr.rsc", "parentOutputId": "/isr", "groupId": 3, "route": "/isr", "fallback": {"file": ".next/server/route-cache/APP_PAGE/h1/$/isr.rsc", "initialRevalidate": 30}},
        {"id": "/isr.segments/_tree.segment.rsc", "pathname": "/docs/isr.segments/_tree.segment.rsc", "parentOutputId": "/isr", "groupId": 3, "route": "/isr", "fallback": {"file": ".next/server/route-cache/APP_PAGE/h1/$/isr.segments/_tree.segment.rsc", "initialRevalidate": 30}},
        {"id": "/shop/[id]", "pathname": "/docs/shop/[id]", "parentOutputId": "/shop/[id]", "groupId": 4, "route": "/shop/[id]", "routeType": "shell", "response": "initial", "compute": "resuming",
         "fallback": {"file": ".next/server/route-cache/APP_PAGE/h2/$/shop/[id].html", "initialRevalidate": false, "initialStatus": 200, "postponedState": "POSTPONED"},
         "allowQuery": ["id"], "renderingMode": "PARTIALLY_STATIC", "pprChain": {"headers": {"next-resume": "1"}}},
        {"id": "/legacy", "pathname": "/docs/legacy", "parentOutputId": "/legacy", "groupId": 5, "route": "/legacy", "routeType": "page", "fallback": {"file": ".next/server/route-cache/PAGES/h3/$/legacy.html", "initialRevalidate": 10}},
        {"id": "/_next/data/b1/legacy.json", "pathname": "/docs/_next/data/b1/legacy.json", "parentOutputId": "/legacy", "groupId": 5, "route": "/legacy", "fallback": {"file": ".next/server/route-cache/PAGES/h3/$/legacy.json", "initialRevalidate": 10}},
        {"id": "/feed", "pathname": "/docs/feed", "parentOutputId": "/feed", "groupId": 6, "route": "/feed", "routeType": "route", "fallback": {"file": ".next/server/route-cache/APP_ROUTE/h4/$/feed.body", "initialRevalidate": 3600, "initialHeaders": {"content-type": "application/xml"}}},
        {"id": "/orphan.rsc", "pathname": "/docs/orphan.rsc", "parentOutputId": "/orphan", "groupId": 9, "route": "/orphan", "fallback": {"file": ".next/server/route-cache/APP_PAGE/h5/$/orphan.rsc"}},
        {"id": "/no-file", "pathname": "/docs/no-file.rsc", "parentOutputId": "/no-file", "groupId": 10, "route": "/no-file"}
      ]
    }"#;

    #[test]
    fn a_manifest_from_the_adapters_outputs() {
        let dir = project("next-adapter");
        built(
            dir.path(),
            "apps/web",
            r#"{"distDir": ".next", "basePath": "/docs", "cacheComponents": true, "cacheHandler": ""}"#,
            &[
                (".next/forkrail-next.json", ADAPTER_DUMP),
                (".next/prerender-manifest.json", &format!(r#"{{"version": 4, "routes": {{}}, "dynamicRoutes": {{}}, {PREVIEW}}}"#)),
                (".next/server/route-cache/APP_PAGE/h1/$/isr.html", "<html>isr</html>"),
                (".next/server/route-cache/APP_PAGE/h1/$/isr.meta", "{}"),
                (".next/server/route-cache/APP_PAGE/h1/$/isr.rsc", "rsc"),
                (".next/server/route-cache/APP_PAGE/h1/$/isr.segments/_tree.segment.rsc", "tree"),
                (".next/server/route-cache/APP_PAGE/h2/$/shop/[id].html", "<html>shell</html>"),
                (".next/server/route-cache/PAGES/h3/$/legacy.html", "<html/>"),
                (".next/server/route-cache/PAGES/h3/$/legacy.json", "{}"),
                (".next/server/route-cache/APP_ROUTE/h4/$/feed.body", "<rss/>"),
                (".next/server/route-cache/APP_PAGE/h5/$/orphan.rsc", "rsc"),
            ],
        );
        let dist = read_dist(dir.path(), DIST).unwrap();
        assert_eq!(dist.app_dir, "apps/web");
        let (m, handlers) = assemble_with(dir.path(), &dist, "16.4.6", None).unwrap();
        assert_eq!((m.source, m.base_path.as_str(), m.cache_components, m.cache_handler, m.app_dir.as_str()), (From::Adapter, "/docs", true, false, "apps/web"));
        // What a package of direct handler calls is made from: Next's outputs as handed over,
        // with the build's own facts, and every file once.
        let handlers = Handlers(handlers.unwrap());
        assert_eq!((handlers.0["format"].as_u64(), handlers.0["buildId"].as_str(), handlers.0["basePath"].as_str(), handlers.0["distDir"].as_str()), (Some(1), Some("b1"), Some("/docs"), Some(".next")));
        assert!(handlers.0["routing"].get("beforeFiles").is_some() && handlers.0["prerenders"].as_array().unwrap().len() == 9);
        assert_eq!(handlers.0["outputs"][1]["config"]["env"]["KEY"], "k");
        let files = handlers.files(Path::new("/now/repo"));
        assert_eq!(
            files.iter().map(|(path, from)| (path.as_str(), from.to_str().unwrap())).collect::<Vec<_>>(),
            [
                ("apps/web/.next/node_modules/pg-1a2b/index.js", "/now/repo/node_modules/pg/index.js"),
                ("apps/web/.next/server/app/api/slow/route.js", "/now/repo/apps/web/.next/server/app/api/slow/route.js"),
                ("apps/web/.next/server/app/isr/page.js", "/now/repo/apps/web/.next/server/app/isr/page.js"),
                ("apps/web/.next/server/chunks/1.js", "/now/repo/apps/web/.next/server/chunks/1.js"),
                ("apps/web/.next/server/chunks/x.wasm", "/now/repo/apps/web/.next/server/chunks/x.wasm"),
                ("apps/web/.next/server/middleware.js", "/now/repo/apps/web/.next/server/middleware.js"),
                ("node_modules/next/dist/server.js", "/now/repo/node_modules/next/dist/server.js"),
            ]
        );
        // The manifest names the outputs by id and keeps what ties a prerender to its output.
        let slow = m.functions.iter().find(|f| f.pathname == "/api/slow").unwrap();
        assert_eq!((slow.id.as_deref(), slow.edge_runtime.as_ref().map(|e| e["entryKey"].clone())), (Some("/api/slow"), Some(Value::from("e"))));
        let shell = m.prerenders.iter().find(|p| p.pathname == "/shop/[id]").unwrap();
        assert_eq!((shell.parent.as_deref(), shell.ppr_chain.as_ref().map(|c| c["headers"]["next-resume"].clone())), (Some("/shop/[id]"), Some(Value::from("1"))));
        assert!(m.adapter_routing.as_ref().unwrap().get("beforeFiles").is_some());
        assert_eq!(m.middleware.as_ref().unwrap().matchers[0].missing.as_ref().unwrap()[0]["key"], "x-prerender-revalidate");
        // A page and its `.rsc` twin are one function, named without the base path Next hands it with.
        assert_eq!(m.functions.iter().map(|f| (f.pathname.as_str(), f.kind)).collect::<Vec<_>>(), [("/", FunctionKind::AppPage), ("/api/slow", FunctionKind::AppRoute), ("/isr", FunctionKind::AppPage), ("/legacy", FunctionKind::Page)]);
        let slow = m.functions.iter().find(|f| f.pathname == "/api/slow").unwrap();
        assert_eq!((slow.runtime.as_str(), slow.max_duration, slow.regions.clone()), ("edge", Some(42), Some(serde_json::json!(["sfo1"]))));
        // Each page with everything made for it, at its place in a workspace's package.
        let isr = m.prerenders.iter().find(|p| p.pathname == "/isr").unwrap();
        assert_eq!((isr.kind, isr.router, isr.group, isr.revalidate, isr.expire), (PrerenderKind::Page, Router::App, Some(3), Some(30), Some(31536000)));
        assert_eq!(isr.page.as_deref(), Some("/isr/page"));
        assert_eq!((isr.tags.clone(), isr.headers["vary"].as_str(), isr.allow_query.clone()), (vec!["_N_T_/isr".to_owned(), "posts".to_owned()], "rsc, next-router-state-tree", Some(vec![])));
        assert_eq!(
            isr.files.iter().map(|f| (f.pathname.as_str(), f.kind, f.file.as_str())).collect::<Vec<_>>(),
            [
                ("/isr", FileKind::Html, "apps/web/.next/server/route-cache/APP_PAGE/h1/$/isr.html"),
                ("/isr.meta", FileKind::Meta, "apps/web/.next/server/route-cache/APP_PAGE/h1/$/isr.meta"),
                ("/isr.rsc", FileKind::Rsc, "apps/web/.next/server/route-cache/APP_PAGE/h1/$/isr.rsc"),
                ("/isr.segments/_tree.segment.rsc", FileKind::Segment, "apps/web/.next/server/route-cache/APP_PAGE/h1/$/isr.segments/_tree.segment.rsc")
            ]
        );
        let shell = m.prerenders.iter().find(|p| p.pathname == "/shop/[id]").unwrap();
        assert_eq!((shell.kind, shell.postponed.as_deref(), shell.revalidate, shell.status, shell.allow_query.clone()), (PrerenderKind::Shell, Some("POSTPONED"), None, Some(200), Some(vec!["id".to_owned()])));
        let legacy = m.prerenders.iter().find(|p| p.pathname == "/legacy").unwrap();
        assert_eq!((legacy.router, legacy.files.iter().map(|f| f.kind).collect::<Vec<_>>()), (Router::Pages, vec![FileKind::Data, FileKind::Html]));
        let feed = m.prerenders.iter().find(|p| p.pathname == "/feed").unwrap();
        assert_eq!((feed.kind, feed.files[0].kind, feed.headers["content-type"].as_str()), (PrerenderKind::Route, FileKind::Body, "application/xml"));
        // Files whose page the build didn't list keep an entry of their own; one without a file has none.
        let orphan = m.prerenders.iter().find(|p| p.pathname == "/orphan").unwrap();
        assert_eq!((orphan.kind, orphan.files.len()), (PrerenderKind::Data, 1));
        assert!(!m.prerenders.iter().any(|p| p.pathname.starts_with("/no-file")));
        // Another build's dump, left in a dist folder, isn't read.
        std::fs::write(dir.path().join(".next/BUILD_ID"), "b2").unwrap();
        let dist = read_dist(dir.path(), DIST).unwrap();
        assert_eq!(assemble(dir.path(), &dist, "16.4.6", None).unwrap().source, From::Manifests);
    }

    #[test]
    fn the_adapter_asks_for_a_standalone_server_and_writes_what_next_hands_it() {
        if Command::new("node").arg("--version").output().is_err() {
            return;
        }
        let dir = project("next-adapter-js");
        let adapter = dir.path().join("adapter.cjs");
        std::fs::write(&adapter, ADAPTER).unwrap();
        write(dir.path(), &[("app/.next/server/x.html", "<html/>")]);
        let script = r#"
          const [adapterPath, projectDir] = process.argv.slice(1);
          // Before 16.2 the hook isn't there: Next 15.5 then runs no pass of its own for it.
          const early = require(adapterPath);
          if (early.onBuildComplete !== undefined) throw new Error('onBuildComplete without FORKRAIL_NEXT_OUTPUTS');
          delete require.cache[require.resolve(adapterPath)];
          process.env.FORKRAIL_NEXT_OUTPUTS = '1';
          const adapter = require(adapterPath);
          const out = {};
          out.plain = adapter.modifyConfig({ output: undefined, basePath: '/x', cacheHandlers: { mine: '/m.js' } });
          out.exported = adapter.modifyConfig({ output: 'export' });
          process.env.FORKRAIL_NEXT_CACHE_HANDLERS = JSON.stringify({ cacheHandler: '/h.js', cacheHandlers: { default: '/d.js' }, cacheMaxMemorySize: 0 });
          out.cached = adapter.modifyConfig({ output: 'export2', cacheHandlers: { mine: '/m.js' } });
          // Next 15.5's experimental context: nothing written, nothing thrown.
          adapter.onBuildComplete({ outputs: { pages: [] }, routes: {} });
          adapter.onBuildComplete({
            nextVersion: '16.4.6', buildId: 'b1', projectDir, repoRoot: projectDir, distDir: projectDir + '/.next',
            config: { basePath: '', experimental: {}, cacheComponents: true, images: {} },
            routing: { beforeFiles: [], rsc: {} },
            outputs: {
              pages: [], pagesApi: [], appRoutes: [], staticFiles: [],
              appPages: [{ id: '/x', pathname: '/x', sourcePage: '/x/page', runtime: 'nodejs', filePath: projectDir + '/.next/server/app/x/page.js', assets: { 'node_modules/a.js': projectDir + '/node_modules/a.js', '.next/node_modules/b-1a2b/index.js': projectDir + '/node_modules/b/index.js' }, config: { maxDuration: 9 } }],
              middleware: { id: '/_middleware', pathname: '/_middleware', runtime: 'edge', filePath: projectDir + '/.next/server/middleware.js', assets: {}, config: { matchers: [{ source: '/x', sourceRegex: '^/x$' }], env: { SECRET_KEY: 'never-copied' } } },
              prerenders: [{ id: '/x', pathname: '/x', parentOutputId: '/x', groupId: 1, route: '/x', routeType: 'page', fallback: { filePath: projectDir + '/.next/server/x.html', initialRevalidate: 5, postponedState: undefined }, config: { allowQuery: [], bypassToken: 'never-copied' } }],
            },
          });
          console.log(JSON.stringify(out));
        "#;
        let project = dir.path().join("app");
        let output = Command::new("node").arg("-e").arg(script).arg(&adapter).arg(&project).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let out: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!((out["plain"]["output"].as_str(), out["plain"]["basePath"].as_str()), (Some("standalone"), Some("/x")));
        assert!(out["plain"].get("cacheHandler").is_none(), "no handler unless the build names one");
        assert_eq!(out["exported"]["output"], "export", "a static export is left alone");
        assert_eq!((out["cached"]["output"].as_str(), out["cached"]["cacheHandler"].as_str(), out["cached"]["cacheMaxMemorySize"].as_u64()), (Some("standalone"), Some("/h.js"), Some(0)));
        assert_eq!(out["cached"]["cacheHandlers"], serde_json::json!({"mine": "/m.js", "default": "/d.js"}));
        let text = std::fs::read_to_string(project.join(".next").join(ADAPTER_OUTPUT)).unwrap();
        let dump: Value = serde_json::from_str(&text).unwrap();
        // The manifest's part has no keys and no traced files; the handlers' part has both.
        let mut slim = dump.clone();
        slim.as_object_mut().unwrap().remove("handlers");
        assert!(!slim.to_string().contains("never-copied") && !slim.to_string().contains("a.js"));
        assert!(!text.contains("bypassToken"));
        let h = &dump["handlers"];
        assert_eq!((h["projectDir"].as_str(), h["files"].clone()), (Some(""), serde_json::json!(["node_modules/a.js", [".next/node_modules/b-1a2b/index.js", "node_modules/b/index.js"]])));
        assert_eq!(h["outputs"][0], serde_json::json!({"kind": "appPages", "id": "/x", "pathname": "/x", "sourcePage": "/x/page", "runtime": "nodejs", "file": ".next/server/app/x/page.js", "config": {"maxDuration": 9}, "assets": [0, 1]}));
        assert_eq!((h["middleware"]["file"].as_str(), h["middleware"]["config"]["env"]["SECRET_KEY"].as_str(), h["middleware"]["runtime"].as_str()), (Some(".next/server/middleware.js"), Some("never-copied"), Some("edge")));
        assert_eq!((dump["format"].as_u64(), dump["buildId"].as_str(), dump["distDir"].as_str()), (Some(1), Some("b1"), Some(".next")));
        assert_eq!(dump["functions"][0], serde_json::json!({"kind": "appPages", "id": "/x", "pathname": "/x", "sourcePage": "/x/page", "runtime": "nodejs", "file": ".next/server/app/x/page.js", "maxDuration": 9}));
        assert_eq!(dump["middleware"]["matchers"][0]["sourceRegex"], "^/x$");
        assert_eq!((dump["prerenders"][0]["fallback"]["file"].as_str(), dump["prerenders"][0]["fallback"]["initialRevalidate"].as_u64()), (Some(".next/server/x.html"), Some(5)));
    }

    #[cfg(unix)]
    #[test]
    fn a_standalone_folder_keeps_its_links() {
        use std::os::unix::fs::symlink;
        let dir = project("next-walk");
        let root = dir.path().join("standalone");
        let outside = dir.path().join("elsewhere");
        write(
            &root,
            &[
                ("server.js", "x"),
                ("node_modules/.pnpm/next@16/node_modules/next/package.json", "{}"),
                ("node_modules/.pnpm/react@19/node_modules/react/index.js", "react"),
                (".next/server/app/page.js", "page"),
                (".DS_Store", "junk"),
            ],
        );
        write(&outside, &[("shared/util.js", "util"), ("one.txt", "one")]);
        // pnpm's: the package by its name, and a dependency beside the package that needs it.
        symlink(".pnpm/next@16/node_modules/next", root.join("node_modules/next")).unwrap();
        symlink("../../react@19/node_modules/react", root.join("node_modules/.pnpm/next@16/node_modules/react")).unwrap();
        // Turbopack's: an external under a hashed name, and one written with a whole path.
        std::fs::create_dir_all(root.join(".next/node_modules")).unwrap();
        symlink("../../node_modules/next", root.join(".next/node_modules/next-1a2b")).unwrap();
        symlink(root.join("node_modules/.pnpm/react@19/node_modules/react"), root.join(".next/node_modules/react-3c4d")).unwrap();
        // Links that leave the folder: replaced by what they point at.
        symlink(outside.join("shared"), root.join("shared")).unwrap();
        symlink(outside.join("one.txt"), root.join("one.txt")).unwrap();
        symlink(&root, outside.join("shared/back")).unwrap();
        symlink(outside.join("nothing"), root.join("dangling")).unwrap();
        let entries = walk(&root).unwrap();
        let link = |name: &str| match &entries[name].source {
            Source::Link(target) => target.clone(),
            Source::File(_) => panic!("{name} is a file"),
        };
        assert_eq!(link("node_modules/next"), ".pnpm/next@16/node_modules/next");
        assert_eq!(link("node_modules/.pnpm/next@16/node_modules/react"), "../../react@19/node_modules/react");
        assert_eq!(link(".next/node_modules/next-1a2b"), "../../node_modules/next");
        assert_eq!(link(".next/node_modules/react-3c4d"), "../../node_modules/.pnpm/react@19/node_modules/react", "a whole path becomes one inside the package");
        // Each real folder once: nothing is listed through a link.
        assert!(!entries.keys().any(|k| k.starts_with("node_modules/next/")));
        assert_eq!(entries.keys().filter(|k| k.ends_with("react/index.js")).count(), 1);
        assert!(matches!(&entries["shared/util.js"].source, Source::File(_)) && matches!(&entries["one.txt"].source, Source::File(_)));
        assert!(!entries.contains_key("dangling") && !entries.contains_key(".DS_Store"));
        assert_eq!(link("shared/back"), "..", "a link back into the folder from outside it stays a link, from where it is in the package");
        // Files keep their own times, for Next to tell when a page was made.
        assert!(entries[".next/server/app/page.js"].modified.is_some());
    }

    #[test]
    fn matchers_decide_which_files_the_router_may_answer() {
        let matcher = |regexp: &str| Matcher { source: None, regexp: regexp.to_owned(), has: None, missing: None };
        // saleor's: everything but /api, /_next and files with an extension.
        let most = Middleware { runtime: "edge".into(), matchers: vec![matcher(r"^(?:\/(_next\/data\/[^/]{1,}))?(?:\/((?!api\/|api$|_next\/|.*\.[\w]+$).*))(\.json)?[\/#\?]?$")] };
        let m = Matchers::of(Some(&most));
        assert!(m.may_match("/en/products") && !m.may_match("/_next/static/chunks/a.js") && !m.may_match("/logo.svg") && !m.may_match("/api/x"));
        // No matcher in the app: Next's own pattern takes every path.
        let all = Middleware { runtime: "nodejs".into(), matchers: vec![matcher("^/.*$")] };
        assert!(Matchers::of(Some(&all)).may_match("/_next/static/chunks/a.js"));
        // A pattern this engine can't read counts as matching, so nothing skips the middleware.
        let odd = Middleware { runtime: "edge".into(), matchers: vec![matcher("^/(?<bad")] };
        assert!(Matchers::of(Some(&odd)).may_match("/anything"));
        assert!(!Matchers::of(None).may_match("/anything"));
    }

    #[test]
    fn a_build_becomes_a_package_static_files_and_a_manifest() {
        let dir = project("next-collect");
        built(
            dir.path(),
            "apps/web",
            r#"{"distDir": ".next", "basePath": "/shop", "assetPrefix": "/cdn"}"#,
            &[
                (".next/prerender-manifest.json", &format!(r#"{{"version": 4, "routes": {{"/isr": {{"initialRevalidateSeconds": 30, "srcRoute": null, "dataRoute": "/_next/data/b1/isr.json"}}}}, "dynamicRoutes": {{"/posts/[id]": {{"routeRegex": "x", "dataRoute": "/_next/data/b1/posts/[id].json", "fallback": "/posts/[id].html"}}, "/drafts/[id]": {{"routeRegex": "x", "dataRoute": "/_next/data/b1/drafts/[id].json", "fallback": "/posts/[id].html"}}}}, {PREVIEW}}}"#)),
                (".next/server/pages/isr.html", "<html>isr</html>"),
                (".next/server/pages/isr.json", "{}"),
                (".next/server/pages/posts/[id].html", "<html>fallback</html>"),
                (".next/server/pages/isr.js", "module.exports = 1"),
                (".next/server/middleware-manifest.json", r#"{"version": 3, "middleware": {"/": {"matchers": [{"regexp": "^\\/shop\\/docs(?:\\/.*)?$", "originalSource": "/docs/:path*"}]}}, "functions": {}}"#),
                (".next/server/functions-config-manifest.json", r#"{"version": 1, "functions": {"/api/slow": {"maxDuration": 90}}}"#),
                (".next/server/pages-manifest.json", r#"{"/api/slow": "pages/api/slow.js", "/isr": "pages/isr.js"}"#),
            ],
        );
        let out = project("next-collect-out");
        let log = lines();
        let no_fetch: FetchSharp = &|_, _, _| panic!("there's no sharp to swap");
        // The seed is made from the standalone folder as the build left it.
        let asked = std::sync::Mutex::new(Vec::new());
        let seeder = |app: &Path, dist: &str, into: &Path| {
            asked.lock().unwrap().push((app.to_path_buf(), dist.to_owned(), into.to_path_buf()));
            assert_eq!(std::fs::read_to_string(app.join(".next/server/pages/isr.html")).unwrap(), "<html>isr</html>");
            std::fs::create_dir_all(into).unwrap();
            std::fs::write(into.join("index.json"), r#"{"v":1,"built_ms":1,"entries":["ab"]}"#).unwrap();
            Ok(crate::nextseed::Seeded { entries: 1, bytes: 30, skipped: 0, built_ms: 1 })
        };
        let b = collect_with(dir.path(), None, out.path(), true, no_fetch, &seeder, &log).unwrap();
        assert_eq!(asked.lock().unwrap().clone(), [(dir.path().join(".next/standalone/apps/web"), ".next".to_owned(), out.path().join(SEED_DIR))]);
        assert_eq!(b.seed.as_ref().map(|s| s.entries), Some(1));
        // The package: Next's server where Next put it, forkrail's entry beside it, and no env files.
        let names: Vec<&str> = b.entries.keys().map(String::as_str).collect();
        for expected in ["apps/web/forkrail-server.mjs", "apps/web/forkrail-next.cjs", "apps/web/server.js", "apps/web/node_modules/next/package.json", "apps/web/.next/server/pages/isr.html", "apps/web/.next/server/pages/isr.js"] {
            assert!(names.contains(&expected), "{expected} missing from {names:?}");
        }
        assert!(!names.iter().any(|n| n.contains(".env")));
        assert_eq!(b.manifest.entry, "apps/web/forkrail-server.mjs");
        let Source::File(entry) = &b.entries["apps/web/forkrail-server.mjs"].source else { panic!("the entry is a file") };
        let entry = std::fs::read_to_string(entry).unwrap();
        // N4's order: prime and the router's files' hook, then Next's server, then install.
        let at = |text: &str| entry.find(text).unwrap_or_else(|| panic!("{text} not in {entry}"));
        assert!(at("forkrail.prime()") < at("await import('./server.js')") && at("await import('./server.js')") < at("forkrail.install("));
        assert!(at("forkrail.routerFiles()") < at("await import('./server.js')"));
        // Beside Next's server, the files the router serves that the package doesn't hold: what a
        // rewrite onto one is answered with. Not an image Next's optimizer reads, nor a file the
        // middleware matches (the package holds both).
        let Source::File(listed) = &b.entries["apps/web/forkrail-router-files.txt"].source else { panic!("the list is a file") };
        let listed = std::fs::read_to_string(listed).unwrap();
        let listed: Vec<&str> = listed.lines().collect();
        for path in ["/shop/_next/static/chunks/main-abc.js", "/shop/robots.txt", "/shop/_next/static/service-worker/sw.js"] {
            assert!(listed.contains(&path), "{path} not in {listed:?}");
        }
        for path in ["/shop/logo.svg", "/shop/docs/secret.pdf", "/shop/_next/static/media/hero.123.png"] {
            assert!(!listed.contains(&path), "{path} in {listed:?}");
        }
        let Source::File(helper) = &b.entries["apps/web/forkrail-next.cjs"].source else { panic!("the helper is a file") };
        assert!(std::fs::read_to_string(helper).unwrap().contains("module.exports"));
        let logged = log.0.lock().unwrap().join("\n");
        assert!(logged.contains("warning: .env, .env.production isn't packaged"), "{logged}");
        assert!(logged.contains("shared cache: seeded with 1 pages"), "{logged}");
        assert!(!logged.contains("sharp"), "Next 15 brings sharp itself: {logged}");
        // Static files, at the paths they're served at: under the base path, and the hashed ones
        // under the asset prefix too. What the middleware matches stays with Next's server.
        let served = |rel: &str| b.static_dir.join(rel).is_file();
        assert!(served("shop/_next/static/chunks/main-abc.js") && served("cdn/_next/static/chunks/main-abc.js") && served("shop/logo.svg") && served("shop/robots.txt"));
        assert!(!served("shop/docs/secret.pdf"), "the middleware must run for it");
        assert_eq!(b.hashed, ["/shop/_next/static/", "/cdn/_next/static/"]);
        let files: BTreeMap<&str, &StaticFile> = b.manifest.static_files.iter().map(|f| (f.pathname.as_str(), f)).collect();
        assert_eq!((files["/shop/docs/secret.pdf"].router, files["/shop/docs/secret.pdf"].server), (false, true));
        assert_eq!((files["/shop/_next/static/chunks/main-abc.js"].immutable, files["/shop/_next/static/chunks/main-abc.js"].router, files["/shop/_next/static/chunks/main-abc.js"].server), (true, true, false));
        assert_eq!((files["/shop/robots.txt"].immutable, files["/shop/robots.txt"].server), (false, false));
        // A service worker keeps its name across builds: revalidated, not cached for good.
        assert!(!files["/shop/_next/static/service-worker/sw.js"].immutable && served("shop/_next/static/service-worker/sw.js"));
        assert_eq!(b.revalidated, ["/shop/_next/static/service-worker/", "/cdn/_next/static/service-worker/"]);
        // Images are in the package too: Next's optimizer reads them from its own server.
        assert!(files["/shop/logo.svg"].server && files["/shop/_next/static/media/hero.123.png"].server);
        assert!(names.contains(&"apps/web/public/logo.svg") && names.contains(&"apps/web/.next/static/media/hero.123.png") && names.contains(&"apps/web/public/docs/secret.pdf"));
        assert!(!names.contains(&"apps/web/public/robots.txt") && !names.contains(&"apps/web/.next/static/chunks/main-abc.js"));
        // The manifest is in the build folder; asked to, the package keeps the prerendered files.
        let written: Manifest = serde_json::from_slice(&std::fs::read(out.path().join(MANIFEST_FILE)).unwrap()).unwrap();
        assert_eq!(written, b.manifest);
        assert_eq!((written.prerender_files, written.app_dir.as_str(), written.base_path.as_str(), written.shared_cache), (Kept::Package, "apps/web", "/shop", true));
        // Before Next 16.2 Next hands over no outputs: no handlers file.
        assert!(written.handlers.is_none() && Handlers::read(out.path()).unwrap().is_none());
        assert_eq!(b.floor, Some(crate::state::Floor { memory_mb: None, max_duration_s: Some(90) }));
        assert_eq!((b.outline.functions.len(), b.outline.middleware[0].matcher.clone(), b.outline.middleware[0].edge), (2, Some(vec!["/docs/:path*".to_owned()]), true));
        assert_eq!(b.source_files, b.entries.len());
        // Left to the seed (the default): pages go out of the package, a fallback page stays in it.
        let seeded_out = project("next-collect-seed");
        let seeded = collect_with(dir.path(), None, seeded_out.path(), false, no_fetch, &seeder, &lines()).unwrap();
        assert_eq!(seeded.manifest.prerender_files, Kept::Seed);
        assert!(!seeded.entries.contains_key("apps/web/.next/server/pages/isr.html") && !seeded.entries.contains_key("apps/web/.next/server/pages/isr.json"));
        assert!(seeded.entries.contains_key("apps/web/.next/server/pages/posts/[id].html") && seeded.entries.contains_key("apps/web/.next/server/pages/isr.js"));
        // Nothing of the standalone folder is changed by it.
        assert_eq!(std::fs::read_to_string(dir.path().join(".next/standalone/apps/web/.next/server/pages/posts/[id].html")).unwrap(), "<html>fallback</html>");
        assert!(dir.path().join(".next/standalone/apps/web/.next/server/pages/isr.html").is_file());
        // A seed that fails leaves the pages in the package, and the build goes on.
        let failing = |_: &Path, _: &str, _: &Path| -> Result<crate::nextseed::Seeded> { bail!("no node") };
        let failed_log = lines();
        let failed = collect_with(dir.path(), None, project("next-collect-unseeded").path(), false, no_fetch, &failing, &failed_log).unwrap();
        assert!(failed.seed.is_none() && failed.manifest.prerender_files == Kept::Package && failed.entries.contains_key("apps/web/.next/server/pages/isr.html"));
        assert!(failed_log.0.lock().unwrap().iter().any(|l| l.starts_with("warning: the shared cache couldn't be seeded (no node)")));
        // Next 14 without sharp of the project's own: the router optimizes its images, so nothing
        // is said while the middleware leaves /_next/image alone. Without forkrail's handler in its
        // config there's no shared cache, nor a seed.
        write(dir.path(), &[("node_modules/next/package.json", r#"{"version": "14.2.35"}"#)]);
        let old = lines();
        let no_seed: Seeder = &|_, _, _| panic!("no seed without the shared cache");
        let unshared = collect_with(dir.path(), None, project("next-collect-14").path(), false, no_fetch, no_seed, &old).unwrap();
        assert!(!unshared.manifest.shared_cache && unshared.manifest.prerender_files == Kept::Package);
        let old = old.0.lock().unwrap();
        assert!(!old.iter().any(|l| l.contains("sharp")), "{old:?}");
        assert!(old.iter().any(|l| l.starts_with("warning: the build's config didn't take forkrail's cache handler")));
        drop(old);
        // A middleware that runs on every path: Next's own optimizer answers, and needs sharp.
        write(dir.path(), &[(".next/server/middleware-manifest.json", r#"{"version": 3, "middleware": {"/": {"matchers": [{"regexp": "^/.*$", "originalSource": "/:path*"}]}}, "functions": {}}"#)]);
        let every = lines();
        collect_with(dir.path(), None, project("next-collect-14-mw").path(), false, no_fetch, no_seed, &every).unwrap();
        assert!(every.0.lock().unwrap().iter().any(|l| l.starts_with("warning: the app's middleware runs on /shop/_next/image, so Next's own optimizer answers it")));
        write(dir.path(), &[("node_modules/next/package.json", r#"{"version": "15.5.29"}"#)]);
    }

    #[test]
    fn a_404_page_rendered_at_build_stays_in_the_package_though_paths_not_found_name_it() {
        // `pages/404.js` without getStaticProps: Next renders it to `pages/404.html` at build, and
        // each `notFound: true` path names that file. Next's server reads it for every 404.
        let dir = project("next-static-404");
        let dump = r#"{
          "format": 1, "nextVersion": "16.4.6", "buildId": "b1", "distDir": ".next", "config": {},
          "routing": {"beforeMiddleware": [], "beforeFiles": [], "afterFiles": [], "dynamicRoutes": [], "onMatch": [], "fallback": [], "shouldNormalizeNextData": false, "rsc": {}},
          "functions": [{"kind": "pages", "id": "/gone/[slug]", "pathname": "/gone/[slug]", "sourcePage": "/gone/[slug]", "runtime": "nodejs"}],
          "prerenders": [
            {"id": "/gone/first", "pathname": "/gone/first", "parentOutputId": "/gone/[slug]", "groupId": 1, "route": "/gone/[slug]", "routeType": "page", "fallback": {"file": ".next/server/pages/404.html", "initialRevalidate": 1, "initialStatus": 404}},
            {"id": "/found", "pathname": "/found", "parentOutputId": "/found", "groupId": 2, "route": "/found", "routeType": "page", "fallback": {"file": ".next/server/pages/found.html", "initialRevalidate": 1}}
          ]
        }"#;
        built(
            dir.path(),
            "",
            r#"{"distDir": ".next"}"#,
            &[
                (".next/forkrail-next.json", dump),
                (".next/prerender-manifest.json", &format!(r#"{{"version": 4, "routes": {{}}, "dynamicRoutes": {{}}, {PREVIEW}}}"#)),
                (".next/server/pages-manifest.json", r#"{"/404": "pages/404.html", "/gone/[slug]": "pages/gone/[slug].js", "/found": "pages/found.js"}"#),
                (".next/server/pages/404.html", "<html>404</html>"),
                (".next/server/pages/found.html", "<html>found</html>"),
                (".next/server/pages/found.js", "module.exports = 1"),
                (".next/server/pages/gone/[slug].js", "module.exports = 1"),
            ],
        );
        write(dir.path(), &[("node_modules/next/package.json", r#"{"version": "16.4.6"}"#)]);
        let server_js = "const path = require('path')\nconst nextConfig = {\"adapterPath\":\"/build/scratch/forkrail-next-adapter.cjs\",\"basePath\":\"\"}\n\nprocess.env.__NEXT_PRIVATE_STANDALONE_CONFIG = JSON.stringify(nextConfig)\n";
        write(&dir.path().join(".next/standalone"), &[("server.js", server_js)]);
        let no_fetch: FetchSharp = &|_, _, _| panic!("there's no sharp to swap");
        let seeder = |_: &Path, _: &str, into: &Path| {
            std::fs::create_dir_all(into).unwrap();
            std::fs::write(into.join("index.json"), r#"{"v":1,"built_ms":1,"entries":["ab"]}"#).unwrap();
            Ok(crate::nextseed::Seeded { entries: 2, bytes: 30, skipped: 0, built_ms: 1 })
        };
        let b = collect_with(dir.path(), None, project("next-static-404-out").path(), false, no_fetch, &seeder, &lines()).unwrap();
        assert_eq!((b.manifest.source, b.manifest.prerender_files), (From::Adapter, Kept::Seed));
        let gone = b.manifest.prerenders.iter().find(|p| p.pathname == "/gone/first").unwrap();
        assert_eq!(gone.files[0].file, ".next/server/pages/404.html");
        assert!(b.entries.contains_key(".next/server/pages/404.html"), "Next's server renders every 404 from it");
        assert!(!b.entries.contains_key(".next/server/pages/found.html"), "a page the cache has leaves the package");
        // Next's own server runs without the adapter's path; the build's file is left as it was.
        let Source::File(packaged) = &b.entries["server.js"].source else { panic!("server.js is a file") };
        let packaged = std::fs::read_to_string(packaged).unwrap();
        assert!(!packaged.contains("adapterPath") && packaged.contains(r#"const nextConfig = {"basePath":""}"#), "{packaged}");
        assert_eq!(std::fs::read_to_string(dir.path().join(".next/standalone/server.js")).unwrap(), server_js);
    }

    #[test]
    fn nexts_own_server_runs_without_the_adapters_path() {
        let js = "const path = require('path')\n\nlet keepAliveTimeout = parseInt(process.env.KEEP_ALIVE_TIMEOUT, 10)\nconst nextConfig = {\"env\":{},\"experimental\":{\"adapterPath\":\"/tmp/b/a.cjs\",\"ppr\":false},\"basePath\":\"/docs\"}\n\nprocess.env.__NEXT_PRIVATE_STANDALONE_CONFIG = JSON.stringify(nextConfig)\n";
        // 16.0 and 16.1 keep it under `experimental`, 16.2 and later at the top.
        let out = server_config(js).unwrap();
        assert!(!out.contains("adapterPath") && out.contains(r#""ppr":false"#) && out.contains(r#""basePath":"/docs""#), "{out}");
        assert!(out.starts_with("const path = require('path')\n\nlet keepAliveTimeout") && out.ends_with("}\n\nprocess.env.__NEXT_PRIVATE_STANDALONE_CONFIG = JSON.stringify(nextConfig)\n"), "{out}");
        let top = js.replace(r#""experimental":{"adapterPath":"/tmp/b/a.cjs","ppr":false}"#, r#""adapterPath":"/tmp/b/a.cjs""#);
        assert!(!server_config(&top).unwrap().contains("adapterPath"));
        // Nothing to take (15.5's config, a build without the adapter), or no config found: as it is.
        assert_eq!(server_config(&js.replace(r#""adapterPath":"/tmp/b/a.cjs","#, "")), None);
        assert_eq!(server_config("require('next')"), None);
    }

    #[test]
    fn client_reference_manifests_are_packaged_with_their_lists_once() {
        // As Turbopack writes them in a big app: every client module repeats its group's chunks.
        let chunks = format!("[{}]", (0..80).map(|i| format!("\"server/chunks/ssr/p_{i:04}._.js\"")).collect::<Vec<_>>().join(","));
        let modules: Vec<String> = (0..1500).map(|n| format!(r#""[project]/m{n}.js":{{"id":{n},"name":"*","chunks":{chunks},"async":false}}"#)).collect();
        let big = format!(
            "globalThis.__RSC_MANIFEST = globalThis.__RSC_MANIFEST || {{}};\nglobalThis.__RSC_MANIFEST[\"/docs/page\"] = {{\"moduleLoading\":{{\"prefix\":\"\"}},\"clientModules\":{{{}}},\"ssrModuleMapping\":{{}},\"entryCSSFiles\":{{}}}};\n",
            modules.join(",")
        );
        let dir = project("next-rsc");
        built(dir.path(), "apps/web", r#"{"distDir": ".next"}"#, &[(".next/server/app/docs/page_client-reference-manifest.js", &big)]);
        let log = lines();
        let no_fetch: FetchSharp = &|_, _, _| panic!("there's no sharp to swap");
        let no_seed: Seeder = &|_, _, _| bail!("no seed here");
        let b = collect_with(dir.path(), None, project("next-rsc-out").path(), false, no_fetch, no_seed, &log).unwrap();
        let name = "apps/web/.next/server/app/docs/page_client-reference-manifest.js";
        let Source::File(packaged) = &b.entries[name].source else { panic!("a file") };
        let standalone = dir.path().join(".next/standalone").join(name);
        assert!(!packaged.starts_with(dir.path()), "the rewrite, not the build's file, is packaged");
        assert!(std::fs::metadata(packaged).unwrap().len() < std::fs::metadata(&standalone).unwrap().len() / 10);
        assert_eq!(std::fs::read_to_string(&standalone).unwrap(), big, "the build's file is as Next wrote it");
        assert!(log.0.lock().unwrap().iter().any(|l| l.starts_with("client-reference manifests: 1 files")));
    }

    #[test]
    fn hashed_files_are_where_an_asset_prefix_asks_for_them() {
        assert_eq!(asset_path("/cdn/").as_deref(), Some("/cdn"));
        // N6's `asset-prefix-absolute`: the page asks the deployment for the URL's path.
        assert_eq!(asset_path("https://example.vercel.sh/custom-asset-prefix").as_deref(), Some("/custom-asset-prefix"));
        assert_eq!(asset_path("//cdn.example.com/a/b/").as_deref(), Some("/a/b"));
        assert_eq!(asset_path("https://cdn.example.com"), None);
        assert_eq!(asset_path("https://cdn.example.com/"), None);
        assert_eq!(asset_path(""), None);
        assert_eq!(asset_path("/"), None);
    }

    #[test]
    fn a_file_named_twice_stays_whole() {
        let dir = project("next-link");
        write(dir.path(), &[("a/404.html", "<html>404</html>"), ("other.html", "other")]);
        let (from, to) = (dir.path().join("a/404.html"), dir.path().join("out/x/404.html"));
        // Paths that aren't found each name the 404 page's file: put in place once, it must not
        // be copied over itself (which emptied it, and the page in the package with it).
        link_or_copy(&from, &to).unwrap();
        link_or_copy(&from, &to).unwrap();
        assert_eq!(std::fs::read_to_string(&from).unwrap(), "<html>404</html>");
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "<html>404</html>");
        // The first file at a path stays.
        link_or_copy(&dir.path().join("other.html"), &to).unwrap();
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "<html>404</html>");
    }

    #[test]
    fn builds_that_arent_a_standalone_server_say_so() {
        // A static export has no server to package: it deploys as a static site (next_export.rs),
        // once Next has noted where it exported to.
        let dir = project("next-export");
        built(dir.path(), "", r#"{"distDir": ".next", "output": "export"}"#, &[]);
        let error = check(dir.path(), DIST).unwrap_err().to_string();
        assert!(error.contains("static export") && error.contains("export-detail.json"), "{error}");
        write(dir.path(), &[(".next/export-detail.json", &format!(r#"{{"version": 1, "outDirectory": {:?}, "success": true}}"#, dir.path().join("out").to_string_lossy())), ("out/index.html", "<p>")]);
        check(dir.path(), DIST).unwrap();
        assert!(read_dist(dir.path(), DIST).err().unwrap().to_string().contains("deployed as a static site"));
        // A build without the standalone folder (made elsewhere, `--prebuilt`).
        let plain = project("next-plain");
        built(plain.path(), "", r#"{"distDir": ".next"}"#, &[]);
        std::fs::remove_dir_all(plain.path().join(".next/standalone")).unwrap();
        assert!(check(plain.path(), DIST).unwrap_err().to_string().contains("output: 'standalone'"));
        // Nothing built.
        assert!(check(project("next-nothing").path(), DIST).unwrap_err().to_string().contains("next build"));
        // `distDir` elsewhere is found by what a build leaves.
        let custom = project("next-distdir");
        built(custom.path(), "", r#"{"distDir": "build/next"}"#, &[]);
        std::fs::create_dir_all(custom.path().join("build")).unwrap();
        std::fs::rename(custom.path().join(".next"), custom.path().join("build/next")).unwrap();
        assert_eq!(find_dist(custom.path(), DIST), Some(custom.path().join("build/next")));
        assert_eq!(read_dist(custom.path(), DIST).unwrap().name, "build/next");
        // Deeper than the search goes (an Nx build's `dist/apps/web/.next`): found where
        // vercel.json's `outputDirectory` names it, as Vercel's builder finds it.
        let nx = project("next-nx");
        built(nx.path(), "", r#"{"distDir": ".next"}"#, &[]);
        std::fs::create_dir_all(nx.path().join("dist/apps/web")).unwrap();
        std::fs::rename(nx.path().join(".next"), nx.path().join("dist/apps/web/.next")).unwrap();
        assert_eq!(find_dist(nx.path(), DIST), None);
        assert_eq!(find_dist(nx.path(), "dist/apps/web/.next"), Some(nx.path().join("dist/apps/web/.next")));
        assert_eq!(read_dist(nx.path(), "dist/apps/web/.next").unwrap().name, "dist/apps/web/.next");
    }

    #[test]
    fn a_package_over_lambdas_limit_is_refused_with_what_fills_it() {
        let dir = project("next-big");
        built(dir.path(), "", r#"{"distDir": ".next"}"#, &[]);
        let dist = read_dist(dir.path(), DIST).unwrap();
        // Files of a size, with nothing written: 200 MB of pages and 100 MB of a package.
        let sized = |name: &str, mb: u64| {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::File::create(&path).unwrap().set_len(mb << 20).unwrap();
            Entry::file(path)
        };
        let mut entries = BTreeMap::new();
        entries.insert(".next/server/app/docs/page_client-reference-manifest.js".to_owned(), sized("a", 200));
        entries.insert("node_modules/big/index.node".to_owned(), sized("b", 40));
        check_size(&entries, &dist, 0, false).unwrap();
        entries.insert("node_modules/bigger/index.node".to_owned(), sized("c", 60));
        let error = check_size(&entries, &dist, 150 << 20, false).unwrap_err().to_string();
        assert!(error.contains("300 MB unzipped, over the 250 MB a Lambda function's package may hold"), "{error}");
        assert!(error.contains("node_modules 100 MB") && error.contains(".next/server 200 MB (prerendered pages 150 MB of it)"), "{error}");
        assert!(error.contains("page_client-reference-manifest.js (200 MB)") && error.contains("container images"), "{error}");
    }

    #[test]
    fn linux_sharp_goes_where_the_build_machines_is() {
        assert_eq!(sharp_binary("node_modules/@img/sharp-darwin-arm64/lib/sharp-darwin-arm64.node"), Some(("node_modules/@img/", false, "darwin-arm64", "lib/sharp-darwin-arm64.node")));
        assert_eq!(sharp_binary("a/@img/sharp-libvips-linux-x64"), Some(("a/@img/", true, "linux-x64", "")));
        assert_eq!(sharp_binary("node_modules/@img/sharp-wasm32/package.json"), None);
        assert_eq!(sharp_binary("node_modules/@img/colour/index.js"), None);
        let dir = project("next-sharp");
        // What npm would fetch.
        write(dir.path(), &[("fetched/sharp-linux-arm64/package.json", "{}"), ("fetched/sharp-linux-arm64/lib/sharp-linux-arm64.node", "elf"), ("fetched/sharp-libvips-linux-arm64/lib/libvips.so", "elf")]);
        write(dir.path(), &[("mac/package.json", r#"{"version": "0.35.4"}"#), ("mac/sharp.node", "macho"), ("vips/package.json", r#"{"version": "1.3.3"}"#)]);
        let file = |name: &str| Entry::file(dir.path().join(name));
        let link = |target: &str| Entry { source: Source::Link(target.to_owned()), modified: None };
        // pnpm's layout: the real folders under `.pnpm`, links to them beside sharp.
        let mut entries: BTreeMap<String, Entry> = [
            ("node_modules/.pnpm/@img+sharp-darwin-arm64@0.35.4/node_modules/@img/sharp-darwin-arm64/package.json", file("mac/package.json")),
            ("node_modules/.pnpm/@img+sharp-darwin-arm64@0.35.4/node_modules/@img/sharp-darwin-arm64/lib/sharp-darwin-arm64.node", file("mac/sharp.node")),
            ("node_modules/.pnpm/@img+sharp-libvips-darwin-arm64@1.3.3/node_modules/@img/sharp-libvips-darwin-arm64/package.json", file("vips/package.json")),
            ("node_modules/.pnpm/sharp@0.35.4/node_modules/@img/sharp-darwin-arm64", link("../../../@img+sharp-darwin-arm64@0.35.4/node_modules/@img/sharp-darwin-arm64")),
            ("node_modules/.pnpm/sharp@0.35.4/node_modules/@img/sharp-libvips-darwin-arm64", link("../../../@img+sharp-libvips-darwin-arm64@1.3.3/node_modules/@img/sharp-libvips-darwin-arm64")),
            ("node_modules/.pnpm/sharp@0.35.4/node_modules/sharp/lib/index.js", file("mac/package.json")),
            ("node_modules/@img/sharp-wasm32/package.json", file("mac/package.json")),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        let asked = std::sync::Mutex::new(Vec::new());
        let fetch: FetchSharp = &|package, version, _| {
            asked.lock().unwrap().push(format!("{package}@{version}"));
            Ok(dir.path().join("fetched").join(package))
        };
        fix_sharp(&mut entries, fetch, &lines());
        assert_eq!(*asked.lock().unwrap(), ["sharp-linux-arm64@0.35.4", "sharp-libvips-linux-arm64@1.3.3"], "each at the version the project has");
        let names: Vec<&str> = entries.keys().map(String::as_str).collect();
        assert!(!names.iter().any(|n| n.contains("darwin-arm64/") || n.ends_with("darwin-arm64")), "{names:#?}");
        assert!(names.contains(&"node_modules/.pnpm/@img+sharp-darwin-arm64@0.35.4/node_modules/@img/sharp-linux-arm64/lib/sharp-linux-arm64.node"));
        assert!(names.contains(&"node_modules/.pnpm/@img+sharp-libvips-darwin-arm64@1.3.3/node_modules/@img/sharp-libvips-linux-arm64/lib/libvips.so"));
        assert_eq!(entries["node_modules/.pnpm/sharp@0.35.4/node_modules/@img/sharp-linux-arm64"].source, Source::Link("../../../@img+sharp-darwin-arm64@0.35.4/node_modules/@img/sharp-linux-arm64".to_owned()));
        assert!(names.contains(&"node_modules/@img/sharp-wasm32/package.json") && names.contains(&"node_modules/.pnpm/sharp@0.35.4/node_modules/sharp/lib/index.js"));
        // A fetch that fails is warned of and leaves the build going.
        let mut entries: BTreeMap<String, Entry> = [("node_modules/@img/sharp-darwin-arm64/package.json".to_owned(), file("mac/package.json"))].into();
        let offline: FetchSharp = &|_, _, _| bail!("npm install failed: offline");
        let log = lines();
        fix_sharp(&mut entries, offline, &log);
        assert!(log.0.lock().unwrap().iter().any(|l| l.starts_with("warning: sharp for Linux arm64 couldn't be put in the package") && l.contains("offline")));
        // Linux arm64's own (the builder's install) is left as it is.
        let mut entries: BTreeMap<String, Entry> = [("node_modules/@img/sharp-linux-arm64/package.json".to_owned(), file("mac/package.json"))].into();
        fix_sharp(&mut entries, &|_, _, _| panic!("nothing to fetch"), &lines());
        assert_eq!(entries.len(), 1);
    }
}
