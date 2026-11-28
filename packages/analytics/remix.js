'use client'
// <Analytics /> for Remix and React Router: the React component, named for them.
import { createElement } from 'react'
import { Analytics as Base } from './react.js'

export { track } from './index.js'

export function Analytics(props) {
  return createElement(Base, { framework: 'remix', ...props })
}

export default Analytics
