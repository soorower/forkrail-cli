export default function handler(req, res) {
  res.status(200).json({ hello: 'pages', query: req.query })
}
