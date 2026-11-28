import { NextResponse } from 'next/server'

// Next 16's proxy (formerly middleware): runs on Node.
export function proxy(request) {
  const { pathname } = request.nextUrl
  if (pathname === '/legacy') {
    return NextResponse.redirect(new URL('/ssr', request.url))
  }
  if (pathname === '/rewritten') {
    return NextResponse.rewrite(new URL('/isr', request.url))
  }
  const response = NextResponse.next()
  response.headers.set('x-proxy-ran', '1')
  return response
}

export const config = { matcher: ['/legacy', '/rewritten', '/ssr', '/isr'] }
