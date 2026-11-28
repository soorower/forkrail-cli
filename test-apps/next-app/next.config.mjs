/** @type {import('next').NextConfig} */
export default {
  async redirects() {
    return [{ source: '/old-blog/:slug', destination: '/blog/:slug', permanent: true }]
  },
  async rewrites() {
    return {
      beforeFiles: [{ source: '/hi', destination: '/api/hello' }],
      afterFiles: [{ source: '/docs/:path*', destination: '/ssr' }],
      fallback: [],
    }
  },
  async headers() {
    return [{ source: '/ssr', headers: [{ key: 'x-from-config', value: 'yes' }] }]
  },
}
