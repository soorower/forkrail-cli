module.exports = {
  async redirects() {
    return [{ source: '/old', destination: '/ssr', permanent: false }]
  },
}
