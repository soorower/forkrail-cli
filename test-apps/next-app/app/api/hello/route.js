export const dynamic = 'force-dynamic'

export async function GET(request) {
  const url = new URL(request.url)
  return Response.json({ hello: 'world', path: url.pathname, query: Object.fromEntries(url.searchParams) })
}

export async function POST(request) {
  return Response.json({ got: await request.text() })
}
