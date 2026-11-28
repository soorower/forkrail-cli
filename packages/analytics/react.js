'use client'
// <Analytics />: inject() once, and with `route` (and `path`) given, a page view each time they
// change, for a router that knows its routes (it turns the script's own page views off).
import { useEffect, useRef } from 'react'
import { inject, pageview } from './index.js'

export { track } from './index.js'

export function Analytics(props) {
  const { route, path, ...rest } = props
  const managed = route !== undefined
  const started = useRef(false)
  useEffect(() => {
    if (started.current) return
    started.current = true
    inject({ framework: 'react', ...rest, disableAutoTrack: rest.disableAutoTrack ?? managed })
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])
  useEffect(() => {
    if (managed && route) pageview({ route, path: path ?? (typeof location !== 'undefined' ? location.pathname : undefined) })
  }, [managed, route, path])
  return null
}

export default Analytics
