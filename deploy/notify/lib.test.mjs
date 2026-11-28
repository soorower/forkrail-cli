import assert from 'node:assert/strict'
import { createHmac } from 'node:crypto'
import { test } from 'node:test'

import { GIVE_UP_AFTER_MS, allows, email, fromHeader, isPublicAddress, message, nextTry, recipients, sign, urlProblem, wants } from './lib.mjs'

const failed = {
  id: 'evt_1',
  type: 'deployment.error',
  createdAt: 1_800_000_000_000,
  region: 'us-east-1',
  payload: {
    target: 'production',
    project: { id: 'shop', name: 'shop' },
    deployment: { id: 'a1b2c3d4', name: 'shop', deployer: 'Ada', error: { step: 'Building', message: 'npm ERR! missing script: build' }, meta: { githubCommitMessage: 'Fix the cart' } },
    links: { deployment: 'https://app.example.com/projects/shop/deployments/a1b2c3d4' },
  },
}
const team = [
  { id: 1, role: 'owner', login: 'olga' },
  { id: 2, role: 'member', login: 'ada' },
  { id: 3, role: 'owner', login: 'omar' },
  { id: 4, role: 'viewer', login: 'vic' },
]

test('a webhook gets the events and projects it asks for', () => {
  const hook = { events: ['deployment.error', 'attack.detected'], projects: ['shop'] }
  assert.equal(wants(hook, failed), true)
  assert.equal(wants({ ...hook, projects: ['blog'] }, failed), false)
  assert.equal(wants({ ...hook, projects: 'all' }, failed), true)
  assert.equal(wants({ ...hook, events: ['deployment.created'] }, failed), false)
  // The install's own events have no project: every webhook that asks gets them.
  assert.equal(wants({ ...hook, projects: ['blog'] }, { type: 'attack.detected', payload: {} }), true)
  assert.equal(wants({ ...hook, disabled: true }, failed), false)
})

test('the signature is HMAC-SHA1 of the exact body, in hex', () => {
  const body = JSON.stringify(failed)
  assert.equal(sign(body, 's3cr3t'), createHmac('sha1', 's3cr3t').update(body).digest('hex'))
  assert.match(sign(body, 's3cr3t'), /^[0-9a-f]{40}$/)
  assert.notEqual(sign(body, 's3cr3t'), sign(`${body} `, 's3cr3t'))
})

test('failed deliveries are tried again with backoff for a day', () => {
  const created = 1_800_000_000_000
  assert.equal(nextTry(1, created, created), created + 60_000)
  assert.equal(nextTry(2, created, created + 60_000), created + 60_000 + 300_000)
  let at = created
  let attempt = 1
  let tries = 1
  while ((at = nextTry(attempt, created, at)) != null) {
    attempt++
    tries++
  }
  assert.ok(tries >= 6 && tries <= 9, `${tries} tries`)
  assert.equal(nextTry(3, created, created + GIVE_UP_AFTER_MS), null)
})

test('webhooks reach public addresses only', () => {
  for (const ok of ['93.184.215.14', '2606:4700::1111', '8.8.8.8', '2001:4860:4860::8888']) assert.equal(isPublicAddress(ok), true, ok)
  for (const bad of ['127.0.0.1', '10.1.2.3', '172.31.9.153', '192.168.1.1', '169.254.169.254', '100.64.0.1', '0.0.0.0', '::1', 'fd00::1', 'fe80::1', '::ffff:127.0.0.1', '::ffff:169.254.169.254', '::ffff:7f00:1', '0:0:0:0:0:ffff:a9fe:a9fe', '224.0.0.1', 'nope']) {
    assert.equal(isPublicAddress(bad), false, bad)
  }
  assert.equal(urlProblem('https://hooks.example.com/forkrail'), null)
  assert.match(urlProblem('http://hooks.example.com/x'), /HTTPS/)
  assert.match(urlProblem('https://localhost/x'), /public/)
  assert.match(urlProblem('https://169.254.169.254/latest'), /public/)
  assert.match(urlProblem('https://[::1]/x'), /public/)
  assert.match(urlProblem('https://user:pw@example.com/'), /password/)
  assert.match(urlProblem('nope'), /URL/)
})

test('a failed deployment goes to whoever made it, else to the owners', () => {
  assert.deepEqual(recipients(failed, team), [2])
  const byStranger = { ...failed, payload: { ...failed.payload, deployment: { ...failed.payload.deployment, deployer: 'someone' } } }
  assert.deepEqual(recipients(byStranger, team), [1, 3])
  const byHook = { ...failed, payload: { ...failed.payload, deployment: { ...failed.payload.deployment, deployer: null } } }
  assert.deepEqual(recipients(byHook, team), [1, 3])
  assert.deepEqual(recipients({ type: 'attack.detected', payload: {} }, team), [1, 3])
  assert.deepEqual(recipients({ type: 'deployment.succeeded', payload: {} }, team), [])
})

test('what people are told', () => {
  const m = message(failed, 'https://app.example.com/')
  assert.equal(m.type, 'deployment_failed')
  assert.equal(m.title, 'Deployment failed: shop')
  assert.match(m.body, /^Production deployment a1b2c3d4 \(Fix the cart\) failed while building: npm ERR! missing script: build$/)
  assert.equal(m.link, failed.payload.links.deployment)
  const cert = message({ type: 'domain.certificate.renew.failed', payload: { domain: { name: 'shop.example.com' }, error: 'DNS points elsewhere', expires: '2026-12-01T00:00:00Z' } }, 'https://app.example.com')
  assert.equal(cert.title, 'Certificate renewal failed: shop.example.com')
  assert.match(cert.body, /keeps serving until 2026-12-01/)
  assert.equal(cert.link, 'https://app.example.com/domains')
  const attack = message({ type: 'attack.detected', region: 'us-west-1', payload: { refused: 12345, blocks: 3, addresses: 40 } }, '')
  assert.match(attack.body, /12,345 requests .* 40 addresses .* blocked 3 \(us-west-1\)/)
  assert.equal(attack.link, null)
  assert.equal(message({ type: 'deployment.created', payload: {} }, ''), null)
})

test('preferences: all on unless turned off', () => {
  assert.equal(allows(undefined, 'deployment_failed', 'email'), true)
  assert.equal(allows({ off: { deployment_failed: ['email'] } }, 'deployment_failed', 'email'), false)
  assert.equal(allows({ off: { deployment_failed: ['email'] } }, 'deployment_failed', 'web'), true)
  assert.equal(allows({ off: { attack_detected: ['web'] } }, 'deployment_failed', 'web'), true)
})

test('emails escape what they show', () => {
  const mail = email({ title: 'Deployment failed: <shop>', body: 'a & b "c"', link: 'https://app.example.com/x?a=1&b=2', settings: 'https://app.example.com/account/notifications' })
  assert.equal(mail.subject, 'Deployment failed: <shop>')
  assert.ok(mail.html.includes('Deployment failed: &lt;shop&gt;') && mail.html.includes('a &amp; b &quot;c&quot;') && mail.html.includes('a=1&amp;b=2'))
  assert.ok(!mail.html.includes('<shop>'))
  assert.ok(mail.text.includes('https://app.example.com/x?a=1&b=2') && mail.text.includes('notification settings'))
  assert.equal(fromHeader({ from_email: 'notify@example.com', from_name: 'Acme "Ops"' }), '"Acme Ops" <notify@example.com>')
  assert.equal(fromHeader({ from_email: 'notify@example.com' }), 'notify@example.com')
})

test('spend: a budget reached, a pause, fast spending, a raised default', () => {
  const reached = { id: 'evt_a', type: 'budget.reached', region: 'us-east-1', payload: { scope: 'project', project: 'shop', thresholdPercent: 75, spentN: 15_004_000_000, budgetN: 20e9, month: 202611 } }
  const m = message(reached, 'https://app.example.com/')
  assert.equal(m.title, '75% of budget: shop')
  assert.match(m.body, /^\$15\.00 of shop's budget of \$20\.00 for November is spent \(75%\)\. At 100% shop is paused\.$/)
  assert.equal(m.link, 'https://app.example.com/settings/spend')
  assert.deepEqual(recipients(reached, team), [1, 3], 'owners')
  assert.equal(wants({ events: ['budget.reached'], projects: ['shop'] }, reached), true)
  assert.equal(wants({ events: ['budget.reached'], projects: ['blog'] }, reached), false)
  const install = message({ ...reached, payload: { scope: 'team', project: null, thresholdPercent: 100, spentN: 20e9, budgetN: 20e9, month: 202611 } }, null)
  assert.equal(install.title, 'Team budget reached')
  assert.match(install.body, /of the team budget of \$20\.00 .*Every project but those kept running is paused/)
  assert.equal(message({ ...reached, payload: { scope: 'team', project: null, thresholdPercent: 50, spentN: 10e9, budgetN: 20e9, month: 202611 } }, null).title, '50% of the team budget')

  const all = message({ type: 'spend.paused', payload: { reason: 'install_budget', projects: ['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h'], spentN: 150e9, budgetN: 150e9 } }, null)
  assert.equal(all.title, '8 projects paused')
  assert.match(all.body, /^a, b, c, d, e, f and 2 more are paused because the team budget of \$150\.00 is reached/)
  const one = message({ type: 'spend.paused', payload: { reason: 'project_budget', project: 'shop', projects: ['shop'], spentN: 5e9, budgetN: 5e9, cause: '90% of it data sent to visitors: /files/big.zip 70%' } }, null)
  assert.match(one.body, /The last hour, 90% of it data sent to visitors: \/files\/big\.zip 70%\. Raise the budget/)
  assert.equal(one.title, 'Paused: shop')
  assert.equal(one.project, 'shop')

  const fast = message({ type: 'spend.fast', payload: { project: 'shop', hourN: 2.1e9, budgetN: 20e9, cause: 'egress of /files/big.zip from AS14061' } }, null)
  assert.match(fast.body, /^shop spent \$2\.10 in the last hour.*Most of it: egress of \/files\/big\.zip from AS14061\. Nothing is paused/)
  const raised = message({ type: 'budget.raised', payload: { month: 202611, spentN: 18.4e9, fromN: 20e9, budgetN: 30e9 } }, null)
  assert.equal(raised.title, 'Monthly budget raised to $30.00')
  assert.match(raised.body, /^November spent \$18\.40 of the \$20\.00 default budget/)
  assert.match(message({ type: 'spend.blind', region: 'us-west-1', payload: { blindS: 180 } }, null).body, /us-west-1 .* for 3 minutes/)
})

test('spend: what the guard says', () => {
  const gap = message({ type: 'spend.blind', payload: { source: 'guard', project: 'shop', awsN: 12.5e9, ledgerN: 3e9 } }, null)
  assert.equal(gap.title, 'Spend not fully counted: shop')
  assert.match(gap.body, /at least \$12\.50 this month, more than the \$3\.00 the routers counted/)
  assert.match(message({ type: 'spend.blind', payload: { source: 'guard', staleS: 1500 } }, null).body, /for 25 minutes/)
  const paused = message({ type: 'spend.paused', payload: { reason: 'project_budget', by: 'guard', project: 'shop', projects: ['shop'], spentN: 5e9, budgetN: 5e9 } }, null)
  assert.match(paused.body, /reached \(\$5\.00 spent\) \(by AWS's own numbers: the routers hadn't paused it\)/)
})

test('spend: the bill against the ledger', () => {
  const m = message({ type: 'spend.blind', payload: { source: 'bill', date: '2026-11-20', billN: 7.5e9, ledgerN: 5e9 } }, null)
  assert.equal(m.title, "AWS's bill shows more than was counted (2026-11-20)")
  assert.match(m.body, /2026-11-20 shows \$7\.50 .* than the \$5\.00 the routers counted/)
})
