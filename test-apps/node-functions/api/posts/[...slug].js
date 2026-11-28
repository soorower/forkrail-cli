// A catch-all's name, as Vercel's zero-config routes treat it outside Next.js: one segment, in
// the query under its name.
export default function handler(req, res) {
  res.json({ query: req.query, function: 'api/posts/[...slug].js' });
}
