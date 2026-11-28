//! Frameworks, as Vercel deploys them: forkrail runs the project's install and build, then takes
//! what the build left.
//!
//! - **Servers built with Nitro** (TanStack Start, Nuxt, SolidStart, plain Nitro), as on Vercel:
//!   Nitro picks its `vercel` preset by itself (the build has `VERCEL=1`), whose Build Output is
//!   imported (build_output.rs). A project that pins another preset keeps it, as Nitro decides;
//!   one that pins `node-server` gets that output run as one server (`.output/server/index.mjs`,
//!   a server that listens on `PORT`, and `.output/public/` for the router to serve), as forkrail
//!   ran every Nitro app before.
//! - **Static sites** (Vercel's static presets: Vite, Create React App, Vue CLI, Angular,
//!   Docusaurus, VitePress, Eleventy, Gatsby, Hexo, Parcel, Storybook and the rest of `PRESETS`;
//!   Astro and SvelteKit when they build static files): the build's output folder is the whole
//!   deployment, with Vercel's default folder for each, or `outputDirectory`.
//! - **Anything else with a `build` script** ("Other" on Vercel): built, then `public/` if the
//!   build left one, else the project itself, is served.
//!
//! - **Next.js** (13.5 and later): its own standalone server, packaged as Next wrote it (next.rs);
//!   a static export's folder as a static site (next_export.rs).
//!
//! Frameworks known not to work yet are refused by name rather than deployed half-working.
//!
//! An API has no build, but the web framework it's written with is found too (`api`), only to be
//! recorded with the deployment.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::buildlog::{self, BuildLog};
use crate::package;
use crate::vercel::{Language, Spec};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Framework {
    /// For the user: `TanStack Start`, `Vite`.
    pub name: &'static str,
    /// Vercel's install command for the project's package manager, `npm install`, or `vercel.json`'s
    /// `installCommand`.
    pub install: String,
    /// The build, `npm run build`, or `vercel.json`'s `buildCommand`. None when there's nothing to
    /// build.
    pub build: Option<String>,
    /// The build's output, relative to the project: `.output`, `dist`.
    pub output: String,
    /// The server, relative to `output`: what the runtime starts. Empty for a static site.
    pub entry: &'static str,
    /// Static files, relative to `output`, served at the site's root ("" for the output itself).
    pub public: &'static str,
    /// Set for the build, beside Vercel's build variables. None so far: Nitro picks its output
    /// format by where it builds, and with `VERCEL=1` that is Vercel's, as on Vercel.
    pub build_env: &'static [(&'static str, &'static str)],
    /// Set for the install command only: Vercel installs a Create React App project with
    /// `CI=false` (its build still sees `CI=1`).
    pub install_env: &'static [(&'static str, &'static str)],
    /// Vercel's `envPrefix` for the framework (`VITE_`): the build sees the deployment's system
    /// variables under it too (`prefixed_env`). Empty for none.
    pub env_prefix: &'static str,
    /// Folders the build cache keeps besides `node_modules` (Vercel's `cachePattern`), relative to
    /// the project.
    pub cache: &'static [&'static str],
    /// Where the output is when the folder alone doesn't say (`Output::Fixed` otherwise).
    pub find: Output,
    /// Folders whose files' names change with their contents, served to be cached for good.
    pub hashed: &'static [&'static str],
    /// The routes Vercel's preset adds around the project's own (`defaultRoutes` in
    /// `@vercel/frameworks`), as JSON in `routes` form; empty for none.
    pub routes: &'static str,
    /// Variables of this machine the build command never sees (Next's: what makes its CLI take
    /// the build for an AI agent's session, next.rs).
    pub unset: &'static [&'static str],
}

/// How a build's output is found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// Exactly `output`.
    Fixed,
    /// Angular's, as Vercel finds it: when `dist` holds one thing and it's a folder,
    /// `dist/<project>/browser` (17 and later) if there is one, else `dist/<project>`; otherwise
    /// `dist`.
    Angular,
    /// Docusaurus 1's: the one folder inside `output` when that's all the build left there (it
    /// builds into `build/<project>`), else `output` itself.
    Docusaurus,
    /// Polymer's: the first folder in `output`, by name, without a dot in its name (one per build
    /// `polymer.json` names, `build/default`), else `output` itself.
    Polymer,
    /// Vercel's "Other": `public/` if the build left one, else the project itself.
    PublicOrRoot,
    /// Vercel's Build Output (`.vercel/output`), which the framework's own Vercel adapter writes:
    /// its routes, functions and static files are imported as they are (build_output.rs).
    BuildOutput,
    /// Next.js's standalone server in its dist folder (`.next/standalone`, or under the `distDir`
    /// the project names): packaged as Next wrote it (next.rs).
    Next,
}

/// Nitro's `node-server` output, whichever framework drives it: what a project that pins that
/// preset deploys (`Framework::nitro_server`).
const NITRO_OUTPUT: &str = ".output";
const NITRO_ENTRY: &str = "server/index.mjs";
const NITRO_PUBLIC: &str = "public";
/// What Vite (`/assets/`), Nuxt (`/_nuxt/`) and SolidStart (`/_build/assets/`) write with hashed
/// names, and Nitro serves with a year's `max-age` (on Vercel too, through its own routes).
const NITRO_HASHED: &[&str] = &["/assets/", "/_nuxt/", "/_build/assets/"];
/// Variables that make Nitro pick a preset of the project's choosing over the one for where it
/// builds (nitropack and nitro read both; `NITRO_PRESET` first).
const NITRO_PRESET_ENV: [&str; 2] = ["NITRO_PRESET", "SERVER_PRESET"];
/// CI services std-env (which Nitro asks where it builds) finds before Vercel, by the variable it
/// looks for: one of these beside `VERCEL=1` makes Nitro build for that service, `node-server`
/// for most (seen with GITHUB_ACTIONS, docs/data/c16/nitro-notes.md). std-env 4.3.0's list
/// (`dist/index.mjs`), in its order, up to Vercel's entry; only to say why, in a warning.
const CI_BEFORE_VERCEL: [&str; 41] = [
    "APPVEYOR", "AWS_APP_ID", "SYSTEM_TEAMFOUNDATIONCOLLECTIONURI", "INPUT_AZURE_STATIC_WEB_APPS_API_TOKEN", "AC_APPCIRCLE",
    "bamboo_planKey", "BITBUCKET_COMMIT", "BITRISE_IO", "BUDDY_WORKSPACE_ID", "BUILDKITE", "CIRCLECI", "CIRRUS_CI", "CF_PAGES",
    "WORKERS_CI", "K_SERVICE", "CLOUD_RUN_JOB", "CODEBUILD_BUILD_ARN", "CF_BUILD_ID", "DRONE", "DRONE_BUILD_EVENT", "DSARI",
    "GITHUB_ACTIONS", "GITLAB_CI", "CI_MERGE_REQUEST_ID", "GO_PIPELINE_LABEL", "LAYERCI", "JENKINS_URL", "HUDSON_URL", "MAGNUM",
    "NETLIFY", "NETLIFY_LOCAL", "NEVERCODE", "RENDER", "SAILCI", "SEMAPHORE", "SCREWDRIVER", "SHIPPABLE", "TDDIUM", "STRIDER",
    "TEAMCITY_VERSION", "TRAVIS",
];

/// Dependencies that mean a framework built with Nitro, and what to call it.
const NITRO_FRAMEWORKS: [(&str, &str); 6] = [
    ("nuxt", "Nuxt"),
    ("@tanstack/react-start", "TanStack Start"),
    ("@tanstack/solid-start", "TanStack Start"),
    ("@solidjs/start", "SolidStart"),
    ("nitro", "Nitro"),
    ("nitropack", "Nitro"),
];

/// A static site's framework: the dependency that means it (or what `DETECTORS` says, for the
/// presets in it), its name, Vercel's slug for it, its build when `package.json` has no `build`
/// script (Vercel's command, run with the project's `node_modules/.bin` on `PATH`), and its output
/// folder. In Vercel's order (`@vercel/frameworks` 3.35.0, the list `vercel build` 65.16 detects
/// with): the first one found is the project's, so an Ember or Polymer site that also lists Vite is
/// what Vercel calls it, and so is a VitePress site that lists Vite (Vercel looks for its `dist`).
struct Preset {
    dependency: &'static str,
    name: &'static str,
    slugs: &'static [&'static str],
    build: &'static str,
    output: &'static str,
    /// Vercel's `defaultRoutes` for it (`@vercel/frameworks` 3.35.0), or "".
    routes: &'static str,
}

const PRESETS: [Preset; 33] = [
    Preset { dependency: "gatsby", name: "Gatsby", slugs: &["gatsby"], build: "gatsby build", output: "public", routes: GATSBY_ROUTES },
    Preset { dependency: "astro", name: "Astro", slugs: &["astro"], build: "astro build", output: "dist", routes: ASTRO_ROUTES },
    Preset { dependency: "hexo", name: "Hexo", slugs: &["hexo"], build: "hexo generate", output: "public", routes: "" },
    Preset { dependency: "@11ty/eleventy", name: "Eleventy", slugs: &["eleventy"], build: "npx @11ty/eleventy", output: "_site", routes: "" },
    Preset { dependency: "@docusaurus/core", name: "Docusaurus", slugs: &["docusaurus-2"], build: "docusaurus build", output: "build", routes: DOCUSAURUS_ROUTES },
    Preset { dependency: "docusaurus", name: "Docusaurus (v1)", slugs: &["docusaurus"], build: "docusaurus-build", output: "build", routes: "" },
    Preset { dependency: "preact-cli", name: "Preact", slugs: &["preact"], build: "preact build", output: "build", routes: SPA_ROUTES },
    Preset { dependency: "@dojo/framework", name: "Dojo", slugs: &["dojo"], build: "dojo build", output: "output/dist", routes: DOJO_ROUTES },
    Preset { dependency: "ember-source", name: "Ember.js", slugs: &["ember"], build: "ember build", output: "dist", routes: SPA_ROUTES },
    Preset { dependency: "@vue/cli-service", name: "Vue CLI", slugs: &["vue"], build: "vue-cli-service build", output: "dist", routes: VUE_ROUTES },
    // Vercel's `getOutputDirName` says `dist/static`, where Scully writes the pages it rendered
    // (its settings' `dist` is only what the dashboard shows).
    Preset { dependency: "@scullyio/init", name: "Scully", slugs: &["scully"], build: "ng build && scully", output: "dist/static", routes: "" },
    Preset { dependency: "@ionic/angular", name: "Ionic Angular", slugs: &["ionic-angular"], build: "ng build", output: "www", routes: SPA_ROUTES },
    Preset { dependency: "@angular/cli", name: "Angular", slugs: &["angular"], build: "ng build", output: "dist", routes: SPA_ROUTES },
    Preset { dependency: "polymer-cli", name: "Polymer", slugs: &["polymer"], build: "polymer build", output: "build", routes: SPA_ROUTES },
    Preset { dependency: "@sveltejs/kit", name: "SvelteKit", slugs: &["sveltekit", "sveltekit-1"], build: "vite build", output: "build", routes: "" },
    Preset { dependency: "@ionic/react", name: "Ionic React", slugs: &["ionic-react"], build: "react-scripts build", output: "build", routes: CRA_ROUTES },
    Preset { dependency: "react-scripts", name: "Create React App", slugs: &["create-react-app"], build: "react-scripts build", output: "build", routes: CRA_ROUTES },
    Preset { dependency: "gridsome", name: "Gridsome", slugs: &["gridsome"], build: "gridsome build", output: "dist", routes: "" },
    Preset { dependency: "umi", name: "UmiJS", slugs: &["umijs"], build: "umi build", output: "dist", routes: SPA_ROUTES },
    Preset { dependency: "saber", name: "Saber", slugs: &["saber"], build: "saber build", output: "public", routes: SABER_ROUTES },
    Preset { dependency: "@stencil/core", name: "Stencil", slugs: &["stencil"], build: "stencil build", output: "www", routes: STENCIL_ROUTES },
    // Hugo, Jekyll, Middleman and Zola are found by their config files, without a package.json;
    // only Hugo builds here (hugo.rs), the others are refused by name (`REFUSED`).
    Preset { dependency: "", name: "Hugo", slugs: &["hugo"], build: "hugo --gc", output: "public", routes: "" },
    Preset { dependency: "", name: "Jekyll", slugs: &["jekyll"], build: "jekyll build", output: "_site", routes: "" },
    Preset { dependency: "brunch", name: "Brunch", slugs: &["brunch"], build: "brunch build --production", output: "public", routes: "" },
    Preset { dependency: "", name: "Middleman", slugs: &["middleman"], build: "bundle exec middleman build", output: "build", routes: "" },
    Preset { dependency: "", name: "Zola", slugs: &["zola"], build: "zola build", output: "public", routes: "" },
    Preset { dependency: "vite", name: "Vite", slugs: &["vite"], build: "vite build", output: "dist", routes: "" },
    Preset { dependency: "vitepress", name: "VitePress", slugs: &["vitepress"], build: "vitepress build docs", output: "docs/.vitepress/dist", routes: "" },
    Preset { dependency: "vuepress", name: "VuePress", slugs: &["vuepress"], build: "vuepress build src", output: "src/.vuepress/dist", routes: "" },
    Preset { dependency: "parcel", name: "Parcel", slugs: &["parcel"], build: "parcel build", output: "dist", routes: PARCEL_ROUTES },
    Preset { dependency: "sanity", name: "Sanity", slugs: &["sanity"], build: "sanity build", output: "dist", routes: SPA_ROUTES },
    Preset { dependency: "@sanity/cli", name: "Sanity (v2)", slugs: &["sanity-v2"], build: "sanity build", output: "dist", routes: SPA_ROUTES },
    // Storybook's build is always its own (`ignorePackageJsonScript`): a project's `build` script
    // builds the app its stories are about.
    Preset { dependency: "storybook", name: "Storybook", slugs: &["storybook"], build: "storybook build", output: "storybook-static", routes: "" },
];

/// What a project must have for Vercel to find a framework, when it's more than the preset's one
/// dependency (its `detectors` in `@vercel/frameworks`): everything in `every`, and when `some`
/// names anything, at least one of those. The other presets are found by `dependency` alone.
struct Detectors {
    every: &'static [Has],
    some: &'static [Has],
}

#[derive(Clone, Copy)]
enum Has {
    /// A dependency or devDependency.
    Package(&'static str),
    /// A dependency whose version, but for a leading `^`, starts with this.
    PackageVersion(&'static str, &'static str),
    /// A file (or folder) in the project.
    File(&'static str),
    /// A file in the project that says this somewhere.
    FileSays(&'static str, &'static str),
}

/// By preset name.
const DETECTORS: [(&str, Detectors); 10] = [
    ("Dojo", Detectors { every: &[], some: &[Has::Package("@dojo/framework"), Has::File(".dojorc")] }),
    ("Ember.js", Detectors { every: &[], some: &[Has::Package("ember-source"), Has::Package("ember-cli")] }),
    ("Create React App", Detectors { every: &[], some: &[Has::Package("react-scripts"), Has::Package("react-dev-utils")] }),
    // Not `hugo.toml`, Hugo's own name for it since 0.110: Vercel doesn't look there.
    (
        "Hugo",
        Detectors {
            every: &[],
            some: &[Has::FileSays("config.yaml", "baseURL"), Has::FileSays("config.toml", "baseURL"), Has::FileSays("config.json", "baseURL")],
        },
    ),
    ("Jekyll", Detectors { every: &[Has::File("_config.yml")], some: &[] }),
    ("Middleman", Detectors { every: &[Has::File("config.rb")], some: &[] }),
    ("Zola", Detectors { every: &[Has::FileSays("config.toml", "base_url")], some: &[] }),
    ("Brunch", Detectors { every: &[], some: &[Has::Package("brunch"), Has::File("brunch-config.js")] }),
    (
        "Sanity",
        Detectors {
            every: &[Has::Package("sanity")],
            some: &[Has::File("sanity.config.js"), Has::File("sanity.config.jsx"), Has::File("sanity.config.ts"), Has::File("sanity.config.tsx")],
        },
    ),
    ("Sanity (v2)", Detectors { every: &[Has::PackageVersion("@sanity/cli", "2.")], some: &[Has::File("sanity.json")] }),
];

/// Presets that give way to another found in the same project, whatever the order (Vercel's
/// `supersedes`): an Ionic React app built with Vite is a Vite site (Ionic React's preset builds
/// with react-scripts).
const SUPERSEDES: [(&str, &str); 1] = [("Vite", "Ionic React")];

// Vercel's routes for the presets, copied from `@vercel/frameworks` 3.35.0. They go around the
// project's own as Vercel's `mergeRoutes` puts a builder's (`rules::Program::from_vercel`): an
// Angular, Vue CLI or Create React App site answers any path with its `index.html`, after the
// project's own rewrites.
const DOCUSAURUS_ROUTES: &str = r#"[
  {"src": "^/[^./]+\\.[0-9a-f]{8}\\.(css|js)$", "headers": {"cache-control": "max-age=31536000, immutable"}, "continue": true},
  {"src": "^/assets/images/[^/]+-[0-9a-f]{32}\\.(ico|svg|jpg|jpeg|png|gif|webp)$", "headers": {"cache-control": "max-age=31536000, immutable"}, "continue": true},
  {"src": "^/assets/medias/[^/]+-[0-9a-f]{32}\\.(ogv|wav|mp3|m4a|aac|oga|flac)$", "headers": {"cache-control": "max-age=31536000, immutable"}, "continue": true},
  {"src": "^/assets/files/[^/]+-[0-9a-f]{32}\\.(pdf|doc|docx|xls|xlsx|zip|rar)$", "headers": {"cache-control": "max-age=31536000, immutable"}, "continue": true},
  {"src": "^/ideal-img/[^/]+\\.[0-9a-f]{7}\\.\\d+\\.(png|jpe?g|gif)$", "headers": {"cache-control": "max-age=31536000, immutable"}, "continue": true},
  {"handle": "filesystem"},
  {"src": ".*", "status": 404, "dest": "404.html"}
]"#;
/// Gatsby's routes when its build leaves no `__now_routes_g4t5bY.json` (that file, from the old
/// `gatsby-plugin-now`, isn't read).
const GATSBY_ROUTES: &str = r#"[
  {"src": "^/static/(.*)$", "headers": {"cache-control": "public,max-age=31536000,immutable"}, "continue": true},
  {"src": "^/.*\\.(js|css)$", "headers": {"cache-control": "public,max-age=31536000,immutable"}, "continue": true},
  {"src": "^/(sw\\.js|app-data\\.json|.*\\.html|page-data/.*)$", "headers": {"cache-control": "public,max-age=0,must-revalidate"}, "continue": true},
  {"handle": "filesystem"},
  {"src": ".*", "status": 404, "dest": "404.html"}
]"#;
const ASTRO_ROUTES: &str = r#"[
  {"src": "^/assets/(.*)$", "headers": {"cache-control": "public, max-age=31536000, immutable"}, "continue": true},
  {"src": "^/_astro/(.*)$", "headers": {"cache-control": "public, max-age=31536000, immutable"}, "continue": true}
]"#;
/// A single-page app's: any path its files don't have gets `index.html` (Angular, Ionic Angular,
/// Preact, Ember, Polymer, UmiJS, Sanity).
const SPA_ROUTES: &str = r#"[
  {"handle": "filesystem"},
  {"src": "/(.*)", "dest": "/index.html"}
]"#;
/// Dojo's: as a single-page app, with its service worker never cached, once the filesystem has
/// been looked at.
const DOJO_ROUTES: &str = r#"[
  {"handle": "filesystem"},
  {"src": "/service-worker.js", "headers": {"cache-control": "s-maxage=0"}, "continue": true},
  {"src": "/(.*)", "dest": "/index.html"}
]"#;
/// Saber's: its `/_saber/` files are cached for good (a route that ends routing there, without
/// `continue`, as Vercel wrote it), and a missing path gets `404.html`.
const SABER_ROUTES: &str = r#"[
  {"src": "/_saber/.*", "headers": {"cache-control": "max-age=31536000, immutable"}},
  {"handle": "filesystem"},
  {"src": ".*", "status": 404, "dest": "404.html"}
]"#;
const STENCIL_ROUTES: &str = r#"[
  {"src": "/assets/(.*)", "headers": {"cache-control": "max-age=2592000"}, "continue": true},
  {"src": "/build/p-.*", "headers": {"cache-control": "max-age=31536000, immutable"}, "continue": true},
  {"src": "/sw.js", "headers": {"cache-control": "no-cache"}, "continue": true},
  {"handle": "filesystem"},
  {"src": "/(.*)", "dest": "/index.html"}
]"#;
const PARCEL_ROUTES: &str = r#"[
  {"src": "^/[^./]+\\.[0-9a-f]{8}\\.(css|js|png|jpg|webp|avif|svg)$", "headers": {"cache-control": "s-maxage=31536000, immutable"}, "continue": true},
  {"handle": "filesystem"}
]"#;
/// Create React App's, and Ionic React's (the same in Vercel's list).
const CRA_ROUTES: &str = r#"[
  {"src": "/static/(.*)", "headers": {"cache-control": "s-maxage=31536000, immutable"}, "continue": true},
  {"src": "/service-worker.js", "headers": {"cache-control": "s-maxage=0"}, "continue": true},
  {"src": "/sockjs-node/(.*)", "dest": "/sockjs-node/$1"},
  {"handle": "filesystem"},
  {"src": "/static/(.*)", "status": 404, "dest": "/404.html"},
  {"src": "/(.*)", "headers": {"cache-control": "s-maxage=0"}, "dest": "/index.html"}
]"#;
const VUE_ROUTES: &str = r#"[
  {"src": "^/[^/]*\\.(js|txt|ico|json)", "headers": {"cache-control": "max-age=300"}, "continue": true},
  {"src": "^/(img|js|css|fonts|media)/[^/]+\\.[0-9a-f]{8}\\.*", "headers": {"cache-control": "max-age=31536000, immutable"}, "continue": true},
  {"handle": "filesystem"},
  {"src": "^.*", "dest": "/index.html"}
]"#;

/// Vercel's slugs for the Nitro frameworks.
const NITRO_SLUGS: [(&str, &str); 4] = [("nuxtjs", "Nuxt"), ("tanstack-start", "TanStack Start"), ("solidstart-1", "SolidStart"), ("nitro", "Nitro")];

/// The frameworks that build the app, by Vercel's slug and their name: Nitro's servers first, then
/// the static sites. For the project setting that chooses one (backends::all).
pub fn slugs() -> Vec<(&'static str, &'static str, bool)> {
    std::iter::once(&NEXT_SLUG).chain(&NITRO_SLUGS).chain(&crate::react_router::SLUGS).map(|(slug, name)| (*slug, *name, true)).chain(built_presets().map(|p| (p.slugs[0], p.name, false))).collect()
}

const NEXT_SLUG: (&str, &str) = (crate::next::SLUG, crate::next::NAME);

/// Whether `slug` is one of Vercel's names for a framework this file builds.
pub fn known_slug(slug: &str) -> bool {
    slug == crate::next::SLUG || NITRO_SLUGS.iter().chain(&crate::react_router::SLUGS).any(|(s, _)| *s == slug) || built_presets().any(|p| p.slugs.contains(&slug))
}

/// Whether `name` (as a deployment records it) is a framework that builds the app, so what's
/// deployed is its build output rather than the project's own files.
pub fn builds(name: &str) -> bool {
    name == crate::next::NAME || NITRO_FRAMEWORKS.iter().any(|(_, n)| *n == name) || crate::react_router::Kind::of(name).is_some() || built_presets().any(|p| p.name == name) || name == "Other"
}

/// The static presets forkrail builds: all but those `REFUSED`.
fn built_presets() -> impl Iterator<Item = &'static Preset> {
    PRESETS.iter().filter(|p| refused(p.name).is_none())
}

/// Vercel's presets forkrail can't build yet, refused by name when they're the project's (in
/// Vercel's order, as detection finds them) or chosen.
const REFUSED: [(&str, &str); 3] = [
    ("Jekyll", "Jekyll isn't supported yet: Vercel builds it with Ruby 3.3 and `bundle install`, and forkrail's builder has no Ruby, nor a C compiler for the gems that compile (eventmachine, google-protobuf)"),
    ("Middleman", "Middleman isn't supported yet: Vercel builds it with Ruby 3.3 and `bundle install`, and forkrail's builder has no Ruby, nor a C compiler for the gems that compile (sassc, fast_blank)"),
    ("Zola", "Zola isn't supported yet: Vercel runs its x86_64 Linux build (ZOLA_VERSION), and forkrail builds on arm64, where no Zola release runs on the builder's Amazon Linux 2023 (Zola's arm64 builds, 0.20 and later, need glibc 2.35; it has 2.34)"),
];

fn refused(name: &str) -> Option<&'static str> {
    REFUSED.iter().find(|(n, _)| *n == name).map(|(_, why)| *why)
}

/// Vercel's `envPrefix` for each framework that has one, by name. SvelteKit has none:
/// `sveltekit-1` names none (only the 1.0 prereleases' `sveltekit` said `VITE_`); nor do TanStack
/// Start and Nitro.
const ENV_PREFIXES: [(&str, &str); 9] = [
    ("Gatsby", "GATSBY_"),
    ("Astro", "PUBLIC_"),
    ("Vue CLI", "VUE_APP_"),
    ("Create React App", "REACT_APP_"),
    ("Vite", "VITE_"),
    ("Sanity", "SANITY_STUDIO_"),
    ("Sanity (v2)", "SANITY_STUDIO_"),
    ("Nuxt", "NUXT_ENV_"),
    ("SolidStart", "VITE_"),
];

fn env_prefix(name: &str) -> &'static str {
    ENV_PREFIXES.iter().find(|(n, _)| *n == name).map_or("", |(_, prefix)| *prefix)
}

/// What Vercel's build cache keeps for a framework besides every `node_modules` (its
/// `cachePattern`), by name; and Hugo's download, which Vercel puts in `.vercel/cache`.
const CACHES: [(&str, &[&str]); 4] = [("Gatsby", &[".cache", "public"]), ("Eleventy", &[".cache"]), ("Nuxt", &[".nuxt"]), ("Hugo", &[".vercel/cache"])];

fn cache_dirs(name: &str) -> &'static [&'static str] {
    CACHES.iter().find(|(n, _)| *n == name).map_or(&[], |(_, dirs)| *dirs)
}

/// The system variables Vercel copies under a framework's prefix (`getPrefixedEnvVars` in
/// `@vercel/build-utils`), and the commit's (`VERCEL_GIT_*`).
const PREFIXED: [&str; 8] = [
    "VERCEL_URL",
    "VERCEL_ENV",
    "VERCEL_TARGET_ENV",
    "VERCEL_REGION",
    "VERCEL_BRANCH_URL",
    "VERCEL_PROJECT_PRODUCTION_URL",
    "VERCEL_DEPLOYMENT_ID",
    "VERCEL_PROJECT_ID",
];

/// What Vercel adds to a build's variables for a framework with `prefix`: each system variable
/// again under the prefix (`VITE_VERCEL_URL`), when there's a `VERCEL_URL` and the project hasn't
/// set that name itself, and `TURBO_CI_VENDOR_ENV_KEY`, which tells Turborepo the prefix.
pub fn prefixed_env(env: &BTreeMap<String, String>, prefix: &str) -> BTreeMap<String, String> {
    let mut added = BTreeMap::new();
    if prefix.is_empty() || !env.contains_key("VERCEL_URL") {
        return added;
    }
    for (key, value) in env {
        let name = format!("{prefix}{key}");
        if (PREFIXED.contains(&key.as_str()) || key.starts_with("VERCEL_GIT_")) && !env.contains_key(&name) {
            added.insert(name, value.clone());
        }
    }
    added.insert("TURBO_CI_VENDOR_ENV_KEY".to_owned(), format!("{prefix}VERCEL_"));
    added
}

/// SolidStart before 1.0 (`solid-start`, Vercel's `solidstart`), found by `solid-js` and
/// `solid-start` as Vercel finds it. Its preset names `.output`, which none of its adapters write:
/// on Vercel it deploys only with `solid-start-vercel`, whose Build Output Vercel takes instead.
const SOLIDSTART_0: &str = "SolidStart before 1.0 (`solid-start`) isn't supported: Vercel deploys it through the Build Output its solid-start-vercel adapter writes, which forkrail can't read yet. SolidStart 1 (`@solidjs/start`) is supported";

/// Astro adapters, which make it a server.
const ASTRO_SERVERS: [&str; 4] = ["@astrojs/node", "@astrojs/vercel", "@astrojs/netlify", "@astrojs/cloudflare"];

/// The framework of the project in `dir`, from its `package.json` (or `vercel.json`'s
/// `framework`), or `None` if it has none forkrail knows. `overrides` is `vercel.json`, whose
/// `buildCommand`, `installCommand` and `outputDirectory` win.
pub fn detect(dir: &Path, overrides: Option<&Value>) -> Result<Option<Framework>> {
    let path = dir.join("package.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let package: Option<Value> = text.as_deref().map(serde_json::from_str).transpose().with_context(|| format!("{} isn't valid JSON", path.display()))?;
    let has = |name: &str| {
        package.as_ref().is_some_and(|p| ["dependencies", "devDependencies"].iter().any(|k| p.get(k).and_then(|d| d.get(name)).is_some()))
    };
    // `framework` in vercel.json: a slug picks the preset; null is "Other", whatever is installed.
    let slug = match overrides.and_then(|o| o.get("framework")) {
        None => None,
        Some(Value::Null) => return Ok(None),
        Some(Value::String(slug)) => Some(slug.as_str()),
        Some(_) => bail!("`framework` must be one of Vercel's framework names, or null"),
    };
    // Next.js first, as in Vercel's list: a Next app that also lists Vite (for its tests) is a
    // Next app.
    if slug == Some(crate::next::SLUG) || (slug.is_none() && has("next")) {
        return crate::next::framework(dir, package.as_ref(), overrides).map(Some);
    }
    let nitro = match slug {
        Some(slug) => NITRO_SLUGS.iter().find(|(s, _)| *s == slug).map(|(_, name)| *name),
        None => NITRO_FRAMEWORKS.iter().find(|(dep, _)| has(dep)).map(|(_, name)| *name),
    };
    if let Some(name) = nitro {
        let Some(package) = &package else { bail!("{name} needs a package.json") };
        // TanStack Start builds a Nitro server only with Nitro's Vite plugin.
        if name == "TanStack Start" && !has("nitro") && !has("nitropack") {
            bail!("TanStack Start without Nitro isn't supported yet: add the `nitro` package and `nitro()` to vite.config's plugins");
        }
        if overrides.and_then(|o| o.get("outputDirectory")).is_some() {
            bail!("`outputDirectory` isn't used for {name}: Nitro writes its Build Output (.vercel/output) itself, as on Vercel");
        }
        let manager = PackageManager::of(dir);
        let script = |key: &str| overrides.and_then(|o| o.get(key)).and_then(Value::as_str).map(str::to_owned);
        let build = match (script("buildCommand"), build_script(Some(package))) {
            (Some(command), _) => command,
            (None, Some(run)) => format!("{} run {run}", manager.name()),
            (None, None) => bail!("{} has no `build` script, and vercel.json no buildCommand", path.display()),
        };
        // Nitro picks its preset as on Vercel (std-env sees VERCEL=1): its `vercel` preset's Build
        // Output is imported, routes, ISR, images and crons as Nitro wrote them.
        return Ok(Some(Framework {
            name,
            install: script("installCommand").unwrap_or_else(|| manager.install().to_owned()),
            build: Some(build),
            output: crate::build_output::DIR.to_owned(),
            entry: "",
            public: "static",
            build_env: &[],
            install_env: &[],
            env_prefix: env_prefix(name),
            cache: cache_dirs(name),
            find: Output::BuildOutput,
            hashed: &[],
            routes: "",
            unset: &[],
        }));
    }
    // React Router and Remix, before Astro as in Vercel's list: Vercel's own builder packages
    // their Vite build, and forkrail's port of it does here (react_router.rs, C16 F10).
    if let Some(kind) = crate::react_router::detect(dir, slug, &has) {
        return crate::react_router::framework(dir, kind, package.as_ref(), overrides).map(Some);
    }
    // Astro with Vercel's adapter builds a server through it, as on Vercel, like SvelteKit (C16 F3,
    // F6: its edge middleware).
    if (slug.is_none() || slug == Some("astro")) && has("astro") && has("@astrojs/vercel") {
        let script = |key: &str| overrides.and_then(|o| o.get(key)).and_then(Value::as_str).map(str::to_owned);
        if script("outputDirectory").is_some() {
            bail!("`outputDirectory` isn't used for Astro with a server: its Vercel adapter writes .vercel/output");
        }
        let manager = PackageManager::of(dir);
        let build = match script("buildCommand") {
            Some(command) => command,
            None if package.as_ref().is_some_and(|p| p.pointer("/scripts/build").is_some()) => format!("{} run build", manager.name()),
            None => "npx astro build".to_owned(),
        };
        return Ok(Some(Framework {
            name: "Astro",
            install: script("installCommand").unwrap_or_else(|| manager.install().to_owned()),
            build: Some(build),
            output: crate::build_output::DIR.to_owned(),
            entry: "",
            public: "static",
            build_env: &[],
            install_env: &[],
            env_prefix: env_prefix("Astro"),
            cache: cache_dirs("Astro"),
            find: Output::BuildOutput,
            hashed: &[],
            routes: "",
            unset: &[],
        }));
    }
    let preset = match slug {
        Some(slug) => match PRESETS.iter().find(|p| p.slugs.contains(&slug)) {
            Some(preset) if let Some(why) = refused(preset.name) => bail!("{why}"),
            Some(preset) => Some(preset),
            None if slug == "solidstart" => bail!("{SOLIDSTART_0}"),
            None => bail!("`framework: {slug}` isn't one forkrail builds yet (it builds Nitro servers, and static sites made with {})", preset_names()),
        },
        None => {
            if has("solid-js") && has("solid-start") {
                bail!("{SOLIDSTART_0}");
            }
            match find_preset(package.as_ref(), dir) {
                Some(preset) if let Some(why) = refused(preset.name) => bail!("{why}"),
                found => found,
            }
        }
    };
    let Some(preset) = preset else { return Ok(None) };
    // Some of them build a server unless told otherwise; only their static builds deploy so far.
    if preset.name == "Astro"
        && (ASTRO_SERVERS.iter().any(|d| has(d)) || astro_server(dir))
    {
        bail!("Astro with a server (an adapter, or output: \"server\") isn't supported yet; its static builds are");
    }
    let manager = PackageManager::of(dir);
    // SvelteKit builds a server through its Vercel adapter, as on Vercel: `adapter-vercel`, or
    // `adapter-auto`, which picks it when the build has Vercel's variables (forkrail's builds do).
    // With `adapter-static` it's a static site, below.
    if preset.name == "SvelteKit" && !has("@sveltejs/adapter-static") {
        if !has("@sveltejs/adapter-vercel") && !has("@sveltejs/adapter-auto") {
            bail!("SvelteKit deploys through its Vercel adapter (@sveltejs/adapter-vercel, or adapter-auto) as on Vercel, or as a static site with @sveltejs/adapter-static");
        }
        if overrides.and_then(|o| o.get("outputDirectory")).is_some() {
            bail!("`outputDirectory` isn't used for SvelteKit with a server: its Vercel adapter writes .vercel/output");
        }
        let script = |key: &str| overrides.and_then(|o| o.get(key)).and_then(Value::as_str).map(str::to_owned);
        let build = match script("buildCommand") {
            Some(command) => command,
            None if package.as_ref().is_some_and(|p| p.pointer("/scripts/build").is_some()) => format!("{} run build", manager.name()),
            None => format!("npx {}", preset.build),
        };
        return Ok(Some(Framework {
            name: preset.name,
            install: script("installCommand").unwrap_or_else(|| manager.install().to_owned()),
            build: Some(build),
            output: crate::build_output::DIR.to_owned(),
            entry: "",
            public: "static",
            build_env: &[],
            install_env: &[],
            // Vercel gives SvelteKit no prefix (`sveltekit-1` names none) and keeps no folder of its
            // build besides `node_modules`.
            env_prefix: env_prefix(preset.name),
            cache: cache_dirs(preset.name),
            find: Output::BuildOutput,
            hashed: &[],
            routes: "",
            unset: &[],
        }));
    }
    let script = |key: &str| overrides.and_then(|o| o.get(key)).and_then(Value::as_str).map(str::to_owned);
    let build = match script("buildCommand") {
        Some(command) => command,
        None => match build_script(package.as_ref()).filter(|_| preset.name != "Storybook") {
            Some(run) => format!("{} run {run}", manager.name()),
            None => preset.build.to_owned(),
        },
    };
    let (output, find) = match script("outputDirectory") {
        Some(dir) => (clean_output(&dir)?, Output::Fixed),
        None => match preset.name {
            "Angular" => (preset.output.to_owned(), Output::Angular),
            "Docusaurus (v1)" => (preset.output.to_owned(), Output::Docusaurus),
            "Polymer" => (preset.output.to_owned(), Output::Polymer),
            "Hugo" => (hugo_publish_dir(dir)?, Output::Fixed),
            _ => (preset.output.to_owned(), Output::Fixed),
        },
    };
    Ok(Some(Framework {
        name: preset.name,
        install: script("installCommand").unwrap_or_else(|| manager.install().to_owned()),
        build: Some(build),
        output,
        entry: "",
        public: "",
        build_env: &[],
        // Vercel's static-build sets this for Create React App's install (not Ionic React's).
        install_env: if preset.name == "Create React App" { &[("CI", "false")] } else { &[] },
        env_prefix: env_prefix(preset.name),
        cache: cache_dirs(preset.name),
        find,
        hashed: &[],
        routes: preset.routes,
        unset: &[],
    }))
}

/// Vercel's "Other": a project no preset knows, with a `package.json` whose `build` script (or
/// `buildCommand`) makes the site, or none: the folder is the site.
pub fn other(dir: &Path, overrides: Option<&Value>) -> Result<Framework> {
    let package: Option<Value> = match std::fs::read_to_string(dir.join("package.json")) {
        Ok(text) => Some(serde_json::from_str(&text).context("package.json isn't valid JSON")?),
        Err(_) => None,
    };
    let manager = PackageManager::of(dir);
    let script = |key: &str| overrides.and_then(|o| o.get(key)).and_then(Value::as_str).map(str::to_owned);
    // Without a preset Vercel runs the first of these the project has.
    let build = script("buildCommand").or_else(|| {
        let scripts = package.as_ref()?.get("scripts")?;
        ["vercel-build", "now-build", "build"].into_iter().find(|s| scripts.get(*s).is_some_and(Value::is_string)).map(|s| format!("{} run {s}", manager.name()))
    });
    let (output, find) = match script("outputDirectory") {
        Some(dir) => (clean_output(&dir)?, Output::Fixed),
        None => (String::new(), Output::PublicOrRoot),
    };
    Ok(Framework {
        name: "Other",
        install: script("installCommand").unwrap_or_else(|| manager.install().to_owned()),
        build,
        output,
        entry: "",
        public: "",
        build_env: &[],
        install_env: &[],
        env_prefix: "",
        cache: &[],
        find,
        hashed: &[],
        routes: "",
        unset: &[],
    })
}

fn preset_names() -> String {
    built_presets().map(|p| p.name).collect::<Vec<_>>().join(", ")
}

/// The `package.json` script Vercel runs for a build: when the project has a `build` or
/// `now-build` script, the first of `vercel-build`, `now-build` and `build` it has. With only a
/// `vercel-build`, Vercel runs the preset's own command instead.
fn build_script(package: Option<&Value>) -> Option<&'static str> {
    let scripts = package?.get("scripts")?;
    let has = |name: &str| scripts.get(name).is_some_and(Value::is_string);
    if !has("build") && !has("now-build") {
        return None;
    }
    ["vercel-build", "now-build", "build"].into_iter().find(|s| has(s))
}

/// Hugo's output, as Vercel reads it: `publishDir` in the first of `config.json`, `config.yaml`
/// and `config.toml` the project has (whichever of them made it a Hugo site), else `public`. A
/// file that doesn't parse fails the build, as on Vercel.
fn hugo_publish_dir(dir: &Path) -> Result<String> {
    for name in ["config.json", "config.yaml", "config.toml"] {
        let Ok(text) = std::fs::read_to_string(dir.join(name)) else { continue };
        let publish = match name {
            "config.json" => serde_json::from_str::<Value>(&text).with_context(|| format!("{name} isn't valid JSON"))?.get("publishDir").and_then(Value::as_str).map(str::to_owned),
            "config.toml" => text.parse::<toml::Table>().with_context(|| format!("{name} isn't valid TOML"))?.get("publishDir").and_then(|v| v.as_str()).map(str::to_owned),
            _ => yaml_top_level(&text, "publishDir"),
        };
        return clean_output(publish.as_deref().filter(|p| !p.is_empty()).unwrap_or("public"));
    }
    Ok("public".to_owned())
}

/// A top-level `key: value` of a YAML file, as Hugo's configs write `publishDir`: the line that
/// starts with the key, its value without quotes or a trailing comment. Nested keys, block scalars
/// and anchors aren't read (the CLI has no YAML parser).
fn yaml_top_level(text: &str, key: &str) -> Option<String> {
    let line = text.lines().find_map(|l| l.strip_prefix(key)?.trim_start().strip_prefix(':'))?;
    let value = line.trim();
    let value = match value.chars().next() {
        Some(q @ ('"' | '\'')) => value[1..].split(q).next().unwrap_or(""),
        _ => value.split(" #").next().unwrap_or("").trim_end(),
    };
    Some(value.to_owned())
}

/// The project's static preset, as Vercel's detection picks it: the first in `PRESETS` it has what
/// Vercel looks for, once those a found one supersedes are set aside.
fn find_preset(package: Option<&Value>, dir: &Path) -> Option<&'static Preset> {
    let found: Vec<&'static Preset> = PRESETS.iter().filter(|p| has_preset(p, package, dir)).collect();
    found.iter().copied().find(|p| !SUPERSEDES.iter().any(|(by, gone)| *gone == p.name && found.iter().any(|f| f.name == *by)))
}

/// Whether the project in `dir`, whose `package.json` is `package`, has what Vercel looks for to
/// call it `preset`: its detectors, else its dependency.
fn has_preset(preset: &Preset, package: Option<&Value>, dir: &Path) -> bool {
    let versions = |name: &str| -> Vec<&Value> { ["dependencies", "devDependencies"].iter().filter_map(|k| package?.get(k)?.get(name)).collect() };
    let has = |what: &Has| match *what {
        Has::Package(name) => !versions(name).is_empty(),
        Has::PackageVersion(name, prefix) => {
            versions(name).into_iter().filter_map(Value::as_str).any(|v| v.strip_prefix('^').unwrap_or(v).starts_with(prefix))
        }
        Has::File(path) => dir.join(path).exists(),
        Has::FileSays(path, text) => std::fs::read(dir.join(path)).is_ok_and(|b| String::from_utf8_lossy(&b).contains(text)),
    };
    match DETECTORS.iter().find(|(name, _)| *name == preset.name) {
        Some((_, d)) => d.every.iter().all(has) && (d.some.is_empty() || d.some.iter().any(has)),
        None => has(&Has::Package(preset.dependency)),
    }
}

/// `outputDirectory`: a folder inside the project.
pub(crate) fn clean_output(dir: &str) -> Result<String> {
    let dir = dir.trim().trim_start_matches("./").trim_end_matches('/');
    if dir.is_empty() || dir == "." {
        return Ok(".".to_owned());
    }
    if dir.starts_with('/') || dir.split('/').any(|p| p == "..") {
        bail!("`outputDirectory` must be a folder inside the project, not {dir:?}");
    }
    Ok(dir.to_owned())
}

/// The folder in `base` when it's the only thing there, as Vercel's Angular and Docusaurus presets
/// look for one.
fn sole_folder(base: &Path) -> Option<PathBuf> {
    let entries: Vec<std::fs::DirEntry> = std::fs::read_dir(base).ok()?.filter_map(Result::ok).collect();
    match entries.as_slice() {
        [only] if only.file_type().is_ok_and(|t| t.is_dir()) => Some(only.path()),
        _ => None,
    }
}

/// `bin` first on the build's PATH (the machine's own when the build's variables don't set one).
fn prepend_path(env: &mut BTreeMap<String, String>, bin: &Path) {
    let rest = env.get("PATH").cloned().or_else(|| std::env::var("PATH").ok()).unwrap_or_default();
    let joined = std::env::join_paths(std::iter::once(bin.to_path_buf()).chain(std::env::split_paths(&rest))).map_or(rest.clone(), |p| p.to_string_lossy().into_owned());
    env.insert("PATH".to_owned(), joined);
}

/// Whether an Astro project's config asks for a server.
fn astro_server(dir: &Path) -> bool {
    ["astro.config.mjs", "astro.config.ts", "astro.config.js", "astro.config.mts"].iter().any(|name| {
        std::fs::read_to_string(dir.join(name)).is_ok_and(|text| {
            let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
            compact.contains("output:\"server\"") || compact.contains("output:'server'") || compact.contains("output:\"hybrid\"") || compact.contains("output:'hybrid'")
        })
    })
}

impl Framework {
    /// Whether the build makes a server, the deployment's function.
    pub fn server(&self) -> bool {
        !self.entry.is_empty() || self.find == Output::BuildOutput
    }

    /// Whether it's Next.js, whose build next.rs takes.
    pub fn next(&self) -> bool {
        self.find == Output::Next
    }

    /// Whether the build leaves a Build Output to import (build_output.rs).
    pub fn build_output(&self) -> bool {
        self.find == Output::BuildOutput
    }

    /// Whether Nitro builds it (Nuxt, TanStack Start, SolidStart, Nitro).
    pub fn nitro(&self) -> bool {
        NITRO_FRAMEWORKS.iter().any(|(_, name)| *name == self.name)
    }

    /// For a Nitro build that left no Build Output but Nitro's `node-server` output (the project
    /// pins that preset, or the build ran where std-env finds another service first): that output
    /// as the deployment, one server, as forkrail ran Nitro apps before; and why Nitro chose it,
    /// for the build's log. `env` is the build's variables. None for every other build.
    pub fn nitro_server(&self, dir: &Path, env: &BTreeMap<String, String>) -> Option<(Framework, String)> {
        if !self.nitro() || !self.build_output() || crate::build_output::exists(dir) || !dir.join(NITRO_OUTPUT).join(NITRO_ENTRY).is_file() {
            return None;
        }
        let set = |name: &str| env.get(name).cloned().or_else(|| std::env::var(name).ok()).filter(|v| !v.is_empty());
        let why = if let Some((name, value)) = NITRO_PRESET_ENV.iter().find_map(|n| Some((*n, set(n)?))) {
            format!("{name}={value} is set")
        } else if let Some(name) = CI_BEFORE_VERCEL.iter().find(|n| set(n).is_some()) {
            format!("the build runs where {name} is set, which Nitro (std-env) takes over VERCEL=1")
        } else {
            "the project's config names Nitro's preset".to_owned()
        };
        let server = Framework {
            output: NITRO_OUTPUT.to_owned(),
            entry: NITRO_ENTRY,
            public: NITRO_PUBLIC,
            find: Output::Fixed,
            hashed: NITRO_HASHED,
            ..self.clone()
        };
        Some((server, why))
    }

    /// Whether the project uses Nuxt Image (`@nuxt/image`), which picks its Vercel provider by
    /// VERCEL=1 whatever Nitro's preset, so its pages ask `/_vercel/image` for their images.
    pub fn nuxt_image(dir: &Path) -> bool {
        let Ok(text) = std::fs::read_to_string(dir.join("package.json")) else { return false };
        let Ok(package) = serde_json::from_str::<Value>(&text) else { return false };
        ["dependencies", "devDependencies"].iter().any(|k| package.get(k).and_then(|d| d.get("@nuxt/image")).is_some())
    }

    pub fn output_dir(&self, dir: &Path) -> PathBuf {
        match self.find {
            Output::Fixed | Output::BuildOutput => dir.join(&self.output),
            // Wherever `distDir` put it, once it's built.
            Output::Next => crate::next::find_dist(dir, &self.output).unwrap_or_else(|| dir.join(&self.output)),
            Output::Angular => {
                let dist = dir.join(&self.output);
                match sole_folder(&dist) {
                    Some(project) if project.join("browser").exists() => project.join("browser"),
                    Some(project) => project,
                    None => dist,
                }
            }
            Output::Docusaurus => {
                let base = dir.join(&self.output);
                sole_folder(&base).unwrap_or(base)
            }
            Output::Polymer => {
                // Node lists a folder sorted by name, as Vercel's code reads it.
                let base = dir.join(&self.output);
                let mut names: Vec<std::ffi::OsString> = std::fs::read_dir(&base).map(|e| e.filter_map(|e| e.ok().map(|e| e.file_name())).collect()).unwrap_or_default();
                names.sort();
                names.into_iter().find(|n| !n.to_string_lossy().contains('.')).map_or(base.clone(), |n| base.join(n))
            }
            Output::PublicOrRoot => {
                let public = dir.join("public");
                if public.is_dir() { public } else { dir.to_path_buf() }
            }
        }
    }

    /// The folder of static files the site serves, after the build.
    pub fn public_dir(&self, dir: &Path) -> PathBuf {
        let output = self.output_dir(dir);
        if self.public.is_empty() { output } else { output.join(self.public) }
    }

    /// Install the dependencies, then build, in `dir`, with the build's env vars. A folder that has
    /// its `node_modules` is taken to be installed unless `install` says to run the command anyway.
    /// A site with nothing to build installs nothing either.
    pub fn build(&self, dir: &Path, install: Install, env: &BTreeMap<String, String>, log: &dyn BuildLog) -> Result<()> {
        let Some(build) = &self.build else { return self.check(dir) };
        // What Vercel's static-build gives the install and the build: the framework's prefixed
        // system variables, and Hugo, fetched when it's the framework or HUGO_VERSION asks for it,
        // first on PATH.
        let mut env = env.clone();
        env.extend(prefixed_env(&env, self.env_prefix));
        if self.name == "Hugo" || env.contains_key("HUGO_VERSION") {
            let bin = crate::hugo::install(dir, env.get("HUGO_VERSION").map(String::as_str), log)?;
            prepend_path(&mut env, &bin);
        }
        let package = dir.join("package.json").is_file();
        // A Next project is installed when Next is there as Node would find it: in the project's
        // `node_modules` or its workspace's above it (a workspace package may have none of its
        // own), and not in a folder that only holds the project's own packages.
        let missing = if self.next() { crate::next::installed(dir).is_err() } else { !dir.join("node_modules").is_dir() };
        if package && (install == Install::Always || missing) {
            log.step(buildlog::INSTALLING);
            let mut env = env.clone();
            env.extend(self.install_env.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())));
            shell(dir, &self.install, &env, log)?;
        }
        // A preset's own command (`ember build`) runs with the project's tools on PATH, as Vercel's.
        if package {
            prepend_path(&mut env, &dir.join("node_modules").join(".bin"));
        }
        // An old output must not pass for this build's (never the project itself, or `public/`,
        // which may be the project's own files).
        if matches!(self.find, Output::Fixed | Output::BuildOutput) && self.output != "." && self.server() {
            // Nitro writes one or the other by its preset: neither may be an earlier build's.
            let nitro = self.nitro().then(|| dir.join(NITRO_OUTPUT));
            for output in std::iter::once(self.output_dir(dir)).chain(nitro) {
                if output.exists() {
                    std::fs::remove_dir_all(&output).with_context(|| format!("removing the old {}", output.display()))?;
                }
            }
        }
        env.extend(self.build_env.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())));
        log.step(buildlog::BUILDING);
        let react_router = crate::react_router::Kind::of(self.name);
        if let Some(kind) = react_router {
            crate::react_router::before_build(dir, kind)?;
        }
        // Next: its version is known now that it's installed; the adapter's file lives for the build.
        let next = if self.next() { Some(crate::next::before_build(dir, &mut env, log)?) } else { None };
        let status = shell_to_end(dir, build, &env, self.unset, log)?;
        drop(next);
        if !status.success() {
            if let Some(oom) = crate::build::out_of_memory(status, self.name) {
                return Err(oom.into());
            }
            bail!("`{build}` failed ({status})");
        }
        // Vercel's builder packages React Router's and Remix's build once it has run.
        if let Some(kind) = react_router {
            crate::react_router::package(dir, kind, &env, log)?;
        }
        self.check(dir)
    }

    /// The output is there and is what forkrail runs.
    pub fn check(&self, dir: &Path) -> Result<()> {
        if self.next() {
            return crate::next::check(dir, &self.output);
        }
        let output = self.output_dir(dir);
        if self.build_output() {
            if output.join("config.json").is_file() {
                return Ok(());
            }
            if self.nitro() {
                // A pinned `node-server` preset: its server, checked as such (`nitro_server`).
                let server = dir.join(NITRO_OUTPUT);
                if server.join(NITRO_ENTRY).is_file() {
                    return Framework { output: NITRO_OUTPUT.to_owned(), entry: NITRO_ENTRY, public: NITRO_PUBLIC, find: Output::Fixed, ..self.clone() }.check(dir);
                }
                bail!(
                    "the build left no {}: Nitro writes it with its `vercel` preset, which it picks when it builds with VERCEL=1 as on Vercel (or its `node-server` preset's {}/{NITRO_ENTRY}, which forkrail also runs); check the preset the project names (`preset`, NITRO_PRESET)",
                    output.display(),
                    server.display()
                );
            }
            bail!("the build left no {}: {} deploys through its Vercel adapter, which writes it", output.display(), self.name);
        }
        if !self.server() {
            // A build that wrote a Build Output instead deploys that (`wrote_build_output`).
            if !output.is_dir() && !crate::build_output::exists(dir) {
                bail!("the build left no {} to serve; set `outputDirectory` in vercel.json if the site is built elsewhere", output.display());
            }
            return Ok(());
        }
        let entry = output.join(self.entry);
        if !entry.is_file() {
            bail!("the build left no {}", entry.display());
        }
        if let Ok(text) = std::fs::read_to_string(output.join("nitro.json"))
            && let Ok(info) = serde_json::from_str::<Value>(&text)
            && let Some(preset) = info.get("preset").and_then(Value::as_str)
            && preset != "node-server"
            && preset != "node"
        {
            bail!("{} was built with Nitro's {preset} preset; forkrail runs Nitro's `vercel` preset, as Vercel does, or `node-server`", output.display());
        }
        Ok(())
    }
}

/// When a framework's install command runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Install {
    /// Only without a `node_modules`: a developer's folder, whose dependencies are as they left them.
    IfMissing,
    /// Every time, as on Vercel: the builder, whose `node_modules` is an earlier build's (cache.rs)
    /// or isn't there, and the command brings it to what the lockfile says.
    Always,
}

/// Web frameworks a Python API is written with, by their names on PyPI, and what to call them.
/// The first one the project names wins: FastAPI comes before Starlette, which it's built on and
/// some projects list beside it.
const PYTHON_APIS: [(&str, &str); 6] = [
    ("fastapi", "FastAPI"),
    ("django", "Django"),
    ("flask", "Flask"),
    ("starlette", "Starlette"),
    ("litestar", "Litestar"),
    ("quart", "Quart"),
];

/// The same for a Node API, by their names on npm.
const NODE_APIS: [(&str, &str); 4] = [("express", "Express"), ("hono", "Hono"), ("fastify", "Fastify"), ("koa", "Koa")];

/// The web framework an API is written with, for a project no framework builds: from the
/// `requirements.txt` the packager installs, or `package.json`'s dependencies. `None` when it names
/// none of the known ones, or the file can't be read: this is only recorded, so nothing fails on it.
pub fn api(dir: &Path, spec: &Spec) -> Option<&'static str> {
    // A backend preset names its own, unless it's a language's catch-all ("Python", "Node.js").
    if let Some(backend) = &spec.backend
        && !matches!(backend.preset.slug, "python" | "node")
    {
        return Some(backend.preset.name);
    }
    match spec.language {
        Language::Python => python_api(&std::fs::read_to_string(package::requirements_file(dir, &spec.entry)?).ok()?),
        Language::Node => node_api(&std::fs::read_to_string(dir.join("package.json")).ok()?),
        Language::Go | Language::Rust => None,
    }
}

fn python_api(requirements: &str) -> Option<&'static str> {
    let named: Vec<String> = requirements.lines().filter_map(requirement_name).collect();
    PYTHON_APIS.iter().find(|(package, _)| named.iter().any(|n| n == package)).map(|(_, name)| *name)
}

/// The package a line of `requirements.txt` installs, as PyPI compares names (lowercase, `_` and
/// `.` as `-`): `fastapi[standard]>=0.115` is `fastapi`. `None` for a comment, a blank line, and
/// pip's options (`-r other.txt`, `--index-url`): other files aren't followed.
pub(crate) fn requirement_name(line: &str) -> Option<String> {
    // A file saved with a byte-order mark starts its first line with it.
    let line = line.trim_start_matches('\u{feff}').trim_start();
    if line.starts_with(['#', '-']) {
        return None;
    }
    // The name ends at an extra, a version, a marker, a URL or a comment.
    let end = line.find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))).unwrap_or(line.len());
    let name = &line[..end];
    (!name.is_empty()).then(|| name.to_ascii_lowercase().replace(['_', '.'], "-"))
}

/// Runtime dependencies only: a server's framework isn't a dev dependency.
fn node_api(package_json: &str) -> Option<&'static str> {
    let package: Value = serde_json::from_str(package_json).ok()?;
    let dependencies = package.get("dependencies")?.as_object()?;
    NODE_APIS.iter().find(|(dep, _)| dependencies.contains_key(*dep)).map(|(_, name)| *name)
}

/// The name of the project's package manager, by its lockfile: `npm`, `pnpm`, `yarn`, `bun`.
pub fn package_manager_name(dir: &Path) -> &'static str {
    PackageManager::of(dir).name()
}

/// Vercel's install command for the project's package manager.
pub fn install_command(dir: &Path) -> String {
    PackageManager::of(dir).install().to_owned()
}

/// The install command for a server whose `node_modules` is packaged as it is (a backend preset's):
/// every package in a plain folder of its own, since the package can't hold pnpm's links, nor
/// Yarn's Plug'n'Play.
pub fn flat_install_command(dir: &Path) -> String {
    match PackageManager::of(dir) {
        PackageManager::Pnpm => "pnpm install --config.node-linker=hoisted".to_owned(),
        PackageManager::Yarn => "YARN_NODE_LINKER=node-modules yarn install".to_owned(),
        manager => manager.install().to_owned(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackageManager {
    Npm,
    Pnpm,
    Yarn,
    Bun,
}

impl PackageManager {
    /// By the project's lockfile, or its workspace's: a package of a monorepo has none of its
    /// own, so the folders above it are looked at too, up to the repository's top (the folder
    /// with `.git`). Outside a repository only the project's own folder counts, so a stray
    /// lockfile in a home folder decides nothing. With no lockfile anywhere, the one
    /// `package.json` names in `packageManager`.
    fn of(dir: &Path) -> Self {
        let here = |dir: &Path| {
            if dir.join("pnpm-lock.yaml").is_file() {
                Some(Self::Pnpm)
            } else if dir.join("yarn.lock").is_file() {
                Some(Self::Yarn)
            } else if dir.join("bun.lock").is_file() || dir.join("bun.lockb").is_file() {
                Some(Self::Bun)
            } else if dir.join("package-lock.json").is_file() || dir.join("npm-shrinkwrap.json").is_file() {
                Some(Self::Npm)
            } else {
                None
            }
        };
        // Without a lockfile, `package.json`'s `packageManager` (`pnpm@10.33.0`), as Vercel reads it.
        let named = |dir: &Path| {
            let package: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).ok()?).ok()?;
            match package.get("packageManager")?.as_str()?.split('@').next()? {
                "pnpm" => Some(Self::Pnpm),
                "yarn" => Some(Self::Yarn),
                "bun" => Some(Self::Bun),
                "npm" => Some(Self::Npm),
                _ => None,
            }
        };
        if let Some(found) = here(dir) {
            return found;
        }
        let top = dir.ancestors().find(|a| a.join(".git").exists());
        if let Some(top) = top {
            for folder in dir.ancestors().skip(1) {
                if let Some(found) = here(folder) {
                    return found;
                }
                if folder == top {
                    break;
                }
            }
        }
        named(dir).unwrap_or(Self::Npm)
    }

    fn name(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
            Self::Yarn => "yarn",
            Self::Bun => "bun",
        }
    }

    /// Vercel's install commands. They leave a `node_modules` that's already right alone, which is
    /// what makes keeping it between builds worth it; `npm ci` would delete it first.
    fn install(self) -> &'static str {
        match self {
            Self::Npm => "npm install",
            Self::Pnpm => "pnpm install",
            Self::Yarn => "yarn install",
            Self::Bun => "bun install",
        }
    }
}

fn shell(dir: &Path, command: &str, env: &BTreeMap<String, String>, log: &dyn BuildLog) -> Result<()> {
    log.info(command);
    buildlog::run(Command::new("sh").arg("-c").arg(command).current_dir(dir), &format!("`{command}`"), env, log)
}

/// `shell`, for the build command: without this machine's `unset` variables, and returning how
/// the command ended, since that can say why it failed.
fn shell_to_end(dir: &Path, command: &str, env: &BTreeMap<String, String>, unset: &[&str], log: &dyn BuildLog) -> Result<std::process::ExitStatus> {
    log.info(command);
    let mut sh = Command::new("sh");
    sh.arg("-c").arg(command).current_dir(dir);
    // The project's own value of one of them (unlikely) still goes through: `env` is set after.
    for name in unset {
        sh.env_remove(name);
    }
    buildlog::run_to_end(&mut sh, &format!("`{command}`"), env, log)
}

/// Config files a Nitro framework may declare `scheduledTasks` in: plain Nitro, Nuxt (under
/// `nitro`), SolidStart (app.config) and TanStack Start (Nitro's Vite plugin).
const SCHEDULE_CONFIGS: [&str; 4] = ["nitro.config", "nuxt.config", "app.config", "vite.config"];
const CONFIG_EXTENSIONS: [&str; 6] = ["ts", "mts", "cts", "js", "mjs", "cjs"];

/// How the build went, for what's said about the schedules a framework declares
/// (`declared_schedules`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Built {
    /// Its Build Output was imported, with the crons and schedules in it.
    BuildOutput,
    /// Nitro's `node-server` output runs as one server (`Framework::nitro_server`).
    NitroServer,
    /// Anything else: a `.vercel/output` in the project isn't what's deployed.
    Other,
}

/// What to tell the user about schedules a framework declares that won't run as they may expect:
/// Nitro's `scheduledTasks` where its preset doesn't make them cron jobs, and `crons`/`schedules`
/// in a `.vercel/output/config.json` that isn't what's deployed. One warning each, naming them, so
/// they aren't dropped silently. Nitro 3's `vercel` preset makes its tasks cron jobs calling
/// `/_vercel/cron` (in the Build Output, and run as Vercel runs them); Nitro 2's (nitropack, which
/// Nuxt 4 builds with) makes nothing of them, and Vercel never runs them.
pub fn declared_schedules(dir: &Path, built: Built) -> Vec<String> {
    let mut warnings = Vec::new();
    // Nitro's record of the build, beside its Build Output, names its version (`2.13.5`, `3.0.…`).
    let nitro3 = std::fs::read_to_string(dir.join(crate::build_output::DIR).join("nitro.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|info| info.pointer("/versions/nitro")?.as_str()?.split('.').next()?.parse::<u32>().ok())
        .is_some_and(|major| major >= 3);
    for name in SCHEDULE_CONFIGS {
        for ext in CONFIG_EXTENSIONS {
            let file = format!("{name}.{ext}");
            let Ok(text) = std::fs::read_to_string(dir.join(&file)) else { continue };
            let Some(tasks) = scheduled_tasks(&text) else { continue };
            let list = if tasks.is_empty() { String::new() } else { format!(" ({})", tasks.join(", ")) };
            match built {
                Built::BuildOutput if nitro3 => {}
                Built::BuildOutput => warnings.push(format!(
                    "{file} declares Nitro scheduledTasks{list}: Nitro 2's `vercel` preset (nitropack, as Nuxt 4 builds) makes no cron jobs of them, so they don't run, as on Vercel. Call a route of the app from vercel.json's `crons` instead"
                )),
                Built::NitroServer => warnings.push(format!(
                    "{file} declares Nitro scheduledTasks{list}: Nitro's node-server preset runs them inside the server, so only while a worker is open, never on a schedule of their own. Call a route of the app from vercel.json's `crons` instead"
                )),
                Built::Other => warnings.push(format!(
                    "{file} declares Nitro scheduledTasks{list}: they don't run here. Call a route of the app from vercel.json's `crons` instead"
                )),
            }
        }
    }
    let output = dir.join(".vercel/output/config.json");
    if built != Built::BuildOutput
        && let Ok(text) = std::fs::read_to_string(&output)
        && let Ok(config) = serde_json::from_str::<Value>(&text)
    {
        for key in ["crons", "schedules"] {
            let Some(list) = config.get(key).and_then(Value::as_array).filter(|l| !l.is_empty()) else { continue };
            let named: Vec<String> = list
                .iter()
                .map(|c| {
                    let what = c.get("path").or_else(|| c.get("function")).and_then(Value::as_str).unwrap_or("?");
                    let when = c.get("schedule").and_then(Value::as_str).unwrap_or("?");
                    format!("{when} {what}")
                })
                .collect();
            warnings.push(format!(
                ".vercel/output/config.json declares {key} ({}): that Build Output isn't what's deployed (forkrail deploys the one a framework's build writes), so they won't run. Put them in vercel.json's `{key}`",
                named.join(", ")
            ));
        }
    }
    warnings
}

/// Nitro's `scheduledTasks: { '<cron>': ['<task>', …] }` in a config file's text: the `cron → tasks`
/// pairs, or an empty list when the object can't be read; None when there's none. Commented-out
/// lines don't count.
fn scheduled_tasks(text: &str) -> Option<Vec<String>> {
    let code: String = text.lines().filter(|l| !l.trim_start().starts_with("//") && !l.trim_start().starts_with('*')).collect::<Vec<_>>().join("\n");
    let at = code.find("scheduledTasks")?;
    let rest = &code[at + "scheduledTasks".len()..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    if !rest.starts_with('{') {
        return Some(Vec::new());
    }
    // The object, to its matching brace.
    let mut depth = 0;
    let mut end = rest.len();
    for (i, c) in rest.char_indices() {
        match c {
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth == 0 {
                    end = i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    // An object left open (the file ends inside it) is read to the end; never split a character.
    let body = rest.get(1..end.saturating_sub(1).max(1)).or_else(|| rest.get(1..)).unwrap_or("");
    if body.trim().is_empty() {
        return None;
    }
    // Each `'<cron>': [ ... ]` or `"<cron>": '<task>'`.
    let mut pairs = Vec::new();
    let quoted = |s: &str| -> Option<(String, usize)> {
        let q = s.chars().next().filter(|c| matches!(c, '\'' | '"' | '`'))?;
        let close = s[1..].find(q)? + 1;
        Some((s[1..close].to_owned(), close + 1))
    };
    let mut rest = body.trim_start();
    while let Some((cron, used)) = quoted(rest) {
        let after = rest[used..].trim_start();
        let Some(after) = after.strip_prefix(':') else { break };
        let after = after.trim_start();
        let (tasks, used) = if let Some(list) = after.strip_prefix('[') {
            let close = list.find(']').unwrap_or(list.len());
            let names: Vec<String> = list[..close].split(',').filter_map(|t| quoted(t.trim()).map(|(n, _)| n)).collect();
            (names, close + 2)
        } else if let Some((name, used)) = quoted(after) {
            (vec![name], used)
        } else {
            (Vec::new(), after.find(',').unwrap_or(after.len()))
        };
        pairs.push(if tasks.is_empty() { cron } else { format!("{cron} → {}", tasks.join(", ")) });
        let next = after.get(used.min(after.len())..).unwrap_or("").trim_start();
        rest = next.strip_prefix(',').unwrap_or(next).trim_start();
    }
    Some(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framework_schedules_are_named() {
        let dir = project(&[
            (
                "nitro.config.ts",
                "export default defineNitroConfig({\n  experimental: { tasks: true },\n  scheduledTasks: {\n    '0 * * * *': ['cms:update'],\n    \"*/5 * * * *\": 'db:clean',\n  },\n})\n",
            ),
            ("nuxt.config.ts", "export default defineNuxtConfig({\n  // scheduledTasks: { '* * * * *': ['x'] },\n})\n"),
            (".vercel/output/config.json", r#"{"version": 3, "crons": [{"path": "/api/cron", "schedule": "0 5 * * *"}]}"#),
        ]);
        // A Build Output that isn't what's deployed: both said.
        let warnings = declared_schedules(&dir, Built::Other);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].starts_with("nitro.config.ts declares Nitro scheduledTasks (0 * * * * → cms:update, */5 * * * * → db:clean)"), "{}", warnings[0]);
        assert!(warnings[1].contains("crons (0 5 * * * /api/cron)"), "{}", warnings[1]);
        // Imported, by Nitro 2: its tasks make no cron jobs (as on Vercel); the Build Output's crons run.
        let warnings = declared_schedules(&dir, Built::BuildOutput);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("Nitro 2's `vercel` preset") && warnings[0].contains("as on Vercel"), "{}", warnings[0]);
        // Nitro 2 names itself in nitro.json too.
        std::fs::write(dir.join(".vercel/output/nitro.json"), r#"{"preset": "vercel", "versions": {"nitro": "2.13.5"}}"#).unwrap();
        assert_eq!(declared_schedules(&dir, Built::BuildOutput).len(), 1);
        // By Nitro 3: its tasks are the Build Output's crons.
        std::fs::write(dir.join(".vercel/output/nitro.json"), r#"{"preset": "vercel", "versions": {"nitro": "3.0.261103-rc"}}"#).unwrap();
        assert!(declared_schedules(&dir, Built::BuildOutput).is_empty());
        // Nitro's node-server preset runs them inside the server only.
        let warnings = declared_schedules(&dir, Built::NitroServer);
        assert!(warnings[0].contains("only while a worker is open"), "{}", warnings[0]);
        // Nothing declared, nothing said.
        assert!(declared_schedules(&project(&[("vite.config.ts", "export default defineConfig({ plugins: [nitro()] })")]), Built::Other).is_empty());
        assert_eq!(scheduled_tasks("scheduledTasks: tasks"), Some(Vec::new()));
        assert_eq!(scheduled_tasks("scheduledTasks: {}"), None);
        // Files that end inside the object, or on a character of more than one byte, don't stop a build.
        assert_eq!(scheduled_tasks("scheduledTasks: {"), None);
        assert_eq!(scheduled_tasks("scheduledTasks: { '0 * * * *': ['a'] // ✨"), Some(vec!["0 * * * * → a".to_owned()]));
        assert!(scheduled_tasks("scheduledTasks: { '{': 'x' }) // ✨").is_some());
    }

    fn project(files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("forkrail-fw-{}", forkrail_common::random_hex(6)));
        for (name, text) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    #[test]
    fn detects_nitro_frameworks() {
        let web = project(&[
            ("package.json", r#"{"scripts": {"build": "vite build"}, "dependencies": {"@tanstack/react-start": "1", "nitro": "3"}}"#),
            ("package-lock.json", "{}"),
        ]);
        let fw = detect(&web, None).unwrap().unwrap();
        assert_eq!((fw.name, fw.install.as_str(), fw.build.as_deref()), ("TanStack Start", "npm install", Some("npm run build")));
        // Nitro's `vercel` preset's Build Output, as on Vercel; no preset forced.
        assert_eq!((fw.output.as_str(), fw.entry, fw.public, fw.find), (".vercel/output", "", "static", Output::BuildOutput));
        assert!(fw.build_env.is_empty() && fw.nitro() && fw.server());
        // A build that left node-server's output instead (a pinned preset) runs that, and says why.
        assert!(fw.nitro_server(&web, &BTreeMap::new()).is_none(), "nothing built yet");
        std::fs::create_dir_all(web.join(".output/server")).unwrap();
        std::fs::write(web.join(".output/server/index.mjs"), "").unwrap();
        let env: BTreeMap<String, String> = [("NITRO_PRESET".to_owned(), "node-server".to_owned())].into();
        let (server, why) = fw.nitro_server(&web, &env).unwrap();
        assert_eq!((server.output.as_str(), server.entry, server.public, server.find), (".output", "server/index.mjs", "public", Output::Fixed));
        assert_eq!(why, "NITRO_PRESET=node-server is set");
        fw.check(&web).unwrap();
        // A Build Output beside it wins: it's what Nitro's `vercel` preset wrote.
        std::fs::create_dir_all(web.join(".vercel/output")).unwrap();
        std::fs::write(web.join(".vercel/output/config.json"), r#"{"version": 3}"#).unwrap();
        assert!(fw.nitro_server(&web, &env).is_none());
        let overrides = serde_json::json!({"buildCommand": "npm run build:prod"});
        assert_eq!(detect(&web, Some(&overrides)).unwrap().unwrap().build.as_deref(), Some("npm run build:prod"));

        let nuxt = project(&[("package.json", r#"{"scripts": {"build": "nuxt build"}, "devDependencies": {"nuxt": "4"}}"#), ("pnpm-lock.yaml", "")]);
        let fw = detect(&nuxt, None).unwrap().unwrap();
        assert_eq!((fw.name, fw.build.as_deref()), ("Nuxt", Some("pnpm run build")));
    }

    #[test]
    fn detects_next() {
        // By its dependency, before anything else the project lists (Vercel's order).
        let app = project(&[
            ("package.json", r#"{"scripts": {"build": "next build"}, "dependencies": {"next": "16.4.6", "react": "19"}, "devDependencies": {"vite": "7", "nitro": "3"}}"#),
            ("pnpm-lock.yaml", ""),
        ]);
        let fw = detect(&app, None).unwrap().unwrap();
        assert_eq!((fw.name, fw.build.as_deref(), fw.install.as_str()), ("Next.js", Some("pnpm run build"), "pnpm install"));
        assert!(fw.server() && fw.next() && !fw.build_output() && !fw.nitro());
        assert_eq!((fw.entry, fw.cache, fw.env_prefix), (crate::next::ENTRY, &[".next/cache"][..], "NEXT_PUBLIC_"));
        // Its build never looks like Vercel's own builder's, nor like an AI agent's session.
        assert!(fw.unset.contains(&"NOW_BUILDER") && fw.unset.contains(&"CLAUDECODE") && fw.unset.contains(&"AI_AGENT"));
        // Vercel's builder runs `vercel-build` over `build`, and `next build` when there's neither.
        let scripts = project(&[("package.json", r#"{"scripts": {"build": "next build", "vercel-build": "prisma generate && next build"}, "dependencies": {"next": "15"}}"#)]);
        assert_eq!(detect(&scripts, None).unwrap().unwrap().build.as_deref(), Some("npm run vercel-build"));
        let bare = project(&[("package.json", r#"{"dependencies": {"next": "14.2.0"}}"#)]);
        assert_eq!(detect(&bare, None).unwrap().unwrap().build.as_deref(), Some("next build"));
        // vercel.json: its commands win; the preset by name (a workspace's catalog hides the
        // dependency); null is "Other"; outputDirectory names Next's output folder.
        let overrides = serde_json::json!({"buildCommand": "turbo run build --filter=web", "installCommand": "pnpm install --frozen-lockfile"});
        let fw = detect(&bare, Some(&overrides)).unwrap().unwrap();
        assert_eq!((fw.build.as_deref(), fw.install.as_str()), (Some("turbo run build --filter=web"), "pnpm install --frozen-lockfile"));
        let named = project(&[("package.json", r#"{"scripts": {"build": "next build"}, "dependencies": {"react": "19"}}"#)]);
        assert_eq!(detect(&named, None).unwrap(), None);
        assert_eq!(detect(&named, Some(&serde_json::json!({"framework": "nextjs"}))).unwrap().unwrap().name, "Next.js");
        assert_eq!(detect(&app, Some(&serde_json::json!({"framework": null}))).unwrap(), None);
        assert_eq!(detect(&app, Some(&serde_json::json!({"outputDirectory": "./apps/.next/"}))).unwrap().unwrap().output, "apps/.next");
        assert!(detect(&app, Some(&serde_json::json!({"outputDirectory": "../x/.next"}))).is_err());
        assert!(known_slug("nextjs") && builds("Next.js") && slugs().first() == Some(&("nextjs", "Next.js", true)));
        // Before a build there's nothing to check; the error says what a build leaves.
        assert!(fw.check(&bare).unwrap_err().to_string().contains("next build"));
    }

    #[test]
    fn a_workspace_package_takes_its_workspaces_package_manager() {
        // A monorepo: the lockfile is at the repository's top, not in the app's folder.
        let repo = project(&[("yarn.lock", ""), (".git/HEAD", "ref: refs/heads/main"), ("apps/web/package.json", r#"{"scripts": {"build": "next build"}, "dependencies": {"next": "16"}}"#)]);
        let fw = detect(&repo.join("apps/web"), None).unwrap().unwrap();
        assert_eq!((fw.install.as_str(), fw.build.as_deref()), ("yarn install", Some("yarn run build")));
        // The app's own lockfile wins over the one above it.
        std::fs::write(repo.join("apps/web/bun.lock"), "").unwrap();
        assert_eq!(package_manager_name(&repo.join("apps/web")), "bun");
        // Outside a repository a lockfile in a folder above decides nothing.
        let loose = project(&[("pnpm-lock.yaml", ""), ("site/package.json", "{}")]);
        assert_eq!(package_manager_name(&loose.join("site")), "npm");
        assert_eq!(package_manager_name(&loose), "pnpm");
        // No lockfile at all: the one package.json names.
        let named = project(&[("package.json", r#"{"packageManager": "pnpm@10.33.0+sha512.abc"}"#)]);
        assert_eq!(package_manager_name(&named), "pnpm");
        std::fs::write(named.join("package-lock.json"), "{}").unwrap();
        assert_eq!(package_manager_name(&named), "npm", "a lockfile says more than the field");
    }

    #[test]
    fn refuses_what_isnt_supported() {
        let start = project(&[("package.json", r#"{"scripts": {"build": "vite build"}, "dependencies": {"@tanstack/react-start": "1"}}"#)]);
        assert!(detect(&start, None).unwrap_err().to_string().contains("without Nitro"));
        let plain = project(&[("package.json", r#"{"dependencies": {"express": "5"}}"#)]);
        assert_eq!(detect(&plain, None).unwrap(), None);
        assert_eq!(detect(&project(&[]), None).unwrap(), None);
        let no_build = project(&[("package.json", r#"{"dependencies": {"nitro": "3"}}"#)]);
        assert!(detect(&no_build, None).unwrap_err().to_string().contains("no `build` script"));
        let astro = project(&[("package.json", r#"{"dependencies": {"astro": "5", "@astrojs/node": "9"}}"#)]);
        assert!(detect(&astro, None).unwrap_err().to_string().contains("Astro with a server"));
        let astro = project(&[("package.json", r#"{"dependencies": {"astro": "5"}}"#), ("astro.config.mjs", "export default { output: 'server' }")]);
        assert!(detect(&astro, None).unwrap_err().to_string().contains("Astro with a server"));
        let kit = project(&[("package.json", r#"{"devDependencies": {"@sveltejs/kit": "2", "vite": "7"}}"#)]);
        assert!(detect(&kit, None).unwrap_err().to_string().contains("adapter-static"));
        let empty = project(&[("package.json", "{}")]);
        assert!(detect(&empty, Some(&serde_json::json!({"framework": "hydrogen"}))).unwrap_err().to_string().contains("isn't one forkrail builds"));
    }

    #[test]
    fn detects_static_sites() {
        let vite = project(&[("package.json", r#"{"scripts": {"build": "tsc && vite build"}, "devDependencies": {"vite": "7", "react": "19"}}"#)]);
        let fw = detect(&vite, None).unwrap().unwrap();
        assert_eq!((fw.name, fw.build.as_deref(), fw.output.as_str(), fw.server()), ("Vite", Some("npm run build"), "dist", false));
        assert_eq!(fw.public_dir(&vite), vite.join("dist"));
        // VitePress comes after Vite, as in Vercel's list: a VitePress site that names Vite is a
        // Vite site there.
        let docs = project(&[("package.json", r#"{"dependencies": {"vitepress": "1"}}"#)]);
        let fw = detect(&docs, None).unwrap().unwrap();
        assert_eq!((fw.name, fw.build.as_deref(), fw.output.as_str()), ("VitePress", Some("vitepress build docs"), "docs/.vitepress/dist"));
        let both = project(&[("package.json", r#"{"dependencies": {"vitepress": "1", "vite": "7"}}"#)]);
        assert_eq!(detect(&both, None).unwrap().unwrap().name, "Vite");
        let cra = project(&[("package.json", r#"{"scripts": {"build": "react-scripts build"}, "dependencies": {"react-scripts": "5"}}"#), ("yarn.lock", "")]);
        let fw = detect(&cra, None).unwrap().unwrap();
        assert_eq!((fw.name, fw.install.as_str(), fw.output.as_str()), ("Create React App", "yarn install", "build"));
        let kit = project(&[("package.json", r#"{"devDependencies": {"@sveltejs/kit": "2", "@sveltejs/adapter-static": "3"}}"#)]);
        assert_eq!(detect(&kit, None).unwrap().unwrap().output, "build");
        // vercel.json's outputDirectory and framework win.
        let overrides = serde_json::json!({"outputDirectory": "./site/", "framework": "vite"});
        let fw = detect(&project(&[("package.json", "{}")]), Some(&overrides)).unwrap().unwrap();
        assert_eq!((fw.name, fw.output.as_str()), ("Vite", "site"));
        assert!(detect(&vite, Some(&serde_json::json!({"outputDirectory": "../x"}))).is_err());
        assert_eq!(detect(&vite, Some(&serde_json::json!({"framework": null}))).unwrap(), None);
        // Angular is found by its CLI, and its output folder after the build.
        let ng = project(&[("package.json", r#"{"devDependencies": {"@angular/cli": "20"}, "dependencies": {"@angular/core": "20"}}"#), ("dist/shop/browser/index.html", "<p>")]);
        let fw = detect(&ng, None).unwrap().unwrap();
        assert_eq!(fw.public_dir(&ng), ng.join("dist/shop/browser"));
        assert_eq!(detect(&project(&[("package.json", r#"{"dependencies": {"@angular/core": "20"}}"#)]), None).unwrap(), None);
        // Other: public/ if there is one, else the project.
        let plain = project(&[("index.html", "<p>")]);
        let fw = other(&plain, None).unwrap();
        assert_eq!((fw.build.as_deref(), fw.public_dir(&plain)), (None, plain.clone()));
        let with_public = project(&[("package.json", r#"{"scripts": {"build": "node gen.js"}}"#), ("public/index.html", "<p>")]);
        let fw = other(&with_public, None).unwrap();
        assert_eq!((fw.build.as_deref(), fw.public_dir(&with_public)), (Some("npm run build"), with_public.join("public")));
    }

    /// Vercel's list (`@vercel/frameworks` 3.35.0), the slugs of its static presets in its order.
    const VERCEL_ORDER: [&str; 34] = [
        "gatsby", "astro", "hexo", "eleventy", "docusaurus-2", "docusaurus", "preact", "dojo", "ember", "vue", "scully", "ionic-angular", "angular",
        "polymer", "sveltekit", "sveltekit-1", "ionic-react", "create-react-app", "gridsome", "umijs", "saber", "stencil", "hugo", "jekyll", "brunch",
        "middleman", "zola", "vite", "vitepress", "vuepress", "parcel", "sanity", "sanity-v2", "storybook",
    ];

    #[test]
    fn presets_are_in_vercels_order() {
        let slugs: Vec<&str> = PRESETS.iter().flat_map(|p| p.slugs.iter().copied()).collect();
        assert_eq!(slugs, VERCEL_ORDER);
        // The side tables name real presets (or Nitro frameworks), and one without a dependency is
        // found by its files.
        let named = |name: &str| PRESETS.iter().any(|p| p.name == name) || NITRO_FRAMEWORKS.iter().any(|(_, n)| *n == name);
        for name in DETECTORS.iter().map(|(n, _)| *n).chain(REFUSED.iter().map(|(n, _)| *n)).chain(ENV_PREFIXES.iter().map(|(n, _)| *n)).chain(CACHES.iter().map(|(n, _)| *n)) {
            assert!(named(name), "{name}");
        }
        for (by, gone) in SUPERSEDES {
            assert!(named(by) && named(gone), "{by} {gone}");
        }
        for preset in PRESETS.iter().filter(|p| p.dependency.is_empty()) {
            assert!(DETECTORS.iter().any(|(n, _)| *n == preset.name), "{}", preset.name);
        }
    }

    #[test]
    fn each_preset_is_found_by_what_vercel_looks_for() {
        let deps = |names: &[&str]| {
            let list: Vec<String> = names.iter().map(|n| format!("\"{n}\": \"1\"")).collect();
            format!(r#"{{"devDependencies": {{{}}}}}"#, list.join(", "))
        };
        // The project's packages and other files, the preset, its build without a `build` script,
        // and its output.
        type Case = (&'static [&'static str], &'static [(&'static str, &'static str)], &'static str, &'static str, &'static str);
        let cases: [Case; 21] = [
            (&["hexo"], &[], "Hexo", "hexo generate", "public"),
            (&["docusaurus"], &[], "Docusaurus (v1)", "docusaurus-build", "build"),
            (&["preact-cli", "preact"], &[], "Preact", "preact build", "build"),
            (&["@dojo/framework"], &[], "Dojo", "dojo build", "output/dist"),
            (&[], &[(".dojorc", "{}")], "Dojo", "dojo build", "output/dist"),
            (&["ember-source"], &[], "Ember.js", "ember build", "dist"),
            (&["ember-cli"], &[], "Ember.js", "ember build", "dist"),
            (&["@scullyio/init", "@angular/core"], &[], "Scully", "ng build && scully", "dist/static"),
            (&["@ionic/angular", "@angular/core"], &[], "Ionic Angular", "ng build", "www"),
            (&["polymer-cli"], &[], "Polymer", "polymer build", "build"),
            (&["@ionic/react", "react-scripts"], &[], "Ionic React", "react-scripts build", "build"),
            (&["gridsome"], &[], "Gridsome", "gridsome build", "dist"),
            (&["umi"], &[], "UmiJS", "umi build", "dist"),
            (&["saber"], &[], "Saber", "saber build", "public"),
            (&["@stencil/core"], &[], "Stencil", "stencil build", "www"),
            (&[], &[("brunch-config.js", "")], "Brunch", "brunch build --production", "public"),
            (&["vuepress"], &[], "VuePress", "vuepress build src", "src/.vuepress/dist"),
            (&["parcel"], &[], "Parcel", "parcel build", "dist"),
            (&["sanity"], &[("sanity.config.ts", "")], "Sanity", "sanity build", "dist"),
            (&["react-dev-utils"], &[], "Create React App", "react-scripts build", "build"),
            (&[], &[("config.toml", "baseURL = 'https://example.org/'\npublishDir = 'site'\n")], "Hugo", "hugo --gc", "site"),
        ];
        for (packages, files, name, build, output) in cases {
            let text = deps(packages);
            let mut all: Vec<(&str, &str)> = vec![("package.json", text.as_str())];
            all.extend_from_slice(files);
            let fw = detect(&project(&all), None).unwrap().unwrap_or_else(|| panic!("{name}: nothing found"));
            assert_eq!((fw.name, fw.build.as_deref(), fw.output.as_str(), fw.server()), (name, Some(build), output, false));
        }
        let fw = detect(&project(&[("package.json", r#"{"dependencies": {"brunch": "^2"}}"#)]), None).unwrap().unwrap();
        assert_eq!(fw.name, "Brunch");
        // Sanity 2 by its CLI's major version and sanity.json; a Sanity package without its config
        // file isn't a Sanity Studio.
        let v2 = project(&[("package.json", r#"{"dependencies": {"@sanity/cli": "^2.26", "@sanity/core": "^2.26"}}"#), ("sanity.json", "{}")]);
        assert_eq!(detect(&v2, None).unwrap().unwrap().name, "Sanity (v2)");
        let v3 = project(&[("package.json", r#"{"dependencies": {"@sanity/cli": "^3.0.0"}}"#), ("sanity.json", "{}")]);
        assert_eq!(detect(&v3, None).unwrap(), None);
        assert_eq!(detect(&project(&[("package.json", r#"{"dependencies": {"sanity": "3"}}"#)]), None).unwrap(), None);
        // A `build` script runs instead of the preset's command, as on Vercel, but for Storybook's.
        let hexo = project(&[("package.json", r#"{"scripts": {"build": "hexo generate"}, "dependencies": {"hexo": "^5"}}"#), ("yarn.lock", "")]);
        assert_eq!(detect(&hexo, None).unwrap().unwrap().build.as_deref(), Some("yarn run build"));
        let book = project(&[("package.json", r#"{"scripts": {"build": "tsc"}, "devDependencies": {"storybook": "8", "@storybook/react-webpack5": "8"}}"#)]);
        let fw = detect(&book, None).unwrap().unwrap();
        assert_eq!((fw.name, fw.build.as_deref(), fw.output.as_str()), ("Storybook", Some("storybook build"), "storybook-static"));
        // Vercel's command as it is: Eleventy's says npx itself.
        let eleventy = project(&[("package.json", r#"{"dependencies": {"@11ty/eleventy": "3"}}"#)]);
        assert_eq!(detect(&eleventy, None).unwrap().unwrap().build.as_deref(), Some("npx @11ty/eleventy"));
        // SolidStart before 1.0 is refused, found or named.
        let solid = project(&[("package.json", r#"{"dependencies": {"solid-js": "1", "solid-start": "0.3"}}"#)]);
        assert!(detect(&solid, None).unwrap_err().to_string().contains("solid-start-vercel"));
        let empty = project(&[("package.json", "{}")]);
        assert!(detect(&empty, Some(&serde_json::json!({"framework": "solidstart"}))).unwrap_err().to_string().contains("SolidStart 1"));
    }

    #[test]
    fn a_slug_picks_its_preset() {
        let empty = project(&[("package.json", "{}")]);
        for preset in PRESETS.iter().filter(|p| !matches!(p.name, "Astro" | "SvelteKit")) {
            if let Some(why) = refused(preset.name) {
                let err = detect(&empty, Some(&serde_json::json!({"framework": preset.slugs[0]}))).unwrap_err().to_string();
                assert_eq!(err, why);
                assert!(!known_slug(preset.slugs[0]) && !builds(preset.name), "{}", preset.name);
                continue;
            }
            for slug in preset.slugs {
                let fw = detect(&empty, Some(&serde_json::json!({"framework": slug}))).unwrap().unwrap();
                assert_eq!((fw.name, fw.output.as_str()), (preset.name, preset.output), "{slug}");
                assert!(known_slug(slug), "{slug}");
            }
        }
        // Vercel's `docusaurus` is Docusaurus 1; 2 and later are `docusaurus-2`.
        let fw = detect(&empty, Some(&serde_json::json!({"framework": "docusaurus"}))).unwrap().unwrap();
        assert_eq!(fw.name, "Docusaurus (v1)");
        // The setting lists each preset once, by its first slug.
        let listed: Vec<&str> = slugs().into_iter().filter(|(_, _, server)| !server).map(|(slug, _, _)| slug).collect();
        assert_eq!(listed.len(), PRESETS.len() - REFUSED.len());
        assert!(listed.contains(&"storybook") && listed.contains(&"sanity-v2") && listed.contains(&"docusaurus"));
    }

    #[test]
    fn detection_order_is_vercels() {
        let found = |deps: &str| detect(&project(&[("package.json", &format!(r#"{{"devDependencies": {deps}}}"#))]), None).unwrap().map(|f| f.name);
        // Earlier in Vercel's list wins: these frameworks' projects often list Vite or Angular too.
        assert_eq!(found(r#"{"ember-source": "5", "vite": "6"}"#), Some("Ember.js"));
        assert_eq!(found(r#"{"polymer-cli": "1", "vite": "6"}"#), Some("Polymer"));
        assert_eq!(found(r#"{"@scullyio/init": "2", "@ionic/angular": "8", "@angular/core": "19"}"#), Some("Scully"));
        assert_eq!(found(r#"{"@ionic/angular": "8", "@angular/core": "19"}"#), Some("Ionic Angular"));
        assert_eq!(found(r#"{"hexo": "7", "@11ty/eleventy": "3"}"#), Some("Hexo"));
        assert_eq!(found(r#"{"@docusaurus/core": "3", "docusaurus": "1"}"#), Some("Docusaurus"));
        assert_eq!(found(r#"{"@ionic/react": "8", "react-scripts": "5"}"#), Some("Ionic React"));
        // Vite supersedes Ionic React wherever it is (Vercel's own Ionic React example is a Vite
        // site); Create React App, before Vite, then wins if it's there too.
        assert_eq!(found(r#"{"@ionic/react": "8", "vite": "6"}"#), Some("Vite"));
        assert_eq!(found(r#"{"@ionic/react": "8", "react-scripts": "5", "vite": "6"}"#), Some("Create React App"));
        // Later than Vite in Vercel's list: a Storybook or Parcel project that lists Vite is a Vite
        // site there, and here.
        assert_eq!(found(r#"{"storybook": "8", "@storybook/react-vite": "8", "vite": "6"}"#), Some("Vite"));
        assert_eq!(found(r#"{"parcel": "2", "vite": "6"}"#), Some("Vite"));
        assert_eq!(found(r#"{"vuepress": "2", "vite": "6"}"#), Some("Vite"));
        assert_eq!(found(r#"{"storybook": "8", "sanity": "3"}"#), Some("Storybook"));
        assert_eq!(found(r#"{"vitepress": "1", "vite": "6"}"#), Some("Vite"));
        // Hugo, Jekyll, Middleman and Zola come before Vite: a Vite project with Hugo's config file
        // is a Hugo site on Vercel, and with a `_config.yml` it's Jekyll's, refused here.
        let vite = r#"{"devDependencies": {"vite": "6"}}"#;
        let hugo = project(&[("package.json", vite), ("config.json", r#"{"baseURL": "https://example.org/"}"#)]);
        assert_eq!(detect(&hugo, None).unwrap().unwrap().name, "Hugo");
        let jekyll = project(&[("package.json", vite), ("_config.yml", "title: x\n")]);
        assert!(detect(&jekyll, None).unwrap_err().to_string().starts_with("Jekyll isn't supported yet"));
        // Without a package.json at all.
        assert!(detect(&project(&[("config.rb", "")]), None).unwrap_err().to_string().starts_with("Middleman isn't supported yet"));
        assert!(detect(&project(&[("config.toml", "base_url = '/'\n")]), None).unwrap_err().to_string().contains("glibc 2.35"));
        // Hugo's detector is Vercel's: `baseURL` in config.yaml, .toml or .json, not hugo.toml.
        assert_eq!(detect(&project(&[("hugo.toml", "baseURL = '/'\n")]), None).unwrap(), None);
        assert_eq!(detect(&project(&[("config.toml", "title = 'x'\n")]), None).unwrap(), None);
    }

    #[test]
    fn build_scripts_as_vercel_picks_them() {
        let build = |scripts: &str, deps: &str| {
            let text = format!(r#"{{"scripts": {scripts}, "dependencies": {deps}}}"#);
            detect(&project(&[("package.json", &text)]), None).unwrap().unwrap().build.unwrap()
        };
        let hexo = r#"{"hexo": "7"}"#;
        assert_eq!(build(r#"{"build": "hexo generate", "vercel-build": "x"}"#, hexo), "npm run vercel-build");
        assert_eq!(build(r#"{"now-build": "x"}"#, hexo), "npm run now-build");
        assert_eq!(build(r#"{"build": "x", "now-build": "y"}"#, hexo), "npm run now-build");
        // `vercel-build` alone doesn't count: the preset's own command runs.
        assert_eq!(build(r#"{"vercel-build": "x"}"#, hexo), "hexo generate");
        assert_eq!(build(r#"{"build": 1}"#, hexo), "hexo generate");
        // Servers too.
        assert_eq!(build(r#"{"build": "nuxt build", "vercel-build": "nuxt build --x"}"#, r#"{"nuxt": "4"}"#), "npm run vercel-build");
        // Other runs the first of the three it has.
        let other_build = |scripts: &str| other(&project(&[("package.json", &format!(r#"{{"scripts": {scripts}}}"#))]), None).unwrap().build;
        assert_eq!(other_build(r#"{"now-build": "x", "build": "y"}"#).as_deref(), Some("npm run now-build"));
        assert_eq!(other_build(r#"{"vercel-build": "x"}"#).as_deref(), Some("npm run vercel-build"));
    }

    #[test]
    fn framework_prefixed_system_variables() {
        let env: BTreeMap<String, String> = [
            ("VERCEL", "1"),
            ("VERCEL_URL", "shop-a1.example.dev"),
            ("VERCEL_ENV", "preview"),
            ("VERCEL_PROJECT_NAME", "shop"),
            ("VERCEL_GIT_COMMIT_SHA", "abc"),
            ("VITE_VERCEL_ENV", "mine"),
            ("API_KEY", "k"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        let added = prefixed_env(&env, "VITE_");
        let want: BTreeMap<String, String> = [("VITE_VERCEL_URL", "shop-a1.example.dev"), ("VITE_VERCEL_GIT_COMMIT_SHA", "abc"), ("TURBO_CI_VENDOR_ENV_KEY", "VITE_VERCEL_")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        // The project's own VITE_VERCEL_ENV stays; VERCEL and VERCEL_PROJECT_NAME aren't copied.
        assert_eq!(added, want);
        assert!(prefixed_env(&env, "").is_empty());
        let mut no_url = env.clone();
        no_url.remove("VERCEL_URL");
        assert!(prefixed_env(&no_url, "VITE_").is_empty());
        // Each framework's prefix, servers included; SvelteKit and TanStack Start have none.
        let prefix = |deps: &str| detect(&project(&[("package.json", &format!(r#"{{"scripts": {{"build": "x"}}, "dependencies": {deps}}}"#))]), None).unwrap().unwrap().env_prefix;
        assert_eq!(prefix(r#"{"vite": "7"}"#), "VITE_");
        assert_eq!(prefix(r#"{"react-scripts": "5"}"#), "REACT_APP_");
        assert_eq!(prefix(r#"{"gatsby": "5"}"#), "GATSBY_");
        assert_eq!(prefix(r#"{"astro": "5"}"#), "PUBLIC_");
        assert_eq!(prefix(r#"{"@vue/cli-service": "5"}"#), "VUE_APP_");
        assert_eq!(prefix(r#"{"nuxt": "4"}"#), "NUXT_ENV_");
        assert_eq!(prefix(r#"{"@solidjs/start": "1", "solid-js": "1"}"#), "VITE_");
        assert_eq!(prefix(r#"{"@sveltejs/kit": "2", "@sveltejs/adapter-static": "3"}"#), "");
        assert_eq!(prefix(r#"{"@tanstack/react-start": "1", "nitro": "3"}"#), "");
        assert_eq!(prefix(r#"{"@ionic/react": "8", "react-scripts": "5"}"#), "");
    }

    #[test]
    fn what_the_install_and_the_build_see() {
        // A Create React App project whose install and build write down what they were given.
        let dir = project(&[("package.json", r#"{"dependencies": {"react-scripts": "5"}}"#)]);
        let overrides = serde_json::json!({
            "installCommand": "mkdir -p node_modules/.bin && env > install.env",
            "buildCommand": "env > build.env && mkdir -p build",
        });
        let fw = detect(&dir, Some(&overrides)).unwrap().unwrap();
        let env: BTreeMap<String, String> = [("VERCEL_URL", "a.example.dev"), ("CI", "1"), ("PATH", "/usr/bin:/bin")].into_iter().map(|(k, v)| (k.to_owned(), v.to_owned())).collect();
        struct Quiet;
        impl BuildLog for Quiet {
            fn line(&self, _: buildlog::Stream, _: &str) {}
        }
        fw.build(&dir, Install::Always, &env, &Quiet).unwrap();
        let read = |name: &str| -> BTreeMap<String, String> {
            std::fs::read_to_string(dir.join(name)).unwrap().lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_owned(), v.to_owned())).collect()
        };
        let (install, build) = (read("install.env"), read("build.env"));
        // CI=false for the install only, as Vercel's static-build sets it; the build keeps CI=1.
        assert_eq!((install["CI"].as_str(), build["CI"].as_str()), ("false", "1"));
        assert_eq!((install["REACT_APP_VERCEL_URL"].as_str(), build["REACT_APP_VERCEL_URL"].as_str()), ("a.example.dev", "a.example.dev"));
        // The project's tools are first on the build's PATH, not the install's.
        let bin = dir.join("node_modules/.bin");
        assert!(build["PATH"].starts_with(&format!("{}:", bin.display())), "{}", build["PATH"]);
        assert_eq!(install["PATH"], "/usr/bin:/bin");
    }

    #[test]
    fn hugo_output_as_vercel_reads_it() {
        let out = |files: &[(&str, &str)]| hugo_publish_dir(&project(files));
        assert_eq!(out(&[("config.toml", "baseURL = '/'\npublishDir = 'site'\n")]).unwrap(), "site");
        assert_eq!(out(&[("config.yaml", "baseURL: /\npublishDir: \"out\" # where\n")]).unwrap(), "out");
        assert_eq!(out(&[("config.yaml", "params:\n  publishDir: nested\n")]).unwrap(), "public");
        assert_eq!(out(&[("config.json", r#"{"baseURL": "/", "publishDir": "json"}"#)]).unwrap(), "json");
        // The first of config.json, .yaml, .toml that's there, whichever made it a Hugo site.
        assert_eq!(out(&[("config.json", "{}"), ("config.toml", "baseURL = '/'\npublishDir = 'x'\n")]).unwrap(), "public");
        assert_eq!(out(&[]).unwrap(), "public");
        assert!(out(&[("config.toml", "baseURL = ")]).is_err());
        assert!(out(&[("config.toml", "publishDir = '../up'")]).is_err());
    }

    #[test]
    fn outputs_found_after_the_build() {
        let docusaurus = |files: &[(&str, &str)]| {
            let dir = project(files);
            let fw = detect(&dir, Some(&serde_json::json!({"framework": "docusaurus"}))).unwrap().unwrap();
            fw.public_dir(&dir).strip_prefix(&dir).unwrap().to_path_buf()
        };
        assert_eq!(docusaurus(&[("package.json", "{}"), ("build/my-site/index.html", "<p>")]), Path::new("build/my-site"));
        assert_eq!(docusaurus(&[("package.json", "{}"), ("build/index.html", "<p>")]), Path::new("build"));
        assert_eq!(docusaurus(&[("package.json", "{}"), ("build/a/index.html", "<p>"), ("build/b/index.html", "<p>")]), Path::new("build"));
        let polymer = |files: &[(&str, &str)]| {
            let dir = project(files);
            let fw = detect(&dir, Some(&serde_json::json!({"framework": "polymer"}))).unwrap().unwrap();
            fw.public_dir(&dir).strip_prefix(&dir).unwrap().to_path_buf()
        };
        let built = [("package.json", "{}"), ("build/polymer.json", "{}"), ("build/es6-bundled/index.html", "<p>"), ("build/default/index.html", "<p>")];
        assert_eq!(polymer(&built), Path::new("build/default"));
        assert_eq!(polymer(&[("package.json", "{}"), ("build/polymer.json", "{}")]), Path::new("build"));
        assert_eq!(polymer(&[("package.json", "{}")]), Path::new("build"));
        let angular = |files: &[(&str, &str)]| {
            let dir = project(files);
            let fw = detect(&dir, Some(&serde_json::json!({"framework": "angular"}))).unwrap().unwrap();
            fw.public_dir(&dir).strip_prefix(&dir).unwrap().to_path_buf()
        };
        assert_eq!(angular(&[("package.json", "{}"), ("dist/shop/browser/index.html", "<p>"), ("dist/shop/server/main.js", "")]), Path::new("dist/shop/browser"));
        assert_eq!(angular(&[("package.json", "{}"), ("dist/shop/index.html", "<p>")]), Path::new("dist/shop"));
        // More than the one folder: `dist` itself, as Vercel takes it.
        assert_eq!(angular(&[("package.json", "{}"), ("dist/shop/index.html", "<p>"), ("dist/admin/index.html", "<p>")]), Path::new("dist"));
        assert_eq!(angular(&[("package.json", "{}"), ("dist/shop/index.html", "<p>"), ("dist/3rdpartylicenses.txt", "")]), Path::new("dist"));
        // outputDirectory names the folder itself.
        let dir = project(&[("package.json", "{}"), ("build/es6/index.html", "<p>")]);
        let fw = detect(&dir, Some(&serde_json::json!({"framework": "polymer", "outputDirectory": "build"}))).unwrap().unwrap();
        assert_eq!(fw.public_dir(&dir), dir.join("build"));
    }

    #[test]
    fn requirement_names() {
        assert_eq!(requirement_name("fastapi[standard]>=0.115").as_deref(), Some("fastapi"));
        assert_eq!(requirement_name("Django==5.1").as_deref(), Some("django"));
        assert_eq!(requirement_name("  Flask ; python_version >= '3.9'").as_deref(), Some("flask"));
        assert_eq!(requirement_name("starlette @ https://example.com/starlette-0.41-py3-none-any.whl").as_deref(), Some("starlette"));
        assert_eq!(requirement_name("uvicorn  # the server").as_deref(), Some("uvicorn"));
        assert_eq!(requirement_name("curl_cffi==0.15.0").as_deref(), Some("curl-cffi"));
        for skipped in ["# fastapi", "", "   ", "-r other.txt", "--index-url https://example.com/simple", "-e ."] {
            assert_eq!(requirement_name(skipped), None, "{skipped:?}");
        }
    }

    #[test]
    fn python_api_frameworks() {
        assert_eq!(python_api("fastapi[standard]>=0.115\nhttpx\n"), Some("FastAPI"));
        assert_eq!(python_api("requests\nDjango==5.1\n"), Some("Django"));
        assert_eq!(python_api("\u{feff}Flask\r\ngunicorn\r\n"), Some("Flask"));
        assert_eq!(python_api("litestar[standard]\n"), Some("Litestar"));
        assert_eq!(python_api("quart~=0.20\n"), Some("Quart"));
        // A commented line names nothing, and another file isn't followed.
        assert_eq!(python_api("# fastapi\n-r other.txt\nrequests\n"), None);
        // FastAPI wherever it's listed, when Starlette is pinned beside it; Starlette alone is itself.
        assert_eq!(python_api("starlette==0.41\nfastapi\n"), Some("FastAPI"));
        assert_eq!(python_api("starlette\nuvicorn\n"), Some("Starlette"));
        // A package named after a framework isn't the framework.
        assert_eq!(python_api("flask-cors\ndjango_environ\nfastapi-slim\n"), None);
        assert_eq!(python_api(""), None);
    }

    #[test]
    fn node_api_frameworks() {
        assert_eq!(node_api(r#"{"dependencies": {"zod": "3", "hono": "^4.6"}}"#), Some("Hono"));
        assert_eq!(node_api(r#"{"dependencies": {"express": "5"}}"#), Some("Express"));
        assert_eq!(node_api(r#"{"dependencies": {"fastify": "5"}}"#), Some("Fastify"));
        assert_eq!(node_api(r#"{"dependencies": {"koa": "2", "@koa/router": "13"}}"#), Some("Koa"));
        assert_eq!(node_api(r#"{"dependencies": {"@hono/node-server": "1"}, "devDependencies": {"express": "5"}}"#), None);
        assert_eq!(node_api("{}"), None);
        assert_eq!(node_api("not json"), None);
    }

    #[test]
    fn api_reads_what_the_packager_reads() {
        let spec = |entry: &str, language| Spec {
            entry: entry.to_owned(),
            language,
            memory_mb: None,
            max_duration_s: None,
            concurrent: None,
            warnings: Vec::new(),
            config: None,
            framework: None,
            ignore_command: None,
            routing: None,
            public: None,
            bulk_redirects: None,
            schedules: Vec::new(),
            backend: None,
            version: None,
            functions: Vec::new(),
            floor: None,
        };
        // The root's requirements.txt before the one beside the entry, as pip is given it.
        let both = project(&[("api/main.py", ""), ("requirements.txt", "Django==5.1\n"), ("api/requirements.txt", "fastapi\n")]);
        assert_eq!(api(&both, &spec("api/main.py", Language::Python)), Some("Django"));
        let beside = project(&[("api/main.py", ""), ("api/requirements.txt", "fastapi\n")]);
        assert_eq!(api(&beside, &spec("api/main.py", Language::Python)), Some("FastAPI"));
        let node = project(&[("server.mjs", ""), ("package.json", r#"{"dependencies": {"hono": "4"}}"#)]);
        assert_eq!(api(&node, &spec("server.mjs", Language::Node)), Some("Hono"));
        // A Python project isn't judged by a package.json it happens to have, nor a Node one by requirements.txt.
        let mixed = project(&[("api/main.py", ""), ("server.mjs", ""), ("requirements.txt", "requests\n"), ("package.json", r#"{"dependencies": {"express": "5"}}"#)]);
        assert_eq!(api(&mixed, &spec("api/main.py", Language::Python)), None);
        assert_eq!(api(&mixed, &spec("server.mjs", Language::Node)), Some("Express"));
        assert_eq!(api(&project(&[("api/main.py", "")]), &spec("api/main.py", Language::Python)), None);
        // The repo's own test apps: FastAPI, and a plain `node:http` server with no package.json.
        let apps = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-apps");
        assert_eq!(api(&apps.join("python-fastapi"), &spec("api/main.py", Language::Python)), Some("FastAPI"));
        assert_eq!(api(&apps.join("node-http"), &spec("server.mjs", Language::Node)), None);
    }

    #[test]
    fn presets_routes_are_vercels() {
        // Every preset's routes are JSON that makes a program, and only Nitro's builds keep files
        // for good by folder: a plain site's `/assets/` is revalidated, as on Vercel.
        for preset in PRESETS.iter().filter(|p| !p.routes.is_empty()) {
            let list: Value = serde_json::from_str(preset.routes).unwrap_or_else(|e| panic!("{}: {e}", preset.name));
            let program = forkrail_common::rules::Program::from_vercel(&serde_json::Map::new(), &Default::default(), Some((preset.name, &list)));
            assert!(program.unwrap_or_else(|e| panic!("{}: {e}", preset.name)).is_some(), "{}", preset.name);
        }
        assert!(NITRO_HASHED.contains(&"/assets/"));
    }
}
