'use client'
// <Analytics /> for Next.js, App Router or Pages Router: a page view for each page with its route
// template (`/blog/[slug]`), from next/navigation's path and params.
import { createElement } from 'react'
import { useParams, usePathname } from 'next/navigation'
import { Analytics as Base } from './react.js'
import { computeRoute } from './index.js'

export { track } from './index.js'

export function Analytics(props) {
  const path = usePathname()
  const params = useParams()
  return createElement(Base, { framework: 'next', ...props, route: computeRoute(path, params), path: path ?? undefined })
}

export default Analytics
