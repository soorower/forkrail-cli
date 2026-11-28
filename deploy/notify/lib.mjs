// The notifier's own logic, apart from AWS (index.mjs wires that): which webhooks and which people
// an event goes to, what they're told, how a webhook body is signed, which addresses a webhook may
// reach, and when a failed delivery is tried again. `node --test deploy/notify/` runs its tests.

import { createHmac } from 'node:crypto'
import net from 'node:net'

/** Events a webhook can ask for, as the dashboard lists them. */
export const EVENTS = [
  'deployment.created',
  'deployment.succeeded',
  'deployment.error',
  'deployment.canceled',
  'deployment.promoted',
  'deployment.rollback',
  'attack.detected',
  'domain.certificate.renew.failed',
  'budget.reached',
  'spend.paused',
  'spend.fast',
  'budget.raised',
]

/** Notifications people get, by event: their kind, and who gets them. */
export const NOTIFIED = {
  'deployment.error': { type: 'deployment_failed', to: 'creator' },
  'domain.certificate.renew.failed': { type: 'certificate_failed', to: 'owners' },
  'attack.detected': { type: 'attack_detected', to: 'owners' },
  'budget.reached': { type: 'budget_reached', to: 'owners' },
  'spend.paused': { type: 'project_paused', to: 'owners' },
  'spend.fast': { type: 'spend_fast', to: 'owners' },
  'spend.blind': { type: 'spend_blind', to: 'owners' },
  'budget.raised': { type: 'budget_raised', to: 'owners' },
}

/** Events whose ids say what they're about (the routers make them alike, so each is told once):
 * kept as handled for longer than a month, against a router that restarts mid-month. */
export const ONCE_A_MONTH = new Set(['budget.reached', 'spend.paused', 'spend.fast', 'budget.raised'])

/** Nano-dollars as dollars to the cent. */
export function money(n) {
  const d = Number(n ?? 0) / 1e9
  return `$${d.toLocaleString('en-US', { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`
}

/** `202611` as "November". */
function monthName(month) {
  const m = Number(month) % 100
  return m >= 1 && m <= 12 ? new Date(Date.UTC(2000, m - 1, 1)).toLocaleString('en-US', { month: 'long', timeZone: 'UTC' }) : 'this month'
}

/** Each failed delivery is tried again after these (seconds), until a day after the event. */
export const RETRY_AFTER_S = [60, 300, 900, 3600, 7200, 14400, 28800, 43200]
export const GIVE_UP_AFTER_MS = 24 * 3600 * 1000
/** A delivery waits this long for an answer, as the platform this copies. */
export const TIMEOUT_MS = 30_000
/** SQS delays a message at most this long; a longer wait is several. */
export const MAX_DELAY_S = 900

/** The project an event is about, if any (attacks and certificates are the install's). */
export function projectOf(event) {
  const p = event?.payload?.project
  return (typeof p === 'string' ? p : p?.name) ?? null
}

/** Whether `webhook` asks for `event`. */
export function wants(webhook, event) {
  if (!Array.isArray(webhook.events) || !webhook.events.includes(event.type)) return false
  if (webhook.disabled) return false
  const project = projectOf(event)
  if (webhook.projects === 'all' || webhook.projects == null || project == null) return true
  return Array.isArray(webhook.projects) && webhook.projects.includes(project)
}

/** The signature header's value: HMAC-SHA1 of the exact body, in hex. */
export function sign(body, secret) {
  return createHmac('sha1', secret).update(body).digest('hex')
}

/** When a delivery that failed on `attempt` (1 for the first) is due again, or null to give up. */
export function nextTry(attempt, eventCreatedAt, now = Date.now()) {
  const after = RETRY_AFTER_S[attempt - 1]
  if (after == null) return null
  const due = now + after * 1000
  if (due - eventCreatedAt > GIVE_UP_AFTER_MS) return null
  return due
}

/** Addresses a webhook may never reach: private, loopback, link-local, shared, reserved, multicast. */
const blocked = new net.BlockList()
for (const [net4, bits] of [
  ['0.0.0.0', 8], ['10.0.0.0', 8], ['100.64.0.0', 10], ['127.0.0.0', 8], ['169.254.0.0', 16],
  ['172.16.0.0', 12], ['192.0.0.0', 24], ['192.0.2.0', 24], ['192.88.99.0', 24], ['192.168.0.0', 16],
  ['198.18.0.0', 15], ['198.51.100.0', 24], ['203.0.113.0', 24], ['224.0.0.0', 4], ['240.0.0.0', 4],
]) blocked.addSubnet(net4, bits, 'ipv4')
// (Not ::ffff:0:0/96: Node matches IPv4 addresses against IPv6 rules in that form, so it would
// refuse them all. An IPv4 address written as IPv6 is checked as itself instead, below.)
for (const [net6, bits] of [
  ['::', 128], ['::1', 128], ['64:ff9b::', 96], ['64:ff9b:1::', 48], ['100::', 64],
  ['2001::', 23], ['2001:db8::', 32], ['2002::', 16], ['fc00::', 7], ['fe80::', 10], ['ff00::', 8],
]) blocked.addSubnet(net6, bits, 'ipv6')

/** The IPv4 address an IPv4-mapped IPv6 address stands for (`::ffff:a.b.c.d` or `::ffff:xxxx:yyyy`). */
function mappedV4(address) {
  const m = /^(?:0{0,4}:){0,5}:?ffff:(.+)$/i.exec(address)
  if (!m) return null
  if (net.isIPv4(m[1])) return m[1]
  const hex = /^([0-9a-f]{1,4}):([0-9a-f]{1,4})$/i.exec(m[1])
  if (!hex) return null
  const [hi, lo] = [parseInt(hex[1], 16), parseInt(hex[2], 16)]
  return [hi >> 8, hi & 255, lo >> 8, lo & 255].join('.')
}

/** Whether a resolved address is one a webhook may be sent to. */
export function isPublicAddress(address) {
  const family = net.isIP(address)
  if (family === 0) return false
  const v4 = family === 6 ? mappedV4(address) : null
  if (v4) return !blocked.check(v4, 'ipv4')
  return !blocked.check(address, family === 4 ? 'ipv4' : 'ipv6')
}

/** Why a webhook URL can't be used, or null if it can (its host's addresses are checked when it's sent). */
export function urlProblem(text) {
  let url
  try {
    url = new URL(text)
  } catch {
    return 'not a URL'
  }
  if (url.protocol !== 'https:') return 'webhooks are sent over HTTPS only'
  if (url.username || url.password) return 'no user name or password in the URL'
  const host = url.hostname.replace(/^\[|\]$/g, '').toLowerCase()
  if (host === 'localhost' || host.endsWith('.localhost') || host.endsWith('.local') || host.endsWith('.internal')) return 'a public host only'
  if (net.isIP(host) && !isPublicAddress(host)) return 'a public address only'
  return null
}

/** A deployment's words: production or preview. */
function targetWord(event) {
  return event?.payload?.target === 'production' ? 'Production' : 'Preview'
}

function clip(text, n) {
  const s = String(text ?? '').replace(/\s+/g, ' ').trim()
  return s.length > n ? `${s.slice(0, n - 1)}…` : s
}

/**
 * What a person is told of an event: `{type, title, body, link, project}`, or null if nobody is.
 * `dashboard` is the dashboard's address, for links an event doesn't carry.
 */
export function message(event, dashboard) {
  const n = NOTIFIED[event.type]
  if (!n) return null
  const p = event.payload ?? {}
  const base = (dashboard ?? '').replace(/\/+$/, '')
  if (n.type === 'deployment_failed') {
    const d = p.deployment ?? {}
    const project = p.project?.name ?? d.name ?? 'a project'
    const step = d.error?.step ? ` while ${String(d.error.step).toLowerCase()}` : ''
    const why = d.error?.message ? `: ${clip(d.error.message, 280)}` : ''
    const commit = d.meta?.githubCommitMessage ? ` (${clip(d.meta.githubCommitMessage, 80)})` : ''
    return {
      type: n.type,
      project,
      title: `Deployment failed: ${project}`,
      body: `${targetWord(event)} deployment ${d.id ?? ''}${commit} failed${step}${why}`,
      link: p.links?.deployment ?? (base ? `${base}/projects/${project}` : null),
    }
  }
  if (n.type === 'certificate_failed') {
    const domain = p.domain?.name ?? 'a domain'
    const until = p.expires ? ` The current certificate keeps serving until ${String(p.expires).slice(0, 10)}.` : ''
    return {
      type: n.type,
      project: null,
      title: `Certificate renewal failed: ${domain}`,
      body: `Renewing the certificate for ${domain} failed${p.error ? `: ${clip(p.error, 240)}` : ''}.${until}`,
      link: base ? `${base}/domains` : null,
    }
  }
  const spend = base ? `${base}/settings/spend` : null
  if (n.type === 'budget_reached') {
    const project = typeof p.project === 'string' ? p.project : null
    const whose = project ? `${project}'s budget` : 'the team budget'
    const pct = Number(p.thresholdPercent ?? 0)
    const spent = money(p.spentN)
    const budget = money(p.budgetN)
    const then = pct >= 100
      ? (project ? ` ${project} is paused until it's resumed.` : ' Every project but those kept running is paused until resumed.')
      : (project ? ` At 100% ${project} is paused.` : " At 100% every project but those kept running is paused.")
    return {
      type: n.type,
      project,
      title: project ? `${pct >= 100 ? 'Budget reached' : `${pct}% of budget`}: ${project}` : pct >= 100 ? 'Team budget reached' : `${pct}% of the team budget`,
      body: `${spent} of ${whose} of ${budget} for ${monthName(p.month)} is spent (${pct}%).${then}`,
      link: spend,
    }
  }
  if (n.type === 'project_paused') {
    const projects = Array.isArray(p.projects) ? p.projects : (p.project ? [p.project] : [])
    const why = p.reason === 'install_budget'
      ? `the team budget of ${money(p.budgetN)} is reached (${money(p.spentN)} spent)`
      : p.reason === 'project_budget'
        ? `its monthly budget of ${money(p.budgetN)} is reached (${money(p.spentN)} spent)`
        : 'it was paused'
    const by = p.by === 'guard' ? " (by AWS's own numbers: the routers hadn't paused it)" : ''
    const list = projects.length > 6 ? `${projects.slice(0, 6).join(', ')} and ${projects.length - 6} more` : projects.join(', ')
    const one = projects.length === 1
    return {
      type: n.type,
      project: one ? projects[0] : null,
      title: one ? `Paused: ${projects[0]}` : `${projects.length} projects paused`,
      body: `${one ? projects[0] : list} ${one ? 'is' : 'are'} paused because ${why}${by}: no function runs and no file is served, so nothing more is spent.${p.cause ? ` The last hour, ${clip(p.cause, 200)}.` : ''} Raise the budget, then resume in Settings → Spend.`,
      link: spend,
    }
  }
  if (n.type === 'spend_fast') {
    const project = typeof p.project === 'string' ? p.project : 'A project'
    return {
      type: n.type,
      project: typeof p.project === 'string' ? p.project : null,
      title: `Fast spending: ${project}`,
      body: `${project} spent ${money(p.hourN)} in the last hour, more than 10% of its monthly budget of ${money(p.budgetN)}.${p.cause ? ` Most of it: ${clip(p.cause, 200)}.` : ''} Nothing is paused for this; the budget still pauses it if reached.`,
      link: spend,
    }
  }
  if (n.type === 'spend_blind' && p.source === 'bill') {
    return {
      type: n.type,
      project: null,
      title: `AWS's bill shows more than was counted (${p.date ?? 'yesterday'})`,
      body: `AWS's bill for ${p.date ?? 'yesterday'} shows ${money(p.billN)} of usage forkrail meters, more than the ${money(p.ledgerN)} the routers counted. Something isn't metered: budgets may pause later than they should.`,
      link: spend,
    }
  }
  if (n.type === 'spend_blind' && p.source === 'guard') {
    if (p.staleS) {
      return {
        type: n.type,
        project: null,
        title: "The routers' spend isn't up to date",
        body: `No router has published its month to date for ${Math.round(p.staleS / 60)} minutes while AWS shows forkrail's functions running. The guard still holds AWS's own numbers against the budgets.`,
        link: spend,
      }
    }
    return {
      type: n.type,
      project: p.project ?? null,
      title: `Spend not fully counted: ${p.project ?? 'a project'}`,
      body: `AWS's own numbers show ${p.project ?? 'a project'}'s functions cost at least ${money(p.awsN)} this month, more than the ${money(p.ledgerN)} the routers counted. Something isn't metered; the guard pauses by AWS's numbers if they reach a budget.`,
      link: spend,
    }
  }
  if (n.type === 'spend_blind') {
    return {
      type: n.type,
      project: null,
      title: `Spend can't be read (${event.region ?? 'a router'})`,
      body: `The router in ${event.region ?? 'one region'} can't read the month's spend${p.blindS ? ` for ${Math.round(p.blindS / 60)} minutes` : ''}${p.error ? ` (${clip(p.error, 160)})` : ''}. Until it can, it counts its own spend twice over against the budgets, so a project may pause a little early.`,
      link: spend,
    }
  }
  if (n.type === 'budget_raised') {
    return {
      type: n.type,
      project: null,
      title: `Monthly budget raised to ${money(p.budgetN)}`,
      body: `${monthName(p.month)} spent ${money(p.spentN)} of the ${money(p.fromN)} default budget (90% or more), so the next month's is ${money(p.budgetN)}. Set your own in Settings → Spend, and it stays as you set it.`,
      link: spend,
    }
  }
  if (n.type === 'attack_detected') {
    const refused = Number(p.refused ?? 0).toLocaleString('en-US')
    const blocks = Number(p.blocks ?? 0)
    return {
      type: n.type,
      project: null,
      title: 'Attack detected',
      body: `The system mitigations refused ${refused} requests and connections from ${p.addresses ?? 'several'} addresses in the last minute${blocks ? `, and blocked ${blocks}` : ''} (${event.region ?? 'a router'}).`,
      link: base || null,
    }
  }
  return null
}

/**
 * Who gets a notification of `event`: user ids, from the team's members (`[{id, role, login}]`).
 * A failed deployment goes to whoever made it if they're a member, else to the owners.
 */
export function recipients(event, members) {
  const n = NOTIFIED[event.type]
  if (!n) return []
  const owners = members.filter((m) => m.role === 'owner').map((m) => m.id)
  if (n.to === 'creator') {
    const who = String(event.payload?.deployment?.deployer ?? '').toLowerCase()
    const creator = who && members.find((m) => String(m.login ?? '').toLowerCase() === who)
    if (creator) return [creator.id]
  }
  return owners
}

/** Whether a person's preferences let `type` through `channel` (`web` or `email`): all on unless turned off. */
export function allows(prefs, type, channel) {
  const off = prefs?.off?.[type]
  return !(Array.isArray(off) && off.includes(channel))
}

function escape(text) {
  return String(text ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c])
}

/** An email: `{subject, text, html}`. `settings` is where the reader changes what they get. */
export function email({ title, body, link, action = 'Open in forkrail', settings }) {
  const footer = settings ? `You get this because of your notification settings: ${settings}` : 'Sent by forkrail.'
  const text = [title, '', body, ...(link ? ['', `${action}: ${link}`] : []), '', '--', footer].join('\n')
  const html = `<!doctype html><html><body style="margin:0;padding:24px;background:#f6f6f7;font-family:-apple-system,Segoe UI,Helvetica,Arial,sans-serif;color:#18181b">
<table role="presentation" width="100%" cellspacing="0" cellpadding="0" style="max-width:560px;margin:0 auto;background:#ffffff;border:1px solid #e4e4e7;border-radius:10px">
<tr><td style="padding:28px 28px 8px;font-size:13px;color:#71717a;letter-spacing:.02em">forkrail</td></tr>
<tr><td style="padding:0 28px;font-size:20px;font-weight:600;line-height:1.35">${escape(title)}</td></tr>
<tr><td style="padding:12px 28px 4px;font-size:15px;line-height:1.55;color:#3f3f46">${escape(body)}</td></tr>
${link ? `<tr><td style="padding:20px 28px 8px"><a href="${escape(link)}" style="display:inline-block;background:#5b5bd6;color:#ffffff;text-decoration:none;font-size:14px;font-weight:600;padding:10px 16px;border-radius:8px">${escape(action)}</a></td></tr>` : ''}
<tr><td style="padding:20px 28px 28px;font-size:12px;line-height:1.5;color:#a1a1aa">${settings ? `You get this because of your <a href="${escape(settings)}" style="color:#71717a">notification settings</a>.` : 'Sent by forkrail.'}</td></tr>
</table></body></html>`
  return { subject: title, text, html }
}

/** The From header: `Name <address>`, or the address alone. */
export function fromHeader(config) {
  const address = String(config?.from_email ?? '').trim()
  const name = String(config?.from_name ?? '').replace(/["\r\n<>]/g, '').trim()
  return name ? `"${name}" <${address}>` : address
}
