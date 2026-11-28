// CommonJS, with the helpers: body by content type, cookies, query, send.
module.exports = (req, res) => {
  res.setHeader('x-shape', 'node');
  res.send({ method: req.method, body: req.body, cookies: req.cookies, query: req.query });
};
