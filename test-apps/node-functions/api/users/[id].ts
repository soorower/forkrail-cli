import type { VercelRequest, VercelResponse } from '@vercel/node';

// A dynamic segment: its value is in the query.
export default function handler(req: VercelRequest, res: VercelResponse) {
  res.status(200).json({ id: req.query.id, query: req.query, path: req.url, function: 'api/users/[id].ts' });
}
