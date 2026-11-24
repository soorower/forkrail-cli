# forkrail

Install the forkrail CLI:

```bash
curl -fsSL https://github.com/soorower/forkrail-cli/releases/latest/download/install.sh | sh
```

Then set forkrail up in your own AWS account:

```bash
forkrail install
```

You need an AWS account, a domain whose DNS is on Cloudflare (the free plan is enough), and a GitHub account. The installer asks for everything first, checks the account before it changes anything, and then sets everything up in about 25 minutes.

Each release holds the CLI for macOS and Linux and the files `forkrail install` copies into your account, each with a `.sha256` beside it.
