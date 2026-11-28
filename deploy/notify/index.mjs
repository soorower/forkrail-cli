// forkrail-notify (deploy/install/notify.yaml): takes the install's events off its queue
// (`forkrail-events`), posts each to the team's webhooks that ask for it, and tells the people it
// concerns: a notification in the dashboard's bell and, if they have a verified address and the
// team has email set up, an email. Email goes through the install's own Amazon SES or its own
// SMTP server; forkrail sends nothing itself.
//
// The dashboard also asks it directly (a synchronous invoke) to send a test email, a verification
// email, a test webhook delivery, and to look at or set up the install's SES domain: secrets (the
// SMTP password, webhook secrets) are encrypted by the dashboard with `alias/forkrail-notify`,
// which only this function can decrypt.
//
// Items it reads and writes in the dashboard's table (web/src/server/web-table.ts says the same):
//   {pk: 'team', sk: 'member#<id>'}            members and roles (read)
//   {pk: 'user#<id>', sk: 'user'}              logins (read)
//   {pk: 'user#<id>', sk: 'notify'}            a person's preferences and verified email (read)
//   {pk: 'notif#<id>', sk: '<iso>#<hex>'}      a notification in their bell (written)
//   {pk: 'notify', sk: 'email'}                how the team sends email (read)
//   {pk: 'notify', sk: 'webhook#<id>'}         the team's webhooks (read; `last` written)
//   {pk: 'webhook#<id>', sk: '<iso>#<hex>'}    a webhook's deliveries (written, kept 30 days)
//   {pk: 'event#<id>', sk: 'seen'}             an event already handled (written, kept 2 days; a
//                                              budget's, 40: lib.mjs ONCE_A_MONTH)
//
// A webhook delivery that fails is put back on the queue for its next try (lib.mjs RETRY_AFTER_S),
// up to a day after the event. Webhooks reach public HTTPS addresses only: the host is resolved,
// every address checked, and the connection made to the checked one.

import { randomBytes } from 'node:crypto'
import { lookup } from 'node:dns/promises'
import https from 'node:https'

import { BatchGetItemCommand, DeleteItemCommand, DynamoDBClient, GetItemCommand, PutItemCommand, QueryCommand, UpdateItemCommand } from '@aws-sdk/client-dynamodb'
import { DecryptCommand, KMSClient } from '@aws-sdk/client-kms'
import { CreateEmailIdentityCommand, GetAccountCommand, GetEmailIdentityCommand, SESv2Client, SendEmailCommand } from '@aws-sdk/client-sesv2'
import { SQSClient, SendMessageCommand } from '@aws-sdk/client-sqs'
import { marshall, unmarshall } from '@aws-sdk/util-dynamodb'

import { MAX_DELAY_S, NOTIFIED, ONCE_A_MONTH, TIMEOUT_MS, allows, email, fromHeader, isPublicAddress, message, nextTry, recipients, sign, urlProblem, wants } from './lib.mjs'

const TABLE = process.env.WEB_TABLE || 'forkrail-web'
const QUEUE_URL = process.env.EVENTS_QUEUE_URL || ''
const DASHBOARD = (process.env.DASHBOARD_URL || '').replace(/\/+$/, '')
const REGION = process.env.AWS_REGION || 'us-east-1'
const DAY_S = 86_400
const USER_AGENT = 'forkrail-webhooks/1.0'

const db = new DynamoDBClient({})
const kms = new KMSClient({})
const sqs = new SQSClient({})
const sesClients = new Map()
const ses = (region) => {
  const r = region || REGION
  if (!sesClients.has(r)) sesClients.set(r, new SESv2Client({ region: r }))
  return sesClients.get(r)
}

/* ── the table ───────────────────────────────────────────────── */

async function getItem(pk, sk) {
  const out = await db.send(new GetItemCommand({ TableName: TABLE, Key: marshall({ pk, sk }), ConsistentRead: true }))
  return out.Item ? unmarshall(out.Item) : null
}

async function query(pk, prefix) {
  const items = []
  let start
  do {
    const out = await db.send(new QueryCommand({
      TableName: TABLE,
      KeyConditionExpression: '#pk = :pk AND begins_with(#sk, :prefix)',
      ExpressionAttributeNames: { '#pk': 'pk', '#sk': 'sk' },
      ExpressionAttributeValues: marshall({ ':pk': pk, ':prefix': prefix }),
      ExclusiveStartKey: start,
    }))
    items.push(...(out.Items ?? []).map((i) => unmarshall(i)))
    start = out.LastEvaluatedKey
  } while (start)
  return items
}

async function getMany(keys) {
  const found = []
  for (let i = 0; i < keys.length; i += 100) {
    let request = { [TABLE]: { Keys: keys.slice(i, i + 100).map((k) => marshall(k)), ConsistentRead: true } }
    for (let round = 0; request && Object.keys(request).length && round < 5; round++) {
      const out = await db.send(new BatchGetItemCommand({ RequestItems: request }))
      found.push(...(out.Responses?.[TABLE] ?? []).map((x) => unmarshall(x)))
      request = out.UnprocessedKeys && Object.keys(out.UnprocessedKeys).length ? out.UnprocessedKeys : null
    }
  }
  return found
}

async function putItem(item, condition) {
  await db.send(new PutItemCommand({
    TableName: TABLE,
    Item: marshall(item, { removeUndefinedValues: true }),
    ...(condition ? { ConditionExpression: condition, ExpressionAttributeNames: { '#pk': 'pk' } } : {}),
  }))
}

const stamp = (ms = Date.now()) => `${new Date(ms).toISOString()}#${randomBytes(4).toString('hex')}`

/** Mark an event handled; false if it was already (the queue delivers at least once, and every
 * router sends a budget's events under the same id). */
async function claim(id, days = 2) {
  try {
    await putItem({ pk: `event#${id}`, sk: 'seen', at: Date.now(), ttl: Math.ceil(Date.now() / 1000) + days * DAY_S }, 'attribute_not_exists(#pk)')
    return true
  } catch (e) {
    if (e.name === 'ConditionalCheckFailedException') return false
    throw e
  }
}

/* ── secrets ─────────────────────────────────────────────────── */

async function decrypt(ciphertext, context) {
  const out = await kms.send(new DecryptCommand({ CiphertextBlob: Buffer.from(ciphertext, 'base64'), EncryptionContext: context }))
  return Buffer.from(out.Plaintext).toString('utf8')
}

/* ── email ───────────────────────────────────────────────────── */

async function emailConfig() {
  const config = await getItem('notify', 'email')
  return config && config.mode && config.mode !== 'off' && config.from_email ? config : null
}

/** Send one email with the team's transport; the provider's message id. */
async function send(config, to, mail) {
  if (config.mode === 'ses') {
    const out = await ses(config.ses_region).send(new SendEmailCommand({
      FromEmailAddress: fromHeader(config),
      Destination: { ToAddresses: [to] },
      Content: { Simple: { Subject: { Data: mail.subject, Charset: 'UTF-8' }, Body: { Text: { Data: mail.text, Charset: 'UTF-8' }, Html: { Data: mail.html, Charset: 'UTF-8' } } } },
    }))
    return out.MessageId ?? null
  }
  if (config.mode === 'smtp') {
    const { default: nodemailer } = await import('nodemailer')
    const port = Number(config.smtp_port) || 587
    const password = config.smtp_password ? await decrypt(config.smtp_password, { forkrail: 'smtp-password' }) : undefined
    const transport = nodemailer.createTransport({
      host: config.smtp_host,
      port,
      secure: config.smtp_security === 'tls' || (config.smtp_security == null && port === 465),
      requireTLS: config.smtp_security !== 'tls',
      auth: config.smtp_user ? { user: config.smtp_user, pass: password } : undefined,
      connectionTimeout: 10_000,
      greetingTimeout: 10_000,
      socketTimeout: 20_000,
    })
    try {
      const out = await transport.sendMail({ from: fromHeader(config), to, subject: mail.subject, text: mail.text, html: mail.html })
      return out.messageId ?? null
    } finally {
      transport.close()
    }
  }
  throw new Error(`email mode ${config.mode} isn't one this notifier knows`)
}

/** An error as one line a person can act on. */
function said(e) {
  const text = `${e?.name && e.name !== 'Error' ? `${e.name}: ` : ''}${e?.message ?? e}`
  if (/not verified/i.test(text)) return `${text} (while SES is in its sandbox, it sends only to addresses verified in SES)`
  return text
}

/* ── webhooks ────────────────────────────────────────────────── */

/** Post `body` to `url`, at an address checked to be public. `{status, ms, error?}`. */
async function post(url, body, headers) {
  const started = Date.now()
  const problem = urlProblem(url)
  if (problem) return { status: null, ms: 0, error: problem }
  const { hostname } = new URL(url)
  const host = hostname.replace(/^\[|\]$/g, '')
  let addresses
  try {
    addresses = await lookup(host, { all: true, verbatim: true })
  } catch (e) {
    return { status: null, ms: Date.now() - started, error: `${host} doesn't resolve (${e.code ?? e.message})` }
  }
  if (!addresses.length || addresses.some((a) => !isPublicAddress(a.address))) {
    return { status: null, ms: Date.now() - started, error: `${host} resolves to an address webhooks may not reach` }
  }
  const pinned = addresses[0]
  return new Promise((resolve) => {
    let done = false
    const finish = (result) => {
      if (!done) {
        done = true
        clearTimeout(deadline)
        resolve(result)
      }
    }
    const request = https.request(url, {
      method: 'POST',
      headers: { 'content-type': 'application/json', 'content-length': Buffer.byteLength(body), 'user-agent': USER_AGENT, ...headers },
      timeout: TIMEOUT_MS,
      lookup: (_name, options, callback) => (options?.all ? callback(null, [pinned]) : callback(null, pinned.address, pinned.family)),
    }, (response) => {
      // The status is the answer: a body trickling in isn't waited for.
      finish({ status: response.statusCode, ms: Date.now() - started })
      response.destroy()
    })
    // A deadline no trickle of bytes (or of 100 Continues) moves: the invocation has others to
    // send, and its claim on the event is let go only if it gets to say it failed.
    const deadline = setTimeout(() => request.destroy(new Error(`no answer within ${TIMEOUT_MS / 1000} s`)), TIMEOUT_MS)
    request.on('timeout', () => request.destroy(new Error(`no answer within ${TIMEOUT_MS / 1000} s`)))
    request.on('error', (e) => finish({ status: null, ms: Date.now() - started, error: e.message }))
    request.end(body)
  })
}

/** Deliver `event` to `webhook` once: logged with the webhook. `{ok, status, ms, error?}`. */
async function deliver(webhook, event, attempt) {
  const body = JSON.stringify(event)
  let result
  try {
    const secret = await decrypt(webhook.secret, { forkrail: 'webhook-secret', webhook: webhook.id })
    result = await post(webhook.url, body, { 'x-forkrail-signature': sign(body, secret), 'x-forkrail-event': event.type, 'x-forkrail-delivery': `${event.id}.${attempt}` })
  } catch (e) {
    result = { status: null, ms: 0, error: `the webhook's secret couldn't be read: ${said(e)}` }
  }
  const ok = result.status != null && result.status >= 200 && result.status < 300
  if (!ok && !result.error && result.status != null) result.error = `answered HTTP ${result.status}`
  const at = Date.now()
  const line = { event_id: event.id, type: event.type, status: result.status, ok, ms: result.ms, error: result.error, attempt, at }
  await Promise.allSettled([
    putItem({ pk: `webhook#${webhook.id}`, sk: stamp(at), ...line, ttl: Math.ceil(at / 1000) + 30 * DAY_S }),
    db.send(new UpdateItemCommand({
      TableName: TABLE,
      Key: marshall({ pk: 'notify', sk: `webhook#${webhook.id}` }),
      UpdateExpression: 'SET #last = :last',
      ConditionExpression: 'attribute_exists(#pk)',
      ExpressionAttributeNames: { '#last': 'last', '#pk': 'pk' },
      ExpressionAttributeValues: marshall({ ':last': { at, type: event.type, status: result.status, ok, ms: result.ms, error: result.error } }, { removeUndefinedValues: true }),
    })),
  ])
  return { ok, status: result.status, ms: result.ms, ...(result.error ? { error: result.error } : {}) }
}

/** Put a delivery back on the queue to be tried at `due` (ms), in steps of at most 15 minutes. */
async function later(message, due) {
  const delay = Math.max(0, Math.min(MAX_DELAY_S, Math.ceil((due - Date.now()) / 1000)))
  await sqs.send(new SendMessageCommand({ QueueUrl: QUEUE_URL, MessageBody: JSON.stringify(message), DelaySeconds: delay }))
}

/** Deliver to one webhook, and schedule the next try if it failed. */
async function deliverAndRetry(webhook, event, attempt) {
  const result = await deliver(webhook, event, attempt)
  if (!result.ok) {
    const due = nextTry(attempt, Number(event.createdAt) || Date.now())
    if (due != null) await later({ kind: 'delivery', webhook: webhook.id, attempt: attempt + 1, due, event }, due)
    else console.log(JSON.stringify({ gave_up: event.id, webhook: webhook.id, attempt }))
  }
  return result
}

/* ── people ──────────────────────────────────────────────────── */

async function members() {
  const rows = await query('team', 'member#')
  const users = await getMany(rows.map((m) => ({ pk: `user#${m.id}`, sk: 'user' })))
  const login = new Map(users.map((u) => [u.id, u.login]))
  return rows.map((m) => ({ id: m.id, role: m.role, login: login.get(m.id) ?? null }))
}

/** Tell the people `event` concerns: in the bell, and by email where they and the team allow it. */
async function notifyPeople(event) {
  const what = message(event, DASHBOARD)
  if (!what) return
  const ids = recipients(event, await members())
  if (!ids.length) return
  const prefs = new Map((await getMany(ids.map((id) => ({ pk: `user#${id}`, sk: 'notify' })))).map((p) => [Number(String(p.pk).slice(5)), p]))
  let config
  for (const id of ids) {
    const p = prefs.get(id) ?? {}
    const at = Date.now()
    if (allows(p, what.type, 'web')) {
      await putItem({ pk: `notif#${id}`, sk: stamp(at), type: what.type, title: what.title, body: what.body, link: what.link ?? undefined, project: what.project ?? undefined, event_id: event.id, at, ttl: Math.ceil(at / 1000) + 365 * DAY_S })
    }
    if (p.email && p.email_verified_at && allows(p, what.type, 'email')) {
      config ??= (await emailConfig()) ?? false
      if (!config) continue
      try {
        await send(config, p.email, email({ ...what, settings: DASHBOARD ? `${DASHBOARD}/account/notifications` : null }))
      } catch (e) {
        console.log(JSON.stringify({ email_failed: event.id, user: id, error: said(e) }))
      }
    }
  }
}

/* ── events ──────────────────────────────────────────────────── */

async function handleEvent(event) {
  if (!event?.id || !event?.type) throw new Error('not an event')
  if (!(await claim(event.id, ONCE_A_MONTH.has(event.type) ? 40 : 2))) return
  try {
    const webhooks = (await query('notify', 'webhook#')).filter((w) => wants(w, event))
    await Promise.allSettled(webhooks.map((w) => deliverAndRetry(w, event, 1)))
    if (NOTIFIED[event.type]) await notifyPeople(event)
  } catch (e) {
    // Tried again from the queue: not handled yet.
    await db.send(new DeleteItemCommand({ TableName: TABLE, Key: marshall({ pk: `event#${event.id}`, sk: 'seen' }) })).catch(() => {})
    throw e
  }
}

async function handleDelivery(m) {
  // Not due yet: a wait longer than SQS's longest delay.
  if (m.due && m.due - Date.now() > 5000) return later(m, m.due)
  const webhook = await getItem('notify', `webhook#${m.webhook}`)
  if (!webhook || !wants(webhook, m.event)) return
  await deliverAndRetry(webhook, m.event, m.attempt)
}

/* ── what the dashboard asks ─────────────────────────────────── */

const isEmail = (s) => typeof s === 'string' && s.length <= 254 && /^[^\s@<>"]+@[^\s@<>"]+\.[^\s@<>"]+$/.test(s)

async function ask(request) {
  switch (request.kind) {
    case 'test_email':
    case 'verify_email': {
      if (!isEmail(request.to)) return { ok: false, error: 'not an email address' }
      const config = await emailConfig()
      if (!config) return { ok: false, error: "the team's email is off: an owner sets it up in Settings → Email" }
      const mail = request.kind === 'test_email'
        ? email({ title: 'Test email from forkrail', body: `This is a test. forkrail sends this team's email through ${config.mode === 'ses' ? 'Amazon SES' : `SMTP (${config.smtp_host})`}, from ${config.from_email}.`, link: DASHBOARD || null })
        : email({ title: 'Confirm your email address', body: `${request.name ? `Hi ${request.name}, c` : 'C'}onfirm that forkrail may send your notifications to this address. The link works for 24 hours; if you didn't ask for this, ignore this email.`, link: request.link, action: 'Confirm address' })
      try {
        return { ok: true, message_id: await send(config, request.to, mail) }
      } catch (e) {
        return { ok: false, error: said(e) }
      }
    }
    case 'test_webhook': {
      const webhook = await getItem('notify', `webhook#${request.webhook}`)
      if (!webhook) return { ok: false, status: null, ms: 0, error: 'no such webhook' }
      const event = { id: `evt_${randomBytes(12).toString('hex')}`, type: 'webhook.test', createdAt: Date.now(), payload: { webhook: { id: webhook.id }, message: 'A test delivery from forkrail.' }, region: REGION }
      return deliver(webhook, event, 1)
    }
    case 'ses_status': {
      const domain = String(request.domain ?? '').toLowerCase()
      const config = await getItem('notify', 'email')
      const client = ses(request.region || config?.ses_region)
      let identity = null
      try {
        const out = await client.send(new GetEmailIdentityCommand({ EmailIdentity: domain }))
        identity = { verified: !!out.VerifiedForSendingStatus, dkim_status: out.DkimAttributes?.Status ?? 'NOT_STARTED', tokens: out.DkimAttributes?.Tokens ?? [] }
      } catch (e) {
        if (e.name !== 'NotFoundException') return { ok: false, error: said(e) }
      }
      try {
        const account = await client.send(new GetAccountCommand({}))
        return {
          ok: true,
          region: await client.config.region(),
          identity,
          account: { production: !!account.ProductionAccessEnabled, max_24h: account.SendQuota?.Max24HourSend ?? 0, sent_24h: account.SendQuota?.SentLast24Hours ?? 0, max_per_second: account.SendQuota?.MaxSendRate ?? 0 },
        }
      } catch (e) {
        return { ok: false, error: said(e) }
      }
    }
    case 'ses_setup': {
      const domain = String(request.domain ?? '').toLowerCase()
      if (!/^[a-z0-9.-]+\.[a-z]{2,}$/.test(domain)) return { ok: false, error: 'not a domain' }
      const config = await getItem('notify', 'email')
      try {
        const out = await ses(request.region || config?.ses_region).send(new CreateEmailIdentityCommand({ EmailIdentity: domain, Tags: [{ Key: 'project', Value: 'forkrail' }] }))
        return { ok: true, tokens: out.DkimAttributes?.Tokens ?? [] }
      } catch (e) {
        return { ok: false, error: said(e) }
      }
    }
    default:
      return { ok: false, error: `nothing called ${request.kind}` }
  }
}

/* ── the handler ─────────────────────────────────────────────── */

export async function handler(input) {
  if (!Array.isArray(input?.Records)) return ask(input ?? {})
  const failures = []
  // Together, so a slow webhook (30 s at most) holds up only its own message.
  await Promise.all(input.Records.map(async (record) => {
    try {
      const body = JSON.parse(record.body)
      if (body.kind === 'delivery') await handleDelivery(body)
      else await handleEvent(body)
    } catch (e) {
      console.log(JSON.stringify({ failed: record.messageId, error: said(e) }))
      // A message that isn't JSON or an event won't get better: dropped, not retried.
      if (!(e instanceof SyntaxError) && e?.message !== 'not an event') failures.push({ itemIdentifier: record.messageId })
    }
  }))
  return { batchItemFailures: failures }
}
