import type { VercelRequest, VercelResponse } from '@vercel/node';

// The cron in vercel.json calls this; Vercel (and forkrail) send CRON_SECRET as a bearer token.
export default function handler(req: VercelRequest, res: VercelResponse) {
  const authorized = req.headers.authorization === `Bearer ${process.env.CRON_SECRET}`;
  console.log(`cron ran: authorized=${authorized} schedule=${req.headers['x-vercel-cron-schedule']}`);
  if (!authorized) return res.status(401).json({ ok: false });
  res.json({ ok: true, schedule: req.headers['x-vercel-cron-schedule'], agent: req.headers['user-agent'] });
}
