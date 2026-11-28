// The Nuxt module: `modules: ['@forkrail/analytics/nuxt']` in nuxt.config adds a client plugin that
// calls inject() once. Page views follow the address, client-side navigations included.
import { addPlugin, addTemplate, defineNuxtModule } from '@nuxt/kit'

export default defineNuxtModule({
  meta: { name: '@forkrail/analytics', configKey: 'analytics' },
  defaults: {},
  setup(options) {
    const plugin = addTemplate({
      filename: 'forkrail-analytics.client.mjs',
      getContents: () => `import { inject } from '@forkrail/analytics'
import { defineNuxtPlugin } from '#imports'

export default defineNuxtPlugin(() => {
  inject({ framework: 'nuxt', ...${JSON.stringify(options || {})} })
})
`,
    })
    addPlugin({ src: plugin.dst, mode: 'client' })
  },
})
