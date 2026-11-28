import { NextResponse } from 'next/server'

export function middleware() {
  const response = NextResponse.next()
  response.headers.set('x-middleware-ran', '1')
  return response
}

export const config = { matcher: ['/ssr'] }
