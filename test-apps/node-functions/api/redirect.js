module.exports = (req, res) => {
  res.redirect(req.query.to || '/api/hello');
};
