// injectAnalytics() for SvelteKit, from the root +layout.ts (or +layout.svelte): inject() in the
// browser. Page views follow the address, client-side navigations included.
import { inject } from './index.js'

export { track } from './index.js'

export function injectAnalytics(props = {}) {
  inject({ framework: 'sveltekit', ...props })
}
