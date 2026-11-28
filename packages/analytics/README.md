# @forkrail/analytics

Web Analytics for apps deployed on forkrail: page views, where visitors came from, and custom
events. No cookies. Your own routers collect it, and it's kept on your analytics box in your own
AWS account. Nothing goes to a third party.

Switch it on for a project first: in the dashboard (Analytics, or Project settings → General), or
`forkrail project NAME --analytics on`. Until then the routers answer the script with an empty one.

Apps that already use Vercel's `@vercel/analytics` need no change: forkrail's routers answer its
requests themselves, and nothing goes to Vercel.

## Use

```sh
npm i @forkrail/analytics
```

Then add it once, where every page goes through:

| Framework | Where | What |
| --- | --- | --- |
| Next.js | `app/layout.tsx` (in `<body>`) or `pages/_app.tsx` | `import { Analytics } from '@forkrail/analytics/next'` and `<Analytics />` |
| TanStack Start | `src/routes/__root.tsx` | `import { inject } from '@forkrail/analytics'` and `inject()` |
| React (Vite...) | `src/App.tsx` | `import { Analytics } from '@forkrail/analytics/react'` and `<Analytics />` |
| React Router / Remix | `app/root.tsx` (in `<body>`) | `import { Analytics } from '@forkrail/analytics/remix'` and `<Analytics />` |
| SvelteKit | `src/routes/+layout.ts` | `import { injectAnalytics } from '@forkrail/analytics/sveltekit'` and `injectAnalytics()` |
| Nuxt | `nuxt.config.ts` | `modules: ['@forkrail/analytics/nuxt']` |
| Vue | `src/App.vue` | `import { Analytics } from '@forkrail/analytics/vue'` and `<Analytics />` |
| Astro | your layout (in `<body>`) | `import Analytics from '@forkrail/analytics/astro'` and `<Analytics />` |

For example, Next.js:

```tsx
import { Analytics } from '@forkrail/analytics/next'

export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en">
      <body>
        {children}
        <Analytics />
      </body>
    </html>
  )
}
```

The Next.js and Astro components count each page with its route (`/blog/[slug]`), client-side
navigations and view transitions included. Every entry takes the same options as `inject()`.

Without a package, a script tag does the same:

```html
<script>
  window.va = window.va || function () { (window.vaq = window.vaq || []).push(arguments); };
</script>
<script defer src="/_forkrail/insights/script.js"></script>
```

## Custom events

```js
import { track } from '@forkrail/analytics'

track('Signup', { plan: 'pro' })
```

From a function, with the request it's answering, so the event counts as that visitor's:

```js
import { track } from '@forkrail/analytics/server'

await track('Checkout', { total: 42 }, { request })
```

## What it sends

The script (`crates/router/src/insights.js`) posts each page view to `/_forkrail/insights/view` and
each event to `/_forkrail/insights/event`, on the site's own address: the page's address, the
referrer on the first page, the route if you give one, the event's name and properties (text,
numbers, true or false). The router adds the country and the kind of device and browser from the
request, and a visitor number made from the address and browser under a salt that changes every
day, so neither is kept. In development (`mode: 'development'`) it prints instead of sending.

| Call | What it does |
| --- | --- |
| `inject(props)` | Adds the script once. `mode`, `debug`, `beforeSend`, `disableAutoTrack`, `scriptSrc`, `endpoint`, `basePath`. |
| `track(name, properties, { flags })` | A custom event. |
| `pageview({ route, path })` | A page view, with `disableAutoTrack` (a router that knows its routes). |
| `computeRoute(path, params)` | `/blog/hello` with `{ slug: 'hello' }` is `/blog/[slug]`. |
| `<Analytics route path />` | `inject()` in React; with `route`, a page view on each change. |
| `<Analytics />` from `/next` | The same, with each page's route from Next.js. |
| `/remix`, `/vue`, `/astro`, `/sveltekit`, `/nuxt` | `inject()` for that framework (Astro also sends routes). |

`node --test test.js` runs its tests.
