import { headers } from 'next/headers'

export const dynamic = 'force-dynamic'

export default async function Ssr() {
  const h = await headers()
  return (
    <main>
      <h1>SSR</h1>
      <p id="now">{new Date().toISOString()}</p>
      <p id="proxy">{h.get('x-proxy-ran') ?? 'none'}</p>
    </main>
  )
}
