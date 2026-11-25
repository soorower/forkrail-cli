# forkrail

Install the forkrail CLI on macOS or Linux:

```bash
curl -fsSL https://forkrail.com/install | sh
```

On Windows, in PowerShell:

```powershell
irm https://forkrail.com/install.ps1 | iex
```

Then set forkrail up in your own AWS account:

```bash
forkrail install
```

On a Mac you can use the app instead, with the same installer in a window of its own: [forkrail.com/download/mac](https://forkrail.com/download/mac).

You need an AWS account, a domain whose DNS is on Cloudflare (the free plan is enough), and a GitHub account. The installer asks for everything first, checks the account before it changes anything, and then sets everything up in about 15 minutes.

Each release holds the CLI for macOS, Linux and Windows, the Mac app, and the files `forkrail install` copies into your account, each with a `.sha256` beside it. The Windows CLI isn't code-signed yet: run from a `.zip` you downloaded in a browser, Windows SmartScreen asks once before it starts (More info, then Run anyway). The PowerShell line above installs it without that question.

## Licence

forkrail is open source under the Apache License 2.0 ([LICENSE](LICENSE)). [NOTICE](NOTICE) and [THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt) list the open-source code it's built with and their licences. The forkrail name and logo aren't covered by the licence.
