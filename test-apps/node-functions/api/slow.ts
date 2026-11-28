import type { VercelRequest, VercelResponse } from '@vercel/node';

// Its own maxDuration, from the file: the router stops it after 2 seconds.
export const config = { maxDuration: 2 };

export default async function handler(req: VercelRequest, res: VercelResponse) {
  const wait = Number(req.query.ms ?? 0);
  await new Promise((r) => setTimeout(r, wait));
  res.json({ waited: wait });
}
