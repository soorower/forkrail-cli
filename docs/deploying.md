# Deploying an API to forkrail

How to put a Vercel-style Python API (FastAPI, laid out as `api/main.py` exporting `app`) on forkrail, test it, and move real users to it. The examples use a project called `my-api`; any project with one Python function works the same way. Websites built with SvelteKit, Astro, Nuxt, TanStack Start, React Router or Remix, and static sites, are in section 9.

What you get:
- a preview at `https://<project>-<id>.forkrail.com` on every deploy, and production at `https://<project>.forkrail.com`
- each deployment is a Lambda version, so it costs nothing while idle, and a rollback is instant
- your own domain (`api.example.com`) on production, with its own certificate (section 6)
- the app's output (`print`, tracebacks) in the router's log, with `test-env.sh logs`; every request logged, copied to S3 every 10 minutes, and alarms that mail you (section 7)

Builds run on your machine. Nothing in the project folder is changed: the CLI copies the files it packages.

## 1. One-time setup

You need:
- Rust with `cargo-zigbuild` and zig (all from Homebrew), and Python 3.13 (`brew install python@3.13`, which provides `python3.13`)
- AWS credentials for the account forkrail runs in, such as an `aws login` profile. When a session expires, the CLI says so, and `aws login --profile NAME` renews it.

Tell the CLI once which profile and home region reach your install; it checks that the account has one, then saves them in `~/.config/forkrail/config.json`:

```bash
forkrail link --profile forkrail --region us-east-1
```

After that, commands need no `--profile` or `--region`. A flag, `AWS_PROFILE` or `FORKRAIL_HOME_REGION` still overrides what was saved. With nothing saved, the CLI uses the AWS SDK's default credentials in us-east-1.

Build the runtime (the Lambda `bootstrap`, for Linux arm64) and the CLI:

```bash
cd forkrail   # your clone of this repository
```

```bash
cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.34 -p forkrail-runtime
```

```bash
cargo build --release -p forkrail-cli
```

Rebuild both after pulling changes to forkrail. The CLI finds the runtime next to its own target directory, so it works from any folder. Add an alias to `~/.zshrc`:

```bash
echo "alias forkrail=\"$PWD/target/release/forkrail\"" >> ~/.zshrc && source ~/.zshrc
```

Check it:

```bash
forkrail ls
```

## 2. Keep secrets and test scripts out of the package

The package is everything in the folder except `.git`, `.vercel`, `__pycache__`, `.venv`, `venv`, `node_modules`, `.DS_Store`, `.env*`, and what `.vercelignore` lists (gitignore syntax, the same file Vercel reads). If `my-api` has a `test.py` that isn't part of the API, list it:

```bash
cd ~/code/my-api && printf 'test.py\n' >> .vercelignore
```

Vercel reads the same file, so its deploys leave `test.py` out too, which changes nothing for the API.

Env vars, if the app needs any, go in per project and environment. Values come from stdin (or a prompt) and are never printed:

```bash
forkrail env add my-api SOME_KEY --target production
```

Production's and Preview's take effect on the next deploy, and their values are never read back: not by the dashboard, not by the CLI. `forkrail env ls my-api` lists the names.

**Names.** Letters, digits and `_`, not starting with a digit, as on Vercel; the platform keeps `AWS_LAMBDA_*`, `FORKRAIL_*`, `PATH`, `HOME`, `TEMP`, `TZ`, `PYTHONPATH` and a few of Lambda's own. A project may set the AWS SDK's `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, `AWS_REGION` and `AWS_DEFAULT_REGION`, as Vercel allows: Lambda keeps those names for the function's own role, so they reach the app through the sealed file below, and when the project sets its own keys without a session token the role's token is kept from the app. (A project role, "Calling AWS from the app" in section 9, is still the better way to reach AWS.)

**System variables.** Every deployment's app is told about itself, under Vercel's names so code moved from Vercel reads what it expects, with forkrail's values: `VERCEL=1`, `VERCEL_ENV` and `VERCEL_TARGET_ENV` (production or preview), `VERCEL_URL` (the deployment's own host), `VERCEL_BRANCH_URL` (a branch preview's host), `VERCEL_PROJECT_PRODUCTION_URL`, `VERCEL_REGION` (`iad1`, `sfo1` … where the environment runs), `VERCEL_DEPLOYMENT_ID`, `VERCEL_PROJECT_ID` and `VERCEL_PROJECT_NAME` (the project's name), `VERCEL_AUTOMATION_BYPASS_SECRET` (when a bypass secret is set as an env var), and for a commit `VERCEL_GIT_PROVIDER`, `_REPO_OWNER`, `_REPO_SLUG`, `_COMMIT_REF`, `_COMMIT_SHA`, `_COMMIT_MESSAGE` (2 KB at most), `_COMMIT_AUTHOR_NAME` and `_PULL_REQUEST_ID`. forkrail's own names carry the main ones: `FORKRAIL_ENV`, `FORKRAIL_URL`, `FORKRAIL_BRANCH_URL`, `FORKRAIL_PROJECT_PRODUCTION_URL`, `FORKRAIL_REGION` (`us-east-1` …), `FORKRAIL_DEPLOYMENT_ID`, `FORKRAIL_PROJECT_NAME`, `FORKRAIL_GIT_COMMIT_REF`, `FORKRAIL_GIT_COMMIT_SHA`. Builds get the same, plus `CI=1` and `VERCEL_GIT_PREVIOUS_SHA`. Node apps (servers, `api/` functions, frameworks' functions) also run with `NODE_ENV=production`, previews too, as Vercel runs its functions (Express then leaves stack traces off its error pages, React uses its production build). A project's own variable of the same name wins. `VERCEL_SKEW_PROTECTION_ENABLED` is set while skew protection is on (section 9). Not given: `VERCEL_OIDC_TOKEN` (a project role does that job). Since a preview is told it's a preview, promoting one publishes its build again with production's variables (Vercel builds it again); a build made by an older runtime, published again, goes without the system variables if they don't fit beside the project's in Lambda's 4 KB (the deploy says so). Without `--target` a variable is set for production and previews; `--target` (or `-e`, `--environment`) takes `production`, `preview`, `development` or several comma-separated.

**Development** variables are for running the app on your own machine; no deployment uses them, and they're the ones whose values come back:

```bash
forkrail env add my-api API_URL -e development
forkrail env pull                    # .env.local, for the project named after this folder
forkrail env pull .env --project my-api --environment preview
forkrail env run -- next dev         # the same values, with no file written
```

A file that's there is merged, after a yes (`--yes` for scripts): variables only in it are kept, and a production or preview variable, whose value can't be read back, keeps the file's own value or is written empty to fill in. A new file is readable by you only; the CLI warns when git doesn't ignore it. `env run` leaves a variable already set in your shell as it is. Reading values back takes a developer's role or more; a viewer sees names.

**One branch's previews.** A preview variable can be for one Git branch, as on Vercel: its value wins over preview's own of the same name for that branch's previews, which still get every other preview variable, so a branch needs only the values it changes. The branch is the one a deployment was made from: a push's, or for `forkrail deploy` the branch checked out (none for a detached checkout). Production deployments never get one, and a connected project's production branch can't have any.

```bash
forkrail env add my-api API_URL --git-branch feature/logs     # preview only; -e preview may be given
forkrail env ls my-api                                        # a line per branch after the environments
forkrail env rm my-api API_URL --git-branch feature/logs      # without --git-branch, a branch's own stay
forkrail env pull --git-branch feature/logs                   # what its previews get (names: preview's are secrets)
```

**How much.** A project's variables may take 64 KB per environment, names and values together, as on Vercel (for a branch's previews, preview's and the branch's own together); one value may take all of it. Lambda's own environment holds 4 KB, so a deployment whose variables don't fit there carries them in a sealed file instead: encrypted (AES-256-GCM) with a key made for that deployment, shipped as a version of the Lambda layer `forkrail-env-<project>` and deleted as soon as the function version using it is published (Lambda keeps a deleted layer version's content for the functions already using it, so nothing else can fetch the file). The key sits in the function's environment, encrypted with the install's key like any other variable, and the runtime opens the file when an environment starts and hands the variables to the app (never the key). Nothing changes for a project under 4 KB. Only a package built with a runtime from this release or later can carry a sealed file: publishing an older build with more than 4 KB is refused, and a deploy builds it again. `forkrail sweep` removes any layer version a failed publish left. A deployment with a sealed file can't be copied to another region later (`regions copy` says so); it runs where it was published until it's deployed again.

**Where the values are.** Each value is an SSM SecureString parameter (`/forkrail/env/<project>/<target>/<KEY>`, a branch's under `/forkrail/env/<project>/branches/<branch>/<KEY>` with every character SSM doesn't allow in a name written as `_` and its hex, `feature/logs` as `feature_2Flogs`; one longer than 4 KB is kept in pieces, `<KEY>`, `<KEY>.1`, …), encrypted with forkrail's own KMS key, and each function's environment is encrypted with the same key (one per region: `deploy/install/secrets.yaml` in the home region, made by the region's setup elsewhere). Project files in S3 keep only the names. Only someone IAM allows `kms:Decrypt` on that key can read a value: the CLI's admin credentials can; the routers' role can't (the key refuses it outright), nor the dashboard's, nor the apps'. The keys cost $1 a month each; the parameters are free. `forkrail env migrate` moved the values out of the project files on 2026-11-11; it's safe to run again, and an older forkrail refuses a moved project rather than deploy it without its env vars.

**The install's own secrets** (the routers' Cloudflare token and challenge key, the ClickHouse passwords) are SSM SecureStrings under a second key, `alias/forkrail-install` (`deploy/install/install-secrets.yaml`, home region). Each reader may decrypt its own and nothing else: the routers their three, the analytics box the three ClickHouse passwords, the dashboard the reader and build-log passwords. The routers' and the analytics box's roles also refuse every other parameter in the account, although AWS's managed policy for Run Command would allow it. Apps, builds and the screenshot function can't decrypt with either key. Set it up once, after the readers' policies:

```bash
deploy/aws/stack.sh apply install-secrets
deploy/aws/test-env.sh roles           # the routers' role (one for every region)
deploy/aws/analytics.sh role && deploy/aws/dashboard.sh role
deploy/aws/install-secrets.sh move     # values piped SSM to SSM, checked by hash, never printed
deploy/aws/install-secrets.sh status
```

The scripts that make these secrets put them under that key from the start.

## 3. A preview

The folder name has a space, so name the project. `vercel.json` says 512 MB; deploy at 640 MB, the size forkrail's APIs run at:

```bash
forkrail deploy ~/code/my-api --project my-api --memory 640
```

The first deploy takes a minute or so (pip for Linux arm64); later ones reuse the installed requirements unless `requirements.txt` changes. It prints the preview URL, `https://my-api-<id>.forkrail.com`.

The deployment shows in the dashboard as soon as the deploy starts, as **Building**, with its build log live: the steps (preparing, installing dependencies, building, packaging, publishing) and every line, as in the terminal. It ends **Ready**, **Error** (with the step that failed and why; the log stays there to read) or **Canceled** (Ctrl-C while it builds; once publishing has started it finishes). Builds stop after 45 minutes, as Vercel's do, and a deployment still building after 50 (its terminal was closed, say) reads as failed. In the log, any env var value of 8 characters or more is shown as `[NAME hidden]`; the terminal shows lines as they are. Logs are kept as on Vercel: up to 4 MB each, for as long as the deployment is. `forkrail inspect <id> --logs` prints one that has ended.

### Where a build runs

```bash
forkrail deploy ~/code/my-api --project my-api --memory 640
```

Every deploy builds in the install's builder (`--local` builds on your machine instead, with the install's AWS credentials; `--prebuilt` and `--bootstrap` imply it), a Lambda function in your AWS account (`forkrail-builder`), as Vercel builds on its machines rather than yours. The terminal uploads the project's files and shows the log as it's written; closing it doesn't stop the build. What that changes:

- **The build runs where the app runs:** Lambda's own Amazon Linux 2023 on arm64, with the Python and Node versions the project asks for (section 9a), Go, and Rust with zig. pip installs as it does on Vercel, so a release that only has a source distribution builds from it, instead of forkrail settling for an older release that has a Linux wheel, as a build on a Mac must. The image has no C compiler, so a package that needs one to build from source fails (loudly).
- **What's uploaded** is what a git checkout of the project would have: the files `.gitignore` and `.vercelignore` leave, without dependencies, build output and caches, and never `.env.local` or `.env.*.local` (Vercel never uploads those either). A committed `.env` or `.npmrc` goes, since the build may need it. The upload is encrypted under the install's env key, like env var values, so the routers can't read it, and it's kept for rebuilds. At most 100,000 files and 1 GB.
- **A build has 15 minutes**, Lambda's limit, not Vercel's 45; every app so far builds in 1-3. Build a longer one on your machine (`--local`).
- **A build gets 4,096 MB, and 8,192 MB if it needs them** (Vercel's standard build machine has 8 GB). A build that runs out of the builder's memory (its build command killed, or the builder itself) isn't failed: it's built once more on the large builder (`forkrail-builder-large`), in the same deployment, its log going on after the first build's lines with a line saying why. From then on the project's builds start there (`large_builds` in its project file), so they don't run out first. A build that runs out on the large builder too fails, saying so; nothing is tried a third time. The sizes are the builder stack's `MemorySize` and `LargeMemorySize` (at most Lambda's 10,240 MB; `LargeMemorySize=0` makes no large builder). Neither costs anything between builds; a second on the large builder costs twice one on the builder, and its builds run faster.
- **Dependencies are kept between builds**, as on Vercel. After a build that succeeded, the builder keeps the project's `node_modules` folders (a site), or its installed, compiled Python requirements (an API), in the install's bucket (`cache/<project>/`), and the next build starts from them. A site's install command still runs every time, as Vercel's does, and has little to do: the dashboard's `npm install` takes 1-3 s that way instead of 10-16. A Python API's requirements are used again while `requirements.txt` says the same, and installed again when it changes. Production builds use only what production builds kept; a preview uses the previews', or production's until there's one. A cache is used for 30 days, then built fresh, so an unpinned requirement moves to its newest release at least that often. `--no-cache` on `deploy` or `redeploy` builds without it and keeps a new one, like Vercel's `--force`. A custom `installCommand` that deletes `node_modules` first (`npm ci`) gains nothing from it. Go keeps its module and build caches, Rust Cargo's registry and target folder, with their files' times (Cargo goes by them): a Go build drops from about 20 s to 2, a small Rust one from about 75 s to 24. A Next.js build also keeps its `.next/cache`, in an archive of its own (`cache/<project>/<target>.next.tar.zst`) written after every build, not only when the dependencies change: see "`.next/cache`" under Next.js.
- **Redeploy builds it again** from that upload, as Vercel's does, with the project's env vars and settings as they are now. A deployment built on a machine has no upload, so its redeploy publishes its build again, as before.
- **Cancel** in the dashboard, or `forkrail cancel <id>`, stops it within a few seconds; nothing is published. Ctrl-C in the terminal only stops showing the log.

The builder can do nothing in the install by itself: it reads its order and source, and writes its output, through URLs signed for that one build, and it writes no CloudWatch logs. The project's code it runs therefore can't reach the install's state, functions, secrets or other projects' builds. Between builds of different projects everything a build left behind is deleted, and every process it started is stopped. Set it up once with `deploy/aws/builder.sh repo` (the image repository) and `deploy/aws/builder.sh release` (the image and the function, `deploy/install/builder.yaml`); `release` again to pick up AWS's patched base images.

Call it the way you call the Vercel URL, with your own key and headers. The first request after a while idle is a cold start (1–2 s for a FastAPI app); then requests take about as long as on Vercel.

If something fails, look at the app's output:

```bash
deploy/aws/test-env.sh logs my-api "10 min ago"
```

Everything the app prints (`print`, tracebacks, whatever its middlewares log) goes to the router's log on its EC2 instance, as it went to Vercel's logs before. Check what your app prints before sending real traffic.

### APIs that need their own IP per request

By default forkrail runs concurrent compute: the router packs many requests onto each worker (up to 10), so requests in flight at the same time share one Lambda environment and one outbound IP. An API that needs each concurrent request to leave from its own IP (to spread calls to an upstream across addresses, say) should turn concurrent compute off and run one request per worker instead: classic Lambda, or a Vercel project with fluid compute off. Say so in `vercel.json` with `fluid`, Vercel's key for its own version of this, which Vercel reads too:

```json
"fluid": false
```

or as the project's setting, `forkrail project my-api --concurrent false` (or Concurrent compute in the dashboard's project settings), or for one deploy with `--concurrent false`. The flag wins over `vercel.json`, and `vercel.json` over the project's setting. The deploy prints `one request per worker`, and `forkrail inspect <id>` shows `"fluid": false` (deployment records keep the setting under `vercel.json`'s key). Prefer `vercel.json` or the project setting: a later deploy without the flag would run concurrent compute again. The CLI warns when a deploy or `promote` would replace a production deployment that runs one request per worker with a concurrent one. `--fluid` still works as another name for `--concurrent`.

In a test, 20 requests at once got 20 environments and 20 different IPs; requests that follow one another on an environment share its IP, as on Lambda. It bills about what classic Lambda does (0.6% more with 1 s requests), so more than concurrent compute for an I/O-bound API.

Between the two, concurrent compute can give each worker fewer requests than its 10, or more:

```bash
forkrail project my-api --max-inflight 3   # 2 to 50; `default` is 10
```

(or Max requests per instance under Concurrent compute in the dashboard's project settings, which offers about one per 50 MB of the function's memory: 15 at 768 MB, up to 50 from 2,560 MB). Each worker then takes at most 3 requests at once, so no more than 3 share its environment and outbound IP, and more workers start as traffic needs them, each counting against the account's Lambda concurrency (below). Unlike the other function settings it applies within seconds to the project's deployments that run concurrent compute, with no redeploy, and to later deploys; a deployment that runs one request per worker keeps one. A Python WSGI app (Flask, Django) runs at most 10 requests at once in each worker whatever the number; an ASGI app (FastAPI) runs `async` routes on its event loop, so a blocking call in one holds up the others.

A deployment may run as many workers as the AWS account's Lambda concurrency allows (400, shared with everything else in the account), so with concurrent compute off that many requests at once. What counts is requests in flight, about requests a second times seconds each: 100 a second at 1–2 s each is 100–200 at once. Past the account's limit, Lambda throttles new invocations and requests wait for a worker for up to 15 s, then get 503. To cap one project, so it can't take the whole account:

```bash
forkrail limits my-api --max-workers 300
```

It applies to the project's deployments within 5 s, with no redeploy, and to later deploys; `forkrail limits my-api` shows it, and `--max-workers default` removes the cap. The router can also cap every deployment (`FORKRAIL_POOL_MAX_WORKERS`) or all of them together (`FORKRAIL_MAX_WORKERS`); neither is set by default.

Memory buys CPU (a full core at 1,769 MB, in proportion below), and with one request per worker each request has its worker's CPU to itself. An API that mostly waits on other servers runs as fast at 256 MB as at 640 MB, for 40% of the price; only its own CPU work slows down, by 1,769 ÷ memory. Cold starts don't slow down at small sizes. Deploy a preview at the size `vercel.json` says, run the API's tests, and look at `cpu_us` per route in `test-env.sh report`: a route that spends 20 ms of CPU takes about 55 ms of it at 640 MB and 140 ms at 256 MB.

## 4. Production

Deploy again with `--prod`:

```bash
forkrail deploy ~/code/my-api --project my-api --prod --memory 640
```

It prints the deployment's own URL and `https://my-api.forkrail.com`, which now points at it. Test that the same way.

Why `--prod` and not `forkrail promote <id>`: a deployment keeps the env vars it was built with, so a promoted preview would serve production with the preview's env. `promote` refuses a preview whose preview env differs from production's (it names the variables, never their values), unless you pass `--allow-preview-env`. `forkrail redeploy <id> --prod` publishes the preview's build again as a production deployment with production's env vars, which is what the dashboard's Promote offers in that case, and what Vercel does. A Next.js build holds the env it was built with (`NEXT_PUBLIC_*` in its client code, every variable its prerendered pages read), so for one only building it again changes them: the dashboard's Promote and `redeploy --prod` build it again from its source in the builder, as Vercel's Promote always does ("a complete rebuild"); one deployed with `--local` has no source in the install, so they refuse and say to deploy its source with `--prod`. Promoting a production deployment, and `rollback`, are always fine.

**Warm before it's live.** A new deployment has no running environments, so its first requests would each wait a second or more for one to be created and the app to start. Before production is pointed at a deployment (a production deploy, a promote, a rollback), forkrail starts three environments of it in every region it runs in and lets the app start in each, without sending the app any request; five for a project with concurrent compute off. The deploy's log says how it went:

```
==> warm-up in us-east-1: 3 of 3 environments ready in 1.6 s (3 new, the app started in 739 ms)
```

Its first requests then wait some 5-35 ms where they'd have waited a second. Lambda keeps those environments for five to ten minutes unused. A warm-up never stops production from moving: if the app didn't start, or Lambda had no room, the log warns and the first requests start as they used to. A rollback moves production at once and warms meanwhile. `FORKRAIL_WARM=N` warms N environments in each region instead (at most 50), and `FORKRAIL_WARM=0` none.

### Function settings

Memory, maxDuration and concurrent compute come from, in order: deploy's flags (`--memory`, `--concurrent`), the deployment's `vercel.json` (`functions`, `fluid`), the project's settings, and forkrail's defaults (640 MB, 300 s, concurrent compute on). The project's settings are Vercel's project settings: they apply to the next deploy or redeploy, never to deployments that exist.

```bash
forkrail project my-api --memory 768 --max-duration 120 --concurrent true   # `default` clears one
forkrail project my-api                                                      # shows them
```

The dashboard's project settings (Settings → Functions) change the same ones. A redeploy takes them the same way as a new deploy of the same files: its own `vercel.json`, then the project's settings as they are now, then the defaults. A project put back to the default gets the default on its next deploy or redeploy, whatever the deployment had. The memory is the Lambda function's in every region the deployment is published in, so each region's copy runs at the same size. `--memory` on `forkrail deploy` is for that one deployment; a redeploy of it doesn't keep it, so set the project's memory to keep a size.

A project of functions in `api/` (section 9a) runs them in one Lambda function: `functions` patterns set each one's memory and maxDuration, the shared function gets the largest of each (and at least the project's settings, when some functions set none), and the router holds each function to its own maxDuration.

A request body is limited as on Vercel: one that would make the function's payload 6,000,000 bytes or more, counting the body as base64 (4/3 of its size) and the request's path, query and headers as they are, gets `413 FUNCTION_PAYLOAD_TOO_LARGE` before any function runs. For an ordinary request that's a body of about 4.49 MB (Vercel's documented "4.5 MB"); large headers or a long query make it a little less. A body sent without a length is read in full before the function starts, as Vercel reads every body, and refused once it passes the limit. Over HTTP/1.1 the rest of a refused body is read and thrown away before the 413 (up to 64 MB more, for up to 30 s), so a client that sends the whole body before reading the answer gets the 413 and can keep the connection; a client that waits for `100 Continue` gets the 413 at once and sends nothing. Answers have no limit: they stream.

**Calls back to the install.** A request a function sends to one of the install's own hosts (its own project or another project, by a deployment's host or a custom domain) carries `x-forkrail-loop`, which forkrail's runtime adds to `fetch`, `node:http`/`node:https`, and Python's `requests`, `httpx`, `aiohttp`, `curl_cffi` and `urllib`; a call anywhere else goes out exactly as the app made it. When such calls nest more than 5 deep, or one chain makes more than 1,000 requests at depth 2 and deeper, the next one gets `508 INFINITE_LOOP_DETECTED` before anything of the deployment runs, as Vercel stops a function that calls itself without end: the call fails at once, and the chain above it unwinds. A page fetching its own API route is at depth 1 and never affected, nor is a schedule calling its own API for each of thousands of items. Calls made another way (raw sockets, `curl` in a subprocess, by IP address) carry no mark; the spend budget covers those.

### Function regions

A new project's functions run in one region, as on Vercel: the install's home region (us-east-1, Vercel's `iad1`). After that a project stays where its production runs until it picks regions, so the projects from before function regions, which run in every router's region, keep doing so. Pick regions where the function's database or the APIs it calls are, so each request's calls to them stay short:

```bash
forkrail project my-api --regions iad1               # Vercel's names or AWS's (us-east-1)
forkrail project my-api --regions us-east-1,fra1     # more than one: each visitor gets the nearest
forkrail project my-api --regions default            # no setting: where production runs now
```

Or in `vercel.json`, which wins over the project's setting, as on Vercel: `"regions": ["iad1"]`. The dashboard's project settings (Regions) change the same setting. Like the other function settings it applies from the next deploy or redeploy; a redeploy keeps its own `vercel.json`'s.

Any of Vercel's 20 regions can run functions, routers or not: `iad1` (us-east-1), `cle1` (us-east-2), `sfo1` (us-west-1), `pdx1` (us-west-2), `yul1` (ca-central-1), `gru1` (sa-east-1), `fra1` (eu-central-1), `dub1` (eu-west-1), `lhr1` (eu-west-2), `cdg1` (eu-west-3), `arn1` (eu-north-1), `bom1` (ap-south-1), `sin1` (ap-southeast-1), `syd1` (ap-southeast-2), `hnd1` (ap-northeast-1), `icn1` (ap-northeast-2), `kix1` (ap-northeast-3), and the AWS opt-in regions `hkg1` (ap-east-1), `cpt1` (af-south-1) and `dxb1` (me-central-1), which the account must turn on first. Visitors reach the nearest router; that router runs the function's version in its own region if the project has one there, and otherwise in the nearest region the project picked: through that region's router when it has one, else on workers started there that dial back to it. So a router in a region speeds up its visitors twice: static files and cached pages are answered there, and a project that runs its functions there too answers them without crossing an ocean. A function region with no router still helps whatever the function calls from there.

**Between regions.** A router with no copy of the function sends the request to the router of the function's region, when there is one, over the link the routers keep open to each other (port 7445, the one cached pages use; one HTTP/2 connection, opened once, with no compression), and that router runs it on its own workers, beside them. Each request is one stream on that connection: one round trip between the regions (about 62 ms between us-west-1 and us-east-1) plus the other router's own start (2 ms on a warm worker, 17 to 20 ms on a thaw), and both routers' requests share its warm workers. Firewall, rate limits, bot checks, the cache and the access log stay with the router the visitor reached; the invocations, the app's log lines (by the request's id) and keep warm are the running router's. While the link is down, for a request body too large to send twice, and for a region with no router, the router instead calls Lambda's Invoke API in that region once per worker, and the worker dials back over TLS and keeps that connection for its whole life. A worker started that way pays about three round trips between the regions before its first request (the invoke, then the connection's handshakes): measured 194 ms against 17 ms in the router's own region, which is why the link is used when there is one. The traffic uses public addresses, but between AWS regions it stays on AWS's own network. It costs $0.02/GB instead of the $0.01/GB a router pays to a worker in its own region (spend budgets count it so; through the link the running router's own $0.01/GB to its worker is added). Vercel's edge makes the same trip, from the point of presence nearest the visitor to the function's region, and bills it as Fast Origin Transfer ($0.06/GB in iad1). So pick one region when the function's data is in one place (a request's calls to it outweigh one hop), and a region near each group of visitors when it isn't. `FORKRAIL_RELAY=0` on a router makes it start workers in the other region itself, as before.

**Which way, decided ahead.** Every router knows the install's routers, in memory: those `FORKRAIL_PEERS` names, and those that say where they are in the state bucket (`state/routers/<region>/<host:port>`, written by each router when it starts and every 10 minutes, read with one LIST every 5; one silent for 30 minutes is taken as gone). So a router added later is known to the others without a restart. How a deployment's requests reach its function is settled when the router makes its pool for it, not per request: its version in the router's own region runs on the router's workers; in another region with a router, through that router (the default wherever there is one); in a region with none, invoked there from the router that took the request. A request then reads one pointer to find the link. When the routers change, only the pools whose way changes are made again. Among a deployment's regions, a router picks the nearest, counting one without a router as three times as far (a new worker from there costs about three round trips, the link one), so a region with a router wins unless one without is much nearer.

A region runs functions once it has a code bucket and the install's env var key ($1 a month). Picking a region for a project sets it up the first time: saving the dashboard's setting, or `forkrail project NAME --regions`, makes both there (the control function may make only a bucket named like the install's and a key tagged `forkrail-key=env`; `deploy/install/control.yaml`). `forkrail regions setup eu-central-1` does the same ahead of time. An opt-in region has to be turned on for the account first, and is refused until it is. `forkrail uninstall` removes both again, in every region. `forkrail regions ls` lists the regions with a router or functions, how many deployments run in each and how many projects pick each.

**Routers** are the install's: `forkrail regions add us-west-1` says one runs there, so every deploy sends its static files and routing rules to that region's bucket as well (`regions add` sends production's and its rollback targets' at once); `regions rm` stops that. Before function regions, every deployment's function was published in every router's region; those projects have no setting, so they keep running there until one picks its regions. A new router region isn't added to them: a project picks it to run there.

### Staged production deployments ("Auto-assign custom domains")

On by default: a production deployment takes the project's production domains (`<project>.<base>` and its custom domains) as soon as it's ready. Turned off, every production deployment waits instead, as Vercel's staged deployments do: it's published and answers on its own `<project>-<id>.<base>`, production stays where it is, and **Promote** puts it live in seconds, without building it again (it's already built with production's env vars).

```bash
forkrail project my-api --auto-assign-domains false     # true or `default` turns it back on
forkrail deploy DIR --prod                              # staged: "production is unchanged"
forkrail promote <id>                                   # live
```

Every way a production deployment is made follows it: `forkrail deploy --prod` (here or through a login), a push to the production branch, Redeploy as production, a deploy hook. The dashboard has it under Settings → Deployments, lists the deployments waiting there with a Promote button, and marks them **Staged** in the deployments list. `--skip-domain` (and `github.autoAlias: false`) stage a deployment whatever the setting. Promoting a preview whose env vars differ (which publishes it again for production) always goes live: it was asked for by name. Changing the setting is a member's or an owner's.

### Schedules

A project's production deployment can be called on a schedule: `crons` and `schedules` in `vercel.json`.

```json
{
  "crons": [{ "path": "/api/sync", "schedule": "0 5 * * *" }],
  "schedules": {
    "daily-report": { "schedule": "0 9 * * 1-5", "path": "/api/report", "requestMethod": "GET", "timezone": "America/New_York", "jitter": 5 }
  }
}
```

- `crons`: a GET to `path`, in UTC, within the minute.
- `schedules`: a named schedule with its own IANA `timezone` (UTC by default), `requestMethod` (`GET` or `POST`, POST by default) and `jitter` (1-15 minutes, 1 by default: the run starts at some point in that window, and the window can't be longer than the time between runs). Only `path` targets so far: `function`, `service` and `topic` are refused.
- Expressions have five fields (minute, hour, day of the month, month, day of the week, Sunday = 0), with digits, `*`, `,`, `-` and `/`; no names such as `MON`, and not a day of the month and a day of the week together. At most 100 a project, both keys together. A static site has none (there's no function to call).
- Each run is a request to the production deployment (`<project>.forkrail.com`), logged like any other (entry `schedule`, with the schedule's key and the run's id). No retries, redirects aren't followed, and the deployment's maxDuration applies. Runs that overlap aren't stopped: make the handler safe to run twice, as a run can (rarely) arrive twice or be skipped.
- Set `CRON_SECRET` (an env var) and each run sends `Authorization: Bearer <CRON_SECRET>`; check it in the handler, since the path is public. The value comes from the deployment that runs, so a rollback brings back the old secret with the old schedules, and changing it takes a deploy. A value a header can't carry (a line break, spaces at either end) fails the deploy.
- Only production runs them: a deploy, promote or rollback brings the schedules of the deployment that's production now. Previews never run them.
- A run that can't start on time starts late, up to its jitter plus the time between runs (at least 10 minutes); later than that it's skipped and logged (`schedule_expired`). Monthly and yearly crons never expire.

```bash
forkrail schedules ls my-api                  # on or off, next runs (alias: forkrail crons)
forkrail schedules run my-api daily-report    # now, whatever its state; a cron by its key from ls
forkrail schedules disable my-api --all       # stays off across deploys and rollbacks; or one by name
forkrail schedules enable my-api --all
```

The dashboard's Schedules page does the same, with each schedule's runs, and the logs page filters by schedule. Anyone on the team sees them; members and owners run and change them.

## 5. Real users

Change the API's base URL from the Vercel URL to `https://my-api.forkrail.com` wherever it is called: an API marketplace's settings, your own apps, partners' configuration. Keep the Vercel deployment: switching the base URL back is the fallback.

Then watch it for a few days:

```bash
deploy/aws/test-env.sh report "1 day ago"
```

per project: requests by status, p50/p95/p99, retries and 5xx, workers and cold starts, and the estimated Lambda, egress and transfer cost. Compare with `vercel usage --group-by project` for the days before the switch.

## 6. Your own domain

A project serves its production deployment on any domain you give it: `api.example.com`, `example.com` itself, or a wildcard `*.example.com` (one label below it). A domain can also follow a git branch's newest preview, or redirect to another of the project's domains. The dashboard's Domains page does all of this; so does the CLI, signed in or with `--aws`:

```bash
forkrail domains add api.example.com shop
forkrail domains add example.com shop
forkrail domains add www.example.com shop --redirect example.com --status 308
forkrail domains add staging.example.com shop --branch staging
forkrail domains add '*.example.com' shop
forkrail domains ls [PROJECT]
forkrail domains inspect api.example.com
forkrail domains update www.example.com --production
forkrail domains check api.example.com
forkrail domains rm api.example.com
```

`add` and `inspect` print the DNS records the domain needs at its DNS provider:

| Record | For | When |
|---|---|---|
| `api.example.com CNAME shop.forkrail.com` | pointing it here | a name under a registered domain |
| `example.com A` at the install's two addresses | pointing it here | the registered domain itself (`@`), which can't be a CNAME |
| `_acme-challenge.api.example.com CNAME <hash>._acme.forkrail.com` | its certificate before DNS points here | optional; required for a wildcard |

**Certificates.** The router in us-east-1 gets each domain a Let's Encrypt certificate, proving the domain is yours the first way that works:

1. the domain is in a Cloudflare zone the install's token can edit (the token in SSM `/forkrail/cloudflare-api-token`; give it DNS edit on that zone in Cloudflare → My Profile → API Tokens): a TXT record there;
2. its `_acme-challenge` name is a CNAME to the `<hash>._acme.<base domain>` that `add` printed: a TXT record in the install's own zone, which Let's Encrypt follows the CNAME to. This works before the domain points here, so a domain moved from another host has its certificate before DNS moves, with no moment without HTTPS; it's also the only way a wildcard gets one;
3. the domain already points here: an HTTP challenge, answered on port 80 by both regions' routers.

Before asking Let's Encrypt anything, the router asks the domain's own nameservers whether one of those is set up, so a domain whose DNS isn't ready yet costs no failed validation (Let's Encrypt allows 5 an hour). Until it is, `domains ls` shows `invalid` configuration and the router looks again every 30 seconds (every 5 minutes once a domain has waited a day); with the dashboard's Domains page open, the page checks every few seconds and tells the router the moment the records are right, and `forkrail domains check` (or Check again) has it look within seconds. A domain that works is looked at every 5 minutes, so one moved away shows `invalid` too. On Cloudflare the record must be DNS only (proxy off): a proxied record shows `invalid (proxied)`. The router writes how each certificate stands to `state/certs/<domain>/status.json` whenever that changes, when a check is asked for, and otherwise every 10 minutes (hourly once issued): `issued`, `pending`, `invalid configuration` or `renewal failed`, the way it proved the domain, the last error and when it looks next. Certificates renew with a third of their 90 days left; alarm `forkrail-cert-expiry` mails when one has under 14 days left.

**Redirects.** A domain that redirects (301, 302, 307 or 308) to another of the project's domains, or to `<project>.forkrail.com`, keeps the path and query: `https://www.example.com/a?b=1` → `https://example.com/a?b=1`. It needs its own DNS record and certificate like any other. The dashboard offers the `www` pair whenever you add a registered domain.

**Branches.** A domain given `--branch staging` serves what `shop-git-staging.forkrail.com` serves: the newest ready preview of that branch, moving with each push. The project must be connected to a repository. Until the branch has a ready preview, it answers 404; when the branch is deleted, it serves nothing until a branch of that name is pushed again.

**Moving a domain from Vercel without a gap** (`api.example.com`, which Vercel serves):

1. Add it: `forkrail domains add api.example.com my-api`. In a Cloudflare zone the token edits, the certificate comes within a minute; elsewhere, add the `_acme-challenge` CNAME it printed first.
2. Wait for `domains ls` to show `issued`.
3. Test it through the router while DNS still points at Vercel:

   ```bash
   curl -sS --resolve api.example.com:443:$(deploy/aws/test-env.sh ip) https://api.example.com/openapi.json -o /dev/null -w '%{http_code}\n'
   ```

4. Lower the record's TTL first (same target, TTL 60), wait out the old TTL (5 minutes for 300), then point it at the router. Both print what the record was:

   ```bash
   deploy/aws/test-env.sh point api.example.com cname.vercel-dns.com
   ```

   ```bash
   deploy/aws/test-env.sh point api.example.com my-api.forkrail.com
   ```

5. To go back, point it at Vercel again; clients follow within a minute or two. Keep the Vercel deployment and its domain setting while testing: Vercel keeps serving anyone who still has its address.

`forkrail domains rm` stops serving a domain within seconds; move its DNS away first. A domain other domains redirect to can't be removed until they change.

Owners and members change domains; a developer may ask for a check; a viewer looks.

## 6a. Spend budgets

forkrail counts what every project costs while it's spent, not hours later from AWS's bill, and pauses a project the moment its spend reaches a budget, so a bug, a loop or a flood can't run up a bill.

**What's counted**, at AWS's list price (free allowances aren't taken off: they're the whole account's), in the units AWS bills:
- function time: every invocation from the invoke until it returns, start-up included, counted every second while it's open, at the function's memory; and each invocation;
- bytes to and from visitors, as the router's kernel counted them on each connection (TLS handshakes, headers and retransmits included), at $0.09/GB plus Global Accelerator's premium for where the visitor is ($0.015/GB in North America and Europe, up to $0.105/GB for Australia);
- bytes between the router and the functions ($0.01/GB each way; $0.02/GB between regions);
- what a function sends to the internet itself (its own calls and uploads, $0.09/GB): its runtime reads the environment's network counters every 2 seconds and takes off what went to the router;
- builds: the builder's memory for as long as each build ran;
- the image optimizer's invocations, warm-ups, and the router's S3 requests.

Fixed costs (the routers, the accelerator, the analytics box) aren't in a budget: they're known in advance and the same every month.

**Budgets** are monthly, in dollars, for the calendar month in UTC:
- the team budget, which every install has (a new install starts at $20; while nobody sets it, a month that ends at 90% or more of it raises the next month's a step);
- a project's own, if you want one; its spend counts toward both.

```bash
forkrail spend show                          # the budgets, and what's paused
forkrail spend budget 200                    # the team budget: $200 a month
forkrail spend budget 50 --project my-api    # my-api's own
forkrail spend no-budget my-api              # only the team budget applies to it again
forkrail spend exempt forkrail-web           # the dashboard keeps running when the team budget is reached
```

**At a budget**, within about a second (another region's spend within ten):
- every host of the project, or of every project but those kept running for the team budget, answers `503 DEPLOYMENT_PAUSED`: no function runs and no file is served;
- its functions' work in flight is stopped, and Lambda is told to refuse any invocation of the project's function (its reserved concurrency is set to 0; one it had before, the dashboard's say, is put back on resume);
- no build of it starts;
- the team hears at 50, 75 and 100% of each budget, then at each further 100%, and when a project spends more than 10% of its monthly budget in an hour (a warning only: nothing is paused for spending fast). A pause and a fast-spending warning name what spent most of the last hour: the kind of cost, the path, and the client's network (its /24, AS and country).

The team's owners hear in the dashboard's bell and by email (if the team has email set up), and webhooks can ask for `budget.reached` (each threshold: `budgetAmount`, `currentSpend`, `thresholdPercent`, `scope` `team` or `project`, `scopeName`), `spend.paused`, `spend.fast` and `budget.raised` (the default budget grew for next month). Each is said once, although every router sees it.

**The guard** (`deploy/aws/guard.sh release`, stack `forkrail-guard`) is a second line that shares nothing with the routers: every 5 minutes it reads AWS's own numbers (Lambda's Duration for every forkrail function in every region, and the egress Global Accelerator counted), prices them as a floor (never more than was spent), and pauses a project whose budget that floor reaches: concurrency 0 in every region, the pause recorded by control, the team told. It acts only if the routers didn't. It also says when AWS shows more function time than the routers counted (by over 10% and $1): something isn't metered. Once a day it also holds AWS's own bill for the day before (Cost Explorer, $0.01 a look) against what the routers counted, and says when the bill shows more. That needs the install's cost allocation tag active in Billing: `deploy/aws/stack.sh apply cost-tags` does it (free; it applies within a day). It also counts forkrail's own costs that no router sees (storage, the state table and the dashboard's, control and its other functions) into `state/spend/own.json`, which the routers and the dashboard add to the team's spend. It costs about $0.35 a month, nearly all of it the daily bill look: its CloudWatch reads (GetMetricStatistics) fit in CloudWatch's free million requests a month. `deploy/aws/guard.sh check` runs it once without acting.

**Resuming** is by hand only, never by itself, not even when a new month starts:

```bash
forkrail spend resume                        # every paused project
forkrail spend resume --keep shop            # all but shop (say, the one being attacked)
forkrail spend resume my-api                 # just my-api
```

Raise the budget first: a project still at or over a budget is paused again within a second. Owners change budgets and pause or resume; everyone in the team sees them.

**What it can't see:** what's billed outside forkrail's own resources (other services an app calls with its own role: its database, queues, other AWS services). Calls to AWS services in the same region count as the function's own traffic, though AWS doesn't charge for them, so the figure errs high. Everything a budget doesn't count is still in the usage page, from AWS's bill.

## 7. Watching production

What the router keeps:
- **Every request**, in its log on the EC2 instance (`/var/log/forkrail`, hourly files): host, deployment, method, path, query (parameters named like `key`, `token` or `secret` masked), referer (masked the same way), client IP, HTTP version, user agent, status, outcome, retries, time queued, time to first byte, total time, bytes, the worker, and its memory and CPU when it ran alone. Every response carries `x-forkrail-request-id`, the line's `request` id, so one request can be found from a client's report.
- **What Vercel shows about the function**, on each request too: the environment (production or preview), runtime, entrypoint, memory used against the memory size, duration against `maxDuration`, the route template (`/items/{item_id}`, for FastAPI and Starlette apps), and the start: `warm` (an open worker took it), `thaw` (it waited for a frozen environment, usually 15–25 ms) or `cold` (it waited for a new one).
- **Where the client is**: country, region, city, coordinates and network (autonomous system and its owner), looked up on the router from the connecting address in DB-IP's free City and ASN databases ("IP Geolocation by DB-IP", https://db-ip.com, CC BY 4.0), which a daily timer on each router refreshes when a new month's files come out. The router also passes it to the app as Vercel does: `x-vercel-ip-country`, `x-vercel-ip-continent`, `x-vercel-ip-city`, `x-vercel-ip-latitude` and `x-vercel-ip-longitude` (any the client sent are removed first), plus `x-real-ip` and `x-forwarded-for`. The free files have no region codes, time zones or postal codes, so `x-vercel-ip-country-region`, `-timezone` and `-postal-code` aren't sent. A request that came through a proxy (an API marketplace, a caller's own server) shows where that proxy is.
- **The routers themselves**, every 15 seconds: CPU, memory, load, disk and network I/O, the root filesystem, TCP connections, and the CPU and memory of each service (the router, Vector, the SSM agent...), in `forkrail.host_metrics`.
- **Outgoing requests**: each HTTP call the app makes while serving a request, with its method, host, path, query (masked), status or error, duration and size. Python's curl_cffi, requests, httpx, aiohttp and urllib are timed, and Node's fetch and `node:http`; your code isn't changed.
- **Problems**: retries, app crashes, timeouts, cut-off responses, upstream failures, spawn failures, and once a minute a count of failed TLS handshakes.
- **The app's output**, line by line, with Vercel's limits: each request keeps up to 256 lines and 1 MB of output, and a line can be 256 KB. There is no limit per second: a busy deployment keeps every request's output. What a request prints past those limits, and lines printed outside any request (start-up, background threads), share a budget per deployment of 128 KB a second, with 2 MB at once. What isn't kept is counted on the request (`lines_dropped`), and the dashboard's request panel says how much is missing.
- **Each request's outgoing requests are always kept**, with no limit per second (a single request that reports more than 10,000 is taken for a loop).
- **A copy in S3** every 10 minutes, as JSON lines: `s3://forkrail-artifacts-<account>/logs/router/YYYY/MM/DD/`. The instance keeps two days; S3 keeps everything.
- **The analytics store**: ClickHouse on its own instance (`deploy/aws/analytics.sh`), which each router's Vector fills from its log every 2 seconds. Requests, invocations (cold starts, billed time), the app's output and the router's warnings, kept for good, plus per-minute and per-hour rollups for charts over long ranges.

From your machine, the CLI reads the analytics store (it needs your IP allowed once: `deploy/aws/analytics.sh allow-me`, and again whenever your IP changes):

```bash
forkrail logs my-api                       # the last hour's requests, newest last
forkrail logs my-api --status 5xx --since 1d
forkrail logs my-api --path /search -f     # follow: new requests every 2 seconds
forkrail logs my-api --request 1f3a9c0d2e4b5a6c   # one request: its function, timeline, outgoing requests and output
forkrail logs my-api --app --level error --since 6h   # the app's output; --search TEXT too
forkrail stats my-api --since 7d           # status, latency, workers, cold starts, regions, by hour or day, busiest paths, countries, networks, external APIs
forkrail usage --since 30d                 # per project: requests, GB-seconds, transfer, cost, and Vercel's estimate
forkrail usage my-api --daily
```

Each request in `logs` shows how many lines the app printed for it. Since 2026-11-09 the runtime tags each line with the request the app was serving, so `--request ID` shows exactly that request's output; deployments made before then show the worker's lines while it ran, until they're deployed again. The same goes for outgoing requests and route templates, which only deployments made since then report.

Lines an app prints after its response (a traceback logged once the 500 has gone out, say) can reach the router only when Lambda next runs that environment, since it freezes the environment once the worker has nothing to do. `--request` still finds them by the request's id.

Alarms (set up once with `test-env.sh monitor HOST EMAIL`, then confirm the mail from AWS):
- Route 53 checks `https://HOST/_forkrail/health` every 30 s from several regions, and records connect and first-byte times (Route 53 console → Health checks → the check → Latency). Two failed minutes mail you: the router, its certificate or DNS is broken.
- 20 or more 5xx in a 10-minute window mail you.

The daily look:

```bash
deploy/aws/test-env.sh report "1 day ago"
```

Per project: requests by status, p50/p95/p99, retries, workers, cold starts and estimated cost; then for public traffic, by hour (requests, 4xx, 5xx, p50/p95/p99, peak requests in flight, peak workers, cold starts), the busiest paths, the latest failures and the slowest requests, each with its request id, query, client and user agent.

### Notifications, webhooks and email

What happens is sent to the install's events queue, `forkrail-events`: a deployment created, ready (`deployment.succeeded`), failed (`deployment.error`), canceled, promoted (production moved to it, by a deploy or a promote) or rolled back to (`deployment.rollback`); an attack the system mitigations are refusing (`attack.detected`, from the router that sees it, at most every 10 minutes); a custom domain's certificate that failed to renew (`domain.certificate.renew.failed`, from the issuing router, once as it starts failing). The control function, the routers and the CLI with AWS credentials all send them; sending never holds up or fails a deploy. The notifier, `forkrail-notify` (`deploy/notify`, stack `forkrail-notify`), takes them off the queue:

- **Webhooks** (dashboard: team Settings → Webhooks, owners; up to 20): each is an HTTPS URL, the events it wants, and all projects or chosen ones (attack and certificate events are the install's and go to every webhook that wants them). Each event is one POST with Vercel's body, `{"id", "type", "createdAt", "payload", "region"}`: `payload.deployment` (`id`, `url`, `name`, `meta` with the commit, plus forkrail's `via`, `deployer`, `hook` and `error`), `payload.links`, `payload.target` (`production` or null), `payload.project`. The header `x-forkrail-signature` is the hex HMAC-SHA1 of the raw body with the webhook's secret, shown once when it's made; `x-forkrail-event` and `x-forkrail-delivery` say what and which try. An answer other than 2xx within 30 s is tried again after 1, 5 and 15 minutes, then 1, 2, 4, 8 and 12 hours, for up to a day after the event; each try is listed with the webhook for 30 days, and **Send test event** posts a `webhook.test`. Webhooks reach public addresses only: the host is resolved, every address must be public (no private, loopback, link-local or metadata addresses), and the connection goes to the address checked; redirects aren't followed.
- **Notifications** (the bell in the dashboard, kept a year): a **failed deployment** goes to whoever made it if they're a member (the GitHub user who pushed, or who pressed the button), else to the owners; a **certificate renewal failure** and an **attack** go to the owners. Each person turns each kind on or off for the bell and for email under **My notifications**, where they also give and confirm their email address (a link, good for 24 hours).
- **Email** (team Settings → Email, owners): off until an owner sets it up, and then through the install's own provider; forkrail sends nothing itself. **Amazon SES** in the install's account: the dashboard shows the From domain's state in SES, can add it, and lists its three DKIM records to put in DNS; a new account is in SES's sandbox, where mail reaches only addresses verified in SES, until AWS grants production access (SES console → Account dashboard → Request production access). **SMTP** for any other provider (Resend, Postmark, SendGrid, Mailgun, Google Workspace, your own server): host, port (587 with STARTTLS, or 465 with TLS; Lambda blocks 25), user name and password. **Send test email** says what the provider answered.

The SMTP password and webhook secrets are encrypted by the dashboard with `alias/forkrail-notify`, a key the dashboard may encrypt with and only the notifier's role may decrypt, and stored in the dashboard's table. The notifier's own role reaches only the members, preferences, notifications and webhooks there, never sessions, tokens or the audit trail.

```bash
deploy/aws/notify.sh test                                        # its tests, here
deploy/aws/notify.sh release                                     # zip, upload, apply the stack (asks first)
deploy/aws/notify.sh ask '{"kind":"ses_status","domain":"example.com"}'
deploy/aws/notify.sh dead                                        # events that failed five times (kept 14 days)
```

Setting it up the first time: `notify.sh release`, then `stack.sh apply web` (the table lets the notifier in), `stack.sh apply control` (control may send to the queue), `dashboard.sh role` (the dashboard may ask the notifier and encrypt with its key), `test-env.sh roles` and `test-env.sh router-env FORKRAIL_EVENTS_QUEUE=<the stack's QueueUrl>` in each region (the routers send attacks and certificates).

## 8. Day to day

| Task | Command |
|---|---|
| Deployments, newest first, with their status and URLs | `forkrail ls my-api` |
| One deployment's record, or its build log | `forkrail inspect <id> [--logs]` |
| Back to the previous production deployment, live within 5 s | `forkrail rollback my-api` |
| Back to a given production deployment | `forkrail rollback my-api --to <id>` |
| Make an older production deployment live again | `forkrail promote <id>` |
| Make a deployment again with today's env vars and settings (Vercel's Redeploy): built again from its source if it was built in the builder, else its build published again; as production with `--prod` | `forkrail redeploy <id> [--prod]` |
| Build on this machine instead of the install's builder | `forkrail deploy DIR --local` |
| Stop a deployment building in the builder | `forkrail cancel <id>` |
| Delete a deployment that isn't production (restorable for up to 30 days) | `forkrail rm <id>` |
| Bring a deleted deployment back, exactly as it was | `forkrail restore <id>` |
| How long deployments are kept, and what the policy does with each | `forkrail retention [--project my-api]` |
| Change the periods (a project's, or the install's default without `--project`); `--dry-run` lists what it would delete | `forkrail retention set [--project my-api] [--preview 7d] [--production 1y] [--errored 2w] [--canceled default]` |
| Run the policy now; without `--yes` it only lists | `forkrail retention sweep [--project my-api] [--yes]` |
| Production deployments wait for `promote` | `forkrail project my-api --auto-assign-domains false` |
| Deploy production again unchanged, on a new runtime (refused if the files or settings differ) | `forkrail deploy DIR --prod --same-source` |
| Publish a production deployment without putting it live: it answers on its own `<project>-<id>` host until `forkrail promote <id>` | `forkrail deploy DIR --prod --skip-domain` |
| Env vars (then redeploy) | `forkrail env add\|ls\|rm my-api KEY [--target production\|preview\|development] [--git-branch BRANCH]` |
| Development's into `.env.local`, or around a command | `forkrail env pull [FILE]`, `forkrail env run -- CMD` |
| Workers at once per deployment (no cap by default) | `forkrail limits my-api [--max-workers N\|default]` |
| Function settings for the next deploys, the IAM role its functions run as, private targets it reaches through the router | `forkrail project my-api [--memory MB] [--max-duration S] [--concurrent BOOL] [--role NAME\|default] [--egress NAME,...\|none]` |
| Where its functions run from the next deploy (Vercel's or AWS's names) | `forkrail project my-api --regions iad1[,fra1...]\|default` |
| Skew protection: open pages keep their own deployment after a deploy (at once) | `forkrail project my-site --skew-protection on\|off [--skew-max-age 12h] [--skew-threshold ID\|none] [--skew-allowed-domains D,...\|none]` |
| Web Analytics, off until switched on (at once, no redeploy); pages count once they load `@forkrail/analytics`, `@vercel/analytics` or the script | `forkrail project my-site --analytics on\|off` |
| Rename a project (also Settings → General): the dashboard and terminal show the new name and take either; its addresses, functions, env values and logs keep the first name, and no other project can take either name. Its apps see the new one in `FORKRAIL_PROJECT_NAME` from the next deploy. The first name again undoes it | `forkrail project my-api --rename homes` |
| Requests, one request in full, the app's output | `forkrail logs my-api [--status 5xx] [--request ID] [--app] [-f]` |
| Traffic, latency, workers, busiest paths | `forkrail stats my-api [--since 7d]` |
| Usage and cost per project or day, against Vercel | `forkrail usage [my-api] [--since 30d] [--daily]` |
| Each router: CPU, memory, disk, network, connections, requests, and the router's and Vector's own use | `forkrail routers [--since 1h]` |
| The same from the router's own log, on the instance | `test-env.sh logs my-api ["SINCE"]`, `test-env.sh report "SINCE" ["UNTIL"]` |
| Custom domains, their redirects and branches, and their certificates | `forkrail domains add\|update\|ls\|inspect\|check\|rm` |
| Move a domain's DNS (and back) | `test-env.sh point HOST TARGET` |
| Regions with a router or functions, how many deployments run in each and how many projects pick each | `forkrail regions ls` |
| Make a region ready for functions (picking it for a project does it too) | `forkrail regions setup eu-central-1` |
| A router runs in another region: send it production's and its rollback targets' files, then every deploy's | `forkrail regions add us-west-1` |
| Try one deployment's function in another region | `forkrail regions copy us-west-1 <id>...` |
| Work on another region's router | `AWS_REGION=us-west-1 test-env.sh logs\|report\|run ...` |

### How long deployments are kept

Vercel's Deployment Retention. Each state has a period: **canceled**, **errored**, **preview** and **production** deployments, 30 days each unless set (`1d` up to ten years, or `unlimited`). The install has a default (`forkrail retention set --production 1y`, Team settings → Deployments; an owner's), and a project may set its own periods over it, one by one (`--project`, Settings → Deployments; a member's). A deployment older than its period is deleted, unless something keeps it whatever its age:

- Vercel's: among the project's 10 newest deployments; among its 20 newest ready production deployments, or its 20 newest ready previews; production now; the latest ready deployment of a branch with a domain; a preview a custom domain points at; the latest preview of a branch that still exists (Vercel asks for an open pull request; here its branch's URL is enough, so a little more is kept).
- forkrail's own, so nothing the fixed counts before kept goes: one of the last 5 deployments that served production (rollback targets: a preview promoted here stays a preview), anything still building, and the 10 newest that failed or were canceled, so their logs can be read.

The policy runs after every publish, for that project, and once a day for every project (`deploy/install/retention.yaml`: EventBridge Scheduler invokes the control function at 04:17 UTC; applying it is the install owner's step). `forkrail retention --project NAME` shows what it does with each deployment and why (`expires`, `kept: …`, `DELETED by the next sweep`), and `retention set … --dry-run` or `retention sweep` without `--yes` list what a change or a run would delete without doing it; the dashboard shows the same before a change is saved.

**Deleting is in two steps**, as on Vercel. A deployment deleted (by the policy, by `forkrail rm`, its ⋯ menu → Delete, or with its branch) stops answering at once and moves to the project's Recently deleted list, with everything of it still stored: its Lambda versions in every region, its record, package, source, static files and build log. `forkrail restore <id>` (or Restore in Settings → Deployments) puts it back exactly as it was, at its own address, and the policy then counts its age from the restore. It stays restorable for 30 days; less when it's younger: by the policy, the policy's period; by hand, its age, at least a day. After that the next run deletes it for good (its versions, record, log, and the files no other deployment uses): a deleted Lambda version can't come back. Deleting a project takes its deleted deployments with it.

Builds send their log lines to the dashboard (`forkrail dashboard https://<dashboard>` sets where; `FORKRAIL_DASHBOARD_URL` overrides it for one command), which writes them into ClickHouse as an insert-only user, so the deployment page shows them live. Each deployment has its own key for that, which works only while it builds; its record keeps the key's hash. When the deployment ends, the whole log is kept in S3 too (`logs/builds/<project>/<id>.ndjson.gz`), which is what the dashboard reads from then on, so lines that didn't arrive live (the dashboard was down) aren't lost.

The dashboard does the same things through the install's control function (`forkrail-control`, from `deploy/install/control.yaml`): promote, instant rollback, redeploy, a project's function settings, its env vars (set, replace, remove, import a `.env`; values are never shown back), and each deployment's source.

Which to use on an earlier deployment, as on Vercel: **Instant rollback** puts an earlier production deployment back exactly as it was (its code, env vars and settings), instantly. **Promote** makes a deployment production: instantly for a production deployment, and for a preview whose env vars differ from production's, by publishing its build again with production's. **Redeploy** makes a new deployment with the project's env vars and settings as they are now (built again from its source when it was built in the builder, else from the same build): it's how a changed variable or setting reaches a deployment.

Each deploy keeps the project's own files for the dashboard's Source view: what `.gitignore` and `.vercelignore` leave, without dependencies (`node_modules`, virtualenvs), build output, caches, `.env*`, or files whose names say they hold secrets (`*.pem`, `*.key`, `.npmrc` and the like). They're stored once per distinct set of files, at `sources/<project>/<hash>.zip` in the install's bucket. A folder of more than 20,000 files or 100 MB isn't kept; the deploy goes ahead and says so. The dashboard's own role can only read and invoke that function; the function holds the rights to publish and to decrypt env values, and nothing else can call it. Its audit trail, one line per action with who asked, is `deploy/aws/control.sh logs`.

**Build and publish apart.** `forkrail deploy` builds, then publishes. The two halves also run on their own, like `vercel build` and `vercel deploy --prebuilt`:

```bash
forkrail build DIR --out OUT [--prod] [--project NAME]
```

```bash
forkrail publish OUT [--memory MB] [--skip-domain]
```

The build writes `build.json`, `package.zip` and the site's static files into `OUT`, and touches nothing in AWS: with `--request FILE` (the deployment's id, project, target, base domain and env vars, as JSON) it doesn't even need credentials. Builds get the project's env vars for their target (its own `AWS_*` included) and the system variables (section 2: `VERCEL_ENV`, `VERCEL_URL`, `VERCEL_GIT_*`, forkrail's names, `CI=1`), never the machine's `AWS_*` variables, and every line they print goes through forkrail's build log. Publishes of one project take turns: a second waits for the first to finish.

### Where the install's state is kept

Project files and deployment records move from the bucket (`state/projects/`, `state/deployments/`) to a DynamoDB table, `forkrail-state` (`deploy/install/state.yaml`, applied with `deploy/aws/stack.sh apply state AdminPrincipals=<your admin ARNs>`). The routing table, settings and certificates stay in the bucket, so the routers never depend on the table: if it were unreachable, sites keep serving and only deploys and the dashboard stop. `state/store.json` says which one is read, and every running control function follows it within 30 s:

| Step | Command |
|---|---|
| Where they're kept, and how many there are on each side | `forkrail state status` |
| The bucket stays the one read; every write is also copied to the table | `forkrail state mode mirror` |
| Copy everything written before that | `forkrail state migrate` |
| Compare the two (and make the one not read like the one read) | `forkrail state check [--repair]` |
| Read the table; the bucket keeps a copy of every write | `forkrail state mode dynamodb` |
| Back to the bucket, losing nothing (refused unless the bucket was kept level) | `forkrail state mode s3` |
| Later, stop copying to the bucket | `forkrail state mode dynamodb --no-s3-copy` |

Who can reach the items: the control function (read and write), the dashboard (read only) and the administrators named in `AdminPrincipals`. The table's resource policy refuses every data action to anyone else, whatever their own IAM policies say (the worker role apps run as, a project's own role, the routers, the builder, the analytics box, other workloads in the account), and it's encrypted with its own key, `alias/forkrail-state`, which the routers', the worker's, the builder's and the analytics box's roles can never use. Every write is recorded with who made it (CloudTrail trail `forkrail-audit`, kept a year in `forkrail-audit-<account>`). It can't be deleted while deletion protection is on, it's kept if the stack goes, and it can be restored to any second of the last 35 days (`aws dynamodb restore-table-to-point-in-time`, into a new table; then `forkrail state mode` with that table). It's billed per request: cents a month for one team.

### Releasing forkrail itself

`deploy/aws/release.sh` releases the install's own parts from a pushed commit, in the order that keeps old and new working together: the routers (each extra region, then the home region, one at a time, with no gap), then control and the builder together, then the dashboard.

| Step | Command |
|---|---|
| What changed against what runs, and what a release would do (changes nothing) | `deploy/aws/release.sh plan` |
| Release every part that changed (asks once) | `deploy/aws/release.sh run` |
| Only some parts; the dashboard up to its preview | `deploy/aws/release.sh run routers backend web [--web-preview]` |
| Each host's status through each router and accelerator address, against the last saved | `deploy/aws/release.sh check [--save]` |
| A router binary a release replaced, back on every router | `deploy/aws/release.sh routers-back <sha16>` |

Each step is checked before the next one starts:
- every custom domain and production host answers through the router just restarted as it did before the release;
- the router runs the binary just built;
- control answers;
- test builds of two projects go through the new builder.

A difference stops the release, and the summary prints the way back for each part already released: router binary, control's zip, builder image, dashboard deployment. Logs, timings and the summary are kept in `target/releases/`, and a record of each release in the bucket under `releases/`.

Work that doesn't depend on other steps runs at the same time:
- one compile for the router, control and the builder;
- this machine's CLI, the builder image and control's zip while the routers restart;
- both stacks applied at once;
- test builds and the dashboard's preview together.

The builder image keeps the layers it takes from AWS's images and the toolchains (in `target/builder-layers/`), so only forkrail's own layer is rebuilt while the bases are unchanged. Before this script the same steps took 20 to 30 minutes by hand.

`forkrail`'s `--since` takes an age (`30s`, `15m`, `2h`, `7d`) or a UTC time (`2026-11-08 14:00`); `test-env.sh`'s `SINCE` takes journalctl's syntax: `"1 hour ago"`, `"2026-11-04 10:00:00"` (UTC).

**Compression is up to the app.** Vercel's CDN gzips or brotli-compresses JSON and text for clients that send `Accept-Encoding`; forkrail's router passes responses on as the app sends them. With FastAPI, add `GZipMiddleware` (Express: `compression`). It compresses only for clients that ask for it, so others still get plain JSON; Python `requests`, httpx, Node's `fetch`, axios and `curl --compressed` all unpack it themselves. A route that sets `content-encoding: identity` turns compression off for that response, on Vercel too. Large JSON shrinks to about a tenth, and you pay for transfer by the byte.

<a id="9-websites-tanstack-start-nuxt-solidstart-and-static-sites"></a>

## 9. Websites: SvelteKit, Astro, Nuxt, TanStack Start, React Router, Remix, Next.js, and static sites

A website deploys the way Vercel deploys it: run `forkrail deploy` in the project. The CLI finds the framework in `package.json` (or `vercel.json`'s `framework`, or the project's setting, section 9a), installs the dependencies if `node_modules` is missing (Vercel's install command for the lockfile: `npm install`, or pnpm's, yarn's or bun's; in the builder it runs every time) and runs the `build` script with Vercel's build variables (`VERCEL=1`, `NOW_BUILDER=1`, `CI=1` and section 2's system variables). A package of a monorepo is installed with its workspace's package manager: the lockfile is looked for in the folders above it too, up to the repository's top. A framework with a server builds through its own Vercel adapter, which writes Vercel's Build Output (`.vercel/output/`), and forkrail deploys that ("Frameworks", next). A site with no server is its build's files ("Static sites", below).

```bash
cd ~/code/my-site
forkrail deploy --memory 1024              # a preview
forkrail deploy --memory 1024 --prod       # production
forkrail deploy --prebuilt                 # the .vercel/output/ (or Nitro's node-server .output/) you already built, as it is
```

SSR is CPU work: 1,024 MB buys 0.58 of a core, where the default 640 MB buys 0.36. `vercel.json` may set `buildCommand`, `installCommand`, `fluid` (concurrent compute) and the routing keys (below). Next.js builds another way, as its own server ("Next.js", below), and isn't finished: read what's still missing there before deploying one.

### Frameworks

The build runs the framework's own Vercel adapter, the one the project already deploys to Vercel with, and forkrail imports the Build Output it writes: its routes go to the router, its functions into the deployment's Lambda function (Node 24 on the forkrail runtime, many requests at a time), its static files to S3 in every region the deployment runs in, its prerendered pages to the router's cache, its `images` to Image Optimization and its crons to the project's schedules. The project's code and config aren't changed. React Router and Remix have no adapter of their own on Vercel: Vercel's builder packages their Vite build, and forkrail does that work itself (ported from `@vercel/remix-builder` 31.0.0, without running Vercel's code), writing what `vercel build` writes. Each framework with a server was compared with the same app deployed on Vercel, request by request; what still differs is under "Not supported yet, and where forkrail differs".

| Framework | What the project needs |
|---|---|
| SvelteKit | `@sveltejs/adapter-vercel`, or `adapter-auto` (which picks it: the build has `VERCEL=1`); `split` and each route's `config` (`memory`, `maxDuration`) work. With `adapter-static` it's a static site |
| Astro | `@astrojs/vercel` for a server (`output: 'server'`, or static pages with on-demand routes); none for a static site |
| Nuxt, Nitro, Vite + Nitro | nothing: Nitro picks its own `vercel` preset |
| TanStack Start | Nitro's Vite plugin: the `nitro` package and `nitro()` in `vite.config` |
| React Router 7 and later | Vite (`reactRouter()` in `vite.config`); `vercelPreset()` from `@vercel/react-router` in `react-router.config` for a function per route `config` |
| Remix 2 | Vite (`vitePlugin` from `@remix-run/dev`); `vercelPreset()` from `@vercel/remix` in `vite.config` |
| Vite and the other static presets | nothing ("Static sites") |
| Hugo | a `config.toml`, `config.yaml` or `config.json` with `baseURL` (Vercel doesn't look at `hugo.toml` either; with only that, name the framework). `HUGO_VERSION` picks the release, else Vercel's default, 0.58.2, downloaded at build |

What each does on forkrail, against Vercel's own feature list:

| | SSR | Streaming | ISR | Images | Middleware | Skew protection | Cron jobs | Several functions |
|---|---|---|---|---|---|---|---|---|
| SvelteKit | yes | yes (1) | `config.isr` | the adapter's `images` | yes | yes | `vercel.json` | yes (2) |
| Astro | yes | yes | the adapter's `isr` | `imageService` | `edgeMiddleware` (3), or the project's | yes | `vercel.json` | yes (2) |
| Nuxt | yes | pages render whole, as on Vercel | `isr` route rules | Nuxt Image | yes | yes | `vercel.json` (4) | `functionRules` (2) |
| TanStack Start, Nitro, Vite + Nitro | yes | yes | `isr` route rules | `vercel.json` | yes | yes | `scheduledTasks` (4), `vercel.json` | `functionRules` (2) |
| React Router | yes | yes | no, as on Vercel (5) | `vercel.json` | yes | yes | `vercel.json` | `vercelPreset()` (2) |
| Remix 2 | yes | yes | no, as on Vercel (5) | `vercel.json` | yes | no, as on Vercel | `vercel.json` | `vercelPreset()` (2) |
| Vite, the static presets, Hugo | – | – | – | `vercel.json` | yes | – | – | – |
| Next.js | yes | yes | its own, in the shared cache | its own optimizer | yes | yes (6) | `vercel.json` | one function |

"Images" is Image Optimization (`/_vercel/image`): the framework's own settings, or `images` in `vercel.json` for any project. "Middleware" is Routing Middleware: "yes" means the project's own root `middleware.ts` or `vercel.json` `proxy` runs. Skew protection does something where the framework sends the deployment's id; it's on by default for every new project ("Skew protection", below).

1. SvelteKit's ISR function says in its `.vc-config.json` that it doesn't stream. Vercel has streamed every Node.js function's answer by default since 2024, and so does forkrail (the page kept is the same either way).
2. A deployment's functions run in one Lambda function, in one Node process, at one memory size: the largest any function asks for, and at least the project's setting. Each keeps its own `maxDuration`, which the router holds it to. Vercel runs each apart. Their own `environment` variables must agree, and a file one function reads from its own folder (SvelteKit's `read()`, Astro's `includeFiles`) is copied into the shared one. One route that asks for 1,024 MB makes every route 1,024 MB, and Lambda bills by the size.
3. Astro's `edgeMiddleware` runs on Node, as every Edge function does here, and fetches each page from the deployment's own address (`/_render`, `/_isr`), as on Vercel: a page is two requests (two invocations on Vercel), and on forkrail its bytes leave Lambda and come back through the router. The page still sees the visitor's address and location: the router sends every app `x-forkrail-visitor`, its signed word for the visitor (good for 5 minutes from the visitor's own request, on that project and environment only: a preview's can't pass on production, and a call that came with one gets one that expires no later), and a request carrying a good one, as such a call does with the visitor's headers copied, is taken as the visitor's own (its forwarded and `x-vercel-ip-*` headers, the log, the firewall and deployment protection).
4. Nitro 3's `scheduledTasks` (TanStack Start, Nitro 3, Vite + Nitro) become cron jobs; Nitro 2's (Nuxt 4) make none, as on Vercel, and the build says so.
5. Their prerendered pages are static files. Any function's answer can still be kept with `s-maxage` ("The response cache", below).
6. Next.js 14.1.4 and later by themselves, 13.5 to 14.1.3 with `experimental.useDeploymentId` ("Skew protection", below). Next.js has its own section ("Next.js", below): it's not finished.

**Nitro** (Nuxt, TanStack Start, Nitro, Vite + Nitro). Nitro sees `VERCEL=1` and picks its `vercel` preset, as on Vercel:
- Nitro 2's function (Nuxt 4) is a Node `(req, res)` handler, Nitro 3's a Web `fetch` handler with `context.waitUntil`; a `vercel.functionRules` entry is a function of its own, with its own `maxDuration`. Nitro 2's `vercel-edge` preset runs as an Edge function, on Node.
- Route rules are answered by the router before the function wakes: `redirect`s, `headers`, a `proxy` to another site (an external rewrite), `isr` (`allowQuery`, `passQuery`; Nuxt's `swr` is kept until the next deploy, as Nitro 2 tells Vercel), and Nuxt Image's `/_vercel/image` URLs.
- Static files are served before the function with the cache headers Nitro gives them (a year, `immutable`, for Vite's `/assets/` and Nuxt's `/_nuxt/`).
- Nitro 3's `scheduledTasks` become cron jobs calling `/_vercel/cron` with the expression in `x-vercel-cron-schedule` (and `Authorization: Bearer $CRON_SECRET` when the project sets `CRON_SECRET`), as on Vercel, beside `vercel.json`'s `crons` (the same path and expression in both runs once). Nitro 2's make none, on Vercel either: call a route from `vercel.json`'s `crons` instead.

A project that pins another preset keeps it, as Nitro decides (`NITRO_PRESET`, `nitro.preset`, Nitro 3's `nitro({ preset })`; a build where std-env finds a CI service ahead of Vercel, such as `GITHUB_ACTIONS`, gets `node-server` too). With `node-server` forkrail runs `.output/server/index.mjs` as one server, as it ran every Nitro site before, and the build warns that route rules, `isr` and scheduled tasks then run inside it, not as on Vercel (and that Nuxt Image's `/_vercel/image` URLs won't load). Another preset (`bun`, `netlify`) is refused. SolidStart builds the same way, and Analog may, but neither has been compared with Vercel yet.

**SvelteKit and Astro.** Form actions, hooks, `$env`, Astro's actions, server islands and `redirects` work as on Vercel. `getClientAddress()` and `Astro.clientAddress` are the visitor's real address: the router replaces any `x-forwarded-for` a client sends. Astro's sessions need a driver the app names, on Vercel too (its adapter sets none).

**React Router and Remix.** One function per server bundle: with `vercelPreset()`, routes whose `config` (`memory`, `maxDuration`) differ get their own; without it one function serves every route and the build warns, as Vercel's does. SPA mode (`ssr: false`) is a static site. A function takes each npm package it imports whole, with its dependencies, where Vercel's tracing keeps single files: packages are bigger (the Remix test app's is 36 MB unzipped). pnpm and Yarn projects install flat (`node-linker=hoisted`, `node-modules`), so the files are where Node looks for them.

**Cron jobs.** `crons` and `schedules` in `vercel.json` work for every framework (section 4, "Schedules"), merged with the Build Output's as `vercel build` merges them. Each run costs what a request to the function costs.

**In the dashboard and the logs.** A deployment's page lists its functions (paths, runtime, memory, maxDuration, whether they stream), its prerendered pages and their expiration, whether it has Image Optimization, and its middleware. Each request's log line says what the cache did (`cache`: `HIT`, `STALE`, …), what the middleware said (`middleware`, `middleware_ms`) and what skew protection did (`skew`); the dashboard's logs filter by each, and image resizes are lines of their own. Usage shows image resizes, and cache hits with the function runs they saved. A store from before these columns (`deploy/aws/clickhouse/schema.sql`: `cache`, `middleware`, `middleware_ms`, `skew`) shows none of them until they're added, and the rest of the logs as before.

### Next.js

**Not finished.** A Next.js project (13.5 to 16, App Router and Pages Router) builds, packages and deploys, every route answers as it does under `next start`, and its workers share one cache. The router's part in front of Next is still to come ("What's still missing", below): don't put a Next.js site in production on forkrail yet.

forkrail runs **Next's own server**: the build asks Next for its standalone output (`.next/standalone`: `server.js` with the files Next traced for it), and that server runs as the deployment's one function, like any Node server. Next routes, runs the middleware, renders and reads its caches itself. The project isn't changed: nothing is added to `package.json` or `next.config`.

- **Found** by the `next` dependency, `"framework": "nextjs"` in `vercel.json`, or the project's setting. It comes first, as in Vercel's list: a Next app that also lists Vite is a Next app.
- **The build** is the project's own: `buildCommand`, else its `vercel-build`, `now-build` or `build` script, else `next build`; installed with the package manager of its lockfile, looked for in the folders above too, up to the repository's top (a workspace keeps one lockfile at its top: pnpm, Yarn and Bun workspaces build from the app's own folder), else the one `package.json` names in `packageManager`. It runs on the Node major `engines.node` asks for (22 or 24).
- **Its variables** are section 2's system variables, with these differences: `NEXT_TELEMETRY_DISABLED=1`; `NEXT_PRIVATE_STANDALONE=1`, Next's own switch for standalone output on every version; and **no `NOW_BUILDER`**, which every other framework's build gets. With it Next builds for Vercel's own infrastructure: it leaves its image optimizer and `sharp` out of the server, turns off its compression and writes its config for Vercel's proxy. The variables AI coding agents set (`CLAUDECODE`, `AI_AGENT`, `CURSOR_AGENT`, …) are taken off the build command, wherever the CLI was started: Next 16.4.3 and later end a finished build with exit 1 to show an upgrade notice when they see one.
- **On Next 15.5 and later the build goes through forkrail's adapter**, a small file the CLI writes into a temporary folder and names in `NEXT_ADAPTER_PATH`. Its `modifyConfig` sets `output: 'standalone'` whatever the config says (`output: 'export'` is left alone), and on 16.2 and later its `onBuildComplete` writes down what Next hands an adapter (`.next/forkrail-next.json`, read by the CLI, never packaged), entrypoints and their traced files included ("The handlers file", below). A project that sets its own `adapterPath` keeps it; forkrail then reads `.next`'s manifests, as it does before 16.2. Two ranges build without it, on `NEXT_PRIVATE_STANDALONE=1` alone, and serve the same: 16.0 and 16.1 (an adapter puts their whole build in minimal mode), and 16.3.0 to 16.3.4, whose `next build` fails with any adapter and a standalone server (`ENOENT .next/next-server.js.nft.json`, Next's bug, fixed in 16.3.5; the build log says so).
- **The package** is the standalone folder as Next wrote it, under the project's path in a workspace (`apps/web/server.js`), started through forkrail's `forkrail-server.mjs` beside it, which loads `forkrail-next.cjs` (the runtime's `next-standalone.cjs`) around Next's own `server.js`: Next builds `request.url` and its redirects from the visitor's host and protocol, which the router sets (never from a client's `x-forwarded-host`), not from its own `http://localhost:<port>`; and before the worker takes its first request Next has started up and loaded its pages' modules, within a few seconds of the start (`FORKRAIL_PRIME=0` in the env vars turns that off, `FORKRAIL_PRIME_PATHS=/,/pricing` renders up to 8 pages that are safe to render with no visitor too). Links stay links (pnpm's `node_modules`, Turbopack's externals), so each real folder is packaged once and Node resolves packages as in the build. Every file keeps its own time: Next takes a prerendered page's file time for when it was made, and a package stamped with one fixed time made every timed page look expired on its first request. Built away from the builder (`--local` on a Mac), the machine's `sharp` is replaced by Linux arm64's from npm at the versions the project has, so Next's image optimizer works on Lambda. The router answers `/_next/image` itself with the install's image function, as Vercel's platform does, on every version; only where the app's middleware runs on `/_next/image` does Next's own optimizer answer, and Next 13.5 and 14 have one in a standalone server only when the project itself lists `sharp` (Next's own rule): without it the images are answered as they are, and the build warns. `.env` files Next copies for its server are left out, as forkrail leaves them out of every package (section 2), with a warning that names them: the build read them, the running app doesn't.
- **Static files** go to the router, which answers them before the function: `.next/static` at `<basePath>/_next/static/` with `cache-control: public, max-age=31536000, immutable` (and under `assetPrefix`'s path too, whether it's a path, `/cdn`, or a URL with one, `https://cdn.example.com/x`, whose path pages ask the deployment for), `public/` at `<basePath>/` like any static file. Two kinds are in the package as well: a file the app's **middleware may match** is left to Next's server alone, so the middleware runs for it as under `next start` (the build log counts them; a matcher that leaves out `_next/static` and files, as Next's docs suggest, lets the router answer everything); and images, which Next's optimizer reads from its own server.
- **Settings**: a route's `export const maxDuration`, and `vercel.json`'s `functions` (`{"app/api/**/*": {"memory": 1024, "maxDuration": 60}}`, patterns over the project's source files, with or without `src/`). The deployment's function takes the most any route asks for; each route's own are in the manifest. `crons`, `regions` and the routing keys are read as for any project.
- **One cache for the deployment.** Next's caches (its pages, route handler answers, `fetch` and `unstable_cache` data, `'use cache'` entries, and what `revalidatePath`/`revalidateTag` invalidate) are the router's, not each worker's: a page made again by one worker is the one every worker serves, and a revalidation reaches them all. The function gets `FORKRAIL_NEXT_CACHE=1`, and the runtime hands Next forkrail's handlers. Next 15 and later find them at run time with nothing in the config. Next 13.5 to 14.2 take a handler only from their config, so their build's Node processes preload a file of forkrail's (`NODE_OPTIONS=--require`, for the build command alone) that gives the config Next loads `cacheHandler` (`experimental.incrementalCacheHandlerPath` on 13.5 and 14.0) naming `node_modules/.forkrail/forkrail-cache-handler.cjs`, and turns Next's in-process copy off; that file is written for the build and taken away after it, whichever way it ended, and Next copies it into its server. `next.config` is never changed. A project that names a cache handler of its own keeps it, and its caches are its handler's (the build says so).
- **The seed.** After the build, every prerendered page, route answer and data file is read through that Next's own cache in its standalone folder, as its server would read it, and written as the router's entries with the build's time as their age (the build folder's `next-seed/`); publishing puts them under `static/next-seed/<deployment>/` in each region, and a router reads one the first time it's asked for (`x-nextjs-cache: HIT` on a page's first request, `STALE` and made again once its `revalidate` has passed). They then leave the package, which Next never reads them from with the shared cache on; fallbacks and PPR shells stay. `FORKRAIL_NEXT_PRERENDERS=package` at build keeps them in the package too, for running it without forkrail's router. A seed that can't be made leaves them in the package, with a warning. A redeploy writes the seed again for its new deployment. Cost: one S3 object a page in each region the deployment runs in ($0.005 per 1,000 written; next-features' 7 pages are 130 KB), deleted with the deployment.
- **`.next/cache`** is kept after every build, as on Vercel, and restored before the next one: Turbopack's or webpack's cache (a small app's compile drops from about 5.6 s to 0.2 s), its fetch and image caches, and the keys Next keeps between builds there (the Server Actions key, below, and Draft Mode's). It's an archive of its own beside the dependencies' (`cache/<project>/<target>.next.tar.zst`, under the same rules: production builds use production's; a preview uses the previews', or production's until there's one; 30 days; `--no-cache` starts without it), so the dependencies aren't packed again every time. It's packed and written while the build's output is (a small app's 36 MB in about half a second here), and the build log says what was kept, its size, and whether Next's Server Actions key is in it. Wherever `distDir` puts the dist folder, its `cache` is the one kept.
- `forkrail deploy --prebuilt` takes a `.next` built with `output: 'standalone'`, or a finished static export.
- **A static export** (`output: 'export'`) has no server: the build is the same `next build`, and the folder Next exported to (`out`, or the one `distDir` names, as Next notes it in `.next/export-detail.json`) deploys as a static site, with no function, served as Vercel serves an export. Each page by its name without `.html`: `/about` is `about.html` (`about/index.html` with `trailingSlash`), `/` is `index.html`, and `/about.html` is not found. Next's own trailing-slash redirect, and the `redirects()`, `rewrites()` and `headers()` of `next.config`, apply as on Vercel (Next's build warns they "will not automatically work": that is for hosts that don't apply them), around the project's own `vercel.json` routing keys. `/404` answers 404; a path nothing serves gets the export's `404.html` with status 404. `_next/static/` is cached for good. With a `basePath` the files are served under it (`/docs/about`, `/docs/_next/static/…`), where the pages ask for them; Vercel's builder serves an export's files at the top whatever the base path, so there a base-path export's pages find neither their scripts nor each other. Middleware and API routes don't run in an export (Next's build says so). An export whose pages use `next/image` with the default loader needs `images.unoptimized: true`, as `next build` asks anywhere but on Vercel, whose optimizer answers those images.

Refused, with the reason:
- Next before 13.5; Yarn's Plug'n'Play (no `node_modules` for Next to trace its server from: set `nodeLinker: node-modules`).
- A server over 250 MB unzipped, Lambda's limit for a function's package: the message gives the size of `node_modules`, of `.next/server` and of the prerendered pages in it, and the three largest files. Such apps need functions packaged as container images, which aren't supported yet.
- A build that is killed (SIGKILL) is reported as out of memory, with the builder's size; when Lambda ends the whole builder, the deployment says the build ran out of the builder's memory. Either is built once more on the large builder ("Where a build runs", section 3); one that runs out there too fails. A builder has at most 10,240 MB; a larger app builds on a bigger machine with `--local`.

**The router in front of Next.** A Next deployment's entry in the routing table says so (`next`: its shape and the id of a config the build writes, `static/next/<id>.json`, with its middleware's matchers, preview id, base path, routes and prerenders), and the router serves it by Next's rules, as Vercel does:

- **Middleware first.** A request the app's middleware (or `proxy`) may match is never answered from the router's cache: it goes to Next, which runs the middleware and then reads the shared cache. "May match" is the matcher's pattern with its `has` and `missing`; one the router can't read counts as matching. Matchers that leave out the paths that need none (static pages, `_next/static`, images, as Next's docs suggest) let the router answer those itself.
- **Draft Mode** (Next's `__prerender_bypass` cookie with the build's id) and **on-demand revalidation** (`x-prerender-revalidate` with it) always go to Next and are never kept (`x-vercel-cache: BYPASS`, `REVALIDATED`).
- **Everything else Next keeps is served by the router** from the shared cache: an App Router page and its RSC payload or segment prefetches, a route handler's kept answer, a Pages Router page and its `/_next/data` JSON. While it's fresh the answer is `x-vercel-cache: HIT` with its `age`; once its time is up, or a tag of it was revalidated with a profile, it is served `STALE` while Next makes it again once, in the home region (the other regions get the new copy pushed); after `revalidatePath`, `updateTag` or `revalidateTag` without a profile the next request waits for Next to make it. The browser gets `public, max-age=0, must-revalidate`, Vercel's `vary` and `x-nextjs-prerender`, a weak ETag, and the config's `headers()`. A path a config redirect or rewrite may take, a site with `i18n`, and a partially prerendered page (PPR) are Next's to answer every time.
- **Headers.** Next's own client may send `x-middleware-prefetch` (every other `x-middleware-*` from a visitor is still taken off, `x-middleware-subrequest` included), and Next's internal headers (`x-invoke-*`, `next-resume`, `x-nextjs-*` but `x-nextjs-data`) are taken off. Next's answers keep the `x-middleware-*` its client reads (`-rewrite`, `-skip`, `-cache`, `-redirect`) and any the app names itself; the ones that carry a request on inside Next never reach a visitor. Next's `x-nextjs-cache` becomes `x-vercel-cache`.
- **`/_next/image`** (under the base path too) is answered by the image function with Next's own settings (sizes, `qualities`, formats, remote and local patterns, `minimumCacheTTL`, SVG, `contentDispositionType`), as on Vercel, unless the app's middleware runs on it; `unoptimized` or a custom loader leaves it to Next.
- A **missing `/_next/static` file** is a 404 from the router; a page that names its deployment (`?dpl=`, skew protection) still gets the older deployment's files. A path no route, file, redirect or rewrite of the app takes gets the build's own not-found page from the router (404, when the build made one that never goes stale). `<assetPrefix's path>/_next/…` is answered as `/_next/…`, as Next's own server does. The config's `headers()` go on the files the router serves too. A route's own `maxDuration` holds for its requests. Config redirects are left to Next.

**What's still missing**, each a step of its own:
- **Background regeneration on Next 13.5 to 14.2.** Those versions make a stale page again after answering, outside anything that keeps the worker open (15 and later hand it to `waitUntil`): when the worker is retired as soon as it's idle, that work is cut off and the cache keeps the old page until a worker that stays up makes it.

**The manifest.** A Next build's folder has `next.json` beside `build.json` (which names it under `next`, with its hash, the Next version, the build id, `shared_cache` and the seed's size; a deployment's record keeps the same) and `next-seed/`, the shared cache's seed. Format 1; fields are added, never changed in meaning:

| Field | What |
| --- | --- |
| `format`, `nextVersion`, `buildId`, `deploymentId` | `1`; the Next that built it; Next's `BUILD_ID`; `deploymentId` if the build had one |
| `source` | `adapter` (read from Next 16.2's adapter outputs) or `manifests` (read from `.next`'s own files) |
| `basePath`, `assetPrefix`, `trailingSlash`, `i18n` | as configured (`i18n` whole: `locales`, `defaultLocale`, `domains`) |
| `appDir`, `distDir`, `entry` | where `server.js` is in the package (`""`, or `apps/web`); the dist folder under it (`.next`); what the runtime starts (`forkrail-server.mjs` beside `server.js`) |
| `preview.id` | preview mode's id: what `x-prerender-revalidate` must carry for Next to regenerate a page on demand, and what Draft Mode's cookie is checked against. A secret of the build |
| `middleware` | `runtime` (`edge` for `middleware.ts`, `nodejs` for `proxy.ts`) and `matchers`: each `regexp` as Next matches a path with it, the `source` as written, and its `has`/`missing` conditions |
| `routes` | `routes-manifest.json`'s `redirects`, `headers`, `rewrites`, `dynamicRoutes`, `staticRoutes`, `dataRoutes`, `rsc`, `caseSensitive`, `pages404`, as Next wrote them |
| `adapterRouting` | 16.2 and later: the lists Next hands an adapter (`beforeMiddleware`, `beforeFiles`, `afterFiles`, `dynamicRoutes`, `onMatch`, `fallback`, `shouldNormalizeNextData`, `rsc`) |
| `cacheComponents`, `cacheHandler`, `sharedCache` | Cache Components or PPR is on; the project names a cache handler of its own; Next's caches are forkrail's shared cache |
| `functions[]` | each route that may run code: `pathname` (`/blog/[slug]`, without the base path), `page` (`/blog/[slug]/page`), `kind` (`page`, `api`, `appPage`, `appRoute`), `runtime` (`nodejs`, `edge`), its own `maxDuration`, `memory`, `regions` when set, and from Next 16.2 its output `id` and, for an Edge entry, `edgeRuntime` |
| `prerenders[]` | each path made at build: `pathname` (without the base path, as Next's own manifests name it), `route`, `page` (the route's page in the project, `/(site)/blog/[slug]/page`), `router` (`app`, `pages`), `kind` (`page`, `route`, `shell` for a PPR shell, `fallback`, `data`), `group` (16.2 and later: revalidated together), `parent` and `pprChain` (16.2 and later: the id of the output that renders it, and the headers a PPR shell's resume is chained with), `revalidate` (seconds; null never), `expire`, `status`, `headers`, `tags` (from `x-next-cache-tags`), `postponed` (the PPR state), `allowQuery`, `allowHeader`, `bypassFor`, `renderingMode`, and `files[]`: each with the `pathname` it answers, again without the base path (`/blog/first`, `/blog/first.rsc`, `/_next/data/<id>/blog/first.json`, `/blog/first.segments/…`; the home page's are `/`, `/index.rsc`), its `kind` (`html`, `rsc`, `data`, `body`, `segment`, `prefetch`, `meta`), the `file` and its `size` |
| `prerenderFiles` | `seed`: pages, route answers and data are in the seed alone (fallbacks and PPR shells stay in the package). `package`: the package holds them too (`FORKRAIL_NEXT_PRERENDERS=package`, no shared cache, or no seed) |
| `handlers` | 16.2 and later: `next-handlers.json`, the file below |
| `images` | `images-manifest.json`'s `images` (sizes, formats, `remotePatterns`, `minimumCacheTTL`, `loader`, `unoptimized`), as Next wrote it |
| `staticFiles[]` | each static file: the `pathname` it's served at (with the base path), `size`, `immutable`, `router` (the router serves it) and `server` (the package holds it too) |

**The handlers file.** Next 16.2 and later hand an adapter every entrypoint of the build with the files it needs, enough to answer requests by calling the entrypoints directly, with no Next server in between. forkrail runs the standalone server for every version today; so that the other way needs no second build, the same build also leaves `next-handlers.json` in the build folder (format 1), with its files still where the build put them, in the project's `.next` and `node_modules`:

| Field | What |
| --- | --- |
| `format`, `nextVersion`, `buildId`, `distDir`, `basePath`, `trailingSlash`, `i18n` | as in the manifest |
| `repoRoot`, `projectDir` | the repository's root as it was at build, and the project's path under it (`""`, `apps/web`); every path below is from that root |
| `routing` | Next's routing lists, whole (`beforeMiddleware`, `beforeFiles`, `afterFiles`, `dynamicRoutes`, `onMatch`, `fallback`, `shouldNormalizeNextData`, `rsc`) |
| `outputs[]`, `middleware` | every entry of `pages`, `pagesApi`, `appPages` and `appRoutes`, and the middleware: `kind`, `id`, `pathname` (with the base path, as Next hands it), `sourcePage`, `runtime`, `file` (the entry), `config` (`maxDuration`, `preferredRegion`, and an Edge entry's `env`, which holds the build's keys), `edgeRuntime`, `wasmAssets`, and `assets`: its traced files, as places in `files` |
| `files[]` | each traced file once: its path, or `[path, source]` when Next places it under another name than where it is (Turbopack's externals) |
| `prerenders[]` | as Next hands them: `id`, `pathname`, `parentOutputId`, `groupId`, `route`, `routeType`, `renderingMode`, `pprChain`, `allowQuery`, `allowHeader`, `bypassFor`, and `fallback` (`file` from the project, `initialStatus`, `initialHeaders`, `initialRevalidate`, `initialExpiration`, `postponedState`) |

It holds the build's own keys (an Edge entry's `env`), as the package does: it stays in the build folder and is never served.

### The response cache: ISR and CDN caching

The router keeps two kinds of function answers and serves them again without running the function, as Vercel does. Each answer it serves this way says so in `x-vercel-cache`: `HIT`, `STALE` (served while it's made again), `MISS`, `PRERENDER` (the build's fallback page), `REVALIDATED` or `BYPASS`.

- **Pages (ISR):** a Build Output's prerenders (SvelteKit's `config.isr`, Astro's `isr`, Nitro's `isr` route rules). A page is kept until it expires, then served once more (`STALE`) while the function makes it again in the background, one regeneration at a time. Until a page has been made, the build's fallback is served (`PRERENDER`), or the first visitor waits for it. Pages differ by the prerender's `allowQuery` parameters only, and the function sees only those. A regeneration that fails leaves the page as it was and is tried again after 30 s. A page is made without the visitor's cookies and `Authorization`, since it's kept for everyone, unless the install sets `FORKRAIL_PAGES_VISITOR_HEADERS=pass`; Draft Mode still gets them. Pages up to 20 MB.
- **The same pages on every router:** each keeps them on its own disk (at most 2 GB, never leaving the disk under 1 GB free; a page unused for 31 days is deleted, Vercel's rule), the most used in memory. The home region's router makes each page once and pushes it to the others over a connection of their own (port 7445, open only to the other routers' addresses; both sides prove the install's key). Another router asks home for a page it hasn't got and tells it when one goes stale. A router that can't reach home makes its own until it can, and home's newer copy replaces it then. `test-env.sh push` sets the link up in each region.
- **On demand:** a GET or HEAD with `x-prerender-revalidate: <the prerender's bypass token>` has home make the page again now, and answers `REVALIDATED` once it's kept; prerenders of the same `group` go stale together. Draft Mode: a request with the `__prerender_bypass` cookie set to the token runs the function every time (`BYPASS`) and nothing is kept.
- **CDN answers:** any function's answer that asks for it in `Vercel-CDN-Cache-Control`, `CDN-Cache-Control` or `Cache-Control`'s `s-maxage` (in that order) is kept in each router's memory for that long, then served stale for its `stale-while-revalidate` while one request refreshes it, as Vercel's CDN keeps them per region. The visitor gets it without `Vercel-CDN-Cache-Control`, and without `s-maxage` and `stale-while-revalidate` in `Cache-Control`. Kept: GET and HEAD without `Authorization`, statuses 200, 301, 302, 307, 308, 404 and 410, up to 10 MB, without `set-cookie`, `private`, `no-cache`, `no-store` or a `Vary` of `*` or `Cookie`; keyed by host, deployment, path and query as sent, `Accept`, `Accept-Encoding` and the headers `Vary` names. POSTs are never kept.
- **Every function answer** that sets no caching gets Vercel's `cache-control: public, max-age=0, must-revalidate`, `age: 0` and `x-vercel-cache: MISS`, APIs' too.

Requests answered from the cache are logged with outcome `cache`, and `cache` says how (`background` for a regeneration, `peer` for a page home made for another router). Cost: no fixed cost and nothing in S3; a kept answer is a function run saved, and a regeneration is one request to the function. Pages go between regions at $0.02/GB (a few KB a page), and the link between routers sends about 30 MB a month per region besides home's, under a tenth of a cent. The disks are the routers' own.

### Routing Middleware

A function that runs before routing on the paths it matches, as Vercel's Routing Middleware. It answers the request itself, lets it go on (`next()`, `x-middleware-next`), rewrites it to another path or a URL elsewhere (`rewrite()`, `x-middleware-rewrite`), changes its request headers (`x-middleware-request-*`, listed in `x-middleware-override-headers`) and adds headers and cookies to the answer. It comes from:
- `vercel.json`'s `proxy`: `{ "entrypoint": "proxy.ts", "matcher": ["/api/:path*"] }`, any `.js` or `.ts` file of the project, on Node (refused for Astro, as on Vercel);
- else a `middleware.ts` or `middleware.js` at the project's root, with its `config.matcher` and `config.runtime` (`edge` bundles it for the Edge runtime's imports);
- or the framework's own: Astro's `edgeMiddleware`, Nitro 2's `vercel-edge`, any Build Output route with `middlewarePath`.

The matcher is read without running the file: a path or a list of them (`/api/:path*`); none matches every path. The file is bundled with esbuild, its npm packages included. It runs on Node in the deployment's own Lambda function, Edge middleware too, as Vercel now runs middleware by default, after `vercel.json`'s redirects and headers and before the files, rewrites and functions. It must start answering within its own maxDuration, else 25 s, or the request gets 504 `MIDDLEWARE_INVOCATION_TIMEOUT`; a request body over 4.5 MB on its paths gets 413 `FUNCTION_PAYLOAD_TOO_LARGE`, as on Vercel. `@vercel/functions`' `next()`, `rewrite()`, `geolocation()`, `ipAddress()` and `waitUntil()` work. It runs beside a static site, Node functions in `api/` or a framework's Build Output; beside a Python API or a Node server (Express, a `node-server` Nitro build) it's refused for now. Next.js and Storybook projects don't take a root `middleware.ts`, as on Vercel.

Each call is in the request's log line (`middleware`: what it said; `middleware_ms`). Cost: each request it matches is a call on the deployment's Lambda worker, billed as Lambda time like a function request (Vercel bills it as an invocation); paths outside its matcher don't run it.

### Moving a site from Vercel

A SvelteKit, Astro, Nuxt, TanStack Start, React Router or Remix project needs no change: the same adapter, the same `vercel.json` (or `vercel.ts`).

1. Copy its env vars (`forkrail env add`, or import a `.env` in the dashboard), but not the `VERCEL_OIDC_TOKEN` that `vercel env pull` writes (a 12-hour token).
2. Deploy a preview, `forkrail deploy`, and request the same pages on both: status, headers (`x-vercel-cache`), streamed pages, images, a page loaded before a second deploy.
3. Then `forkrail deploy --prod`, and move the domain (section 6).

What code moved from Vercel finds: the `VERCEL_*` system variables (section 2), with `NOW_BUILDER=1` and `CI=1` in builds and `NODE_ENV=production` in a framework's functions; `x-vercel-forwarded-for`, `x-vercel-id` and the `x-vercel-ip-*` location headers (section 7), set by the router, never a client's; `@vercel/functions`' helpers and `@vercel/request-context`'s `waitUntil`, which keeps the invocation open until its work settles. `@vercel/analytics` reports to forkrail's own Web Analytics once a project has it on (below), and loads an empty script while it's off. Speed Insights keep working without errors: the router answers `/_vercel/speed-insights/*` with a script that does nothing and 204 for beacons (nothing is collected).

Services that sign in with Vercel's OIDC token need their own keys here, since apps get no `VERCEL_OIDC_TOKEN`: AI Gateway (the AI SDK's default provider for a model given by name) needs `AI_GATEWAY_API_KEY`, or a provider called directly; Vercel Blob connected by OIDC only (`BLOB_STORE_ID`) needs `BLOB_READ_WRITE_TOKEN`; Vercel Flags its SDK key in `FLAGS`; AWS through `@vercel/oidc-aws-credentials-provider` a project role instead ("Calling AWS from the app", below). The Workflow SDK picks Vercel's backend whenever `VERCEL_DEPLOYMENT_ID` is set, as it is here: set `WORKFLOW_TARGET_WORLD`. Services reached with a key in an env var (a database, Upstash, Blob with a read-write token, Edge Config with a request per read) work from forkrail as from anywhere.

**A Nitro project already on forkrail** builds with Nitro's `vercel` preset from its next deploy: route rules, `isr`, Nuxt Image and Nitro 3's tasks then work as on Vercel. One that pins `node-server` (`NITRO_PRESET`, `nitro.preset`, or a CI variable such as `GITHUB_ACTIONS` in its env) keeps running as one server, as before, and its build warns; remove the pin to move it.

### Not supported yet, and where forkrail differs

Refused by name, so the build stops rather than deploy something that behaves differently:
- Next.js before 13.5, and a Next.js server over 250 MB unzipped ("Next.js", above).
- React Router or Remix routes on the Edge runtime (`export const config = { runtime: "edge" }`: remove it to run the route on Node); Remix's classic compiler (`remix.config.js` without Vite); React Router without a `vite.config`.
- Astro with another adapter (`@astrojs/node`, Netlify's, Cloudflare's), or `output: "server"` without one; SvelteKit without Vercel's adapter, `adapter-auto` or `adapter-static`; TanStack Start without Nitro; a Nitro preset other than `vercel` and `node-server`; SolidStart before 1.0 (`solid-start`).
- Jekyll and Middleman: Vercel builds them with Ruby 3.3 and `bundle install`, and forkrail's builder has no Ruby, nor a C compiler for the gems that compile. Zola: Vercel runs its x86_64 Linux build; forkrail builds on arm64, and Zola's arm64 builds (0.20 and later) need glibc 2.35, where the builder's Amazon Linux 2023 has 2.34.
- In a Build Output: `services` (several apps in one deployment) and `daemons`; functions on Python or any runtime but Node and Edge; `awsLambdaHandler`; `experimentalTriggers` (Vercel Queues); schedules sent to a `function`, `topic` or `service` (Nitro's `NITRO_VERCEL_SCHEDULES=private`); routes with `locale` (Next.js's i18n), `important`, `env` or `handle: rewrite`/`resource`; functions whose own `environment` give a variable different values; more than 250 MB of functions, Lambda's limit; a version other than 3.
- `vercel.json`'s `skipMiddlewareRequestBody`; middleware written in Python.

Deployed with a warning in the build log: an Edge function's `assets` (not served to its `fetch`), `shouldAddSourcemapSupport` (stack traces without source maps), prerender settings forkrail doesn't read, a prerender fallback missing from the output (the page is made on its first request), and `images` in `vercel.json` beside a framework's own (the framework's are used).

Not built yet:
- Buffering, as Vercel does, the answer of a function whose `.vc-config.json` says it doesn't stream: forkrail streams it.
- Cache tags. `vercel-cache-tag` is taken off what visitors get, but invalidating by tag (`invalidateByTag`, `dangerouslyDeleteByTag`, Astro's `cache.invalidate`) does nothing: the page stays until it expires.
- Not compared with Vercel yet, so not claimed: SolidStart and Analog (the same Nitro path); Qwik City (its `vercel-edge` build is deployed as any build that writes a Build Output, its one Edge function on Node); Gatsby's SSR and DSG pages (Gatsby deploys as a static site: those need Vercel's Gatsby builder plugin, which forkrail doesn't run); Mastra, xmcp, Hydrogen and RedwoodJS.

Where forkrail does it differently, on purpose:
- One process and one memory size for all of a deployment's functions (note 2 above).
- Edge functions and Edge middleware run on Node, in the deployment's own Lambda function; Vercel recommends Node for them too.
- POSTs are never cached. (Vercel kept POST answers of Astro routes behind its edge middleware, serving one visitor's to the next.)
- React Router and Remix functions take whole npm packages, and pnpm and Yarn projects install flat.
- A cron in both `vercel.json` and the framework's output runs once; Vercel's list has it twice.
- A Nitro build pinned to `node-server` runs as one server; Vercel would deploy no server for it.

### Static sites

A site with no server deploys as static files alone: no function, nothing to start, nothing billed but storage and transfer. The router serves every file from S3 in its own region.
- **A framework's build**: Vite, Create React App, Vue CLI, Angular, Docusaurus, VitePress, Eleventy, Gatsby, Astro (static output) and SvelteKit (with `@sveltejs/adapter-static`). The CLI finds the framework in `package.json` (or `vercel.json`'s `framework`), installs, runs the `build` script (or the framework's own build), and deploys Vercel's output folder for it: `dist`, `build`, `_site`, `public`, `docs/.vitepress/dist`, or Angular's `dist/<project>/browser`. `outputDirectory` names another.
- **Anything else** ("Other" on Vercel): with a `build` script it's built first; then `public/` is served if there is one, else the folder itself, without hidden files (but `.well-known`), `node_modules`, `vercel.json`, what `.gitignore` and `.vercelignore` name, and what Vercel leaves out at the top of the folder: `README.md`, `package.json`, lockfiles, `middleware.js`/`.ts` and `api/`.

`/docs` and `/docs/` both serve `docs/index.html`. A path nothing serves gets the site's `404.html` with status 404, or forkrail's `NOT_FOUND` page. Files are revalidated on every visit (`max-age=0`, then a 304), as on Vercel, unless your `headers` say otherwise.

Every file goes with the headers Vercel's CDN gives its files: `access-control-allow-origin: *`; `x-vercel-cache` (`HIT` when the router had it in memory, `MISS` when it read it from S3), with `last-modified` the time it came into the router's memory and `age` the seconds since, as Vercel's are its cache's; `content-disposition: inline` with the name of the path that found it (`/docs` is "docs", a rewrite to `/rewritten.html` "rewritten.html"). A file sent as it is (not compressed) says `accept-ranges: bytes` and answers one byte range (`Range: bytes=0-99`, `bytes=100-`, `bytes=-100`) with 206, or `416 RANGE_NOT_SATISFIABLE` when the range lies outside the file; several ranges at once get the whole file, as on Vercel. `If-Range`, `If-None-Match` and `If-Modified-Since` are answered too; a 304 carries `cache-control`, `etag` and `x-vercel-cache` alone. A 404 page takes no ranges and no 304s.

A framework's own routes come with it, as Vercel adds them: an Angular, Vue CLI or Create React App site answers any path its files don't have with its `index.html` (after your own rewrites, which win); Create React App's `/static/`, Astro's `/assets/` and `/_astro/`, and the hashed files of Gatsby, Docusaurus and Vue CLI are cached for good; Docusaurus and Gatsby send a missing path to their `404.html`. A Gatsby build whose plugin writes `public/__now_routes_g4t5bY.json` gets those routes instead of the preset's, and the file isn't served, as on Vercel. Vite, VitePress, Eleventy and SvelteKit have none: a single-page app built with Vite needs a rewrite of its own (`{ "source": "/(.*)", "destination": "/index.html" }`), as on Vercel.

Files answer GET and HEAD; OPTIONS gets a 204, anything else a 405, whether or not the deployment has a function.

An API keeps its `public/` folder the same way: its files are served before the function.

### Redirects, rewrites and headers

Every routing key of `vercel.json` works as on Vercel, for APIs, websites and static sites: `redirects`, `rewrites`, `headers`, `cleanUrls`, `trailingSlash`, and the older `routes` (with `handle: filesystem`, `miss`, `hit` and `error`). Patterns are Vercel's: `/blog/:slug`, `/docs/:path*`, `/(.*)`, inline regexes (`/:id(\\d+)`), lookaheads (`/((?!uk/).*)`), and `has`/`missing` conditions on headers, cookies, the host and the query, with comparisons (`eq`, `neq`, `inc`, `ninc`, `pre`, `suf`, `re`, `gt`, `gte`, `lt`, `lte`).

```json
{
  "cleanUrls": true,
  "trailingSlash": false,
  "redirects": [{ "source": "/old-blog/:slug", "destination": "/blog/:slug", "permanent": true }],
  "rewrites": [{ "source": "/app/:path*", "destination": "/app/index.html" }],
  "headers": [{ "source": "/(.*)", "headers": [{ "key": "X-Frame-Options", "value": "DENY" }] }]
}
```

- The order is Vercel's: `cleanUrls` and `trailingSlash`, then `redirects`, then `headers`, then the deployment's files, then `rewrites`.
- Redirects default to 308; `permanent: false` is 307, `statusCode` any of 301, 302, 303, 307, 308. The visitor's query string is kept, with the destination's parameters set over it (`/old?x=1` to `/new?from=old` goes to `/new?x=1&from=old`).
- A rewrite to one of the deployment's files serves it; a rewrite to the function (`/api/main.py`) runs it with the path the visitor asked for, and the destination's query parameters set over the visitor's, as on Vercel; a rewrite to `https://...` is proxied (120 s to start answering, else 504 `ROUTER_EXTERNAL_TARGET_ERROR`). A rewrite's `statusCode` is the response's status; if its destination doesn't exist, that status is all it answers with.
- With `cleanUrls`, `.html` files are known only by their clean names, as on Vercel: a destination of `/about` finds `about.html`, one of `/about.html` finds nothing.
- Headers apply to whatever answers: a file, the function, an external site, the 404 page. A header your function sets itself wins over the same one from `vercel.json`; one from an external site doesn't.
- Patterns from `redirects`, `rewrites` and `headers` match case-sensitively; `routes` alone, case-insensitively, as on Vercel. Values in `has` and `missing`, and cookie names, match whatever their case (what they capture keeps it); query keys don't.
- As on Vercel, for every deployment: a path with repeated slashes (`//about`) is redirected (308) to the same path with single ones, an encoded `..` in a path is refused (403, or 400 when it climbs above the root), and every query string reaches the app rebuilt the same way (`a+b` as `a%20b`, `%7E` as `~`).
- Vercel's limits: 2,048 items in each key, 4,096 characters in a source or destination, 16 conditions in a `has` or `missing`.
- Mistakes are refused when deploying, before anything is uploaded: a pattern that doesn't compile, a destination using a parameter the source doesn't have, `permanent` with `statusCode`.

The deploy prints how many rules it found. The router compiles them once per deployment; 2,048 rules cost it about 7 µs a request. Each request's log line names the rule that decided it (`redirects[3]`). `tools/routing-check.py` compares forkrail's answers with Vercel's for the test projects in `test-apps/routing*`, request by request.

**Transforms.** A route in `routes` may change the request before it goes on, and the answer on its way back, with Vercel's `transforms`: `request.headers`, `request.query` and `response.headers` with `set` (replaces), `append` (adds, or sets if missing) and `delete` (the key, or with `args` only those values), the target a name or comparisons on names (`{"pre": "x-react-"}`); and `request.path` with `set`, the path the function sees (`"args": "/posts/$id"`). In `rewrites` only `request.path` is allowed, written as a path pattern (`/posts/:slug`). `env` in a transform is refused: the routers can't read env values.

```json
{ "routes": [{ "src": "/home", "continue": true, "transforms": [
  { "type": "request.query", "op": "set", "target": { "key": "theme" }, "args": "dark" },
  { "type": "response.headers", "op": "delete", "target": { "key": "x-powered-by" } }
] }] }
```

**Challenge or deny in a route.** A route in `routes` (or a framework's Build Output) may say `"mitigate": {"action": "deny"}` or `{"action": "challenge"}`, as on Vercel: a request it matches gets the firewall's answer (section "Firewall": 403 with `x-forkrail-mitigated: deny`, or the challenge page), and one with a challenge pass goes on through the routes after it, with the route's headers. The request log names the route as the firewall rule.

**`vercel.toml` and `vercel.ts`.** Any one of `vercel.json`, `vercel.toml`, `vercel.ts`, `vercel.mts`, `vercel.js`, `vercel.mjs` or `vercel.cjs` works; two is refused, as on Vercel. A `vercel.ts` runs with Node when the project is read (types stripped, top-level `await` allowed) and its `config` export (or default export) is taken as `vercel.json`. If it imports `@vercel/config` before the project's packages are installed, that one package is installed in a cache of forkrail's first (the version in `package.json`, else the latest). A push's "does this branch deploy" check can't run a `vercel.ts` in the control function, so there `git.deploymentEnabled` is read only from `vercel.json` or `vercel.toml`.

### Bulk redirects

For thousands of plain path-to-URL redirects (up to 1,000,000 a project), as on Vercel. They're looked up before anything else: the project's own (below), then the deployment's, then every other rule. Each is a `source` (a path, or a full URL to match one host: `https://old.example.com/page`), a `destination` (a path or a URL), and optionally `permanent` (308, else 307), `statusCode` (301, 302, 303, 307, 308), `caseSensitive` (default false) and `preserveQueryParams` (default false: the visitor's query string is dropped). Sources are matched exactly, without their query, percent-decoded, without regard to case unless `caseSensitive`; one with a host is tried before one without.

**At deploy:** `bulkRedirectsPath` in `vercel.json` names a file, or a folder of up to 100, in CSV (a header naming the columns, in any order; `t`/`f` for booleans), JSON (a list) or JSONL. The build may write them; they're read after it, and never served as files.

```csv
source,destination,permanent
/old-page,/new-page,true
https://old.example.com/blog,/blog,true
```

**Per project, without a redeploy:** the project's Bulk redirects page, `forkrail redirects`, or the API (`POST /api/v1/projects/NAME/redirects` with `{"op": ...}`). Every change is staged as a new version and routes nothing until it's published; publishing reaches every router within seconds. Older versions can be restored.

```bash
forkrail redirects add my-site /old /new --permanent
forkrail redirects import my-site redirects.csv          # added to the current ones; --replace for all
forkrail redirects ls my-site -q old                     # the staged ones if there are changes, else the live ones
forkrail redirects test my-site https://my-site.example.com/old
forkrail redirects publish my-site                       # or: discard, versions, restore VERSION, export
```

A million redirects make an 88 MB table, built in about 2 s; the router keeps a large one on disk, mapped, and looks a request up in under a microsecond. Through a login, an import is at most 5 MB (about 50,000 redirects); with the install's AWS credentials (`--aws`) any size.

### Image Optimization

`images` in `vercel.json`, in Vercel's shape (`sizes`, `domains`, `remotePatterns`, `localPatterns`, `qualities`, `minimumCacheTTL`, `formats`, `dangerouslyAllowSVG`, `contentSecurityPolicy`, `contentDispositionType`), gives the deployment `/_vercel/image?url=…&w=…&q=…` (and `/_next/image`), as on Vercel: the same checks and the same answers (`400 INVALID_IMAGE_OPTIMIZE_REQUEST`, `404 NOT_FOUND` for a source that isn't there), the first of `formats` the browser's `Accept` names, the width `w` without ever enlarging. `url` is one of the deployment's own paths (a file, or what its function answers) or a URL elsewhere that `domains` or `remotePatterns` allows; `remotePatterns`' and `localPatterns`' `hostname` and `pathname` are regular expressions, as in Vercel's Build Output. Only JPEG, PNG, WebP and AVIF are resized; other images (GIF, SVG with `dangerouslyAllowSVG`, icons) and animations are served as they are. Without `images`, those paths are the app's like any other.

```json
{ "images": { "sizes": [640, 1080, 1920], "formats": ["image/avif", "image/webp"], "minimumCacheTTL": 60 } }
```

A framework's Build Output brings its own settings, served the same way: SvelteKit's adapter `images` option, Astro's `imageService`, Nuxt Image's Vercel provider. With `images` in `vercel.json` too, the framework's are used and the build warns.

The router answers from what it has kept: the most used in its memory, and every one once for the whole install, in the home bucket's `images/` (Vercel's shared cache: an image made through one region is there for every other; a router elsewhere reads it across regions when its memory doesn't have it). A miss is resized by the install's `forkrail-image` function in the router's region (`deploy/aws/image.sh release`, which applies it in each region and keeps optimized images 31 days from when they're made: the next request after that makes one again). What's kept, and for how long, follows Vercel: a deployment's own file is kept by its content for every deployment of the project (a redeploy doesn't start over) and never goes stale; a function's image or one from elsewhere is kept for the longer of its own `max-age` and `minimumCacheTTL`, then served once more (`x-forkrail-cache: STALE`) while it's made again. Requests are logged with outcome `image_hit`, `image_miss`, `image_stale`, `image_refused` or `image_failed`. Since what's made is kept for everyone, a function that answers an image gets none of the visitor's headers (no cookies, no `Authorization`, not their address or location), as Next.js's optimizer and Vercel's: an image that depends on who asks (a private avatar) belongs at its own URL, not behind `/_vercel/image`.

Limits: a source over 8,192 pixels a side is refused, as on Vercel; an image a function answers is read up to 4 MB, and one from elsewhere up to 50 MB (Vercel reads up to 300 MB). Images from elsewhere are fetched from public addresses only, through at most 3 redirects. Cost: nothing while unused; about $0.011 per 1,000 WebP resizes (Lambda time and one S3 write), more for AVIF, and $0.0004 per 1,000 reads from S3 when the router's memory doesn't have one; one copy of each image is stored. Outside the home region, a resize's write and a router's read cross regions at $0.02/GB (a 100 KB image: $0.000002).

### Purging the cache by tag

What the routers keep (ISR pages, and answers that ask for the CDN with `s-maxage`, `CDN-Cache-Control` or `Vercel-CDN-Cache-Control`) can carry cache tags, as on Vercel: the answer's `Vercel-Cache-Tag` header (comma-separated; visitors never see it), or `addCacheTag()` from `@vercel/functions`, which is how an ISR function tags its page. Tags are case-sensitive, up to 256 bytes each, 128 an answer, and never contain a comma.

A purge names tags in one project, and in one environment or both:
- **invalidate**: what was kept with those tags before is served once more (`x-vercel-cache: STALE`) while it's made again in the background;
- **delete**: it's made again while the visitor waits (`REVALIDATED`). Many paths at once can stampede the functions, so prefer invalidate. With a revalidation deadline, it's served stale (and made again) until then, and deleted only if nothing asked for it meanwhile;
- `*` is everything the project's deployments kept.

From the app, `@vercel/functions`' `invalidateByTag(tags)` and `dangerouslyDeleteByTag(tags, { revalidationDeadlineSeconds })` purge the deployment's own project and environment, and the promise settles once the home region's router has it, which pushes it to the others at once (Astro's `cache.invalidate` calls `invalidateByTag`). From the CLI or the dashboard's API, a member or owner purges either environment, or both:

```bash
forkrail cache invalidate -p my-site --tag products,product-123     # --target production|preview; both when not given
forkrail cache dangerously-delete -p my-site --tag products --revalidation-deadline-seconds 3600
forkrail cache purge -p my-site                                     # invalidate everything; --yes skips the question
forkrail cache ls my-site                                           # the latest purges
```

A purge from the app reaches every router over the routers' own link (under a millisecond between two routers on one machine; about half a round trip between regions); one from the CLI or the dashboard goes in the routing table, which every router reads within a second. Purging by source image (`invalidateBySrcImage`, `--srcimg`) is accepted but doesn't yet reach optimized images, which follow their own cache time. Cost: nothing while unused; a purge is a few hundred bytes between routers.

### Routing rules without a redeploy

Vercel's project-level routing rules: each matches a path (exactly, by a path pattern like `/blog/:slug`, or by a regex), optionally a host, header, cookie or query, and then rewrites, redirects (301, 302, 303, 307, 308) or answers with a status, and changes response headers, request headers and the query. They run on every request to any of the project's deployments, after bulk redirects and before the deployment's own routes: the first rule with a rewrite, redirect or status decides, and the changes of every rule that matches apply. A rewrite to a path that isn't one of the deployment's files goes on through the deployment's own routes. Rules match with the deployment's case sensitivity unless they say they're case-sensitive.

Like bulk redirects, every change is staged until published, with history and restore: the project's Routing rules page, `forkrail routes`, or `POST /api/v1/projects/NAME/routes`.

```bash
forkrail routes add my-site "API proxy" --src '/api/:path*' --src-syntax path-to-regexp --action rewrite --dest 'https://api.example.com/:path*'
forkrail routes add my-site "Old blog" --src /blog --src-syntax equals --action redirect --dest /articles --status 301
forkrail routes add my-site "Login" --src '/account/:path*' --src-syntax path-to-regexp --action redirect --dest /login --missing cookie:session
forkrail routes add my-site "CORS" --src '^/api/.*$' --set-response-header 'access-control-allow-origin=*'
forkrail routes ls my-site                               # move, enable, disable, update, rm, export, import
forkrail routes test my-site /account/x -H 'cookie: session=1'
forkrail routes publish my-site
```

Looking and testing are anyone's in the team; changes are a member's or an owner's.

**Calling AWS from the app.** On Vercel you'd put access keys in env vars. A forkrail app runs in your own account, so give its project an IAM role instead, and the AWS SDK picks it up with no keys at all:

```bash
forkrail project my-site --role my-site-role      # by name or ARN; `default` for the shared forkrail-worker
```

**Private addresses.** A database or service that admits only forkrail's routers (the analytics box does) can be reached through the router: each router maps a name to an address its network reaches (`FORKRAIL_EGRESS_TARGETS=analytics=172.31.9.153:8443,postgres=172.31.9.153:5432`), and a project allowed that name gets `FORKRAIL_EGRESS_ANALYTICS=127.0.0.1:<port>` in its environment. Connect there as you would to the real address: the bytes pass through the router untouched, TLS included. Connections close when the worker retires, so a pooled connection may be gone after a pause; retry once on a reset.

```bash
forkrail project my-site --egress analytics       # comma-separated, or `none`
```

Both take effect on the next deploy. The dashboard itself is deployed this way (`deploy/aws/dashboard.sh`).

### Skew protection

A page loaded before a deploy keeps talking to the deployment that served it: its scripts, data requests and server actions don't land on a newer deployment that has other file names or other data shapes. As on Vercel, a request that names one of the project's deployments (`?dpl=<id>`, the `x-deployment-id` header, or the `__vdpl` cookie) is served by that deployment, its files, routes and function, while it's younger than the maximum age (12 hours by default, counted from its creation) and still kept. SvelteKit, Nuxt, TanStack Start (Nitro's `vercel` preset), Qwik and React Router set the cookie on page loads; Next.js and Astro put the id on the requests they make; Remix does neither, on Vercel either. A Next.js build with it on gets `NEXT_DEPLOYMENT_ID=<the deployment's id>`, as on Vercel (a value of the project's own wins, and is then taken as the deployment's name): Next puts it on its scripts and styles (`?dpl=`), and on its navigations', prefetches' and Server Actions' requests (`x-deployment-id`), so an open page's chunks, RSC data and actions are its own deployment's. Next 14.1.4 and later do so by themselves; 13.5 to 14.1.3 only with `experimental.useDeploymentId` (and `useDeploymentIdServerActions`) in their config, as Vercel's docs say, and the build says so. Next 15 and later keep the key that encrypts Server Actions' bound arguments in `.next/cache` (`.rscinfo`, renewed every 14 days), which the builder keeps after every build, so a project's production builds share it until Next renews it, as on Vercel (13.5 to 14.2 make a new one every build); setting `NEXT_SERVER_ACTIONS_ENCRYPTION_KEY` in its env vars fixes one for good. A Build Output that names its deployment (`deploymentId`) is reached by that name too. The build and its app get `VERCEL_SKEW_PROTECTION_ENABLED=1` (and `FORKRAIL_SKEW_PROTECTION_ENABLED`) when it's on, which is what tells a framework to do so; `VERCEL_DEPLOYMENT_ID` is always set.

It's on by default for new projects, as on Vercel (every project made on Vercel in November 2026 had it, whatever its framework): a new project has it on from its first build that succeeds, with the 12-hour maximum age. Projects made before keep their setting. The dashboard has it under Settings → Advanced → Skew Protection (on, the maximum age, a threshold deployment chosen from the project's, the allowed domains); it costs nothing (the router does it).

```bash
forkrail project my-site --skew-protection on               # off turns it off; at once, no redeploy
forkrail project my-site --skew-max-age 7d                  # 30m, 12h, 7d…; `default` is 12 hours
forkrail project my-site --skew-threshold a1b2c3d4          # this deployment and older ones are never reached again; `none`
forkrail project my-site --skew-allowed-domains app.example.com,*.example.org   # other sites whose pages may pin; `*` for any; `none`
```

What's never pinned, whatever names a deployment: a page load (`sec-fetch-dest: document`), so a refresh gets the newest deployment; a request from another site, unless it's one of the allowed domains (up to 12); a preview on a production name (`<project>.<base>` or a production domain: protection guards previews, and those names are open); on any other name (a deployment's own, a branch's), a deployment whose own address the visitor's way in doesn't open (a sign-in, share link or exception opens one host only, so with deployment protection on previews pin nothing there); a `__vdpl` sent twice with different values (another app under the base domain can set one for all of it); a deployment older than the maximum age, before the threshold, deleted by the retention policy, or another project's. Googlebot and Bingbot reach back 60 days, when they connect from the addresses Google and Bing publish. A `dpl` that names one of the project's deployments is taken out of the query before routing and the app see it. The access log's `skew` says what happened to each request that named a deployment (`pinned`, `current`, `document`, `cross_site`, `environment`, `protected`, `threshold`, `expired`, `unknown`). The router honours ids at once; a framework starts sending them from the first deployment built with it on.

### Web Analytics

Visitors, page views, bounce rate and custom events per project, as Vercel's Web Analytics shows them, collected by the routers and kept on the analytics box in your AWS account. It's off until someone switches it on for a project, in the dashboard (Analytics, or Settings → General → Web Analytics) or here; the routers apply it within seconds, with no redeploy:

```bash
forkrail project my-site --analytics on      # off stops collecting; what was kept stays
```

Pages count once they load the script, with forkrail's own package, `@forkrail/analytics` (`packages/analytics` on npm: one import per framework, `<Analytics />` from `/next`, `/react`, `/remix`, `/vue` or `/astro`, `injectAnalytics()` from `/sveltekit`, the `/nuxt` module, or `inject()` anywhere else, TanStack Start included). An app that already uses `@vercel/analytics` (MIT) needs no change: the routers answer `/_vercel/insights/script.js`, `/view` and `/event` themselves, and nothing goes to Vercel. Without a package, a script tag does the same (`<script defer src="/_forkrail/insights/script.js"></script>`). While it's off, the script is an empty one, so an app can ship the package before it's switched on. A function's `track()` from `@vercel/analytics/server` posts to `VERCEL_URL`, which forkrail sets, and counts as the visitor whose request it was answering.

No cookies: the router makes a visitor's number from their address, browser and the project under a random salt that changes every UTC day (shared by every router through `state/insights/salts/` in the state bucket, which the router role may write), so the same person is a new visitor the next day, and neither the address nor the browser string is kept. Bots, headless browsers and scripts aren't counted, nor are beacons sent from another project's pages. Each page view or event is a row of ClickHouse's `web_events` (13 months), through the router's log and Vector; a beacon counts in the project's requests and usage as any request does. Visitors in the last 3 minutes are the dashboard's "online".

### Firewall

Each project has a firewall in front of all its deployments and domains, and the install has one in front of every project: custom rules and IP blocks, in the shape of Vercel's firewall configuration, so a configuration exported from Vercel imports as it is (`forkrail firewall import my-api vercel-firewall.json`). It runs in the routers before deployment protection and before anything of the deployment's (static files, redirects, the function), and changes without a redeploy.

**Custom rules** run in order. A rule has condition groups: it matches when any group does, and a group when all its conditions do. A condition is a parameter, an operator and a value:
- parameters: `path` (decoded, without a trailing slash), `raw_path` (as sent), `target_path` (the static file a request is served from; the request's path for a function), `method`, `host`, `user_agent`, `header`, `query` and `cookie` (each with its name as `key`), `ip_address` (an address or CIDR range), `protocol` (`HTTP/1.1`, `HTTP/2.0`), `scheme`, `environment` (`production`, `preview`), `region` (the router's AWS region), `geo_continent`, `geo_country`, `geo_country_region` (the region's name), `geo_city`, `geo_as_number`, `ja4_digest` (the TLS client's JA4 fingerprint, `t13d1516h2_8daaf6152771_e5627efa2ab1`: a browser, a script library or a scanner each have their own whatever user agent they claim; none over plain HTTP), `rate_limit_api_id`;
- operators: `eq`, `neq`, `inc` and `ninc` (is / isn't any of up to 75 values), `pre`, `suf`, `sub`, `re` (a regex), `ex` and `nex` (a header, query parameter or cookie is or isn't there), `gt`, `gte`, `lt`, `lte`, and `list` (the address is in one of the configuration's named lists); `neg` inverts any of them. Every comparison ignores case;
- actions: `deny` (403, `x-forkrail-mitigated: deny`), `challenge` (below), `redirect` (307, or 301 with `permanent`, to a path or URL), `bypass` (skip the remaining rules), `log` (record and go on; the last log rule matched is the one reported), `rate_limit` (below). A deny or challenge may hold the client's address for 1 minute to 1 hour (`actionDuration`): every request it sends to the project gets that answer until then (from an install rule, to every project). Holds are each router's own.

**The challenge** is a page the router answers with (403, `x-forkrail-mitigated: challenge`) whose script does a moment's work (a SHA-256 proof of work, under a second on a phone) and then reloads with a pass: a cookie, `_forkrail_challenge`, good for an hour for that browser's network (/24, or /48 for IPv6), user agent and host, signed with a key every router shares (`/forkrail/router/challenge-key` in SSM). A rule that challenges lets a request with a pass by, and the rules after it run. It's for browsers: anything that doesn't run JavaScript (an API client, `curl`) stays at the page, so use rate limits and IP blocks for APIs.

**Rate limits** are rules with the `rate_limit` action and a `rateLimit`: `limit` requests per `window` seconds (10 to 3,600; limits 1 to 10,000,000) for each client, where `keys` say what a client is: `ip` (an IPv6 address counts by its /64), `ja4`, `header:<name>`, `user_agent`, `asn`, `cookie:<name>`, `query:<name>`, `path`, `host`, `country`; several keys count each combination apart, and requests without a key's value share one count. `algo` is `fixed_window` (windows start on the clock, every `window` seconds) or `token_bucket` (`limit` requests at once, then `limit` per `window` as they come back). Under the limit the request goes on to the next rule; over it, it gets the rule's `rateLimit.action`: `rate_limit` (the default: 429 with `retry-after` and `x-forkrail-mitigated: rate_limit`), `deny` (403) or `log`. With `actionDuration`, a client that goes over gets that answer for every request to the project for that long. Counts are each router's own, as Vercel's are each region's; a router keeps up to 1,000,000 counts at once (`FORKRAIL_RATE_LIMIT_BUCKETS`), past which new clients go uncounted rather than refused. A configuration may have up to 1,000 rules, 100 of them rate limits.

**Rate limits from an app's own code:** a rule with a `rate_limit_api_id` condition (`--if 'rate_limit_api_id eq update-object'`) answers questions the app asks at `GET /.well-known/forkrail/rate-limit-api/<id>` on its own host, with the client's key (a user id, say) in `x-forkrail-rate-limit-key`: 204 within the limit, 429 over it, 403 if a rule denies, 404 if no rule names the id. Only rules naming the id judge these questions, IP blocks don't, and the count is the key's (the rule's `keys` aren't used). A question counts only from the app's own function: it must carry the `x-forkrail-visitor` header the router put on the request the function is answering (pass it on as it came, within five minutes); from anyone else it is an ordinary request, so nobody outside can run up a user's count. An app moved from Vercel that calls `checkRateLimit` from `@vercel/firewall` with the request (or its headers) keeps working unchanged: the package passes the request's headers on.

**IP blocks** come before the custom rules: an address or range, on one host, every subdomain (`*.example.com`) or every host of the project (`*`), with `deny`, `log`, or `bypass` (the custom rules are skipped). The first entry that matches decides. Port 80 refuses blocked addresses too.

The install's firewall runs first; a project's bypass never skips it. Turning a project's custom rules off (`forkrail firewall disable`) leaves its IP blocks on. Schedules' runs come from the router itself and skip the firewall. Header conditions see the headers as the app will: the router's own `x-forwarded-*` and location headers replace any the client sent.

**Attack mode** challenges every visitor to a project, before its IP blocks and rules, for 1, 6 or 24 hours (`forkrail firewall attack-mode my-app --for 6h`, `--off`); it applies at once. Crons, verified crawlers (Google, Bing, Apple, DuckDuckGo and OpenAI's search and user fetchers, by the address ranges they publish, fetched daily) and Stripe's and GitHub's webhooks pass, as do system bypasses.

**System mitigations** are always on, in front of everything, for every project: an address may hold at most 256 connections open (`FORKRAIL_SHIELD_CONNECTIONS`), and one that sends more than 5,000 requests in 10 seconds over all projects (`FORKRAIL_SHIELD_REQUESTS`; the busiest legitimate client seen sent 391) is blocked for 10 minutes (`FORKRAIL_SHIELD_BLOCK_S`): its connections are closed before the TLS handshake, and requests on connections it has get 429. What they refuse isn't logged request by request: a `mitigation` line per address a minute says how much, and `forkrail-attack` (`test-env.sh attack-alarm`) mails when 10,000 or more are refused in 10 minutes. **System bypasses** leave an address or range alone, on every host of the project or one domain (`forkrail firewall system-bypass add my-app 203.0.113.0/24 --domain api.example.com --note partner`; up to 100), and apply at once; they never skip the custom rules. A project's (which its members set) covers its own hosts' requests; only the install's (an owner's, `--install`) also lift the connection cap and the refusal of a blocked address at connect, since a connection names no host yet. The limits are an install's to change, all projects at once, and apply on every router within a second: `forkrail firewall limits --connections 1024 --requests 20000 --block 5m` (an owner's; without options it shows them; `--reset` goes back to 256, 5,000 and 10 minutes), or the DDoS tab's Edit. Raise them for traffic that really comes from few addresses (a carrier's shared address, a partner's servers in bulk); for one known address a system bypass is better. An owner can pause the system mitigations for 24 hours (`forkrail firewall pause`, `resume`). Below HTTP, Global Accelerator brings AWS Shield Standard.

Every change is a draft until published (attack mode, system bypasses and the pause apply at once); `diff` shows what publishing would change, every version is kept, and any can be made live again. A published change is in force on every router within about a second (each router reads the routing table every second, and loads a new configuration before using it: one it can't read leaves the previous one in force).

```bash
forkrail firewall rule add my-api --name "Block .env probes" --if 'path suf .env' --then deny --for 1h
forkrail firewall rule add my-api --name "Admin from outside the US" --if 'path pre /admin' --and 'not country eq US' --then deny
forkrail firewall rule add my-api --name "Old page" --if 'path eq /old' --then redirect --location /new --permanent
forkrail firewall rule add my-api --name "Scrapers" --if 'ua sub python-requests' --or 'asn inc 14061,16276' --then log
forkrail firewall rule add my-api --name "Login" --if 'path eq /api/login' --then rate_limit --requests 10 --window 60 --over deny --for 15m
forkrail firewall rule add my-api --name "API keys" --if 'path pre /api' --then rate_limit --requests 1000 --window 60 --key header:x-api-key --algo token_bucket
forkrail firewall ip add my-api 192.0.2.0/24 --host api.example.com --note "abuse report 42"
forkrail firewall show my-api                 # rule on|off|rm|move, ip rm, enable, disable, versions
forkrail firewall test my-api https://api.example.com/admin --ip 192.0.2.7 --country DE
forkrail firewall diff my-api
forkrail firewall publish my-api              # rollback VERSION, discard
forkrail firewall show --install              # the install's own, an owner's to change
forkrail firewall rule add my-app --name "Login page" --if 'path eq /login' --then challenge
forkrail firewall attack-mode my-app --for 1h  # --off
forkrail firewall system-bypass add my-api 203.0.113.0/24 --note "partner's servers"   # rm ID|IP
forkrail firewall mitigations my-api          # attack mode and bypasses; --install for the pause
```

Signed in, the same goes through `POST /api/v1/firewall` (`{"op": "change", "project": "my-api", "changes": [{"action": "rules.insert", "value": {...}}]}`, Vercel's PATCH actions). Looking and testing are anyone's in the team; a project's firewall is a member's or an owner's to change; the install's, an owner's. The access log records what the firewall did with each request (`firewall`: deny, challenge, rate_limit, redirect, bypass, log, or held; `firewall_rule`: the rule's id, or `attack_mode`) and the client's `ja4`, and requests it answered have outcome `firewall` (an app's rate-limit questions, `rate_limit_api`; challenge answers, `challenge_solved` or `challenge_failed`).

## 9a. Languages, frameworks and versions

A project is found the way Vercel finds it, with no configuration: its framework preset (chosen in the dashboard's Settings → Build, with `forkrail project NAME --framework SLUG`, or by `vercel.json`'s `framework`, which wins), its entry, and the version of its language.

**Python** (FastAPI, Flask, Django, FastHTML, or any ASGI or WSGI app):
- The entry is the first of `app.py`, `index.py`, `server.py`, `main.py`, `wsgi.py`, `asgi.py` at the top, then in `src/`, `app/` and `api/`, that defines `app`, `application` or `handler` at its top level; or `[tool.vercel] entrypoint = "module:app"` in `pyproject.toml`. Django's comes from `manage.py`'s settings module (`ASGI_APPLICATION`, else `WSGI_APPLICATION`).
- WSGI apps (Flask, Django) run in 10 threads through a2wsgi under uvicorn; ASGI apps as before.
- Django: when its settings set `STATIC_ROOT`, `collectstatic` runs at build time and the router serves the files at `STATIC_URL`; a manifest storage's `staticfiles.json` goes back into the package. `ALLOWED_HOSTS` must take the deployment's hosts (`.forkrail.com`, your domains).
- Dependencies: `pyproject.toml`'s `[project] dependencies` (pinned by a `uv.lock` beside it), else `Pipfile.lock`, else `Pipfile`, else `requirements.txt`, as Vercel reads them (it installs with uv; forkrail with pip).
- The version: `.python-version` (the nearest to the entry), else `requires-python`, else `Pipfile.lock`'s `python_version`. A project that names none runs 3.13, what every project ran before versions were read (Vercel's default is 3.12). A range tries the default first, then the newest, as Vercel does: `>=3.12` is 3.13, `<3.13` is 3.12, `>=3.14` is 3.14. Lambda runs `python3.12`, `python3.13` and `python3.14`; anything else gets 3.13 and a warning. A redeploy keeps the runtime its build had.

**Node** (Express, Hono, Fastify, Koa, NestJS, Elysia, H3, or a `server.*` file; SvelteKit, Astro, Nuxt, TanStack Start, React Router, Remix and Next.js in section 9):
- The entry is the first of `app`, `index`, `server`, `src/app`, `src/index`, `src/server` (NestJS: `src/main` first) with a `.js`, `.mjs`, `.cjs`, `.ts`, `.mts` or `.cts` extension that imports the framework, else `package.json`'s `main`.
- The project's dependencies are installed with its package manager (pnpm and Yarn into a flat `node_modules`), and its `vercel-build` or `build` script runs. TypeScript is bundled with esbuild (the project's own, else 0.28.2), CommonJS unless `package.json` says `"type": "module"`.
- The entry may listen on `PORT`, or export its app (`export default app`), as Vercel takes both: a launcher serves an exported Express or Koa app with its `listen`, Fastify with `listen({ port })`, and an app with `fetch` (Hono, Elysia, H3 2) through Web requests and responses.
- The version: `engines.node`, the newest major its range takes, as on Vercel. Lambda runs `nodejs24.x` (the default) and `nodejs22.x`; Node 26, Vercel's default since October 2026, is still a preview on Lambda, and a range that takes neither 22 nor 24 fails the build.

**Functions in `api/`** (Vercel's zero-config functions, any of the languages below): every file in `api/` is a function, served at its path without the extension, as on Vercel:
- `api/hello.ts` at `/api/hello` (and `/api/hello.ts`), an `index` file at its folder (`/api`, `/api/`), `api/users/[id].ts` at `/api/users/<id>` with `id` in the query. `api/posts/[...slug].ts` is a dynamic segment named `...slug`, which matches one segment, as Vercel's zero-config routes treat it outside Next.js. Any other path under `/api` is a 404.
- Node: `.js`, `.mjs`, `.ts` and `.tsx` files (`.cjs`, `.mts` and `.cts` are modules the functions import). Python: `.py` files that define `app` or `application` (ASGI or WSGI) or a `handler` class (a `BaseHTTPRequestHandler`) at their top level; others are modules. Go and Rust: below. Files and folders starting with `_` or `.` aren't functions.
- Node handlers take both of Vercel's shapes: `export default function handler(req, res)` (or `module.exports`), with `req.query`, `req.cookies`, `req.body` (by content type), `res.status()`, `res.json()`, `res.send()` and `res.redirect()`; and Web handlers, `export function GET(request)` (a method it doesn't export is 405) or `export default { fetch }`, which return a `Response` and may stream it. A file with `export const config = { runtime: 'edge' }` runs in Node, its default export taking a `Request`. TypeScript is compiled per file with esbuild; the function's package keeps the npm packages it imports and what they depend on, not the whole `node_modules`.
- A Python `handler` class runs in an HTTP server of its own, as on Vercel, and sees the raw path; ASGI apps get their lifespan when first called; WSGI apps run through a2wsgi. One install of the project's requirements serves them all. A Python app found from the project's dependencies (FastAPI, Flask, Django) is that framework's preset instead, as Vercel's import picks it, unless `api/` holds several Python functions or `vercel.json` says `"framework": null`.
- `functions` sets `memory`, `maxDuration` (or `"max"`), `includeFiles` and `excludeFiles` by pattern (`api/*.ts`, `api/**/*.py`); the first pattern that matches a file is the one it gets, and a pattern that matches none fails the build, as on Vercel. A Node file's own `export const config = { maxDuration }` counts when `functions` gives it none. `crons` reach a function through the same routes, so `{ "path": "/api/cron" }` runs `api/cron.ts` with `Authorization: Bearer $CRON_SECRET`.
- Beside them the site is served as on Vercel: a static framework's build (Vite, Astro, …), or the project's own folder (`public/` if it has one). A project's functions share one Lambda function, so they run on one language's runtime: Node and Python functions in one project, or functions beside a server (Express, Nitro), are refused for now.
- A project forkrail always ran as one function stays one: `builds`, `functions` naming one `.py` or `.js` file, or `api/main.py` or `server.mjs`, when that file is the only function of its language in `api/`.

**Go and Rust**, on Lambda's `provided.al2023` with forkrail's runtime as the `bootstrap`:
- **Vercel's layout:** each file in `api/` is a function at `/api/<name>` (an `index` file at its folder, `api/users/[id].go` at `/api/users/<id>` with `id` in the query; files and folders starting with `_` aren't functions). Go: each file has one exported `func(http.ResponseWriter, *http.Request)`; they're built into one program with a generated dispatcher, each file's handler renamed after it as Vercel's builder renames it. Rust: each file is a `[[bin]]` in `Cargo.toml`, written with Vercel's `vercel_runtime` crate, unchanged: the runtime runs each as its own program (the crate serves HTTP on `VERCEL_DEV_PORT`). They share one Lambda function, so `functions`' memory and maxDuration patterns give it the largest of each. Beside them the site is served as on Vercel: a framework's build, or the project's own folder (`public/` if it has one).
- **A web server:** a Go `main` package (`main.go`, `cmd/api/main.go` or `cmd/server/main.go`; Vercel's Go preset) or a Rust binary (axum, actix, …) that listens on `PORT`.
- Go builds with `CGO_ENABLED=0` and `-ldflags=-s -w` (or `GO_BUILD_FLAGS`), `-mod=vendor` when there's a `vendor/`; `go.mod`'s `toolchain` or `go` line fetches that Go. Rust builds with `cargo zigbuild --release` for Amazon Linux 2023's glibc (2.34). Their module, registry and target folders are kept in the build cache.
- A program has no shim: a line it prints is kept with its request when only one is in flight; its outgoing calls aren't logged yet.

## 10. Signing in, and your team

People sign in to the dashboard with GitHub, through a GitHub App that belongs to your install alone. Nobody gets an invitation or a password: an owner adds a GitHub username, and that person opens the dashboard and chooses **Continue with GitHub**.

**Once per install**, whoever holds the AWS account creates the app:

```bash
forkrail dashboard https://forkrail-web.example.com     # where the dashboard is served, if not set yet
forkrail git setup                                      # opens GitHub with the app filled in: click Create
```

Your browser opens GitHub's "Create GitHub App" page with everything filled in. Click **Create GitHub App**, and the command stores the app's keys in your account (SSM, encrypted with forkrail's own key) and makes you the install's first owner. Options:
- `--org NAME` creates the app under an organization instead of your own account; then `--owner YOUR_USERNAME` says who signs in first.
- `--callback https://app.example.com` (repeatable) lists another address the dashboard is, or will be, served at. GitHub sends people back only to addresses the app lists, and has no API to add one later: after a move, add it on the app's GitHub page by hand.
- `forkrail git status` shows the app, where its keys are, and the owners.

**Moving the dashboard to an address of its own** (say `dashboard.example.com` instead of `forkrail-web.example.com`):
1. Add the domain to the dashboard's project (Domains, or `forkrail domains add`). A name one label under the install's domain is served with its wildcard certificate at once.
2. On the app's GitHub page, add `https://dashboard.example.com/auth/github/callback` under Callback URL (and, if you like, the webhook URL `https://dashboard.example.com/api/github/webhook`; the old one keeps working).
3. Make it the app's first callback in the install's record (SSM `/forkrail/github/app`: `callback`, and first in `callbacks`; keep the old ones). From then on the dashboard's pages on its built-in `forkrail-web.<domain>` address move there (308); signing in, the API, webhooks, builds' logs and protection answer on both.
4. `forkrail dashboard https://dashboard.example.com` (links in pull requests, notifications and deploy hooks), `forkrail protection setup --login https://dashboard.example.com` (protected deployments' sign-in, same key), and `forkrail login https://dashboard.example.com` on each terminal.

The app is public on GitHub, because GitHub lets only an app's owner sign in through a private one. Anyone can therefore try to sign in; the dashboard lets in only the people listed below. It asks GitHub for nothing about a person beyond who they are, and keeps no GitHub token. It also asks, once, for what deploys from GitHub need (read a repository's code, write commit statuses, deployments and pull request comments), which applies only to repositories you later install it on (section 11).

**Roles.** In the dashboard, Settings → Members:

| Role | Can |
|---|---|
| Owner | everything a member can, and add or remove members, change roles, rename the team, see Infrastructure |
| Member | deploy, promote, roll back, redeploy, cancel, change settings and env vars, connect repositories, make projects |
| Developer | deploy previews (from a terminal or by pushing), redeploy and cancel them, change previews' env vars; nothing that touches production |
| Viewer | see everything (deployments, logs, usage, env var names); change nothing |

A developer reaches production one way: a commit merged to a project's production branch deploys there (section 11). Who may merge is GitHub's to say, with branch protection on that branch. Promote, rollback, `deploy --prod`, a redeploy of production, production's env vars, a project's settings and its repository are refused to them, in the dashboard and from their terminal alike.

Type a GitHub username and the page shows whose account it is before you add it. A removed member is signed out everywhere within half a minute. People are remembered by GitHub's numeric id, so a renamed account stays the same person and a freed username gives nobody access.

**The install's owners** are kept outside the dashboard, and only someone with the AWS account can change them. They are always owners, whatever the members list says, so this is also the way back in if you lock yourself out:

```bash
forkrail owners ls
forkrail owners add ada-lovelace     # can sign in as an owner within a minute
forkrail owners rm ada-lovelace      # the last one can't be removed
```

**Where it's kept.** People, the team, members, sessions and terminals' logins are in the DynamoDB table `forkrail-web`, made with the rest of the install's stacks:

```bash
deploy/aws/stack.sh apply web AdminPrincipals=arn:aws:iam::<account>:root
```

Only the dashboard's role and the administrators you name can read or write it: its resource policy refuses everyone else, whatever their own permissions say (your apps, the routers, the builder, the control function). A session is a random token in a cookie scripts can't read; the table keeps only its hash, so a copy of it signs nobody in. A browser stays signed in for 30 days after its last use, and Account (in the user menu) lists yours and signs the others out. Every change to the team (sign-ins, refusals, members added, roles, removals, terminals) is kept in the table's audit trail.

The table has its own key, can't be deleted while deletion protection is on, is kept if its stack is deleted, and can be put back as it was at any second of the last 35 days (point-in-time recovery, in the DynamoDB console or `aws dynamodb restore-table-to-point-in-time`). An alarm, `forkrail-web-errors`, mails you if DynamoDB fails on its side.

Signing in depends on nothing else of the install's: not the analytics box (logs and metrics), not the routers' network. Dashboards from before the table kept people in Postgres on the analytics box; `web/scripts/copy-accounts.ts` copies them over.

### New projects and page previews

**New project** in the dashboard makes a project three ways: import a GitHub repository (section 11); give it a name only, and deploy it from a terminal with `forkrail deploy --project NAME`; or start from a template (FastAPI, or a plain Node.js server), whose files are built and deployed to production at once. The templates' files are in the repository's `templates/` folder.

A project's overview, and each deployment's page, show what the deployment answers at its address: production at the project's own `<project>.<base>/`, any other deployment at its own host. A page (HTML) is shown as a screenshot; anything else (an API's JSON) as the answer itself. It's looked at once, the first time someone opens that page, and kept in the install's bucket under `previews/` (about 20 KB a deployment), so later visits send no request. A protected deployment is opened with a five-minute token of the dashboard's own; one only trusted addresses may open shows what it answers others. Screenshots come from a function of their own, headless Chromium with no rights at all, which only the dashboard may call; set it up once:

```bash
deploy/aws/screenshot.sh release      # the function forkrail-screenshot (deploy/install/screenshot.yaml)
deploy/aws/dashboard.sh role          # lets the dashboard keep previews under previews/
```

Without it, pages show why there's no screenshot, and an API's answer still shows.

### Deploying without AWS credentials

A member needs no AWS account of their own, only the CLI and the dashboard's address:

```bash
forkrail login https://forkrail-web.example.com
```

It prints an address and a short code and opens the address in the browser. The page there, behind the same GitHub sign-in, shows which machine is asking and the code: check it against the terminal and choose **Yes, sign it in**. On a machine with no browser (over SSH), open the address anywhere; `--no-browser` only prints it.

From then on that terminal acts as that person, with their role:

```bash
forkrail deploy                  # the folder is uploaded and built in the install's builder; the log shows here
forkrail deploy --prod
forkrail ls [PROJECT]
forkrail inspect ID [--logs]
forkrail logs PROJECT [--app] [--follow] [--request ID]   # as in section 7
forkrail promote ID
forkrail rollback [PROJECT] [--to ID]
forkrail redeploy ID [--prod] [--no-cache]
forkrail cancel ID
forkrail whoami
forkrail logout
```

- A viewer's terminal can list and read; changes are refused, as in the dashboard. A developer's also deploys, redeploys and cancels previews; `deploy --prod`, `promote` and `rollback` are refused.
- The source goes straight to S3, to an address signed for that one upload: S3 refuses any other file there. It never passes through the dashboard.
- The login is a token kept in `~/.config/forkrail/auth.json`, readable only by its owner; the dashboard keeps its hash. It lasts 90 days after its last use. Account (in the dashboard's user menu) lists your terminals and signs any of them out; so does `forkrail logout`. Removing a member ends their terminals' logins too.
- For a machine nobody sits at, set `FORKRAIL_TOKEN` and `FORKRAIL_API` instead of a file.
- Env vars (`env ls|add|rm|pull|run`) and a project's function settings (`forkrail project NAME --memory|--max-duration|--concurrent`) go through a login too, under the same roles: a developer changes previews' and Development's variables, not production's or the settings. A project's IAM role and egress targets (`--role`, `--egress`) stay the administrator's, with AWS credentials.
- Not through a login yet: `forkrail stats` and `usage`, `limits`, `env migrate`, building on your own machine (`--local`, `--prebuilt`). Those need the install's AWS credentials (or the dashboard).

Whoever holds the AWS account can sign in too. Their commands then go through the dashboard like everyone's; `--aws` (or `--profile`, or `FORKRAIL_AWS=1`) uses the AWS credentials directly, which scripts that build locally need.

## 11. Deploying from GitHub

Connect a project to a GitHub repository and every push deploys it, as on Vercel: a push to the production branch goes to production, a push to any other branch makes a preview with its own URL. Deploys from the CLI keep working beside it.

**Give the app the repository.** The GitHub App from the section above signs people in; to read code it must also be installed on the account that owns the repository. In the dashboard, open the project's **Settings → Git**: the first time, **Install on GitHub** takes you to GitHub, where you pick the account and either all its repositories or only the ones you choose, and brings you back. (`forkrail git status` shows which accounts it's installed on.)

**Connect.** Back on Settings → Git the repositories the app can read are listed, the most recently pushed first, with a search box. **Connect** one and the card shows it from then on, with:
- the **production branch**: the repository's default branch unless you change it;
- the **root directory** (its own card): the folder that holds the project, for a repository with several;
- **Disconnect**, which stops deploying from it and leaves every deployment as it is.

A repository can be connected to more than one project (each with its own root directory), and a push deploys them all. The same from a terminal, with AWS credentials:

```bash
forkrail git repos                                   # what the app can read
forkrail git connect shop acme/shop                  # pushes to its default branch go to production
forkrail git connect shop acme/mono --branch release --root apps/shop
forkrail git connect shop acme/shop --create         # make the project shop from the repository, and deploy it
forkrail git deploy shop                             # deploy the production branch's newest commit now
forkrail git disconnect shop
```

**A new project from a repository.** A project needn't exist first. On the dashboard's **New project** page, **Import** one of the listed repositories, then give the project its name (it can't be changed later), its production branch, its root directory, and any env vars its build and its app need (they're set for production and previews alike; paste a `.env` file into a key to fill them in). **Deploy** makes the project, connected from the start, and builds its production branch at once: you land on that build's log. If the build can't be started the project is there all the same, and its next push deploys; if its env vars couldn't be saved, nothing is deployed until you add them.

**What a push does.** GitHub tells the dashboard (a signed webhook; anything unsigned is refused), which hands the push to forkrail's control function. That function fetches the pushed commit's files from GitHub, and they're built in your account's builder exactly like `forkrail deploy`: the build log is live in the dashboard, the project's env vars for the target apply, and the build cache is used. On GitHub the commit shows a status, `forkrail/<project>`: pending while it builds, then a link to the deployment, or to its build log if it failed. GitHub is also told of the deployment itself, under an environment named `Production – <project>` or `Preview – <project>`: that is what puts **View deployment** on the commit's pull request once it's ready.

- Two pushes to the production branch close together both build; if the older one finishes last it keeps its own URL and production stays on the newer one.
- A deployment made by a push can be redeployed, promoted and rolled back like any other.
- The app's private key, which is what can read your code, is readable only by the control function's role and whoever administers the AWS account: not by the dashboard, the routers, the builder or your apps. A build gets the commit's files and never a GitHub token.
- A repository's files at one commit may be up to 256 MB zipped. Submodules and Git LFS files aren't fetched.

**A branch's own address.** Besides each deployment's own URL, a branch's previews share one that follows the branch: `<project>-git-<branch>.<base domain>`, the branch in lowercase with anything but letters and digits as dashes (`feature/Logs` is `shop-git-feature-logs.example.dev`; a name too long for a host is cut and given a few digits of its hash). It points at the branch's newest preview that is ready: a build that fails, is canceled or is skipped leaves it where it was, and so does an older build that finishes after a newer one. A redeploy of the deployment that holds it takes it over. The deployment's page lists it under Domains.

**Pull requests.** Every open pull request of the branch carries one comment per connected project, kept up to date: building, ready with the branch's address, failed with the step and a link to the build log, canceled or skipped. A pull request opened after the push gets the comment then. Pull requests from forks get none: their commits aren't built.

**When a branch is deleted** (a merged pull request's, usually) its previews are removed, with their Lambda versions, and the branch's address stops answering. What stays: anything that is or has been production (a preview you promoted is a rollback target), and deployments made from a terminal while that branch was checked out.

**Pushes that don't deploy.** Two keys of `vercel.json`, read from the pushed commit, as on Vercel:
- `git.deploymentEnabled`: `false` turns pushes off altogether; an object turns them off per branch or pattern (`*` within a path segment, `**` across them), and a branch deploys unless a rule it matches says `false` and none says `true`. Such a push makes no deployment and sets nothing on GitHub. (`github.enabled: false`, the older spelling, does the same.)
  ```json
  { "git": { "deploymentEnabled": { "dependabot/**": false, "wip-*": false } } }
  ```
- `ignoreCommand`: a command run at the start of a push's build, in the project's folder, with the build's env vars. If it exits 0 the build is skipped: the deployment is listed as canceled, with the reason and the command's output in its log, the commit's status is a success (so a required check doesn't hold a merge up) and the branch's address stays where it was. Any other exit builds. Only pushes ask it: a deploy or a redeploy you start yourself is always built.
  ```json
  { "ignoreCommand": "[ \"$VERCEL_GIT_COMMIT_REF\" = docs ]" }
  ```
  A build has the commit's files, not the repository's history, so `git diff HEAD^ HEAD` can't be used here yet (it fails, and the build goes on). `VERCEL_GIT_COMMIT_REF`, `VERCEL_GIT_COMMIT_SHA`, `VERCEL_GIT_PREVIOUS_SHA` (the commit the branch was at before the push) and `VERCEL_ENV` are set.

**Deploy hooks.** A secret URL that builds a branch's newest commit when it's called with GET or POST and nothing else, for a CMS that publishes, a cron service or a script, as on Vercel. Make one on Settings → Git → Deploy Hooks (a name and a branch; members and owners), or:

```bash
forkrail deploy-hooks create shop "Sanity publish" --branch main   # prints the URL, the only time it's shown
forkrail deploy-hooks ls shop
forkrail deploy-hooks rm shop 3f9a0c1b2d4e
curl -X POST "https://<dashboard>/api/deploy-hooks/shop/<secret>"            # {"job": {"id": "<deployment>", "state": "PENDING", ...}}
curl -X POST "https://<dashboard>/api/deploy-hooks/shop/<secret>?buildCache=false"
```

The dashboard answers the URL, so the install needs its address (`forkrail dashboard URL`). Its deployment is built like a push to that branch (production for the production branch, else a preview that takes the branch's address), is listed as made by the hook, and GitHub hears how it ends; the project's `ignoreCommand` isn't asked (it's a build you asked for), and pushes turned off by `git.deploymentEnabled` turn the hook off too. Calling the same hook again for the same commit cancels the build it started before. A project may have 5 hooks, called 60 times an hour in all (then 429 with `retry-after`); a wrong secret is a 404 and isn't counted. Only the secret's SHA-256 is kept: lose the URL and make a new hook. The routers log a hook's path with the secret as `REDACTED`, so the dashboard's own request logs don't show it. Anyone with the URL can deploy the branch, so treat it as a password and remove a hook you think leaked.

**Stored files.** A deployment's package, its kept source and its upload are deleted with the last deployment that uses them (deployments of the same content share them), in every region. For what earlier versions left behind:

```bash
forkrail sweep                 # lists the files no deployment uses, by project and region
forkrail sweep --yes           # deletes them
forkrail sweep shop --yes      # one project's
```

Lambda keeps its own copy of every published version's code, so nothing that runs reads these files; files under an hour old are never counted, in case a deploy is under way.

A sweep of the whole install also covers `static/` in every region's bucket: static files, their manifests, routing programs, bulk redirect tables and firewall configurations, all named by content and shared between projects. One goes only when nothing names it: no kept deployment, no entry of the routing table, no kept version of a project's redirects, rules or firewall (an old version can be published again). `static/` is left alone while a deployment is being made, since a deploy doesn't upload files the bucket has already, and when anything that might name a file can't be read.

**Deleting a project** (Settings → Advanced in the dashboard, or `forkrail rm --project NAME`, which asks for the name again unless `--yes`) takes out everything of it: it stops being served at once, then its schedules, env var values, functions in every region, deployments (records, build logs, the deleted ones still restorable too) and files (packages, sources, uploads, build cache, page previews, versions of its redirects, rules and firewall) go, and its custom domains with them. A build under way is canceled first. The request logs it already wrote are kept, as everyone's are. Every step can run again, so a delete that stops halfway is finished by deleting again. The dashboard's own project can't be deleted this way. One deployment that isn't production goes with `forkrail rm ID` or its ⋯ menu → Delete (a developer may delete previews), restorably: section 8, "How long deployments are kept".

## 12. What's refused

A request's headers are refused as Vercel refuses them, before any function runs: one header field over 32 KiB (name and value) gets `494 REQUEST_HEADER_TOO_LARGE`; more than 500 fields, or a request line and fields over about 127.5 KB in all, get `431`. Anything smaller reaches the app whole (Node's and uvicorn's own 16 KiB limits are raised to match).

The CLI reads `vercel.json` and refuses what it can't yet do the way Vercel does, naming the rule, rather than deploying something that behaves differently:
- `builds` with more than one entry; functions in `api/` in more than one language, or beside a server (Express, Nitro, …) (section 9a)
- `skipMiddlewareRequestBody`, `functionFailoverRegions`, `bunVersion`, and `public` (which Vercel dropped too); `schedules` targets other than `path`
- what a framework's Build Output can't do here yet, and the frameworks not supported yet (section 9, "Not supported yet, and where forkrail differs")
- in routes: `env` (in a route or a transform), `locale`, `middlewarePath`, and `handle: rewrite` or `resource`
- function settings other than `memory`, `maxDuration`, `includeFiles`, `excludeFiles`, `architecture: "arm64"` and a `runtime` naming the language's own
- keys that don't fit the project: `fluid` on a static site, `outputDirectory` on an API

`fluid` is understood, as concurrent compute on or off (see "APIs that need their own IP per request"), and so are `proxy` (see "Routing Middleware"), `images` (see "Image Optimization"), `git.deploymentEnabled` and `ignoreCommand` (see "Pushes that don't deploy") and the routing keys (see "Redirects, rewrites and headers"). `github.autoJobCancelation` (default true: a branch's newer push cancels its older builds still running) and `github.autoAlias` (false: the production branch's pushes build production deployments that leave production where it is) work as on Vercel. `version`, `regions`, `name`, the other `github` keys, `devCommand` and `builds[].config.maxLambdaSize` are ignored, with a warning where it matters.

Project names are lowercase letters, digits and dashes. `www`, `api`, `app`, `docs`, `dashboard`, `admin`, `status` and `mail` are kept for the site. Names ending in a dash and 8 hex digits are refused, since `<name>.forkrail.com` would look like another project's deployment, and so are names with `-git-` in them, which would look like another project's branch.

## 13. Moving from Vercel

Most code written for Vercel runs unchanged. forkrail gives apps Vercel's system variables (section 2) and its request context (`waitUntil` from `@vercel/functions` keeps the worker open until the work settles; `ipAddress`, `geolocation` and `getEnv` read what the router sends), answers the paths pages ask the platform for (`/_vercel/insights/*` itself, so `@vercel/analytics` reports to forkrail's Web Analytics once it's on; `/_vercel/speed-insights/*` with an empty script and 204s, so `@vercel/speed-insights` loads without errors and reports nothing; `/_vercel/image`), and takes the platform's own `x-vercel-*` headers off what visitors send (`x-vercel-forwarded-for` is always the visitor's address). Packages that reach a service with a key of their own work once their env vars come along: `@vercel/kv` and `@vercel/postgres` (Upstash and Neon), `@vercel/edge-config` (`EDGE_CONFIG`; each read is a request to Vercel, which bills it), `flags`, `@vercel/og`, `@vercel/firewall` (the router answers its rate-limit checks) and `@vercel/otel` (traces go where `OTEL_EXPORTER_OTLP_*` says).

**Vercel's own services are different.** They sign in with an OIDC token Vercel makes for each of its functions, which only Vercel can make, so on forkrail they build and deploy fine and then fail at their first call. A build looks for them in `package.json` (and the lockfile, for the version installed) and in the names of the project's env vars for the target being deployed (never their values), and says what to do as a warning in the build log (`warning: …`, shown in the deployment's log in the dashboard). A static site, or a Python, Go or Rust app (whose `package.json` is only for its front end), gets none: no Node code of its own runs on the server.

| Package | What breaks on forkrail | Fix |
|---|---|---|
| `ai` 5 and later, `@ai-sdk/gateway` (AI Gateway) | A model named by a string (`'openai/gpt-5'`) or by `gateway(…)` signs in with `AI_GATEWAY_API_KEY`, else the OIDC token: every such call fails with `GatewayAuthenticationError`. A project with a provider package of its own (`@ai-sdk/openai` and the like) is taken to call that one and isn't warned. | Add `AI_GATEWAY_API_KEY` (an AI Gateway API key from Vercel's dashboard; Vercel still bills the calls), or call a provider directly, such as `@ai-sdk/amazon-bedrock` with a project role (section 9) |
| `@vercel/blob`, `@vercel/kv2` (Blob underneath) | A store connected through OIDC gives the project `BLOB_STORE_ID` and no token: every call fails with "No blob credentials found" | Create a read-write token for the store in Vercel's dashboard and add it as `BLOB_READ_WRITE_TOKEN` (the files stay in Vercel Blob, billed by Vercel), or move the files to S3 |
| `workflow` 4 and later, `@workflow/*` (Workflow SDK) | It picks its backend as the app starts: `WORKFLOW_TARGET_WORLD`, else Vercel's whenever `VERCEL_DEPLOYMENT_ID` is set, **which forkrail sets, as Vercel does**. Vercel's backend needs the OIDC token, so every workflow fails to start | `WORKFLOW_TARGET_WORLD=@workflow/world-postgres` with `WORKFLOW_POSTGRES_URL` (or `DATABASE_URL`), and `@workflow/world-postgres` in the dependencies. Its worker runs inside the app, and an app on forkrail runs only while it has a worker open, so queued steps and `sleep`s go on while requests keep one open, not on their own |
| `@vercel/sandbox` | Without `token`, `teamId` and `projectId` passed in, it asks for the OIDC token | Add `VERCEL_TOKEN` (a Vercel access token), `VERCEL_TEAM_ID` and `VERCEL_PROJECT_ID` and pass them to `Sandbox.create` (forkrail's own `VERCEL_PROJECT_ID` is the project's name; a project's own value wins) |
| `@vercel/flags-core`, `@flags-sdk/vercel` (Vercel Flags) | Reads flags with the `FLAGS` SDK key, else the OIDC token | Add `FLAGS` (the SDK key, `vf_server_…`) |
| `@vercel/queue` (Vercel Queues) | Sends with the OIDC token; only Vercel delivers to the consumers `vercel.json`'s queue triggers name | A queue in your AWS account, such as SQS, with a project role |
| `@vercel/schedules` | Signs in with the OIDC token | `crons` in `vercel.json` (section 4, Schedules) |
| `@vercel/connect`, `@vercel/integrations` | Sign in with the OIDC token unless the code passes `vercelToken` | The services' own keys or OAuth apps, as env vars |
| `@vercel/kms` | Signs in with the OIDC token | AWS KMS with a project role |
| `@vercel/passport` | Reads the visitor from a header Vercel's proxy adds after its own sign-in: every visitor reads as signed out | Sign-in in the app, or deployment protection (`forkrail protection`) for a team-only site |
| `@vercel/oidc-aws-credentials-provider`, `@vercel/oidc` | No token to trade for AWS credentials, or to read | A project role (`forkrail project NAME --role ROLE`, section 9): the AWS SDK picks it up with no keys |
| `upgradeWebSocket()` (`@vercel/functions`) | Throws: forkrail's functions don't take WebSocket connections. No warning, since the rest of the package works | A WebSocket service of its own |

Never copy `VERCEL_OIDC_TOKEN` from `vercel env pull` into forkrail: it's a development token that expires in 12 hours.

Also worth checking when a project moves: `*.vercel.app` URLs written into code or settings (OAuth redirect URIs, CORS lists, `NEXTAUTH_URL`), `@vercel/microfrontends` (the paths another project serves on Vercel 404 here), and integration env vars Vercel added (`KV_REST_API_*`, `POSTGRES_URL*`, `EDGE_CONFIG`, `UPSTASH_*`, `DATABASE_URL`): bring them along with the project's own.

## How it fits together

- `*.forkrail.com` is a DNS-only record in Cloudflare (no Cloudflare proxy) pointing at Global Accelerator's two static IPs, which send each client to the nearest healthy router.
- The router, on one EC2 instance in us-east-1, terminates TLS with a Let's Encrypt wildcard certificate it issues and renews itself. It maps the hostname to a deployment and sends the request to a Lambda worker over a tunnel the worker opened.
- A router in another region (us-west-1) reads the same routing table and certificates, so it serves every host. Each deployment is published there too, and that router runs its own region's version; one that isn't there yet runs from us-east-1. Global Accelerator (`test-env.sh accelerator`) puts both behind two static IPs, sends each client to the nearest healthy router, and moves clients to the other region about 47 s after a router stops answering.
- Plain `http://` gets a 308 redirect to `https://`, except Let's Encrypt's HTTP challenges for custom domains, which every router answers on port 80.
- Router restarts (`test-env.sh push`, `router-env`) have no gap: the new router starts beside the old one and takes over once it's ready. Hosts that aren't deployments get 404 `DEPLOYMENT_NOT_FOUND`.
- Schedules: each project's production schedules are EventBridge Scheduler schedules (group `forkrail-<project>`) that send a small message per run to the SQS queue `forkrail-schedules` (`deploy/install/schedules.yaml`). Both routers long-poll it (`FORKRAIL_SCHEDULE_QUEUE`); whichever takes a message makes the request to the project's production deployment through its own proxy, so the run is routed, limited and logged like a visitor's request. The worker adds `CRON_SECRET` from its own environment. No Lambda of its own, no egress.
- A custom domain is a CNAME to `<project>.forkrail.com` (or A records at the accelerator's addresses for a registered domain itself), with its own certificate on the router, issued through its Cloudflare zone, a delegated `_acme-challenge` CNAME, or HTTP.
- A website's static files never reach Lambda: the router reads them from S3 in its own region (free in-region) and keeps the busiest in memory.
- Project files and deployment records are in the `forkrail-state` DynamoDB table once the install has moved them (section 8); the routing table, settings and certificates the routers read stay in S3.
- Limits per deployment: no cap on workers at once unless `forkrail limits` sets one (each takes up to 10 requests, or one with concurrent compute off), and the `maxDuration` in `vercel.json` (default 300 s), after which a request gets 504 `FUNCTION_INVOCATION_TIMEOUT`.
