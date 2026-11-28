# Installing forkrail

forkrail runs in your own AWS account. One command sets it up: `forkrail install`. It checks
the account first and changes nothing until the checks pass, then makes everything in order. If
a step fails, it says what went wrong and how to fix it, and running it again continues from that
step. An account holds one install: run it again on an account that has one and it finishes what's
missing, or says there's nothing to do.

## Get the CLI

```bash
curl -fsSL https://forkrail.com/install | sh
```

`forkrail.com/install` redirects to `install.sh` in the newest release on GitHub
(`https://github.com/soorower/forkrail-cli/releases/latest/download/install.sh`, which works as
well). It puts `forkrail` in `~/.local/bin` (or `FORKRAIL_BIN`), for macOS (Apple silicon or Intel) or
Linux (x86_64 or arm64), after checking the download against its SHA-256. Nothing else is
installed. The macOS programs are signed with a Developer ID and notarized by Apple, so they open
however they were downloaded.

**On a Mac, without Terminal**: download Forkrail.dmg from
[forkrail.com/download/mac](https://forkrail.com/download/mac) (a redirect to the newest release's
`Forkrail.dmg` on GitHub), open it, drag Forkrail to Applications and open it. The app is the same installer, `forkrail install`,
with its page in a window of its own instead of the browser (deploy/macos). AWS's and GitHub's
sign-in pages still open in your browser, where passkeys work. Its output is kept in
`~/Library/Logs/Forkrail/install.log`. Closing it while it runs stops the install; opening it
again carries on.

## What you need

- **An AWS account.** The installer signs you in itself: its page has a **Sign in to AWS** button
  that opens AWS's sign-in page in your browser, or on a server it prints an address to open on
  any device and takes the code that page shows. No AWS CLI and no profile to set up. The install
  makes roles, keys and machines, so sign in as an administrator or the account's root user. The
  root user needs an MFA device first (AWS
  answers "403 … You must have a multi-factor authentication (MFA) device configured" otherwise):
  add one under [Security credentials](https://console.aws.amazon.com/iam/home#/security_credentials)
  → Assign MFA device; a passkey takes a minute. A computer that's already signed in (`aws login`,
  `aws sso login`, an access key, AWS CloudShell) is used as it is.
- **A domain whose DNS is on Cloudflare** (the free plan is enough). Your apps get
  `<name>.yourdomain.com`, and the dashboard `dashboard.yourdomain.com`. The installer adds one
  record, `*.yourdomain.com`, and never touches records it didn't make. The domain itself and any
  names already in use stay as they are.

  The installer asks first where the domain's DNS is. If it's somewhere else (GoDaddy, Namecheap,
  Route 53, …), it shows how to move it and stops before changing anything. The domain stays with
  the company it was bought from; only its DNS moves:
  1. [Add the domain to Cloudflare](https://dash.cloudflare.com/?to=/:account/add-site) on the free plan.
  2. Where the domain was bought, replace its nameservers with the two Cloudflare shows.
  3. When Cloudflare says the domain is active (usually within an hour), run `forkrail install` again.
- **A Cloudflare API token** for that domain: DNS edit and zone read.
  [This link](https://dash.cloudflare.com/profile/api-tokens?permissionGroupKeys=%5B%7B%22key%22%3A%22dns%22%2C%22type%22%3A%22edit%22%7D%2C%7B%22key%22%3A%22zone%22%2C%22type%22%3A%22read%22%7D%5D&accountId=%2A&zoneId=all&name=forkrail)
  (the page's **Create the token on Cloudflare** button) opens Cloudflare with everything filled in:
  click **Continue to summary**, then **Create Token**, and paste the token. As filled in it works
  for all your domains; to keep it to one, pick that domain under Zone Resources before you continue.
- **A GitHub account.** Right after the questions and checks, the page's button opens GitHub with the
  install's GitHub App filled in, and you click **Create GitHub App**. You sign in to the dashboard with it.
- An email address for alerts (optional): the monthly budget, and certificate problems.

Nothing else: no Docker, no Rust, no AWS CLI on the machine that installs.

## Run it

```bash
forkrail install
```

Everything you do happens in the first few minutes, in this order:

1. **Sign in to AWS**, if this computer isn't signed in yet. Click **Sign in to AWS** on the page:
   AWS's sign-in page opens in your browser, only when you click. Sign in with the account
   forkrail goes in, and the tab says you can close it.
2. **Answer the questions**: where the domain's DNS is, the domain, the region (picked from a
   list, US East (N. Virginia) unless you pick another), an alert email, the Cloudflare token.
3. The installer **checks** the account and the token. Nothing is changed until they pass.
4. **One click on GitHub**: click **Create the GitHub App** on the page, and GitHub opens with
   forkrail's app filled in. Click **Create GitHub App** there. If GitHub first asks you to confirm
   it's you (a password or passkey) and then shows an empty form, click the page's button again:
   GitHub opens filled in this time.

Then it sets everything up on its own, and shows each step as it runs with a line on what it's
doing. Nothing more is asked. forkrail itself for the CLI's version (about 1 GB: the router, the
functions, the builder's image, the dashboard) goes from its GitHub release straight into the
account: a short-lived function the installer makes there (`forkrail-install-fetch`,
deploy/install/fetch.py) downloads it, checks it against the SHA-256 published beside it, and puts
it in the install's bucket and registry, in a minute or two. Nothing that size goes through your
connection. If the account can't do that, the installer downloads the release itself instead,
keeps it in the cache folder, and uploads it.

On a computer with a browser, a page opens for all of this. On a server over SSH, the same
questions come in the terminal, and the AWS sign-in prints an address to open on your phone or
laptop and asks for the code that page shows. `--terminal` keeps everything in the terminal.

The sign-in is saved the way `aws login` saves it: in your `default` AWS profile, or in
`forkrail-<account>` when `default` already holds other credentials. `forkrail uninstall` and the
AWS CLI use it from there. It renews itself until AWS ends the session; after that, the installer
signs you in again when it needs to.

For a run with no one to answer (a script):

```bash
FORKRAIL_CLOUDFLARE_TOKEN=... forkrail install --yes --domain example.com --region us-east-1 --email you@example.com
```

`forkrail install --check` only runs the checks.

When it's done, open `https://dashboard.<your domain>` and sign in with GitHub. Whoever made the
GitHub App is the install's first owner. (A run with `--yes` makes the app at the end instead, and
waits there for the click.)

## What it makes, and what it costs

| | |
|---|---|
| The router (a t4g.small with a fixed address) | about $0.45 a day |
| The log server, ClickHouse (a t4g.medium, 50 GB) | about $0.90 a day |
| Two fixed addresses, disks, keys, the audit trail | about $0.45 a day |
| Your apps: Lambda, S3, data out | what they use; idle apps cost nothing |

So about $1.80 a day (24 hours) before traffic. The install also gets an AWS budget on everything
it runs ($150 a month by default, AWS budgets are monthly; `FORKRAIL_INSTALL_BUDGET=300` changes
it): at 100% it stops forkrail's functions, so a runaway bill can't happen, and with an alert email
AWS emails you at 80% and 100% too. Without an email it tells the install's alerts topic instead,
and still stops the functions. The budget counts what's tagged `project=forkrail`; in a member
account of an AWS organization, which can't turn that tag on (only the organization's management
account can), it counts everything the account spends.

Inside forkrail, the team budget starts at $20 a month (Settings → Spend Management in the dashboard), growing
as the apps get used until you set your own. At it, apps pause, but the dashboard keeps running so
you can raise it. Running a project's functions in another region adds that region's encryption
key, about $1 a month: picking the region in the dashboard sets it up, and `forkrail uninstall`
removes it.

The installer leaves the account's limits as they are. It doesn't ask AWS for more Lambda
concurrency: a new account's 10 is fine to start, and AWS raises it as the account gets used.

The steps below run side by side wherever they don't need each other: the release downloads while
the account's first steps run, the log server is set up while the release is copied, and the
services' stacks are made together. In order of what needs what, the steps are:

1. **The install's bucket** (`forkrail-artifacts-<account>`): projects, packages, static files, logs.
2. **Encryption keys** for env var values and the install's own secrets.
3. **Roles** for the router, the log server, the dashboard and apps.
4. **Secrets**: the Cloudflare token and the routers' keys, in SSM, encrypted.
5. **forkrail itself**: the router's program, the functions' code, the builder's image.
6. **Services**: the control, build, notification, image and screenshot functions, the tables,
   queues and schedules.
7. **The log server**: ClickHouse, for request logs, function logs and analytics.
8. **The router**, which takes every request, gets the domain's certificate from Let's Encrypt,
   and runs apps on Lambda.
9. **The domain**: `*.yourdomain.com` pointed at the router.
10. **The dashboard**, deployed as a forkrail project and served at `dashboard.yourdomain.com`.
    The release has it built already (`web-built.tar.gz`), so it's published as it is.
11. **Spend protection**: the AWS budget above.
12. **GitHub sign-in**: the GitHub App made at the start, its keys stored in the account.

Everything is tagged `project=forkrail` and made with CloudFormation stacks named `forkrail-*`.

## When something fails

Each AWS call is tried twice. If the second try fails too, the installer stops and prints what it
was doing, what AWS refused in plain words, how to fix it, and AWS's own message (both tries, when
they differ):

```
Builds, deploys and alerts failed while setting up the control service (stack forkrail-control).
  What happened: …
  How to fix it: …
  AWS said: …
Run `forkrail install` again to continue from this step; the steps before it are kept.
```

A stack that fails to create can leave an empty log group behind, which makes the next try fail
with "already exists". The installer deletes such leftovers (empty, and named like its own) before
it tries again.

The usual ones on a new account:

- **Signing in answers 403, "You must have a multi-factor authentication (MFA) device configured"**:
  you signed in as the root user, which AWS lets use a terminal only with MFA. Add an MFA device
  under Security credentials, sign out of the console (a session from before the device doesn't
  count), then run `forkrail install` again and use the device when it asks.

- **"AWS is still verifying this new account"**: new accounts can't start machines for a while.
  Wait for AWS's email, then run it again.
- **Function memory**: a new account may let each Lambda function use only 512 MB at first
  (AWS raises it as the account gets used; it isn't in Service Quotas). This stops nothing: the
  check finds the limit, every forkrail function is made to fit it, and so is every app you deploy
  later (a deployment asking for more gets the most the account allows, and its log says so). The
  dashboard comes already built with the release, so nothing has to build in the account. Until
  the limit rises, builds of big apps can run out of the builder's memory, and page screenshots may
  fail. The dashboard shows the limit under Infrastructure → Capacity → AWS limits and in each
  project's function memory setting, and takes it off once AWS raises it. To get it sooner, open a
  free support case (Account and billing) asking for 10,240 MB in your region.
- **The vCPU limit**: new accounts may start with fewer than the 4 vCPUs the two machines need.
  The check says so before anything is made, with the Service Quotas page to ask for 8.
- **The Cloudflare token** can't edit the zone, or the zone isn't active yet: the check says which.

## Making a release

A release is everything `forkrail install` copies into an account, for one CLI version. To make one
and publish it where the installer and `install.sh` download from (GitHub releases of
`soorower/forkrail-cli`, which cost nothing to host):

```bash
deploy/aws/bundle.sh DIR [BUILDER_IMAGE|keep]   # the release folder
deploy/release/publish.sh DIR --dry-run         # the CLIs and the release file, in DIR.out
deploy/release/publish.sh DIR                   # the same, published as v<version>
```

Publishing on a Mac signs and notarizes the macOS CLIs (`deploy/release/sign-macos.sh`): set
`FORKRAIL_SIGN_IDENTITY` to the Developer ID Application identity and `FORKRAIL_NOTARY_PROFILE` to
a notarytool keychain profile.

The CLI downloads the release of its own version, so raise `version` in `crates/cli/Cargo.toml`
for each new one. The dashboard is in a release already built and without its source, and
`publish.sh` packs only the files `manifest.json` names. Every download carries forkrail's
`LICENSE` (Apache-2.0), `NOTICE` and `THIRD-PARTY-NOTICES.txt`, the licences of the code it's built with
(`deploy/release/notices.py`: the Rust crates, the dashboard's npm packages, and the code adapted
from Vercel's routing-utils, path-to-regexp and JA4). `forkrail install --release DIR` (or
`FORKRAIL_RELEASE=DIR`) installs from a release folder, uploaded from this computer, instead of the
published one. The Mac app passes
its environment on to the installer, so `open --env FORKRAIL_RELEASE=DIR --env AWS_PROFILE=NAME
Forkrail.app` tries an unpublished release with a chosen AWS profile.

## Changing your answers

Run the installer again: the page shows what you answered before, and you can change any of it.
A new email or Cloudflare token is used from then on. A new domain or region for an install that
hasn't finished starts it over: the page says so first, and the installer removes what the earlier
run made before it installs again. A finished install keeps its domain and region.

## Removing it

```bash
forkrail uninstall
```

asks you to type the domain, then deletes everything the installer made: the machines and their
disks and addresses, the apps and their data, the dashboard, the buckets, the tables, the secrets,
the logs, the budget, the DNS records. Apps moved to other regions are cleaned up there too: their
functions, code buckets, env layers and logs, and any stack made there (a router). AWS deletes the
encryption keys 7 days later (keys waiting for deletion cost nothing). The GitHub App stays on GitHub, where
only you can delete it; the command prints its page. An install that was set up by hand, without
the installer, is never touched.
