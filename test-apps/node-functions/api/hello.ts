import type { VercelRequest, VercelResponse } from '@vercel/node';
import ms from 'ms';
import { greet } from './_lib/greet';

// Vercel's Node shape: (req, res) with its helpers.
export default function handler(req: VercelRequest, res: VercelResponse) {
  const { name = 'World' } = req.query;
  return res.json({ message: greet(String(name)), uptime: ms(Math.round(process.uptime() * 1000)), function: 'api/hello.ts' });
}
